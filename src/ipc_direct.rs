//! DIALED P1 — **direct-drive ranged reads** on the shim miss path
//! (`docs/design-preload-interception.md` §5.5/§12 pre-agreed fallback
//! shape; charter `.benchmarks/2026-07-26-ipc-handoff-economy.md` §7).
//!
//! For the governed miss shapes — an O_DIRECT binding's single-block
//! device-class (4–64 KiB) ranged read of a striped, whole-block-mapped,
//! non-overlay block, on either a `direct_device_true` mount (DIALED P1:
//! every such miss) or a DEFAULT hybrid mount when the R1b admission
//! decision DENIES the miss (DIALED P1.5: first touch / Red / cooldown /
//! governor token denial — semantically identical to a device-true
//! serve: ranged device read, no tier publish, nothing to invalidate) —
//! the IPC service thread submits the device read DIRECTLY on this
//! module's ipc-host-owned io_uring and the ring slot completes from
//! the CQE: **no task, no tokio, no handler**. That deletes the per-op
//! handler-lane spawn + full async read-path descent (~14 µs/op of
//! handler CPU on the fabric-latency rig) + the `NvmeBlockDev`
//! channel/oneshot round trip the handoff path pays.
//!
//! ## The policy prelude is exact, synchronous, and complete
//!
//! [`SqueezefsFilesystem::ipc_direct_read_probe`] decides eligibility
//! from RAM-authoritative state only — metadata cache residency +
//! striped layout + whole-block mapping, the overlay/staged-sibling/
//! extent-record screen (ANY overlay presence ⇒ handler; correctness
//! owns ambiguity), and the 795 custody snapshot (binding key + custody
//! epoch + fill incarnation). Any prelude miss falls back to the
//! existing handler path (fallback-is-correctness, the aio slot-reroute
//! posture), recorded in the `ipc_direct_ineligible_*` decision ledger.
//! On DEFAULT mounts the sink's prelude additionally runs the admission
//! decision synchronously (`DataPlaneSink::try_direct_drive_default`):
//! tier probes ride the sync fast path first (O_DIRECT tier hits still
//! serve from tier — the 2026-07-15 hybrid directive), the ghost touch
//! is RECORDED either way (skew evidence keeps accumulating through the
//! ring), and GRANT-shaped escalation candidates route to the handler,
//! where the admission fetch + publish machinery lives unchanged.
//!
//! ## Lock-order lattice
//!
//! The direct-drive path takes **no inode guards and no node locks**
//! across the device I/O — the prelude is lock-free probes (moka / scc
//! / dashmap / atomics), the submit holds only this module's private
//! SQ mutex (never across I/O or any wait), and the CQE side re-runs
//! the same lock-free probes. This matches the shipped ranged-read
//! posture: the handler's own `get_block_range_for_index` descent runs
//! outside the inode lock too.
//!
//! ## Singleflight: bypass, by design
//!
//! R3 ranged reads are "NOT single-flighted by design" (§5.6 of the
//! read-path design): deduping 4 KiB fetches under a 4 MiB block key
//! serializes independent sub-reads for zero byte savings. Direct-drive
//! keeps that posture — device-true reads never publish to any tier, so
//! a concurrent handler whole-block fetch of the same block composes
//! safely: serve validity comes from the post-DMA revalidation (binding
//! + incarnation + custody epoch — the same proof obligation as the
//! handler's validated ranged loop), not from fetch dedup.
//!
//! ## R5 / copy ledger
//!
//! The arena IS the DMA destination on the aligned leg (window ==
//! request and the arena offset takes O_DIRECT-class DMA: 4 KiB-aligned
//! by the client slab allocator's construction) — zero new buffers,
//! zero copies: strictly one better than the handoff path's pool-bounce
//! + `payload.write` copy. Unaligned windows (LBA-rounding skew) or
//! unaligned arena destinations bounce via the existing 64 KiB
//! `RANGED_BUF_POOL` (its own R5 component) and pay ONE copy of the
//! request slice — exactly the handoff path's copy count. Direct-arena
//! DMA is §5.3.1-rule-3 legal: read payloads are uninterpreted bytes
//! (transform volumes are prelude-ineligible — the ranged window read
//! requires passthrough crypto, same as the handler's ranged leg).
//!
//! ## Crash / teardown posture
//!
//! Every in-flight op's [`SlotCompletion`] + [`DataOp`] pin the session
//! mapping `Arc` (§5.3.1 rule 4), so a client kill-9 mid-DMA never
//! unmaps the destination under the CQE — teardown's munmap is ordered
//! after the last accessor structurally. Engine shutdown (sink drop /
//! daemon teardown) marks the flag, NOP-wakes the reaper, and the
//! reaper drains every in-flight CQE before exiting — bounded by device
//! latency, keeping umount prompt.
//!
//! ## Sharding (D12 randread-shim residual, 2026-08-05)
//!
//! Rings + reapers shard by the DERIVED drain-lane width
//! ([`dd_shards_from`] == `il_drain_lanes_default`, the SAME
//! `clamp(3×cpus/8, 2, 64)` slope that ceilings the service threads
//! feeding this engine — re-graded from cpus/4 by the counted 2026-08-06
//! field width sweep), one lane per service thread (`set_service_lane`
//! from the host's `service_loop`; the owner→node partition pins each
//! shard's reaper alongside its submitter). The pre-shard single-shared-ring +
//! single-unpinned-reaper shape serialized every CQE-side serve on ONE
//! thread — the field's 208k-IOPS-flat rand-4k il ceiling at 235 µs
//! fabric RTT × 256 in-flight (fio libaio 32×qd8) while the kernel
//! path spread the same reply work over per-CPU queues (273–280k).
//! Semantics are unchanged per shard: same prelude, same revalidation,
//! same fallback ladder, same accounting.
//!
//! ## Loom posture
//!
//! No new lock-free protocol is introduced: each shard's submission
//! and in-flight table are guarded by ONE ordinary mutex, each shard's
//! CQ has a single consumer (its reaper thread) by construction, and
//! completion reuses the existing loom-modeled slot machinery
//! (`ipc_slot_core`). The lane routing is a thread-local read.

use crate::fuse_client::{IpcDirectSnapshot, SqueezefsFilesystem, METRICS};
use crate::ipc_host::{DataOp, SlotCompletion};
use io_uring::{opcode, types, IoUring};
use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Reaper-wake sentinel (shutdown NOP) — never a slab index.
const NOP_WAKE: u64 = u64::MAX;

/// TEST SEAM (the block-conveyor rails' determinism gate — the
/// `data_custody::test_clear_poison` precedent): while held, the engine's
/// CQE consumption pauses (`drain_cq_locked` returns without touching the
/// CQ — nothing is lost, the reaper's bounded 100 ms cadence resumes it),
/// which pins a write leader's guard tenure open so same-block followers
/// deterministically observe a lane-held block. One relaxed load per
/// drain pass when idle; production never sets it.
static TEST_DDW_CQE_HOLD: AtomicBool = AtomicBool::new(false);

/// Arm/release the CQE-consumption hold (tests only — see
/// `TEST_DDW_CQE_HOLD`). Callers own reopening it: the test suites wrap
/// it in an RAII guard so a panicking assertion can never wedge the
/// engine's shutdown drain behind a closed gate.
pub fn set_test_ddw_cqe_hold(hold: bool) {
    TEST_DDW_CQE_HOLD.store(hold, Ordering::SeqCst);
}

/// Tests-only engine registry (the hold seam's companion): the last
/// spawned engine, weakly held so the seam never extends a shutdown.
static TEST_ENGINE: Mutex<Option<std::sync::Weak<DirectDriveEngine>>> = Mutex::new(None);

/// Tests-only: CQEs currently POSTED-and-unconsumed across the engine's
/// shards (a peek under each shard's `cq_gate` — never consumes). With
/// the hold armed this is the deterministic "all K completions are
/// queued" gate the ACK-fast drain rails wait on (a bare counter wait on
/// submits would release before the DMAs completed — a race, not a
/// contract).
pub fn test_ddw_cq_ready() -> usize {
    let engine = TEST_ENGINE
        .lock()
        .expect("test engine registry never poisons")
        .as_ref()
        .and_then(std::sync::Weak::upgrade);
    let Some(engine) = engine else { return 0 };
    let mut ready = 0usize;
    for shard in &engine.shards {
        let _gate = shard
            .cq_gate
            .lock()
            .expect("direct-drive cq gate never poisons");
        // SAFETY: single CQ accessor under the gate (module docs).
        let mut cq = unsafe { shard.ring.completion_shared() };
        cq.sync();
        ready += cq.len();
    }
    ready
}

/// SQ/CQ entries PER SHARD. 512 in-flight direct reads ≫ any observed
/// per-lane governed depth (t32qd32 offers ≤ 1024 across 8 service
/// threads, and each service thread owns its own shard; SQ-full
/// refusals fall back to the handler — counted, never stranded).
const RING_ENTRIES: u32 = 512;

/// Shard width: env lever wins verbatim (clamp 1..=64 — the
/// `service_thread_ceiling_from` env-clamp parity), derived default =
/// [`squeezefs_ipc::sizing::il_drain_lanes_default`] — the ONE drain-LANE
/// slope (`clamp(3×cpus/8, 2, 64)`, the counted 2026-08-06 field width
/// sweep: W12 695–700k vs W8 622–636k vs W24 616–626k on the 32-CPU
/// squeeze-test shape, `.benchmarks/2026-08-06-dd-width-slope.md`), the
/// SAME function that ceilings the service threads feeding this engine,
/// so lanes and service threads are 1:1 by construction (the
/// ingest-economy paired-derivation law: two independent constants here
/// would be the DEFAULTS-MISMATCH class again — and a shard set wider
/// than the ceiling is production-DARK, since governed submits only ever
/// ride owner-indexed lanes). Unit-pinned by
/// `dd_shard_width_derivation_ties_to_drain_lane_width`.
pub fn dd_shards_from(env: Option<&str>, cpus: usize) -> usize {
    env.and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, 64))
        .unwrap_or_else(|| squeezefs_ipc::sizing::il_drain_lanes_default(cpus))
}

fn dd_shards() -> usize {
    dd_shards_from(
        std::env::var("SQUEEZEFS_IPC_DD_SHARDS").ok().as_deref(),
        // PROCESS parallelism, never `available_parallelism()` — the
        // engine spawns lazily from a service thread the NUMA partition
        // may have pinned to one node (the Hang-1 pinned-first-toucher
        // sizing poison, `uring_fs::resolve_worker_count`'s law).
        crate::cpu::process_parallelism(),
    )
}

/// Mid-sweep eager-flush lever (`SQUEEZEFS_IPC_DD_EAGER_FLUSH`,
/// re-graded by the r4 lane-depth campaign): explicit `K > 0` wins
/// verbatim (enter once a lane's unflushed SQE count reaches K);
/// explicit `0` = the pre-r4 SWEEP-ONLY posture (one enter per drain
/// sweep — the A/B lever); ABSENT = the DERIVED adaptive threshold
/// ([`dd_eager_threshold`]). The sweep-only default was the r4 red
/// baseline's per-lane depth ceiling: SQEs pushed mid-sweep became
/// kernel-visible only at the sweep tail (+ the reaper's
/// post-completion re-enter), so at fabric RTT the lane's device
/// concurrency was bounded by issue CADENCE, not by demand — the
/// field's ~18-per-lane arithmetic (`.benchmarks/
/// 2026-08-08-dd-lane-depth-r4.md`).
fn dd_eager_flush() -> Option<u32> {
    static V: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        dd_eager_flush_from(
            std::env::var("SQUEEZEFS_IPC_DD_EAGER_FLUSH")
                .ok()
                .as_deref(),
        )
    })
}

/// Pure sizing form (unit-pinned like `dd_shards_from`). `None` =
/// absent/unparseable ⇒ derived adaptive threshold.
fn dd_eager_flush_from(v: Option<&str>) -> Option<u32> {
    v.and_then(|v| v.trim().parse::<u32>().ok())
        .map(|n| n.clamp(0, RING_ENTRIES))
}

/// The issue-cadence law, third adjudication (write-wall Addendum 7,
/// 2026-08-12): the r5 sweep-only ruling was counted when the REAP
/// cycle was the wall; with the ACK-fast drain the system is
/// fabric-RTT × in-kernel-concurrency limited and issue promptness
/// PAYS again — the counted qd32 sweep peaked at K=16 (+4–8 % in two
/// independent A-B-B-A windows: 849/847 k vs off 776/782 k; 839 k vs
/// off 808 k). BUT no closed-form derivation captured it: the
/// `inflight/2` candidate was FALSIFIED in-bracket (785/799 k vs its
/// own 808 k control — at steady per-shard inflight ≈ 40 the threshold
/// exceeds the sweep size and collapses to sweep-only behavior), so per
/// the no-fixed-constants law the DEFAULT stays sweep-only and the
/// measured optimum ships only as the explicit lever. The sanctioned
/// follow-on is a `ProbeCore` cadence governor (the write-pipeline /
/// read-lane pattern): probe the threshold on delivery, adopt on
/// response — board item, not this commit. `inflight` stays a
/// parameter so the law test pins that ABSENT ignores it.
fn dd_eager_threshold(explicit: Option<u32>, governed: u32) -> u32 {
    match explicit {
        Some(0) => u32::MAX,
        Some(k) => k,
        None if governed > 0 => governed,
        None => u32::MAX,
    }
}

