//! **The fused write lane** (zc-write-fusion campaign, 2026-08-07 —
//! ruling D16's fast-tracked fix for the armed rand-4k write caveat).
//!
//! # The term this kills
//!
//! On a zc-armed queue a small hold-candidate WRITE used to dispatch to
//! a foreign handler lane and then pay a per-op cross-thread round trip
//! for every slot-source consumption: handler → `WorkerMsg::ZcStore/
//! ZcExtract` (channel send + eventfd wake) → worker pushes the bridge
//! SQE → CQE → oneshot send → handler-lane wake → handler resumes →
//! `WorkerMsg::Commit` (another wake). Two cross-thread wakes and two
//! schedules per 4 KiB op, on top of the dispatch spawn itself — the
//! zcws-10 bracket's 0.796× rand-4k row at 99.5 % direct engagement.
//!
//! # What is structural and what was not
//!
//! The prior "fusion priced and declined as structural" verdict
//! (`.benchmarks/2026-08-07-zc-bridge-cqe-wedge.md` §8) covered
//! submitting the DMA from a foreign thread — still true:
//! `IORING_SETUP_SINGLE_ISSUER` + `DEFER_TASKRUN` restrict submission
//! to the ring's owner, the payload pages exist only as THAT ring's
//! sparse-table bvec, and the ACK must follow the DMA's CQE. What was
//! never structural is WHERE the handler future runs: its per-poll CPU
//! at small sizes is bounded, so the ring's owner can poll it itself —
//! the shim-iops campaign's reaper/drain fusion (the owning thread
//! consumes its own CQ) and the §5.5.1 inline warm serves are the house
//! patterns. Fused, the store/extract round trip becomes: same-thread
//! channel send → same worker pass pushes the SQE → CQE resumes the
//! future inline on the next pass. Zero cross-thread wakes.
//!
//! # The executor
//!
//! One [`FusedLane`] per drain-group worker: a slab of handler futures
//! plus a shared [`FusedRunQueue`]. Wakers push their task id and wake
//! the worker through the group's EXISTING loom-verified
//! `WakeCoalescer` + eventfd protocol (publish state → `arm()` →
//! eventfd write — the same producer discipline `submit_reply` uses),
//! so no new lock-free machinery exists here: the run queue is a plain
//! `Mutex<VecDeque>` with tiny critical sections, and the parked-worker
//! wake rides the protocol whose loss-freedom is already modeled.
//!
//! The worker drains the lane once per pass, AFTER the eventfd drain +
//! coalescer disarm (the producer-scan law) and BEFORE the commit-
//! channel drain (so a poll's `WorkerMsg` sends are picked up in the
//! same pass). Each poll is wrapped in `catch_unwind`: a panicked
//! handler future is dropped — its `ReplyTx` drop-guard synthesizes the
//! error reply exactly as a TPC-lane panic does — and counted loudly.
//!
//! # Bounding (the non-blocking drain law)
//!
//! Only writes at or under the fusion ceiling are minted onto the lane
//! (`SQUEEZEFS_FUSE_ZC_FUSION_MAX`, default derived payload/8 — the
//! knob doc carries the crossover derivation), and the lane's capacity
//! is the group's aggregate ent depth (a fused task exists only while
//! its ent owes a reply, so the bound is structural; the capacity check
//! is the belt). A future that parks on FS-level state (inode lock,
//! meta commit) parks in the slab — never the thread.

use std::collections::VecDeque;
use std::future::Future;
use std::os::fd::RawFd;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tracing::error;

use super::wake_core::WakeCoalescer;
use super::{InboundUringReq, TRANSPORT_WAKES_ELIDED, TRANSPORT_WAKE_WRITES};

/// A boxed fused handler future (output `()` — every reply path,
/// including the drop-guard synthesis, lives inside it).
pub type FusedFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// How the run queue wakes a (possibly parked) drain-group worker: the
/// group's coalescer + eventfd, the exact `submit_reply` producer
/// protocol. Injectable so the executor core is testable without a
/// ring (tests hand it a bare eventfd).
pub(crate) struct FusedWakeSink {
    pub(crate) coalescer: Arc<WakeCoalescer>,
    pub(crate) wake_fd: RawFd,
}

