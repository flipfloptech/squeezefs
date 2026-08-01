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
//! ## Loom posture
//!
//! No new lock-free protocol is introduced: submission and the
//! in-flight table are guarded by ONE ordinary mutex, the CQ has a
//! single consumer (the reaper thread) by construction, and completion
//! reuses the existing loom-modeled slot machinery (`ipc_slot_core`).

use crate::fuse_client::{IpcDirectSnapshot, SqueezefsFilesystem, METRICS};
use crate::ipc_host::{DataOp, SlotCompletion};
use io_uring::{opcode, types, IoUring};
use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Reaper-wake sentinel (shutdown NOP) — never a slab index.
const NOP_WAKE: u64 = u64::MAX;

/// SQ/CQ entries. 512 in-flight direct reads ≫ any observed governed
/// depth (t32qd32 offers ≤ 1024 across 8 service threads; SQ-full
/// refusals fall back to the handler — counted, never stranded).
const RING_ENTRIES: u32 = 512;

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

/// The ipc-host-owned direct-drive uring: one shared ring (submissions
/// from the service threads under `state`'s mutex; completions reaped
/// by ONE dedicated thread — profile said shared-ring-plus-mutex first,
/// per-thread rings stay the recorded fallback if SQ contention ever
/// shows on a profile).
pub(crate) struct DirectDriveEngine {
    fs: Arc<SqueezefsFilesystem>,
    ring: IoUring,
    /// Guards SQ pushes AND the in-flight slab (one lock, short holds,
    /// never across I/O or a wait).
    state: Mutex<EngineState>,
    vols: HashMap<String, VolSlot>,
    /// Keeps the device fds open for the engine's lifetime.
    _files: Vec<std::fs::File>,
    use_fixed: bool,
    /// Fallback request identity (the sink's ring-op identity).
    req_uid: u32,
    req_gid: u32,
    req_pid: u32,
    shutting_down: AtomicBool,
    reaper: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// SQEs pushed since the last `io_uring_enter` — the submit-batch
    /// economy (the M3/transport-commit-batch lesson, re-learned here:
    /// a per-op enter from every service thread was the measured
    /// kernel-cycle governor at t32qd32 — 4 saturated service threads,
    /// dominant samples in the syscall path). The host's drain pass
    /// calls [`Self::flush`] once per sweep; qd1 pays the same one
    /// enter per op it always did.
    pending_submits: std::sync::atomic::AtomicU32,
}

impl DirectDriveEngine {
    /// Open the data volumes (O_DIRECT with buffered fallback — the
    /// `NvmeBlockDev` worker's own posture on file-backed substrates),
    /// register them as fixed files where the kernel allows, and start
    /// the reaper. Volumes added AFTER spawn are prelude-ineligible
    /// (`ipc_direct_ineligible_backend`) — recorded residual.
    pub(crate) fn spawn(
        fs: Arc<SqueezefsFilesystem>,
        req_uid: u32,
        req_gid: u32,
        req_pid: u32,
    ) -> std::io::Result<Arc<Self>> {
        let ring = IoUring::new(RING_ENTRIES)?;

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
        let use_fixed = if fds.is_empty() {
            false
        } else {
            match ring.submitter().register_files(&fds) {
                Ok(()) => true,
                Err(e) => {
                    log::debug!(
                        "ipc direct-drive: fixed-file register failed ({e}) — using raw fds"
                    );
                    false
                }
            }
        };

        let engine = Arc::new(Self {
            fs,
            ring,
            state: Mutex::new(EngineState {
                inflight: Vec::with_capacity(RING_ENTRIES as usize),
                free: Vec::new(),
                inflight_count: 0,
            }),
            vols,
            _files: files,
            use_fixed,
            req_uid,
            req_gid,
            req_pid,
            shutting_down: AtomicBool::new(false),
            reaper: Mutex::new(None),
            pending_submits: std::sync::atomic::AtomicU32::new(0),
        });
        let reaper_engine = Arc::clone(&engine);
        let handle = std::thread::Builder::new()
            .name("sqz-ipc-dd".into())
            .spawn(move || reaper_engine.reap_loop())?;
        *engine
            .reaper
            .lock()
            .expect("direct-drive reaper mutex never poisons") = Some(handle);
        log::info!(
            "ipc direct-drive engine up: {} volume(s), fixed_files={}, entries={}",
            engine.vols.len(),
            use_fixed,
            RING_ENTRIES
        );
        Ok(engine)
    }