/// Reaper/drain FUSION arm (shim-iops campaign, 2026-08-07 — the A/B
/// lever, default ON): the owning service thread's flush pass consumes
/// its lane's completed CQEs INLINE (userspace CQ peek — zero syscall,
/// zero ctx switch) where the shipped shape paid the dedicated reaper's
/// per-batch `io_uring_enter` wake + context switch per ~1.4 CQEs (the
/// decomposition's measured 0.72 enters/op + 2.36 ctx/op at the 32×8
/// ceiling shape). The reaper stays the blocking BACKSTOP — its wait is
/// timeout-bounded (see `REAP_WAIT_TIMEOUT_NS`), which is what makes a
/// fusion-consumed wake unstrandable.
fn dd_inline_reap() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| crate::env_knobs::bool_knob("SQUEEZEFS_IPC_DD_INLINE_REAP", true))
}

/// LANE-SCOPED flush (drain-funnel campaign, 2026-08-08 r3 — the A/B
/// lever, default ON): a service thread's flush pass enters ONLY its
/// own lane's ring. The shipped flush-ALL sweep put every svc thread on
/// every shard's kernel `uring_lock` whenever that shard had unflushed
/// SQEs — the funnel profile's #1 term (31 % of svc cycles in
/// `mutex_spin_on_owner`/`osq_lock` under `io_uring_enter` at 32×32,
/// 12 threads racing 12 rings). Liveness is unchanged where it matters:
/// every production submitter IS a lane owner (svc threads pin their
/// lane; the shard partition mirrors the session→owner partition), so
/// its own sweep-end flush carries its SQEs — and any straggler on a
/// foreign lane (tests, fallback-lane submitters) is carried by that
/// lane's REAPER, whose `submit_and_wait` re-enters on its bounded
/// 100 ms EXT_ARG cadence at the latest. `0` restores flush-all.
fn dd_lane_flush() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| crate::env_knobs::bool_knob("SQUEEZEFS_IPC_DD_LANE_FLUSH", true))
}

/// COOP_TASKRUN on the dd rings (r3) as a MEASUREMENT LEVER
/// (write-wall campaign 2026-08-12): with it, a CQE submitted by a svc
/// thread posts only at that thread's NEXT ring entry — one sweep away
/// (231–375 µs pass cadence), which the post-ACK-fast `device_cq`
/// ledger reads as CQE-post deferral proportional to offered load. `0`
/// restores signal-delivered task-work (prompt posting, the pre-r3
/// posture whose mid-drain `uring_lock` cost r3 measured — the trade
/// this lever re-counts at the new operating point).
fn dd_coop_taskrun() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| crate::env_knobs::bool_knob("SQUEEZEFS_IPC_DD_COOP_TASKRUN", true))
}

thread_local! {
    /// The submitting thread's direct-drive LANE. Service threads set
    /// their owner index at loop start ([`set_service_lane`] from
    /// `IpcHost::service_loop`); a foreign thread that ever submits
    /// (tests, future callers) falls back to a dense round-robin
    /// assignment on first use — spread, never stranded.
    static DD_LANE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Foreign-thread fallback lane cursor (dense round-robin).
static FALLBACK_LANE: AtomicUsize = AtomicUsize::new(0);

/// Pin the calling thread's direct-drive lane to service-thread owner
/// index `idx` (D12 randread-shim residual, 2026-08-05): governed
/// submissions from this thread ride shard `idx % width`, so the shard
/// partition mirrors the session→owner partition — submissions never
/// cross service threads, and each shard's SQ mutex is effectively
/// uncontended on the submit side.
pub(crate) fn set_service_lane(idx: usize) {
    DD_LANE.with(|c| c.set(Some(idx)));
}

fn current_lane() -> usize {
    DD_LANE.with(|c| match c.get() {
        Some(lane) => lane,
        None => {
            let lane = FALLBACK_LANE.fetch_add(1, Ordering::Relaxed);
            c.set(Some(lane));
            lane
        }
    })
}

/// The conservative LBA the ranged window rounds to (matches the
/// handler's `get_block_range_for_index` — approved OQ #1 there).
const LBA: u64 = 4096;

struct SendPtr(*mut u8);
// SAFETY: raw pointer moved between the submitting service thread and
// the reaper; the backing (arena mapping or pooled bounce buffer) is
// kept alive by the owning fields in the same `Pending`.
unsafe impl Send for SendPtr {}

/// One in-flight direct-drive op. Holds everything the CQE needs — and
/// everything that must stay alive across the DMA: the `DataOp`'s
/// arena window + the `SlotCompletion` both pin the session mapping.
struct Pending {
    op: DataOp,
    completion: SlotCompletion,
    snap: IpcDirectSnapshot,
    /// DMA window length (LBA-rounded, ≥ the request).
    window: usize,
    /// Request offset inside the window (0 on the aligned leg).
    win_skew: usize,
    /// Bounce backing (`None` = direct arena DMA). The `Bytes` owner
    /// recycles the buffer into its home pool on drop.
    bounce: Option<(SendPtr, bytes::Bytes)>,
    /// `ipc_direct_phase_ns` `inflight` anchor: slab-insert instant
    /// (CLOCK_MONOTONIC ns — r5 single-read law; `snap.t0_ns` anchors
    /// `admit`/`total`).
    t_insert_ns: u64,
    /// `sq_wait`/`device_cq` split anchor: the instant of the
    /// `io_uring_enter` that carried this SQE, stamped at the
    /// `pending_submits` claim under the shard state lock. `0` = the
    /// racer class (published after a concurrent claim, swept into that
    /// claimer's enter) — records no `sq_wait`, full-span `device_cq`.
    t_flush_ns: u64,
}

/// One in-flight direct-drive WRITE (design-il-direct-write §3, PR-3):
/// the arena window is the DMA **source** (or its severed pooled copy on
/// the unaligned leg), and the op travels with everything the CQE
/// postlude needs — including the block's HELD `BLOCK_FLUSH_LOCKS` guard
/// (the W1 patch's protection: block `b`'s mapping mutates only under
/// it, so the probe-time resolution stays authoritative across the DMA)
/// and the allocator whose `publish_block` re-stabilizes the §5.1 word
/// on BOTH exits.
struct PendingWrite {
    /// Pins the session mapping (§5.3.1 rule 4) AND is the live DMA
    /// source on the aligned leg — a client kill-9 mid-DMA never unmaps
    /// under the kernel's read of the arena.
    op: DataOp,
    completion: SlotCompletion,
    snap: crate::fuse_client::IpcDirectWriteSnapshot,
    allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    /// Held from the probe through the postlude's purge — the extended
    /// P1-9 order is exactly the patch path's (level 3 only; nothing
    /// else is ever acquired under it here).
    block_guard: crate::sqz_sync::SqzMutexGuard<'static, ()>,
    /// Severed 4 KiB-aligned pooled source (`None` = direct arena DMA).
    /// The `Bytes` owner recycles the backing on drop.
    bounce: Option<(SendPtr, bytes::Bytes)>,
    /// The W1 inval hook (§5.6.2 — fired in the postlude, step 6).
    inval: Option<Arc<crate::ipc_service::Invalidator>>,
    /// `ipc_direct_phase_ns` `inflight` anchor (r5 single-read law).
    t_insert_ns: u64,
    /// The carrying-enter stamp (see [`Pending::t_flush_ns`]).
    t_flush_ns: u64,
}

/// Ops per guard tenure AND the park queue's capacity: ONE ring depth's
/// worth ([`fuse3::raw::Q_DEPTH_DESIRED`] — the per-queue depth ceiling),
/// so a fold/flush/handler waiter on the block's stripe parks behind at
/// most one train. Derived, never invented (the derivation-sweep law);
/// overflow past the queue capacity falls back on the honest
/// `block_lock` arm.
const TRAIN_BOUND: u32 = fuse3::raw::Q_DEPTH_DESIRED as u32;

/// One follower parked on a block's train: the op/completion travel
/// AS-IS (allocation-free steady state — the queue slot is the only
/// storage) plus the sink's invalidator for its eventual postlude or
/// handler fallback. The op's arena window stays pinned (§5.3.1 rule 4)
/// and UNREAD until the re-drive — parking severs nothing.
struct ParkedWrite {
    op: DataOp,
    completion: SlotCompletion,
    inval: Option<Arc<crate::ipc_service::Invalidator>>,
}

/// One block's conveyor state. `open` is the LANE-HOLDER authority: true
/// exactly while a dd-write leader holds the block's `BLOCK_FLUSH_LOCKS`
/// guard (armed at probe success, closed on EVERY release path), so a
/// park can never wait behind a foreign holder — the wedge-free rule is
/// "any entry in the queue while `open` is observed by the closing
/// drain or a CQE pop", serialized by the map's per-entry exclusive
/// access.
struct TrainState<P> {
    open: bool,
    /// Ops served under the current guard tenure (leader = 1); the
    /// [`TRAIN_BOUND`] cap is what keeps fold/flush/handler waiters on
    /// the stripe bounded to one train's wait.
    tenure: u32,
    /// FIFO per block (pop order == park order). Capacity is retained
    /// across tenures (steady-state parks/pops allocate nothing).
    queue: std::collections::VecDeque<P>,
}

/// The per-block follower conveyor registry (design-il-direct-write §6
/// conveyor notes; the rig's 99.8% block_lock attribution). One entry
/// per (ino, block) ever dd-write-contended — entries are never removed
/// (the `last_write_end`/Invalidator RES-13 recorded-ceiling class:
/// ~100 B + retained queue capacity per contended block, bounded by the
/// working set; the only correct sweep signal is the ino's death and a
/// lost entry would cost correctness here, not one extra notify).
struct WriteTrains<P = ParkedWrite> {
    map: scc::HashMap<(u64, u32), TrainState<P>>,
}

/// One residual-drain pop outcome (the re-park livelock fix, 2026-08-12):
/// `Rearmed` = the train re-opened under a NEW leader while the drain
/// ran — the drain must STOP (the leader's CQE pump owns the queue), and
/// on the reap thread it must stop URGENTLY: the drain runs under
/// `cq_gate`, so spinning here blocks the very CQE that would end the
/// new leader's tenure — the qd12+ field wedge (160 s stripe waits, the
/// 2.2 B park/redrive ping-pong).
enum ResidualPop<P> {
    Popped(P),
    Rearmed,
    Empty,
}

impl<P> WriteTrains<P> {
    fn new() -> Self {
        Self {
            map: scc::HashMap::new(),
        }
    }

    /// Leader arm: the calling thread HOLDS the block's guard (probe
    /// success). Opens the train and resets the tenure; a queue left
    /// over from a mid-drain re-arm keeps its entries (they simply ride
    /// the new tenure, FIFO preserved).
    fn arm(&self, key: (u64, u32)) {
        match self.map.entry_sync(key) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let t = o.get_mut();
                t.open = true;
                t.tenure = 1;
            }
            scc::hash_map::Entry::Vacant(v) => {
                let _ = v.insert_entry(TrainState {
                    open: true,
                    tenure: 1,
                    queue: std::collections::VecDeque::new(),
                });
            }
        }
    }

    /// Park a follower on `key`'s OPEN train (counted). `Err` hands the
    /// entry back: no train / closed / at the [`TRAIN_BOUND`] cap — the
    /// caller falls back on the honest `block_lock` arm.
    fn try_park(&self, key: (u64, u32), parked: P) -> Result<(), P> {
        match self.map.entry_sync(key) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let t = o.get_mut();
                if t.open && t.queue.len() < TRAIN_BOUND as usize {
                    t.queue.push_back(parked);
                    METRICS
                        .ipc_dd_write_block_parks
                        .fetch_add(1, Ordering::Relaxed);
                    Ok(())
                } else {
                    Err(parked)
                }
            }
            scc::hash_map::Entry::Vacant(_) => Err(parked),
        }
    }

    /// The CQE postlude's pop: the next follower while the tenure has
    /// room, else CLOSE the train (`None` — the caller releases the
    /// guard and drains residuals via [`Self::pop_residual`]). The close
    /// and the pop are one atomic decision under the entry's exclusive
    /// access, which is what makes a park-vs-close race impossible: a
    /// parker either lands before this (and is popped here or by the
    /// residual drain) or observes `open == false` and falls back.
    fn pop_under_tenure(&self, key: (u64, u32)) -> Option<P> {
        match self.map.entry_sync(key) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let t = o.get_mut();
                if t.tenure < TRAIN_BOUND {
                    if let Some(p) = t.queue.pop_front() {
                        t.tenure += 1;
                        METRICS
                            .ipc_dd_write_park_redrives
                            .fetch_add(1, Ordering::Relaxed);
                        return Some(p);
                    }
                }
                t.open = false;
                None
            }
            // A guard holder always armed; tolerated for symmetry.
            scc::hash_map::Entry::Vacant(_) => None,
        }
    }

    /// Close the train WITHOUT popping (the error-path releases: engine
    /// refusals, fence refusals, the reaper's leak arm). Residuals drain
    /// via [`Self::pop_residual`] after the guard drops.
    fn close(&self, key: (u64, u32)) {
        if let scc::hash_map::Entry::Occupied(mut o) = self.map.entry_sync(key) {
            o.get_mut().open = false;
        }
    }

    /// Drain one residual after a close (counted as a re-drive — it left
    /// the queue toward a disposition). Pops ONLY while the train stays
    /// CLOSED: with `open == false` no new park can land, so the drain
    /// terminates — and a concurrent RE-ARM (a residual that won a fresh
    /// try_lock) transfers queue ownership to the NEW leader's CQE pump
    /// ([`ResidualPop::Rearmed`] — the drain STOPS). The pre-fix
    /// unconditional pop is the qd12+ field wedge: a popped residual
    /// re-parked onto the re-armed train the drain was popping — a
    /// ping-pong at memory speed, on the reap thread, under `cq_gate`,
    /// blocking the very CQE that would close the new tenure (2.2 B
    /// park/redrive pairs, 160 s stripe waits).
    fn pop_residual(&self, key: (u64, u32)) -> ResidualPop<P> {
        match self.map.entry_sync(key) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let t = o.get_mut();
                if t.open {
                    return ResidualPop::Rearmed;
                }
                match t.queue.pop_front() {
                    Some(p) => {
                        METRICS
                            .ipc_dd_write_park_redrives
                            .fetch_add(1, Ordering::Relaxed);
                        ResidualPop::Popped(p)
                    }
                    None => ResidualPop::Empty,
                }
            }
            scc::hash_map::Entry::Vacant(_) => ResidualPop::Empty,
        }
    }

    /// Unconditional drain — the ring-death leak arm ONLY (no CQE pump
    /// will ever run again on that shard, so ownership transfer is
    /// meaningless there; every parked op must fail LOUD).
    fn pop_any(&self, key: (u64, u32)) -> Option<P> {
        match self.map.entry_sync(key) {
            scc::hash_map::Entry::Occupied(mut o) => {
                let p = o.get_mut().queue.pop_front();
                if p.is_some() {
                    METRICS
                        .ipc_dd_write_park_redrives
                        .fetch_add(1, Ordering::Relaxed);
                }
                p
            }
            scc::hash_map::Entry::Vacant(_) => None,
        }
    }
}

