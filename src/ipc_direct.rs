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
        for idx in 0..width {
            let ring = IoUring::new(RING_ENTRIES)?;
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
        // slab insert); the same instant anchors `inflight`.
        let t_insert = std::time::Instant::now();
        crate::fuse_client::ipc_direct_phase_record(
            crate::fuse_client::IpcDirectPhase::Admit,
            snap.t0,
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
        // No enter here: the SQE is published; the owning service
        // thread's drain pass flushes ONCE per sweep (`flush`), so a
        // deep-qd burst pays one syscall per batch instead of one per
        // op. (The SQ-full path above already entered to make room.)
        shard.pending_submits.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Flush pushed-but-unsubmitted SQEs — one `io_uring_enter` per
    /// shard with published work, per drain sweep. Cheap when idle
    /// (one atomic load per shard, width ≤ 16 derived / 64 clamped);
    /// concurrent flushes are harmless (the kernel consumes the
    /// published tail). Flushing ALL shards (not just the caller's
    /// lane) keeps the liveness rule thread-pairing-independent.
    pub(crate) fn flush(&self) {
        for (lane, shard) in self.shards.iter().enumerate() {
            if shard.pending_submits.swap(0, Ordering::AcqRel) == 0 {
                continue;
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
            match shard.ring.submitter().submit_and_wait(1) {
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
            // SAFETY: this thread is the ONLY CQ accessor of THIS
            // shard's ring (construction invariant; §module docs).
            let mut cq = unsafe { shard.ring.completion_shared() };
            cq.sync();
            let cqes: Vec<(u64, i32)> = cq.by_ref().map(|c| (c.user_data(), c.result())).collect();
            drop(cq);
            for (user_data, res) in cqes {
                if user_data == NOP_WAKE {
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
                // device service + reap batching); the same instant
                // anchors `finish`.
                let t_cqe = std::time::Instant::now();
                crate::fuse_client::ipc_direct_phase_record(
                    crate::fuse_client::IpcDirectPhase::Inflight,
                    pending.t_insert,
                );
                self.finish(pending, res, t_cqe);
            }
        }
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

    /// The eager-flush lever (shim-iops campaign, 2026-08-07): 0 =
    /// shipped end-of-sweep flush only (the M3 submit-batch economy);
    /// K > 0 = a lane whose unflushed SQE count reaches K enters
    /// inline, so a mid-sweep burst starts its device service without
    /// waiting for the sweep tail. Clamp ceiling = the per-shard ring
    /// depth (an eager threshold past SQ capacity is meaningless).
    #[test]
    fn dd_eager_flush_default_and_clamp() {
        assert_eq!(dd_eager_flush_from(None), 0, "shipped = end-of-sweep");
        assert_eq!(dd_eager_flush_from(Some("4")), 4);
        assert_eq!(
            dd_eager_flush_from(Some("999999")),
            RING_ENTRIES,
            "clamp ceiling = ring entries"
        );
        assert_eq!(dd_eager_flush_from(Some("garbage")), 0);
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
