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

/// The fused write lane (zc-write-fusion campaign, 2026-08-07): a
/// bounded per-worker executor that runs SMALL armed-WRITE handler
/// futures on the queue worker's own thread — the store/extract bridge
/// round trip pays zero cross-thread wakes. Child of this module (like
/// `wake_core`) so it shares the wake statics + `InboundUringReq`.
#[path = "fused.rs"]
pub mod fused;

use super::kmbuf::{self, KmbufQueue, TransportBufferMode};
use super::zc::{self, ZcBounce, ZcPend};
use crate::raw::request::ReplySlot;

/// `FUSE_OVER_IO_URING` (1ULL<<41) → `flags2` bit 9.
pub const FUSE_OVER_IO_URING_FLAGS2: u32 = 1u32 << 9;

pub const FUSE_URING_IN_OUT_HEADER_SZ: usize = 128;
pub const FUSE_URING_OP_IN_OUT_SZ: usize = 128;

const FUSE_IO_URING_CMD_REGISTER: u32 = 1;
const FUSE_IO_URING_CMD_COMMIT_AND_FETCH: u32 = 2;
/// Series patch 0029 (uapi "7.47"): release a COMMIT_RETAIN'd zc write
/// payload and re-arm the parked ent. Doubles as the §3.5 capability
/// probe opcode (see [`probe_retention_surface`]).
const FUSE_IO_URING_CMD_RELEASE_PAYLOAD: u32 = 3;
const FUSE_IN_HEADER_SIZE: usize = 40;
/// `linux/fuse.h` opcode 16 — the only opcode whose payload rides a lease.
const FUSE_WRITE_OPCODE: u32 = crate::raw::abi::fuse_opcode::FUSE_WRITE as u32;
const FUSE_WRITE_CACHE: u32 = crate::raw::abi::FUSE_WRITE_CACHE;

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
    /// FUSE request unique (also embedded in `header_and_op`) — the
    /// fused-write mint's parse-refusal identity (zc-write-fusion).
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
    /// The queue the reply's slot lives on. Lever 2 (ingress-queue-spread,
    /// 2026-08-05): commit channels are per drain GROUP, so the message
    /// itself must carry the member queue its `ent_idx` addresses.
    qid: u16,
    ent_idx: u16,
    commit_id: u64,
    header: Vec<u8>,
    reply_body: Bytes,
    /// zc direct-leg reply (K1 kill): `Some(n)` = the payload's `n`
    /// bytes ALREADY SIT in the request's pages (the handler's device
    /// fetch landed them through the sparse slot) — the worker commits
    /// header + `payload_sz = n` with no body move of any kind.
    prefilled: Option<u32>,
    /// ACK-early (0029): this reply's COMMIT carries
    /// `FUSE_URING_COMMIT_RETAIN` — the zc slot stays registered and the
    /// daemon's store continuation owes the RELEASE. Set from the
    /// per-ent retain flag at mint (the handler armed it via
    /// `zc_commit_retain` strictly before replying).
    retain: bool,
}

/// A handler-initiated zc device fetch (the direct read leg): DMA `len`
/// bytes from `fd` at `off` straight into the request's pages via the
/// ent's sparse slot. The worker forwards the raw CQE result (`res`) on
/// `done`; the handler validates and only then commits its reply.
pub(crate) struct ZcFetchMsg {
    qid: u16,
    ent_idx: u16,
    fd: RawFd,
    off: u64,
    len: u32,
    done: crate::sqz_channel::oneshot::Sender<i32>,
}

/// A handler-initiated zc device STORE (the D14 write-side direct leg):
/// DMA the ent's HELD WRITE payload (`len` = the held length, resolved
/// worker-side) from the slot's registered source pages straight to
/// `fd` at `dev_off`. The worker forwards the raw CQE result on `done`;
/// the handler validates full length and owns the fallback.
pub(crate) struct ZcStoreMsg {
    qid: u16,
    ent_idx: u16,
    fd: RawFd,
    dev_off: u64,
    done: crate::sqz_channel::oneshot::Sender<i32>,
}

/// A handler-requested LAZY WRITE-payload extraction
/// (dispatch-before-extraction, D14): bridge the ent's HELD payload
/// slot → memfd bounce and answer with a §5.4 lease over the bounce
/// bytes.
pub(crate) struct ZcExtractMsg {
    qid: u16,
    ent_idx: u16,
    done: crate::sqz_channel::oneshot::Sender<io::Result<Bytes>>,
}

/// One message to a drain-group worker (commit channel payload).
pub(crate) enum WorkerMsg {
    Commit(CommitMsg),
    ZcFetch(ZcFetchMsg),
    ZcStore(ZcStoreMsg),
    ZcExtract(ZcExtractMsg),
    /// Release a COMMIT_RETAIN'd zc slot (design-zc-write-kernel-v2
    /// §3.3): the daemon's ACK-early store continuation sends this once
    /// its retained DMA completed (or terminally failed). Ordered
    /// against the commit by the worker's deferral protocol — a release
    /// arriving before the RETAIN commit was submitted parks per-ent
    /// and fires the moment the commit goes out, so the kernel can
    /// never see RELEASE-before-COMMIT (`-EBUSY`).
    ZcRelease {
        qid: u16,
        ent_idx: u16,
    },
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
    /// COMMIT_AND_FETCH submitted WITH `FUSE_URING_COMMIT_RETAIN`
    /// (design-zc-write-kernel-v2 §3.3): the request ended kernel-side
    /// (the application's write returned — ACK-early), but the zc slot's
    /// pages stay registered and the commit cmd is PARKED (no CQE). The
    /// slot owes no reply; the daemon's store continuation owes exactly
    /// one RELEASE_PAYLOAD, whose submission returns the slot to
    /// [`SlotState::Replied`]. Teardown drains retained ents kernel-side
    /// (`-ECONNABORTED` on the parked cmd), so this state never wedges.
    Retained { commit_id: u64 },
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
    /// A ZcRelease that arrived ahead of its ent's RETAIN commit —
    /// consumed by the commit submission (the release-after-commit
    /// ordering protocol; see [`Self::set_release_pending`]).
    release_pending: Vec<bool>,
    /// Debug-only ownership proof (single-owner by construction).
    #[cfg(debug_assertions)]
    owner: std::thread::ThreadId,
}

impl SlotTable {
    /// Slot population (== the member queue's ring depth) — what maps a
    /// group-local ent id back to this table's queue-local index.
    pub(crate) fn len(&self) -> usize {
        self.states.len()
    }

    pub(crate) fn new(depth: usize) -> Self {
        Self {
            states: vec![SlotState::Registered; depth],
            register_failures: vec![0; depth],
            retry_at: vec![None; depth],
            release_pending: vec![false; depth],
            #[cfg(debug_assertions)]
            owner: std::thread::current().id(),
        }
    }

    /// A ZcRelease arrived before this ent's RETAIN commit was
    /// submitted (the store CQE beat the reply through the channels) —
    /// park it; the commit submission fires it (the kernel must never
    /// see RELEASE-before-COMMIT, which would answer `-EBUSY`).
    pub(crate) fn set_release_pending(&mut self, ent: usize) {
        self.assert_owner();
        self.release_pending[ent] = true;
    }