/// The in-flight slab's entry: reads and writes share the rings, the
/// slab, the reapers and the flush cadence — one engine, both directions
/// (the user directive: reuse the dd machinery in reverse, never build
/// parallel plumbing).
// Deliberately UNBOXED both ways (clippy wants the 696-byte read variant
// boxed): the slab is a pre-sized per-shard Vec whose entries recycle via
// the free list, so inline variants keep BOTH directions' submit paths
// allocation-free — a per-op Box on the read arm would put a heap
// round-trip on the 1M-IOPS lane the op-economy campaign just emptied
// (allocs/op ≈ 0, `tests/ipc_op_economy_tests.rs`). ~360 KiB/shard at
// the 512-entry ring depth is the whole cost.
#[allow(clippy::large_enum_variant)]
enum PendingOp {
    Read(Pending),
    Write(PendingWrite),
}

impl PendingOp {
    fn t_insert_ns(&self) -> u64 {
        match self {
            PendingOp::Read(p) => p.t_insert_ns,
            PendingOp::Write(p) => p.t_insert_ns,
        }
    }

    fn t_flush_ns(&self) -> u64 {
        match self {
            PendingOp::Read(p) => p.t_flush_ns,
            PendingOp::Write(p) => p.t_flush_ns,
        }
    }

    /// Stamp the carrying enter (claim-and-stamp — see
    /// [`DirectDriveEngine::claim_and_stamp`]). Records the op's
    /// `sq_wait` span with the caller's single clock read.
    fn stamp_flush(&mut self, now_ns: u64) {
        let (t_insert, trace_id, slot) = match self {
            PendingOp::Read(p) => (p.t_insert_ns, p.op.trace_id, &mut p.t_flush_ns),
            PendingOp::Write(p) => (p.t_insert_ns, p.op.trace_id, &mut p.t_flush_ns),
        };
        *slot = now_ns;
        crate::fuse_client::ipc_direct_phase_record_span(
            crate::fuse_client::IpcDirectPhase::SqWait,
            std::time::Duration::from_nanos(now_ns.saturating_sub(t_insert)),
        );
        crate::op_trace::stamp_mono(trace_id, crate::op_trace::Stage::IpcSqEnter, now_ns);
    }

    /// The op's trace id (0 = untraced) — see `DataOp::trace_id`.
    fn trace_id(&self) -> u64 {
        match self {
            PendingOp::Read(p) => p.op.trace_id,
            PendingOp::Write(p) => p.op.trace_id,
        }
    }
}

/// [`DirectDriveEngine::submit_write_core`]'s outcome: every
/// non-submitted arm hands the block guard BACK so the caller owns the
/// train disposition — the leader closes the train, the CQE postlude
/// keeps popping followers (a dropped-inside-core guard would strand
/// whatever parked behind it).
enum DdWriteSubmit {
    /// SQE in flight; the guard travels with the pending op to the CQE.
    Submitted,
    /// Completed LOUD (the RES-6 fence refusal — errno already posted).
    Refused(crate::sqz_sync::SqzMutexGuard<'static, ()>),
    /// Handler fallback (ledger class counted at the refusal site).
    Fallback {
        op: DataOp,
        completion: SlotCompletion,
        inval: Option<Arc<crate::ipc_service::Invalidator>>,
        guard: crate::sqz_sync::SqzMutexGuard<'static, ()>,
    },
}

/// One drain pass's deferred tails (ACK-fast drain, write-wall campaign
/// 2026-08-12): everything a write postlude may do AFTER its ACK is
/// collected here and runs once the batch's last ACK has posted —
/// `times` feeds ONE coalesced durable-times handoff (ino-deduped),
/// `trains` holds each op's still-held block guard for the conveyor
/// pump (follower re-drives pay SQE prep + inline TX — the inter-pop
/// stall term the `device_cq` decomposition convicted).
#[derive(Default)]
struct DrainBatch {
    times: Vec<(u64, u64, Option<usize>)>,
    trains: Vec<((u64, u32), crate::sqz_sync::SqzMutexGuard<'static, ()>)>,
}

struct EngineState {
    inflight: Vec<Option<PendingOp>>,
    free: Vec<usize>,
    inflight_count: usize,
    /// Slab idxs inserted since the last claimed enter — drained by
    /// [`DirectDriveEngine::claim_and_stamp`] (the `sq_wait`/`device_cq`
    /// split's stamp list; reused Vec, no steady-state allocation).
    staged: Vec<usize>,
}

/// A registered data volume: fixed-file index when registration
/// succeeded, raw fd otherwise.
struct VolSlot {
    fixed: u32,
    raw: RawFd,
    /// The fd opened read+write (the direct-drive WRITE lane's
    /// requirement — design-il-direct-write §3). `false` = the R/W open
    /// ladder degraded to read-only: governed reads keep working, direct
    /// writes stay on the handler path for this volume (counted in the
    /// `backend` ledger class), loudly once at spawn.
    writable: bool,
}

/// One direct-drive SHARD: its own ring, in-flight slab, and (lazily
/// spawned, NUMA-pinned) reaper thread. The pre-shard engine's "one
/// shared ring + one reaper" shape — whose module doc recorded
/// per-thread rings as the fallback "if SQ contention ever shows on a
/// profile" — was the field's 208k-IOPS-flat rand-4k il ceiling
/// (2026-08-05, D12 randread-shim residual): at 235 µs fabric RTT and
/// 256 in-flight (fio libaio 32×qd8), ONE unpinned reaper serialized
/// every CQE-side serve (~4.8 µs/op of revalidate + accounting + slot
/// completion) while the kernel FUSE path spread the same work over
/// per-CPU queues (273–280k). Shards are keyed to service-thread
/// owners (lane = owner % width), so the submit-side mutex is
/// single-writer in practice and reap work spreads over the derived
/// drain-parallelism width.
struct DdShard {
    ring: IoUring,
    /// Guards SQ pushes AND the in-flight slab (one lock, short holds,
    /// never across I/O or a wait; contended only by this shard's lane
    /// owner and its reaper).
    state: Mutex<EngineState>,
    /// CQ consumer gate (reaper/drain fusion, 2026-08-07): the CQ has
    /// exactly one consumer AT A TIME — the reaper takes it around its
    /// post-enter drain, the owning service thread's flush pass
    /// `try_lock`s it for the inline drain (never blocks; contention
    /// means the reaper is already draining). Lock order: `cq_gate`
    /// then `state` (both drain bodies take `state` per CQE) — never
    /// the reverse.
    cq_gate: Mutex<()>,
    /// Fixed-file registration outcome for THIS shard's ring.
    use_fixed: bool,
    /// SQEs pushed since the last `io_uring_enter` — the submit-batch
    /// economy (the M3/transport-commit-batch lesson, re-learned here:
    /// a per-op enter from every service thread was the measured
    /// kernel-cycle governor at t32qd32). The host's drain pass calls
    /// [`DirectDriveEngine::flush`] once per sweep; qd1 pays the same
    /// one enter per op it always did.
    pending_submits: std::sync::atomic::AtomicU32,
    /// The shard's reaper thread — spawned on the shard's FIRST
    /// governed submit (spawn-on-bind, the `ensure_service_threads`
    /// precedent: a lane that never direct-drives owns no thread).
    reaper: Mutex<Option<std::thread::JoinHandle<()>>>,
    reaper_live: AtomicBool,
    /// One-shot spawn-failure latch: refuse fast (handler fallback),
    /// log once.
    spawn_failed: AtomicBool,
    /// The reaper's pin target — the `numa_core::owner_nodes` CPU-
    /// weighted partition at this shard's index, mirroring the service
    /// threads' own partition so a lane's reaper sits where its
    /// submitter (and the session arenas it serves) live. `pin` is
    /// gated inside `crate::numa::pin_service_thread`
    /// (`SQUEEZEFS_NUMA=0` / single-node maps ⇒ no-op).
    node: Option<usize>,
}

/// The ipc-host-owned direct-drive engine: `width` = [`dd_shards`]
/// shards (rings + reapers), lane-routed by submitting service thread.
pub(crate) struct DirectDriveEngine {
    fs: Arc<SqueezefsFilesystem>,
    shards: Vec<DdShard>,
    vols: HashMap<String, VolSlot>,
    /// The per-block follower conveyor (same-block dd writes train on
    /// the guard holder instead of falling back to the handler and then
    /// blocking on the same stripe anyway).
    trains: WriteTrains,
    /// Keeps the device fds open for the engine's lifetime.
    _files: Vec<std::fs::File>,
    /// Fallback request identity (the sink's ring-op identity).
    req_uid: u32,
    req_gid: u32,
    req_pid: u32,
    shutting_down: AtomicBool,
    /// LIVE shard reapers — mirrored into the `ipc_direct_shards`
    /// gauge (the sharding engagement instrument).
    reapers_spawned: AtomicUsize,
    /// The issue-cadence governor (fourth adjudication): discovers the
    /// eager-flush threshold against live delivery — claims censused at
    /// every `claim_and_stamp`, completions at every drain, epochs
    /// rolled from the SAME clock reads those sites already pay.
    cadence: crate::write_pipeline_core::CadenceCore,
    /// Reaper waits are timeout-bounded via `IORING_ENTER_EXT_ARG`
    /// (kernel ≥ 5.11). `false` = the kernel refused EXT_ARG once —
    /// the reaper degrades to the unbounded `submit_and_wait` and the
    /// fusion inline arm DISARMS (without the bounded wait, an
    /// inline-consumed NOP wake could strand the shutdown join —
    /// negotiate-and-degrade-loudly, the kernel-feature law).
    ext_arg_ok: AtomicBool,
}

impl DirectDriveEngine {
    /// Open the data volumes (O_DIRECT with buffered fallback — the
    /// `NvmeBlockDev` worker's own posture on file-backed substrates),
    /// build the derived-width shard set (each shard registers the
    /// volumes as fixed files where the kernel allows). Reapers spawn
    /// per shard on its first governed submit. Volumes added AFTER
    /// spawn are prelude-ineligible (`ipc_direct_ineligible_backend`)
    /// — recorded residual.
    pub(crate) fn spawn(
        fs: Arc<SqueezefsFilesystem>,
        req_uid: u32,
        req_gid: u32,
        req_pid: u32,
    ) -> std::io::Result<Arc<Self>> {
        let mut paths: Vec<(String, String)> = vec![(
            "backend_0".to_string(),
            fs.router.backend_router.default_device.device_path.clone(),
        )];
        for entry in fs.router.backend_router.backends.iter() {
            paths.push((
                entry.key().clone(),
                entry.value().device.device_path.clone(),
            ));
        }

        let mut files = Vec::with_capacity(paths.len());
        let mut vols = HashMap::new();
        for (be_id, path) in paths {
            // Open ladder (WRITE lane, design-il-direct-write §3): R/W +
            // O_DIRECT → R/W buffered (tmpfs sandboxes, like the worker)
            // → the historical read-only pair (governed reads keep
            // working; direct writes degrade to the handler, loudly).
            let open = |write: bool, direct: bool| {
                let mut opts = std::fs::OpenOptions::new();
                opts.read(true);
                if write {
                    opts.write(true);
                }
                if direct {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.custom_flags(libc::O_DIRECT);
                }
                opts.open(&path)
            };
            let (file, writable) = match open(true, true).or_else(|_| open(true, false)) {
                Ok(f) => (f, true),
                Err(rw_err) => match open(false, true).or_else(|_| open(false, false)) {
                    Ok(f) => {
                        log::warn!(
                            "ipc direct-drive: data volume '{be_id}' at {path} refused a \
                             read+write open ({rw_err}) — direct WRITES stay on the \
                             handler path for this volume"
                        );
                        (f, false)
                    }
                    Err(e) => {
                        log::warn!(
                            "ipc direct-drive: cannot open data volume '{be_id}' at \
                             {path}: {e} — its blocks stay on the handler path"
                        );
                        continue;
                    }
                },
            };
            let fixed = files.len() as u32;
            vols.insert(
                be_id,
                VolSlot {
                    fixed,
                    raw: file.as_raw_fd(),
                    writable,
                },
            );
            files.push(file);
        }
        let fds: Vec<RawFd> = files.iter().map(|f| f.as_raw_fd()).collect();

        // Derived shard width + the CPU-weighted node partition (the
        // service threads' own `owner_nodes` law at this pool's width —
        // width == the service-thread ceiling by shared derivation, so
        // lane i's reaper lands on lane i's submitter's node).
        let width = dd_shards();
        let nodes = crate::numa_core::topology().owner_nodes(width);
        let mut shards = Vec::with_capacity(width);
        // COOP_TASKRUN (drain-funnel 2026-08-08 r3): without it the
        // kernel delivers each completion's task-work by TWA_SIGNAL —
        // interrupting whichever thread last touched the ring and
        // running `io_handle_tw_list` under the ring's `uring_lock` ON
        // THE SVC THREAD (the funnel profile's 9.5 % `get_signal →
        // task_work_run → __mutex_lock` term). With it, task-work runs
        // only when a task enters the ring — the reaper's own bounded
        // wait, where it belongs. Negotiate-and-degrade: pre-5.19
        // kernels refuse the flag (EINVAL) and fall back to the plain
        // setup, loudly, once.
        let mut coop_ok = dd_coop_taskrun();
        for idx in 0..width {
            let ring = if coop_ok {
                match IoUring::builder().setup_coop_taskrun().build(RING_ENTRIES) {
                    Ok(r) => r,
                    Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                        coop_ok = false;
                        log::warn!(
                            "ipc direct-drive: kernel refused IORING_SETUP_COOP_TASKRUN \
                             — dd rings degrade to signal-delivered task-work"
                        );
                        IoUring::new(RING_ENTRIES)?
                    }
                    Err(e) => return Err(e),
                }
            } else {
                IoUring::new(RING_ENTRIES)?
            };
            let use_fixed = if fds.is_empty() {
                false
            } else {
                match ring.submitter().register_files(&fds) {
                    Ok(()) => true,
                    Err(e) => {
                        log::debug!(
                            "ipc direct-drive: shard {idx} fixed-file register failed \
                             ({e}) — using raw fds"
                        );
                        false
                    }
                }
            };
            shards.push(DdShard {
                ring,
                state: Mutex::new(EngineState {
                    inflight: Vec::with_capacity(RING_ENTRIES as usize),
                    free: Vec::new(),
                    inflight_count: 0,
                    staged: Vec::with_capacity(RING_ENTRIES as usize),
                }),
                cq_gate: Mutex::new(()),
                use_fixed,
                pending_submits: std::sync::atomic::AtomicU32::new(0),
                reaper: Mutex::new(None),
                reaper_live: AtomicBool::new(false),
                spawn_failed: AtomicBool::new(false),
                node: nodes.get(idx).copied(),
            });
        }