impl FusedWakeSink {
    fn wake(&self) {
        if self.coalescer.arm() {
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to the group's live eventfd (the
            // fd outlives the pool; same call every producer makes).
            let _ = unsafe { libc::write(self.wake_fd, &one as *const u64 as *const _, 8) };
            TRANSPORT_WAKE_WRITES.fetch_add(1, Ordering::Relaxed);
        } else {
            TRANSPORT_WAKES_ELIDED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The shared ready queue: wakers push task ids; the worker drains them
/// once per pass. Duplicate ids are legal (spurious polls are legal);
/// stale ids (task already completed) are skipped by the slab lookup.
pub(crate) struct FusedRunQueue {
    /// `(task id, push stamp ns)` — the stamp feeds the always-on
    /// `wake_to_poll` timeline histogram (write-IOPS campaign).
    ready: Mutex<VecDeque<(usize, u64)>>,
    sink: FusedWakeSink,
}

impl FusedRunQueue {
    fn push(&self, id: usize) {
        // Publish the observable state FIRST (the producer law), then
        // wake through the coalescer.
        self.ready
            .lock()
            .expect("fused run queue poisoned-free")
            .push_back((id, crate::raw::read_phase::transport_now_ns()));
        self.sink.wake();
    }

    fn pop(&self) -> Option<(usize, u64)> {
        self.ready
            .lock()
            .expect("fused run queue poisoned-free")
            .pop_front()
    }
}

/// Per-task waker: same-process, same-lane — a wake is one mutex push +
/// the coalescer-elided eventfd write (no futex, no scheduler).
struct FusedWaker {
    id: usize,
    rq: Arc<FusedRunQueue>,
}

impl std::task::Wake for FusedWaker {
    fn wake(self: Arc<Self>) {
        self.rq.push(self.id);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.rq.push(self.id);
    }
}

struct FusedTask {
    fut: FusedFuture,
    /// For the loud panic line only (the reply obligation lives in the
    /// future's own `ReplyTx` drop-guard).
    unique: u64,
}

/// The per-worker fused executor. Owned exclusively by its drain-group
/// worker thread (like the `SlotTable`); only the [`FusedRunQueue`] is
/// shared with wakers.
pub(crate) struct FusedLane {
    tasks: slab::Slab<FusedTask>,
    rq: Arc<FusedRunQueue>,
    capacity: usize,
    /// Fused futures that panicked mid-poll (dropped + reply
    /// synthesized by the guard). Mirrors the TPC-lane blast radius.
    pub(crate) panics: u64,
}

impl FusedLane {
    pub(crate) fn new(capacity: usize, coalescer: Arc<WakeCoalescer>, wake_fd: RawFd) -> Self {
        Self {
            tasks: slab::Slab::with_capacity(capacity.max(1)),
            rq: Arc::new(FusedRunQueue {
                ready: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
                sink: FusedWakeSink { coalescer, wake_fd },
            }),
            capacity: capacity.max(1),
            panics: 0,
        }
    }

    /// Number of live fused tasks (parked + ready).
    pub(crate) fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Admit a fused handler future. `false` = at capacity — the caller
    /// demotes to the classic dispatch (never refuses the request).
    pub(crate) fn spawn(&mut self, fut: FusedFuture, unique: u64) -> bool {
        if self.tasks.len() >= self.capacity {
            return false;
        }
        let id = self.tasks.insert(FusedTask { fut, unique });
        // First poll happens on the worker's next lane drain: enqueue
        // through the run queue so mint sites need no poll context.
        self.rq.push(id);
        true
    }

    /// Poll exactly ONE ready task (the pass-interleave primitive —
    /// 2026-08-11 write-DMA-overlap fix): the worker alternates one
    /// handler poll with one WorkerMsg drain + eager ring submit, so a
    /// write's device DMA launches while the NEXT handler's CPU runs
    /// instead of after ALL of them (the serial pass shape left devices
    /// busy ~50 µs of every ~600 µs pass — the measured ~21-in-flight
    /// equilibrium on the 32-CPU rig; nothing here is a width constant,
    /// the overlap scales with whatever the run queue holds). Runs
    /// inside `handle.enter()` so handler futures may use tokio
    /// timers/spawns.
    ///
    /// Returns `false` when the run queue named nothing pollable.
    pub(crate) fn drain_one(&mut self, handle: &tokio::runtime::Handle) -> bool {
        loop {
            let Some((id, pushed_ns)) = self.rq.pop() else {
                return false;
            };
            let Some(task) = self.tasks.get_mut(id) else {
                // Stale wake for a completed/recycled slot — legal;
                // keep looking for a live one so a stale burst cannot
                // starve the interleave of its poll (and it records no
                // wake_to_poll span — only real polls feed the timeline).
                continue;
            };
            crate::raw::read_phase::note_fused_wake_to_poll(
                crate::raw::read_phase::transport_now_ns().saturating_sub(pushed_ns),
            );
            let waker = std::task::Waker::from(Arc::new(FusedWaker {
                id,
                rq: Arc::clone(&self.rq),
            }));
            let mut cx = Context::from_waker(&waker);
            let _rt = handle.enter();
            match std::panic::catch_unwind(AssertUnwindSafe(|| task.fut.as_mut().poll(&mut cx))) {
                Ok(Poll::Ready(())) => {
                    self.tasks.remove(id);
                }
                Ok(Poll::Pending) => {}
                Err(_) => {
                    // Same blast radius as a TPC-lane handler panic:
                    // the future is dropped and its ReplyTx drop-guard
                    // synthesizes the error reply. Loud + counted.
                    let unique = task.unique;
                    self.tasks.remove(id);
                    self.panics += 1;
                    error!(
                        unique,
                        "fuse3: fused write handler PANICKED mid-poll — future dropped, \
                         reply synthesized by the drop-guard (fused lane panic #{})",
                        self.panics
                    );
                }
            }
            return true;
        }
    }

    /// Drain every task the run queue names, bounded by the population
    /// at entry (wakes produced by these polls are consumed by the NEXT
    /// pass — the coalescer's eventfd keeps the worker from parking over
    /// them). One [`Self::drain_one`] per ready task; the production
    /// worker interleaves `drain_one` with its message pump + eager ring
    /// flush instead (the DMA-overlap shape), so this whole-queue form
    /// survives only as the contract tests' harness (`#[cfg(test)]` —
    /// the no-dead-code law's cfg-completeness exception).
    ///
    /// Returns the number of polls performed.
    #[cfg(test)]
    pub(crate) fn drain(&mut self, handle: &tokio::runtime::Handle) -> usize {
        let budget = {
            self.rq
                .ready
                .lock()
                .expect("fused run queue poisoned-free")
                .len()
        };
        let mut polls = 0;
        for _ in 0..budget {
            if !self.drain_one(handle) {
                break;
            }
            polls += 1;
        }
        polls
    }
}

/// The fused-dispatch eligibility core (pure — pinned by unit tests):
/// fuse a zc WRITE delivery of `len` payload bytes iff the lever is on
/// and the length sits at or under the ceiling. Zero-length WRITEs are
/// never held (the delivery path handles them before this predicate).
pub fn fuse_candidate(len: u32, ceiling: u32, enabled: bool) -> bool {
    enabled && len > 0 && len <= ceiling
}

/// The fusion ceiling: explicit `SQUEEZEFS_FUSE_ZC_FUSION_MAX` wins
/// verbatim (the env-knob law — out-of-range refused at daemon startup
/// by the registry gate, so the parse here is trust-the-gate); the
/// default derives from the negotiated transport payload size:
/// `payload/8` = 128 KiB at the shipped 1 MiB geometry. Derivation: the
/// hop the fused lane deletes is two cross-thread wakes + two schedules
/// (the ~5–10 µs class the shim-iops ledger prices); at DRAM-bandwidth
/// merge copy (~10 GB/s single-thread) that buys ~50–100 KiB of inline
/// work, and payload/8 brackets that crossover from above while keeping
/// every rand-small shape (the campaign's population) inside. Always
/// ≤ the hold bound's ceiling by construction at the default; an
/// explicit larger value is the operator's verbatim choice.
pub(crate) fn fusion_ceiling(payload_sz: usize) -> u32 {
    if let Ok(v) = std::env::var("SQUEEZEFS_FUSE_ZC_FUSION_MAX") {
        if let Ok(n) = v.trim().parse::<u32>() {
            if n >= 4096 {
                return n;
            }
        }
    }
    (payload_sz / 8).max(4096) as u32
}

/// The fusion lever (`SQUEEZEFS_FUSE_ZC_WRITE_FUSION`, **default ON** —
/// RESOLVED 2026-08-08, fused-lane-predicate campaign: the 2026-08-08
/// field falsification (armed rand-4k 0.45× at ~300 µs fabric RTT) was
/// the PREDICATE, not the lane — shape-only `hold_candidate` held/fused
/// W1-ineligible shapes, which then paid hold + fused poll + LATE
/// extraction serialized at fabric RTT (the both-vehicles signature:
/// fusions ≈ ops ∧ extractions ≈ ops, 46–97 % reproduced on the netem
/// venue). With the hold gated on the filesystem's W1-eligibility seam
/// (`zc_write_hold_eligible`) and ineligible shapes extracting at
/// delivery on the classic dispatch, the fabric-emulated A-B-B-A reads
/// fused ≥ fusion-off on the field shape AND the eligible shape, armed
/// ≥ unarmed on both, and the un-emulated W1-shape win survives
/// (+11.6 %, both bracket orders) — evidence
/// `.benchmarks/2026-08-08-fused-lane-predicate.md`. `0` is the A/B
/// control; absent/malformed keeps the default (the transport lever
/// law — the daemon's startup gate already refused a bad value).
pub(crate) fn fusion_enabled() -> bool {
    crate::env_knob_core::parse_bool(
        "SQUEEZEFS_FUSE_ZC_WRITE_FUSION",
        std::env::var("SQUEEZEFS_FUSE_ZC_WRITE_FUSION")
            .ok()
            .as_deref(),
    )
    .ok()
    .flatten()
    .unwrap_or(true)
}

/// A registered fused-write dispatcher: the session's mint (builds the
/// WRITE handler future for one delivery) plus the runtime handle whose
/// timers/spawn the polled futures may use.
pub struct FusedWriteDispatch {
    pub mint: Arc<dyn Fn(InboundUringReq) -> FusedFuture + Send + Sync + 'static>,
    pub handle: tokio::runtime::Handle,
}

/// The zc-write HOLD gate (fused-lane-predicate campaign, 2026-08-08):
/// `(ino, offset, len, odirect) → may this payload stay HELD in the
/// sparse slot as a direct-consume candidate?` — the transport face of
/// `Filesystem::zc_write_hold_eligible`. `odirect` is the GUP/O_DIRECT
/// class (open flags carry `O_DIRECT` and the op is not
/// `FUSE_WRITE_CACHE`): overlay-eligible O_DIRECT must return false so
/// the worker extracts at delivery (batched) instead of a late handler
/// extract on the ACK path. Called on the queue-worker thread once per
/// armed WRITE delivery: must be cheap, sync, non-blocking, lock-free.
pub type ZcHoldGate = Arc<dyn Fn(u64, u64, u32, bool) -> bool + Send + Sync + 'static>;

/// Count one LATE (handler-initiated lazy) extraction of a HELD write —
/// the hold gate's staleness gauge (`fuse3_zc_write_lazy_extractions`):
/// the delivery-time eligibility hint said hold, the handler's
/// authoritative predicate then declined (racing overlay park, clone
/// pin, custody move), so the payload extracted AFTER dispatch instead
/// of at delivery. Bounded by construction (once per request, the
/// memoized materialize); **≈ 0 in steady state** — sustained growth
/// means the delivery-time probe drifted from the handler's ladder (the
/// field-falsification class re-forming).
pub fn note_zc_write_lazy_extraction() {
    ZC_WRITE_LAZY_EXTRACTIONS.fetch_add(1, Ordering::Relaxed);
}

/// `fuse3_zc_write_lazy_extractions` (stats inode): see
/// [`note_zc_write_lazy_extraction`].
pub fn zc_write_lazy_extractions() -> u64 {
    ZC_WRITE_LAZY_EXTRACTIONS.load(Ordering::Relaxed)
}

static ZC_WRITE_LAZY_EXTRACTIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Count one fused dispatch + its payload bytes (the engagement pair —
/// `fuse3_zc_write_fusions`/`_bytes` on the stats inode). Counted AT
/// MINT on the worker; the vehicle ledgers (direct/extraction) keep
/// counting at their consuming sites, so fusion changes the VENUE
/// attribution, never the vehicle attribution.
pub fn note_zc_write_fusion(bytes: u64) {
    ZC_WRITE_FUSIONS.fetch_add(1, Ordering::Relaxed);
    ZC_WRITE_FUSION_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// `fuse3_zc_write_fusions` (stats inode): armed WRITEs whose handler
/// ran on the queue worker's fused lane. 0 by construction until a
/// session arms zc with the fusion lever on.
pub fn zc_write_fusions() -> u64 {
    ZC_WRITE_FUSIONS.load(Ordering::Relaxed)
}

/// `fuse3_zc_write_fusion_bytes` (stats inode): the byte face of
/// [`zc_write_fusions`].
pub fn zc_write_fusion_bytes() -> u64 {
    ZC_WRITE_FUSION_BYTES.load(Ordering::Relaxed)
}

/// Count one fusion-eligible delivery that had to ride the classic
/// dispatch anyway (no dispatcher registered yet / lane at capacity).
/// Ceiling declines are NOT demotions — they are the bound working.
pub fn note_zc_write_fusion_demotion() {
    ZC_WRITE_FUSION_DEMOTIONS.fetch_add(1, Ordering::Relaxed);
}

/// `fuse3_zc_write_fusion_demotions` (stats inode): ≈ 0 in steady state
/// (a burst at mount arm — deliveries before the session registers the
/// dispatcher — is legal; sustained growth means the lane is saturated
/// or the registration never happened).
pub fn zc_write_fusion_demotions() -> u64 {
    ZC_WRITE_FUSION_DEMOTIONS.load(Ordering::Relaxed)
}

static ZC_WRITE_FUSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ZC_WRITE_FUSION_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ZC_WRITE_FUSION_DEMOTIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn test_lane(capacity: usize) -> (FusedLane, RawFd, Arc<WakeCoalescer>) {
        // SAFETY: fresh eventfd, closed by the test process at exit
        // (lanes never close their fd — the group owns it in prod).
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "eventfd");
        let coalescer = Arc::new(WakeCoalescer::new());
        (
            FusedLane::new(capacity, Arc::clone(&coalescer), fd),
            fd,
            coalescer,
        )
    }

    fn drain_eventfd(fd: RawFd) -> u64 {
        let mut buf = 0u64;
        // SAFETY: reading 8 bytes from our own eventfd.
        let n = unsafe { libc::read(fd, &mut buf as *mut u64 as *mut _, 8) };
        if n == 8 {
            buf
        } else {
            0
        }
    }

    /// A ready future completes on the first drain and leaves the lane
    /// empty; the poll count reports the work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fused_lane_runs_ready_future_to_completion() {
        let (mut lane, _fd, _c) = test_lane(4);
        let hit = Arc::new(AtomicU64::new(0));
        let h = Arc::clone(&hit);
        assert!(lane.spawn(
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
            }),
            7
        ));
        assert_eq!(lane.len(), 1);
        let polls = lane.drain(&tokio::runtime::Handle::current());
        assert_eq!(polls, 1);
        assert_eq!(hit.load(Ordering::SeqCst), 1, "future ran");
        assert_eq!(lane.len(), 0, "completed task removed");
        assert_eq!(lane.drain(&tokio::runtime::Handle::current()), 0, "idle");
    }