    /// Submit one prelude-eligible op. `Err` returns the op for the
    /// handler fallback (unknown/unhealthy volume, SQ full) — counted
    /// in the `backend` ledger class by the caller's contract here.
    pub(crate) fn submit(
        &self,
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
        let pending = Pending {
            op,
            completion,
            snap,
            window,
            win_skew,
            bounce,
        };

        // Slab insert + SQE push under the ONE engine mutex.
        {
            let mut st = self
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
            let sqe = if self.use_fixed {
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
            let mut sq = unsafe { self.ring.submission_shared() };
            let mut pushed = unsafe { sq.push(&sqe).is_ok() };
            if !pushed {
                // SQ full: flush what's queued, retry once, else refuse
                // (client backpressure via the handler path).
                sq.sync();
                drop(sq);
                let _ = self.ring.submit();
                // SAFETY: as above — still under the mutex.
                let mut sq = unsafe { self.ring.submission_shared() };
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
        self.pending_submits.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Flush pushed-but-unsubmitted SQEs — one `io_uring_enter` per
    /// drain sweep. Cheap when idle (one atomic load); concurrent
    /// flushes are harmless (the kernel consumes the published tail).
    pub(crate) fn flush(&self) {
        if self.pending_submits.swap(0, Ordering::AcqRel) == 0 {
            return;
        }
        for attempt in 0..3 {
            match self.ring.submit() {
                Ok(_) => return,
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
                    log::error!("ipc direct-drive: io_uring submit failed: {e}");
                    return;
                }
            }
        }
    }

    /// The single CQ consumer: block in `io_uring_enter(GETEVENTS,
    /// min_complete=1)`, complete slots straight from CQEs. Exits only
    /// when shutdown is flagged AND the in-flight set is drained (every
    /// pending op pins its session mapping until here — §5.3.1 rule 4).
    fn reap_loop(self: Arc<Self>) {
        loop {
            {
                let st = self
                    .state
                    .lock()
                    .expect("direct-drive state mutex never poisons");
                if self.shutting_down.load(Ordering::SeqCst) && st.inflight_count == 0 {
                    return;
                }
            }
            match self.ring.submitter().submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) => {
                    if self.shutting_down.load(Ordering::SeqCst) {
                        return;
                    }
                    log::error!("ipc direct-drive: reaper enter failed: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            // SAFETY: this thread is the ONLY CQ accessor (construction
            // invariant; §module docs).
            let mut cq = unsafe { self.ring.completion_shared() };
            cq.sync();
            let cqes: Vec<(u64, i32)> = cq.by_ref().map(|c| (c.user_data(), c.result())).collect();
            drop(cq);
            for (user_data, res) in cqes {
                if user_data == NOP_WAKE {
                    continue;
                }
                let pending = {
                    let mut st = self
                        .state
                        .lock()
                        .expect("direct-drive state mutex never poisons");
                    let idx = user_data as usize;
                    let p = st.inflight.get_mut(idx).and_then(Option::take);
                    if p.is_some() {
                        st.free.push(idx);
                        st.inflight_count -= 1;
                    }
                    p
                };
                let Some(pending) = pending else {
                    log::error!("ipc direct-drive: CQE for unknown slot {user_data} — dropped");
                    continue;
                };
                self.finish(pending, res);
            }
        }
    }

    /// CQE disposition: exact-length + 795 revalidation ⇒ serve;
    /// anything else falls back to the handler path (which re-runs the
    /// full moving-custody read protocol and surfaces genuine errors).
    fn finish(&self, pending: Pending, res: i32) {
        let Pending {
            op,
            completion,
            snap,
            window,
            win_skew,
            bounce,
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
            };
            crate::ipc_service::spawn_read_handoff(Arc::clone(&self.fs), request, op, completion);
        }
        // A bounce backing drops here → recycled into its home pool.
        drop(bounce);
    }

    /// Flag shutdown, NOP-wake the reaper, join it (it drains every
    /// in-flight CQE first — bounded by device latency).
    pub(crate) fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let _st = self
                .state
                .lock()
                .expect("direct-drive state mutex never poisons");
            let sqe = opcode::Nop::new().build().user_data(NOP_WAKE);
            // SAFETY: SQ exclusive under the mutex.
            let mut sq = unsafe { self.ring.submission_shared() };
            let _ = unsafe { sq.push(&sqe) };
            sq.sync();
        }
        let _ = self.ring.submit();
        let handle = self
            .reaper
            .lock()
            .expect("direct-drive reaper mutex never poisons")
            .take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}