        let engine = Arc::new(Self {
            fs,
            shards,
            vols,
            trains: WriteTrains::new(),
            _files: files,
            req_uid,
            req_gid,
            req_pid,
            shutting_down: AtomicBool::new(false),
            reapers_spawned: AtomicUsize::new(0),
            cadence: crate::write_pipeline_core::CadenceCore::new(),
            ext_arg_ok: AtomicBool::new(true),
        });
        // Fresh engine: no shard reaper is live yet (spawn-on-first-
        // submit) — the gauge reflects THIS engine from here on.
        METRICS.ipc_direct_shards.store(0, Ordering::Relaxed);
        *TEST_ENGINE
            .lock()
            .expect("test engine registry never poisons") = Some(Arc::downgrade(&engine));
        log::info!(
            "ipc direct-drive engine up: {} volume(s), {} shard(s), entries={}/shard",
            engine.vols.len(),
            engine.shards.len(),
            RING_ENTRIES
        );
        Ok(engine)
    }

    /// Guarantee shard `idx`'s reaper exists (spawn-on-first-submit).
    /// `false` = spawn failed or shutdown raced — the caller falls back
    /// to the handler path (fallback-is-correctness).
    fn ensure_reaper(self: &Arc<Self>, idx: usize) -> bool {
        let shard = &self.shards[idx];
        if shard.reaper_live.load(Ordering::Acquire) {
            return true;
        }
        if shard.spawn_failed.load(Ordering::Relaxed) {
            return false;
        }
        let mut guard = shard
            .reaper
            .lock()
            .expect("direct-drive reaper mutex never poisons");
        if shard.reaper_live.load(Ordering::Acquire) {
            return true;
        }
        // Serialized against `shutdown`'s handle take on this mutex: a
        // spawn that wins lands its handle for the join; one that loses
        // observes the flag and refuses — no leaked reaper either way.
        if self.shutting_down.load(Ordering::SeqCst) {
            return false;
        }
        let engine = Arc::clone(self);
        match std::thread::Builder::new()
            .name(squeezefs_ipc::comm_core::comm_name(&format!(
                "sqz-ipc-dd{idx}"
            )))
            .spawn(move || engine.reap_loop(idx))
        {
            Ok(handle) => {
                *guard = Some(handle);
                shard.reaper_live.store(true, Ordering::Release);
                let live = self.reapers_spawned.fetch_add(1, Ordering::AcqRel) + 1;
                METRICS
                    .ipc_direct_shards
                    .store(live as u64, Ordering::Relaxed);
                true
            }
            Err(e) => {
                shard.spawn_failed.store(true, Ordering::Relaxed);
                log::warn!(
                    "ipc direct-drive: shard {idx} reaper failed to spawn ({e}) — \
                     this lane's governed reads stay on the handler path"
                );
                false
            }
        }
    }