    /// A parked future stays in the slab without blocking the drain; the
    /// oneshot wake re-queues it (same-thread wake — the hop the lane
    /// exists to kill) and the eventfd is armed for a parked worker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fused_lane_parks_and_resumes_on_wake() {
        let (mut lane, fd, coalescer) = test_lane(4);
        let (tx, rx) = tokio::sync::oneshot::channel::<u32>();
        let got = Arc::new(AtomicU64::new(0));
        let g = Arc::clone(&got);
        assert!(lane.spawn(
            Box::pin(async move {
                let v = rx.await.expect("oneshot");
                g.store(u64::from(v), Ordering::SeqCst);
            }),
            9
        ));
        assert_eq!(lane.drain(&tokio::runtime::Handle::current()), 1);
        assert_eq!(lane.len(), 1, "parked, not completed, not blocking");
        // Mirror the worker's pass-top protocol: drain the eventfd, then
        // DISARM the coalescer (a producer wake after this must re-arm
        // and re-write — the exact posture of a parked worker).
        drain_eventfd(fd);
        coalescer.disarm();

        // Resolve from another thread: the waker must re-queue the task
        // AND write the wake eventfd (the parked-worker path).
        tx.send(42).expect("send");
        let polls = lane.drain(&tokio::runtime::Handle::current());
        assert_eq!(polls, 1, "wake re-queued the task");
        assert_eq!(got.load(Ordering::SeqCst), 42);
        assert_eq!(lane.len(), 0);
        assert!(
            drain_eventfd(fd) > 0,
            "the wake must arm the worker eventfd (a parked worker would \
             otherwise never run the drain)"
        );
    }

    /// Capacity refusal demotes, never drops: spawn returns false and
    /// the caller keeps the classic dispatch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fused_lane_capacity_refusal_is_a_demotion_signal() {
        let (mut lane, _fd, _c) = test_lane(1);
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        assert!(lane.spawn(
            Box::pin(async move {
                let _ = rx.await;
            }),
            1
        ));
        assert!(
            !lane.spawn(Box::pin(async {}), 2),
            "at capacity the lane refuses (the caller demotes)"
        );
        assert_eq!(lane.len(), 1);
    }

    /// A panicking future is contained: dropped, counted, and the lane
    /// keeps serving (the TPC-lane blast-radius law).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fused_lane_contains_handler_panics() {
        let (mut lane, _fd, _c) = test_lane(4);
        assert!(lane.spawn(
            Box::pin(async {
                panic!("handler bug");
            }),
            3
        ));
        let after = Arc::new(AtomicU64::new(0));
        let a = Arc::clone(&after);
        assert!(lane.spawn(
            Box::pin(async move {
                a.fetch_add(1, Ordering::SeqCst);
            }),
            4
        ));
        let polls = lane.drain(&tokio::runtime::Handle::current());
        assert_eq!(polls, 2);
        assert_eq!(lane.panics, 1, "panic counted loudly");
        assert_eq!(lane.len(), 0, "both tasks retired");
        assert_eq!(after.load(Ordering::SeqCst), 1, "the lane kept serving");
    }

    /// A stale wake (task already completed) is skipped, never a panic
    /// and never a spurious poll of a recycled slot's NEW task beyond
    /// the legal-spurious contract.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fused_lane_stale_wakes_are_harmless() {
        let (mut lane, _fd, _c) = test_lane(4);
        assert!(lane.spawn(Box::pin(async {}), 5));
        assert_eq!(lane.drain(&tokio::runtime::Handle::current()), 1);
        // Push a stale id by hand (the waker of the completed task).
        lane.rq.push(0);
        assert_eq!(
            lane.drain(&tokio::runtime::Handle::current()),
            0,
            "stale id skipped"
        );
    }

    /// The eligibility core: lever + ceiling, zero-length never.
    #[test]
    fn fuse_candidate_ladder() {
        assert!(fuse_candidate(4096, 131072, true));
        assert!(fuse_candidate(131072, 131072, true), "ceiling inclusive");
        assert!(!fuse_candidate(131073, 131072, true), "above ceiling");
        assert!(!fuse_candidate(4096, 131072, false), "lever off");
        assert!(!fuse_candidate(0, 131072, true), "zero-length never");
    }

    /// The ceiling derivation: payload/8 with a 4 KiB physical floor
    /// (one LBA — smaller could never hold anyway).
    #[test]
    fn fusion_ceiling_derives_from_payload_geometry() {
        // NOTE: reads the ambient env — the suite never sets the knob.
        assert_eq!(
            fusion_ceiling(1 << 20),
            128 * 1024,
            "shipped 1 MiB geometry"
        );
        assert_eq!(
            fusion_ceiling(4 << 20),
            512 * 1024,
            "sqz-host 4 MiB geometry"
        );
        assert_eq!(fusion_ceiling(8 * 1024), 4096, "physical floor");
    }
}
