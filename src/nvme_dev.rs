/*
 * JuiceFS, Copyright 2026 Juicedata, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::error::Result;
use io_uring::{opcode, types, types::Fd, IoUring};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use tokio::sync::oneshot;

pub static SIMULATE_CORRUPTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test/fault-injection: next N `write_block` calls fail before I/O.
/// Used by layout atomicity tests (P0-2) and related regression suites.
pub static FAIL_NEXT_WRITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn set_simulate_corruption(val: bool) {
    SIMULATE_CORRUPTION.store(val, std::sync::atomic::Ordering::Relaxed);
}

pub fn set_fail_next_writes(n: usize) {
    FAIL_NEXT_WRITES.store(n, std::sync::atomic::Ordering::SeqCst);
}

pub fn clear_fail_next_writes() {
    FAIL_NEXT_WRITES.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Test seam: the worker stalls the next N read submissions by
/// [`read_stall_ms`] each — the deterministic stand-in for a
/// wedged/fabric-stalled device (the `SQUEEZEFS_TEST_WRITE_STALL_MS`
/// precedent: load selects such schedules; this lever selects them
/// deterministically — `tests/nvme_dest_ownership_tests.rs`, MEM-1).
/// One relaxed load unset; never set in production.
static STALL_NEXT_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static STALL_READ_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Arm the read-stall test seam: the next `count` worker read submissions
/// each stall `ms` milliseconds (`0` disables).
pub fn set_test_read_stall(count: usize, ms: u64) {
    STALL_READ_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
    STALL_NEXT_READS.store(count, std::sync::atomic::Ordering::SeqCst);
}

/// Test seam: the worker stalls the next N WRITE submissions (the
/// deterministic stand-in for a slow fabric DMA — the read seam's twin,
/// used by the DUR-2 cross-lane flush-drain contract). One relaxed load
/// unset; never set in production.
static STALL_NEXT_WRITES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static STALL_WRITE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Arm the write-stall test seam: the next `count` worker write
/// submissions each stall `ms` milliseconds (`0` disables).
pub fn set_test_write_stall(count: usize, ms: u64) {
    STALL_WRITE_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
    STALL_NEXT_WRITES.store(count, std::sync::atomic::Ordering::SeqCst);
}

/// Caller-side wait bound (ms) on the uring-worker read oneshot. Default
/// 30 000 — a wedged device/worker must not freeze the whole FUSE session
/// (pre-MEM-1 posture, unchanged); the MEM-1 stall legs shorten it via
/// [`set_test_read_timeout_ms`] / `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS`
/// so the repro runs in test time.
fn read_timeout_ms_cell() -> &'static std::sync::atomic::AtomicU64 {
    static CELL: std::sync::OnceLock<std::sync::atomic::AtomicU64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(30_000);
        std::sync::atomic::AtomicU64::new(v.max(1))
    })
}

/// Set the read-timeout test seam (tests only; clamped to ≥ 1 ms).
pub fn set_test_read_timeout_ms(ms: u64) {
    read_timeout_ms_cell().store(ms.max(1), std::sync::atomic::Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// MEM-1 (pre-rc engineering spec §2, P0): zero-copy read-destination
// ownership. A `dest_addr` read DMAs device bytes straight into memory the
// worker does not own — for kernel READs that is the FUSE-over-io_uring
// registered ent payload buffer, whose COMMIT_AND_FETCH re-arm hands it to
// a NEW kernel request. If the awaiting future times out (or is dropped)
// while the SQE is still in flight, the handler replies, the ent re-arms,
// and the late DMA lands in someone else's buffer: cross-request
// corruption. The registry below closes it with the transport's own §5.4
// grain: at request build the read path CLAIMS an owner token covering the
// destination (for transport dests, a `fuse3` `DestDmaLease` holding the
// ent's lease refs); the token rides inside the worker's in-flight request
// and drops only at CQE completion (or worker teardown), so the transport
// commit gate parks the re-arm until no SQE can still write the buffer.
// ---------------------------------------------------------------------------

/// Owner token for one in-flight zero-copy read destination. Opaque to the
/// worker: it is held for exactly the SQE's lifetime and dropped at CQE
/// completion — the drop is what releases the destination back to its
/// owner (for transport payload dests: the §5.4 lease release that lets a
/// parked COMMIT_AND_FETCH re-arm proceed).
pub type DestToken = Box<dyn std::any::Any + Send>;

/// Resolver: `(dest_addr, len)` → an owner token when the window lies
/// inside a region this resolver owns (`None` = not mine). Must be cheap
/// and lock-free — it runs once per dest-bearing device read.
pub type DestResolver = Arc<dyn Fn(u64, usize) -> Option<DestToken> + Send + Sync>;

struct DestResolverEntry {
    /// Liveness anchor (the session connection): a dead anchor is skipped
    /// at claim time and pruned on the next registration, so sessions
    /// never need an explicit unregister.
    anchor: std::sync::Weak<dyn std::any::Any + Send + Sync>,
    resolve: DestResolver,
}

/// Lock-free read-mostly registry (`ArcSwap` — registrations happen once
/// per mount; claims are hot-ish, once per dest-bearing device read).
fn dest_resolvers() -> &'static arc_swap::ArcSwap<Vec<Arc<DestResolverEntry>>> {
    static CELL: std::sync::OnceLock<arc_swap::ArcSwap<Vec<Arc<DestResolverEntry>>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| arc_swap::ArcSwap::from_pointee(Vec::new()))
}

/// Register a zero-copy destination resolver (mount arm path). `anchor`
/// scopes its life: claims skip entries whose anchor is gone, and dead
/// entries are pruned on the next registration.
pub fn register_dest_resolver(
    anchor: std::sync::Weak<dyn std::any::Any + Send + Sync>,
    resolve: DestResolver,
) {
    // Serialize writers only (registration is a per-mount event); readers
    // stay lock-free through the ArcSwap.
    static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = WRITER.lock().expect("dest-resolver writer lock");
    let cur = dest_resolvers().load_full();
    let mut next: Vec<Arc<DestResolverEntry>> = cur
        .iter()
        .filter(|e| e.anchor.strong_count() > 0)
        .cloned()
        .collect();
    next.push(Arc::new(DestResolverEntry { anchor, resolve }));
    dest_resolvers().store(Arc::new(next));
}

/// Claim the owner token covering `[addr, addr + len)`. `None` = no
/// registered owner claims the window — e.g. IPC-arena dests, whose
/// session mapping has lifetime machinery of its own.
pub fn claim_dest_token(addr: u64, len: usize) -> Option<DestToken> {
    let entries = dest_resolvers().load();
    for e in entries.iter() {
        if e.anchor.strong_count() == 0 {
            continue;
        }
        if let Some(t) = (e.resolve)(addr, len) {
            return Some(t);
        }
    }
    None
}

struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

enum WriteData {
    Aligned {
        data: bytes::Bytes,
    },
    /// Heap buffer from `posix_memalign` — free with `libc::free`.
    Unaligned {
        ptr: SendPtr,
        len: usize,
    },
    /// Buffer from [`crate::cache::ALIGNED_BUF_POOL`] — recycle on completion (P2-4).
    PooledUnaligned {
        ptr: SendPtr,
        len: usize,
    },
}

fn release_write_buf(data: &WriteData) {
    match data {
        WriteData::Aligned { .. } => {}
        WriteData::Unaligned { ptr, .. } => {
            if !ptr.0.is_null() {
                // SAFETY: `WriteData::Unaligned` pointers come from this module's
                // `posix_memalign` below (never a pool slot), and this release is the
                // buffer's single terminal use — the request is complete.
                unsafe {
                    libc::free(ptr.0 as *mut libc::c_void);
                }
            }
        }
        WriteData::PooledUnaligned { ptr, .. } => {
            // SAFETY: `PooledUnaligned` pointers come from
            // `ALIGNED_BUF_POOL.alloc_raw()` (P2-4) and this release is the
            // buffer's single terminal use.
            unsafe { crate::cache::ALIGNED_BUF_POOL.recycle(ptr.0) };
        }
    }
}

fn release_free_ptr(kind: FreePtrKind, p: SendPtr) {
    match kind {
        FreePtrKind::Libc => {
            if !p.0.is_null() {
                // SAFETY: `FreePtrKind::Libc` pointers come from `posix_memalign`
                // (never a pool slot), and this release is their single terminal use.
                unsafe {
                    libc::free(p.0 as *mut libc::c_void);
                }
            }
        }
        FreePtrKind::Pool => {
            // SAFETY: `FreePtrKind::Pool` pointers come from
            // `ALIGNED_BUF_POOL.alloc_raw()` and this release is the buffer's
            // single terminal use.
            unsafe { crate::cache::ALIGNED_BUF_POOL.recycle(p.0) };
        }
    }
}

#[derive(Clone, Copy)]
enum FreePtrKind {
    Libc,
    Pool,
}

enum UringRequest {
    Read {
        offset: u64,
        buf_ptr: SendPtr,
        size: usize,
        bytes: bytes::Bytes,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
        /// Enqueue stamp — feeds `read_fill_phase_ns.dev_queue`
        /// (channel + slot wait before the SQE submits).
        enq: std::time::Instant,
        /// MEM-1: owner token for a zero-copy destination (`dest_addr`
        /// reads). The WORKER holds it for exactly the SQE's lifetime and
        /// drops it at CQE completion (before the caller oneshot fires),
        /// so an abandoned caller future (timeout / drop) cannot let the
        /// destination's owner — the transport ent re-arm path — hand the
        /// buffer to a new request while device DMA can still land in it.
        /// `None` for pooled reads (ownership already moves with the
        /// pooled `Bytes`) and for dests with no registered owner.
        dest_token: Option<DestToken>,
    },
    Write {
        offset: u64,
        data: WriteData,
        tx: oneshot::Sender<Result<()>>,
    },
    /// DUR-2: the data-device durability barrier (io_uring `Fsync`,
    /// `FSYNC_DATASYNC` when `datasync`). `O_DIRECT` bypasses the page
    /// cache, NOT the device's volatile write cache — without this op
    /// the striped write-through sequence (DMA → map merge → meta
    /// journal → meta fdatasync) ordered nothing at all on the data
    /// device. Issued only through
    /// [`NvmeBlockDev::flush`], which coalesces callers.
    Fsync {
        datasync: bool,
        tx: oneshot::Sender<Result<()>>,
    },
}

enum UringResponse {
    Read {
        bytes: bytes::Bytes,
        size: usize,
        offset: u64,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
        /// SQE-submit stamp — feeds `read_fill_phase_ns.dev_service`
        /// (submit → CQE completion).
        submitted: std::time::Instant,
    },
    Write {
        tx: oneshot::Sender<Result<()>>,
    },
}

/// Map a read CQE to the caller-visible result under the exact-length
/// contract (VL8 item 5): every `UringResponse::Read` consumer
/// (`read_block` / `read_block_with_dest` / `verify_write_block` and the
/// routing-layer `read_block`/`read_block_range`/`read_file_range_zero_copy`)
/// assumes the returned buffer holds exactly `size` bytes of device data.
/// A short or zero completion (past-EOF on file-backed substrates; real
/// block devices are all-or-EIO) must therefore fail loud — success would
/// surface recycled pool-buffer bytes as data (silent-garbage class).
fn finish_read(
    io_res: std::io::Result<usize>,
    bytes: bytes::Bytes,
    size: usize,
    offset: u64,
) -> Result<bytes::Bytes> {
    let got = io_res.map_err(crate::error::SqueezefsError::Io)?;
    if got != size {
        return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "short read: kernel returned {got} of {size} bytes at offset {offset} \
                 (past-EOF/short reads must not surface recycled buffer bytes)"
            ),
        )));
    }
    Ok(bytes.slice(0..size))
}

/// Per-lane WRITE completion watermark (write-lane-fanout campaign,
/// 2026-08-07): the DUR-2 barrier's cross-lane ordering instrument. Every
/// write/fsync submission bumps `submitted` (caller side, before the
/// channel send); every TERMINAL resolution of one — CQE completion,
/// SQ-full refusal, worker-teardown drain — bumps `completed` (worker
/// side, authoritative: a dropped caller future cannot strand the
/// ledger). [`NvmeBlockDev::flush`] captures each lane's `submitted`
/// watermark at barrier start and waits until `completed` reaches it
/// BEFORE issuing the device Fsync, so "everything written before the
/// barrier started" holds at submission grain across every lane — a
/// device flush command only covers commands the device completed before
/// processing it, and an io_uring `Fsync` never drains its own ring's
/// in-flight SQEs (no `IO_DRAIN`), so the watermark is what makes the
/// contract true (it was caller-await-dependent even on the single
/// lane).
///
/// Reads deliberately do NOT ride the ledger: a barrier must never wait
/// out a wedged read's 30 s timeout class.
///
/// Wake economy: the completer pays a `notify_waiters` only when a
/// barrier is actually parked (`waiters > 0` — the `transport_wake_*` /
/// `ipc_cqe_wake_*` discipline). The waiter-registration/completion pair
/// is the cqe_core Dekker shape: both sides are SeqCst RMW/loads, so a
/// completer that misses the waiter forces the waiter to see the count.
struct WriteWatermark {
    submitted: std::sync::atomic::AtomicU64,
    completed: std::sync::atomic::AtomicU64,
    waiters: std::sync::atomic::AtomicUsize,
    drained: tokio::sync::Notify,
    /// Worker thread exited (normal drain OR refused open/ring): any
    /// unreachable watermark target fails loud instead of parking a
    /// barrier forever.
    dead: std::sync::atomic::AtomicBool,
}

impl WriteWatermark {
    fn new() -> Self {
        Self {
            submitted: std::sync::atomic::AtomicU64::new(0),
            completed: std::sync::atomic::AtomicU64::new(0),
            waiters: std::sync::atomic::AtomicUsize::new(0),
            drained: tokio::sync::Notify::new(),
            dead: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// One write/fsync entered this lane's channel.
    fn note_submitted(&self) {
        self.submitted
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// One write/fsync terminally resolved (CQE, refusal, or teardown).
    fn note_completed(&self) {
        self.completed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.waiters.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.drained.notify_waiters();
        }
    }

    /// The worker thread is gone (all exit paths, including refused
    /// open/ring setup where channel residue was dropped unaccounted).
    fn mark_dead(&self) {
        self.dead.store(true, std::sync::atomic::Ordering::SeqCst);
        self.drained.notify_waiters();
    }

    /// Park until `completed ≥ target` (the DUR-2 drain). Errors loud if
    /// the worker died — a barrier must fail, never falsely complete.
    async fn wait_completed(&self, target: u64) -> Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        /// Cancellation-safe waiter count (a dropped barrier future must
        /// not leave the completer notifying forever).
        struct WaiterGuard<'a>(&'a WriteWatermark);
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                self.0.waiters.fetch_sub(1, SeqCst);
            }
        }
        loop {
            if self.completed.load(SeqCst) >= target {
                return Ok(());
            }
            if self.dead.load(SeqCst) {
                return Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shut down during data-device barrier drain".to_string(),
                ));
            }
            // Dekker registration: publish waiter presence, arm the
            // Notify (enable = registered for notify_waiters), THEN
            // re-check — the completer's SeqCst pair cannot miss us.
            self.waiters.fetch_add(1, SeqCst);
            let _g = WaiterGuard(self);
            let mut notified = std::pin::pin!(self.drained.notified());
            notified.as_mut().enable();
            if self.completed.load(SeqCst) >= target || self.dead.load(SeqCst) {
                continue;
            }
            notified.await;
        }
    }
}

struct UringWorker {
    /// Dropped first in `Drop` so the worker observes disconnect and drains.
    tx: Option<crossbeam::channel::Sender<UringRequest>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The lane's DUR-2 write watermark (see [`WriteWatermark`]).
    wm: Arc<WriteWatermark>,
}

/// P1-6: bound io_uring request queue to apply backpressure under overload.
const URING_REQ_QUEUE_CAP: usize = 4096;

impl UringWorker {
    fn new(device_path: String) -> Self {
        let (tx, rx) = crossbeam::channel::bounded(URING_REQ_QUEUE_CAP);
        let wm = Arc::new(WriteWatermark::new());
        let worker_wm = wm.clone();
        let thread = std::thread::spawn(move || {
            worker_thread_loop(device_path, rx, &worker_wm);
            // Every exit path (normal drain, refused O_DIRECT open,
            // refused ring build) lands here: unreachable watermark
            // targets fail parked barriers loud instead of forever.
            worker_wm.mark_dead();
        });
        Self {
            tx: Some(tx),
            thread: Some(thread),
            wm,
        }
    }

    fn sender(&self) -> &crossbeam::channel::Sender<UringRequest> {
        self.tx
            .as_ref()
            .expect("UringWorker sender used after drop")
    }
}

impl Drop for UringWorker {
    /// RES-12 (pre-RC spec §7): this join stays **synchronous** on purpose.
    ///
    /// The spec item is that dropping this from an async context blocks a
    /// tokio worker for the whole drain. That is true, and the fix is at
    /// the DROP SITES, not here: deferring the join would make teardown
    /// ordering non-deterministic, and P0-1's contract is precisely that
    /// the worker's exit cleanup has run when `Drop` returns.
    ///
    /// The traced sites, and what each does now:
    /// * `config_ops::probe_data_volume_rw` — constructs and drops a device
    ///   inside one async fn, reachable from the LIVE daemon via the
    ///   admin-lane `volume-add-data` verb
    ///   (`SqueezefsFilesystem::admin_add_data_volume`) as well as from the
    ///   offline CLI. Hops through `detached::drop_off_runtime` and awaits.
    /// * `routing::BackendRouter::retire_backend` — the VL4 retire drops
    ///   the last `Arc<StorageBackend>` (hence this worker) on a live
    ///   mount. Hops through `drop_off_runtime`, not awaited.
    /// * the daemon's own `default_device` + per-volume backends, and the
    ///   offline verbs (`fsck`, `defrag`, `volume add-data`, `main.rs`'s
    ///   one-shot device) — these drop when the whole object graph does, at
    ///   the end of the process. Blocking a worker there has no victim
    ///   (nothing else needs that thread), so they are deliberately left
    ///   inline.
    ///
    /// Worst case for the join: idle workers park in `rx.recv()` and wake
    /// on disconnect immediately; a worker with in-flight SQEs finishes
    /// them first, bounded by device latency (the same bound MEM-1's 30 s
    /// timeout guards).
    fn drop(&mut self) {
        // Close the channel first so the worker stops accepting work and exits
        // its loop, then join so exit cleanup (free unaligned bufs) runs before
        // we return (P0-1).
        drop(self.tx.take());
        if let Some(handle) = self.thread.take() {
            if let Err(e) = handle.join() {
                log::error!("NvmeBlockDev uring worker thread panicked: {:?}", e);
            }
        }
    }
}

/// PERF-4 (a): build the worker ring with the strongest issue-economy
/// flags the running kernel accepts — a RUNTIME probe ladder, never a
/// kernel-version table (portable law). `SINGLE_ISSUER` is free by
/// construction (the worker thread that builds the ring is its only
/// submitter); `DEFER_TASKRUN` moves completion task-work onto this
/// thread's own `io_uring_enter` instead of async IPI/task_work
/// interrupts (requires SINGLE_ISSUER, 6.1+); `COOP_TASKRUN` is the
/// milder 5.19+ variant. Each refusal falls back one rung; the last rung
/// is today's plain ring.
fn build_worker_ring(entries: u32) -> std::io::Result<(IoUring, &'static str)> {
    if let Ok(r) = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(entries)
    {
        return Ok((r, "single_issuer+defer_taskrun"));
    }
    if let Ok(r) = IoUring::builder()
        .setup_single_issuer()
        .setup_coop_taskrun()
        .build(entries)
    {
        return Ok((r, "single_issuer+coop_taskrun"));
    }
    if let Ok(r) = IoUring::builder().setup_single_issuer().build(entries) {
        return Ok((r, "single_issuer"));
    }
    Ok((IoUring::new(entries)?, "plain"))
}

fn worker_thread_loop(
    device_path: String,
    rx: crossbeam::channel::Receiver<UringRequest>,
    wm: &WriteWatermark,
) {
    // TEST-1 env seam (`SQUEEZEFS_TEST_POWER_CUT_DEVS`): read ONCE per
    // worker, zero cost unset, never set in production.
    crate::dev_power_cut::arm_from_env(&device_path);

    let mut open_opts = OpenOptions::new();
    open_opts.read(true).write(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.custom_flags(libc::O_DIRECT);
    }

    let file = match open_opts.open(&device_path) {
        Ok(f) => f,
        Err(e) => {
            // DUR-2 decision (a) — FAIL LOUD, never degrade. The old
            // buffered fallback ran the data plane in a mode where the
            // kernel owns dirty pages nobody ever flushed, so
            // acknowledged data was lost on power failure with a single
            // `warn!` as the only trace. The worker exits: every request
            // on this device fails loudly, and mounts that took the
            // checked open ([`NvmeBlockDev::open_checked`]) refused
            // before reaching here.
            log::error!(
                "REFUSING data volume {:?}: O_DIRECT open failed ({:?}). Buffered device \
                 I/O is not a supported data-plane mode (DUR-2: acknowledged writes would \
                 have no barrier). Fix the substrate — do not degrade.",
                device_path,
                e
            );
            return;
        }
    };

    let fd = file.as_raw_fd();

    let mut ring = match build_worker_ring(1024) {
        Ok((r, flags)) => {
            log::debug!(
                "NvmeBlockDev worker ring for {:?}: {} ({} entries)",
                device_path,
                flags,
                1024
            );
            r
        }
        Err(e) => {
            log::error!("Failed to initialize worker io_uring: {:?}", e);
            return;
        }
    };

    // P2-8: register the block device as a fixed file so hot SQEs can use
    // Fixed(0) and avoid per-op fd lookup overhead. Fall back to plain Fd(fd)
    // if the kernel rejects registration.
    let use_fixed = match ring.submitter().register_files(&[fd]) {
        Ok(()) => {
            log::debug!(
                "NvmeBlockDev: registered fixed file for {:?} (index 0)",
                device_path
            );
            true
        }
        Err(e) => {
            log::debug!(
                "NvmeBlockDev: fixed-file register failed for {:?}: {:?} (using raw Fd)",
                device_path,
                e
            );
            false
        }
    };

    // PERF-4 (b): register the ranged read-bounce slab as ONE fixed
    // buffer (index 0), so slab-resident cold-fill DMAs ride ReadFixed
    // and skip the per-op page pin (gup) every unregistered DMA pays —
    // the tree's first `register_buffers` user. Same probe-fallback
    // pattern as the fixed-file registration above: a refusal (memlock
    // caps, old kernel) degrades to plain Read SQEs. The slab is
    // process-static pool memory, and registration is what commits/pins
    // its pages (once per worker ring; the pages themselves commit once).
    // The whole-block 4 MiB pool is deliberately NOT registered: pinning
    // cores×16×4 MiB (GiBs) trades real RAM for a pin cost already
    // amortized across 4 MiB DMAs.
    let slab_range = crate::cache::pool::RANGED_BUF_POOL.slab_range();
    let use_fixed_buf = match slab_range {
        Some((base, len)) => {
            let iov = libc::iovec {
                iov_base: base as *mut libc::c_void,
                iov_len: len,
            };
            // SAFETY: the iovec covers the RANGED_BUF_POOL slab — a
            // process-static allocation that outlives every ring.
            match unsafe { ring.submitter().register_buffers(&[iov]) } {
                Ok(()) => {
                    log::debug!(
                        "NvmeBlockDev: registered read-bounce slab ({} MiB) as fixed buffer 0",
                        len >> 20
                    );
                    true
                }
                Err(e) => {
                    log::debug!(
                        "NvmeBlockDev: fixed-buffer register failed for {:?}: {:?} \
                         (ranged reads use plain Read)",
                        device_path,
                        e
                    );
                    false
                }
            }
        }
        None => false,
    };

    struct ActiveReq {
        response: UringResponse,
        free_ptr: Option<(FreePtrKind, SendPtr)>,
        _keep_alive: Option<bytes::Bytes>,
        /// MEM-1: dest owner token — held while the SQE is in flight,
        /// dropped at CQE completion BEFORE the caller oneshot fires (so
        /// the happy path never pays a spurious commit park) and on every
        /// teardown path (drop = release; the owner's re-arm gate unparks).
        dest_token: Option<DestToken>,
    }

    /// Complete one in-flight request at CQE time. MEM-1 ordering: the
    /// dest owner token drops FIRST — the DMA is over, so the
    /// destination's owner (the transport ent re-arm gate) is released
    /// before the caller oneshot can possibly produce a reply; the happy
    /// path therefore never pays a spurious commit park. The write
    /// watermark bumps BEFORE the caller oneshot for the same reason a
    /// barrier drain keyed on it must not depend on caller scheduling.
    fn complete_one(act: ActiveReq, io_res: std::io::Result<usize>, wm: &WriteWatermark) {
        let ActiveReq {
            response,
            free_ptr,
            _keep_alive: keep_alive,
            dest_token,
        } = act;
        drop(dest_token);
        match response {
            UringResponse::Read {
                bytes,
                size,
                offset,
                tx,
                submitted,
            } => {
                crate::fuse_client::read_fill_phase_record(
                    crate::fuse_client::ReadFillPhase::DevService,
                    submitted,
                );
                let _ = tx.send(finish_read(io_res, bytes, size, offset));
            }
            UringResponse::Write { tx } => {
                // DUR-2 watermark: writes AND fsyncs ride
                // `UringResponse::Write`, and both count symmetrically at
                // submit, so the ledger stays exact (an asymmetric count
                // would let an unrelated fsync completion "cover" a
                // pending write in a barrier drain).
                wm.note_completed();
                let mapped = io_res.map(|_| ()).map_err(crate::error::SqueezefsError::Io);
                let _ = tx.send(mapped);
            }
        }
        if let Some((kind, p)) = free_ptr {
            release_free_ptr(kind, p);
        }
        drop(keep_alive);
    }

    let mut active: Vec<Option<ActiveReq>> = Vec::with_capacity(1024);
    let mut free_slots: Vec<usize> = Vec::new();
    let mut active_count = 0;
    let mut disconnected = false;

    loop {
        loop {
            let req = if active_count == 0 {
                match rx.recv() {
                    Ok(r) => Some(r),
                    Err(_) => {
                        disconnected = true;
                        None
                    }
                }
            } else {
                match rx.try_recv() {
                    Ok(r) => Some(r),
                    Err(crossbeam::channel::TryRecvError::Empty) => None,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        None
                    }
                }
            };

            let req = match req {
                Some(r) => r,
                None => break,
            };

            let slot_idx = match free_slots.pop() {
                Some(idx) => {
                    active[idx] = None;
                    idx
                }
                None => {
                    let idx = active.len();
                    active.push(None);
                    idx
                }
            };

            let sqe = match req {
                UringRequest::Read {
                    offset,
                    buf_ptr,
                    size,
                    bytes,
                    tx,
                    enq,
                    dest_token,
                } => {
                    // Test seam: deterministic device-order stall (MEM-1
                    // repro) — the request already owns its dest token, so
                    // the ownership property under test spans this window
                    // exactly like an in-flight DMA.
                    loop {
                        let cur = STALL_NEXT_READS.load(std::sync::atomic::Ordering::SeqCst);
                        if cur == 0 {
                            break;
                        }
                        if STALL_NEXT_READS
                            .compare_exchange(
                                cur,
                                cur - 1,
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                            )
                            .is_ok()
                        {
                            let ms = STALL_READ_MS.load(std::sync::atomic::Ordering::SeqCst);
                            if ms > 0 {
                                std::thread::sleep(std::time::Duration::from_millis(ms));
                            }
                            break;
                        }
                    }
                    // read_fill_phase_ns: `dev_queue` = enqueue → SQE
                    // build (channel + slot wait); `dev_service` starts
                    // here and records at CQE completion.
                    crate::fuse_client::read_fill_phase_record(
                        crate::fuse_client::ReadFillPhase::DevQueue,
                        enq,
                    );
                    active[slot_idx] = Some(ActiveReq {
                        response: UringResponse::Read {
                            bytes,
                            size,
                            offset,
                            tx,
                            submitted: std::time::Instant::now(),
                        },
                        free_ptr: None,
                        _keep_alive: None,
                        dest_token,
                    });
                    // PERF-4 (b): slab-resident bounces ride the
                    // registered fixed buffer (no per-op page pin); dest
                    // and fresh-backing reads stay on plain Read.
                    let in_slab = use_fixed_buf
                        && slab_range.is_some_and(|(base, len)| {
                            let p = buf_ptr.0 as usize;
                            p >= base && p + size <= base + len
                        });
                    match (in_slab, use_fixed) {
                        (true, true) => {
                            opcode::ReadFixed::new(types::Fixed(0), buf_ptr.0, size as _, 0)
                                .offset(offset)
                                .build()
                                .user_data(slot_idx as u64)
                        }
                        (true, false) => opcode::ReadFixed::new(Fd(fd), buf_ptr.0, size as _, 0)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64),
                        (false, true) => opcode::Read::new(types::Fixed(0), buf_ptr.0, size as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64),
                        (false, false) => opcode::Read::new(Fd(fd), buf_ptr.0, size as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64),
                    }
                }
                UringRequest::Write { offset, data, tx } => {
                    // Test seam: deterministic slow-DMA stall (the read
                    // seam's twin) — the write is already accounted
                    // submitted on its lane's watermark, so the DUR-2
                    // cross-lane drain property under test spans this
                    // window exactly like an in-flight fabric DMA.
                    loop {
                        let cur = STALL_NEXT_WRITES.load(std::sync::atomic::Ordering::SeqCst);
                        if cur == 0 {
                            break;
                        }
                        if STALL_NEXT_WRITES
                            .compare_exchange(
                                cur,
                                cur - 1,
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                            )
                            .is_ok()
                        {
                            let ms = STALL_WRITE_MS.load(std::sync::atomic::Ordering::SeqCst);
                            if ms > 0 {
                                std::thread::sleep(std::time::Duration::from_millis(ms));
                            }
                            break;
                        }
                    }
                    let (ptr, len, free_ptr, keep_alive) = match data {
                        WriteData::Aligned { data } => {
                            (data.as_ptr(), data.len(), None, Some(data))
                        }
                        WriteData::Unaligned { ptr, len } => (
                            ptr.0 as *const u8,
                            len,
                            Some((FreePtrKind::Libc, ptr)),
                            None,
                        ),
                        WriteData::PooledUnaligned { ptr, len } => (
                            ptr.0 as *const u8,
                            len,
                            Some((FreePtrKind::Pool, ptr)),
                            None,
                        ),
                    };
                    // TEST-1 fault seam: journal the prior bytes of this
                    // write BEFORE the SQE goes out (armed only — one
                    // relaxed load otherwise).
                    crate::dev_power_cut::note_write(&device_path, offset, len);
                    active[slot_idx] = Some(ActiveReq {
                        response: UringResponse::Write { tx },
                        free_ptr,
                        _keep_alive: keep_alive,
                        dest_token: None,
                    });
                    if use_fixed {
                        opcode::Write::new(types::Fixed(0), ptr, len as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    } else {
                        opcode::Write::new(Fd(fd), ptr, len as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    }
                }
                // DUR-2 barrier. Its completion is a plain unit result,
                // so it rides `UringResponse::Write` (no third response
                // variant to fan out through every drain site); the
                // TEST-1 coverage accounting rides the caller
                // (`NvmeBlockDev::flush`), which knows when the barrier
                // started.
                UringRequest::Fsync { datasync, tx } => {
                    active[slot_idx] = Some(ActiveReq {
                        response: UringResponse::Write { tx },
                        free_ptr: None,
                        _keep_alive: None,
                        // MEM-1: a barrier carries no payload destination,
                        // so it owns no §5.4 lease token.
                        dest_token: None,
                    });
                    let flags = if datasync {
                        types::FsyncFlags::DATASYNC
                    } else {
                        types::FsyncFlags::empty()
                    };
                    if use_fixed {
                        opcode::Fsync::new(types::Fixed(0))
                            .flags(flags)
                            .build()
                            .user_data(slot_idx as u64)
                    } else {
                        opcode::Fsync::new(Fd(fd))
                            .flags(flags)
                            .build()
                            .user_data(slot_idx as u64)
                    }
                }
            };

            let mut pushed_sqe = false;
            for _retry in 0..3 {
                // SAFETY: this worker owns its ring exclusively (one submitter per
                // `NvmeBlockDev` worker thread), so SQ access is unaliased; the SQE
                // borrows only buffers whose owner tokens outlive the CQE (MEM-1).
                unsafe {
                    if let Ok(()) = ring.submission().push(&sqe) {
                        pushed_sqe = true;
                        break;
                    }
                }

                // Push failed: the submission ring is full of this pass's
                // not-yet-submitted SQEs. ONE enter both flushes them and
                // waits for ≥ 1 completion (PERF-4 (c) — and the only
                // DEFER_TASKRUN-correct shape: a plain submit() runs no
                // completion task-work, so the old submit-then-peek pass
                // drained nothing on deferred rings).
                if let Err(e) = ring.submit_and_wait(1) {
                    log::error!(
                        "Uring worker: submit_and_wait(1) failed on SQ full: {:?}",
                        e
                    );
                }
                let mut cq = ring.completion();
                cq.sync();
                let mut completed_slots = Vec::new();
                for cqe in cq {
                    let slot_idx = cqe.user_data() as usize;
                    let res = cqe.result();

                    if let Some(act) = active[slot_idx].take() {
                        let io_res = if res < 0 {
                            Err(std::io::Error::from_raw_os_error(-res))
                        } else {
                            Ok(res as usize)
                        };
                        complete_one(act, io_res, wm);
                    }
                    completed_slots.push(slot_idx);
                }

                for idx in completed_slots {
                    free_slots.push(idx);
                    active_count -= 1;
                }
            }

            if !pushed_sqe {
                log::error!("Uring request queue full or closed (backpressure)");
                if let Some(act) = active[slot_idx].take() {
                    match act.response {
                        UringResponse::Read { tx, .. } => {
                            let _ = tx.send(Err(crate::error::SqueezefsError::Io(
                                std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    "Submission queue full",
                                ),
                            )));
                        }
                        UringResponse::Write { tx } => {
                            // DUR-2 watermark: a refusal is a terminal
                            // resolution.
                            wm.note_completed();
                            let _ = tx.send(Err(crate::error::SqueezefsError::Io(
                                std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    "Submission queue full",
                                ),
                            )));
                        }
                    }
                    if let Some((kind, p)) = act.free_ptr {
                        release_free_ptr(kind, p);
                    }
                }
                free_slots.push(slot_idx);
                break;
            }

            active_count += 1;
        }

        // PERF-4 (c): ONE io_uring_enter per pass — submit_and_wait both
        // flushes every SQE this pass pushed and parks for ≥ 1 completion
        // (the old shape paid submit() + submit_and_wait(1) = two enters).
        // A successful push always increments active_count, so pushed > 0
        // ⇒ active_count > 0 and nothing is ever left unflushed here.
        if active_count > 0 {
            if let Err(e) = ring.submit_and_wait(1) {
                log::error!("io_uring submit_and_wait failed: {:?}", e);
            }

            let mut cq = ring.completion();
            cq.sync();

            let mut completed_slots = Vec::new();
            for cqe in cq {
                let slot_idx = cqe.user_data() as usize;
                let res = cqe.result();

                if let Some(act) = active[slot_idx].take() {
                    let io_res = if res < 0 {
                        Err(std::io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };
                    complete_one(act, io_res, wm);
                }

                completed_slots.push(slot_idx);
            }

            for idx in completed_slots {
                free_slots.push(idx);
                active_count -= 1;
            }
        }

        if disconnected && active_count == 0 {
            break;
        }
    }

    // P0-1: Worker exit cleanup — free any unaligned write buffers still held
    // in-flight or left on the channel, and fail pending oneshots so callers
    // do not hang after NvmeBlockDev drop.
    while let Ok(req) = rx.try_recv() {
        match req {
            UringRequest::Write { data, tx, .. } => {
                release_write_buf(&data);
                // DUR-2 watermark: teardown is a terminal resolution.
                wm.note_completed();
                let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shutting down".to_string(),
                )));
            }
            UringRequest::Read { tx, .. } => {
                let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shutting down".to_string(),
                )));
            }
            UringRequest::Fsync { tx, .. } => {
                wm.note_completed();
                let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shutting down".to_string(),
                )));
            }
        }
    }

    for slot in active.iter_mut() {
        if let Some(act) = slot.take() {
            // MEM-1: `act.dest_token` drops with `act` at scope end —
            // worker teardown releases every held destination (the
            // owner's re-arm gate unparks; a genuinely-wedged SQE never
            // reaches here because the loop drains active_count to 0
            // before exiting).
            let free_ptr = act.free_ptr;
            match act.response {
                UringResponse::Read { tx, .. } => {
                    let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                        "NvmeBlockDev worker shutting down".to_string(),
                    )));
                }
                UringResponse::Write { tx } => {
                    // DUR-2 watermark: teardown is a terminal resolution.
                    wm.note_completed();
                    let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                        "NvmeBlockDev worker shutting down".to_string(),
                    )));
                }
            }
            if let Some((kind, p)) = free_ptr {
                release_free_ptr(kind, p);
            }
        }
    }
}

/// TTL of the cached device-node liveness probe (see
/// [`NvmeBlockDev::node_exists_cached`]): a vanished node is detected
/// within this window; a per-call probe is the 0.76 statx/op L3
/// transport-economy regression.
pub const NODE_PROBE_TTL_MS: u64 = 1000;

/// TTL-cached device-node liveness state, shared across [`NvmeBlockDev`]
/// clones (they describe the same node, so they share one probe).
struct NodeProbe {
    /// Coarse monotonic ms of the last probe (0 = never probed).
    at_ms: std::sync::atomic::AtomicU64,
    /// Last probe outcome (valid while fresh).
    seen: std::sync::atomic::AtomicBool,
}

/// RES-6 (pre-RC engineering spec §7): the DATA plane's D0 writer-guard
/// fence.
///
/// The reclaim queue already observes the mount's `failed` latch so a
/// fenced zombie can never issue another destructive discard
/// (`block_reclaim::ReclaimQueue::fence_halted`). This is the same
/// predicate for DMA SUBMISSION: NVMe reservations cover metadata
/// volumes only, so on a detection-grade substrate a fenced holder's
/// writes would otherwise keep landing on offsets the successor writer
/// has replayed and reallocated.
///
/// Sticky by construction — a fenced holder is dead until remount, so
/// the steady-state cost is exactly one relaxed load per submit.
pub(crate) struct DeviceFence {
    signal: std::sync::OnceLock<Arc<dyn Fn() -> bool + Send + Sync>>,
    halted: std::sync::atomic::AtomicBool,
}

impl DeviceFence {
    fn new() -> Self {
        Self {
            signal: std::sync::OnceLock::new(),
            halted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `true` ⇔ writes are (now) fence-halted. One relaxed load once
    /// latched; one cheap probe (a few atomic loads over the meta set)
    /// otherwise. Latches sticky and loud on the first observation.
    #[inline]
    fn halted(&self, device_path: &str) -> bool {
        if self.halted.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        let Some(sig) = self.signal.get() else {
            return false;
        };
        if sig() {
            if !self.halted.swap(true, std::sync::atomic::Ordering::AcqRel) {
                log::error!(
                    "data volume {device_path}: writer guard FENCED / volume fail-stopped \
                     — refusing ALL further data-plane DMA permanently (a fenced zombie's \
                     write can land on offsets the successor writer has replayed and \
                     reallocated). Counted in data_dma_fence_refusals; remount required"
                );
            }
            return true;
        }
        false
    }
}

#[derive(Clone)]
pub struct NvmeBlockDev {
    pub device_path: String,
    /// Lane 0: barriers and probes always; writes when un-armed
    /// (`write_lanes == 1`, every offline tool/test); reads when the
    /// pool is 1.
    worker: Arc<UringWorker>,
    /// I/O submission fan-out pool (read-queue-wall 2026-08-06 +
    /// write-lane-fanout 2026-08-07): full [`UringWorker`]s on the same
    /// device (own thread, own ring, own O_DIRECT fd), so their
    /// submissions map to DIFFERENT blk-mq software queues (per-CPU) and
    /// therefore different fabric queues/connections — see
    /// [`io_lanes_for`]. Reads round-robin the first `read_lanes` lanes;
    /// writes offset-affinity over the first `write_lanes` lanes. Always
    /// contains lane 0; grows via [`Self::set_read_lanes`] /
    /// [`Self::set_write_lanes`].
    lane_pool: Arc<arc_swap::ArcSwap<Vec<Arc<UringWorker>>>>,
    lane_rr: Arc<std::sync::atomic::AtomicUsize>,
    /// Reads round-robin the first `read_lanes` pool lanes (monotone;
    /// 1 = the prior single-worker posture).
    read_lanes: Arc<std::sync::atomic::AtomicUsize>,
    /// Writes spread by BLOCK-OFFSET AFFINITY over the first
    /// `write_lanes` pool lanes (monotone; 1 = the pre-fanout posture —
    /// every write on lane 0). Affinity, never round-robin: same block →
    /// same lane → same channel FIFO → same connection, so two DMAs
    /// naming one device offset keep exactly the per-offset submission
    /// order the single worker gave (see [`write_lane_index`]).
    write_lanes: Arc<std::sync::atomic::AtomicUsize>,
    /// The write-affinity grain = the volume block size, handed by the
    /// arming site ([`Self::set_write_lanes`], re-stamped by
    /// `DataRouter::set_block_size` at FUSE init) — a derivation from
    /// the volume format, never a constant. Never read while
    /// `write_lanes == 1`, and affinity is a structural-preservation
    /// property, not a correctness dependency (per-offset serialization
    /// lives above the device layer), so the init-time re-stamp is safe.
    write_grain: Arc<std::sync::atomic::AtomicU64>,
    node_probe: Arc<NodeProbe>,
    /// RES-6: the D0 writer-guard fence, shared across clones (they
    /// describe the same device). Wired by `DataRouter::set_meta_backend`
    /// with the same probe the reclaim queue uses; bare devices (tests,
    /// offline tools) have no probe and never halt.
    fence: Arc<DeviceFence>,
    /// DUR-2: per-device barrier coalescer — many concurrent `fsync`s on
    /// one device need one device flush each in principle, and one
    /// `Fsync` op satisfies every caller whose write completed before it
    /// started. Reuses the metadata plane's `SyncCoalescer` discipline
    /// verbatim (registration atomic with the flushing flag; no lost
    /// wakeup). Shared across clones — they describe the same device.
    sync: Arc<crate::meta_backend::sync_coalescer::SyncCoalescer>,
    /// Volatile-write-cache classification probed once at construction
    /// (`data_volume_write_cache` on the stats inode).
    write_cache: crate::write_cache::WriteCacheClass,
    /// zcrx read lane session (docs/design-zcrx-read-lane.md §6): armed
    /// lazily on the first eligible read (`SQUEEZEFS_ZCRX_LANE=1` +
    /// capability probes), `None` cached on any arm refusal — the kernel
    /// path stays byte-identical. Shared across clones (one association
    /// per device node).
    lane: Arc<tokio::sync::OnceCell<Option<Arc<crate::zcrx_lane::LaneSession>>>>,
    /// FUSE-zc direct-leg read fd (K1 kill, 2026-08-06): a dedicated
    /// `O_DIRECT` read-only fd on the device, opened lazily at the first
    /// zc resolve and held for the mount's life — the fuse3 queue ring
    /// issues `READ_FIXED` against it (device DMA into the request's
    /// pages via the sparse slot). Its own fd by design: zc fetches ride
    /// a FOREIGN ring, so sharing a worker's fd would only couple
    /// lifetimes, not queues. `None` cached on open failure (the zc leg
    /// falls through to the ordinary ladder). Shared across clones.
    zc_fd: Arc<std::sync::OnceLock<Option<std::os::fd::OwnedFd>>>,
    /// The D14 write-side direct-leg fd (`O_DIRECT | O_WRONLY`), opened
    /// lazily like [`Self::zc_fd`]; `None` on refusal — the direct
    /// store leg falls back to the extraction vehicle, loudly once.
    zc_wfd: Arc<std::sync::OnceLock<Option<std::os::fd::OwnedFd>>>,
}

/// Capacity in bytes of a backing file OR block device (seek-to-end works
/// for both; `metadata.len()` is 0 for block devices).
pub fn device_capacity_bytes(path: &str) -> std::io::Result<u64> {
    use std::io::Seek;
    let mut f = std::fs::File::open(path)?;
    f.seek(std::io::SeekFrom::End(0))
}

/// DUR-2 decision (a): prove the substrate serves `O_DIRECT` before a
/// mount commits to it. Buffered device I/O is not a supported
/// data-plane mode — the kernel would own dirty pages the daemon never
/// flushes — so this refusal is loud and terminal, never a degrade.
pub fn probe_direct_io(device_path: &str) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_DIRECT);
    }
    opts.open(device_path).map(|_| ()).map_err(|e| {
        crate::error::SqueezefsError::InvalidOperation(format!(
            "data volume {device_path} cannot be opened with O_DIRECT ({e}); buffered \
             device I/O is not a supported data-plane mode (DUR-2: acknowledged writes \
             would carry no durability barrier). Use a substrate that supports direct I/O."
        ))
    })
}

/// Coarse monotonic milliseconds since process start (probe-TTL clock).
fn coarse_monotonic_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

/// I/O-lane fan-out derivation — ONE term for both planes: "submitting
/// CPUs per device".
///
/// Reads (2026-08-06 read-queue-wall campaign — the flat field D-ladder's
/// device-plane term): ONE submitting thread per device is ONE blk-mq
/// software queue is ONE nvme-tcp connection is ONE RX softirq core, and
/// nvme-tcp RX pays the per-byte kernel copy on that core (~2.7
/// GB/s/core measured class) — 8 field namespaces × one core ≈ the 22
/// GB/s read wall, invariant to any client-side depth (local
/// written-region discriminator: 1 submitter/dev qd16 → qd32 = 11.5 →
/// 12.1 GB/s, while 4 submitters/dev at the SAME in-flight = 16.7).
///
/// Writes (2026-08-07 write-lane-fanout campaign): the read campaign's
/// "writes escape it" held only up to the TX splice's HIGHER
/// per-connection ceiling — the field decomposition walls kernel seq
/// writes at 35.31 GB/s = 7.06 GB/s × 5 data namespaces
/// (`write_pipeline_phase_ns` dma mode 2–8 ms, interior µs-clean; the
/// depth governor pinned at base with probe_ups 20 = backoffs 20 — depth
/// into a per-connection wall converts nothing) while raw fio reaches
/// 49.7 GB/s on the SAME namespaces by spreading queues. So DATA WRITES
/// fan out with the SAME derivation, by block-offset affinity (see
/// [`write_lane_index`]).
///
/// The count derives — never a constant: `cpus / data_devices`, floor 1
/// (a device never loses its worker), self-bounded by `cpus` (total
/// submitting threads per plane ≈ the machine, the raw row's own shape
/// when devices = 1). Explicit `SQUEEZEFS_NVME_READ_LANES` /
/// `SQUEEZEFS_NVME_WRITE_LANES` win verbatim at the arming site.
pub fn io_lanes_for(cpus: usize, data_devices: usize) -> usize {
    (cpus / data_devices.max(1)).clamp(1, cpus.max(1))
}

/// The write-lane pick — a PURE function of the block offset:
/// `(offset / grain) % lanes`, grain = the volume block size. Same block
/// → same lane → same channel FIFO → same submission queue/connection,
/// so every pair of DMAs naming one device offset (supersession /
/// in-place-overwrite / W1 sub-block patch shapes) keeps the per-offset
/// SUBMISSION order the single worker provided — structurally, not by
/// caller discipline. (Completion order was never guaranteed even on
/// one lane — no `IO_LINK`/`IO_DRAIN`, NVMe completes out of order —
/// which is why per-offset serialization lives above the device layer:
/// `BLOCK_FLUSH_LOCKS`, awaited DMAs before dependent publishes, the W1
/// §5.1 fence. The affinity preserves the status quo and adds no new
/// reordering surface.) Degenerate inputs floor at lane 0.
pub fn write_lane_index(offset: u64, grain: u64, lanes: usize) -> usize {
    ((offset / grain.max(1)) % lanes.max(1) as u64) as usize
}

impl NvmeBlockDev {
    pub fn new(device_path: &str) -> Self {
        let worker = Arc::new(UringWorker::new(device_path.to_string()));
        Self {
            device_path: device_path.to_string(),
            lane_pool: Arc::new(arc_swap::ArcSwap::from_pointee(vec![worker.clone()])),
            lane_rr: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            read_lanes: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            write_lanes: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            write_grain: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            worker,
            node_probe: Arc::new(NodeProbe {
                at_ms: std::sync::atomic::AtomicU64::new(0),
                seen: std::sync::atomic::AtomicBool::new(false),
            }),
            sync: Arc::new(crate::meta_backend::sync_coalescer::SyncCoalescer::new()),
            // One-shot sysfs read at construction (never on a cadence or
            // a per-op path — the derived-defaults law).
            write_cache: crate::write_cache::probe_data_volume(std::path::Path::new(device_path)),
            lane: Arc::new(tokio::sync::OnceCell::new()),
            fence: Arc::new(DeviceFence::new()),
            zc_fd: Arc::new(std::sync::OnceLock::new()),
            zc_wfd: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// The FUSE-zc direct-leg read fd (see the field doc): opened lazily
    /// (`O_DIRECT | O_RDONLY`, mirroring the worker's alignment
    /// contract), `None` on refusal — the caller's zc leg falls through
    /// to the ordinary read ladder, loudly once.
    pub fn zc_read_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::fd::AsRawFd;
        self.zc_fd
            .get_or_init(|| {
                let mut opts = std::fs::OpenOptions::new();
                opts.read(true);
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.custom_flags(libc::O_DIRECT);
                }
                match opts.open(&self.device_path) {
                    Ok(f) => Some(std::os::fd::OwnedFd::from(f)),
                    Err(e) => {
                        log::warn!(
                            "zc read fd open failed for {:?} ({e}) — the FUSE-zc direct \
                             leg stays cold on this device (ordinary ladder serves)",
                            self.device_path
                        );
                        None
                    }
                }
            })
            .as_ref()
            .map(|fd| fd.as_raw_fd())
    }

    /// The D14 write-side direct-leg fd (`O_DIRECT | O_WRONLY`, opened
    /// lazily like [`Self::zc_read_fd`]): the transport ring's
    /// `WRITE_FIXED(device fd ← slot)` names it. `None` on refusal —
    /// the direct store leg falls back to the extraction vehicle,
    /// loudly once per device.
    pub fn zc_write_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::fd::AsRawFd;
        self.zc_wfd
            .get_or_init(|| {
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true);
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.custom_flags(libc::O_DIRECT);
                }
                match opts.open(&self.device_path) {
                    Ok(f) => Some(std::os::fd::OwnedFd::from(f)),
                    Err(e) => {
                        log::warn!(
                            "zc write fd open failed for {:?} ({e}) — the D14 direct \
                             store leg stays cold on this device (extraction serves)",
                            self.device_path
                        );
                        None
                    }
                }
            })
            .as_ref()
            .map(|fd| fd.as_raw_fd())
    }

    /// RES-6/S7 authorization for a D14 direct slot→device store: the
    /// transport ring's `WRITE_FIXED` bypasses [`Self::write_block`]'s
    /// worker, so the SAME submit gate must run here — the D0 latch
    /// probe (poisons process custody on first observation) followed by
    /// THE authorization point (`data_custody::authorize_dma`, which
    /// owns the refusal/classification/counting). Called strictly
    /// before every direct store submission.
    pub fn authorize_zc_store(&self) -> Result<()> {
        self.fence_gate(None)
    }

    /// Grow the shared submission pool to at least `n` lanes. Lane 0 is
    /// the construction worker; extra lanes are full [`UringWorker`]s on
    /// the same device (own thread, own ring, own O_DIRECT fd), so their
    /// submissions map to DIFFERENT blk-mq software queues (per-CPU) and
    /// therefore different fabric queues/RX cores. Monotone: the pool
    /// only grows (shrink = the A/B lever's remount); idempotent and
    /// latch-free (ArcSwap — in-flight requests keep their lane alive by
    /// Arc; a lost CAS race drops only freshly-spawned idle workers).
    fn grow_pool(&self, n: usize) {
        loop {
            let cur = self.lane_pool.load_full();
            if cur.len() >= n {
                return;
            }
            let mut next = (*cur).clone();
            while next.len() < n {
                next.push(Arc::new(UringWorker::new(self.device_path.clone())));
            }
            let prev = self.lane_pool.compare_and_swap(&cur, Arc::new(next));
            if Arc::ptr_eq(&prev, &cur) {
                return;
            }
        }
    }

    /// Arm `n` READ submission lanes (see [`io_lanes_for`] — the
    /// read-queue-wall campaign). Reads round-robin the first `n` pool
    /// lanes; monotone.
    pub fn set_read_lanes(&self, n: usize) {
        let n = n.max(1);
        self.grow_pool(n);
        let prev = self
            .read_lanes
            .fetch_max(n, std::sync::atomic::Ordering::Relaxed);
        if n > prev {
            log::info!(
                "NvmeBlockDev {}: read submission fan-out armed ({} lanes)",
                self.device_path,
                n
            );
        }
    }

    /// Arm `n` DATA-WRITE submission lanes with affinity `grain` = the
    /// volume block size (see [`io_lanes_for`] + [`write_lane_index`] —
    /// the write-lane-fanout campaign, 2026-08-07). Writes spread by
    /// block-offset affinity over the first `n` pool lanes; barriers and
    /// probes keep lane 0, and [`Self::flush`] drains EVERY lane's write
    /// watermark before the device Fsync issues. Monotone lane count;
    /// the grain re-stamps freely (it is a structural-affinity term, not
    /// a correctness dependency — see the field doc).
    pub fn set_write_lanes(&self, n: usize, grain: u64) {
        let n = n.max(1);
        self.grow_pool(n);
        self.write_grain
            .store(grain.max(1), std::sync::atomic::Ordering::Relaxed);
        let prev = self
            .write_lanes
            .fetch_max(n, std::sync::atomic::Ordering::Relaxed);
        if n > prev {
            log::info!(
                "NvmeBlockDev {}: write submission fan-out armed ({} lanes, {} B affinity grain)",
                self.device_path,
                n,
                grain.max(1)
            );
        }
    }

    /// Re-stamp the write-affinity grain (the `DataRouter::set_block_size`
    /// hook: the mount arms lanes before FUSE init reads the configured
    /// block size).
    pub fn set_write_lane_grain(&self, grain: u64) {
        self.write_grain
            .store(grain.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// Live read-lane count (`data_read_lanes` stats face; 1 = the prior
    /// single-worker posture).
    pub fn read_lanes(&self) -> usize {
        self.read_lanes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Live write-lane count (`data_write_lanes` stats face; 1 = the
    /// pre-fanout posture — every write on lane 0).
    pub fn write_lanes(&self) -> usize {
        self.write_lanes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Per-pool-lane `(writes_submitted, writes_completed)` watermarks —
    /// the lane-spread ENGAGEMENT instrument (`data_write_lane_submits`
    /// on the stats inode: a fan-out row is invalid unless > 1 lane's
    /// submit counter moved) and the flush-drain contract's test face.
    pub fn write_lane_watermarks(&self) -> Vec<(u64, u64)> {
        self.lane_pool
            .load()
            .iter()
            .map(|w| {
                (
                    w.wm.submitted.load(std::sync::atomic::Ordering::SeqCst),
                    w.wm.completed.load(std::sync::atomic::Ordering::SeqCst),
                )
            })
            .collect()
    }

    /// Round-robin pick for one READ submission (lock-free; the returned
    /// Arc keeps the lane alive across the request's lifetime).
    fn read_lane_pick(&self) -> Arc<UringWorker> {
        let pool = self.lane_pool.load();
        let lanes = self
            .read_lanes
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(pool.len())
            .max(1);
        let idx = self
            .lane_rr
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % lanes;
        pool[idx].clone()
    }

    /// Block-offset-affinity pick for one WRITE submission (lock-free;
    /// see [`write_lane_index`]). `write_lanes == 1` (every un-armed
    /// device) short-circuits to the construction worker — byte-for-byte
    /// the pre-fanout posture.
    fn write_lane_pick(&self, offset: u64) -> Arc<UringWorker> {
        let lanes = self.write_lanes.load(std::sync::atomic::Ordering::Relaxed);
        if lanes <= 1 {
            return self.worker.clone();
        }
        let pool = self.lane_pool.load();
        let lanes = lanes.min(pool.len()).max(1);
        let grain = self.write_grain.load(std::sync::atomic::Ordering::Relaxed);
        pool[write_lane_index(offset, grain, lanes)].clone()
    }

    /// RES-6: wire the D0 writer-guard fence probe (the SAME per-volume
    /// `failed` latch `set_reclaim_fence_signal` gives the reclaim
    /// queue). Set once at meta-backend wiring; later calls are no-ops.
    pub fn set_fence_signal(&self, sig: Arc<dyn Fn() -> bool + Send + Sync>) {
        let _ = self.fence.signal.set(sig);
    }

    /// `true` ⇔ this device's data plane is fence-halted (stats /
    /// tests). Sticky: a fenced holder is dead until remount.
    pub fn fenced(&self) -> bool {
        self.fence.halted(&self.device_path)
    }

    /// RES-6 + DLM **S7** submit gate: the D0 latch probe (which poisons
    /// process custody on its first observation) followed by THE
    /// authorization point, [`crate::data_custody::authorize_dma`] — which
    /// owns the refusal, the classification and the counting so there is
    /// exactly one place that decides whether a DMA may be submitted.
    ///
    /// `carried` is the epoch the submission was authorized under
    /// ([`Self::write_block_authorized`]); `None` = authorize at submit
    /// (every pre-S7 call site).
    ///
    /// Reads are deliberately NOT gated — a fenced holder reading its own
    /// device corrupts nothing, and refusing would turn a fail-stop into a
    /// hang.
    #[inline]
    fn fence_gate(&self, carried: Option<crate::data_custody::CustodyEpoch>) -> Result<()> {
        // One relaxed load once latched; the cheap probe otherwise. A
        // latched device retires the process's data-plane custody: the D0
        // fail-stop lattice is MOUNT-wide (the reclaim queue already
        // ceases EVERY device's reclaims on the first observation), so a
        // sibling volume must not have to evaluate the same probe before
        // it stops writing, and every authorization minted before this
        // instant is void. Idempotent — poisoning is a one-way latch.
        if self.fence.halted(&self.device_path) {
            crate::data_custody::poison(&format!(
                "data volume {} writer guard fenced",
                self.device_path
            ));
        }
        crate::data_custody::authorize_dma(carried).map(|_| ())
    }

    /// Mount-path constructor (DUR-2 decision (a)): probe `O_DIRECT`
    /// FIRST and refuse loudly if the substrate cannot serve it, then
    /// log the volatile-write-cache classification. The worker refuses
    /// buffered mode too, but a mount must fail at setup with a
    /// diagnosable error rather than at its first I/O.
    pub fn open_checked(device_path: &str) -> Result<Self> {
        probe_direct_io(device_path)?;
        let dev = Self::new(device_path);
        log::info!(
            "data volume {}: write_cache={} (durability barrier: io_uring \
             Fsync/DATASYNC per device, coalesced)",
            device_path,
            dev.write_cache.as_str()
        );
        Ok(dev)
    }

    /// The device's probed volatile-write-cache class
    /// (`data_volume_write_cache`).
    pub fn write_cache(&self) -> crate::write_cache::WriteCacheClass {
        self.write_cache
    }

    /// **DUR-2 — the data-device durability barrier.** Completes only
    /// once the device reports its volatile write cache flushed for
    /// everything written before the barrier started, so a caller that
    /// awaited its DMAs can name those blocks in durable metadata.
    ///
    /// Write-lane-fanout (2026-08-07): the barrier orders against ALL
    /// prior writes on EVERY lane, not just lane 0 — each barrier op
    /// first drains every lane's write watermark (writes submitted
    /// before the op started have completed at the device) and only then
    /// issues the ONE device Fsync on lane 0. A device flush command
    /// covers only commands the device completed before processing it,
    /// so without the drain a stalled DMA on another lane would silently
    /// escape the barrier. (Chosen over an `IO_DRAIN` Fsync fan-out:
    /// that would order behind in-flight READS sharing each ring — a
    /// wedged read's 30 s timeout class holding every fsync hostage —
    /// and pay N device flushes per barrier instead of one.)
    ///
    /// Concurrent callers coalesce through the metadata plane's
    /// [`crate::meta_backend::sync_coalescer::SyncCoalescer`] — reused,
    /// not re-derived: a caller is released only by an op that STARTED
    /// after it registered, which is exactly the group-commit safety
    /// property this path needs.
    pub async fn flush(&self) -> Result<()> {
        crate::fuse_client::METRICS
            .data_device_sync_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.sync
            .barrier(|| async {
                crate::fuse_client::METRICS
                    .data_device_syncs
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // TEST-1 barrier fault (armed only): the device accepts
                // I/O and rejects the flush — nothing becomes covered.
                if let Some(code) = crate::dev_power_cut::barrier_fault(&self.device_path) {
                    return Err(crate::error::SqueezefsError::Io(
                        std::io::Error::from_raw_os_error(code),
                    ));
                }
                // The cross-lane drain — BEFORE the coverage frontier and
                // the Fsync, so every prior write is at the device when
                // the flush command executes.
                self.drain_write_lanes().await?;
                // TEST-1 coverage frontier: everything journaled before
                // this op started is durable when it completes.
                let covered = crate::dev_power_cut::mark_barrier_start(&self.device_path);
                let (tx, rx) = oneshot::channel();
                // The Fsync rides lane 0's watermark symmetrically (see
                // `complete_one` — fsyncs and writes share the response
                // variant, so both must count or neither).
                self.worker.wm.note_submitted();
                self.worker
                    .sender()
                    .try_send(UringRequest::Fsync { datasync: true, tx })
                    .map_err(|e| {
                        self.worker.wm.note_completed();
                        crate::fuse_client::METRICS
                            .uring_queue_full
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        crate::error::SqueezefsError::InvalidOperation(format!(
                            "Uring request queue full or closed (data-device barrier): {:?}",
                            e
                        ))
                    })?;
                rx.await.map_err(|e| {
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "Worker thread closed receiver during data-device barrier: {:?}",
                        e
                    ))
                })??;
                crate::dev_power_cut::complete_barrier(&self.device_path, covered);
                Ok(())
            })
            .await
    }

    /// The DUR-2 cross-lane write drain: snapshot every lane's submitted
    /// watermark FIRST (the barrier covers writes submitted before it
    /// started; writes racing in behind the snapshot must not extend the
    /// wait), then park until each lane's completions reach its snapshot.
    /// Errors loud if a lane's worker died — a barrier fails, it never
    /// falsely completes.
    async fn drain_write_lanes(&self) -> Result<()> {
        let pool = self.lane_pool.load_full();
        let targets: Vec<(Arc<UringWorker>, u64)> = pool
            .iter()
            .map(|w| {
                (
                    w.clone(),
                    w.wm.submitted.load(std::sync::atomic::Ordering::SeqCst),
                )
            })
            .collect();
        for (w, target) in targets {
            w.wm.wait_completed(target).await?;
        }
        Ok(())
    }

    /// zcrx-lane read attempt (design §6). `Some(bytes)` = the lane served
    /// this read; `None` = ineligible / not armed / lane error — the caller
    /// proceeds on the kernel path unchanged (reads are idempotent, so the
    /// per-op fallback retry is safe by construction and counted in
    /// `zcrx_fill_fallbacks`, which must stay ≈ 0).
    ///
    /// Two serve shapes (PR Z3 — design §4.4/§10 gather fusion):
    /// * `dest_addr = Some`: the lane's ONE completion gather lands
    ///   DIRECTLY in the caller's registered destination (area-class
    ///   backends only — [`crate::zcrx_lane::LaneSession::read_into_dest`]),
    ///   deleting the Z2 intermediate copy on the raw full-block and R3
    ///   ranged zero-copy legs. Engagement gauge: `zcrx_dest_gather_bytes`.
    /// * `dest_addr = None`: pooled fill (the Z2 shape — memory that must
    ///   outlive the serve for tiers/holds pays the pooled gather).
    /// Contract seam (engagement round 2 — `tests/zcrx_lane_tests.rs`
    /// bypass-accounting suite): the lane session this device's funnel
    /// armed, if any. Read-only access; the OnceCell stays the owner.
    pub fn lane_session_for_test(&self) -> Option<Arc<crate::zcrx_lane::LaneSession>> {
        self.lane.get().and_then(|o| o.clone())
    }

    async fn try_lane_read(
        &self,
        offset: u64,
        size: usize,
        dest_addr: Option<u64>,
    ) -> Option<Result<bytes::Bytes>> {
        if !crate::zcrx_lane::lane_env_armed() {
            return None;
        }
        let sess = self
            .lane
            .get_or_init(|| crate::zcrx_lane::arm_for_device(&self.device_path))
            .await
            .as_ref()?;
        if sess.poisoned() || sess.refill_structural() {
            // FINDING F + round-8 FINDING J: a poisoned session — and a
            // session whose refill starvation proved STRUCTURAL
            // (prolonged-degraded ≡ poisoned in lifecycle terms) — must
            // release its ifqs/rules/RSS exclusion and arbiter lease
            // PROMPTLY: the no-harm law is worst-case lane-on ≈
            // lane-off, and the field showed a 25 % RSS-width hole
            // taxing the kernel path ~40 % at 0.08 % lane engagement.
            // Idempotent fire-and-forget; the kernel path serves.
            sess.spawn_teardown();
            return None;
        }
        if sess.refill_degraded() {
            // Round 5: a refill-starved queue must never hold reads
            // hostage — the lane declines while recovering; the kernel
            // path serves. COUNTED since engagement round 2: the
            // formerly-uncounted "ineligibility class" was the silent
            // volume gate that held two field campaigns' rows at ~0
            // engagement with no instrument naming it (round-1 tape:
            // ~1.5 M lease reads unaccounted at fill = 0.6 GB while the
            // park → failover → trickle cycle kept this latch set).
            crate::fuse_client::METRICS
                .zcrx_degraded_bypasses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        if !sess.range_eligible(offset, size) {
            return None;
        }
        if let Some(addr) = dest_addr {
            // Fused dest serve: area-class backends only (the classic
            // reader writes from a foreign task — the MEM-1 hazard class
            // for registered memory). Ineligibility is NOT a fallback:
            // the kernel path is simply the serve, uncounted.
            if !sess.dest_serve_eligible() {
                return None;
            }
            return match sess.read_into_dest(offset, addr as *mut u8, size).await {
                Ok(()) => {
                    let m = &crate::fuse_client::METRICS;
                    m.zcrx_fills
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    m.zcrx_fill_bytes
                        .fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
                    // Same shape as the kernel dest arm: the caller owns
                    // the pre-registered pinned destination.
                    // SAFETY: dest contract of read_block_with_dest.
                    let b = unsafe {
                        bytes::Bytes::from_static(std::slice::from_raw_parts(
                            addr as *const u8,
                            size,
                        ))
                    };
                    Some(Ok(b))
                }
                Err(crate::error::SqueezefsError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    // Round-8 admission decline: the lane is FULL, not
                    // broken — the kernel path serves (ineligibility
                    // class: uncounted as fallback, counted in
                    // zcrx_area_admission_waits at the decline site).
                    None
                }
                Err(e) => {
                    // Partial gathers are harmless: the kernel-path retry
                    // overwrites the whole destination (idempotent reads).
                    crate::fuse_client::METRICS
                        .zcrx_fill_fallbacks
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    log::warn!(
                        "zcrx-lane: dest read fell back to the kernel path \
                         (offset={offset}, size={size}): {e}"
                    );
                    None
                }
            };
        }
        let (buf_ptr, bytes) = crate::cache::pool::read_bounce_pool(size).alloc();
        // MEM-3 custody: the lane holds a clone of the pooled `Bytes`
        // until no lane context can write the destination — a cancelled
        // funnel future can never let the pool recycle a buffer the
        // classic reader task still holds a span pointer into.
        match sess
            .read_into_pooled(offset, buf_ptr, size, bytes.clone())
            .await
        {
            Ok(()) => {
                let m = &crate::fuse_client::METRICS;
                m.zcrx_fills
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                m.zcrx_fill_bytes
                    .fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
                Some(Ok(bytes.slice(0..size)))
            }
            Err(crate::error::SqueezefsError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // Round-8 admission decline (see the dest arm above).
                None
            }
            Err(e) => {
                // Dropping `bytes` recycles the pooled buffer; the lane's
                // custody law guarantees no writer touches it after the
                // op resolves (entries hold the keep-alive until the
                // driver is done; timeout paths abort+join first).
                crate::fuse_client::METRICS
                    .zcrx_fill_fallbacks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                log::warn!(
                    "zcrx-lane: read fell back to the kernel path \
                     (offset={offset}, size={size}): {e}"
                );
                None
            }
        }
    }

    /// TTL-cached device-node liveness (L3 statx residual: the per-ranged-
    /// read `Path::exists()` in `is_backend_healthy` was 0.76 statx/op on
    /// the charter workload). The probe is a liveness *hint* — the I/O
    /// path itself fails loud on a vanished device inside the TTL window —
    /// so a ≤ [`NODE_PROBE_TTL_MS`] detection delay trades nothing real.
    /// Racing expirers may both re-probe (idempotent statx on the same
    /// path); a torn seen/at pairing pairs two probes of the same node
    /// microseconds apart and self-heals within one TTL.
    pub fn node_exists_cached(&self) -> bool {
        use std::sync::atomic::Ordering;
        let now = coarse_monotonic_ms();
        let at = self.node_probe.at_ms.load(Ordering::Relaxed);
        if at != 0 && now.saturating_sub(at) < NODE_PROBE_TTL_MS {
            return self.node_probe.seen.load(Ordering::Relaxed);
        }
        let seen = std::path::Path::new(&self.device_path).exists();
        self.node_probe.seen.store(seen, Ordering::Relaxed);
        // `max(1)`: 0 is the never-probed sentinel; a probe inside the
        // process's first millisecond must still record as probed.
        self.node_probe.at_ms.store(now.max(1), Ordering::Relaxed);
        seen
    }

    /// Submit a block write under an epoch-bearing authorization (DLM
    /// **S7**): `auth` is the custody epoch the caller was authorized
    /// under when its custody was ESTABLISHED — write-pipeline admission
    /// today, S9's remote grants next — and a submission whose epoch is no
    /// longer current is refused at the authorization point
    /// ([`crate::data_custody::authorize_dma`]) before any device work.
    ///
    /// This is the form every write that outlives its authorization must
    /// use. [`Self::write_block`] is the authorize-at-submit form: correct
    /// under the D0 single-writer guard, where the only thing that can
    /// happen between capture and submit is the fence the same gate
    /// observes.
    pub async fn write_block_authorized(
        &self,
        offset: u64,
        data: bytes::Bytes,
        auth: crate::data_custody::CustodyEpoch,
    ) -> Result<()> {
        self.fence_gate(Some(auth))?;
        self.write_block_gated(offset, data).await
    }

    pub async fn write_block(&self, offset: u64, data: bytes::Bytes) -> Result<()> {
        // RES-6 + S7: the D0 writer-guard gate + the authorization point —
        // one relaxed load plus one comparison per submit.
        self.fence_gate(None)?;
        self.write_block_gated(offset, data).await
    }

    /// The submission body — reachable ONLY through [`Self::fence_gate`]
    /// (the S7 "one authorization point" discipline: `write_block` and
    /// `write_block_authorized` are the two doors, and both gate first).
    async fn write_block_gated(&self, offset: u64, data: bytes::Bytes) -> Result<()> {
        // Fault injection for atomicity / durability tests (no-op when counter is 0).
        loop {
            let cur = FAIL_NEXT_WRITES.load(std::sync::atomic::Ordering::SeqCst);
            if cur == 0 {
                break;
            }
            if FAIL_NEXT_WRITES
                .compare_exchange(
                    cur,
                    cur - 1,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::other(
                    "injected write_block failure (FAIL_NEXT_WRITES)",
                )));
            }
        }

        let data_len = data.len();
        let alignment = crate::cache::pool::POOLED_BUF_ALIGN;

        // Write-lane-fanout (2026-08-07): block-offset-affinity lane pick
        // — same block, same lane, same connection (per-offset submission
        // order preserved structurally). The picked Arc lives past the
        // await, keeping the lane's worker alive for this request's whole
        // lifetime; the lane's DUR-2 watermark is bumped BEFORE the send
        // (balanced by note_completed on every terminal outcome), so a
        // barrier that starts after this line waits this write out.
        let lane = self.write_lane_pick(offset);

        let rx_oneshot = if (data.as_ptr() as usize) % alignment == 0 && data_len % alignment == 0 {
            let data_type = WriteData::Aligned { data: data.clone() };
            let (tx, rx) = oneshot::channel();
            lane.wm.note_submitted();
            lane.sender()
                .try_send(UringRequest::Write {
                    offset,
                    data: data_type,
                    tx,
                })
                .map_err(|e| {
                    // The send never happened: terminally resolved here.
                    lane.wm.note_completed();
                    crate::fuse_client::METRICS
                        .uring_queue_full
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "Uring request queue full or closed (backpressure): {:?}",
                        e
                    ))
                })?;
            rx
        } else {
            // Zero-copy write-path §5.6 (PR 2): pooled write sources are
            // 4 KiB-aligned by contract, so this bounce-copy branch must stay
            // cold on aligned workloads. Non-4 KiB-multiple payloads
            // (compressed/encrypted output, tail blocks) are its only
            // legitimate traffic; growth on a passthrough full-block workload
            // means a buffer escaped the aligned pools (contract violation).
            crate::fuse_client::METRICS
                .nvme_unaligned_write_fallbacks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // P2-4: prefer the process-wide 4K-aligned buffer pool for typical
            // block sizes; fall back to posix_memalign only when the write is
            // larger than the pool buffer.
            let aligned_len = (data_len + 4095) & !4095;
            let pool = &crate::cache::ALIGNED_BUF_POOL;
            let use_pool = aligned_len <= pool.buf_size();

            let (rp, data_type) = if use_pool {
                let rp = pool.alloc_raw();
                // SAFETY: `rp` is a fresh `alloc_raw()` slot of `pool.buf_size()`
                // bytes and `aligned_len <= buf_size` was just checked, so both the
                // copy of `data_len` bytes and the tail zero-fill stay in bounds;
                // `data` is a live slice of at least `data_len` bytes.
                unsafe {
                    libc::memcpy(
                        rp as *mut libc::c_void,
                        data.as_ptr() as *const libc::c_void,
                        data_len,
                    );
                    if aligned_len > data_len {
                        libc::memset(
                            (rp as usize + data_len) as *mut libc::c_void,
                            0,
                            aligned_len - data_len,
                        );
                    }
                }
                (
                    rp,
                    WriteData::PooledUnaligned {
                        ptr: SendPtr(rp),
                        len: aligned_len,
                    },
                )
            } else {
                // SAFETY: `posix_memalign` either returns 0 with a live
                // `aligned_len`-byte allocation or we bail out; the copy writes
                // `data_len <= aligned_len` bytes from a live slice and the memset
                // covers exactly the remaining tail.
                let rp = unsafe {
                    let mut buf_ptr: *mut libc::c_void = std::ptr::null_mut();
                    if libc::posix_memalign(&mut buf_ptr, alignment, aligned_len) != 0 {
                        return Err(crate::error::SqueezefsError::InvalidOperation(
                            "posix_memalign failed for write block".to_string(),
                        ));
                    }
                    libc::memcpy(buf_ptr, data.as_ptr() as *const libc::c_void, data_len);
                    if aligned_len > data_len {
                        libc::memset(
                            (buf_ptr as usize + data_len) as *mut libc::c_void,
                            0,
                            aligned_len - data_len,
                        );
                    }
                    buf_ptr as *mut u8
                };
                (
                    rp,
                    WriteData::Unaligned {
                        ptr: SendPtr(rp),
                        len: aligned_len,
                    },
                )
            };
            debug_assert_eq!(
                rp as usize % alignment,
                0,
                "unaligned-write bounce buffer violates the 4 KiB pooled-buffer \
                 alignment contract"
            );
            let (tx, rx) = oneshot::channel();
            lane.wm.note_submitted();
            if lane
                .sender()
                .try_send(UringRequest::Write {
                    offset,
                    data: data_type,
                    tx,
                })
                .is_err()
            {
                // Send failed; worker never took ownership. Release
                // immediately (and terminally resolve the watermark).
                lane.wm.note_completed();
                if use_pool {
                    // SAFETY: `rp` was `pool.alloc_raw()`'d above and the
                    // worker never took ownership — this is its only release.
                    unsafe { pool.recycle(rp) };
                } else {
                    unsafe {
                        libc::free(rp as *mut libc::c_void);
                    }
                }
                crate::fuse_client::METRICS
                    .uring_queue_full
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Err(crate::error::SqueezefsError::InvalidOperation(
                    "Uring request queue full or closed (backpressure)".to_string(),
                ));
            }
            // rp raw pointer value was copied into SendPtr inside the sent message.
            // The local `rp` binding ends with this block; no !Send value crosses the await below.
            rx
        };

        rx_oneshot.await.map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!(
                "Worker thread closed receiver: {:?}",
                e
            ))
        })??;

        crate::fuse_client::METRICS
            .put_obj
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // P2-9: full RAW only when verification is on *and* this write is sampled.
        if crate::write_verification_should_check() {
            let verified = self.verify_write_block(offset, &data).await?;
            if !verified {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Write verification failed: checksum mismatch at offset {} on device {}",
                    offset, self.device_path
                )));
            }
        }

        Ok(())
    }

    pub async fn verify_write_block(&self, offset: u64, expected: &[u8]) -> Result<bool> {
        let read_bytes = self.read_block(offset, expected.len()).await?;
        let mut matched = read_bytes.as_ref() == expected;
        if matched && SIMULATE_CORRUPTION.load(std::sync::atomic::Ordering::Relaxed) {
            matched = false;
        }
        Ok(matched)
    }

    pub async fn read_block(&self, offset: u64, size: usize) -> Result<bytes::Bytes> {
        self.read_block_with_dest(offset, size, None).await
    }

    /// Control-plane liveness read (the health worker's probe): same I/O
    /// machinery as [`Self::read_block`] but **never counted in
    /// `get_obj`** — that counter is the raw *data* device-read-op
    /// counter (the churn detector, AGENTS.md), and a probe completing
    /// behind queued data I/O at a nondeterministic point poisoned every
    /// counter-window gate keyed on it (the 2026-07-26 one-extra-fetch
    /// flake class, pinned in tests/backend_health_probe_tests.rs).
    pub async fn probe_read_block(&self, offset: u64, size: usize) -> Result<bytes::Bytes> {
        self.read_block_with_dest_inner(offset, size, None, false)
            .await
    }

    /// Read exactly `size` bytes at `offset` (exact-length contract, VL8
    /// item 5): on success the returned buffer holds `size` bytes of device
    /// data. Short/zero kernel completions (past-EOF on file-backed
    /// substrates) fail loud with `UnexpectedEof` — never partial or
    /// recycled-buffer data.
    pub async fn read_block_with_dest(
        &self,
        offset: u64,
        size: usize,
        dest_addr: Option<u64>,
    ) -> Result<bytes::Bytes> {
        self.read_block_with_dest_inner(offset, size, dest_addr, true)
            .await
    }

    /// The one read implementation behind [`Self::read_block_with_dest`]
    /// (data reads — counted) and [`Self::probe_read_block`]
    /// (control-plane probes — uncounted).
    async fn read_block_with_dest_inner(
        &self,
        offset: u64,
        size: usize,
        dest_addr: Option<u64>,
        count_in_get_obj: bool,
    ) -> Result<bytes::Bytes> {
        if dest_addr.is_none() && size > crate::cache::pool::ALIGNED_BUF_POOL.buf_size() {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "Read size {} exceeds pool buffer size {}",
                size,
                crate::cache::pool::ALIGNED_BUF_POOL.buf_size()
            )));
        }

        // zcrx read lane (design §6): eligible fills may be served by the
        // userspace NVMe/TCP lane — pooled (Z2 shape) AND, since PR Z3,
        // registered-destination reads (fused gather straight into the
        // dest); `None` (disarmed / ineligible / lane error) falls through
        // to the kernel-path worker unchanged.
        if let Some(res) = self.try_lane_read(offset, size, dest_addr).await {
            let out = res?;
            if count_in_get_obj {
                crate::fuse_client::METRICS
                    .get_obj
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return Ok(out);
        }

        let (buf_ptr, bytes, dest_token) = if let Some(addr) = dest_addr {
            // MEM-1: claim the destination's owner token BEFORE the
            // request exists — the worker holds it for the SQE's lifetime,
            // so the owner's re-arm path (the transport §5.4 commit gate)
            // waits out any DMA that can still land here even if this
            // future times out or is dropped. `None` = no registered
            // owner (IPC-arena dests carry session-lifetime ownership of
            // their own) — proceed as before.
            let token = claim_dest_token(addr, size);
            // SAFETY: the destination is registered, pinned memory whose
            // mapping outlives the pool (transport payload arenas are
            // pool-lifetime — fuse_over_uring::QueueHandle::arena), and
            // the token above parks the owner's re-arm while the worker
            // can still DMA into it (MEM-1).
            let b = unsafe {
                bytes::Bytes::from_static(std::slice::from_raw_parts(addr as *const u8, size))
            };
            (addr as *mut u8, b, token)
        } else {
            // Size-classed bounce (2026-07-25 ipc-miss-path fix): sub-block
            // windows ride the 64 KiB RANGED_BUF_POOL — a 4 KiB ranged read
            // checking out a 4 MiB whole-block backing exhausted that pool
            // at miss-path concurrency and paid a THP-zeroing fault + TLB
            // storm per excess op (see pool.rs::RANGED_BUF_POOL).
            let (p, b) = crate::cache::pool::read_bounce_pool(size).alloc();
            (p, b, None)
        };

        let (tx, rx_oneshot) = oneshot::channel();
        // Read-queue-wall campaign: READ submissions round-robin the lane
        // pool (writes ride block-offset affinity — `write_lane_pick`;
        // barriers/probes stay on lane 0).
        // The picked Arc lives past the await, keeping the lane's worker
        // alive for this request's whole lifetime.
        let read_lane = self.read_lane_pick();
        read_lane
            .sender()
            .try_send(UringRequest::Read {
                offset,
                buf_ptr: SendPtr(buf_ptr),
                size,
                bytes,
                tx,
                enq: std::time::Instant::now(),
                dest_token,
            })
            .map_err(|e| {
                crate::fuse_client::METRICS
                    .uring_queue_full
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "Uring request queue full or closed (backpressure): {:?}",
                    e
                ))
            })?;

        // Never wait unbounded on the uring worker (wedged device/worker must not
        // freeze the entire FUSE session including virtual .config reads).
        let timeout_ms = read_timeout_ms_cell().load(std::sync::atomic::Ordering::Relaxed);
        let res =
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), rx_oneshot)
                .await
            {
                Ok(Ok(r)) => r?,
                Ok(Err(e)) => {
                    return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                        "Worker thread closed receiver: {:?}",
                        e
                    )));
                }
                Err(_) => {
                    return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "NvmeBlockDev read timed out after {} ms (offset={}, size={})",
                            timeout_ms, offset, size
                        ),
                    )));
                }
            };

        if count_in_get_obj {
            crate::fuse_client::METRICS
                .get_obj
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        Ok(res)
    }
}