    /// Submit one prelude-eligible op on the calling thread's LANE
    /// shard. `Err` returns the op for the handler fallback (unknown/
    /// unhealthy volume, SQ full, reaper spawn failure) — counted in
    /// the `backend` ledger class by the caller's contract here.
    pub(crate) fn submit(
        self: &Arc<Self>,
        op: DataOp,
        completion: SlotCompletion,
        snap: IpcDirectSnapshot,
    ) -> Result<(), (DataOp, SlotCompletion)> {
        let router = &self.fs.router;
        // Parse-carry (r5): the probe resolved `(be_id, dev_off)` once;
        // the per-op re-parse (the StrSearcher/TwoWay svc term) is gone.
        let (be_id, dev_off) = (snap.be_id.as_str(), snap.dev_off);
        let Some(vol) = self.vols.get(be_id) else {
            METRICS
                .ipc_direct_ineligible_backend
                .fetch_add(1, Ordering::Relaxed);
            return Err((op, completion));
        };
        if !router.backend_router.is_backend_healthy(be_id) {
            METRICS
                .ipc_direct_ineligible_backend
                .fetch_add(1, Ordering::Relaxed);
            return Err((op, completion));
        }

        // Window geometry (the handler's ranged arithmetic verbatim):
        // `[floor(rel), ceil(rel_end))` capped at the arm's window —
        // the block on the striped arm, the tenant's slot on the packed
        // arm (PK8: `dev_off` is then the tenant's device base, so the
        // grain-rounded tail can never reach a neighbouring tenant).
        let block_size = router.block_size.load(Ordering::Relaxed);
        let rel = snap.offset - u64::from(snap.block) * block_size;
        let rel_end = rel + u64::from(snap.len);
        let aligned_start = rel & !(LBA - 1);
        let aligned_end = std::cmp::min(rel_end.div_ceil(LBA) * LBA, snap.window_cap);
        let window = (aligned_end - aligned_start) as usize;
        let win_skew = (rel - aligned_start) as usize;
        let req_len = snap.len as usize;
        let packed = snap.packed_file_id.is_some();

        // Arena-direct DMA only when the window IS the request and the
        // arena destination can take O_DIRECT-class DMA (4 KiB-aligned).
        let arena_ptr = op.payload.as_base_ptr();
        let direct = window == req_len && (arena_ptr as usize) % (LBA as usize) == 0;
        let (dest_ptr, bounce) = if direct {
            (arena_ptr, None)
        } else {
            METRICS
                .ipc_direct_drive_bounces
                .fetch_add(1, Ordering::Relaxed);
            let (bptr, bbytes) = crate::cache::pool::read_bounce_pool(window).alloc();
            (bptr, Some((SendPtr(bptr), bbytes)))
        };

        // Governed-read accounting (the amplification bounds + hybrid
        // observability stay intact — engagement instruments must not
        // go dark because the handler was deleted from this path).
        METRICS
            .ipc_direct_drive_submits
            .fetch_add(1, Ordering::Relaxed);
        if packed {
            // The funnel's own engagement pair (design-small-file-packing
            // §5.10 — `packed_read_bytes ≤ Σ ceil(image)` is the packed
            // amplification bound) for the read the handler would have
            // issued through `read_mapping_window`.
            METRICS.packed_reads.fetch_add(1, Ordering::Relaxed);
            METRICS
                .packed_read_bytes
                .fetch_add(window as u64, Ordering::Relaxed);
        } else {
            METRICS.ranged_reads.fetch_add(1, Ordering::Relaxed);
            METRICS
                .ranged_read_bytes
                .fetch_add(window as u64, Ordering::Relaxed);
            if window != req_len {
                METRICS
                    .ranged_read_unaligned_bounces
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        // Posture parity (DIALED P1.5): `read_device_true_reads` is the
        // ddt escape's family — the handler counts it only under
        // `device_true`, so a default-mount governor-denied direct-drive
        // must not inflate it (the amplification-methodology ruler).
        if router.direct_device_true() {
            METRICS
                .read_device_true_reads
                .fetch_add(1, Ordering::Relaxed);
        }
        METRICS
            .read_odirect_requests
            .fetch_add(1, Ordering::Relaxed);
        router
            .cache
            .admission_governor
            .note_foreground(window as u64);

        let dev_read_off = dev_off + aligned_start;
        // ipc_direct_phase_ns: `admit` closes here (probe entry →
        // slab insert); the SAME ns read anchors `inflight` — one
        // clock read for both (r5 single-read law).
        let t_insert_ns = crate::mono_core::monotonic_ns_u64();
        crate::fuse_client::ipc_direct_phase_record_span(
            crate::fuse_client::IpcDirectPhase::Admit,
            std::time::Duration::from_nanos(t_insert_ns.saturating_sub(snap.t0_ns)),
        );
        crate::op_trace::stamp_mono(
            op.trace_id,
            crate::op_trace::Stage::IpcAdmitted,
            t_insert_ns,
        );
        let pending = Pending {
            op,
            completion,
            snap,
            window,
            win_skew,
            bounce,
            t_insert_ns,
            t_flush_ns: 0,
        };

        // Lane routing: this thread's shard (service threads pin their
        // owner index; the shard partition mirrors the session→owner
        // partition). The reaper must be live BEFORE the SQE publishes.
        let lane = current_lane() % self.shards.len();
        if !self.ensure_reaper(lane) {
            METRICS
                .ipc_direct_ineligible_backend
                .fetch_add(1, Ordering::Relaxed);
            // Un-count the submit that will not happen.
            METRICS
                .ipc_direct_drive_submits
                .fetch_sub(1, Ordering::Relaxed);
            let Pending { op, completion, .. } = pending;
            return Err((op, completion));
        }
        let shard = &self.shards[lane];

        // Slab insert + SQE push under the ONE shard mutex.
        {
            let mut st = shard
                .state
                .lock()
                .expect("direct-drive state mutex never poisons");
            let idx = match st.free.pop() {
                Some(i) => i,
                None => {
                    st.inflight.push(None);
                    st.inflight.len() - 1
                }
            };
            let sqe = if shard.use_fixed {
                opcode::Read::new(types::Fixed(vol.fixed), dest_ptr, pending.window as u32)
                    .offset(dev_read_off)
                    .build()
                    .user_data(idx as u64)
            } else {
                opcode::Read::new(types::Fd(vol.raw), dest_ptr, pending.window as u32)
                    .offset(dev_read_off)
                    .build()
                    .user_data(idx as u64)
            };
            // SAFETY: SQ access is exclusive under `state`'s mutex (the
            // reaper never touches the SQ; §module docs).
            let mut sq = unsafe { shard.ring.submission_shared() };
            let mut pushed = unsafe { sq.push(&sqe).is_ok() };
            if !pushed {
                // SQ full: flush what's queued, retry once, else refuse
                // (client backpressure via the handler path).
                sq.sync();
                drop(sq);
                // This enter carries the previously staged SQEs — stamp
                // them (we hold the state lock; the current op is not
                // yet staged, its push failed).
                Self::stamp_staged_locked(&mut st);
                let _ = shard.ring.submit();
                // SAFETY: as above — still under the mutex.
                let mut sq = unsafe { shard.ring.submission_shared() };
                pushed = unsafe { sq.push(&sqe).is_ok() };
                sq.sync();
            } else {
                sq.sync();
            }
            if !pushed {
                st.free.push(idx);
                METRICS
                    .ipc_direct_ineligible_backend
                    .fetch_add(1, Ordering::Relaxed);
                // Un-count the submit that will not happen.
                METRICS
                    .ipc_direct_drive_submits
                    .fetch_sub(1, Ordering::Relaxed);
                let Pending { op, completion, .. } = pending;
                return Err((op, completion));
            }
            st.inflight[idx] = Some(PendingOp::Read(pending));
            st.inflight_count += 1;
            st.staged.push(idx);
        }
        // Issue cadence (BDP-derived — see `dd_eager_threshold`).
        let pending = shard.pending_submits.fetch_add(1, Ordering::Release) + 1;
        if pending >= dd_eager_threshold(dd_eager_flush(), self.cadence.k() as u32) {
            self.flush_shard(lane, shard);
        }
        Ok(())
    }

    /// The engine's synthetic request identity for CQE-side handler
    /// fallbacks (the sink's `ring_request` shape — authorization
    /// already happened at the §5.2 fd screen).
    fn ring_request(&self) -> fuse3::raw::Request {
        fuse3::raw::Request {
            unique: 0,
            uid: self.req_uid,
            gid: self.req_gid,
            pid: self.req_pid,
            // Ring-origin op: no kernel delivery, no reply slot.
            slot: fuse3::raw::ReplySlot::Classical,
        }
    }

    /// Park one probe-`BlockLock`-refused WRITE on its block's OPEN
    /// train (the per-block follower conveyor). `Err` hands the op back
    /// for the honest `block_lock` fallback: the guard holder is NOT the
    /// dd-write lane (no train armed — a handler/fold/flush/truncate has
    /// the stripe), the train just closed, or the queue is at its
    /// [`TRAIN_BOUND`] cap. The op's shape screens already passed (the
    /// probe runs them before its try_lock); the state screens run at
    /// re-drive time under the then-held guard.
    pub(crate) fn try_park_write(
        &self,
        op: DataOp,
        completion: SlotCompletion,
        inval: Option<Arc<crate::ipc_service::Invalidator>>,
    ) -> Result<(), (DataOp, SlotCompletion)> {
        let block_size = self.fs.router.block_size.load(Ordering::Relaxed);
        if block_size == 0 {
            return Err((op, completion));
        }
        let key = (op.binding.ino, (op.desc.offset / block_size) as u32);
        self.trains
            .try_park(
                key,
                ParkedWrite {
                    op,
                    completion,
                    inval,
                },
            )
            .map_err(|ParkedWrite { op, completion, .. }| (op, completion))
    }

    /// Submit one probe-eligible WRITE on the calling thread's LANE
    /// shard (design-il-direct-write §3, PR-3): the arena is the DMA
    /// SOURCE (or its severed pooled copy when the window cannot take
    /// O_DIRECT-class DMA), the §5.1 sole-owner fence and the RES-6
    /// authorization door run strictly BEFORE the SQE, and the CQE
    /// completes the ring slot after the patch postlude
    /// ([`Self::finish_write`]).
    ///
    /// This is the LEADER entry: it ARMS the block's train (the
    /// lane-holder authority parks same-block followers behind this
    /// guard tenure) and CLOSES it on every non-submitted outcome, so a
    /// parked op can never outlive the guard it waits on.
    ///
    /// Outcomes:
    /// * `Ok(())` — consumed: submitted, **or fence-refused LOUD**
    ///   (errno completed to the client; a fenced holder must never
    ///   fall back to a SECOND submission path — the patch path's law).
    /// * `Err((op, completion))` — handler fallback (unknown/unhealthy/
    ///   read-only volume, reaper spawn failure, SQ full, clone-pinned
    ///   block), its ledger class counted at the refusal site; the §5.1
    ///   word is re-stabilized where the fence had already been taken,
    ///   so the handler path re-runs the whole patch protocol on
    ///   unchanged content.
    pub(crate) fn submit_write(
        self: &Arc<Self>,
        op: DataOp,
        completion: SlotCompletion,
        snap: crate::fuse_client::IpcDirectWriteSnapshot,
        block_guard: crate::sqz_sync::SqzMutexGuard<'static, ()>,
        inval: Option<Arc<crate::ipc_service::Invalidator>>,
    ) -> Result<(), (DataOp, SlotCompletion)> {
        let key = (snap.ino, snap.block);
        self.trains.arm(key);
        match self.submit_write_core(op, completion, snap, block_guard, inval) {
            DdWriteSubmit::Submitted => Ok(()),
            DdWriteSubmit::Refused(guard) => {
                self.close_train_error(key, guard);
                Ok(())
            }
            DdWriteSubmit::Fallback {
                op,
                completion,
                inval: _,
                guard,
            } => {
                self.close_train_error(key, guard);
                Err((op, completion))
            }
        }
    }

    /// Close `key`'s train on a non-CQE guard release (engine refusals,
    /// fence refusals): residuals parked in the arm→refusal window go
    /// STRAIGHT to the handler — the engine just proved this block's
    /// submit path refusing, so a re-probe would re-hit it (their pops
    /// count `park_redrives`; the closure law holds).
    fn close_train_error(
        self: &Arc<Self>,
        key: (u64, u32),
        guard: crate::sqz_sync::SqzMutexGuard<'static, ()>,
    ) {
        self.trains.close(key);
        drop(guard);
        while let ResidualPop::Popped(parked) = self.trains.pop_residual(key) {
            crate::ipc_service::spawn_write_handoff(
                Arc::clone(&self.fs),
                self.ring_request(),
                parked.inval,
                parked.op,
                parked.completion,
                false,
            );
        }
    }

    /// The submission interior, shared by the leader path
    /// ([`Self::submit_write`]) and the conveyor's under-guard follower
    /// re-drive ([`Self::run_train`]): every non-submitted outcome hands
    /// the GUARD BACK so the caller owns the train disposition (the
    /// leader closes; the postlude keeps popping).
    fn submit_write_core(
        self: &Arc<Self>,
        op: DataOp,
        completion: SlotCompletion,
        snap: crate::fuse_client::IpcDirectWriteSnapshot,
        block_guard: crate::sqz_sync::SqzMutexGuard<'static, ()>,
        inval: Option<Arc<crate::ipc_service::Invalidator>>,
    ) -> DdWriteSubmit {
        let router = &self.fs.router;
        let backend_refuse = |op, completion, inval, guard| {
            METRICS
                .ipc_dd_write_ineligible_backend
                .fetch_add(1, Ordering::Relaxed);
            DdWriteSubmit::Fallback {
                op,
                completion,
                inval,
                guard,
            }
        };
        let Some(vol) = self.vols.get(snap.be_id.as_str()) else {
            return backend_refuse(op, completion, inval, block_guard);
        };
        if !vol.writable
            || !router
                .backend_router
                .is_backend_healthy(snap.be_id.as_str())
        {
            return backend_refuse(op, completion, inval, block_guard);
        }
        let Ok((allocator, device)) = router.backend_router.get_backend(snap.be_id.as_str()) else {
            return backend_refuse(op, completion, inval, block_guard);
        };
        // The reaper must be live BEFORE the SQE publishes.
        let lane = current_lane() % self.shards.len();
        if !self.ensure_reaper(lane) {
            return backend_refuse(op, completion, inval, block_guard);
        }

        // DMA source: the arena window verbatim when it can take
        // O_DIRECT-class DMA (4 KiB-aligned — the read leg's own
        // screen), else the §5.5.2 sever lands in a 4 KiB-aligned
        // pooled buffer (the patch path's pooled vehicle: same ONE
        // copy the handoff path pays, none of its handler descent). A
        // racing client scribble on the live-arena leg is torn CONTENT
        // on the device — the client's own POSIX concurrent-buffer
        // hazard (§5.3.1 rule 2's severed-copy discipline exists for
        // DERIVED values; the payload is uninterpreted bytes DMA'd
        // exactly once).
        let len = snap.len as usize;
        let arena_ptr = op.payload.as_base_ptr();
        let (src_ptr, bounce) = if (arena_ptr as usize) % (LBA as usize) == 0 {
            (arena_ptr, None)
        } else {
            let mut buf = crate::cache::pool::BUFFER_POOL.alloc();
            if len > buf.capacity() {
                buf.resize(len, 0);
            }
            // SAFETY: the dequeued op's validated arena window is alive
            // for this call (op is held); destination capacity ensured
            // above; a racing client write yields torn content, never UB.
            unsafe {
                std::ptr::copy_nonoverlapping(arena_ptr, buf.backing_mut().as_mut_ptr(), len);
            }
            buf.set_written_len(len);
            // Locality instrument: ONE CPU pass over arena bytes.
            crate::numa::count_current_pass(op.payload.arena_node(), len);
            let bytes = buf.into_bytes();
            let p = bytes.as_ptr() as *mut u8;
            (p, Some((SendPtr(p), bytes)))
        };

        // §5.1 steps 1a+1b — mark-unstable → fence(SeqCst) → refcount
        // re-check. A clone pinned the block ⇒ re-stabilize (content
        // never changed) and fall back to the handler's CoW arm — the
        // ledger's one post-probe arm, attributed apart from the prelude
        // custody classes (per-cause split, 2026-08-11).
        if !allocator.begin_patch_sole_owner(snap.dev_off) {
            allocator.publish_block(snap.dev_off);
            METRICS
                .ipc_dd_write_ineligible_fence_backoff
                .fetch_add(1, Ordering::Relaxed);
            return DdWriteSubmit::Fallback {
                op,
                completion,
                inval,
                guard: block_guard,
            };
        }
        // RES-6/S7: THE authorization door, strictly before the SQE (the
        // same gate `write_block`'s worker and the patch's D14 leg run).
        // A fence refusal fails the op LOUD through the patch path's
        // common re-stabilize/purge exit — never a fallback: a fenced
        // holder must not fall back to a second submission path.
        if let Err(e) = device.authorize_zc_store() {
            allocator.publish_block(snap.dev_off);
            router.cache.purge_block_key(snap.key.as_str());
            let file_path = crate::keys::inode_path_stack(snap.ino);
            router.cache.write_lru.remove(file_path.as_str());
            router.cache.read_lru.remove(file_path.as_str());
            METRICS
                .ipc_dd_write_fence_refusals
                .fetch_add(1, Ordering::Relaxed);
            log::error!(
                "ipc direct-drive: WRITE refused by the data-plane fence for ino {} \
                 block {} ({e}) — failed loud to the client (RES-6)",
                snap.ino,
                snap.block
            );
            completion.complete(-i64::from(e.to_errno()));
            return DdWriteSubmit::Refused(block_guard);
        }

        // ipc_direct_phase_ns `admit` closes at slab insert; the SAME
        // ns read anchors `inflight` (r5 single-read law).
        let t_insert_ns = crate::mono_core::monotonic_ns_u64();
        crate::fuse_client::ipc_direct_phase_record_span(
            crate::fuse_client::IpcDirectPhase::Admit,
            std::time::Duration::from_nanos(t_insert_ns.saturating_sub(snap.t0_ns)),
        );
        crate::op_trace::stamp_mono(
            op.trace_id,
            crate::op_trace::Stage::IpcAdmitted,
            t_insert_ns,
        );
        let dev_write_off = snap.dev_write_off;
        let dev_off = snap.dev_off;
        let pending = PendingWrite {
            op,
            completion,
            snap,
            allocator,
            block_guard,
            bounce,
            inval,
            t_insert_ns,
            t_flush_ns: 0,
        };
        let shard = &self.shards[lane];

        // Slab insert + WRITE SQE push under the ONE shard mutex (the
        // read submit's discipline verbatim, direction reversed).
        {
            let mut st = shard
                .state
                .lock()
                .expect("direct-drive state mutex never poisons");
            let idx = match st.free.pop() {
                Some(i) => i,
                None => {
                    st.inflight.push(None);
                    st.inflight.len() - 1
                }
            };
            let sqe = if shard.use_fixed {
                opcode::Write::new(types::Fixed(vol.fixed), src_ptr, len as u32)
                    .offset(dev_write_off)
                    .build()
                    .user_data(idx as u64)
            } else {
                opcode::Write::new(types::Fd(vol.raw), src_ptr, len as u32)
                    .offset(dev_write_off)
                    .build()
                    .user_data(idx as u64)
            };
            // SAFETY: SQ access is exclusive under `state`'s mutex (the
            // reaper never touches the SQ; §module docs).
            let mut sq = unsafe { shard.ring.submission_shared() };
            let mut pushed = unsafe { sq.push(&sqe).is_ok() };
            if !pushed {
                // SQ full: flush what's queued, retry once, else refuse
                // (client backpressure via the handler path).
                sq.sync();
                drop(sq);
                // This enter carries the previously staged SQEs — stamp
                // them (we hold the state lock; the current op is not
                // yet staged, its push failed).
                Self::stamp_staged_locked(&mut st);
                let _ = shard.ring.submit();
                // SAFETY: as above — still under the mutex.
                let mut sq = unsafe { shard.ring.submission_shared() };
                pushed = unsafe { sq.push(&sqe).is_ok() };
                sq.sync();
            } else {
                sq.sync();
            }
            if !pushed {
                st.free.push(idx);
                drop(st);
                // Nothing DMA'd: re-stabilize the §5.1 word (content
                // unchanged) and let the handler re-run the protocol.
                let PendingWrite {
                    op,
                    completion,
                    allocator,
                    block_guard,
                    inval,
                    ..
                } = pending;
                allocator.publish_block(dev_off);
                return backend_refuse(op, completion, inval, block_guard);
            }
            st.inflight[idx] = Some(PendingOp::Write(pending));
            st.inflight_count += 1;
            st.staged.push(idx);
        }
        // Issue cadence (BDP-derived — see `dd_eager_threshold`).
        let pending_count = shard.pending_submits.fetch_add(1, Ordering::Release) + 1;
        if pending_count >= dd_eager_threshold(dd_eager_flush(), self.cadence.k() as u32) {
            self.flush_shard(lane, shard);
        }
        DdWriteSubmit::Submitted
    }

    /// Flush pushed-but-unsubmitted SQEs — one `io_uring_enter` per
    /// shard with published work, per drain sweep. Cheap when idle
    /// (one atomic load per shard, width ≤ 16 derived / 64 clamped);
    /// concurrent flushes are harmless (the kernel consumes the
    /// published tail). Flushing ALL shards (not just the caller's
    /// lane) keeps the liveness rule thread-pairing-independent.
    ///
    /// Fusion (2026-08-07): after the submit sweep, the calling
    /// service thread opportunistically drains ITS OWN lane's CQ
    /// (userspace peek — zero syscall; `try_lock` so the reaper's
    /// drain is never waited on, other lanes' CQs never touched:
    /// cross-lane inline reaping would put every svc thread on every
    /// CQ gate). Disabled while shutting down — teardown CQEs belong
    /// to the reaper's drain contract.
    ///
    /// LANE SCOPE (drain-funnel 2026-08-08 r3, default ON): the enter
    /// sweep covers ONLY the calling thread's lane — the flush-ALL
    /// posture serialized every svc thread on every shard's kernel
    /// `uring_lock` (the funnel profile's 31 % spin term). See
    /// [`dd_lane_flush`] for the liveness argument; `0` restores the
    /// thread-pairing-independent sweep.
    pub(crate) fn flush(self: &Arc<Self>) {
        if dd_lane_flush() {
            let lane = current_lane() % self.shards.len();
            self.flush_shard(lane, &self.shards[lane]);
        } else {
            for (lane, shard) in self.shards.iter().enumerate() {
                self.flush_shard(lane, shard);
            }
        }
        if dd_inline_reap()
            && self.ext_arg_ok.load(Ordering::Relaxed)
            && !self.shutting_down.load(Ordering::SeqCst)
        {
            let lane = current_lane() % self.shards.len();
            let shard = &self.shards[lane];
            // No reaper yet ⇒ nothing ever submitted on this lane.
            if shard.reaper_live.load(Ordering::Acquire) {
                if let Ok(_gate) = shard.cq_gate.try_lock() {
                    let served = self.drain_cq_locked(lane);
                    if served > 0 {
                        METRICS
                            .ipc_direct_inline_reaps
                            .fetch_add(served as u64, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// One shard's enter (shared by the per-sweep [`Self::flush`] and
    /// the eager mid-sweep arm in [`Self::submit`]): swap-claims the
    /// pending count, so concurrent callers never double-enter for the
    /// same published tail (and a lost race is harmless — the other
    /// caller's enter carried the SQEs).
    /// Stamp every staged-and-unstamped op with `now_ns` as its carrying
    /// enter (records `sq_wait`; the `t_flush_ns != 0` screen makes a
    /// ghost/reused slab idx harmless — a slot is stamped at most once
    /// per op). Caller holds the shard state lock and owes an enter.
    fn stamp_staged_locked(st: &mut EngineState) -> Option<u64> {
        if st.staged.is_empty() {
            return None;
        }
        let now_ns = crate::mono_core::monotonic_ns_u64();
        while let Some(idx) = st.staged.pop() {
            if let Some(p) = st.inflight.get_mut(idx).and_then(Option::as_mut) {
                if p.t_flush_ns() == 0 {
                    p.stamp_flush(now_ns);
                }
            }
        }
        Some(now_ns)
    }

    /// Swap-claim the shard's published tail AND stamp the carried ops
    /// (`sq_wait`/`device_cq` split, write-wall campaign 2026-08-11).
    /// Returns whether the caller owes an enter. Cost: one short state
    /// hold + ONE clock read per claimed enter — never per op (r5).
    fn claim_and_stamp(&self, shard: &DdShard) -> bool {
        let claimed = shard.pending_submits.swap(0, Ordering::AcqRel);
        if claimed == 0 {
            return false;
        }
        // Cadence census: this claim's size is the sweep-size signal
        // (the governor's probe entry point); the roll rides the clock
        // read the stamp pass pays anyway (r5 economy).
        self.cadence.on_claim(u64::from(claimed));
        let mut st = shard
            .state
            .lock()
            .expect("direct-drive state mutex never poisons");
        let now_ns = Self::stamp_staged_locked(&mut st);
        drop(st);
        if let Some(ns) = now_ns {
            if self.cadence.roll(ns / 1_000_000, u64::from(RING_ENTRIES)) {
                METRICS
                    .ipc_dd_cadence_k
                    .store(self.cadence.k(), Ordering::Relaxed);
                METRICS
                    .ipc_dd_cadence_probe_ups
                    .store(self.cadence.probe_ups(), Ordering::Relaxed);
                METRICS
                    .ipc_dd_cadence_probe_backoffs
                    .store(self.cadence.probe_backoffs(), Ordering::Relaxed);
            }
        }
        true
    }

    fn flush_shard(&self, lane: usize, shard: &DdShard) {
        if !self.claim_and_stamp(shard) {
            return;
        }
        for attempt in 0..3 {
            match shard.ring.submit() {
                Ok(_) => break,
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::EBUSY)
                    ) && attempt < 2 =>
                {
                    std::hint::spin_loop();
                }
                Err(e) => {
                    // The SQEs are published; the reaper's own enter
                    // carries them. Loud because it should not happen.
                    log::error!("ipc direct-drive: shard {lane} io_uring submit failed: {e}");
                    break;
                }
            }
        }
    }

    /// Shard `idx`'s single CQ consumer: block in `io_uring_enter(
    /// GETEVENTS, min_complete=1)`, complete slots straight from CQEs.
    /// Exits only when shutdown is flagged AND the shard's in-flight
    /// set is drained (every pending op pins its session mapping until
    /// here — §5.3.1 rule 4). Pinned to the shard's partition node
    /// (gated inside `pin_service_thread`: `SQUEEZEFS_NUMA=0` /
    /// single-node maps ⇒ no-op) so the CQE-side revalidate + serve
    /// run where the lane's submitter and session arenas live.
    fn reap_loop(self: Arc<Self>, idx: usize) {
        let shard = &self.shards[idx];
        if let Some(node) = shard.node {
            crate::numa::pin_service_thread(node);
        }
        // Conveyor lane pin: follower submissions from this reaper's
        // CQE postludes ride ITS OWN shard, so a train's next hop is
        // reaped right here (no cross-shard reaper spawn per train).
        set_service_lane(idx);
        // MEM-7c escalation bound: consecutive failed enters while ops are
        // in flight. A permanently broken ring must neither unmap under DMA
        // nor hang `shutdown`'s join forever, so past this many 1 ms
        // retries the pending ops are LEAKED deliberately (their DMA
        // destinations stay mapped for the process's life) and the reaper
        // exits loud.
        const REAP_STALL_LIMIT: u32 = 5_000;
        let mut consecutive_stalls = 0u32;
        loop {
            {
                let st = shard
                    .state
                    .lock()
                    .expect("direct-drive state mutex never poisons");
                if self.shutting_down.load(Ordering::SeqCst) && st.inflight_count == 0 {
                    return;
                }
            }
            // The reaper's enter carries any published-but-unclaimed
            // SQEs (its `submit_and_wait` submits unconditionally) —
            // claim-and-stamp them so their `sq_wait` closes at the
            // enter that actually carried them.
            self.claim_and_stamp(shard);
            match self.reaper_wait(shard) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) => {
                    // MEM-7c: this arm used to `return` on ANY enter error
                    // while shutting down — including with ops still in
                    // flight. Every pending op PINS its session mapping
                    // (§5.3.1 rule 4), so returning here lets `shutdown`
                    // join, the drive drop, and the mapping release while
                    // kernel DMA can still land in it. The loop's own exit
                    // condition (shutdown AND `inflight_count == 0`) is the
                    // only legal one: keep retrying, loudly, and count the
                    // stall. A permanently broken ring degrades to a
                    // deliberately LEAKED mapping (below), never to an
                    // unmap under DMA.
                    let inflight = {
                        let st = shard
                            .state
                            .lock()
                            .expect("direct-drive state mutex never poisons");
                        st.inflight_count
                    };
                    if self.shutting_down.load(Ordering::SeqCst) && inflight == 0 {
                        return;
                    }
                    METRICS
                        .ipc_direct_reap_stalls
                        .fetch_add(1, Ordering::Relaxed);
                    consecutive_stalls += 1;
                    log::error!(
                        "ipc direct-drive: shard {idx} reaper enter failed: {e} \
                         ({inflight} op(s) still in flight — their DMA destinations \
                         stay pinned, stall {consecutive_stalls}/{REAP_STALL_LIMIT})"
                    );
                    if consecutive_stalls >= REAP_STALL_LIMIT {
                        // Leak the pins rather than unmap under DMA, and
                        // rather than hang the shutdown join forever.
                        let leaked = {
                            let mut st = shard
                                .state
                                .lock()
                                .expect("direct-drive state mutex never poisons");
                            let mut n = 0usize;
                            let mut wedged_trains: Vec<(u64, u32)> = Vec::new();
                            for slot in st.inflight.iter_mut() {
                                if let Some(p) = slot.take() {
                                    // A forgotten WRITE also leaks its held
                                    // BLOCK_FLUSH_LOCKS guard — that block's
                                    // stripe wedges, which is strictly better
                                    // than a mapping (DMA source) or word
                                    // (§5.1) whose owner the kernel may still
                                    // touch; this arm is the unrecoverable-
                                    // ring terminal state either way. Its
                                    // TRAIN, however, must not strand parked
                                    // followers silently: they have NO DMA in
                                    // flight, so they fail LOUD below.
                                    if let PendingOp::Write(w) = &p {
                                        wedged_trains.push((w.snap.ino, w.snap.block));
                                    }
                                    std::mem::forget(p);
                                    n += 1;
                                }
                            }
                            st.inflight_count = 0;
                            drop(st);
                            for key in wedged_trains {
                                self.trains.close(key);
                                while let Some(parked) = self.trains.pop_any(key) {
                                    log::error!(
                                        "ipc direct-drive: failing parked follower on \
                                         ino {} block {} LOUD (EIO) — its train's guard \
                                         holder was leaked with the unrecoverable ring \
                                         (never silence)",
                                        key.0,
                                        key.1
                                    );
                                    parked.completion.complete(-i64::from(libc::EIO));
                                }
                            }
                            n
                        };
                        log::error!(
                            "ipc direct-drive: shard {idx} ring unrecoverable after \
                             {REAP_STALL_LIMIT} failed enters — LEAKING {leaked} \
                             in-flight destination(s) (their pages stay mapped \
                             for the process's life; unmapping under kernel DMA \
                             is not an option) and exiting the reaper"
                        );
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            consecutive_stalls = 0;
            // The reaper is one of exactly two CQ consumers (fusion,
            // 2026-08-07); the gate serializes them. A blocking lock is
            // fine here — the inline side only ever `try_lock`s, so the
            // reaper waits at most one inline drain.
            let gate = shard
                .cq_gate
                .lock()
                .expect("direct-drive cq gate never poisons");
            self.drain_cq_locked(idx);
            drop(gate);
        }
    }

    /// The reaper's blocking enter: `submit_and_wait(1)` bounded by
    /// `IORING_ENTER_EXT_ARG` timeout where the kernel supports it
    /// (≥ 5.11). The bound is a LIVENESS re-check cadence, not tuning
    /// (the SERVICE_PARK_MAX posture): with the fusion inline arm
    /// consuming CQEs — including, in one shutdown race, the NOP wake —
    /// the reaper must re-check `shutting_down` on its own clock or the
    /// join can strand. 100 ms bounds that race's shutdown latency;
    /// idle cost is 10 wakes/s/shard. A timeout expiry returns `Ok(0)`.
    fn reaper_wait(&self, shard: &DdShard) -> std::io::Result<usize> {
        if self.ext_arg_ok.load(Ordering::Relaxed) {
            const REAP_WAIT_TIMEOUT: types::Timespec = types::Timespec::new().nsec(100_000_000);
            let args = types::SubmitArgs::new().timespec(&REAP_WAIT_TIMEOUT);
            match shard.ring.submitter().submit_with_args(1, &args) {
                Ok(n) => return Ok(n),
                // Timeout expiry: nothing completed — a normal bounded
                // wake (the liveness re-check), never an error.
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => return Ok(0),
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                    // Pre-5.11 kernel: no EXT_ARG. Degrade loudly ONCE —
                    // unbounded waits + the fusion arm disarmed (see
                    // `ext_arg_ok`).
                    self.ext_arg_ok.store(false, Ordering::Relaxed);
                    log::warn!(
                        "ipc direct-drive: kernel refused IORING_ENTER_EXT_ARG — \
                         reaper waits are unbounded and the inline-reap fusion \
                         arm is DISARMED on this kernel"
                    );
                }
                Err(e) => return Err(e),
            }
        }
        shard.ring.submitter().submit_and_wait(1)
    }

    /// Drain THIS shard's CQ and complete every popped op — the ONE
    /// CQE-consumption body, shared by the reaper (post-enter) and the
    /// fusion inline arm ([`Self::flush`]). Caller MUST hold the
    /// shard's `cq_gate` (the single-consumer-at-a-time invariant the
    /// pre-fusion design got from the dedicated reaper thread).
    /// Returns ops completed (NOP wakes excluded).
    fn drain_cq_locked(self: &Arc<Self>, idx: usize) -> usize {
        // Conveyor-rail determinism seam: leave the CQ untouched while a
        // test pins guard tenures open (the reaper's bounded wait resumes
        // consumption when the gate reopens; nothing is lost).
        if TEST_DDW_CQE_HOLD.load(Ordering::Relaxed) {
            return 0;
        }
        // ACK-fast drain (write-wall campaign 2026-08-12): the batch
        // context. Per-op postludes keep every pre-ACK law inline
        // (publish_block, purges, LRU drops, attr publish, inval, ACK);
        // the HEAVY tails — the durable-times handoff (one dispatch per
        // batch, inos deduped) and the train pump (follower re-drives:
        // SQE prep + inline nvme-tcp TX) — defer past the LAST ACK, so
        // a CQE popped mid-batch never waits behind a sibling's tail.
        // Two small per-BATCH allocations (~40 ops amortize them; the
        // op-economy law governs per-OP allocs).
        let mut batch = DrainBatch::default();
        let shard = &self.shards[idx];
        // SAFETY: exactly one CQ accessor at a time — the caller holds
        // `cq_gate` (module docs; the fusion campaign's gate).
        let mut cq = unsafe { shard.ring.completion_shared() };
        cq.sync();
        let cqes: Vec<(u64, i32)> = cq.by_ref().map(|c| (c.user_data(), c.result())).collect();
        drop(cq);
        let mut served = 0usize;
        let mut last_cqe_ns = 0u64;
        for (user_data, res) in cqes {
            if user_data == NOP_WAKE {
                // Shutdown wake sentinel. Whichever consumer pops it,
                // the reaper cannot strand: its kernel wait is
                // timeout-bounded (`REAP_WAIT_TIMEOUT_NS`), so it
                // re-checks the shutdown flag on its own cadence.
                continue;
            }
            let pending = {
                let mut st = shard
                    .state
                    .lock()
                    .expect("direct-drive state mutex never poisons");
                let slot_idx = user_data as usize;
                let p = st.inflight.get_mut(slot_idx).and_then(Option::take);
                if p.is_some() {
                    st.free.push(slot_idx);
                    st.inflight_count -= 1;
                }
                p
            };
            let Some(pending) = pending else {
                log::error!(
                    "ipc direct-drive: shard {idx} CQE for unknown slot \
                     {user_data} — dropped"
                );
                continue;
            };
            // ipc_direct_phase_ns: `inflight` closes at the CQE pop
            // (slab insert → here — SQE push + flush-batch wait +
            // device service + reap batching); the SAME ns read
            // anchors `device_cq` and `finish` — one clock read (r5
            // single-read law).
            let t_cqe_ns = crate::mono_core::monotonic_ns_u64();
            last_cqe_ns = t_cqe_ns;
            crate::fuse_client::ipc_direct_phase_record_span(
                crate::fuse_client::IpcDirectPhase::Inflight,
                std::time::Duration::from_nanos(t_cqe_ns.saturating_sub(pending.t_insert_ns())),
            );
            crate::op_trace::stamp_mono(
                pending.trace_id(),
                crate::op_trace::Stage::IpcCqe,
                t_cqe_ns,
            );
            // The split's second half: carrying enter → CQE pop. An
            // unstamped op (`t_flush_ns == 0` — the racer class, or a
            // shutdown straggler) records the FULL span here and no
            // sq_wait, so the two halves never double-count.
            let cq_anchor = match pending.t_flush_ns() {
                0 => pending.t_insert_ns(),
                t => t,
            };
            crate::fuse_client::ipc_direct_phase_record_span(
                crate::fuse_client::IpcDirectPhase::DeviceCq,
                std::time::Duration::from_nanos(t_cqe_ns.saturating_sub(cq_anchor)),
            );
            match pending {
                PendingOp::Read(p) => self.finish(p, res, t_cqe_ns),
                PendingOp::Write(p) => self.finish_write(p, res, t_cqe_ns, &mut batch),
            }
            served += 1;
        }
        // The deferred tails — every ACK in the batch has posted.
        if !batch.times.is_empty() {
            let node = batch.times[0].2;
            let entries: Vec<(u64, u64)> = {
                // Dedup by ino keeping the max stamp (same-batch writes
                // to one ino are one refinement park).
                let mut m = std::collections::HashMap::with_capacity(batch.times.len());
                for (ino, ns, _) in batch.times.drain(..) {
                    let e = m.entry(ino).or_insert(0u64);
                    *e = (*e).max(ns);
                }
                m.into_iter().collect()
            };
            METRICS
                .ipc_dd_write_times_dispatches
                .fetch_add(1, Ordering::Relaxed);
            crate::ipc_service::spawn_dd_write_times_park_batch(
                Arc::clone(&self.fs),
                node,
                entries,
            );
        }
        for (key, guard) in batch.trains.drain(..) {
            self.run_train(key, guard);
        }
        if served > 0 {
            // Cadence delivery census + epoch roll — rides the loop's
            // last CQE clock read (no new read; r5 economy).
            self.cadence.on_ops(served as u64);
            if self
                .cadence
                .roll(last_cqe_ns / 1_000_000, u64::from(RING_ENTRIES))
            {
                METRICS
                    .ipc_dd_cadence_k
                    .store(self.cadence.k(), Ordering::Relaxed);
                METRICS
                    .ipc_dd_cadence_probe_ups
                    .store(self.cadence.probe_ups(), Ordering::Relaxed);
                METRICS
                    .ipc_dd_cadence_probe_backoffs
                    .store(self.cadence.probe_backoffs(), Ordering::Relaxed);
            }
        }
        served
    }

    /// CQE disposition: exact-length + 795 revalidation ⇒ serve;
    /// anything else falls back to the handler path (which re-runs the
    /// full moving-custody read protocol and surfaces genuine errors).
    /// `t_cqe` anchors the `ipc_direct_phase_ns` `finish` span.
    fn finish(&self, pending: Pending, res: i32, t_cqe_ns: u64) {
        let Pending {
            op,
            completion,
            snap,
            window,
            win_skew,
            bounce,
            t_insert_ns: _,
            t_flush_ns: _,
        } = pending;
        let exact = res >= 0 && res as usize == window;
        if exact && self.fs.ipc_direct_revalidate(&snap) {
            let req_len = snap.len as usize;
            if let Some((bptr, _backing)) = &bounce {
                // ONE copy of the request slice window→arena (the
                // handoff path's own copy count; aligned legs pay zero).
                // SAFETY: `bptr` is a pool buffer of ≥ `window` bytes,
                // fully DMA-covered by the exact-length check above.
                let slice = unsafe { std::slice::from_raw_parts(bptr.0.add(win_skew), req_len) };
                op.payload.write(slice);
                // Copy ledger: window DMA landed in the pooled bounce
                // (the arena copy above is counted at the write site).
                METRICS
                    .read_fill_dma_bytes
                    .fetch_add(window as u64, Ordering::Relaxed);
            } else {
                // Copy ledger: device DMA straight into the arena window
                // — the zero-daemon-copy direct-drive leg.
                METRICS
                    .read_dest_dma_bytes
                    .fetch_add(window as u64, Ordering::Relaxed);
            }
            METRICS.get_obj.fetch_add(1, Ordering::Relaxed);
            METRICS.ipc_ops_read.fetch_add(1, Ordering::Relaxed);
            METRICS
                .ipc_bytes_out
                .fetch_add(req_len as u64, Ordering::Relaxed);
            METRICS
                .ipc_direct_drive_serves
                .fetch_add(1, Ordering::Relaxed);
            if snap.packed_file_id.is_some() {
                METRICS
                    .ipc_direct_packed_serves
                    .fetch_add(1, Ordering::Relaxed);
                METRICS
                    .ipc_direct_packed_bytes
                    .fetch_add(req_len as u64, Ordering::Relaxed);
            }
            completion.complete(req_len as i64);
            // ipc_direct_phase_ns: served ops close `finish` (CQE pop →
            // completion posted) and `total` (probe entry → here — the
            // daemon-side residence). Fallbacks ride the handler, whose
            // own family times them.
            // ONE end-of-op ns read closes BOTH finish and total (r5
            // single-read law — the dd population's clock share was
            // 7.1 % of cycles at 3 reads/op).
            let end_ns = crate::mono_core::monotonic_ns_u64();
            crate::fuse_client::ipc_direct_phase_record_span(
                crate::fuse_client::IpcDirectPhase::Finish,
                std::time::Duration::from_nanos(end_ns.saturating_sub(t_cqe_ns)),
            );
            crate::fuse_client::ipc_direct_phase_record_span(
                crate::fuse_client::IpcDirectPhase::Total,
                std::time::Duration::from_nanos(end_ns.saturating_sub(snap.t0_ns)),
            );
            crate::op_trace::stamp_mono(op.trace_id, crate::op_trace::Stage::IpcComplete, end_ns);
        } else {
            // Custody moved mid-DMA, or the device said no: the handler
            // owns the truth (never-lossy, never-fabricating).
            METRICS
                .ipc_direct_drive_fallbacks_post
                .fetch_add(1, Ordering::Relaxed);
            if res < 0 {
                log::debug!(
                    "ipc direct-drive: CQE error {res} on ino {} block {} — handler fallback",
                    snap.ino,
                    snap.block
                );
            }
            let request = fuse3::raw::Request {
                unique: 0,
                uid: self.req_uid,
                gid: self.req_gid,
                pid: self.req_pid,
                // Ring-origin op: no kernel delivery, no reply slot.
                slot: fuse3::raw::ReplySlot::Classical,
            };
            crate::ipc_service::spawn_read_handoff(Arc::clone(&self.fs), request, op, completion);
        }
        // A bounce backing drops here → recycled into its home pool.
        drop(bounce);
    }

    /// WRITE CQE disposition (design-il-direct-write §3, PR-3): the W1
    /// patch postlude, order copied from `try_sole_owner_patch` + the
    /// write handler's own tail — re-stabilize + purge on BOTH exits
    /// (after the DMA, strictly before the ACK), then the ledger, the
    /// WriteTimes publish, the W1 inval, the completion, and the
    /// non-ACK-blocking durable-times tail on the handler lanes. Runs on
    /// the shard reaper or the fusion inline arm — every step is
    /// synchronous and runtime-free except the dispatched tail.
    fn finish_write(
        self: &Arc<Self>,
        pending: PendingWrite,
        res: i32,
        t_cqe_ns: u64,
        batch: &mut DrainBatch,
    ) {
        let PendingWrite {
            op,
            completion,
            snap,
            allocator,
            block_guard,
            bounce,
            inval,
            t_insert_ns: _,
            t_flush_ns: _,
        } = pending;
        let len = snap.len as usize;
        let exact = res >= 0 && res as usize == len;
        // Steps 1 + 2 — the `upload_full_block` invalidation set and
        // ordering, on BOTH exits: `publish_block` (the §5.1 word
        // re-stabilizes under a NEW generation), the 4-arm block-key
        // purge, the stale whole-file LRU drops. No allocate, no free,
        // no block-map merge, no journal entry, no staging — the map
        // names the same key.
        allocator.publish_block(snap.dev_off);
        self.fs.router.cache.purge_block_key(snap.key.as_str());
        let file_path = crate::keys::inode_path_stack(snap.ino);
        self.fs.router.cache.write_lru.remove(file_path.as_str());
        self.fs.router.cache.read_lru.remove(file_path.as_str());
        // The block guard held since the probe survives this op's
        // postlude: after the completion posts, the CONVEYOR pops the
        // block's next parked follower and re-drives it under this same
        // tenure ([`Self::run_train`] — where the guard finally drops
        // when the queue empties or the tenure hits its bound).
        let train_key = (snap.ino, snap.block);
        if exact {
            // W-5: an in-place DMA changes no block-map key — stamp the
            // namespace for the ino's next fsync barrier (the publish
            // path's stamp never sees this write).
            self.fs
                .router
                .backend_router
                .note_fsync_touched_keys(snap.ino, std::iter::once(snap.key.as_str()));
            // A direct write IS a patch write (the W1 ledger) + the
            // lane's own engagement instruments + the charter-rule-4
            // data-plane accounting.
            METRICS.patch_writes.fetch_add(1, Ordering::Relaxed);
            METRICS
                .patch_write_bytes
                .fetch_add(len as u64, Ordering::Relaxed);
            METRICS.ipc_dd_write_serves.fetch_add(1, Ordering::Relaxed);
            METRICS
                .ipc_dd_write_bytes
                .fetch_add(len as u64, Ordering::Relaxed);
            METRICS.ipc_ops_write.fetch_add(1, Ordering::Relaxed);
            METRICS
                .ipc_bytes_in
                .fetch_add(len as u64, Ordering::Relaxed);
            // Step 3 — coverage: the ino's stream word was swapped at the
            // probe's commit point (`note_last_write_end` — the coverage
            // bookkeeping the handler performs for THIS shape; the
            // `record_write` union lives on accumulation buffers, which
            // the overlay screen excluded structurally).
            // Step 4 — the WriteTimes publish through the
            // identical-publish elision door. `size_claim: None`: the
            // shape is NON-EXTENDING by eligibility, and a size-neutral
            // completion must never resurrect a stale-high size (KD-6).
            let now_ns = crate::coarse_realtime_ns() as i64;
            let sec = now_ns.div_euclid(1_000_000_000);
            let nsec = now_ns.rem_euclid(1_000_000_000) as u32;
            let _ = self.fs.publish_attr(
                snap.ino,
                crate::fuse_client::AttrPublish::WriteTimes {
                    mtime: fuse3::Timestamp::new(sec, nsec),
                    ctime: fuse3::Timestamp::new(sec, nsec),
                    size_claim: None,
                },
            );
            // Step 5 — killpriv: structurally nothing to clear here —
            // eligibility admits a `kill_priv`-flagged binding only
            // under a HELD known-clean latch (the handler's own D4
            // short-circuit); a non-clean ino fell back so the handler
            // cleared privs BEFORE its data landed (the VFS order).
            // Step 6 — the §5.6.2 W1 inval, after the write landed.
            if let Some(iv) = inval {
                iv.on_write(snap.ino, snap.offset + u64::from(snap.len));
            }
            // Step 7 — ACK.
            completion.complete(len as i64);
            // ONE end-of-op ns read closes BOTH finish and total (r5).
            let end_ns = crate::mono_core::monotonic_ns_u64();
            crate::fuse_client::ipc_direct_phase_record_span(
                crate::fuse_client::IpcDirectPhase::Finish,
                std::time::Duration::from_nanos(end_ns.saturating_sub(t_cqe_ns)),
            );
            crate::fuse_client::ipc_direct_phase_record_span(
                crate::fuse_client::IpcDirectPhase::Total,
                std::time::Duration::from_nanos(end_ns.saturating_sub(snap.t0_ns)),
            );
            crate::op_trace::stamp_mono(op.trace_id, crate::op_trace::Stage::IpcComplete, end_ns);
            // The non-ACK-blocking tail: collected into the drain
            // batch — ONE coalesced handoff per batch (ino-deduped)
            // after the last ACK, never a per-op dispatch (the 208 k/s
            // dd→tpc wake edge the write-wall capture convicted).
            batch
                .times
                .push((snap.ino, now_ns as u64, op.payload.arena_node()));
        } else {
            // A failed/short DMA fails exactly THIS write: nothing
            // acked, nothing parked, nothing lost — tiers purged and
            // the word re-stabilized above, so no stale serve (the
            // patch path's EIO law; only app-written sectors were ever
            // addressed, so the crash blast radius holds).
            METRICS.patch_dma_errors.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "ipc direct-drive: WRITE CQE {res} (wanted {len}) on ino {} block {} — \
                 failing exactly this write (tiers purged, word re-stabilized)",
                snap.ino,
                snap.block
            );
            completion.complete(if res < 0 {
                i64::from(res)
            } else {
                -i64::from(libc::EIO)
            });
        }
        // The bounce backing (if any) recycles; the op's arena window
        // pin (§5.3.1 rule 4) releases only now — after the last DMA
        // byte was consumed by the kernel.
        drop(bounce);
        drop(op);
        // The conveyor pump DEFERS to the batch tail (ACK-fast drain):
        // the guard stays held — parked followers still re-drive under
        // the leader's tenure in FIFO order — but their SQE prep +
        // inline TX no longer run between a sibling CQE's pop and ACK.
        batch.trains.push((train_key, block_guard));
    }

    /// The per-block follower conveyor's CQE-side pump: while the guard
    /// tenure has room, pop the block's next parked follower FIFO and
    /// re-drive it UNDER THE ALREADY-HELD GUARD — the probe-commit steps
    /// re-run via `ipc_direct_write_probe_locked` (no try_lock; the
    /// world may have moved while parked, so a follower failing a state
    /// screen rides the handler, counted on its honest arm), then its
    /// own §5.1 fence + RES-6 authorization + SQE
    /// ([`Self::submit_write_core`] — every follower authorizes its OWN
    /// submission). When the queue empties or the tenure hits
    /// [`TRAIN_BOUND`] (one ring depth's worth — fold/flush wait at most
    /// one train), the guard drops and any residual followers re-drive
    /// through the NORMAL probe (fresh try_lock) from this reap/inline
    /// thread: a residual that wins the lock becomes the next leader
    /// (FIFO preserved — later residuals park on its train); one that
    /// loses to a queued foreign waiter falls back on the honest
    /// `block_lock` arm. Never dropped, never blocked on the svc thread.
    fn run_train(
        self: &Arc<Self>,
        key: (u64, u32),
        mut guard: crate::sqz_sync::SqzMutexGuard<'static, ()>,
    ) {
        loop {
            let Some(parked) = self.trains.pop_under_tenure(key) else {
                // Tenure closed (bound hit or queue empty): release the
                // guard, then drain residuals via the normal drive. A
                // Rearmed pop STOPS the drain — the residual that won
                // the fresh try_lock is the new leader and its CQE pump
                // owns the queue (this thread may hold `cq_gate`; only
                // by returning can that CQE ever be drained).
                drop(guard);
                while let ResidualPop::Popped(parked) = self.trains.pop_residual(key) {
                    let ParkedWrite {
                        op,
                        completion,
                        inval,
                    } = parked;
                    if let Err((op, completion)) = crate::ipc_service::drive_direct_write(
                        &self.fs,
                        Some(self),
                        &inval,
                        op,
                        completion,
                    ) {
                        crate::ipc_service::spawn_write_handoff(
                            Arc::clone(&self.fs),
                            self.ring_request(),
                            inval,
                            op,
                            completion,
                            false,
                        );
                    }
                }
                return;
            };
            let ParkedWrite {
                op,
                completion,
                inval,
            } = parked;
            // The follower's probe-commit under the held guard (its
            // `t0_ns` is the DEQUEUE instant, so the admit span honestly
            // carries the park residence).
            match self.fs.ipc_direct_write_probe_locked(
                op.binding.ino,
                op.desc.offset,
                op.desc.len,
                op.binding.kill_priv,
                op.t0_ns,
            ) {
                Ok(snap) => {
                    match self.submit_write_core(op, completion, snap, guard, inval) {
                        DdWriteSubmit::Submitted => {
                            // The follower's SQE must be kernel-visible
                            // NOW: no sweep-end flush runs on this
                            // thread's cadence, and the reaper's bounded
                            // wake would tax the train hop with up to
                            // its 100 ms liveness cadence.
                            let lane = current_lane() % self.shards.len();
                            self.flush_shard(lane, &self.shards[lane]);
                            return;
                        }
                        DdWriteSubmit::Refused(g) => {
                            // Completed LOUD (fence) — keep the train
                            // moving; each follower fences individually
                            // (loud, never silent).
                            guard = g;
                        }
                        DdWriteSubmit::Fallback {
                            op,
                            completion,
                            inval,
                            guard: g,
                        } => {
                            guard = g;
                            crate::ipc_service::spawn_write_handoff(
                                Arc::clone(&self.fs),
                                self.ring_request(),
                                inval,
                                op,
                                completion,
                                false,
                            );
                        }
                    }
                }
                Err(class) => {
                    // The world moved while parked (overlay parked, size
                    // grew, adjacency…): honest arm + the handler path.
                    crate::ipc_service::count_dd_write_ineligible(class);
                    crate::ipc_service::spawn_write_handoff(
                        Arc::clone(&self.fs),
                        self.ring_request(),
                        inval,
                        op,
                        completion,
                        false,
                    );
                }
            }
        }
    }

    /// Flag shutdown, NOP-wake EVERY spawned shard reaper, join them
    /// (each drains its shard's in-flight CQEs first — bounded by
    /// device latency). Never-engaged shards have no reaper and no
    /// in-flight ops: nothing to wake, nothing to join.
    pub(crate) fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        for shard in &self.shards {
            // Take the handle under the reaper mutex — the same mutex
            // `ensure_reaper` spawns under, so a racing spawn either
            // lands its handle here or observes the flag and refuses.
            let handle = shard
                .reaper
                .lock()
                .expect("direct-drive reaper mutex never poisons")
                .take();
            let Some(handle) = handle else { continue };
            {
                let _st = shard
                    .state
                    .lock()
                    .expect("direct-drive state mutex never poisons");
                let sqe = opcode::Nop::new().build().user_data(NOP_WAKE);
                // SAFETY: SQ exclusive under the mutex.
                let mut sq = unsafe { shard.ring.submission_shared() };
                let _ = unsafe { sq.push(&sqe) };
                sq.sync();
            }
            let _ = shard.ring.submit();
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Foreign threads (no service-lane pin) get a dense round-robin
    /// lane on first use, stable for the thread's life — spread, never
    /// stranded, never racing another thread's assignment.
    #[test]
    fn foreign_thread_lane_fallback_is_dense_and_stable() {
        let lanes: Vec<usize> = (0..3)
            .map(|_| {
                std::thread::spawn(|| {
                    let a = current_lane();
                    let b = current_lane();
                    assert_eq!(a, b, "a thread's lane must be stable");
                    a
                })
                .join()
                .expect("lane probe thread")
            })
            .collect();
        let mut sorted = lanes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "three foreign threads get three lanes");
    }

    /// The re-park livelock rail (field wedge 2026-08-12, qd12+: 2.2 B
    /// park/redrive ping-pong pairs on the reap thread under `cq_gate`,
    /// 160 s stripe waits): a residual drain that observes a RE-ARMED
    /// train must STOP — the new leader's CQE pump owns the queue. The
    /// pre-fix unconditional pop popped the re-armed train's entries,
    /// which the normal drive then re-parked onto the SAME queue: the
    /// exact ping-pong, reconstructed here move by move.
    #[test]
    fn residual_drain_stops_when_the_train_rearms() {
        let trains: WriteTrains<u32> = WriteTrains::new();
        let key = (7u64, 3u32);
        // Leader arms, follower parks, tenure exhausts (close).
        trains.arm(key);
        trains.try_park(key, 11).expect("park on the open train");
        trains.try_park(key, 12).expect("park on the open train");
        assert!(matches!(trains.pop_under_tenure(key), Some(11)));
        // Simulate the tenure bound: close without popping 12.
        trains.close(key);
        // Residual drain begins; a residual (11) won a fresh try_lock
        // mid-drain and RE-ARMED the train.
        trains.arm(key);
        // The old semantics popped 12 here — and the normal drive,
        // seeing an OPEN dd-lane train, parked it right back: the
        // ping-pong. The fixed drain observes the re-arm and stops.
        assert!(
            matches!(trains.pop_residual(key), ResidualPop::Rearmed),
            "a residual drain must never pop from a re-armed train \
             (queue ownership transferred to the live leader's CQE pump)"
        );
        // The queue is intact for the new leader's pump.
        assert!(matches!(trains.pop_under_tenure(key), Some(12)));
        trains.close(key);
        assert!(matches!(trains.pop_residual(key), ResidualPop::Empty));
    }

    /// The closed-train residual drain still drains (the fix must not
    /// strand residuals when NO re-arm happens), and the ring-death
    /// leak arm's unconditional pop keeps working on an open train
    /// (no CQE pump will ever run there).
    #[test]
    fn residual_drain_drains_closed_trains_and_leak_arm_pops_any() {
        let trains: WriteTrains<u32> = WriteTrains::new();
        let key = (9u64, 1u32);
        trains.arm(key);
        trains.try_park(key, 21).expect("park");
        trains.try_park(key, 22).expect("park");
        trains.close(key);
        assert!(matches!(trains.pop_residual(key), ResidualPop::Popped(21)));
        assert!(matches!(trains.pop_residual(key), ResidualPop::Popped(22)));
        assert!(matches!(trains.pop_residual(key), ResidualPop::Empty));
        // Leak arm: pops regardless of open state.
        trains.arm(key);
        trains.try_park(key, 23).expect("park");
        assert_eq!(trains.pop_any(key), Some(23), "ring-death arm pops any");
    }

    /// The eager-flush lever after the r4 re-grade: explicit K wins
    /// verbatim (clamped to the ring depth), explicit 0 = the pre-r4
    /// sweep-only A/B posture, ABSENT = the derived adaptive threshold.
    #[test]
    fn dd_eager_flush_lever_parses_and_clamps() {
        assert_eq!(dd_eager_flush_from(None), None, "absent = sweep-only");
        assert_eq!(dd_eager_flush_from(Some("4")), Some(4));
        assert_eq!(
            dd_eager_flush_from(Some("0")),
            Some(0),
            "0 = sweep-only A/B"
        );
        assert_eq!(
            dd_eager_flush_from(Some("999999")),
            Some(RING_ENTRIES),
            "clamp ceiling = ring entries"
        );
        assert_eq!(dd_eager_flush_from(Some("garbage")), None);
    }

    /// The issue-cadence law, fourth adjudication (write-wall
    /// Addendum 7/8): ABSENT = the `CadenceCore`-GOVERNED threshold —
    /// discovered against live delivery (probe down by halving from the
    /// measured claim size, adopt on response, retreat/decay to
    /// sweep-only), because K=16 was a counted +4–8 % but every
    /// closed-form derivation was falsified. Explicit K wins verbatim;
    /// explicit 0 = sweep-only, governor ignored (the ungoverned A/B
    /// control).
    #[test]
    fn dd_eager_threshold_law() {
        assert_eq!(
            dd_eager_threshold(None, 16),
            16,
            "absent = the governed threshold"
        );
        assert_eq!(
            dd_eager_threshold(None, 0),
            u32::MAX,
            "governor OFF (k=0) = sweep-only"
        );
        assert_eq!(
            dd_eager_threshold(Some(0), 16),
            u32::MAX,
            "0 = sweep-only, governor ignored (A/B)"
        );
        assert_eq!(dd_eager_threshold(Some(4), 16), 4, "explicit wins verbatim");
        assert_eq!(
            dd_eager_threshold(Some(16), 0),
            16,
            "the counted K=16 measurement arm"
        );
    }

    /// The service-lane pin wins over the fallback (the owner-partition
    /// mapping `lane = owner % width` depends on it).
    #[test]
    fn service_lane_pin_wins() {
        std::thread::spawn(|| {
            set_service_lane(7);
            assert_eq!(current_lane(), 7);
        })
        .join()
        .expect("pin probe thread");
    }
}
