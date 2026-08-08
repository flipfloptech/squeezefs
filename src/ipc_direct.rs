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

/// The issue-cadence law after the r5 CALIBRATED re-adjudication:
/// ABSENT/0 = sweep-only (the M3 submit-batch posture — one enter per
/// drain sweep), explicit K wins verbatim. The r4 derived-adaptive arm
/// (un-issued tail ≤ kernel_inflight/8) was FALSIFIED on the licensed
/// venue (`.benchmarks/2026-08-08-iops-internal-time-r5.md` §calibration:
/// sweep-only 955 k vs adaptive 822 k vs K=16 878 k at 32×32 over the
/// calibrated ~316 µs operating point — every mid-sweep enter taxes the
/// svc thread more than issue promptness pays once the sweep cadence
/// itself is prompt; r4's +19 % was an artifact of its miscalibrated
/// 2.5×-slow venue) and deleted per the no-dead-code law. The knob
/// stays the counted measurement lever.
fn dd_eager_threshold(explicit: Option<u32>) -> u32 {
    match explicit {
        Some(k) if k > 0 => k,
        _ => u32::MAX,
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
    /// (shim-iops campaign, 2026-08-07 — always-on residence
    /// decomposition; `snap.t0` anchors `admit`/`total`).
    t_insert: std::time::Instant,
}

struct EngineState {
    inflight: Vec<Option<Pending>>,
    free: Vec<usize>,
    inflight_count: usize,
}

/// A registered data volume: fixed-file index when registration
/// succeeded, raw fd otherwise.
struct VolSlot {
    fixed: u32,
    raw: RawFd,
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
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true);
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_DIRECT);
            }
            let file = match opts.open(&path) {
                Ok(f) => f,
                Err(_) => {
                    // File-backed substrates that refuse O_DIRECT
                    // (tmpfs sandboxes) — buffered, like the worker.
                    match std::fs::OpenOptions::new().read(true).open(&path) {
                        Ok(f) => f,
                        Err(e) => {
                            log::warn!(
                                "ipc direct-drive: cannot open data volume '{be_id}' at \
                                 {path}: {e} — its blocks stay on the handler path"
                            );
                            continue;
                        }
                    }
                }
            };
            let fixed = files.len() as u32;
            vols.insert(
                be_id,
                VolSlot {
                    fixed,
                    raw: file.as_raw_fd(),
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
        let mut coop_ok = true;
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
            _files: files,
            req_uid,
            req_gid,
            req_pid,
            shutting_down: AtomicBool::new(false),
            reapers_spawned: AtomicUsize::new(0),
            ext_arg_ok: AtomicBool::new(true),
        });
        // Fresh engine: no shard reaper is live yet (spawn-on-first-
        // submit) — the gauge reflects THIS engine from here on.
        METRICS.ipc_direct_shards.store(0, Ordering::Relaxed);
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
            .name(format!("sqz-ipc-dd{idx}"))
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
        let (be_id, dev_off) = match router.backend_router.parse_block_key(&snap.key) {
            Ok(v) => v,
            Err(_) => {
                METRICS
                    .ipc_direct_ineligible_backend
                    .fetch_add(1, Ordering::Relaxed);
                return Err((op, completion));
            }
        };
        let Some(vol) = self.vols.get(&be_id) else {
            METRICS
                .ipc_direct_ineligible_backend
                .fetch_add(1, Ordering::Relaxed);
            return Err((op, completion));
        };
        if !router.backend_router.is_backend_healthy(&be_id) {
            METRICS
                .ipc_direct_ineligible_backend
                .fetch_add(1, Ordering::Relaxed);
            return Err((op, completion));
        }

        // Window geometry (the handler's ranged arithmetic verbatim).
        let block_size = router.block_size.load(Ordering::Relaxed);
        let rel = snap.offset - u64::from(snap.block) * block_size;
        let rel_end = rel + u64::from(snap.len);
        let aligned_start = rel & !(LBA - 1);
        let aligned_end = std::cmp::min(rel_end.div_ceil(LBA) * LBA, block_size);
        let window = (aligned_end - aligned_start) as usize;
        let win_skew = (rel - aligned_start) as usize;
        let req_len = snap.len as usize;

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
        METRICS.ranged_reads.fetch_add(1, Ordering::Relaxed);
        METRICS
            .ranged_read_bytes
            .fetch_add(window as u64, Ordering::Relaxed);
        if window != req_len {
            METRICS
                .ranged_read_unaligned_bounces
                .fetch_add(1, Ordering::Relaxed);
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
        // slab insert); the SAME instant anchors `inflight` — one
        // clock read for both (drain-funnel clock economy, r3).
        let t_insert = std::time::Instant::now();
        crate::fuse_client::ipc_direct_phase_record_span(
            crate::fuse_client::IpcDirectPhase::Admit,
            t_insert.saturating_duration_since(snap.t0),
        );
        let pending = Pending {
            op,
            completion,
            snap,
            window,
            win_skew,
            bounce,
            t_insert,
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
            st.inflight[idx] = Some(pending);
            st.inflight_count += 1;
        }
        // Issue cadence (r5 calibrated re-adjudication): sweep-only by
        // default — the SQE becomes kernel-visible at the sweep tail
        // (`flush`); every mid-sweep enter taxes the svc thread more
        // than issue promptness pays once the sweep cadence is prompt
        // (the r4 adaptive arm was falsified on the licensed venue).
        // Explicit `SQUEEZEFS_IPC_DD_EAGER_FLUSH=K` remains the counted
        // measurement lever.
        let pending = shard.pending_submits.fetch_add(1, Ordering::Release) + 1;
        if pending >= dd_eager_threshold(dd_eager_flush()) {
            Self::flush_shard(lane, shard);
        }
        Ok(())
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
    pub(crate) fn flush(&self) {
        if dd_lane_flush() {
            let lane = current_lane() % self.shards.len();
            Self::flush_shard(lane, &self.shards[lane]);
        } else {
            for (lane, shard) in self.shards.iter().enumerate() {
                Self::flush_shard(lane, shard);
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
    fn flush_shard(lane: usize, shard: &DdShard) {
        if shard.pending_submits.swap(0, Ordering::AcqRel) == 0 {
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
                            for slot in st.inflight.iter_mut() {
                                if let Some(p) = slot.take() {
                                    std::mem::forget(p);
                                    n += 1;
                                }
                            }
                            st.inflight_count = 0;
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
    fn drain_cq_locked(&self, idx: usize) -> usize {
        let shard = &self.shards[idx];
        // SAFETY: exactly one CQ accessor at a time — the caller holds
        // `cq_gate` (module docs; the fusion campaign's gate).
        let mut cq = unsafe { shard.ring.completion_shared() };
        cq.sync();
        let cqes: Vec<(u64, i32)> = cq.by_ref().map(|c| (c.user_data(), c.result())).collect();
        drop(cq);
        let mut served = 0usize;
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
            // device service + reap batching); the SAME instant
            // anchors `finish` — one clock read for both (r3).
            let t_cqe = std::time::Instant::now();
            crate::fuse_client::ipc_direct_phase_record_span(
                crate::fuse_client::IpcDirectPhase::Inflight,
                t_cqe.saturating_duration_since(pending.t_insert),
            );
            self.finish(pending, res, t_cqe);
            served += 1;
        }
        served
    }

    /// CQE disposition: exact-length + 795 revalidation ⇒ serve;
    /// anything else falls back to the handler path (which re-runs the
    /// full moving-custody read protocol and surfaces genuine errors).
    /// `t_cqe` anchors the `ipc_direct_phase_ns` `finish` span.
    fn finish(&self, pending: Pending, res: i32, t_cqe: std::time::Instant) {
        let Pending {
            op,
            completion,
            snap,
            window,
            win_skew,
            bounce,
            t_insert: _,
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
            completion.complete(req_len as i64);
            // ipc_direct_phase_ns: served ops close `finish` (CQE pop →
            // completion posted) and `total` (probe entry → here — the
            // daemon-side residence). Fallbacks ride the handler, whose
            // own family times them.
            crate::fuse_client::ipc_direct_phase_record(
                crate::fuse_client::IpcDirectPhase::Finish,
                t_cqe,
            );
            crate::fuse_client::ipc_direct_phase_record(
                crate::fuse_client::IpcDirectPhase::Total,
                snap.t0,
            );
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

    /// The issue-cadence law after the r5 calibrated re-adjudication:
    /// absent/0 = sweep-only (never mid-sweep — the shipped posture,
    /// re-proven on the licensed venue), explicit K wins verbatim.
    #[test]
    fn dd_eager_threshold_law() {
        assert_eq!(dd_eager_threshold(None), u32::MAX, "absent = sweep-only");
        assert_eq!(dd_eager_threshold(Some(0)), u32::MAX, "0 = sweep-only");
        assert_eq!(dd_eager_threshold(Some(4)), 4, "explicit wins verbatim");
        assert_eq!(dd_eager_threshold(Some(16)), 16, "the K=16 measurement arm");
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