    /// Consume a parked release (at RETAIN-commit submission).
    pub(crate) fn take_release_pending(&mut self, ent: usize) -> bool {
        self.assert_owner();
        std::mem::take(&mut self.release_pending[ent])
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

    /// A COMMIT_AND_FETCH SQE carrying `FUSE_URING_COMMIT_RETAIN` was
    /// pushed — the reply is out (the slot owes nothing) but the ent is
    /// kernel-parked until RELEASE.
    pub(crate) fn on_commit_submitted_retained(&mut self, ent: usize, commit_id: u64) {
        self.assert_owner();
        self.states[ent] = SlotState::Retained { commit_id };
    }

    /// A RELEASE_PAYLOAD SQE for a retained `ent` was pushed — the
    /// kernel re-arms the parked commit into the fetch path, so the slot
    /// is `Replied` again (next CQE = next delivery). Inert on any other
    /// state (the teardown race).
    pub(crate) fn on_release_submitted(&mut self, ent: usize) {
        self.assert_owner();
        if let SlotState::Retained { commit_id } = self.states[ent] {
            self.states[ent] = SlotState::Replied { commit_id };
        }
    }

    /// The kernel refused a RETAIN commit (`-EINVAL` on the commit CQE
    /// while `Retained` — the queue was not retention-armed or the ent
    /// was not a zc write source). The ent still holds its applied
    /// reply: recover by plain re-commit, mirroring [`Self::on_commit_retry`].
    pub(crate) fn on_retain_refused(&mut self, ent: usize) {
        self.assert_owner();
        if let SlotState::Retained { commit_id } = self.states[ent] {
            self.states[ent] = SlotState::Delivered {
                unique: 0,
                commit_id,
            };
        }
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
    /// A zc sparse-slot bridge op (K1 kill): `READ_FIXED(device → slot)`
    /// for the direct leg, `READ_FIXED(memfd → slot)` for the bounce
    /// bridge, `WRITE_FIXED(slot → memfd)` for WRITE extraction. The
    /// per-ent pending kind ([`zc::ZcPend`]) disambiguates at the CQE.
    Fetch,
    /// A bridge-deadline `AsyncCancel` targeting an overdue Fetch op
    /// (zc-bridge-cqe-wedge, 2026-08-07 — the bounded-outcome law). Its
    /// own CQE is informational (`0` = found+canceled, `-ENOENT` = the
    /// op already completed, `-EALREADY` = running, may still complete);
    /// resolution always rides the ORIGINAL op's CQE.
    Cancel,
    /// A RELEASE_PAYLOAD uring_cmd for a retained ent (0029). Its CQE is
    /// accounting only: `0` counts a release; nonzero is the must-stay-0
    /// `fuse3_zc_release_failures` tripwire. The ent's next delivery
    /// rides the re-armed parked commit's CQE, a normal Fetch-class
    /// delivery.
    Release,
}

/// `user_data` reserved for the wake-fd PollAdd (unchanged).
pub(crate) const UD_POLL: u64 = u64::MAX;

/// Op-class tag in the high half of `user_data` (the low half carries
/// the ent index; ring depth is clamped to 32 by knob, so 32 bits of
/// index is unbounded headroom).
const UD_TAG_SHIFT: u32 = 32;
const UD_TAG_REGISTER: u64 = 1;
const UD_TAG_COMMIT: u64 = 2;
const UD_TAG_FETCH: u64 = 3;
const UD_TAG_CANCEL: u64 = 4;
const UD_TAG_RELEASE: u64 = 5;

/// Encode `(op, ent_idx)` into an SQE `user_data` word.
#[inline]
pub(crate) fn encode_user_data(op: RingOp, ent_idx: usize) -> u64 {
    let tag = match op {
        RingOp::Register => UD_TAG_REGISTER,
        RingOp::Commit => UD_TAG_COMMIT,
        RingOp::Fetch => UD_TAG_FETCH,
        RingOp::Cancel => UD_TAG_CANCEL,
        RingOp::Release => UD_TAG_RELEASE,
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
        UD_TAG_FETCH => RingOp::Fetch,
        UD_TAG_CANCEL => RingOp::Cancel,
        UD_TAG_RELEASE => RingOp::Release,
        // A word this worker never pushed (kernel echo of an unknown op
        // class): treat as a REGISTER completion — the historical
        // reading — rather than dropping the CQE.
        _ => RingOp::Register,
    };
    Some((op, (user_data & 0xFFFF_FFFF) as usize))
}

struct QueueHandle {
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

/// One drain context's producer-facing half (ingress-queue-spread lever 2,
/// `.benchmarks/2026-08-05-ingress-queue-spread.md`): the commit channel,
/// wake eventfd and wake coalescer SHARED by every member queue of one
/// drain group. N replies to ANY member between two worker passes cost one
/// eventfd write — the L3 lever-B elision keyed on the group's aggregate
/// state instead of one queue's (candidate direction (c), folded into (a)).
struct GroupHandle {
    /// Unbounded: a bounded sync_channel can block the session reply task
    /// if the group worker is briefly not draining, freezing *all* fuse
    /// replies (the per-queue law, unchanged by grouping).
    commit_tx: std::sync::mpsc::Sender<WorkerMsg>,
    /// Wake the group's drain thread (commit or shutdown).
    wake_fd: RawFd,
    /// Keep OwnedFd alive.
    _wake: OwnedFd,
    /// L3 lever B at group scope; shared with every member queue's
    /// [`PayloadArena`] so lease drops elide through the same flag.
    wake_coalescer: Arc<WakeCoalescer>,
    /// Member qids (ascending within the group; a node-membership SET,
    /// not a contiguous range — interleaved node numberings are the
    /// field norm; see [`drain_group_plan`]).
    qids: Vec<u16>,
}

// ---------------------------------------------------------------------------
// Ingress-queue-spread lever 2 — the drain-group plan
// ---------------------------------------------------------------------------

/// Groups (count) and max width of the armed drain-group plan — the
/// lever's engagement gauges (`transport_drain_groups` /
/// `transport_drain_group_width` on the stats inode).
static DRAIN_GROUPS: AtomicU64 = AtomicU64::new(0);
static DRAIN_GROUP_WIDTH_MAX: AtomicU64 = AtomicU64::new(0);

/// Publish the armed plan's shape for the stats inode (called once per
/// `try_start`; sim pools publish their singleton identity too).
pub(crate) fn publish_drain_group_plan(plan: &[Vec<u16>]) {
    DRAIN_GROUPS.store(plan.len() as u64, Ordering::Relaxed);
    DRAIN_GROUP_WIDTH_MAX.store(
        plan.iter().map(|g| g.len()).max().unwrap_or(0) as u64,
        Ordering::Relaxed,
    );
}

/// `(groups, width_max)` of the armed drain-group plan — the field row's
/// engagement instrument: `groups == queues` (width 1) means the lever is
/// structurally inert on this session (kmbuf mode, or a width-1 override).
pub fn drain_group_stats() -> (u64, u64) {
    (
        DRAIN_GROUPS.load(Ordering::Relaxed),
        DRAIN_GROUP_WIDTH_MAX.load(Ordering::Relaxed),
    )
}

/// Queues per drain context, DERIVED from the node's possible-CPU span:
/// the house `cpus/4` drain-parallelism SLOPE (the `il_sessions_default`
/// lineage; the ipc drain-LANE pair — service-thread ceiling + direct-
/// drive shard width — moved to its own class-measured `3×cpus/8` slope
/// in the 2026-08-06 width re-grade, while THIS width keeps the cpus/4
/// slope its own counted bracket validated). Floor
/// 1 = the physical minimum (a drain context owns at least one queue);
/// the node-span ceiling is implicit (`n/4 ≤ n`) and the counted bracket
/// showed whole-node must never be the default (−15 % on the
/// single-thread drain ceiling).
///
/// Bracket validation (2026-08-05, tcp devsub, fio libaio randread-4k,
/// one standing prefilled store + mount-only A-B-B-A alternation) ran on
/// a 32-possible-CPU single-node box, where the slope evaluates to 8 —
/// **byte-identical to the counted bracket winner**, so the A/B rows
/// carry over verbatim for this shape: widths 1–8 at IOPS par on the
/// venue's ~340k ceiling while width 8 halves the drain cost — commit
/// batch mean 1.48 → 2.98 COMMIT_AND_FETCH SQEs per `io_uring_enter`
/// (−54 % enters), eventfd wakes/op 0.687 → 0.340 (elide 31 % → 66 %) on
/// the SAME 32×8 point. The field ladder (the 2026-08-05 evidence note)
/// re-grades the SLOPE, never a constant.
pub fn drain_group_width(node_possible_cpus: usize) -> usize {
    (node_possible_cpus / 4).max(1)
}

/// `SQUEEZEFS_FUSE_DRAIN_GROUP` — explicit queues-per-drain-context width,
/// wins verbatim over the derivation (the ipc-cap explicit-wins pattern;
/// the A/B measurement lever). Range 1..=512 (= the raw queue-count
/// ceiling); the daemon's startup registry refuses malformed values, so a
/// bad value here keeps the derived default (the shim-side asymmetry law).
fn drain_group_width_env() -> Option<usize> {
    crate::env_knob_core::parse_int_in::<usize>(
        "SQUEEZEFS_FUSE_DRAIN_GROUP",
        std::env::var("SQUEEZEFS_FUSE_DRAIN_GROUP").ok().as_deref(),
        1,
        512,
    )
    .ok()
    .flatten()
}

/// The drain-group plan (ingress-queue-spread lever 2): partition qids
/// `0..nqueues` into groups by node MEMBERSHIP — one drain context
/// (thread + io_uring + eventfd + coalescer) per group. The 2026-08-05
/// evidence: with one context per possible CPU, IOPS scale NEGATIVELY
/// with submitter spread at constant in-flight (32×8 = 273k vs 8×32 =
/// 357k; pinning the same submitters to 8 CPUs recovers +21 %) because a
/// context woken for ~1 op pays a full thread wake + a ~1-commit
/// `io_uring_enter`. Grouping aggregates shallow member queues into
/// per-context batches; the kernel's queue *selection* (`task_cpu`) and
/// per-queue capacity are untouched, so submitter freedom is never
/// constrained (the 8×32-pinned-hurts counter-row).
///
/// MEMBERSHIP, never contiguity (the 2026-08-05 field disengagement):
/// 2-socket boxes commonly number CPUs round-robin across sockets
/// (node0 = even, node1 = odd), so contiguous node runs are length 1 and
/// a run-based plan silently derives the A0 per-queue posture
/// (`transport_drain_groups == queues`, width 1 — squeeze-test, live).
/// The portable-by-default law covers arbitrary NUMBERINGS, not just
/// arbitrary domain counts. Groups are qid-ascending within each node's
/// membership list; exactly one group leads with qid 0 (its node's set
/// starts at the global minimum), which is what the SQPOLL leader
/// election keys on.
///
/// Width derivation:
/// - default = [`drain_group_width`] evaluated PER NODE SET — the house
///   `cpus/4` drain-parallelism slope on the set's visible possible-CPU
///   population (floor 1), so both the context count AND the width derive
///   from machine shape (counted local bracket, 2026-08-05: the slope's
///   value at the 32-possible shape — 8 — halved the per-op drain cost at
///   IOPS par while whole-node widths collapsed on the single-thread
///   drain ceiling; the function's doc carries the numbers);
/// - groups never span a node (membership partition — structural), so
///   every member arena stays local to its drain thread;
/// - the RESIDUAL set — unknown-node qids: offline/isolated possible CPUs
///   (sysfs node cpulists carry online CPUs only), dormant by
///   construction (`task_cpu` never names an offline CPU), plus the whole
///   range under testing queue overrides (`qid_is_cpu == false`: no
///   qid↔CPU correspondence, no node info — the per-queue placement law)
///   — chunks at the MACHINE-span slope: no locality constraint exists,
///   dormant queues cost one context per machine-width chunk instead of
///   one each, and a later mass-online still lands on bounded-width
///   contexts;
/// - `BufRing` sessions keep width 1 (today's posture byte-identical):
///   the kmbuf fixed-headers/bufring registration is per-ring per-queue
///   in the sqz kernel surface — grouping under kmbuf is the named
///   follow-on, never a silent behavior fork;
/// - `SQUEEZEFS_FUSE_DRAIN_GROUP` explicit width wins verbatim (still
///   membership-partitioned: node containment is structural, not tuning).
pub fn drain_group_plan(
    nqueues: usize,
    buffer_mode: TransportBufferMode,
    qid_is_cpu: bool,
    node_of: impl Fn(usize) -> Option<usize>,
    explicit_width: Option<usize>,
) -> Vec<Vec<u16>> {
    // kmbuf: per-ring per-queue registration — singleton groups (an
    // explicit width never overrides the format constraint).
    let forced_width = match buffer_mode {
        // kmbuf resources (and the zc slot table riding them) are
        // per-RING: singleton drain groups keep the per-queue shape.
        TransportBufferMode::BufRing | TransportBufferMode::ZeroCopy => Some(1),
        TransportBufferMode::UserEnts => explicit_width.map(|w| w.max(1)),
    };
    // Pass 1: partition qids by node MEMBERSHIP, order-preserving within
    // each node's qid list — NEVER by contiguity (the 2026-08-05 field
    // disengagement: interleaved node numbering — node0 even qids, node1
    // odd — made every contiguous run length 1 and the whole lever derive
    // the A0 posture; contiguity was never load-bearing, each member
    // queue keeps its own SlotTable/ordering and the drain thread pins to
    // the NODE its members genuinely share). BTreeMap for deterministic
    // node order. Node consulted ONLY under the qid↔CPU correspondence
    // (testing queue overrides carry no per-qid node meaning — the whole
    // range is one flat residual set).
    let mut node_sets: std::collections::BTreeMap<usize, Vec<u16>> =
        std::collections::BTreeMap::new();
    // The residual set: unknown-node qids — offline/isolated possible
    // CPUs (sysfs node cpulists carry online CPUs only), dormant by
    // construction (`task_cpu` never names an offline CPU) — plus the
    // whole range when no qid↔CPU correspondence exists.
    let mut residual: Vec<u16> = Vec::new();
    for qid in 0..nqueues {
        match if qid_is_cpu { node_of(qid) } else { None } {
            Some(n) => node_sets.entry(n).or_default().push(qid as u16),
            None => residual.push(qid as u16),
        }
    }
    // Pass 2: chunk each set at its own width — the explicit/kmbuf width
    // verbatim, else the cpus/4 slope on the SET's population (the set IS
    // the node's visible possible-CPU population; queues == possible
    // CPUs). The residual set carries no locality constraint, so it rides
    // the MACHINE-span slope — dormant queues cost one context per
    // machine-width chunk instead of one each, and a later mass-online
    // still has bounded-width contexts serving it.
    let mut plan: Vec<Vec<u16>> = Vec::new();
    let mut chunk = |set: Vec<u16>, width: usize| {
        for c in set.chunks(width.max(1)) {
            plan.push(c.to_vec());
        }
    };
    for (_node, set) in node_sets {
        let width = forced_width.unwrap_or_else(|| drain_group_width(set.len()));
        chunk(set, width);
    }
    if !residual.is_empty() {
        let width = forced_width.unwrap_or_else(|| drain_group_width(nqueues));
        chunk(residual, width);
    }
    plan
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
    /// zc mode: the bounce-region owner (memfd mapping liveness for
    /// leases and dest windows) — same discriminant role as `kmbuf`.
    zc: Option<Arc<ZcBounce>>,
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
            zc: None,
        }))
    }

    /// zc-mode arena view (K1 kill, 2026-08-06): wraps the queue's memfd
    /// bounce region — ent-indexed like the classical arena (WRITE
    /// leases, dest windows and `get_payload_buffer` serves all target
    /// the bounce; the worker bridges it against the sparse slots).
    /// Mapping owned by [`ZcBounce`], held alive here for lease
    /// lifetimes; wake protocol identical.
    fn from_zc(
        zb: Arc<ZcBounce>,
        wake_fd: RawFd,
        wake_coalescer: Arc<WakeCoalescer>,
    ) -> io::Result<Arc<Self>> {
        let dup = unsafe { libc::dup(wake_fd) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` just returned a fresh owned descriptor.
        let wake = unsafe { OwnedFd::from_raw_fd(dup) };
        let (base, span, stride, count) = zb.geometry();
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
            kmbuf: None,
            zc: Some(zb),
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
            zc: None,
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
        if self.kmbuf.is_none() && self.zc.is_none() {
            // SAFETY: unmapping the span mapped in `new`; dropped once.
            // (kmbuf-mode spans are owned and unmapped by KmbufQueue; zc
            // bounce spans by ZcBounce.)
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
    tx: crate::sqz_channel::mpsc::UnboundedSender<InboundUringReq>,
    rx: crate::sqz_sync::SqzMutex<crate::sqz_channel::mpsc::UnboundedReceiver<InboundUringReq>>,
}

impl InboundQueue {
    fn new() -> Self {
        let (tx, rx) = crate::sqz_channel::mpsc::unbounded_channel();
        Self {
            tx,
            rx: crate::sqz_sync::SqzMutex::new(rx),
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
        shutdown: &crate::sqz_notify::Notify,
    ) -> Option<InboundUringReq> {
        let mut rx_guard = self.rx.lock().await;
        loop {
            // Register interest BEFORE the active check: `notify_waiters`
            // wakes only already-registered waiters, so enable-then-check
            // closes the store(false)/notify vs check/park race.
            // `Notified::enable()` registers SYNCHRONOUSLY (before any
            // await): either the check sees the store, or the
            // registration precedes the notify and the race wakes.
            let mut notified = shutdown.notified_raw();
            notified.enable();
            if !active.load(Ordering::Acquire) {
                return None;
            }
            match crate::sqz_future::race2(rx_guard.recv(), notified).await {
                crate::sqz_future::Either::Left(r) => return r,
                crate::sqz_future::Either::Right(()) => continue, // re-check active
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
    shutdown_notify: crate::sqz_notify::Notify,
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
    /// ACK-early (0029): per-(qid, ent) "next commit carries RETAIN"
    /// flags, addressed like `slot_watch`. Set by the daemon handler
    /// (via [`Self::zc_commit_retain`]) strictly before it replies,
    /// consumed at CommitMsg mint, and belt-cleared at delivery so an
    /// errored handler can never leak a stale retain onto the NEXT
    /// request's reply.
    retain_next: Vec<AtomicBool>,
    /// D14 write-side: per-(qid, ent) HELD WRITE-payload lengths
    /// (dispatch-before-extraction — the payload stays in the sparse
    /// slot until the handler stores it to the device or lazily
    /// extracts it). Written by the queue workers, read by the session
    /// validation and the handler's slot-source mint.
    zc_write_held: zc::ZcHeldTable,
    /// Live zc bridge ops across every worker (the bounded-outcome law,
    /// 2026-08-07): the watch thread wakes parked workers while this is
    /// nonzero so their [`zc::BridgeDeadlines`] scans run even on idle
    /// queues.
    zc_bridge_pends: AtomicU64,
    /// Worker-published scan gauges (Stage-1b attribution): each worker
    /// stores, once per deadline-scan pass, how many `Some` pends its
    /// members hold and how many of those lack a ledger stamp. The watch
    /// thread prints them with overdue warns — the live discriminator
    /// between "scan runs and sees nothing" and "scan never runs".
    scan_pends_seen: AtomicU64,
    scan_orphans_seen: AtomicU64,
    scan_passes: AtomicU64,
    scan_max_age_ms: AtomicU64,
    scan_cancel_latched: AtomicU64,
    /// zc bridge WorkerMsg economy (pump-starvation discriminator): a
    /// persistent `sent − taken` gap during a wedge = a ZcStore/ZcExtract
    /// message sitting unconsumed in a worker's commit_rx while that
    /// worker passes — the handler's oneshot then waits on a message no
    /// pump will ever pop.
    zc_msgs_sent: AtomicU64,
    zc_msgs_taken: AtomicU64,
    /// Per-group fused-lane residency watches (Stage-1b wedge
    /// attribution): registered by each worker at lane creation; read by
    /// [`Self::scan_overdue_slots`] to name a stuck fused write's state.
    fused_watches: std::sync::Mutex<
        Vec<(
            std::sync::Arc<fused::FusedWatch>,
            std::sync::Arc<fused::FusedRunQueue>,
        )>,
    >,
    /// The fused-write dispatcher (zc-write-fusion campaign): the
    /// session's WRITE-handler mint + runtime handle, registered once
    /// after INIT. Deliveries before registration ride the classic
    /// dispatch (counted as fusion demotions when otherwise eligible).
    fused_dispatch: std::sync::OnceLock<fused::FusedWriteDispatch>,
    /// The zc-write HOLD gate (fused-lane-predicate campaign,
    /// 2026-08-08): the filesystem's `zc_write_hold_eligible` seam —
    /// may this WRITE's payload stay HELD in the sparse slot as a
    /// direct-consume candidate? **Unregistered ⇒ never hold**: every
    /// armed WRITE extracts at delivery on the worker's batched pass
    /// (the safe posture the field falsification mandates — holding a
    /// shape no direct vehicle consumes buys hold + late extraction,
    /// serialized at fabric RTT).
    zc_hold_gate: std::sync::OnceLock<fused::ZcHoldGate>,
    /// Ring depth (per queue) — the `slot_watch` stride.
    depth: usize,
    /// §5.3 D3.b: session SQPOLL posture for the queue rings (`None` =
    /// knob unset = plain rings). See [`SqpollGroup`] for the one-poller
    /// leader/attach topology.
    sqpoll: Option<SqpollGroup>,
    queues: Vec<QueueHandle>,
    /// Drain groups (lever 2): one entry per drain context; member queues
    /// share its commit channel, eventfd and coalescer.
    groups: Vec<GroupHandle>,
    /// qid → index into `groups` (reply routing).
    group_of: Vec<u16>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    fuse_fd: RawFd,
    payload_sz: usize,
    /// Session buffer mode (2026-08-04 kmbuf campaign): resolved ONCE at
    /// `try_start` from the runtime capability probe + the
    /// `SQUEEZEFS_FUSE_KMBUF` lever. `UserEnts` = today's path,
    /// byte-identical.
    buffer_mode: TransportBufferMode,
    /// Session retention posture (design-zc-write-kernel-v2 §6.1):
    /// resolved ONCE at `try_start` — the §3.5 opcode probe × the
    /// `SQUEEZEFS_FUSE_ZC_RETENTION` lever × zc mode. True ⇒ every
    /// REGISTER carries `FUSE_URING_PAYLOAD_RETENTION`; arming alone is
    /// bit-identical (§3.6) until a commit carries RETAIN.
    retention: bool,
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

/// The zc bridge deadline (`SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS`, default
/// 30 000 — the D1.b op-watchdog threshold's transport twin): a bridge
/// op in flight past this pushes its `AsyncCancel` (the bounded-outcome
/// law; `fuse3_zc_bridge_cancels` is the must-stay-0 tripwire). Read
/// once per process; the daemon's startup gate refuses malformed
/// values, so a bad value here keeps the default.
fn zc_bridge_timeout_ns() -> u64 {
    static NS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *NS.get_or_init(|| {
        let ms = std::env::var("SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&v| (100..=600_000).contains(&v))
            .unwrap_or(30_000);
        ms * 1_000_000
    })
}

/// TEST SEAM budget (`SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES`): how many
/// WRITE-class bridge CQEs the workers should consume-and-drop — the
/// deterministic lost-completion interleave of the zcws-9 W4 wedge
/// (`tests/zc_bridge_cqe_wedge_tests.rs`). Returns `true` when THIS CQE
/// must be dropped. Production cost: one relaxed load of a
/// process-lifetime zero.
fn test_drop_write_cqe() -> bool {
    static BUDGET: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
    let b = BUDGET.get_or_init(|| {
        AtomicU64::new(
            std::env::var("SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0),
        )
    });
    if b.load(Ordering::Relaxed) == 0 {
        return false;
    }
    b.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
        .is_ok()
}

/// `SQUEEZEFS_TRANSPORT_DEBUG=1` — per-request transport tracing to stderr
/// (delivery / reply / commit / CQE errors) for stuck-request forensics.
pub fn transport_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        crate::env_knob_core::parse_bool(
            "SQUEEZEFS_TRANSPORT_DEBUG",
            std::env::var("SQUEEZEFS_TRANSPORT_DEBUG").ok().as_deref(),
        )
        .ok()
        .flatten()
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
    init_queue_depth: u16,
    buf_index: u16,
) -> io::Result<()> {
    if push_cmd(
        ring,
        cmd_op,
        qid,
        commit_id,
        iov,
        user_data,
        init_flags,
        init_queue_depth,
        buf_index,
    )
    .is_err()
    {
        // `push` only fails on a full SQ (§5.3 D3.a SQ-full rule).
        flush_submit(ring, batch)?;
        push_cmd(
            ring,
            cmd_op,
            qid,
            commit_id,
            iov,
            user_data,
            init_flags,
            init_queue_depth,
            buf_index,
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

/// [`flush_submit`] that ALSO materializes completions on
/// `DEFER_TASKRUN` rings — the interleave's flush (write-IOPS campaign,
/// 2026-08-11): a plain `submit()` never sets `IORING_ENTER_GETEVENTS`,
/// and deferred-task-work rings run their completion work ONLY under
/// that flag, so the mid-pass reap synced an eternally-empty CQ (the T1
/// engagement row: `fused_midpass_reaps` +0 against 22.85M pass-bottom
/// resolutions). `want = 1` + a ZERO `EXT_ARG` timespec is the
/// non-blocking GETEVENTS idiom (the dd-ring bounded-wait precedent):
/// task work runs, completed CQEs land, and an empty completion state
/// answers `ETIME` immediately — never a park.
fn flush_submit_getevents(ring: &mut Ring, batch: &mut SubmitBatch) -> io::Result<usize> {
    let (had_reads, had_writes) = batch.note_flush();
    let t0 = (had_reads || had_writes).then(Instant::now);
    let ts = types::Timespec::new();
    let args = types::SubmitArgs::new().timespec(&ts);
    let n = match ring.submitter().submit_with_args(1, &args) {
        Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(0),
        other => other,
    }?;
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
// The park LEDGER's other half: parked commits that later passed the gate
// (`try_unpark` re-proved refs == 0) and had their full reply applied +
// committed. `parked ≡ unparked` at quiesce is the §5.4 re-arm gate's
// closure law — a stranded park (a lease drop that owed a wake, or a
// parked scan that never ran) leaves the reply undelivered forever and
// can never be counted here, so the gap IS the wedge count. The
// shutdown drain's header-only fallback (lease still live at teardown)
// deliberately does NOT count: that is the genuine §5.4 escape, and it
// must remain visible as an unclosed ledger.
static TRANSPORT_UNPARKED_COMMITS: AtomicU64 = AtomicU64::new(0);
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
static TRANSPORT_REPLIES_OVERSIZE: AtomicU64 = AtomicU64::new(0);
/// Bounded-park backstop ticks (Stage-1b field wedge fix, 2026-08-13):
/// a worker holding live bridge pends / resident fused tasks parked its
/// 100 ms EXT_ARG bound out and re-ran its pass (deadline scan + rq
/// drain) under its OWN clock. Nonzero under bridge traffic is the
/// backstop WORKING; the deadline ladder no longer depends on a
/// cross-thread wake reaching a parked worker.
static TRANSPORT_PARK_BACKSTOP_TICKS: AtomicU64 = AtomicU64::new(0);

/// The bounded-park backstop's engagement gauge (stats surface).
pub fn transport_park_backstop_ticks() -> u64 {
    TRANSPORT_PARK_BACKSTOP_TICKS.load(Ordering::Relaxed)
}

// FUSE-3f: completion-queue loss. `transport_cq_overflows` is a
// must-stay-0 tripwire — a dropped CQE is a REGISTER or COMMIT_AND_FETCH
// completion that never arrives, i.e. an ent stalled for the session's
// life (and, for a COMMIT, a request the kernel keeps in `waiting`).
// `transport_cq_nodrop` is the per-session capability probe (1 = this
// kernel keeps overflowing completions in its internal list first).
static TRANSPORT_CQ_OVERFLOWS: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_CQ_NODROP: AtomicU64 = AtomicU64::new(0);

/// FUSE-3f gauges (stats inode): `(cq_overflows, cq_nodrop)`.
///
/// * `cq_overflows` — **must stay 0**: ring completions the kernel could
///   not queue and dropped. The ring geometry (`cqsize = sq * 2` against a
///   worst pass of `depth` commits + `depth` re-REGISTERs + 1 poll re-arm)
///   is what makes this unreachable; this counter is what proves it,
///   rather than assuming it.
/// * `cq_nodrop` — `IORING_FEAT_NODROP` as probed on the queue rings
///   (0 = a full CQ drops immediately; 1 = the kernel stores overflowing
///   completions internally and the counter only moves when even that
///   fails).
pub fn transport_cq_overflow_stats() -> (u64, u64) {
    (
        TRANSPORT_CQ_OVERFLOWS.load(Ordering::Relaxed),
        TRANSPORT_CQ_NODROP.load(Ordering::Relaxed),
    )
}

/// FUSE-3f: publish the queue rings' `IORING_FEAT_NODROP` probe (called
/// once per queue-ring build — the capability is a property of the kernel,
/// so every queue agrees).
fn note_cq_nodrop(nodrop: bool) {
    TRANSPORT_CQ_NODROP.store(u64::from(nodrop), Ordering::Relaxed);
}

/// FUSE-3f: per-queue-worker watch over the ring's cumulative CQ-overflow
/// counter, read after every `cq.sync()`.
///
/// The kernel's `cq_overflow` field counts completions it DROPPED (on a
/// NODROP kernel it is only incremented when the internal overflow entry
/// could not even be allocated — see `io_account_cq_overflow`), and it is
/// written with plain stores, so the watch tracks a wrapping delta rather
/// than an absolute.
struct CqDropWatch {
    last: u32,
    nodrop: bool,
}

impl CqDropWatch {
    fn new(nodrop: bool) -> Self {
        note_cq_nodrop(nodrop);
        Self { last: 0, nodrop }
    }

    /// Observe the ring's counter; returns the newly dropped completions
    /// (0 in the healthy case) and charges them to the tripwire.
    fn observe(&mut self, overflow: u32) -> u32 {
        let new = overflow.wrapping_sub(self.last);
        if new == 0 {
            return 0;
        }
        self.last = overflow;
        TRANSPORT_CQ_OVERFLOWS.fetch_add(u64::from(new), Ordering::Relaxed);
        new
    }

    /// Whether this kernel keeps overflowing completions internally — the
    /// interpretation half of a nonzero [`Self::observe`].
    fn nodrop(&self) -> bool {
        self.nodrop
    }
}

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
/// * `replies_oversize` — replies refused because they exceed the ent's
///   payload buffer (FUSE-3d: an EIO the kernel can act on, instead of a
///   truncated body under a header claiming the full length).
pub fn transport_reply_integrity_stats() -> (u64, u64, u64, u64, u64, u64, u64) {
    (
        TRANSPORT_REQUESTS_FAILED_SYNTHETIC.load(Ordering::Relaxed),
        TRANSPORT_REQUESTS_ABANDONED.load(Ordering::Relaxed),
        TRANSPORT_REPLIES_REFUSED_STALE.load(Ordering::Relaxed),
        TRANSPORT_REPLIES_DROPPED_NO_SLOT.load(Ordering::Relaxed),
        TRANSPORT_ENTS_RETIRED.load(Ordering::Relaxed),
        TRANSPORT_SLOTS_OVERDUE.load(Ordering::Relaxed),
        TRANSPORT_REPLIES_OVERSIZE.load(Ordering::Relaxed),
    )
}

/// Consume the queue eventfd's counter (nonblocking; the worker loop's
/// own prelude uses the same drain). Shared by the loop and the FUSE-3j
/// shutdown drain so both keep the drain→disarm→scan order the wake
/// protocol is loom-verified under.
fn drain_wake_eventfd(wake_fd: RawFd) {
    let mut buf = [0u8; 8];
    loop {
        // SAFETY: an 8-byte read into a local buffer from the queue's
        // eventfd (owned by the pool/arena for the worker's lifetime).
        let n = unsafe { libc::read(wake_fd, buf.as_mut_ptr().cast(), 8) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
    }
}

/// Block until the queue eventfd is readable or `timeout` expires.
fn wait_wake_fd(wake_fd: RawFd, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: wake_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd; the fd outlives the call (pool/arena-owned).
    let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
    rc > 0 && pfd.revents & libc::POLLIN != 0
}

/// FUSE-3j: the shutdown drain's TOTAL lease-wait budget for one queue
/// (was `100 ms` per parked ent, serially — 3.2 s at depth 32). One
/// bounded wait for the whole queue; expiry degrades to the header-only
/// error reply, never to writing a leased payload region.
const DRAIN_LEASE_BUDGET: Duration = Duration::from_millis(100);

/// FUSE-3j: wait for the still-live payload leases of `waiting` (ent
/// indices) to drop, **event-driven and on ONE shared budget**.
///
/// The retired shape slept `1 ms` up to `100 ms` PER ENT, in series: at
/// depth 32 that is 3.2 s of teardown per queue, paid on every umount, for
/// a signal the lease drop already delivers (`lease_release_and_wake`
/// fires this exact eventfd whenever the last ref of a PARKED ent drops).
///
/// Order per pass is the loop's own drain→disarm→scan (wake_core protocol):
/// consuming the eventfd and disarming the coalescer BEFORE the scan is
/// what makes a release that races the scan either arm+write a wake this
/// `poll` still sees, or become visible to the scan itself. `waiting`
/// keeps exactly the ents that are still leased when the budget runs out —
/// those get the header-only error reply (§5.4 forbids writing a leased
/// payload region, at shutdown as much as anywhere else).
fn drain_await_leases(
    wake_fd: RawFd,
    coalescer: &WakeCoalescer,
    lease_states: &[Arc<EntLeaseState>],
    waiting: &mut Vec<usize>,
    budget: Duration,
) {
    let deadline = Instant::now() + budget;
    loop {
        drain_wake_eventfd(wake_fd);
        coalescer.disarm();
        waiting.retain(|&idx| !lease_states[idx].try_unpark());
        if waiting.is_empty() {
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        wait_wake_fd(wake_fd, left);
    }
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
/// parked_commits, unparked_commits, leases_outstanding,
/// lease_max_age_ms, lease_overlong, dest_dma_leases)`.
/// `payload_leases` proves adoption (FUSE_WRITE rides leases, not copies);
/// `parked_commits` ≫ 0 means handlers hold payloads past their reply or
/// Q_DEPTH is too small, and `parked_commits − unparked_commits` at
/// quiesce is the re-arm gate's WEDGE count (parks that never resolved —
/// the invariant is closure, not absence: parking is the gate working);
/// `leases_outstanding` returns to 0 at quiesce;
/// `lease_max_age_ms` is the severance-boundary high-water mark (bounded by
/// one handler invocation); `lease_overlong` counts ≥ 1 s lifetimes — the
/// loud-never-fatal §5.4 tripwire (see `EntPayloadLease::drop`);
/// `dest_dma_leases` counts MEM-1 read-destination owner-token claims
/// (≈ one per dest-bearing device-read SQE on an armed session).
pub fn transport_lease_stats() -> (u64, u64, u64, u64, u64, u64, u64) {
    (
        TRANSPORT_PAYLOAD_LEASES.load(Ordering::Relaxed),
        TRANSPORT_PARKED_COMMITS.load(Ordering::Relaxed),
        TRANSPORT_UNPARKED_COMMITS.load(Ordering::Relaxed),
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
/// FUSE-4d: the kernel's `max_readahead`, exactly as echoed in the INIT
/// reply. Published so the stats inode can show the negotiated limit
/// alongside the R2 prefetch window it is DELIBERATELY independent of (see
/// `negotiate_max_readahead`).
static GEOM_MAX_READAHEAD: AtomicU64 = AtomicU64::new(0);

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

/// FUSE-4d: the session's negotiated `max_readahead` (0 until a session
/// negotiates). Stats inode `transport_max_readahead`.
pub fn negotiated_max_readahead() -> u64 {
    GEOM_MAX_READAHEAD.load(Ordering::Relaxed)
}

/// FUSE-4d: record the readahead limit the INIT reply echoed.
pub fn note_negotiated_max_readahead(v: u32) {
    GEOM_MAX_READAHEAD.store(u64::from(v), Ordering::Relaxed);
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
        // Retention capability (design-zc-write-kernel-v2 §3.5/§6.1):
        // probe by OPCODE post-INIT pre-REGISTER — an old kernel ignores
        // unknown init bits, so bit 2 cannot self-negotiate. Probed only
        // where it could arm (zc mode); the verdict composes the
        // REGISTER flags below and gauges after the arm barrier.
        let retention = buffer_mode == TransportBufferMode::ZeroCopy
            && kmbuf::resolve_retention(probe_retention_surface(fuse_fd), true);
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
        for _ in 0..nqueues {
            queue_handles.push(QueueHandle {
                arena: std::sync::Mutex::new(None),
                kmbuf: std::sync::Mutex::new(None),
                // One §5.4 lease word per ring ent — pool-level so MEM-1
                // dest claims and the worker's commit gate share them.
                lease_states: (0..depth).map(|_| Arc::new(EntLeaseState::new())).collect(),
                dest_window: std::sync::OnceLock::new(),
            });
        }

        // Lever 2 (ingress-queue-spread): one drain context per GROUP of
        // queues — plan resolved once per session (see `drain_group_plan`
        // for the width derivation and its evidence).
        let qid_is_cpu = nqueues == kernel_possible_cpus();
        let topo = crate::numa_core::topology();
        let plan = drain_group_plan(
            nqueues,
            buffer_mode,
            qid_is_cpu,
            |cpu| topo.node_of_cpu(cpu),
            drain_group_width_env(),
        );
        publish_drain_group_plan(&plan);
        let mut group_handles = Vec::with_capacity(plan.len());
        let mut group_of = vec![0u16; nqueues];
        let mut commit_rxs = Vec::with_capacity(plan.len());
        let mut wake_fds = Vec::with_capacity(plan.len());
        for (gi, qids) in plan.iter().enumerate() {
            let (commit_tx, commit_rx) = std::sync::mpsc::channel();
            let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if efd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: efd is a freshly-created, owned eventfd (checked >= 0).
            let wake = unsafe { OwnedFd::from_raw_fd(efd) };
            let wake_fd = wake.as_raw_fd();
            wake_fds.push(wake_fd);
            for &qid in qids.iter() {
                group_of[qid as usize] = gi as u16;
            }
            group_handles.push(GroupHandle {
                commit_tx,
                wake_fd,
                _wake: wake,
                wake_coalescer: Arc::new(WakeCoalescer::new()),
                qids: qids.clone(),
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
            shutdown_notify: crate::sqz_notify::Notify::new(),
            queues_registered: AtomicU64::new(0),
            nqueues: nqueues as u16,
            inbound,
            slot_watch: (0..nqueues * depth).map(|_| SlotWatch::default()).collect(),
            retain_next: (0..nqueues * depth)
                .map(|_| AtomicBool::new(false))
                .collect(),
            zc_write_held: {
                kmbuf::init_zc_store_qid_census(nqueues);
                zc::ZcHeldTable::new(nqueues, depth)
            },
            zc_bridge_pends: AtomicU64::new(0),
            scan_pends_seen: AtomicU64::new(0),
            scan_orphans_seen: AtomicU64::new(0),
            scan_passes: AtomicU64::new(0),
            scan_max_age_ms: AtomicU64::new(0),
            scan_cancel_latched: AtomicU64::new(0),
            zc_msgs_sent: AtomicU64::new(0),
            zc_msgs_taken: AtomicU64::new(0),
            fused_watches: std::sync::Mutex::new(Vec::new()),
            zc_hold_gate: std::sync::OnceLock::new(),
            fused_dispatch: std::sync::OnceLock::new(),
            depth,
            sqpoll,
            queues: queue_handles,
            groups: group_handles,
            group_of,
            workers: Mutex::new(Vec::new()),
            fuse_fd,
            payload_sz,
            buffer_mode,
            retention,
            qid_is_cpu,
            stats_requests: AtomicU64::new(0),
            stats_replies: AtomicU64::new(0),
            stats_cqe_err: AtomicU64::new(0),
            stats_register: AtomicU64::new(0),
        });

        let (err_tx, err_rx) = std::sync::mpsc::sync_channel::<String>(pool.groups.len().max(1));
        let mut handles = Vec::new();
        for (gi, &wake_fd) in wake_fds.iter().enumerate() {
            let pool_c = pool.clone();
            let commit_rx = commit_rxs.remove(0);
            let err_tx = err_tx.clone();
            let qids = pool.groups[gi].qids.clone();
            // Thread-name compat: a singleton group keeps today's
            // per-queue name; multi-member groups carry first-last as a
            // membership LABEL, not a range (comm truncates at 15 chars
            // anyway).
            let name = crate::comm_core::comm_name(&if qids.len() == 1 {
                format!("fuse-over-uring-{}", qids[0])
            } else {
                format!("fuse-over-uring-{}-{}", qids[0], qids[qids.len() - 1])
            });
            let h = std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    if let Err(e) =
                        queue_worker(pool_c.clone(), gi, depth, payload_sz, commit_rx, wake_fd)
                    {
                        let msg = format!("qids={:?}: {e}", pool_c.groups[gi].qids);
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
        kmbuf::set_kmbuf_negotiated(buffer_mode.uses_kmbuf());
        kmbuf::set_zc_negotiated(buffer_mode == TransportBufferMode::ZeroCopy);
        kmbuf::set_retention_negotiated(retention);
        ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);

        // Watch /dev/fuse for POLLERR/POLLHUP/etc so we shut down even if a
        // worker is blocked in submit_and_wait and has not yet seen a CQE with
        // -ENOTCONN (e.g. all ring entries already torn down by the kernel).
        {
            let watch = pool.clone();
            let h = std::thread::Builder::new()
                .name(crate::comm_core::comm_name("fuse-over-uring-watch"))
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
        let mode_state = match (buffer_mode, retention) {
            (TransportBufferMode::UserEnts, _) => "user-ents",
            (TransportBufferMode::BufRing, _) => "kmbuf-bufring",
            (TransportBufferMode::ZeroCopy, false) => "kmbuf-bufring+zero-copy",
            (TransportBufferMode::ZeroCopy, true) => "kmbuf-bufring+zero-copy+retention",
        };
        // The ladder-resolved kmbuf opcode pair (per kernel track:
        // 37/38 = 6.19-sqz, 38/39 = 7.1-sqz, "absent" = stock) — logged
        // once at arm beside the negotiation state.
        let kmbuf_ops = kmbuf::resolved_opcodes_label();
        // Lever-2 evidence line: the armed drain-group shape (groups ==
        // queues ⇒ the lever is structurally inert on this session).
        let (dg, dgw) = drain_group_stats();
        eprintln!(
            "FUSE-over-io_uring registered: queues={nqueues} depth={depth} payload_sz={payload_sz} \
             max_write={max_write} max_pages={max_pages} buffers={mode_state} \
             kmbuf_ops={kmbuf_ops} fd={fuse_fd} sqpoll={sqpoll_state} \
             drain_groups={dg}(width<={dgw})"
        );
        info!(
            "FUSE-over-io_uring registered: queues={nqueues} depth={depth} \
             payload_sz={payload_sz} max_write={max_write} max_pages={max_pages} \
             buffers={mode_state} kmbuf_ops={kmbuf_ops} fd={fuse_fd} \
             sqpoll={sqpoll_state} drain_groups={dg}(width<={dgw})"
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
    ) -> (Arc<Self>, Vec<std::sync::mpsc::Receiver<WorkerMsg>>) {
        Self::sim_inert_inner(nqueues)
    }

    fn sim_inert_inner(nqueues: u16) -> (Arc<Self>, Vec<std::sync::mpsc::Receiver<WorkerMsg>>) {
        let mut inbound = Vec::with_capacity(nqueues as usize);
        let mut queues = Vec::with_capacity(nqueues as usize);
        let mut groups = Vec::with_capacity(nqueues as usize);
        let mut commit_rxs = Vec::with_capacity(nqueues as usize);
        for qid in 0..nqueues {
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
            // Sim groups are the singleton identity (per-queue receivers —
            // the teardown pins address queues individually).
            groups.push(GroupHandle {
                commit_tx,
                wake_fd,
                _wake: wake,
                wake_coalescer: Arc::new(WakeCoalescer::new()),
                qids: vec![qid],
            });
            commit_rxs.push(commit_rx);
        }
        let pool = Arc::new(Self {
            ready: AtomicBool::new(false),
            active: AtomicBool::new(true),
            shutdown_notify: crate::sqz_notify::Notify::new(),
            queues_registered: AtomicU64::new(0),
            nqueues,
            inbound,
            slot_watch: (0..nqueues as usize * Self::SIM_DEPTH)
                .map(|_| SlotWatch::default())
                .collect(),
            retain_next: (0..nqueues as usize * Self::SIM_DEPTH)
                .map(|_| AtomicBool::new(false))
                .collect(),
            zc_write_held: zc::ZcHeldTable::new(nqueues as usize, Self::SIM_DEPTH),
            zc_bridge_pends: AtomicU64::new(0),
            scan_pends_seen: AtomicU64::new(0),
            scan_orphans_seen: AtomicU64::new(0),
            scan_passes: AtomicU64::new(0),
            scan_max_age_ms: AtomicU64::new(0),
            scan_cancel_latched: AtomicU64::new(0),
            zc_msgs_sent: AtomicU64::new(0),
            zc_msgs_taken: AtomicU64::new(0),
            fused_watches: std::sync::Mutex::new(Vec::new()),
            zc_hold_gate: std::sync::OnceLock::new(),
            fused_dispatch: std::sync::OnceLock::new(),
            depth: Self::SIM_DEPTH,
            sqpoll: None,
            queues,
            group_of: (0..nqueues).collect(),
            groups,
            workers: Mutex::new(Vec::new()),
            fuse_fd: -1,
            payload_sz: 1 << 20,
            buffer_mode: TransportBufferMode::UserEnts,
            retention: false,
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
        let g = self
            .group_of
            .get(qid as usize)
            .and_then(|&gi| self.groups.get(gi as usize))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad qid"))?;
        // ACK-early: consume the handler-armed retain flag into the
        // message (set ≺ reply on the handler task, so the read is
        // race-free; delivery belt-clears leaks).
        let retain = self.take_retain_next(qid, ent_idx as usize);
        g.commit_tx
            .send(WorkerMsg::Commit(CommitMsg {
                qid,
                ent_idx,
                commit_id,
                header,
                reply_body,
                prefilled: None,
                retain,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring commit closed"))?;
        // Wake the drain thread. L3 lever B at group scope: the channel
        // send above is the publication; the coalescer elides the eventfd
        // write when a wake is already armed — N replies to ANY member
        // queue between two worker passes cost one write (wake_core
        // protocol, loom-verified send→arm→write order).
        if g.wake_coalescer.arm() {
            let one: u64 = 1;
            let _ = unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
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
        // zc mode (K1 kill): paged serves target the ent's BOUNCE slot
        // (memfd arena — the worker bridges it into the request's pages
        // through the sparse slot). The kmbuf attachment carries only
        // copyable traffic there and is never a serve destination.
        if self.buffer_mode == TransportBufferMode::ZeroCopy {
            let arena = q.arena.lock().unwrap().clone()?;
            let ptr = arena.buf(ent_idx as usize)?;
            return Some((ptr as u64, arena.stride));
        }
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

    /// True when this session's request hot path runs the zc arm — the
    /// gate the root crate's READ handler consults before minting a
    /// device-fetch descriptor.
    pub fn zc_armed(&self) -> bool {
        self.buffer_mode == TransportBufferMode::ZeroCopy && self.is_ready()
    }

    /// zc direct leg (K1 kill): DMA `len` bytes from `fd@off` straight
    /// into the requesting slot's registered pages via the queue ring's
    /// sparse fixed-buffer table. Resolves to the raw ring result:
    /// `Ok(n)` = bytes transferred (the caller treats `n != len` as a
    /// failed leg and falls back), `Err` = the op errored (`EINVAL`
    /// misalignment, a request torn down mid-fetch, teardown).
    ///
    /// The caller owns validation and the eventual reply: this call
    /// happens strictly BEFORE the request's reply exists (mint → fetch
    /// → validate → reply on the handler task), so the ent cannot be
    /// re-armed or re-delivered underneath the fetch (the kernel frees a
    /// slot only at COMMIT, which is gated on the handler's reply).
    pub async fn zc_device_fetch(
        &self,
        slot: ReplySlot,
        fd: RawFd,
        off: u64,
        len: u32,
    ) -> io::Result<u32> {
        let ReplySlot::Ring { qid, ent_idx, .. } = slot else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zc fetch: reply has no ring slot",
            ));
        };
        if !self.zc_armed() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "zc fetch: session not zc-armed",
            ));
        }
        let (done_tx, done_rx) = crate::sqz_channel::oneshot::channel();
        let g = self
            .group_of
            .get(qid as usize)
            .and_then(|&gi| self.groups.get(gi as usize))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad qid"))?;
        g.commit_tx
            .send(WorkerMsg::ZcFetch(ZcFetchMsg {
                qid,
                ent_idx,
                fd,
                off,
                len,
                done: done_tx,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring worker closed"))?;
        if g.wake_coalescer.arm() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to a live eventfd.
            let _ = unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
        let res = done_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "zc fetch dropped (teardown)")
        })?;
        if res < 0 {
            return Err(io::Error::from_raw_os_error(-res));
        }
        Ok(res as u32)
    }

    /// D14 write-side: the HELD payload length of `slot`'s request
    /// (dispatch-before-extraction — `Some(len)` means the WRITE's
    /// payload sits in the sparse slot, consumable via
    /// [`Self::zc_write_store`] / [`Self::zc_write_extract`]).
    pub fn zc_write_held_len(&self, slot: ReplySlot) -> Option<u32> {
        let ReplySlot::Ring { qid, ent_idx, .. } = slot else {
            return None;
        };
        if !self.zc_armed() {
            return None;
        }
        self.zc_write_held.get(qid, ent_idx as usize)
    }

    /// Register the session's fused-write dispatcher (zc-write-fusion
    /// campaign, 2026-08-07): the WRITE-handler mint + the runtime
    /// handle whose timers/spawns the fused polls may use. First set
    /// wins (worker sessions all clone one primary — re-registration is
    /// a benign no-op); deliveries before registration ride the classic
    /// dispatch and count as fusion demotions when otherwise eligible.
    pub fn set_fused_write_dispatcher(&self, d: fused::FusedWriteDispatch) {
        let _ = self.fused_dispatch.set(d);
    }

    /// Register the zc-write HOLD gate (fused-lane-predicate campaign):
    /// the filesystem's cheap W1-eligibility probe. First set wins;
    /// deliveries before registration extract at delivery (never hold).
    pub fn set_zc_write_hold_gate(&self, g: fused::ZcHoldGate) {
        let _ = self.zc_hold_gate.set(g);
    }

    /// D14 write-side direct leg: DMA the request's HELD WRITE payload
    /// from its registered source pages straight to `fd` at `dev_off`
    /// (`WRITE_FIXED(device fd ← slot)`). Resolves to the raw ring
    /// result: `Ok(n)` = bytes transferred (the caller treats
    /// `n != held_len` as a failed leg and falls back to
    /// [`Self::zc_write_extract`]), `Err` = the op errored/refused.
    ///
    /// Ordering contract (patch 0024): the slot's pages unregister at
    /// COMMIT — this call happens strictly BEFORE the request's reply
    /// exists (the handler awaits it), so the DMA always completes
    /// before the commit that releases the pages.
    pub async fn zc_write_store(
        &self,
        slot: ReplySlot,
        fd: RawFd,
        dev_off: u64,
    ) -> io::Result<u32> {
        let ReplySlot::Ring { qid, ent_idx, .. } = slot else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zc store: reply has no ring slot",
            ));
        };
        if !self.zc_armed() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "zc store: session not zc-armed",
            ));
        }
        let (done_tx, done_rx) = crate::sqz_channel::oneshot::channel();
        let g = self
            .group_of
            .get(qid as usize)
            .and_then(|&gi| self.groups.get(gi as usize))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad qid"))?;
        self.zc_msgs_sent.fetch_add(1, Ordering::Relaxed);
        g.commit_tx
            .send(WorkerMsg::ZcStore(ZcStoreMsg {
                qid,
                ent_idx,
                fd,
                dev_off,
                done: done_tx,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring worker closed"))?;
        if g.wake_coalescer.arm() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to a live eventfd.
            let _ = unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
        let res = done_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "zc store dropped (teardown)")
        })?;
        if res < 0 {
            return Err(io::Error::from_raw_os_error(-res));
        }
        Ok(res as u32)
    }

    /// D14 write-side lazy extraction (the ineligible-shape vehicle):
    /// bridge the request's HELD WRITE payload slot → memfd bounce and
    /// answer with a §5.4 lease over the bounce bytes — exactly the
    /// payload the at-delivery extraction used to mint, minted on
    /// demand instead. Clears the held state on success (one
    /// materialization per request; the handler memoizes).
    pub async fn zc_write_extract(&self, slot: ReplySlot) -> io::Result<Bytes> {
        let ReplySlot::Ring { qid, ent_idx, .. } = slot else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zc extract: reply has no ring slot",
            ));
        };
        if !self.zc_armed() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "zc extract: session not zc-armed",
            ));
        }
        let (done_tx, done_rx) = crate::sqz_channel::oneshot::channel();
        let g = self
            .group_of
            .get(qid as usize)
            .and_then(|&gi| self.groups.get(gi as usize))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad qid"))?;
        self.zc_msgs_sent.fetch_add(1, Ordering::Relaxed);
        g.commit_tx
            .send(WorkerMsg::ZcExtract(ZcExtractMsg {
                qid,
                ent_idx,
                done: done_tx,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring worker closed"))?;
        if g.wake_coalescer.arm() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to a live eventfd.
            let _ = unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
        done_rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "zc extract dropped (teardown)")
        })?
    }

    /// Commit a reply whose payload ALREADY SITS in the request's pages
    /// (the zc direct leg — see [`Self::zc_device_fetch`]). `header` is
    /// the 16-byte `fuse_out_header` (its `len` must already read
    /// `16 + payload_len`); the worker sets `payload_sz = payload_len`
    /// and moves no bytes.
    pub fn submit_reply_prefilled(
        &self,
        slot: ReplySlot,
        header: Vec<u8>,
        payload_len: u32,
    ) -> io::Result<()> {
        let ReplySlot::Ring {
            qid,
            ent_idx,
            commit_id,
        } = slot
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "uring: prefilled reply has no ring slot",
            ));
        };
        if !self.active.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "uring: session torn down",
            ));
        }
        let g = self
            .group_of
            .get(qid as usize)
            .and_then(|&gi| self.groups.get(gi as usize))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad qid"))?;
        // Prefilled replies are read-direction — retention is a write
        // law; clear any stale flag rather than carry it (hygiene: the
        // delivery belt-clear also covers this).
        let _ = self.take_retain_next(qid, ent_idx as usize);
        g.commit_tx
            .send(WorkerMsg::Commit(CommitMsg {
                qid,
                ent_idx,
                commit_id,
                header,
                reply_body: Bytes::new(),
                prefilled: Some(payload_len),
                retain: false,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring commit closed"))?;
        if g.wake_coalescer.arm() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to a live eventfd.
            let _ = unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
        self.stats_replies.fetch_add(1, Ordering::Relaxed);
        STATS_REPLIES.fetch_add(1, Ordering::Relaxed);
        Ok(())
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

    /// ACK-early (0029): arm RETAIN for this slot's NEXT commit. Returns
    /// `true` iff armed — the daemon gates its early reply on it (false
    /// ⇒ no retention on this session, keep ACK-after-CQE). Legal only
    /// from the request's own handler, strictly before its reply (the
    /// severance chain handler ≺ reply ≺ COMMIT makes the flag's
    /// lifetime exactly one request; delivery belt-clears it).
    pub fn zc_commit_retain(&self, slot: ReplySlot) -> bool {
        let ReplySlot::Ring { qid, ent_idx, .. } = slot else {
            return false;
        };
        if !self.retention || !self.zc_armed() {
            return false;
        }
        let Some(cell) = self
            .retain_next
            .get(qid as usize * self.depth + ent_idx as usize)
        else {
            return false;
        };
        cell.store(true, Ordering::Release);
        true
    }

    /// Consume the retain flag at CommitMsg mint (one request's reply).
    #[inline]
    fn take_retain_next(&self, qid: u16, ent_idx: usize) -> bool {
        self.retain_next
            .get(qid as usize * self.depth + ent_idx)
            .map(|c| c.swap(false, Ordering::AcqRel))
            .unwrap_or(false)
    }

    /// ACK-early (0029): release a COMMIT_RETAIN'd slot — the store
    /// continuation's ONE obligation once its retained DMA completed
    /// (or terminally failed). Ordering against the commit is the
    /// worker's deferral protocol; a stale release (teardown race) is
    /// inert.
    pub fn zc_release_payload(&self, slot: ReplySlot) -> io::Result<()> {
        let ReplySlot::Ring { qid, ent_idx, .. } = slot else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zc release: reply has no ring slot",
            ));
        };
        if !self.active.load(Ordering::Acquire) {
            // Teardown owns retained ents from here (kernel drains the
            // parked cmds -ECONNABORTED); the release is moot.
            return Ok(());
        }
        let g = self
            .group_of
            .get(qid as usize)
            .and_then(|&gi| self.groups.get(gi as usize))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad qid"))?;
        g.commit_tx
            .send(WorkerMsg::ZcRelease { qid, ent_idx })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring worker closed"))?;
        if g.wake_coalescer.arm() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to a live eventfd.
            let _ = unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
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
            // Stage-1b attribution: is this unique a live fused task?
            // resident+quiet-rq = parked awaiting an unnamed wake;
            // resident+rq>0 persistently = fused polls not running;
            // NOT resident anywhere = the future is GONE without its
            // reply (the reply-obligation class).
            let (fused_state, ready_total) = {
                let watches = self
                    .fused_watches
                    .lock()
                    .expect("fused watch registry poisoned-free");
                let mut state = "not-fused";
                let mut ready = 0usize;
                for (w, rq) in watches.iter() {
                    ready += rq.ready_len();
                    if w.is_resident(unique) {
                        state = "fused-resident";
                    }
                }
                (state, ready)
            };
            warn!(
                "fuse-over-uring qid={qid} ent={ent}: unique={unique} delivered {} ms ago and \
                 still unreplied — the caller is in uninterruptible sleep \
                 (transport_slots_overdue; {fused_state}, fused_ready_total={ready_total}, \
                 zc_bridge_pends={}, scan_passes={}, scan_pends_seen={}, \
                 scan_orphans_seen={}, pend_max_age_ms={}, cancel_latched={}, \
                 park_backstop_ticks={}, zc_msgs_sent={}, zc_msgs_taken={})",
                now.saturating_sub(since) / 1_000_000,
                self.zc_bridge_pends.load(Ordering::Relaxed),
                self.scan_passes.load(Ordering::Relaxed),
                self.scan_pends_seen.load(Ordering::Relaxed),
                self.scan_orphans_seen.load(Ordering::Relaxed),
                self.scan_max_age_ms.load(Ordering::Relaxed),
                self.scan_cancel_latched.load(Ordering::Relaxed),
                TRANSPORT_PARK_BACKSTOP_TICKS.load(Ordering::Relaxed),
                self.zc_msgs_sent.load(Ordering::Relaxed),
                self.zc_msgs_taken.load(Ordering::Relaxed)
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
        for g in &self.groups {
            let _ = unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
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
            // The bounded-outcome law's WAKE half (2026-08-07): a worker
            // parked in cq-wait never scans its bridge deadlines — while
            // any bridge pend is outstanding, tick every group's eventfd
            // so the parked workers run the scan (coalesced; one write
            // per group per watchdog interval at most).
            if pool.zc_bridge_pends.load(Ordering::Relaxed) > 0 {
                for g in &pool.groups {
                    if g.wake_coalescer.arm() {
                        let one: u64 = 1;
                        // SAFETY: writing 8 bytes to a live eventfd.
                        let _ =
                            unsafe { libc::write(g.wake_fd, &one as *const u64 as *const _, 8) };
                        TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
                    } else {
                        TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
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
    group_idx: usize,
    depth: usize,
    payload_sz: usize,
    commit_rx: std::sync::mpsc::Receiver<WorkerMsg>,
    wake_fd: RawFd,
) -> io::Result<()> {
    // The queue's CONFIGURED ent payload size (the delivery loop shadows
    // `payload_sz` with each request's announced size) — the D14
    // hold-candidate bound derives from it.
    let payload_sz_cfg = payload_sz;
    // Lever 2 (ingress-queue-spread): this worker is ONE drain context for
    // every member queue of its group — one ring, one park, one wake, one
    // flush amortized over the group's aggregate traffic. `first_qid`
    // stands in wherever the retired per-queue worker used its qid
    // (affinity home, SQPOLL leader election); per-request logs and reply
    // addressing always carry the member's REAL qid.
    let qids = pool.groups[group_idx].qids.clone();
    let first_qid = qids[0];
    let g = qids.len();
    // The group's NUMA node (NUMA-affinity campaign 2026-07-31): the
    // kernel routes requests to the queue of the requester's CPU, so on
    // queue-per-possible-CPU sessions qid IS a kernel cpu id and the
    // group's home node is a map lookup on any member (the membership
    // partition never lets a group span nodes; the residual set has no
    // known node by construction). Testing queue overrides break the
    // correspondence — no per-queue node, no placement.
    let queue_node = if pool.qid_is_cpu {
        // First member with a KNOWN node (residual-set members report
        // none — offline/unmapped possible CPUs).
        qids.iter()
            .find_map(|&q| crate::numa_core::topology().node_of_cpu(q as usize))
    } else {
        None
    };
    // Affinity posture (transport-ingress campaign 2026-08-01): node
    // scope by DEFAULT — the worker keeps its group's home node (arena
    // binding + reply-serve locality unchanged) but may run on any
    // process-mask CPU of it, so the reap wake stops paying the pinned
    // core's runqueue wait (the same hostage mechanism the fuse3-tpc
    // lanes measured; that wait lands in the KERNEL-SIDE residue term).
    // `SQUEEZEFS_FUSE_PIN_SCOPE=core` restores the pre-campaign posture
    // (a multi-queue group core-pins to its FIRST member's core — the
    // closest analog of the retired per-queue pin).
    match crate::raw::affinity::pin_scope() {
        crate::raw::affinity::PinScope::Core => {
            // Best-effort pin to the home core; when the exact core is
            // outside the process mask (taskset-restricted mounts) fall
            // back to the group's NODE cpu set (intersected with the
            // process mask) so the worker's reply serves and the arenas
            // stay co-located.
            if !core_affinity::set_for_current(core_affinity::CoreId {
                id: first_qid as usize,
            }) {
                if let Some(n) = queue_node {
                    if crate::numa_core::placement_active() {
                        let _ = crate::numa_core::topology().pin_current_to_node(n);
                    }
                }
            }
        }
        crate::raw::affinity::PinScope::Node => {
            // Home = cpu first_qid when qid IS a kernel cpu id; the
            // derivation degrades to the whole process mask for testing
            // queue overrides / unknown nodes / masks excluding the node.
            let topo = crate::numa_core::topology();
            let avail = crate::raw::affinity::process_cpus();
            let cpus = crate::raw::affinity::scoped_affinity_cpus(
                crate::raw::affinity::PinScope::Node,
                first_qid as usize,
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

    // The group ring carries every member's ents: SQ sized to the group's
    // aggregate depth (the same per-queue formula, aggregate-scaled).
    let group_depth = g * depth;
    let sq_entries = (group_depth as u32 + 8).next_power_of_two().max(16);
    // §5.3 D3.b: plain SQE128 ring by default; SQPOLL leader/attach
    // topology when the session knobs opted in (see `build_queue_ring` —
    // the group containing qid 0 is the leader).
    let mut ring: Ring =
        build_queue_ring(sq_entries, first_qid, pool.sqpoll.as_ref(), &pool.active)?;
    // FUSE-3f: probe the feature word ONCE per ring (portable-by-default:
    // probe, never a kernel-version check) and watch the overflow counter
    // after every `cq.sync()` below. A dropped completion is a REGISTER or
    // COMMIT_AND_FETCH that never lands.
    let mut cq_drops = CqDropWatch::new(ring.params().is_feature_nodrop());
    if !cq_drops.nodrop() {
        debug!(
            "fuse-over-uring qids={qids:?}: kernel lacks IORING_FEAT_NODROP — a full              CQ drops completions (cqsize={}, worst pass {}); watching              transport_cq_overflows",
            sq_entries * 2,
            group_depth * 2 + 1
        );
    }

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
    // post-probe refusals never silently downgrade). Grouping never
    // composes with kmbuf in this lever (the plan derives width 1 under
    // BufRing), so the per-ring registration keeps its exact shape.
    let zc_mode = pool.buffer_mode == TransportBufferMode::ZeroCopy;
    // The zc opcode mirror is TRACK-KEYED (2026-08-06 divergence fix):
    // the running kernel's fuse tree decides which opcodes are paged
    // (cachyos 7.1 pages readdir; elrepo 6.19 does not), so the worker
    // captures the ladder-resolved track once. zc_mode ⇒ the surface
    // probed Present ⇒ a track exists; a None here (impossible by that
    // chain) simply keeps every request on the kmbuf path.
    let zc_track: Option<kmbuf::KmbufTrack> = if zc_mode {
        kmbuf::resolved_track()
    } else {
        None
    };
    let kmbuf_q: Option<Arc<KmbufQueue>> = match pool.buffer_mode {
        TransportBufferMode::BufRing | TransportBufferMode::ZeroCopy => {
            debug_assert_eq!(
                g, 1,
                "BufRing/zc sessions run singleton drain groups by plan"
            );
            Some(Arc::new(KmbufQueue::setup(
                &ring, depth, payload_sz, zc_mode,
            )?))
        }
        TransportBufferMode::UserEnts => None,
    };
    // zc mode (K1 kill): the queue's memfd bounce arena — the CPU-side
    // twin of the sparse slot table. Refusal fails the worker → the mount
    // (post-probe refusals never silently downgrade).
    let zc_bounce: Option<Arc<ZcBounce>> = if zc_mode {
        Some(Arc::new(ZcBounce::new(depth, payload_sz)?))
    } else {
        None
    };

    // Per-member state: exactly the retired per-queue worker's locals,
    // one set per member queue. Payload memory lives in Arc'd per-QUEUE
    // arenas (not the worker-local Ent) so FUSE_WRITE leases and
    // `get_payload_buffer` pointers stay valid past worker exit (§5.4);
    // each arena shares the GROUP's wake coalescer so lease-drop wakes
    // elide through the same flag as reply submissions. kmbuf mode: the
    // arena is a bid-indexed view over the queue's mmap'd kernel buffer
    // region (mapping owned by KmbufQueue, held alive by the arena for
    // lease lifetimes).
    struct MemberState {
        qid: u16,
        ents: Vec<Ent>,
        slots: SlotTable,
        parked_msgs: Vec<Option<CommitMsg>>,
        lease_states: Vec<Arc<EntLeaseState>>,
        arena: Arc<PayloadArena>,
        node: Option<usize>,
        /// zc mode: one pending sparse-slot bridge per ent (at most one —
        /// an ent serves one request between two commits). `None`s on
        /// non-zc sessions.
        zc_pend: Vec<Option<ZcPend>>,
        /// The bounded-outcome ledger for `zc_pend` (2026-08-07): stamp
        /// at issue, clear at resolution, cancel-once past the deadline.
        bridge_deadlines: zc::BridgeDeadlines,
    }
    let wake_coalescer = Arc::clone(&pool.groups[group_idx].wake_coalescer);
    // zc-write-fusion (2026-08-07): the per-worker fused lane — a bounded
    // executor that runs SMALL armed-WRITE handler futures on THIS thread
    // (the store/extract bridge round trip then pays zero cross-thread
    // wakes). Capacity = the group's aggregate ent depth (a fused task
    // exists only while its ent owes a reply — the bound is structural);
    // wakes ride the group's own coalescer+eventfd producer protocol.
    let fusion_on = zc_mode && fused::fusion_enabled();
    let fusion_max = fused::fusion_ceiling(payload_sz_cfg);
    let mut fused_lane = fused::FusedLane::new(group_depth, Arc::clone(&wake_coalescer), wake_fd);
    {
        let (watch, rq) = fused_lane.watch_handle();
        pool.fused_watches
            .lock()
            .expect("fused watch registry poisoned-free")
            .push((watch, rq));
    }
    let mut members: Vec<MemberState> = Vec::with_capacity(g);
    for &qid in qids.iter() {
        let member_node = if pool.qid_is_cpu {
            crate::numa_core::topology().node_of_cpu(qid as usize)
        } else {
            None
        };
        // Lease/dest arena per mode: classical = the anon arena; kmbuf =
        // the attached-buffer region view; zc = the memfd BOUNCE (WRITE
        // leases, dest windows and reply serves all target the bounce —
        // the kmbuf attachments carry only copyable traffic there).
        let arena = match (&zc_bounce, &kmbuf_q) {
            (Some(zb), _) => {
                PayloadArena::from_zc(Arc::clone(zb), wake_fd, Arc::clone(&wake_coalescer))?
            }
            (None, Some(kq)) => {
                PayloadArena::from_kmbuf(Arc::clone(kq), wake_fd, Arc::clone(&wake_coalescer))?
            }
            (None, None) => PayloadArena::new(
                depth,
                payload_sz,
                wake_fd,
                Arc::clone(&wake_coalescer),
                member_node,
            )?,
        };
        // One lease state per ring ent (pool-level since MEM-1 — dest
        // claims acquire against the same words) + the worker-local
        // parked commit slots.
        let lease_states: Vec<Arc<EntLeaseState>> = pool.queues[qid as usize].lease_states.clone();
        debug_assert_eq!(lease_states.len(), depth, "lease states sized to depth");
        let parked_msgs: Vec<Option<CommitMsg>> = (0..depth).map(|_| None).collect();

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
        // MEM-1: publish the queue's dest-claim geometry (set exactly once
        // — each qid belongs to exactly one group worker;
        // `lease_dest_window` resolves claims by address containment
        // against this window).
        let _ = pool.queues[qid as usize].dest_window.set(DestWindow {
            base: arena.base,
            span: arena.span,
            stride: arena.stride,
            arena: Arc::clone(&arena),
            // zc mode: the window is the ENT-indexed bounce arena — a
            // dest claim resolves its ent by stride directly (the kmbuf
            // bid indirection applies only when the window IS the kmbuf
            // region).
            kmbuf: if zc_mode { None } else { kmbuf_q.clone() },
        });

        members.push(MemberState {
            qid,
            ents,
            // FUSE-2: the queue's slot state machine — one state per ring
            // ent, owned exclusively by THIS thread (see [`SlotState`]
            // for the exactly-one-reply invariant it enforces).
            slots: SlotTable::new(depth),
            parked_msgs,
            lease_states,
            arena,
            node: member_node,
            zc_pend: (0..depth).map(|_| None).collect(),
            bridge_deadlines: zc::BridgeDeadlines::new(depth),
        });
    }

    // Group-local ent id: `gent = member_slot × depth + ent_idx` — the
    // reply-address word this ring's SQEs carry (round-trip pinned by
    // `drain_group_tests::gent_user_data_round_trips`).
    let gent_of = |mi: usize, ent: usize| mi * depth + ent;

    /// Commit one ADMITTED, lease-clear reply — the zc routing point
    /// shared by the fresh drain and the unpark scan (both must route,
    /// or a lease-parked paged reply would commit its body into a buffer
    /// the kernel will never copy from):
    ///
    /// * `prefilled` (zc direct leg): header + `payload_sz` only — the
    ///   bytes already sit in the request's pages.
    /// * zc out-paged with a body: stage the body into the ent's BOUNCE
    ///   slot and bridge it into the request's pages with
    ///   `READ_FIXED(memfd → slot)`; the commit parks until the bridge
    ///   CQE (`ZcPend::BounceFetch`) — the message is kept whole so a
    ///   failed bridge can fall back to the kmbuf attachment (the
    ///   opcode-mirror safety net).
    /// * everything else: today's `apply_reply` + COMMIT, byte-identical.
    #[allow(clippy::too_many_arguments)] // ring + batch + the member's split fields + the address
    fn commit_ready_reply(
        ring: &mut Ring,
        batch: &mut SubmitBatch,
        slots: &mut SlotTable,
        ent: &mut Ent,
        zc_pend_slot: &mut Option<ZcPend>,
        deadlines: &mut zc::BridgeDeadlines,
        bridge_gauge: &AtomicU64,
        zc_bounce: Option<&Arc<ZcBounce>>,
        zc_track: Option<kmbuf::KmbufTrack>,
        watch: Option<&SlotWatch>,
        qid: u16,
        gent: usize,
        msg: CommitMsg,
    ) -> io::Result<()> {
        let idx = gent % slots.len();
        if let Some(n) = msg.prefilled {
            apply_reply_zc(ent, &msg.header, n);
            kmbuf::note_zc_reply();
            batch.note_commit_opcode(ent.last_opcode);
            return submit_commit(ring, batch, slots, watch, qid, gent, msg.commit_id);
        }
        if let Some((zb, _)) = zc_bounce
            .zip(zc_track)
            .filter(|(_, track)| zc::out_paged(ent.last_opcode, *track))
        {
            let extra = msg.header.len().saturating_sub(16);
            let body_len = msg.reply_body.len();
            if extra > 0 {
                // Out-paged replies are pure body by construction
                // (READ/READDIR/READLINK carry no out-header extra); a
                // reply shaped otherwise is a protocol bug — refuse it
                // the FUSE-3d way, never ship bytes the kernel won't
                // move.
                error!(
                    "fuse-over-uring qid={qid} ent={idx}: out-paged reply carries \
                     {extra} header-extra bytes on a zc queue — committing EIO"
                );
                let unique = u64::from_le_bytes(msg.header[8..16].try_into().unwrap());
                apply_reply(ent, &error_out_header(unique, libc::EIO), &Bytes::new());
                batch.note_commit_opcode(ent.last_opcode);
                return submit_commit(ring, batch, slots, watch, qid, gent, msg.commit_id);
            }
            if body_len > 0 {
                let (Some(dst), Some(off)) = (zb.buf_ptr(idx), zb.offset_of(idx)) else {
                    error!("fuse-over-uring qid={qid} ent={idx}: zc bounce has no slot");
                    let unique = u64::from_le_bytes(msg.header[8..16].try_into().unwrap());
                    apply_reply(ent, &error_out_header(unique, libc::EIO), &Bytes::new());
                    batch.note_commit_opcode(ent.last_opcode);
                    return submit_commit(ring, batch, slots, watch, qid, gent, msg.commit_id);
                };
                if body_len > zb.stride() {
                    // FUSE-3d's zc face: oversize is refused loud, never
                    // truncated under a full-length header.
                    error!(
                        "fuse-over-uring qid={qid} ent={idx}: zc reply body {body_len} B \
                         exceeds the bounce stride ({} B) — committing EIO",
                        zb.stride()
                    );
                    TRANSPORT_REPLIES_OVERSIZE.fetch_add(1, Ordering::Relaxed);
                    let unique = u64::from_le_bytes(msg.header[8..16].try_into().unwrap());
                    apply_reply(ent, &error_out_header(unique, libc::EIO), &Bytes::new());
                    batch.note_commit_opcode(ent.last_opcode);
                    return submit_commit(ring, batch, slots, watch, qid, gent, msg.commit_id);
                }
                // Stage into the bounce slot; dest-armed serves already
                // wrote there (`get_payload_buffer` hands out the bounce
                // in zc mode), so the common warm path elides this copy
                // exactly like `apply_reply`'s ptr-equality elision.
                if !std::ptr::eq(msg.reply_body.as_ptr(), dst) {
                    // SAFETY: dst is this ent's bounce slot (exclusively
                    // this request's between two commits — the §5.4
                    // ownership argument) and `body_len ≤ stride` was
                    // checked above.
                    unsafe {
                        std::ptr::copy_nonoverlapping(msg.reply_body.as_ptr(), dst, body_len);
                    }
                }
                let entry = Entry128::from(
                    opcode::ReadFixed::new(
                        types::Fd(zb.fd()),
                        std::ptr::null_mut(),
                        body_len as u32,
                        idx as u16,
                    )
                    .offset(off)
                    .build()
                    .user_data(encode_user_data(RingOp::Fetch, gent)),
                );
                match push_fetch_batched(ring, batch, entry) {
                    Ok(()) => {
                        slots.on_commit_parked(idx);
                        *zc_pend_slot = Some(ZcPend::BounceFetch {
                            header: msg.header,
                            body: msg.reply_body,
                            commit_id: msg.commit_id,
                            len: body_len as u32,
                        });
                        if deadlines.stamp(idx, crate::raw::read_phase::transport_now_ns()) {
                            bridge_gauge.fetch_add(1, Ordering::Relaxed);
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        error!(
                            "fuse-over-uring qid={qid} ent={idx}: zc bounce bridge push \
                             failed ({e}); committing EIO"
                        );
                        kmbuf::note_zc_fallback();
                        let unique = u64::from_le_bytes(msg.header[8..16].try_into().unwrap());
                        apply_reply(ent, &error_out_header(unique, libc::EIO), &Bytes::new());
                        batch.note_commit_opcode(ent.last_opcode);
                        return submit_commit(ring, batch, slots, watch, qid, gent, msg.commit_id);
                    }
                }
            }
            // Zero-length paged reply (EOF, empty dir page): header-only
            // — the normal path below is already slot-safe.
        }
        apply_reply(ent, &msg.header, &msg.reply_body);
        batch.note_commit_opcode(ent.last_opcode);
        // ACK-early (0029): a WRITE reply whose handler armed RETAIN
        // commits with the flag — the kernel ends the request (the
        // application's write returns NOW) and parks the ent until the
        // store continuation's RELEASE. Every other arm of this routing
        // fn is read-shaped and stays plain.
        submit_commit_retain(
            ring,
            batch,
            slots,
            watch,
            qid,
            gent,
            msg.commit_id,
            msg.retain,
        )
    }

    /// Resolve one zc bridge CQE (`RingOp::Fetch`) against the ent's
    /// pending kind. `res` is the raw ring result — full-length success
    /// is the ONLY good outcome for worker-owned bridges (a short slot
    /// read/write has no resume protocol in v1); handler fetches get the
    /// raw result verbatim (the handler owns validation + fallback).
    #[allow(clippy::too_many_arguments)] // ring + batch + the member's split fields + the address
    fn zc_fetch_complete(
        ring: &mut Ring,
        batch: &mut SubmitBatch,
        slots: &mut SlotTable,
        ent: &mut Ent,
        zc_pend_slot: &mut Option<ZcPend>,
        watch: Option<&SlotWatch>,
        qid: u16,
        gent: usize,
        res: i32,
    ) -> io::Result<Option<PendDone>> {
        let idx = gent % slots.len();
        match zc_pend_slot.take() {
            None => {
                warn!(
                    "fuse-over-uring qid={qid} ent={idx}: zc bridge CQE (res={res}) with no \
                     pending bridge — dropping"
                );
                Ok(None)
            }
            Some(ZcPend::HandlerFetch { done }) | Some(ZcPend::HandlerStore { done }) => {
                // A dropped receiver = the handler gave up (teardown);
                // nothing owed here — its reply path owns the slot. The
                // store's engagement ledger is counted at the CONSUMING
                // site (the root patch path) after ITS validation, so a
                // caller-side length refusal never leaves a phantom
                // count.
                let _ = done.send(res);
                Ok(None)
            }
            Some(ZcPend::BounceFetch {
                header,
                body,
                commit_id,
                len,
            }) => {
                if res == len as i32 {
                    apply_reply_zc(ent, &header, len);
                    kmbuf::note_zc_reply();
                    batch.note_commit_opcode(ent.last_opcode);
                    submit_commit(ring, batch, slots, watch, qid, gent, commit_id)?;
                    return Ok(None);
                }
                // The opcode-mirror safety net: a slot with no registered
                // pages (the kernel served this op copyable) errors here —
                // fall back to the kmbuf attachment when one exists, else
                // an honest EIO. Loud + counted either way.
                kmbuf::note_zc_fallback();
                warn!(
                    "fuse-over-uring qid={qid} ent={idx}: zc bounce bridge failed \
                     (res={res}, want={len}, opcode={}, kmbuf_ops={}) — the kernel \
                     served this opcode copyable while zc::out_paged bridges it on \
                     this track; falling back to {}",
                    ent.last_opcode,
                    kmbuf::resolved_opcodes_label(),
                    if ent.has_payload_buf() {
                        "the kmbuf attachment"
                    } else {
                        "EIO"
                    }
                );
                if ent.has_payload_buf() {
                    apply_reply(ent, &header, &body);
                } else {
                    let unique = if header.len() >= 16 {
                        u64::from_le_bytes(header[8..16].try_into().unwrap())
                    } else {
                        0
                    };
                    apply_reply(ent, &error_out_header(unique, libc::EIO), &Bytes::new());
                }
                batch.note_commit_opcode(ent.last_opcode);
                submit_commit(ring, batch, slots, watch, qid, gent, commit_id)?;
                Ok(None)
            }
            Some(ZcPend::LazyExtract { done, len }) => {
                if res == len as i32 {
                    // The payload now exists in the bounce slot: hand the
                    // completed extraction back to the caller, which
                    // mints the §5.4 lease over the bounce bytes and
                    // answers the parked handler (it owns the member's
                    // arena/lease references). Engagement face
                    // (write-bracket campaign): count the completed
                    // extraction + its payload bytes — PLUS the LATE
                    // split (fused-lane-predicate, 2026-08-08): a lazy
                    // extraction on a held write means the hold gate's
                    // eligibility hint went STALE between delivery and
                    // handler (or the handler's authoritative predicate
                    // declined a racing overlay/refcount) — bounded and
                    // counted, ≈ 0 in steady state; growth here names a
                    // predicate-drift bug.
                    kmbuf::note_zc_write_extraction(u64::from(len));
                    fused::note_zc_write_lazy_extraction();
                    return Ok(Some(PendDone::Lazy { done, len }));
                }
                kmbuf::note_zc_fallback();
                error!(
                    "fuse-over-uring qid={qid} ent={idx}: zc WRITE extraction failed \
                     (res={res}, want={len}, kmbuf_ops={}) — answering the handler \
                     with EIO",
                    kmbuf::resolved_opcodes_label()
                );
                let _ = done.send(Err(io::Error::from_raw_os_error(libc::EIO)));
                Ok(None)
            }
            Some(ZcPend::WriteExtract {
                header_and_op,
                unique,
                commit_id,
                len,
            }) => {
                if res == len as i32 {
                    // The payload now exists in the bounce slot: hand the
                    // deferred delivery back to the caller, which mints
                    // the §5.4 lease and pushes inbound (it owns the
                    // member's arena/lease/pool references). Engagement
                    // face: count the completed extraction + bytes.
                    kmbuf::note_zc_write_extraction(u64::from(len));
                    return Ok(Some(PendDone::Deliver {
                        header_and_op,
                        unique,
                        commit_id,
                        len,
                    }));
                }
                kmbuf::note_zc_fallback();
                error!(
                    "fuse-over-uring qid={qid} ent={idx}: zc WRITE at-delivery extraction \
                     failed (res={res}, want={len}, kmbuf_ops={}) — synthesizing EIO \
                     for unique={unique}",
                    kmbuf::resolved_opcodes_label()
                );
                Ok(Some(PendDone::DeliverFailed))
            }
        }
    }

    /// A completed zc bridge outcome handed from [`zc_fetch_complete`]
    /// back to the loop body (which owns the member's arena, lease and
    /// pool references).
    enum PendDone {
        /// A LAZY extraction completed: mint the §5.4 lease over the
        /// bounce bytes and answer the parked handler's oneshot.
        Lazy {
            done: crate::sqz_channel::oneshot::Sender<io::Result<Bytes>>,
            len: u32,
        },
        /// An AT-DELIVERY extraction completed: mint the lease and push
        /// the deferred delivery inbound.
        Deliver {
            header_and_op: Vec<u8>,
            unique: u64,
            commit_id: u64,
            len: u32,
        },
        /// An at-delivery extraction FAILED: the request cannot be
        /// served — the caller synthesizes its EIO (row-5 discipline).
        DeliverFailed,
    }
    // Membership lookup, not offset arithmetic: groups are node-membership
    // SETS (interleaved numberings are the field norm). A linear scan over
    // ≤ width members beats a map at these sizes.
    let member_of_qid = |q: u16| -> Option<usize> { qids.iter().position(|&x| x == q) };

    // REGISTER shape per mode: classical = 2 iovecs (header + payload);
    // kmbuf = no iovecs, `init.flags = FUSE_URING_BUF_RING`,
    // `sqe->buf_index = ent_idx` (the ent's fixed_buf_id — kmbuf groups
    // are singletons, so gent == ent_idx there).
    let reg_init_flags: u16 = match pool.buffer_mode {
        TransportBufferMode::BufRing => kmbuf::init_flags(true, false),
        // Retention bit 2 composes onto the zc arm only (kernel law:
        // retention without ZERO_COPY refuses EINVAL). The SAME flags
        // serve initial REGISTER and every re-REGISTER — the kernel's
        // re-REGISTER consistency check refuses a queue whose retention
        // bit differs from its creation-time posture.
        TransportBufferMode::ZeroCopy => {
            kmbuf::init_flags_with_retention(true, true, pool.retention)
        }
        TransportBufferMode::UserEnts => 0,
    };
    // zc REGISTERs carry `init.queue_depth` (the kernel refuses zc with a
    // zero depth and derives the headers table index from it).
    let reg_queue_depth: u16 = if zc_mode { depth as u16 } else { 0 };
    let reg_iov = |ent: &Ent| -> Option<(*const libc::iovec, u32)> {
        match pool.buffer_mode {
            TransportBufferMode::BufRing | TransportBufferMode::ZeroCopy => None,
            TransportBufferMode::UserEnts => Some((ent.iov.as_ptr(), 2)),
        }
    };
    let reg_buf_index = |idx: usize| -> u16 {
        match pool.buffer_mode {
            TransportBufferMode::BufRing | TransportBufferMode::ZeroCopy => idx as u16,
            TransportBufferMode::UserEnts => 0,
        }
    };

    for (mi, m) in members.iter_mut().enumerate() {
        for idx in 0..depth {
            let iov = reg_iov(&m.ents[idx]);
            push_cmd(
                &mut ring,
                FUSE_IO_URING_CMD_REGISTER,
                m.qid,
                0,
                iov,
                encode_user_data(RingOp::Register, gent_of(mi, idx)),
                reg_init_flags,
                reg_queue_depth,
                reg_buf_index(idx),
            )
            .map_err(|e| io::Error::other(format!("push REGISTER qid={} ent={idx}: {e}", m.qid)))?;
            m.slots.on_register_submitted(idx);
            pool.stats_register.fetch_add(1, Ordering::Relaxed);
            STATS_REGISTER.fetch_add(1, Ordering::Relaxed);
        }
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
    // FUSE-3c (adjudicated UNIMPLEMENTABLE AS WRITTEN — do not "fix" this to
    // count completions): the barrier counts SUBMISSIONS by necessity. A
    // REGISTER's CQE is not a registration ack — the kernel parks the ent
    // and posts its completion only when it DELIVERS a request on it (see
    // the delivery handling below, which reads the inbound header straight
    // out of a successful `RingOp::Register` CQE), and it only routes
    // requests to the ring once `is_ring_ready()` observes every queue
    // armed. Waiting for `depth` completions here would therefore deadlock
    // the arm: no readiness ⇒ no delivery ⇒ no CQE ⇒ no readiness. The real
    // concern — a REFUSED REGISTER going unnoticed — is covered by FUSE-3a:
    // a refusal arrives as a negative-result CQE, backs off, retires the ent
    // after `REGISTER_RETRY_MAX` (`transport_ents_retired`), and fails the
    // session when every ent has retired.
    pool.queues_registered.fetch_add(g as u64, Ordering::AcqRel);

    // §5.3 D3.a (S2) submit economy: the passes below PUSH their SQEs
    // (commits, poll re-arms, re-REGISTERs) through the batched helpers and
    // share ONE syscall — the loop-bottom `submit_and_wait(1)` flushes the
    // batch on its way into the wait (one `io_uring_enter` = submission +
    // wait). Only the syscall is shared: the §5.4 lease re-arm gate still
    // runs per ent *before* its SQE is pushed.
    let mut batch = SubmitBatch::default();
    // Row 7: sticky across passes — see the re-arm site below.
    let mut need_repoll_sticky = false;
    // Mid-pass-reaped CQEs the narrow bridge arm does NOT resolve —
    // handed to the pass-bottom machinery verbatim, ahead of the
    // post-wait CQ drain (arrival order preserved). Hoisted so passes
    // reuse the allocation.
    let mut deferred_cqes: Vec<(u64, i32, u32)> = Vec::new();

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
        drain_wake_eventfd(wake_fd);
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

        // Fused-lane drain + WorkerMsg pump, INTERLEAVED (zc-write-fusion;
        // DMA-overlap fix 2026-08-11): the retired shape polled EVERY
        // ready fused handler first and converted their WorkerMsgs to
        // SQEs after, so a pass's bridge DMAs all launched at the
        // pass-bottom enter — serially AFTER the pass's handler CPU. On
        // the 32-CPU fabric rig that self-clocked loop capped rand-4k at
        // ~7.5 ops/pass × 32 queues / ~600 µs ≈ 420k IOPS with devices
        // busy ~50 µs of each pass (avg in-flight ≈ 21 — an emergent
        // equilibrium, not a budget). The driver below alternates: pump
        // every pending WorkerMsg into SQEs, EAGER-flush the ring, THEN
        // poll ONE fused handler — so each write's device DMA runs under
        // the NEXT handler's CPU. No width constant anywhere: the
        // overlap scales with the run queue and the queue count (=
        // possible CPUs).
        //
        // §5.4 re-arm gate unchanged: a COMMIT_AND_FETCH both writes the
        // reply into the ent payload and re-arms the registered buffers
        // for the kernel — never legal while a payload lease is live.
        // Gate every commit; park the message when leased and rely on
        // the lease drop's eventfd wake.
        let mut fused_more = fused_lane.len() > 0;
        // Work-conserving pass (write-IOPS campaign, T3 correction): the
        // signals below decide the pass-bottom PARK. A pass that pumped a
        // message, polled a fused task, or holds deferred completions
        // skips the blocking wait entirely — the loop spins back through
        // the eventfd drain and the interleave, so a delivery dispatched
        // at THIS pass's bottom is polled at the NEXT iteration's top
        // instead of waiting out a park (T3 named delivery quantization
        // as the ~2 ms residual: commits re-arm ents whose fresh
        // deliveries landed mid-pass and then waited for the park).
        // Idle passes park exactly as before — a quiet mount burns
        // nothing.
        let mut pass_polls: usize = 0;
        let mut pass_msgs: usize = 0;
        macro_rules! next_wmsg {
            () => {{
                let mut got: Option<WorkerMsg> = None;
                loop {
                    if let Ok(m) = commit_rx.try_recv() {
                        pass_msgs += 1;
                        got = Some(m);
                        break;
                    }
                    if !fused_more {
                        break;
                    }
                    // Launch the PREVIOUS poll's SQEs before burning the
                    // next poll's CPU, and MATERIALIZE completions while
                    // at it: these rings run DEFER_TASKRUN, where CQEs
                    // land only under a GETEVENTS enter — a plain submit
                    // left the mid-pass reap syncing an eternally-empty
                    // CQ (T1: midpass_reaps +0 / 22.85M pass-bottom).
                    // Gated on work being possible: pending SQEs to
                    // launch, or live bridge pends whose CQEs could land.
                    // (An error here is deferred to the pass-bottom
                    // enter, which owns the disconnect handling.)
                    let live_pends: usize = members
                        .iter()
                        .map(|m| m.bridge_deadlines.outstanding())
                        .sum();
                    if batch.pending > 0 || live_pends > 0 {
                        if let Err(e) = flush_submit_getevents(&mut ring, &mut batch) {
                            warn!(
                                "fuse-over-uring qids={qids:?}: eager bridge flush \
                                 failed ({e}); deferring to the pass-bottom submit"
                            );
                            fused_more = false;
                            continue;
                        }
                    }
                    // Mid-pass reap: resolve finished HANDLER bridges NOW
                    // (raw `done.send` + deadline clear — byte-identical
                    // to `zc_fetch_complete`'s arm for these two pends),
                    // so their tasks re-poll AND COMMIT in this same
                    // pass; every other completion class defers verbatim
                    // to the pass-bottom machinery, same-pass, later
                    // point.
                    {
                        let mut cq = ring.completion();
                        cq.sync();
                        let dropped = cq_drops.observe(cq.overflow());
                        if dropped > 0 {
                            error!(
                                "fuse-over-uring qids={qids:?}: kernel DROPPED {dropped} \
                                 completion(s) (CQ overflow, nodrop={}) — the ents they \
                                 belonged to are stalled; transport_cq_overflows",
                                cq_drops.nodrop()
                            );
                        }
                        for c in cq {
                            let (user_data, res, flags) = (c.user_data(), c.result(), c.flags());
                            let Some((op, gent)) = decode_user_data(user_data) else {
                                // wake-fd poll completed — re-arm later.
                                need_repoll_sticky = true;
                                continue;
                            };
                            let deferrable = op != RingOp::Fetch
                                || gent >= members.len() * depth
                                || !matches!(
                                    members[gent / depth].zc_pend[gent % depth],
                                    Some(ZcPend::HandlerFetch { .. })
                                        | Some(ZcPend::HandlerStore { .. })
                                );
                            if deferrable {
                                deferred_cqes.push((user_data, res, flags));
                                continue;
                            }
                            let (mi, ent_idx) = (gent / depth, gent % depth);
                            let m = &mut members[mi];
                            // TEST SEAM (zc-bridge-cqe-wedge): same
                            // consume-and-drop as the pass-bottom check —
                            // consulted here ONLY for the class this arm
                            // resolves, so the seam budget is popped once
                            // per CQE.
                            if matches!(m.zc_pend[ent_idx], Some(ZcPend::HandlerStore { .. }))
                                && test_drop_write_cqe()
                            {
                                error!(
                                    "fuse-over-uring qid={} ent={ent_idx}: TEST SEAM dropping \
                                     WRITE-class bridge CQE (res={res}) — pend stays live",
                                    m.qid
                                );
                                continue;
                            }
                            match m.zc_pend[ent_idx].take() {
                                Some(ZcPend::HandlerFetch { done })
                                | Some(ZcPend::HandlerStore { done }) => {
                                    // Fused timeline: issue → resolution,
                                    // mid-pass venue (the funnel fix's
                                    // engagement instrument).
                                    let born = m.bridge_deadlines.born_ns(ent_idx);
                                    if born != 0 {
                                        crate::raw::read_phase::note_fused_bridge_resolved(
                                            crate::raw::read_phase::transport_now_ns()
                                                .saturating_sub(born),
                                            true,
                                        );
                                    }
                                    let _ = done.send(res);
                                }
                                other => {
                                    // Checked two branches up; keep the
                                    // pend intact rather than lose it.
                                    m.zc_pend[ent_idx] = other;
                                    deferred_cqes.push((user_data, res, flags));
                                    continue;
                                }
                            }
                            if m.bridge_deadlines.clear(ent_idx) {
                                pool.zc_bridge_pends.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                    }
                    // Burst-drain (T2 correction): poll EVERY currently-
                    // ready task between enters — one flush+GETEVENTS per
                    // burst instead of per poll (T2 priced the per-poll
                    // enter at ~2.7 syscalls/op: bridge RTT fell 10× and
                    // the row still lost 6 % to the lengthened pass). The
                    // burst's own wakes (reap resolutions) land in the
                    // ready queue and form the NEXT burst after the next
                    // flush, so DMA launch stays eager per burst.
                    fused_more = match pool.fused_dispatch.get() {
                        // Presence gate only: a registered dispatcher
                        // means this lane may drain (no handle travels
                        // since rip-tokio-total).
                        Some(_) => {
                            let mut any = false;
                            while fused_lane.drain_one() {
                                any = true;
                                pass_polls += 1;
                                if let Ok(m) = commit_rx.try_recv() {
                                    pass_msgs += 1;
                                    got = Some(m);
                                    break;
                                }
                            }
                            any
                        }
                        None => false,
                    };
                    if got.is_some() {
                        break;
                    }
                }
                got
            }};
        }
        while let Some(wmsg) = next_wmsg!() {
            let msg = match wmsg {
                WorkerMsg::Commit(msg) => msg,
                WorkerMsg::ZcFetch(f) => {
                    // zc direct leg: push the device→slot READ_FIXED; the
                    // handler parks on the oneshot until its CQE. Refusals
                    // answer the oneshot with a negative errno (the
                    // handler falls back to the normal serve ladder).
                    let idx = f.ent_idx as usize;
                    let Some(mi) = member_of_qid(f.qid).filter(|_| idx < depth) else {
                        warn!(
                            "fuse-over-uring qids={qids:?}: zc fetch for bad slot qid={} ent={idx}",
                            f.qid
                        );
                        let _ = f.done.send(-libc::EINVAL);
                        continue;
                    };
                    let m = &mut members[mi];
                    if m.zc_pend[idx].is_some() {
                        warn!(
                            "fuse-over-uring qid={} ent={idx}: zc fetch on a slot with a \
                             pending bridge — refusing",
                            f.qid
                        );
                        let _ = f.done.send(-libc::EBUSY);
                        continue;
                    }
                    let gent = gent_of(mi, idx);
                    let entry = Entry128::from(
                        opcode::ReadFixed::new(
                            types::Fd(f.fd),
                            std::ptr::null_mut(),
                            f.len,
                            idx as u16,
                        )
                        .offset(f.off)
                        .build()
                        .user_data(encode_user_data(RingOp::Fetch, gent)),
                    );
                    match push_fetch_batched(&mut ring, &mut batch, entry) {
                        Ok(()) => {
                            m.zc_pend[idx] = Some(ZcPend::HandlerFetch { done: f.done });
                            if m.bridge_deadlines
                                .stamp(idx, crate::raw::read_phase::transport_now_ns())
                            {
                                pool.zc_bridge_pends.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            warn!(
                                "fuse-over-uring qid={} ent={idx}: zc fetch push failed ({e})",
                                f.qid
                            );
                            let _ = f.done.send(-libc::EIO);
                        }
                    }
                    continue;
                }
                WorkerMsg::ZcStore(s) => {
                    pool.zc_msgs_taken.fetch_add(1, Ordering::Relaxed);
                    // D14 direct write leg: WRITE_FIXED(device fd ← the
                    // ent's held source pages). Refusals answer the
                    // oneshot with a negative errno (the handler falls
                    // back to the extraction vehicle).
                    let idx = s.ent_idx as usize;
                    let Some(mi) = member_of_qid(s.qid).filter(|_| idx < depth) else {
                        warn!(
                            "fuse-over-uring qids={qids:?}: zc store for bad slot qid={} ent={idx}",
                            s.qid
                        );
                        let _ = s.done.send(-libc::EINVAL);
                        continue;
                    };
                    let m = &mut members[mi];
                    let Some(len) = pool.zc_write_held.get(s.qid, idx) else {
                        warn!(
                            "fuse-over-uring qid={} ent={idx}: zc store with no held \
                             payload — refusing",
                            s.qid
                        );
                        let _ = s.done.send(-libc::ENOENT);
                        continue;
                    };
                    if m.zc_pend[idx].is_some() {
                        warn!(
                            "fuse-over-uring qid={} ent={idx}: zc store on a slot with a \
                             pending bridge — refusing",
                            s.qid
                        );
                        let _ = s.done.send(-libc::EBUSY);
                        continue;
                    }
                    let gent = gent_of(mi, idx);
                    let entry = Entry128::from(
                        opcode::WriteFixed::new(types::Fd(s.fd), std::ptr::null(), len, idx as u16)
                            .offset(s.dev_off)
                            .build()
                            .user_data(encode_user_data(RingOp::Fetch, gent)),
                    );
                    match push_fetch_batched(&mut ring, &mut batch, entry) {
                        Ok(()) => {
                            // Device-overlay §4.3: the per-qid direct-store
                            // census (the fabric-queue spread instrument).
                            kmbuf::note_zc_store_qid(s.qid);
                            m.zc_pend[idx] = Some(ZcPend::HandlerStore { done: s.done });
                            if m.bridge_deadlines
                                .stamp(idx, crate::raw::read_phase::transport_now_ns())
                            {
                                pool.zc_bridge_pends.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            warn!(
                                "fuse-over-uring qid={} ent={idx}: zc store push failed ({e})",
                                s.qid
                            );
                            let _ = s.done.send(-libc::EIO);
                        }
                    }
                    continue;
                }
                WorkerMsg::ZcExtract(x) => {
                    pool.zc_msgs_taken.fetch_add(1, Ordering::Relaxed);
                    // D14 lazy extraction: WRITE_FIXED(slot → memfd) on
                    // demand — the ineligible-shape vehicle. Refusals
                    // answer the oneshot with an error (the write fails
                    // loud for exactly this request).
                    let idx = x.ent_idx as usize;
                    let Some(mi) = member_of_qid(x.qid).filter(|_| idx < depth) else {
                        warn!(
                            "fuse-over-uring qids={qids:?}: zc extract for bad slot qid={} ent={idx}",
                            x.qid
                        );
                        let _ = x.done.send(Err(io::Error::from_raw_os_error(libc::EINVAL)));
                        continue;
                    };
                    let m = &mut members[mi];
                    let Some(len) = pool.zc_write_held.get(x.qid, idx) else {
                        warn!(
                            "fuse-over-uring qid={} ent={idx}: zc extract with no held \
                             payload — refusing",
                            x.qid
                        );
                        let _ = x.done.send(Err(io::Error::from_raw_os_error(libc::ENOENT)));
                        continue;
                    };
                    if m.zc_pend[idx].is_some() {
                        warn!(
                            "fuse-over-uring qid={} ent={idx}: zc extract on a slot with a \
                             pending bridge — refusing",
                            x.qid
                        );
                        let _ = x.done.send(Err(io::Error::from_raw_os_error(libc::EBUSY)));
                        continue;
                    }
                    let Some(zb) = zc_bounce.as_ref() else {
                        let _ = x.done.send(Err(io::Error::from_raw_os_error(libc::EINVAL)));
                        continue;
                    };
                    let gent = gent_of(mi, idx);
                    let entry = Entry128::from(
                        opcode::WriteFixed::new(
                            types::Fd(zb.fd()),
                            std::ptr::null(),
                            len,
                            idx as u16,
                        )
                        .offset(zb.offset_of(idx).expect("ent in range"))
                        .build()
                        .user_data(encode_user_data(RingOp::Fetch, gent)),
                    );
                    match push_fetch_batched(&mut ring, &mut batch, entry) {
                        Ok(()) => {
                            m.zc_pend[idx] = Some(ZcPend::LazyExtract { done: x.done, len });
                            if m.bridge_deadlines
                                .stamp(idx, crate::raw::read_phase::transport_now_ns())
                            {
                                pool.zc_bridge_pends.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            warn!(
                                "fuse-over-uring qid={} ent={idx}: zc extract push failed ({e})",
                                x.qid
                            );
                            let _ = x.done.send(Err(io::Error::other(e.to_string())));
                        }
                    }
                    continue;
                }
                WorkerMsg::ZcRelease { qid: rqid, ent_idx } => {
                    // ACK-early (0029): release a retained slot. The
                    // ordering protocol: Retained ⇒ push RELEASE now;
                    // commit not yet submitted (Delivered/Parked — the
                    // store CQE beat the reply through the channels) ⇒
                    // park it, the RETAIN commit submission fires it.
                    // Anything else is the teardown race — inert.
                    let idx = ent_idx as usize;
                    let Some(mi) = member_of_qid(rqid).filter(|_| idx < depth) else {
                        warn!(
                            "fuse-over-uring qids={qids:?}: zc release for bad slot \
                             qid={rqid} ent={idx}"
                        );
                        continue;
                    };
                    let m = &mut members[mi];
                    match m.slots.state(idx) {
                        SlotState::Retained { commit_id } => {
                            push_release(
                                &mut ring,
                                &mut batch,
                                &mut m.slots,
                                rqid,
                                gent_of(mi, idx),
                                commit_id,
                            )?;
                        }
                        SlotState::Delivered { .. } | SlotState::Parked { .. } => {
                            m.slots.set_release_pending(idx);
                        }
                        other => {
                            debug!(
                                "fuse-over-uring qid={rqid} ent={idx}: stale zc release \
                                 (state {other:?}) — inert"
                            );
                        }
                    }
                    continue;
                }
            };
            let idx = msg.ent_idx as usize;
            let Some(mi) = member_of_qid(msg.qid).filter(|_| idx < depth) else {
                warn!(
                    "fuse-over-uring qids={qids:?}: commit for bad slot qid={} ent={idx}",
                    msg.qid
                );
                TRANSPORT_REPLIES_REFUSED_STALE.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let m = &mut members[mi];
            let qid = m.qid;
            // FUSE-2 / FUSE-3e: the slot decides. A commit that does not
            // address the request this slot currently owes (double reply,
            // late reply, a reply for a displaced request) is refused
            // loud-never-fatally — never allowed to overwrite a live one,
            // and never turned into a second COMMIT_AND_FETCH.
            if m.slots.admit_commit(idx, msg.commit_id) == CommitAdmit::RefuseStale {
                warn!(
                    "fuse-over-uring qid={qid} ent={idx}: refusing a stale reply \
                     (cid={} state={:?}) — the slot does not owe it",
                    msg.commit_id,
                    m.slots.state(idx)
                );
                continue;
            }
            match m.lease_states[idx].try_commit() {
                CommitGate::Ready => {
                    xport_dbg!("[XPORT] commit qid={qid} ent={idx} cid={}", msg.commit_id);
                    commit_ready_reply(
                        &mut ring,
                        &mut batch,
                        &mut m.slots,
                        &mut m.ents[idx],
                        &mut m.zc_pend[idx],
                        &mut m.bridge_deadlines,
                        &pool.zc_bridge_pends,
                        zc_bounce.as_ref(),
                        zc_track,
                        pool.slot_watch_cell(qid, idx),
                        qid,
                        gent_of(mi, idx),
                        msg,
                    )?;
                }
                CommitGate::Parked => {
                    xport_dbg!(
                        "[XPORT] commit-parked qid={qid} ent={idx} cid={}",
                        msg.commit_id
                    );
                    TRANSPORT_PARKED_COMMITS.fetch_add(1, Ordering::Relaxed);
                    m.slots.on_commit_parked(idx);
                    m.parked_msgs[idx] = Some(msg);
                }
            }
        }

        // Parked scan (runs on every wake path — lease-drop eventfd, new
        // CQEs, commit sends — and always before the worker can sleep in
        // submit_and_wait): un-park and commit every ent whose lease is
        // gone. try_unpark re-proves refs == 0, so the payload write below
        // cannot alias a live lease.
        for (mi, m) in members.iter_mut().enumerate() {
            let qid = m.qid;
            for idx in 0..depth {
                if m.parked_msgs[idx].is_some() && m.lease_states[idx].try_unpark() {
                    let msg = m.parked_msgs[idx].take().expect("checked is_some");
                    xport_dbg!(
                        "[XPORT] commit-unparked qid={qid} ent={idx} cid={}",
                        msg.commit_id
                    );
                    TRANSPORT_UNPARKED_COMMITS.fetch_add(1, Ordering::Relaxed);
                    // Same routing as the fresh drain — a lease-parked
                    // paged reply must still bridge through the slot.
                    commit_ready_reply(
                        &mut ring,
                        &mut batch,
                        &mut m.slots,
                        &mut m.ents[idx],
                        &mut m.zc_pend[idx],
                        &mut m.bridge_deadlines,
                        &pool.zc_bridge_pends,
                        zc_bounce.as_ref(),
                        zc_track,
                        pool.slot_watch_cell(qid, idx),
                        qid,
                        gent_of(mi, idx),
                        msg,
                    )?;
                }
            }
        }

        // Exit promptly when another worker/watch already shut us down (wake_fd).
        if !pool.active.load(Ordering::Relaxed) {
            break;
        }

        // The bounded-outcome law (zc-bridge-cqe-wedge, 2026-08-07):
        // every zc bridge op in flight past the deadline gets ONE
        // AsyncCancel — the original op's CQE (completed or -ECANCELED)
        // then resolves through the loud fallback ladders, so a lost
        // ring completion can wedge NOTHING beyond the deadline. The
        // cancel SQEs ride the same loop-bottom flush; the watch thread
        // wakes parked workers while any pend is outstanding, so this
        // scan runs on idle queues too.
        if pool.zc_bridge_pends.load(Ordering::Relaxed) > 0 {
            let now = crate::raw::read_phase::transport_now_ns();
            let timeout = zc_bridge_timeout_ns();
            pool.scan_passes.fetch_add(1, Ordering::Relaxed);
            let mut pends_seen = 0u64;
            let mut orphans_seen = 0u64;
            let mut max_age_ms = 0u64;
            let mut cancelled_live = 0u64;
            for m in members.iter() {
                for ent_idx in 0..depth {
                    if m.zc_pend[ent_idx].is_some() {
                        pends_seen += 1;
                        let born = m.bridge_deadlines.born_ns(ent_idx);
                        if born == 0 {
                            orphans_seen += 1;
                        } else {
                            max_age_ms = max_age_ms.max(now.saturating_sub(born) / 1_000_000);
                            if m.bridge_deadlines.cancel_latched(ent_idx) {
                                cancelled_live += 1;
                            }
                        }
                    }
                }
            }
            pool.scan_pends_seen
                .fetch_add(pends_seen, Ordering::Relaxed);
            pool.scan_orphans_seen
                .fetch_add(orphans_seen, Ordering::Relaxed);
            // GAUGES: live max pend age + live pends behind the
            // cancel-once latch (a latched pend whose cancel CQE never
            // resolved it would be a silent hole in the ladder).
            pool.scan_max_age_ms.store(max_age_ms, Ordering::Relaxed);
            pool.scan_cancel_latched
                .store(cancelled_live, Ordering::Relaxed);
            for (mi, m) in members.iter_mut().enumerate() {
                // The ORPHANED-PEND sweep (Stage-1b field wedge,
                // 2026-08-13): the live capture showed a pool with
                // zc_bridge_pends=4 while EVERY member deadline ledger
                // read 0 — a live pend without a ledger entry is
                // invisible to the deadline ladder forever (its oneshot
                // strands, the holder parks, the stripe convoys). The
                // invariant is self-healing: every `Some` pend must hold
                // a ledger entry; re-stamp any that lost theirs (loud,
                // counted) so the deadline machinery re-covers them.
                for ent_idx in 0..depth {
                    if m.zc_pend[ent_idx].is_some() && m.bridge_deadlines.born_ns(ent_idx) == 0 {
                        error!(
                            "fuse-over-uring qid={} ent={ent_idx}: LIVE zc bridge pend                              with NO deadline ledger entry — re-stamping (orphaned pend;                              fuse3_zc_bridge_orphans)",
                            m.qid
                        );
                        kmbuf::note_zc_bridge_orphan();
                        // No pends fetch_add: the orphan's ORIGINAL
                        // stamp counted it and no clear() ever -1'd —
                        // the pool gauge still carries it (that is
                        // exactly how the capture read pends=4 with
                        // empty ledgers). Re-stamping only restores
                        // ledger coverage.
                        let _ = m.bridge_deadlines.stamp(ent_idx, now);
                    }
                }
                for ent in m.bridge_deadlines.overdue(now, timeout) {
                    let gent = gent_of(mi, ent);
                    error!(
                        "fuse-over-uring qid={} ent={ent}: zc bridge op in flight past \
                         the {} ms deadline — pushing AsyncCancel; the op's own CQE \
                         resolves it (fuse3_zc_bridge_cancels)",
                        m.qid,
                        timeout / 1_000_000
                    );
                    kmbuf::note_zc_bridge_cancel();
                    let entry = Entry128::from(
                        opcode::AsyncCancel::new(encode_user_data(RingOp::Fetch, gent))
                            .build()
                            .user_data(encode_user_data(RingOp::Cancel, gent)),
                    );
                    if let Err(e) = push_fetch_batched(&mut ring, &mut batch, entry) {
                        warn!(
                            "fuse-over-uring qid={} ent={ent}: bridge AsyncCancel push \
                             failed ({e}) — the slot stays named by the watchdog",
                            m.qid
                        );
                    }
                }
            }
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
        // Row 7, mid-pass face: the interleave's reap may have consumed
        // the wake-fd poll CQE — re-arm BEFORE this pass's park, or an
        // eventfd wake (lease drop, commit send, a foreign-thread waker)
        // lands on an unarmed poll and strands until an unrelated CQE.
        // The PollAdd is level-triggered, so a wake that already raised
        // the counter completes the re-armed poll immediately; the
        // pass-bottom re-arm site stays for CQEs its own drain consumes.
        if need_repoll_sticky {
            match push_poll_batched(&mut ring, &mut batch) {
                Ok(()) => need_repoll_sticky = false,
                Err(e) => warn!(
                    "fuse-over-uring qids={qids:?}: pre-wait wake-poll re-arm push \
                     failed ({e}); retrying next pass"
                ),
            }
        }
        let next_retry = members
            .iter()
            .filter_map(|m| m.slots.next_retry_deadline())
            .min();
        // The bounded-park law (Stage-1b field wedge, 2026-08-13 —
        // docs/design-sqz-sync.md §attribution): a worker holding LIVE
        // BRIDGE PENDS or RESIDENT FUSED TASKS must never park
        // unbounded. Their resolutions arrive via the cross-thread
        // wake chain (foreign oneshot -> FusedWaker -> rq push ->
        // coalescer -> eventfd -> wake-fd PollAdd), and the field
        // capture proved that chain CAN lose exactly one wake under
        // load — zc_bridge_pends=4 with zero deadline cancels for
        // 435 s, the worker asleep in cq-wait while the watch thread
        // ticked. Owning the clock removes the dependence: the park is
        // EXT_ARG-bounded (the ipc dd-reaper's shipped 100 ms backstop
        // cadence), so the pass — deadline scan, rq drain — is
        // self-clocked. Idle workers (no pends, no fused residents)
        // keep the zero-cost unbounded park.
        // Gate on the POOL pend counter, not the per-member ledgers:
        // the 2026-08-13 live capture (all 62 workers parked UNBOUNDED
        // at zc_bridge_pends=4) proved a pend can be pool-visible while
        // every local ledger reads 0 — whatever that accounting drift
        // is, the liveness backstop must not depend on it. Coarse on
        // purpose: every worker of the pool ticks 100 ms while ANY
        // bridge pend lives; pends are rare and deadline-bounded.
        let park_backstop = (fused_lane.len() > 0
            || pool.zc_bridge_pends.load(Ordering::Relaxed) > 0)
            .then(|| Duration::from_millis(100));
        let wait_result = if !deferred_cqes.is_empty() || pass_polls > 0 || pass_msgs > 0 {
            // Work-conserving pass: completions in hand (parking would
            // sleep over work nothing re-signals — the drained-then-park
            // wedge), or this pass did real work whose follow-ons (fresh
            // deliveries from the commits it flushed, wakes from the
            // tasks it polled) are best served by spinning straight back
            // through the interleave. Flush non-blocking with GETEVENTS
            // (DEFER_TASKRUN: deliveries only materialize under the
            // flag) and fall through.
            let ts = types::Timespec::new();
            let args = types::SubmitArgs::new().timespec(&ts);
            match ring.submitter().submit_with_args(1, &args) {
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(0),
                other => other,
            }
        } else {
            let retry_left =
                next_retry.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            match (retry_left, park_backstop) {
                (None, None) => ring.submit_and_wait(1),
                (left, backstop) => {
                    // Bounded park: the earlier of the REGISTER-retry
                    // deadline and the pend/fused backstop tick.
                    let bound = match (left, backstop) {
                        (Some(l), Some(b)) => l.min(b),
                        (Some(l), None) => l,
                        (None, Some(b)) => b,
                        (None, None) => unreachable!(),
                    };
                    let ts = types::Timespec::new()
                        .sec(bound.as_secs())
                        .nsec(bound.subsec_nanos());
                    let args = types::SubmitArgs::new().timespec(&ts);
                    match ring.submitter().submit_with_args(1, &args) {
                        // A timed-out wait is the tick, not an error.
                        Err(e) if e.raw_os_error() == Some(libc::ETIME) => {
                            if backstop.is_some() {
                                TRANSPORT_PARK_BACKSTOP_TICKS.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(0)
                        }
                        other => other,
                    }
                }
            }
        };
        match wait_result {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) if FuseOverUring::is_disconnect_errno(e.raw_os_error().unwrap_or(0)) => {
                info!(
                    "fuse-over-uring qids={qids:?}: submit_and_wait disconnect ({e}); shutting down"
                );
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
            // FUSE-3f: read the overflow counter with the same sync that
            // publishes the tail — a dropped completion is invisible in the
            // CQEs by definition, so this is the only place it can be seen.
            let dropped = cq_drops.observe(cq.overflow());
            if dropped > 0 {
                error!(
                    "fuse-over-uring qids={qids:?}: kernel DROPPED {dropped} completion(s)                      (CQ overflow, nodrop={}) — the ents they belonged to are stalled;                      transport_cq_overflows",
                    cq_drops.nodrop()
                );
            }
            // Mid-pass-deferred CQEs first (they ARRIVED first), then the
            // post-wait drain.
            deferred_cqes
                .drain(..)
                .chain(cq.map(|c| (c.user_data(), c.result(), c.flags())))
                .collect()
        };

        // Reclaim list — group-local ent ids (member recovered by
        // `gent / depth` at the re-REGISTER pass).
        let mut resubmit: Vec<usize> = Vec::new();
        let mut disconnect = false;
        for (user_data, res, cqe_flags) in completed {
            // FUSE-2 row 6: the op class rides `user_data`, so an errored
            // COMMIT is never mistaken for an errored REGISTER (which is
            // how an EAGAIN'd commit used to discard the reply
            // `apply_reply` had already written into the ent).
            let Some((op, gent)) = decode_user_data(user_data) else {
                // wake_fd poll completed — re-arm (or exit if inactive)
                need_repoll_sticky = true;
                continue;
            };
            if gent >= members.len() * depth {
                warn!("fuse-over-uring qids={qids:?}: CQE for out-of-range ent {gent}");
                continue;
            }
            let (mi, ent_idx) = (gent / depth, gent % depth);
            let qid = members[mi].qid;
            // TEST SEAM (zc-bridge-cqe-wedge): consume-and-drop the first
            // N WRITE-class bridge CQEs — pend + deadline stay live, the
            // exact zcws-9 lost-completion posture, selected
            // deterministically instead of by load. Env-gated
            // (`SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES`); one relaxed load in
            // production.
            if op == RingOp::Fetch
                && matches!(
                    members[mi].zc_pend[ent_idx],
                    Some(ZcPend::HandlerStore { .. })
                        | Some(ZcPend::LazyExtract { .. })
                        | Some(ZcPend::WriteExtract { .. })
                )
                && test_drop_write_cqe()
            {
                error!(
                    "fuse-over-uring qid={qid} ent={ent_idx}: TEST SEAM dropping \
                     WRITE-class bridge CQE (res={res}) — pend stays live"
                );
                continue;
            }
            if res < 0 {
                let err = -res;
                let m = &mut members[mi];
                pool.stats_cqe_err.fetch_add(1, Ordering::Relaxed);
                STATS_CQE_ERR.fetch_add(1, Ordering::Relaxed);
                xport_dbg!("[XPORT] cqe-err qid={qid} ent={ent_idx} op={op:?} err={err}");
                // zc bridge errors resolve against the ent's pending kind
                // and NEVER trip the protocol-fatal ladder below — an
                // EINVAL here is a misaligned O_DIRECT fetch or an
                // unregistered slot (the opcode-mirror miss), both of
                // which fall back per-request.
                if op == RingOp::Cancel {
                    // The lost-CQE resolution ladder (zc-bridge-cqe-wedge,
                    // 2026-08-07): the cancel's own CQE classifies the
                    // overdue op — see `zc::cancel_cqe_action` for the
                    // FIFO argument that makes -ENOENT-with-a-live-pend
                    // the PROVEN lost-completion shape.
                    match zc::cancel_cqe_action(res, m.zc_pend[ent_idx].is_some()) {
                        zc::CancelCqeAction::Nothing => {}
                        zc::CancelCqeAction::AwaitOriginal => {
                            debug!(qid, ent_idx, res, "bridge AsyncCancel: op canceled");
                        }
                        zc::CancelCqeAction::SynthesizeLost => {
                            kmbuf::note_zc_bridge_lost();
                            error!(
                                "fuse-over-uring qid={qid} ent={ent_idx}: bridge \
                                 AsyncCancel answered ENOENT with the pend still \
                                 live — the op's completion was POSTED and never \
                                 reaped (a LOST ring completion, the zcws-9 class); \
                                 synthesizing its resolution \
                                 (fuse3_zc_bridge_lost)"
                            );
                            note_handler_bridge_passbottom(
                                &m.zc_pend[ent_idx],
                                &m.bridge_deadlines,
                                ent_idx,
                            );
                            let pend = zc_fetch_complete(
                                &mut ring,
                                &mut batch,
                                &mut m.slots,
                                &mut m.ents[ent_idx],
                                &mut m.zc_pend[ent_idx],
                                pool.slot_watch_cell(qid, ent_idx),
                                qid,
                                gent,
                                -libc::ETIMEDOUT,
                            )?;
                            if m.bridge_deadlines.clear(ent_idx) {
                                pool.zc_bridge_pends.fetch_sub(1, Ordering::Relaxed);
                            }
                            if matches!(pend, Some(PendDone::DeliverFailed)) {
                                fail_ent(
                                    &mut ring,
                                    &mut batch,
                                    &mut m.slots,
                                    &mut m.ents[ent_idx],
                                    &m.lease_states[ent_idx],
                                    pool.slot_watch_cell(qid, ent_idx),
                                    qid,
                                    gent,
                                    libc::EIO,
                                )?;
                            }
                        }
                        zc::CancelCqeAction::Restamp => {
                            warn!(
                                "fuse-over-uring qid={qid} ent={ent_idx}: bridge \
                                 AsyncCancel answered {err} — the op is still \
                                 RUNNING kernel-side and cannot be synthesized \
                                 (a late DMA would alias a recycled slot); \
                                 re-arming the deadline (the slot stays \
                                 watchdog-named until the op completes)"
                            );
                            if m.bridge_deadlines
                                .stamp(ent_idx, crate::raw::read_phase::transport_now_ns())
                            {
                                // Structurally a re-stamp (the pend is
                                // live), but keep the gauge exact if the
                                // ledger ever disagrees.
                                pool.zc_bridge_pends.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    continue;
                }
                if op == RingOp::Fetch {
                    // An errored bridge CQE: LazyExtract answers its
                    // handler's oneshot with the error (the DISPATCHED
                    // request's handler owns the EIO reply — a synthesis
                    // here would double-reply the slot); a failed
                    // AT-DELIVERY extraction is a request nothing
                    // dispatched, so its EIO synthesizes here.
                    note_handler_bridge_passbottom(
                        &m.zc_pend[ent_idx],
                        &m.bridge_deadlines,
                        ent_idx,
                    );
                    let pend = zc_fetch_complete(
                        &mut ring,
                        &mut batch,
                        &mut m.slots,
                        &mut m.ents[ent_idx],
                        &mut m.zc_pend[ent_idx],
                        pool.slot_watch_cell(qid, ent_idx),
                        qid,
                        gent,
                        res,
                    )?;
                    if m.bridge_deadlines.clear(ent_idx) {
                        pool.zc_bridge_pends.fetch_sub(1, Ordering::Relaxed);
                    }
                    if matches!(pend, Some(PendDone::DeliverFailed)) {
                        fail_ent(
                            &mut ring,
                            &mut batch,
                            &mut m.slots,
                            &mut m.ents[ent_idx],
                            &m.lease_states[ent_idx],
                            pool.slot_watch_cell(qid, ent_idx),
                            qid,
                            gent,
                            libc::EIO,
                        )?;
                    }
                    continue;
                }
                // Kernel abort/unmount (dev_uring.c): -ENOTCONN on entry teardown /
                // cancel; -ECONNABORTED when abort_with_err is set.
                if FuseOverUring::is_disconnect_errno(err) {
                    info!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: disconnect CQE err={err}; shutting down"
                    );
                    disconnect = true;
                    break;
                }
                // A failed RELEASE_PAYLOAD is accounting + tripwire, never
                // protocol-fatal (ENOENT = double release, EBUSY = raced a
                // live commit — both unrepresentable under the deferral
                // protocol, hence must-stay-0).
                if op == RingOp::Release {
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: RELEASE_PAYLOAD \
                         cqe err={err} (fuse3_zc_release_failures)"
                    );
                    kmbuf::note_zc_release_failure();
                    continue;
                }
                // Kernel-refused RETAIN (design-zc-write-kernel-v2 §3.3's
                // -EINVAL arm): the ent still holds its applied reply —
                // recover by plain re-commit, count the must-stay-0
                // tripwire, and DON'T let the refusal reach the
                // protocol-fatal ladder below.
                if op == RingOp::Commit && err == libc::EINVAL {
                    if let SlotState::Retained { commit_id } = m.slots.state(ent_idx) {
                        warn!(
                            "fuse-over-uring qid={qid} ent={ent_idx}: RETAIN commit \
                             refused EINVAL — re-committing plain \
                             (fuse3_zc_retain_refused)"
                        );
                        kmbuf::note_zc_retain_refused();
                        m.slots.on_retain_refused(ent_idx);
                        submit_commit(
                            &mut ring,
                            &mut batch,
                            &mut m.slots,
                            pool.slot_watch_cell(qid, ent_idx),
                            qid,
                            gent,
                            commit_id,
                        )?;
                        continue;
                    }
                }
                if err == libc::ENOTSUP || err == libc::EINVAL || err == libc::ENOSYS {
                    error!("fuse-over-uring: kernel rejected protocol err={err}");
                    pool.shutdown();
                    return Err(io::Error::from_raw_os_error(err));
                }
                match op {
                    // Structurally unreachable: Fetch/Cancel errors
                    // resolved (and `continue`d) before the
                    // protocol-fatal ladder above.
                    // Release errors resolved (and `continue`d) before
                    // the protocol-fatal ladder above, like Fetch/Cancel.
                    RingOp::Fetch | RingOp::Cancel | RingOp::Release => {}
                    // Row 6: a transient COMMIT failure re-commits. The
                    // ent still holds the applied reply, so re-pushing
                    // the same COMMIT_AND_FETCH is the whole recovery —
                    // re-REGISTERing here would discard that reply and
                    // leave the caller waiting forever.
                    RingOp::Commit if err == libc::EAGAIN || err == libc::EINTR => {
                        // A transient failure of a RETAIN commit re-commits
                        // WITH retain (the kernel never parked the ent, so
                        // the retained arc restarts whole — the outstanding
                        // count is retracted and re-made at re-submission).
                        let (commit_id, retain) = match m.slots.state(ent_idx) {
                            SlotState::Replied { commit_id } => (Some(commit_id), false),
                            SlotState::Retained { commit_id } => (Some(commit_id), true),
                            _ => (None, false),
                        };
                        match commit_id {
                            Some(cid) => {
                                warn!(
                                    "fuse-over-uring qid={qid} ent={ent_idx}: COMMIT err={err}; \
                                     re-committing cid={cid} retain={retain} \
                                     (reply already applied)"
                                );
                                if retain {
                                    kmbuf::retract_zc_retain_commit();
                                    m.slots.on_retain_refused(ent_idx);
                                } else {
                                    m.slots.on_commit_retry(ent_idx);
                                }
                                submit_commit_retain(
                                    &mut ring,
                                    &mut batch,
                                    &mut m.slots,
                                    pool.slot_watch_cell(qid, ent_idx),
                                    qid,
                                    gent,
                                    cid,
                                    retain,
                                )?;
                            }
                            None => {
                                warn!(
                                    "fuse-over-uring qid={qid} ent={ent_idx}: COMMIT err={err} \
                                     with no reply in flight ({:?}); re-REGISTER",
                                    m.slots.state(ent_idx)
                                );
                                resubmit.push(gent);
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
                        resubmit.push(gent);
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
                            &mut m.slots,
                            &mut m.ents[ent_idx],
                            &m.lease_states[ent_idx],
                            pool.slot_watch_cell(qid, ent_idx),
                            qid,
                            gent,
                            libc::EIO,
                        )? {
                            // A synthesized commit is now in flight for
                            // this ent; the kernel re-arms it.
                            continue;
                        }
                        match m.slots.note_register_failure(ent_idx, Instant::now()) {
                            RegisterAction::Retry { after } => {
                                warn!(
                                    "fuse-over-uring qid={qid} ent={ent_idx}: REGISTER err={err}; \
                                     retry in {after:?}"
                                );
                                if after.is_zero() {
                                    resubmit.push(gent);
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
                                // Per MEMBER, not per group: the kernel
                                // routes by task_cpu, so one fully-retired
                                // member queue strands its CPU's requests
                                // even while siblings stay healthy — the
                                // session must fail exactly as the
                                // per-queue worker did.
                                if m.slots.all_retired() {
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
            let m = &mut members[mi];
            if op == RingOp::Cancel {
                // Informational (see the error-path twin): resolution
                // always rides the original op's CQE.
                debug!(qid, ent_idx, res, "bridge AsyncCancel CQE");
                continue;
            }
            if op == RingOp::Release {
                // Accounting-only (counted at submission); the re-armed
                // parked commit's CQE is the next delivery.
                debug!(qid, ent_idx, res, "RELEASE_PAYLOAD CQE");
                continue;
            }
            if op == RingOp::Fetch {
                // A zc bridge completed (device fetch/store, bounce
                // bridge, or lazy WRITE extraction). NOT a delivery —
                // resolve the pending kind and move on.
                note_handler_bridge_passbottom(&m.zc_pend[ent_idx], &m.bridge_deadlines, ent_idx);
                let pend = zc_fetch_complete(
                    &mut ring,
                    &mut batch,
                    &mut m.slots,
                    &mut m.ents[ent_idx],
                    &mut m.zc_pend[ent_idx],
                    pool.slot_watch_cell(qid, ent_idx),
                    qid,
                    gent,
                    res,
                )?;
                if m.bridge_deadlines.clear(ent_idx) {
                    pool.zc_bridge_pends.fetch_sub(1, Ordering::Relaxed);
                }
                if let Some(p) = pend {
                    // An extracted WRITE payload sits in the bounce
                    // slot: mint the §5.4 lease over it. The lease arena
                    // IS the bounce arena in zc mode, so the drop
                    // protocol (refs → unpark → wake) is byte-identical
                    // to the kmbuf path.
                    let mint_lease = |m: &mut MemberState, len: u32| {
                        let zb = zc_bounce.as_ref().expect("zc pend implies zc mode");
                        let ptr = zb.buf_ptr(ent_idx).expect("pend slot in range") as *const u8;
                        let state = Arc::clone(&m.lease_states[ent_idx]);
                        let prev = state.acquire();
                        debug_assert_eq!(prev, 0, "extraction on a still-leased ent");
                        TRANSPORT_PAYLOAD_LEASES.fetch_add(1, Ordering::Relaxed);
                        TRANSPORT_LEASES_OUTSTANDING.fetch_add(1, Ordering::Relaxed);
                        Bytes::from_owner(EntPayloadLease {
                            arena: Arc::clone(&m.arena),
                            state,
                            ptr,
                            len: len as usize,
                            born: Instant::now(),
                        })
                    };
                    match p {
                        PendDone::Lazy { done, len } => {
                            // The held state clears — one materialization
                            // per request (the handler memoizes). A
                            // dropped receiver = the handler gave up
                            // (teardown); the lease drops with the unsent
                            // Bytes and the commit gate unparks — nothing
                            // leaks.
                            pool.zc_write_held.clear(qid, ent_idx);
                            let payload = mint_lease(m, len);
                            let _ = done.send(Ok(payload));
                        }
                        PendDone::Deliver {
                            header_and_op,
                            unique,
                            commit_id,
                            len,
                        } => {
                            // fused-lane-predicate (2026-08-08): an
                            // at-delivery extraction is by definition a
                            // shape NO direct vehicle consumes (the hold
                            // gate said no) — it dispatches on the
                            // classic handler lanes, NEVER the fused
                            // lane: its handler path parks on fabric-RTT
                            // FS state (allocation, growth publishes)
                            // and the multi-lane venue owns that
                            // concurrency. The 2026-08-07 fusion arm
                            // that lived here is what double-paid the
                            // field's ineligible ops.
                            //
                            // The at-delivery extraction's deferred
                            // dispatch (the streaming arm).
                            let payload = mint_lease(m, len);
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
                                    "fuse-over-uring qid={qid} ent={ent_idx}: session inbound \
                                     queue closed after zc extraction; synthesizing EIO for \
                                     unique={unique} (row 4)",
                                    unique = unique
                                );
                                fail_ent(
                                    &mut ring,
                                    &mut batch,
                                    &mut m.slots,
                                    &mut m.ents[ent_idx],
                                    &m.lease_states[ent_idx],
                                    pool.slot_watch_cell(qid, ent_idx),
                                    qid,
                                    gent,
                                    libc::EIO,
                                )?;
                            }
                        }
                        PendDone::DeliverFailed => {
                            // Short success CQE (res != len): the request
                            // cannot be served — synthesize.
                            fail_ent(
                                &mut ring,
                                &mut batch,
                                &mut m.slots,
                                &mut m.ents[ent_idx],
                                &m.lease_states[ent_idx],
                                pool.slot_watch_cell(qid, ent_idx),
                                qid,
                                gent,
                                libc::EIO,
                            )?;
                        }
                    }
                }
                continue;
            }
            if op == RingOp::Register {
                m.slots.note_register_success(ent_idx);
            }
            // kmbuf attachment law (2026-08-04): a flagged CQE re-points
            // the ent's payload buffer to the freshly-selected kernel
            // buffer; an unflagged one keeps the current attachment (the
            // kernel's reuse case). NUMA locality is re-derived per
            // attachment (bid-indexed buffers). kmbuf groups are
            // singletons, so gent == ent_idx on this arm.
            if let Some(kq) = &kmbuf_q {
                match kq.note_delivery(ent_idx, cqe_flags) {
                    Some((p, len)) => {
                        if m.ents[ent_idx].payload_ptr != p {
                            m.ents[ent_idx].payload_ptr = p;
                            m.ents[ent_idx].payload_len = len;
                            m.ents[ent_idx].node = m.arena.node_of_ptr(p);
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
            let unique =
                u64::from_le_bytes(m.ents[ent_idx].hdr().in_out[8..16].try_into().unwrap());
            let mut commit_id = m.ents[ent_idx].hdr().ring_ent_in_out.commit_id;
            if commit_id == 0 {
                // Fall back to unique — some paths only fill in_out.
                commit_id = unique;
            }
            if unique == 0 {
                // Prefer COMMIT with commit_id if the kernel filled it — re-REGISTER
                // alone leaves USERSPACE entries and permanent waiting/EBUSY umount.
                let cid = m.ents[ent_idx].hdr().ring_ent_in_out.commit_id;
                if cid != 0 {
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: unique=0 commit_id={cid}; force EIO COMMIT"
                    );
                    xport_dbg!("[XPORT] unique0-force-commit qid={qid} ent={ent_idx} cid={cid}");
                    // Delivery on this ent implies its previous commit passed
                    // the refs == 0 gate; header-only reply, payload untouched.
                    debug_assert!(!m.lease_states[ent_idx].leased());
                    m.slots.on_deliver_degenerate(ent_idx, cid);
                    apply_reply(
                        &mut m.ents[ent_idx],
                        &error_out_header(0, libc::EIO),
                        &Bytes::new(),
                    );
                    submit_commit(
                        &mut ring,
                        &mut batch,
                        &mut m.slots,
                        pool.slot_watch_cell(qid, ent_idx),
                        qid,
                        gent,
                        cid,
                    )?;
                } else {
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: unique=0 commit_id=0; re-REGISTER"
                    );
                    xport_dbg!("[XPORT] unique0-re-register qid={qid} ent={ent_idx}");
                    resubmit.push(gent);
                }
                continue;
            }
            let opcode = u32::from_le_bytes(m.ents[ent_idx].hdr().in_out[4..8].try_into().unwrap());
            let payload_sz = m.ents[ent_idx].hdr().ring_ent_in_out.payload_sz as usize;
            m.ents[ent_idx].last_opcode = opcode;
            // zc in-paged deliveries (K1 kill): the announced payload
            // lives in the SPARSE SLOT (the kernel registered the
            // caller's source pages instead of copying them) — it must be
            // extracted through the ring before dispatch.
            let zc_slot_payload =
                zc_track.is_some_and(|track| zc::in_paged(opcode, track)) && payload_sz > 0;
            // The FUSE_IOCTL-class shape: payload announced on a zc queue
            // for an opcode the in-direction mirror does not extract —
            // deliver EMPTY, loudly counted (this daemon serves no data
            // ioctls; a growing counter names a mirror gap, never silent
            // corruption).
            let zc_payload_skipped =
                zc_mode && !zc_slot_payload && payload_sz > 0 && !m.ents[ent_idx].has_payload_buf();
            if zc_payload_skipped {
                warn!(
                    "fuse-over-uring qid={qid} ent={ent_idx}: zc delivery announced \
                     {payload_sz} payload bytes for opcode {opcode} with no kmbuf \
                     attachment and no extraction arm (kmbuf_ops={}) — the \
                     in-direction mirror must learn this opcode for this track; \
                     delivering empty (fuse3_zc_slot_payload_skips)",
                    kmbuf::resolved_opcodes_label()
                );
                kmbuf::note_zc_slot_payload_skip();
            }
            if payload_sz > 0
                && !zc_slot_payload
                && !zc_payload_skipped
                && !m.ents[ent_idx].has_payload_buf()
            {
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
            header_and_op.extend_from_slice(&m.ents[ent_idx].hdr().in_out[..FUSE_IN_HEADER_SIZE]);
            header_and_op.extend_from_slice(&m.ents[ent_idx].hdr().op_in);
            let capped_sz = if zc_slot_payload || zc_payload_skipped {
                0
            } else {
                payload_sz.min(m.ents[ent_idx].payload_len)
            };
            // §5.4: FUSE_WRITE payloads ride a zero-copy lease over the
            // registered buffer (kills the 1 MiB copy + alloc per write
            // request, audit #1); the commit gate above defers the ent's
            // re-arm until the lease drops. FORGET/BATCH_FORGET are
            // auto-committed below *before* the session consumes the payload
            // — leasing them would hand the session a buffer the kernel is
            // already refilling — and non-write opcodes carry small payloads
            // (names, xattrs): both keep the copy. zc WRITE deliveries take
            // `capped_sz == 0` here (placeholder payload); their lease is
            // minted over the BOUNCE at extraction completion.
            let payload = if opcode == FUSE_WRITE_OPCODE && capped_sz > 0 {
                let state = Arc::clone(&m.lease_states[ent_idx]);
                let prev = state.acquire();
                debug_assert_eq!(prev, 0, "delivery on a still-leased ent");
                TRANSPORT_PAYLOAD_LEASES.fetch_add(1, Ordering::Relaxed);
                TRANSPORT_LEASES_OUTSTANDING.fetch_add(1, Ordering::Relaxed);
                // K1 crossing estimate: the kernel copied this payload
                // from the app on ≈ CPU qid (queue selection is by
                // requester CPU) into the buffer's actual node
                // (ent-static classically; per-attachment on kmbuf —
                // `ents[..].node` tracks both).
                numa_classify_pass(m.node, m.ents[ent_idx].node, capped_sz);
                Bytes::from_owner(EntPayloadLease {
                    arena: Arc::clone(&m.arena),
                    state,
                    ptr: m.ents[ent_idx].payload_ptr as *const u8,
                    len: capped_sz,
                    born: Instant::now(),
                })
            } else {
                Bytes::copy_from_slice(&m.ents[ent_idx].payload()[..capped_sz])
            };

            // ACK-early belt: every request starts with a clean retain
            // flag — a handler that armed RETAIN and then errored (its
            // reply synthesized by fail_ent, which never mints a
            // CommitMsg) must not leak the flag onto THIS request's
            // reply.
            if let Some(cell) = pool.retain_next.get(qid as usize * depth + ent_idx) {
                cell.store(false, Ordering::Relaxed);
            }
            // FUSE-2: the slot now owes exactly one commit. A delivery
            // onto a slot that STILL owes one (row 10's overwrite class,
            // and row 11's `fuse_resend` double-delivery shape) is
            // reported loudly and counted on the must-stay-0 tripwire —
            // the displaced request's commit id is gone with it, so it
            // cannot be answered, and pretending otherwise (what
            // `pending.insert` did) hides a wedged caller.
            if let DeliverOutcome::DisplacedRequest { unique: lost } =
                m.slots.on_deliver(ent_idx, unique, commit_id)
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
                debug_assert!(!m.lease_states[ent_idx].leased());
                apply_reply(
                    &mut m.ents[ent_idx],
                    &error_out_header(unique, 0),
                    &Bytes::new(),
                );
                submit_commit(
                    &mut ring,
                    &mut batch,
                    &mut m.slots,
                    pool.slot_watch_cell(qid, ent_idx),
                    qid,
                    gent,
                    commit_id,
                )?;
                pool.stats_replies.fetch_add(1, Ordering::Relaxed);
                STATS_REPLIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // D14 write-side HYBRID delivery (the zcws-8 lesson): a zc
            // WRITE's payload stays HELD in the sparse slot ONLY when
            // the shape is a direct-DMA candidate (`zc::hold_candidate`
            // — aligned, sub-streaming) — the request then dispatches
            // IMMEDIATELY with an empty placeholder and the held length
            // published on the pool table (the handler DMAs it
            // slot→device or materializes lazily). Every OTHER shape —
            // the streaming population — extracts AT DELIVERY on this
            // worker's own drain pass (`WRITE_FIXED(slot → memfd)`,
            // batched SQEs, no per-request task wake: the all-lazy
            // design collapsed the durable streaming row to 0.61×).
            // The oversize shape is refused loud as always.
            if zc_slot_payload {
                let zb = zc_bounce.as_ref().expect("zc_slot_payload implies zc mode");
                if payload_sz > zb.stride() {
                    error!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: zc WRITE payload \
                         {payload_sz} B exceeds the slot stride ({} B) — EIO",
                        zb.stride()
                    );
                    fail_ent(
                        &mut ring,
                        &mut batch,
                        &mut m.slots,
                        &mut m.ents[ent_idx],
                        &m.lease_states[ent_idx],
                        pool.slot_watch_cell(qid, ent_idx),
                        qid,
                        gent,
                        libc::EIO,
                    )?;
                    continue;
                }
                // fuse_write_in rides op_in: fh u64 ‖ offset u64 ‖
                // size u32 ‖ … (little-endian, the ABI shape
                // handle_write deserializes).
                let op_in = &m.ents[ent_idx].hdr().op_in;
                let w_off = u64::from_le_bytes(op_in[8..16].try_into().unwrap());
                let w_size = u32::from_le_bytes(op_in[16..20].try_into().unwrap());
                // fused-lane-predicate (2026-08-08, the field-falsification
                // fix): HOLD only when a slot→device vehicle will actually
                // CONSUME the slot — the shape predicate (`hold_candidate`:
                // aligned, sub-streaming) composed with the FILESYSTEM's
                // W1-eligibility probe (`zc_write_hold_eligible` through
                // the registered gate; layouts live in the root crate, so
                // the transport cannot decide alone). No gate ⇒ never
                // hold. A W1-INELIGIBLE shape held anyway paid hold +
                // fused poll + LATE extraction serialized at fabric RTT —
                // the 0.45× field collapse whose signature is fusions ≈
                // ops ∧ extractions ≈ ops (both vehicles per op).
                let hold_nodeid =
                    u64::from_le_bytes(m.ents[ent_idx].hdr().in_out[16..24].try_into().unwrap());
                // fuse_write_in: write_flags @ 20, flags (open) @ 32.
                // O_DIRECT GUP pages + not WRITE_CACHE ⇒ extract at
                // delivery when the FS overlay arm would otherwise HOLD
                // (handler materialize is the 23 GiB/s late-extract tax).
                let w_flags = u32::from_le_bytes(op_in[20..24].try_into().unwrap());
                let open_flags = if op_in.len() >= 36 {
                    u32::from_le_bytes(op_in[32..36].try_into().unwrap())
                } else {
                    0
                };
                let odirect =
                    (open_flags as i32) & libc::O_DIRECT != 0 && w_flags & FUSE_WRITE_CACHE == 0;
                if zc::hold_candidate(w_off, w_size, payload_sz_cfg)
                    && pool
                        .zc_hold_gate
                        .get()
                        .is_some_and(|g| g(hold_nodeid, w_off, w_size, odirect))
                {
                    pool.zc_write_held.set(qid, ent_idx, payload_sz as u32);
                    // zc-write-fusion: a held write at or under the
                    // fusion ceiling runs its handler on THIS worker's
                    // fused lane — the dispatch spawn and both bridge
                    // round-trip wakes collapse to same-thread channel
                    // ops. Above the ceiling (or lever off) the classic
                    // handler-lane dispatch below is unchanged; an
                    // ELIGIBLE delivery that cannot fuse (no dispatcher
                    // yet / lane at capacity) demotes loudly-counted.
                    if fused::fuse_candidate(payload_sz as u32, fusion_max, fusion_on) {
                        let can = pool
                            .fused_dispatch
                            .get()
                            .filter(|_| fused_lane.len() < group_depth);
                        match can {
                            Some(d) => {
                                let fut = (d.mint)(InboundUringReq {
                                    header_and_op,
                                    payload,
                                    unique,
                                    slot: ReplySlot::Ring {
                                        qid,
                                        ent_idx: ent_idx as u16,
                                        commit_id,
                                    },
                                    arrived_ns: crate::raw::read_phase::transport_now_ns(),
                                });
                                let admitted = fused_lane.spawn(fut, unique);
                                debug_assert!(admitted, "capacity checked above");
                                fused::note_zc_write_fusion(u64::from(payload_sz as u32));
                                continue;
                            }
                            None => fused::note_zc_write_fusion_demotion(),
                        }
                    }
                } else {
                    // The streaming arm: at-delivery extraction, exactly
                    // the zcws-6-era vehicle (kernel shmem copy replaces
                    // the delivery-time folio copy; the inbound push
                    // happens at the CQE with a §5.4 lease over the
                    // bounce).
                    pool.zc_write_held.clear(qid, ent_idx);
                    if m.zc_pend[ent_idx].is_some() {
                        error!(
                            "fuse-over-uring qid={qid} ent={ent_idx}: zc WRITE extraction \
                             refused (pending bridge) — EIO"
                        );
                        fail_ent(
                            &mut ring,
                            &mut batch,
                            &mut m.slots,
                            &mut m.ents[ent_idx],
                            &m.lease_states[ent_idx],
                            pool.slot_watch_cell(qid, ent_idx),
                            qid,
                            gent,
                            libc::EIO,
                        )?;
                        continue;
                    }
                    let entry = Entry128::from(
                        opcode::WriteFixed::new(
                            types::Fd(zb.fd()),
                            std::ptr::null(),
                            payload_sz as u32,
                            ent_idx as u16,
                        )
                        .offset(zb.offset_of(ent_idx).expect("ent in range"))
                        .build()
                        .user_data(encode_user_data(RingOp::Fetch, gent)),
                    );
                    match push_fetch_batched(&mut ring, &mut batch, entry) {
                        Ok(()) => {
                            m.zc_pend[ent_idx] = Some(ZcPend::WriteExtract {
                                header_and_op,
                                unique,
                                commit_id,
                                len: payload_sz as u32,
                            });
                            if m.bridge_deadlines
                                .stamp(ent_idx, crate::raw::read_phase::transport_now_ns())
                            {
                                pool.zc_bridge_pends.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => {
                            error!(
                                "fuse-over-uring qid={qid} ent={ent_idx}: zc WRITE extraction \
                                 push failed ({e}); synthesizing EIO for unique={unique}"
                            );
                            fail_ent(
                                &mut ring,
                                &mut batch,
                                &mut m.slots,
                                &mut m.ents[ent_idx],
                                &m.lease_states[ent_idx],
                                pool.slot_watch_cell(qid, ent_idx),
                                qid,
                                gent,
                                libc::EIO,
                            )?;
                        }
                    }
                    continue;
                }
            } else if zc_mode {
                // Any non-held delivery re-points the ent: stale held
                // state from a prior request must never be readable by a
                // later one (queries only race their own request's
                // window by protocol; this is the belt).
                pool.zc_write_held.clear(qid, ent_idx);
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
                    &mut m.slots,
                    &mut m.ents[ent_idx],
                    &m.lease_states[ent_idx],
                    pool.slot_watch_cell(qid, ent_idx),
                    qid,
                    gent,
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
        // FUSE-2 row 7: losing the wake-fd poll re-arm stops the group
        // waking on `commit_tx` ENTIRELY — every reply on its member
        // queues is then stranded until an unrelated CQE happens by. The
        // flag is STICKY: a failed push is retried on the next pass
        // instead of being swallowed by `let _ =`.
        if need_repoll_sticky {
            match push_poll_batched(&mut ring, &mut batch) {
                Ok(()) => need_repoll_sticky = false,
                Err(e) => warn!(
                    "fuse-over-uring qids={qids:?}: wake-poll re-arm push failed ({e}); \
                     retrying next pass"
                ),
            }
        }
        // Never re-REGISTER after a disconnect; only while still active.
        if !resubmit.is_empty() && pool.active.load(Ordering::Relaxed) {
            for gent in resubmit {
                let (mi, ent_idx) = (gent / depth, gent % depth);
                let m = &mut members[mi];
                let qid = m.qid;
                let iov = reg_iov(&m.ents[ent_idx]);
                // Re-REGISTER is a non-reply exit: an ent that still owes
                // a reply must be failed first (row 5) — `fail_ent` is a
                // no-op for slots that owe nothing.
                fail_ent(
                    &mut ring,
                    &mut batch,
                    &mut m.slots,
                    &mut m.ents[ent_idx],
                    &m.lease_states[ent_idx],
                    pool.slot_watch_cell(qid, ent_idx),
                    qid,
                    gent,
                    libc::EIO,
                )?;
                if let Err(e) = push_cmd_batched(
                    &mut ring,
                    &mut batch,
                    FUSE_IO_URING_CMD_REGISTER,
                    qid,
                    0,
                    iov,
                    encode_user_data(RingOp::Register, gent),
                    reg_init_flags,
                    reg_queue_depth,
                    reg_buf_index(ent_idx),
                ) {
                    warn!("fuse-over-uring qid={qid} ent={ent_idx}: re-REGISTER push failed ({e})");
                    continue;
                }
                m.slots.on_register_submitted(ent_idx);
            }
        }
        // FUSE-3a: serve any ent whose REGISTER backoff has expired. An
        // idle group gets no CQEs, so the loop wait above is bounded
        // while a backoff is outstanding (see `wait_budget`).
        let now = Instant::now();
        for (mi, m) in members.iter_mut().enumerate() {
            let qid = m.qid;
            for ent_idx in m.slots.register_retries_due(now) {
                let iov = reg_iov(&m.ents[ent_idx]);
                if push_cmd_batched(
                    &mut ring,
                    &mut batch,
                    FUSE_IO_URING_CMD_REGISTER,
                    qid,
                    0,
                    iov,
                    encode_user_data(RingOp::Register, gent_of(mi, ent_idx)),
                    reg_init_flags,
                    reg_queue_depth,
                    reg_buf_index(ent_idx),
                )
                .is_ok()
                {
                    m.slots.on_register_submitted(ent_idx);
                }
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
    // `was_parked` carries the ledger provenance into the drain: only a
    // message that PARKED can close its park here (a message pulled off
    // `commit_rx` was never parked, and the header-only fallback below is
    // the unresolved case by construction).
    //
    // zc-write-fusion teardown: drop parked fused futures FIRST — their
    // payload leases release (unparking any lease-gated commit into the
    // drain below) and their ReplyTx drop-guards' replies are refused by
    // the now-inactive pool (NotFound — benign, counted apart), so the
    // row-8 owing() pass below synthesizes each fused slot's EIO exactly
    // once. Never let a fused future outlive its worker's arenas.
    if fused_lane.len() > 0 {
        debug!(
            "fuse-over-uring qids={qids:?}: dropping {} parked fused write task(s) at teardown",
            fused_lane.len()
        );
    }
    drop(fused_lane);
    let mut final_msgs: Vec<(CommitMsg, bool)> = members
        .iter_mut()
        .flat_map(|m| m.parked_msgs.iter_mut())
        .filter_map(|s| s.take().map(|m| (m, true)))
        .collect();
    // zc teardown: parked bounce bridges rejoin the drain as HEADER-ONLY
    // EIO commits — their bodies never reached the request's pages (the
    // bridge CQE is not coming), and committing the body through the
    // attachment would let a still-live kernel skip the folio copy and
    // report success over unwritten pages. Handler fetches/stores and
    // lazy extractions unblock by SENDER DROP (the oneshot receiver
    // reads BrokenPipe and the handler's serve/write fails loud — the
    // dispatched request's own reply path owns the slot); deferred
    // at-delivery WRITE extractions are still OWED slots, which the
    // row-8 owing() pass below synthesizes.
    for m in members.iter_mut() {
        for (idx, pend) in m.zc_pend.iter_mut().enumerate() {
            if let Some(ZcPend::BounceFetch {
                header, commit_id, ..
            }) = pend.take()
            {
                let unique = if header.len() >= 16 {
                    u64::from_le_bytes(header[8..16].try_into().unwrap())
                } else {
                    0
                };
                final_msgs.push((
                    CommitMsg {
                        qid: m.qid,
                        ent_idx: idx as u16,
                        commit_id,
                        header: error_out_header(unique, libc::EIO).to_vec(),
                        reply_body: Bytes::new(),
                        prefilled: None,
                        retain: false,
                    },
                    true,
                ));
            }
        }
    }
    while let Ok(wmsg) = commit_rx.try_recv() {
        match wmsg {
            WorkerMsg::Commit(msg) => final_msgs.push((msg, false)),
            // Dropping the sender unblocks the parked handler (BrokenPipe).
            // A teardown-raced ZcRelease is moot: the kernel drains every
            // retained ent (-ECONNABORTED on the parked cmd) at abort.
            WorkerMsg::ZcFetch(_)
            | WorkerMsg::ZcStore(_)
            | WorkerMsg::ZcExtract(_)
            | WorkerMsg::ZcRelease { .. } => {}
        }
    }
    // Bridge-deadline gauge hygiene: every pend died with this worker —
    // return its share so the watch thread stops waking survivors for
    // ledgers that no longer exist.
    for m in members.iter_mut() {
        let n = m.bridge_deadlines.outstanding() as u64;
        if n > 0 {
            pool.zc_bridge_pends.fetch_sub(n, Ordering::Relaxed);
        }
    }
    xport_dbg!(
        "[XPORT] worker-exit qids={qids:?} final_msgs={} active={}",
        final_msgs.len(),
        pool.active.load(Ordering::Relaxed)
    );
    let mut final_commits = 0;
    // FUSE-3j: ONE bounded, event-driven wait for the whole GROUP. Publish
    // `parked` for every message whose ent is still leased (that is what
    // makes the lease drop fire this group's eventfd — a message pulled
    // straight off `commit_rx` was never parked), then wait for all of them
    // together instead of sleep-polling each in series. The wait indexes a
    // flattened gent-ordered view of every member's lease words.
    let flat_leases: Vec<Arc<EntLeaseState>> = members
        .iter()
        .flat_map(|m| m.lease_states.iter().cloned())
        .collect();
    let mut final_msgs: Vec<(CommitMsg, bool)> = final_msgs;
    let mut waiting: Vec<usize> = Vec::new();
    for (msg, _) in final_msgs.iter() {
        let idx = msg.ent_idx as usize;
        let Some(mi) = member_of_qid(msg.qid).filter(|_| idx < depth) else {
            continue;
        };
        if members[mi].lease_states[idx].try_commit() == CommitGate::Parked {
            waiting.push(gent_of(mi, idx));
        }
    }
    if !waiting.is_empty() {
        drain_await_leases(
            wake_fd,
            &wake_coalescer,
            &flat_leases,
            &mut waiting,
            DRAIN_LEASE_BUDGET,
        );
    }
    for (msg, was_parked) in final_msgs.drain(..) {
        let idx = msg.ent_idx as usize;
        let Some(mi) = member_of_qid(msg.qid).filter(|_| idx < depth) else {
            continue;
        };
        let m = &mut members[mi];
        let qid = m.qid;
        let gent = gent_of(mi, idx);
        // Still in `waiting` ⇒ the budget expired with the lease live.
        let free = !waiting.contains(&gent);
        if free {
            if was_parked {
                TRANSPORT_UNPARKED_COMMITS.fetch_add(1, Ordering::Relaxed);
            }
            apply_reply(&mut m.ents[idx], &msg.header, &msg.reply_body);
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
                &mut m.ents[idx],
                &error_out_header(unique, libc::EIO),
                &Bytes::new(),
            );
        }
        if submit_commit(
            &mut ring,
            &mut batch,
            &mut m.slots,
            pool.slot_watch_cell(qid, idx),
            qid,
            gent,
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
    for (mi, m) in members.iter_mut().enumerate() {
        let qid = m.qid;
        let owed: Vec<(usize, u64, u64)> = m.slots.owing().collect();
        for (idx, unique, commit_id) in owed {
            warn!(
                "fuse-over-uring qid={qid} ent={idx}: unanswered request unique={unique} at \
                 teardown; synthesizing EIO (row 8)"
            );
            let _ = commit_id;
            match fail_ent(
                &mut ring,
                &mut batch,
                &mut m.slots,
                &mut m.ents[idx],
                &m.lease_states[idx],
                pool.slot_watch_cell(qid, idx),
                qid,
                gent_of(mi, idx),
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
                    m.slots.abandon(idx);
                    pool.publish_slot_owed(qid, idx, 0);
                }
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
    debug!("fuse-over-uring qids={qids:?} worker exit");
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
    gent: usize,
    commit_id: u64,
) -> io::Result<()> {
    submit_commit_retain(ring, batch, slots, watch, qid, gent, commit_id, false)
}

/// [`submit_commit`] with the RETAIN arm (design-zc-write-kernel-v2
/// §3.3): `retain = true` carries `FUSE_URING_COMMIT_RETAIN` in the
/// cmd-req union (the kernel ends the request, keeps the zc slot
/// registered, and parks the commit — no CQE until RELEASE), transitions
/// the slot to `Retained`, and fires any release that raced ahead of
/// this commit (the release-after-commit ordering protocol — the two
/// SQEs ride ONE batch, so the kernel processes them in order and can
/// never answer `-EBUSY`).
#[allow(clippy::too_many_arguments)] // one wire word per SQE field (see push_cmd_batched)
fn submit_commit_retain(
    ring: &mut Ring,
    batch: &mut SubmitBatch,
    slots: &mut SlotTable,
    watch: Option<&SlotWatch>,
    qid: u16,
    gent: usize,
    commit_id: u64,
    retain: bool,
) -> io::Result<()> {
    // `gent` is the GROUP-local ent id (member_slot × depth + ent_idx —
    // the SQE reply-address word); the member's slot table indexes by the
    // queue-local ent, recovered here (`gent % depth` — the table holds
    // exactly one member's `depth` slots) so no caller can pass the pair
    // inconsistently.
    let ent_idx = gent % slots.len();
    // The union bytes at cmd offset 18: `init.flags` on REGISTER,
    // `commit.flags` on COMMIT_AND_FETCH (opcode selects the member).
    let union_flags = if retain {
        kmbuf::FUSE_URING_COMMIT_RETAIN
    } else {
        0
    };
    push_cmd_batched(
        ring,
        batch,
        FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
        qid,
        commit_id,
        None,
        encode_user_data(RingOp::Commit, gent),
        union_flags,
        0,
        0,
    )?;
    if retain {
        slots.on_commit_submitted_retained(ent_idx, commit_id);
        kmbuf::note_zc_retain_commit();
        if slots.take_release_pending(ent_idx) {
            push_release(ring, batch, slots, qid, gent, commit_id)?;
        }
    } else {
        slots.on_commit_submitted(ent_idx, commit_id);
    }
    if let Some(w) = watch {
        w.clear();
    }
    Ok(())
}

/// Push one RELEASE_PAYLOAD uring_cmd for a retained ent and transition
/// the slot back to `Replied` (the kernel re-arms the parked commit into
/// the fetch path; its CQE is the next delivery). Counted at submission
/// — the RELEASE cmd's own CQE is accounting-only (`0` expected; nonzero
/// rides the `fuse3_zc_release_failures` must-stay-0 tripwire).
fn push_release(
    ring: &mut Ring,
    batch: &mut SubmitBatch,
    slots: &mut SlotTable,
    qid: u16,
    gent: usize,
    commit_id: u64,
) -> io::Result<()> {
    push_cmd_batched(
        ring,
        batch,
        FUSE_IO_URING_CMD_RELEASE_PAYLOAD,
        qid,
        commit_id,
        None,
        encode_user_data(RingOp::Release, gent),
        0,
        0,
        0,
    )?;
    let ent_idx = gent % slots.len();
    slots.on_release_submitted(ent_idx);
    kmbuf::note_zc_release();
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
    gent: usize,
    errno: i32,
) -> io::Result<bool> {
    // Queue-local slot index of the group-local ent id (see
    // `submit_commit` — same recovery, same invariant).
    let ent_idx = gent % slots.len();
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
    match submit_commit(ring, batch, slots, watch, qid, gent, commit_id) {
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
        // A body-carrying reply on an ent with no attached buffer. On a
        // zc session this is THE reverse opcode-mirror miss: the kernel
        // treated this opcode as PAGED (no kmbuf attach, folio copy
        // skipped at COMMIT) while `zc::out_paged` calls it copyable —
        // the 7.1 readdir divergence's exact shape. The reply degrades
        // to a loud header-only EIO — never an OOB write, never silent
        // garbage — counted on the mirror tripwire and NAMING the op +
        // resolved track so the mirror table can be fixed for it.
        if kmbuf::zc_negotiated() != 0 {
            kmbuf::note_zc_fallback();
            error!(
                "fuse-over-uring: body-carrying reply for opcode {} with no kmbuf \
                 attachment on a zc session (kmbuf_ops={}) — the kernel treats this \
                 opcode as out-paged and zc::out_paged does not: fix the mirror for \
                 this track — committing EIO (fuse3_zc_fallbacks)",
                ent.last_opcode,
                kmbuf::resolved_opcodes_label()
            );
        } else {
            // Non-zc kmbuf mode: structurally unreachable (the kernel
            // attaches a buffer to every request with out args).
            error!(
                "fuse-over-uring: body-carrying reply for opcode {} on an ent with \
                 no payload buffer (kmbuf attachment law violated, kmbuf_ops={}) — EIO",
                ent.last_opcode,
                kmbuf::resolved_opcodes_label()
            );
        }
        ent.hdr_mut().in_out[..4].copy_from_slice(&((OUT_HDR as u32).to_le_bytes()));
        ent.hdr_mut().in_out[4..8].copy_from_slice(&(-libc::EIO).to_le_bytes());
        ent.hdr_mut().ring_ent_in_out.payload_sz = 0;
        return;
    }

    // FUSE-3d: a reply larger than the ent's payload buffer used to be
    // TRUNCATED silently — `payload_sz` reported the short length while
    // `fuse_out_header.len` still claimed the full one, so the kernel
    // copied a header promising N bytes over a body of M < N. That is a
    // corrupt reply, not a small one. Refuse it the way every other
    // degenerate case is refused: header-only `-EIO`, counted, loud.
    let want = header.len() - OUT_HDR + body.len();
    if want > ent.payload_len {
        error!(
            "fuse-over-uring: reply body {want} B exceeds the ent payload buffer              ({} B) — committing EIO instead of a truncated reply whose header              would claim the full length",
            ent.payload_len
        );
        TRANSPORT_REPLIES_OVERSIZE.fetch_add(1, Ordering::Relaxed);
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
            // PERF-19: no per-reply `sched_getcpu` when the map cannot
            // express a choice (single-node boxes).
            crate::numa_core::topology().current_node_for_instrument(),
            ent.node,
            body_len,
        );
    }
    payload_len += body_len;

    ent.hdr_mut().ring_ent_in_out.payload_sz = payload_len as u32;
}

/// Place a zc reply: header only + the payload LENGTH — the payload
/// bytes themselves already sit in the request's pages (direct device
/// leg) or are being bridged there by an in-flight slot fetch (bounce
/// leg). Never touches the ent payload region, so it is legal against
/// any lease state (the §5.4 aliasing contract's header-only class).
fn apply_reply_zc(ent: &mut Ent, header: &[u8], payload_len: u32) {
    const OUT_HDR: usize = 16; // sizeof(fuse_out_header)
    ent.hdr_mut().in_out = [0; FUSE_URING_IN_OUT_HEADER_SZ];
    if header.len() < OUT_HDR {
        ent.hdr_mut().in_out[..4].copy_from_slice(&((OUT_HDR as u32).to_le_bytes()));
        ent.hdr_mut().in_out[4..8].copy_from_slice(&(-libc::EIO).to_le_bytes());
        ent.hdr_mut().ring_ent_in_out.payload_sz = 0;
        return;
    }
    ent.hdr_mut().in_out[..OUT_HDR].copy_from_slice(&header[..OUT_HDR]);
    ent.hdr_mut().ring_ent_in_out.payload_sz = payload_len;
}

/// Fused timeline, pass-bottom venue: record a HANDLER bridge's issue →
/// resolution span for a CQE the mid-pass reap did NOT consume (it parked
/// through the pass-bottom wait first). Reads the deadline stamp BEFORE
/// the caller's `clear`; non-handler pend classes record nothing.
fn note_handler_bridge_passbottom(
    pend: &Option<ZcPend>,
    deadlines: &zc::BridgeDeadlines,
    ent_idx: usize,
) {
    if matches!(
        pend,
        Some(ZcPend::HandlerFetch { .. }) | Some(ZcPend::HandlerStore { .. })
    ) {
        let born = deadlines.born_ns(ent_idx);
        if born != 0 {
            crate::raw::read_phase::note_fused_bridge_resolved(
                crate::raw::read_phase::transport_now_ns().saturating_sub(born),
                false,
            );
        }
    }
}

/// Push one zc sparse-slot bridge SQE (READ_FIXED / WRITE_FIXED) with the
/// shared batch accounting and SQ-full submit-and-continue rule.
fn push_fetch_batched(ring: &mut Ring, batch: &mut SubmitBatch, entry: Entry128) -> io::Result<()> {
    // SAFETY: the SQE's referenced resources (fds, fixed-buffer indices,
    // offsets) outlive the submission — bounce memfds live in the
    // worker-owned ZcBounce; device fds are pool-lifetime (the root
    // crate's NvmeBlockDev holds them for the mount's life).
    if unsafe { ring.submission().push(&entry) }.is_err() {
        flush_submit(ring, batch)?;
        // SAFETY: as above.
        unsafe { ring.submission().push(&entry) }
            .map_err(|_| io::Error::other("sq full (zc fetch)"))?;
    }
    batch.pending += 1;
    Ok(())
}

/// The §3.5 retention capability probe (design-zc-write-kernel-v2):
/// ONE `RELEASE_PAYLOAD` uring_cmd with an impossible `commit_id` on a
/// scratch SQE128 ring against the raw fuse fd, post-INIT pre-REGISTER.
/// The errno is the verdict (`RetentionSurface::from_probe_errno`):
/// `-ENOTCONN` = opcode present, no ring yet (this shape — measured
/// live ×5 on both sqz tracks); `-EINVAL`/`-EOPNOTSUPP` = pre-0029.
/// Bounded wait (1 s): a kernel that parks the probe cmd is ambiguous
/// and reads Absent, fail-safe — bit 2 is never armed on ambiguity.
fn probe_retention_surface(fuse_fd: RawFd) -> kmbuf::RetentionSurface {
    let res: Result<i32, ()> = (|| {
        let mut ring: Ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder()
            .build(8)
            .map_err(|_| ())?;
        let mut cmd = [0u8; 80];
        let req = FuseUringCmdReq {
            flags: 0,
            commit_id: u64::MAX,
            qid: 0,
            init_flags: 0,
            init_queue_depth: 0,
            padding: [0; 2],
        };
        // SAFETY: FuseUringCmdReq is repr(C), 24 bytes; rest stays zero.
        unsafe {
            std::ptr::write(cmd.as_mut_ptr().cast::<FuseUringCmdReq>(), req);
        }
        let entry: Entry128 =
            opcode::UringCmd80::new(types::Fd(fuse_fd), FUSE_IO_URING_CMD_RELEASE_PAYLOAD)
                .cmd(cmd)
                .build()
                .user_data(1);
        // SAFETY: the cmd bytes are owned by the entry; no user memory
        // is referenced by RELEASE_PAYLOAD.
        unsafe { ring.submission().push(&entry) }.map_err(|_| ())?;
        let ts = types::Timespec::new().sec(1);
        let args = types::SubmitArgs::new().timespec(&ts);
        match ring.submitter().submit_with_args(1, &args) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => return Err(()),
            Err(_) => return Err(()),
        }
        let cqe = ring.completion().next().ok_or(())?;
        Ok(cqe.result())
    })();
    kmbuf::RetentionSurface::from_probe_errno(res)
}

/// Build one FUSE uring-cmd SQE (SQE128) — extracted from [`push_cmd`] so
/// the wire encoding is unit-testable byte-for-byte (the kmbuf REGISTER
/// shape: `init.flags` inside the 80-byte cmd area, `sqe->buf_index` at
/// offset 40, no iovecs).
#[allow(clippy::too_many_arguments)] // one wire word per SQE field (see push_cmd_batched)
fn build_cmd_entry(
    cmd_op: u32,
    qid: u16,
    commit_id: u64,
    iov: Option<(*const libc::iovec, u32)>,
    user_data: u64,
    init_flags: u16,
    init_queue_depth: u16,
    buf_index: u16,
) -> Entry128 {
    let mut cmd = [0u8; 80];
    let req = FuseUringCmdReq {
        flags: 0,
        commit_id,
        qid,
        init_flags,
        init_queue_depth,
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
    init_queue_depth: u16,
    buf_index: u16,
) -> io::Result<()> {
    let entry = build_cmd_entry(
        cmd_op,
        qid,
        commit_id,
        iov,
        user_data,
        init_flags,
        init_queue_depth,
        buf_index,
    );
    unsafe {
        ring.submission()
            .push(&entry)
            .map_err(|_| io::Error::other("submission queue full"))?;
    }
    Ok(())
}

/// FUSE-3d — `apply_reply` must never ship a truncated body under a
/// header claiming the full length.
#[cfg(test)]
mod apply_reply_tests {
    use super::*;

    /// A test ent over an owned payload buffer (no ring, no kernel).
    struct TestEnt {
        _buf: Vec<u8>,
        ent: Ent,
        _header: Box<FuseUringReqHeader>,
    }

    fn test_ent(payload_len: usize) -> TestEnt {
        let mut buf = vec![0u8; payload_len];
        let mut header = Box::new(FuseUringReqHeader::default());
        let header_ptr = &mut *header as *mut FuseUringReqHeader;
        let ent = Ent {
            header_ptr,
            _owned_header: None,
            payload_ptr: buf.as_mut_ptr(),
            payload_len,
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
        TestEnt {
            _buf: buf,
            ent,
            _header: header,
        }
    }

    fn out_len_and_error(ent: &Ent) -> (u32, i32) {
        let h = ent.hdr();
        (
            u32::from_le_bytes(h.in_out[0..4].try_into().unwrap()),
            i32::from_le_bytes(h.in_out[4..8].try_into().unwrap()),
        )
    }

    /// A reply that fits is applied verbatim, header length and
    /// `payload_sz` agreeing.
    #[test]
    fn a_fitting_reply_is_applied_whole() {
        let mut t = test_ent(4096);
        let body = Bytes::from(vec![7u8; 1000]);
        let mut hdr = error_out_header(5, 0).to_vec();
        hdr[0..4].copy_from_slice(&((16 + body.len()) as u32).to_le_bytes());
        apply_reply(&mut t.ent, &hdr, &body);
        let (len, error) = out_len_and_error(&t.ent);
        assert_eq!(error, 0);
        assert_eq!(len as usize, 16 + body.len());
        assert_eq!(
            t.ent.hdr().ring_ent_in_out.payload_sz as usize,
            body.len(),
            "payload_sz must account for exactly the body the header claims"
        );
    }

    /// FUSE-3d: an oversize reply becomes a header-only EIO — never a
    /// short body under a header promising more (which is what the
    /// kernel would then copy out to the application).
    #[test]
    fn an_oversize_reply_becomes_eio_not_a_truncated_body() {
        let mut t = test_ent(512);
        let body = Bytes::from(vec![9u8; 4096]);
        let mut hdr = error_out_header(6, 0).to_vec();
        hdr[0..4].copy_from_slice(&((16 + body.len()) as u32).to_le_bytes());
        let before = transport_reply_integrity_stats().6;
        apply_reply(&mut t.ent, &hdr, &body);
        let (len, error) = out_len_and_error(&t.ent);
        assert_eq!(error, -libc::EIO, "an unshippable reply must fail loud");
        assert_eq!(len, 16, "the header must claim exactly what is shipped");
        assert_eq!(t.ent.hdr().ring_ent_in_out.payload_sz, 0);
        assert!(
            transport_reply_integrity_stats().6 > before,
            "the refusal must be counted (transport_replies_oversize)"
        );
    }
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

    /// The retained arc (design-zc-write-kernel-v2 §3.3): a commit
    /// submitted WITH RETAIN parks the ent kernel-side (no CQE), so the
    /// slot enters `Retained` instead of `Replied`; the RELEASE
    /// submission is the ONLY normal exit, returning the slot to
    /// `Replied` (the kernel owns the ent again — its parked commit's
    /// CQE brings the next delivery).
    #[test]
    fn retain_commit_parks_then_release_rearms() {
        let mut t = table(2);
        t.on_deliver(0, 100, 7);
        assert_eq!(t.admit_commit(0, 7), CommitAdmit::Accept);
        t.on_commit_submitted_retained(0, 7);
        assert_eq!(t.state(0), SlotState::Retained { commit_id: 7 });
        // A retained slot owes NO reply: a second commit is stale, and a
        // teardown fail_ent must synthesize nothing (the request already
        // ended kernel-side at the RETAIN commit).
        assert_eq!(t.admit_commit(0, 7), CommitAdmit::RefuseStale);
        assert_eq!(
            t.fail_ent(0),
            FailOutcome::Nothing,
            "a retained slot's request already ended — teardown owes no synthesis"
        );
        assert_eq!(
            t.state(0),
            SlotState::Retained { commit_id: 7 },
            "fail_ent on a retained slot is a no-op (the kernel drains it at teardown)"
        );
        t.on_release_submitted(0);
        assert_eq!(
            t.state(0),
            SlotState::Replied { commit_id: 7 },
            "RELEASE re-arms: the kernel owns the ent, next CQE is a delivery"
        );
    }

    /// Kernel-refused RETAIN (commit CQE `-EINVAL` while `Retained`):
    /// the recovery is a plain re-commit of the SAME applied reply —
    /// `on_retain_refused` mirrors `on_commit_retry`'s shape.
    #[test]
    fn retain_refused_recovers_to_plain_recommit() {
        let mut t = table(1);
        t.on_deliver(0, 200, 9);
        t.on_commit_submitted_retained(0, 9);
        assert_eq!(t.state(0), SlotState::Retained { commit_id: 9 });
        t.on_retain_refused(0);
        assert_eq!(
            t.state(0),
            SlotState::Delivered {
                unique: 0,
                commit_id: 9
            },
            "the ent still holds its applied reply — re-commit plain, never re-REGISTER"
        );
        assert_eq!(t.admit_commit(0, 9), CommitAdmit::Accept);
        t.on_commit_submitted(0, 9);
        assert_eq!(t.state(0), SlotState::Replied { commit_id: 9 });
    }

    /// Release on a slot that is not retained is a stale no-op (the
    /// teardown race) — never a transition, never a panic.
    #[test]
    fn release_on_unretained_slot_is_inert() {
        let mut t = table(1);
        t.on_deliver(0, 300, 4);
        t.on_release_submitted(0);
        assert_eq!(
            t.state(0),
            SlotState::Delivered {
                unique: 300,
                commit_id: 4
            }
        );
    }

    /// The COMMIT wire face of RETAIN: the union bytes at offset 18 of
    /// the 24-byte `fuse_uring_cmd_req` carry `commit.flags` on
    /// COMMIT_AND_FETCH (the same bytes REGISTER reads as `init.flags` —
    /// the kernel selects the union member by opcode). A plain commit
    /// keeps them zero; a RETAIN commit carries bit 0.
    #[test]
    fn commit_entry_carries_retain_in_the_union_bytes() {
        let plain = build_cmd_entry(
            FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
            3,
            0xABCD,
            None,
            42,
            0,
            0,
            0,
        );
        let retained = build_cmd_entry(
            FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
            3,
            0xABCD,
            None,
            42,
            kmbuf::FUSE_URING_COMMIT_RETAIN,
            0,
            0,
        );
        // Entry128 = (Entry, [u8; 64]); the 80-byte cmd area starts at
        // SQE offset 48, so the cmd_req's union u16 sits at 48+18 = 66.
        let bytes_of = |e: &Entry128| -> [u8; 128] {
            // SAFETY: Entry128 is a POD wrapper over the 128-byte SQE.
            unsafe { std::mem::transmute_copy(e) }
        };
        let p = bytes_of(&plain);
        let r = bytes_of(&retained);
        assert_eq!(u16::from_le_bytes([p[66], p[67]]), 0);
        assert_eq!(
            u16::from_le_bytes([r[66], r[67]]),
            kmbuf::FUSE_URING_COMMIT_RETAIN,
            "RETAIN must land in the union u16 the kernel reads as commit.flags"
        );
        // Everything else identical (the flag is the ONLY delta).
        for i in 0..128 {
            if i == 66 || i == 67 {
                continue;
            }
            assert_eq!(p[i], r[i], "byte {i} differs beyond the union flags");
        }
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
            0,
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

        // zc REGISTER (K1 kill): BUF_RING|ZERO_COPY + init.queue_depth,
        // still no iovecs, buf_index = ent slot.
        let e = build_cmd_entry(
            FUSE_IO_URING_CMD_REGISTER,
            6,
            0,
            None,
            9,
            super::kmbuf::init_flags(true, true),
            32,
            13,
        );
        assert_eq!(read_u64(&e, 16), 0, "zc REGISTER carries no iov ptr");
        assert_eq!(read_u16(&e, 40), 13, "sqe->buf_index = sparse slot id");
        assert_eq!(
            read_u16(&e, 48 + 18),
            super::kmbuf::FUSE_URING_BUF_RING | super::kmbuf::FUSE_URING_ZERO_COPY,
            "cmd_req.init.flags = BUF_RING | ZERO_COPY"
        );
        assert_eq!(read_u16(&e, 48 + 20), 32, "init.queue_depth = ring depth");

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
            if now != expected {
                // Self-naming failure: dump every task comm — libtest names
                // threads after their running test, so a poller-count
                // violation names the concurrent test set that produced it.
                // (Armed permanently after a ~1/100 in-suite firing that
                // was 120/120 solo-green — the next firing attributes
                // itself instead of costing another hunt.)
                let mut dump = String::new();
                if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
                    for t in tasks.flatten() {
                        let comm =
                            std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
                        dump.push_str(&format!(
                            "  tid={} comm={}\n",
                            t.file_name().to_string_lossy(),
                            comm.trim_end()
                        ));
                    }
                }
                panic!("{ctx}: now={now} expected={expected}\ntasks:\n{dump}");
            }
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
        let shutdown = crate::sqz_notify::Notify::new();
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
        let shutdown = Arc::new(crate::sqz_notify::Notify::new());
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
        let shutdown = Arc::new(crate::sqz_notify::Notify::new());
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
        crate::sqz_time::sleep(Duration::from_millis(20)).await;
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

/// FUSE-3f — completion-queue overflow is DETECTED, not assumed away.
///
/// The queue rings are built `cqsize = sq * 2` and one drain pass can push
/// `depth` commits + `depth` re-REGISTERs + one poll re-arm, so the
/// geometry is meant to make overflow unreachable. "Meant to" was the
/// whole finding: `IORING_FEAT_NODROP` was never probed and
/// `cq.overflow()` was never read, so a dropped completion — a REGISTER or
/// COMMIT_AND_FETCH that never lands — was indistinguishable from an idle
/// ring. On a kernel without NODROP a full CQ drops the event on the floor;
/// with NODROP the counter still moves when the kernel cannot even
/// allocate its internal overflow entry. Either way it is a lost ring
/// completion and it must be counted, loudly.
#[cfg(test)]
mod cq_overflow_tests {
    use super::*;

    /// `TRANSPORT_CQ_{OVERFLOWS,NODROP}` are process-global (the product
    /// publishes ONE probe per process), and `CqDropWatch::new` stores to
    /// the NODROP word — so every test in this module that reads either
    /// global must hold this lock, or a sibling test's constructor races
    /// its assertion (observed ~1/20 under the default parallel harness).
    static GLOBALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_quiet_ring_reports_no_drops() {
        let _g = GLOBALS.lock().unwrap();
        let mut w = CqDropWatch::new(true);
        for _ in 0..8 {
            assert_eq!(w.observe(0), 0, "an un-overflowed CQ must report nothing");
        }
    }

    #[test]
    fn newly_dropped_completions_are_reported_once_each() {
        let _g = GLOBALS.lock().unwrap();
        let mut w = CqDropWatch::new(false);
        assert_eq!(w.observe(3), 3, "the first observation reports the backlog");
        assert_eq!(w.observe(3), 0, "an unchanged counter is not re-reported");
        assert_eq!(w.observe(5), 2, "only the DELTA is new loss");
    }

    /// The kernel counter is a `u32` written with plain stores; a wrapped
    /// counter must not report ~4 G phantom drops (nor go silent).
    #[test]
    fn the_counter_is_read_as_a_wrapping_delta() {
        let _g = GLOBALS.lock().unwrap();
        let mut w = CqDropWatch::new(false);
        assert_eq!(w.observe(u32::MAX - 1), u32::MAX - 1);
        assert_eq!(w.observe(1), 3, "wraparound is a delta of 3, not 4 billion");
    }

    /// The always-on tripwire moves with the observation (must stay 0 on a
    /// healthy mount — the stats inode carries it next to the FUSE-2
    /// integrity counters).
    #[test]
    fn observations_land_on_the_stats_tripwire() {
        let _g = GLOBALS.lock().unwrap();
        let before = transport_cq_overflow_stats().0;
        let mut w = CqDropWatch::new(false);
        w.observe(7);
        assert_eq!(
            transport_cq_overflow_stats().0,
            before + 7,
            "dropped completions must reach transport_cq_overflows"
        );
    }

    /// The NODROP probe is recorded per session so the stats inode can say
    /// whether this kernel can drop at all.
    #[test]
    fn the_nodrop_probe_is_published() {
        let _g = GLOBALS.lock().unwrap();
        note_cq_nodrop(true);
        assert_eq!(transport_cq_overflow_stats().1, 1);
        note_cq_nodrop(false);
        assert_eq!(transport_cq_overflow_stats().1, 0);
    }
}

/// FUSE-3j — the shutdown lease drain waits ONCE for the whole queue, on
/// the eventfd the lease drop already fires.
///
/// The retired shape slept `1 ms` up to 100 ms **per parked ent, in
/// series**: at depth 32 a queue could spend 3.2 s of teardown polling a
/// word that a lease drop already signals, and `umount`/remount paid it
/// per queue.
#[cfg(test)]
mod drain_wait_tests {
    use super::*;

    fn eventfd() -> OwnedFd {
        // SAFETY: eventfd(2) with a valid flag set; the fd is adopted by
        // OwnedFd, which closes it exactly once.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "eventfd: {}", io::Error::last_os_error());
        // SAFETY: `fd` is a fresh, owned, valid descriptor.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    /// ONE budget for the whole queue: 8 ents leased forever must cost the
    /// budget once, not eight times (the retired per-ent serial poll).
    #[test]
    fn the_budget_is_shared_by_the_whole_queue_not_paid_per_ent() {
        let coalescer = WakeCoalescer::new();
        let wake = eventfd();
        let states: Vec<Arc<EntLeaseState>> =
            (0..8).map(|_| Arc::new(EntLeaseState::new())).collect();
        for s in &states {
            s.acquire();
        }
        let budget = Duration::from_millis(60);
        let t0 = Instant::now();
        let mut waiting: Vec<usize> = (0..states.len()).collect();
        drain_await_leases(wake.as_raw_fd(), &coalescer, &states, &mut waiting, budget);
        let elapsed = t0.elapsed();
        assert_eq!(waiting.len(), 8, "no lease dropped: all still waiting");
        assert!(
            elapsed < budget * 3,
            "the drain spent {elapsed:?} on a {budget:?} budget — the wait is \
             still per-ent instead of one shared deadline"
        );
    }

    /// Event-driven: a lease dropped from another thread resolves the drain
    /// through the eventfd wake, far inside the budget.
    #[test]
    fn a_dropped_lease_wakes_the_drain_through_the_eventfd() {
        let coalescer = Arc::new(WakeCoalescer::new());
        let wake = eventfd();
        let states: Vec<Arc<EntLeaseState>> =
            (0..4).map(|_| Arc::new(EntLeaseState::new())).collect();
        for s in &states {
            s.acquire();
        }
        // The drain publishes `parked` for every waiting ent (that is what
        // makes the releaser fire the eventfd), so park them first.
        for s in &states {
            assert_eq!(s.try_commit(), CommitGate::Parked);
        }
        let releaser = {
            let states: Vec<Arc<EntLeaseState>> = states.iter().map(Arc::clone).collect();
            let fd = wake.as_raw_fd();
            let coalescer = Arc::clone(&coalescer);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(10));
                for s in &states {
                    if s.release() && coalescer.arm() {
                        let one: u64 = 1;
                        // SAFETY: 8-byte write to a live eventfd owned by
                        // the test for the thread's lifetime.
                        unsafe { libc::write(fd, &one as *const u64 as *const _, 8) };
                    }
                }
            })
        };
        let budget = Duration::from_millis(2_000);
        let t0 = Instant::now();
        let mut waiting: Vec<usize> = (0..states.len()).collect();
        drain_await_leases(wake.as_raw_fd(), &coalescer, &states, &mut waiting, budget);
        let elapsed = t0.elapsed();
        releaser.join().expect("releaser thread");
        assert!(
            waiting.is_empty(),
            "every dropped lease must be resolved by the drain, {} left",
            waiting.len()
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "the drain took {elapsed:?} for a lease dropped at 10 ms — it is \
             not waking on the eventfd"
        );
    }

    /// A lease that drops BEFORE the drain looks (the common case) costs no
    /// wait at all.
    #[test]
    fn an_already_free_ent_costs_no_wait() {
        let coalescer = WakeCoalescer::new();
        let wake = eventfd();
        let states: Vec<Arc<EntLeaseState>> =
            (0..4).map(|_| Arc::new(EntLeaseState::new())).collect();
        let t0 = Instant::now();
        let mut waiting: Vec<usize> = (0..states.len()).collect();
        drain_await_leases(
            wake.as_raw_fd(),
            &coalescer,
            &states,
            &mut waiting,
            Duration::from_millis(500),
        );
        assert!(waiting.is_empty());
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "an unleased queue must not wait at all"
        );
    }
}

/// Ingress-queue-spread lever 2 (2026-08-05 evidence note): the drain-group
/// plan — one drain context (thread + ring + eventfd + coalescer) per
/// NUMA-node-contiguous GROUP of FUSE queues, so a spread of shallow
/// submitters (32 queues at ~8-deep — the measured anti-scaling shape)
/// aggregates into per-context batches instead of 32 wake-per-few-ops
/// contexts. The plan is pure and topology-injectable (the numa_core test
/// convention) so every law here is checkable without a mount.
#[cfg(test)]
mod drain_group_tests {
    use super::*;

    /// THE FIELD SHAPE (2026-08-05 disengagement finding): squeeze-test's
    /// 2-socket node numbering is INTERLEAVED — node0 = even qids, node1 =
    /// odd qids (a common BIOS round-robin numbering). The contiguous-run
    /// plan derived 32 singleton groups there (every run length 1 —
    /// `transport_drain_groups=32 width=1`, the A0 posture, structurally
    /// disengaged). Grouping is by node MEMBERSHIP: two 16-qid sets, the
    /// cpus/4 slope per set ⇒ 8 groups of 4, every group single-node —
    /// the portable-by-default law applied to arbitrary NUMBERINGS, not
    /// just arbitrary domain counts.
    #[test]
    fn interleaved_node_numbering_groups_by_membership() {
        let node_of = |cpu: usize| Some(cpu % 2); // node0 even, node1 odd
        let plan = drain_group_plan(32, TransportBufferMode::UserEnts, true, node_of, None);
        assert_eq!(
            plan.len(),
            8,
            "2 nodes × (16/4) contexts — never 32 singletons: {plan:?}"
        );
        assert!(
            plan.iter().all(|g| g.len() == 4),
            "width = node population / 4 on 16-possible nodes: {plan:?}"
        );
        // Node containment + qid order preserved within each group.
        for g in &plan {
            assert!(
                g.iter().all(|&q| q % 2 == g[0] % 2),
                "a group never spans nodes: {g:?}"
            );
            assert!(
                g.windows(2).all(|w| w[0] < w[1]),
                "qid order preserved within the group: {g:?}"
            );
        }
        // The SQPOLL-leader law: exactly one group leads with qid 0.
        assert_eq!(
            plan.iter().filter(|g| g[0] == 0).count(),
            1,
            "exactly one group carries qid 0 as its first member"
        );
        // The first node's members are the even qids, chunked in order.
        assert_eq!(plan[0], vec![0, 2, 4, 6]);
    }

    /// Two-node 32-CPU contiguous-block box: the `cpus/4` slope on each
    /// 16-possible node gives width 4 — four contexts per node, groups
    /// never spanning a node (member arenas stay node-local to their
    /// drain thread). Contiguous boxes derive the SAME memberships the
    /// run-based plan produced.
    #[test]
    fn default_width_is_the_cpus_over_4_slope_node_split() {
        let node_of = |cpu: usize| Some(cpu / 16); // 2 nodes × 16
        let plan = drain_group_plan(32, TransportBufferMode::UserEnts, true, node_of, None);
        let expect: Vec<Vec<u16>> = (0u16..32)
            .step_by(4)
            .map(|s| (s..s + 4).collect())
            .collect();
        assert_eq!(
            plan, expect,
            "width = node population / 4: 4 contexts per 16-possible node"
        );
    }

    /// Wider nodes get MORE contexts at the same slope, never wider
    /// whole-node groups: the counted bracket showed one context per node
    /// collapsing on the single-thread drain ceiling (width-32 −15 % IOPS
    /// at every point vs widths 1–8 on the clean venue).
    #[test]
    fn width_scales_with_the_node_never_the_whole_node() {
        let node_of = |_cpu: usize| Some(0);
        let plan = drain_group_plan(64, TransportBufferMode::UserEnts, true, node_of, None);
        assert_eq!(plan.len(), 4, "64-possible node: 4 contexts at cpus/4");
        assert!(plan.iter().all(|g| g.len() == drain_group_width(64)));
        // The bracket-validated shape: 32-possible/1-node ⇒ width 8 —
        // byte-identical to the counted bracket winner, so the A/B rows
        // carry over verbatim.
        let plan = drain_group_plan(32, TransportBufferMode::UserEnts, true, node_of, None);
        let expect: Vec<Vec<u16>> = (0u16..32)
            .step_by(8)
            .map(|s| (s..s + 8).collect())
            .collect();
        assert_eq!(plan, expect);
        // Tiny boxes land on the floor: 4 possible CPUs ⇒ width 1 —
        // exactly today's per-queue posture.
        let plan = drain_group_plan(4, TransportBufferMode::UserEnts, true, node_of, None);
        assert_eq!(plan, vec![vec![0u16], vec![1], vec![2], vec![3]]);
    }

    /// kmbuf sessions keep today's one-context-per-queue posture
    /// byte-identical: the bufring/fixed-headers registration is per-ring
    /// per-queue in the sqz kernel surface, so grouping under BufRing is
    /// the named follow-on, never a silent behavior fork.
    #[test]
    fn bufring_mode_keeps_singleton_groups() {
        let node_of = |_cpu: usize| Some(0);
        let plan = drain_group_plan(4, TransportBufferMode::BufRing, true, node_of, None);
        assert_eq!(plan, vec![vec![0u16], vec![1], vec![2], vec![3]]);
    }

    /// `SQUEEZEFS_FUSE_DRAIN_GROUP` explicit width wins verbatim (the
    /// ipc-cap explicit-wins pattern) — still membership-partitioned,
    /// because node containment is a structural property (arena
    /// locality), not tuning.
    #[test]
    fn explicit_width_wins_verbatim_but_never_spans_a_node() {
        let node_of = |cpu: usize| Some(cpu / 4); // nodes of 4
        let plan = drain_group_plan(8, TransportBufferMode::UserEnts, true, node_of, Some(3));
        assert_eq!(
            plan,
            vec![vec![0u16, 1, 2], vec![3], vec![4, 5, 6], vec![7]],
            "width 3 chunks inside each 4-wide node"
        );
    }

    /// Testing queue overrides (`qid_is_cpu == false`) have no qid↔CPU
    /// correspondence, so no node info exists: the whole range is one
    /// residual set with the slope evaluated on the machine span, exactly
    /// like the per-queue NUMA placement law (no correspondence ⇒ no
    /// per-queue node derivation).
    #[test]
    fn no_cpu_correspondence_falls_back_to_flat_chunks() {
        let node_of = |_cpu: usize| -> Option<usize> {
            panic!("node lookup must not be consulted without qid↔cpu correspondence")
        };
        // 5 queues: 5/4 = 1 ⇒ singleton groups (the floor posture).
        let plan = drain_group_plan(5, TransportBufferMode::UserEnts, false, node_of, None);
        assert_eq!(plan, vec![vec![0u16], vec![1], vec![2], vec![3], vec![4]]);
        // 32 queues flat: 32/4 = 8 ⇒ four chunks of 8.
        let plan = drain_group_plan(32, TransportBufferMode::UserEnts, false, node_of, None);
        let expect: Vec<Vec<u16>> = (0u16..32)
            .step_by(8)
            .map(|s| (s..s + 8).collect())
            .collect();
        assert_eq!(plan, expect);
    }

    /// Offline-CPU holes ride the RESIDUAL set, never fragmenting node
    /// sets: queues = kernel POSSIBLE CPUs, but sysfs node cpulists carry
    /// only ONLINE ones, so offline/isolated CPUs report no node. They
    /// are dormant by construction (`task_cpu` never names an offline
    /// CPU) and carry no locality constraint, so the residual set chunks
    /// at the MACHINE-span slope — bounded contexts even under a later
    /// mass-online, and never one thread+ring per dormant queue.
    #[test]
    fn offline_cpu_holes_ride_the_residual_set() {
        // The dev-box shape: one node, holes at 4, 6, 14, 16, 22, 24, 30.
        let holes: [usize; 7] = [4, 6, 14, 16, 22, 24, 30];
        let node_of = |cpu: usize| {
            if holes.contains(&cpu) {
                None
            } else {
                Some(0)
            }
        };
        let plan = drain_group_plan(32, TransportBufferMode::UserEnts, true, node_of, None);
        // node0 set = 25 online qids at width 25/4 = 6 ⇒ 5 chunks
        // (6,6,6,6,1); residual = 7 dormant qids at the machine-span
        // width 32/4 = 8 ⇒ ONE group of 7.
        assert_eq!(plan.len(), 6, "5 node chunks + 1 residual group: {plan:?}");
        let residual = plan.last().expect("nonempty plan");
        assert_eq!(
            residual,
            &holes.iter().map(|&c| c as u16).collect::<Vec<_>>(),
            "the residual group is exactly the offline set, qid-ascending"
        );
        assert!(
            plan[..5]
                .iter()
                .all(|g| g.iter().all(|&q| node_of(q as usize) == Some(0))),
            "node chunks carry only node members: {plan:?}"
        );
        // Holes never fragment the node set (the pre-fix dev-box bug
        // shape was 15 fragments).
        assert_eq!(plan[0].len(), drain_group_width(25));
    }

    /// Coverage law: every qid appears exactly once whatever the topology
    /// hands back (interleaved nodes + an unknown-node hole), and every
    /// group is qid-ascending (the worker's member construction order).
    #[test]
    fn plan_covers_every_qid_exactly_once() {
        // Pathological map: alternating nodes + an unknown-node hole.
        let node_of = |cpu: usize| match cpu {
            7 => None,
            c => Some(c % 3),
        };
        let plan = drain_group_plan(17, TransportBufferMode::UserEnts, true, node_of, None);
        let mut seen: Vec<u16> = Vec::new();
        for g in &plan {
            assert!(!g.is_empty(), "no empty groups");
            assert!(g.windows(2).all(|w| w[0] < w[1]), "qid-ascending: {g:?}");
            seen.extend(g.clone());
        }
        seen.sort_unstable();
        assert_eq!(seen, (0u16..17).collect::<Vec<_>>());
    }

    /// The group-local ent id (`gent = member_slot × depth + ent_idx`)
    /// round-trips through the SQE user_data words — the reply-address
    /// law the merged ring rides.
    #[test]
    fn gent_user_data_round_trips() {
        let depth = Q_DEPTH_DESIRED;
        for member in [0usize, 1, 15, 31] {
            for ent in [0usize, 1, depth - 1] {
                let gent = member * depth + ent;
                for op in [
                    RingOp::Register,
                    RingOp::Commit,
                    RingOp::Fetch,
                    RingOp::Cancel,
                ] {
                    let ud = encode_user_data(op, gent);
                    assert_eq!(decode_user_data(ud), Some((op, gent)));
                }
            }
        }
    }

    /// Engagement gauge: the published group stats reflect the armed plan
    /// (groups, max width) — the field row's instrument.
    #[test]
    fn drain_group_stats_reflect_the_plan() {
        let node_of = |_cpu: usize| Some(0);
        let plan = drain_group_plan(8, TransportBufferMode::UserEnts, true, node_of, Some(4));
        publish_drain_group_plan(&plan);
        let (groups, width_max) = drain_group_stats();
        assert_eq!((groups, width_max), (2, 4));
    }
}
