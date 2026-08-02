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
    },
    Write {
        offset: u64,
        data: WriteData,
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

struct UringWorker {
    /// Dropped first in `Drop` so the worker observes disconnect and drains.
    tx: Option<crossbeam::channel::Sender<UringRequest>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// P1-6: bound io_uring request queue to apply backpressure under overload.
const URING_REQ_QUEUE_CAP: usize = 4096;

impl UringWorker {
    fn new(device_path: String) -> Self {
        let (tx, rx) = crossbeam::channel::bounded(URING_REQ_QUEUE_CAP);
        let thread = std::thread::spawn(move || {
            worker_thread_loop(device_path, rx);
        });
        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    fn sender(&self) -> &crossbeam::channel::Sender<UringRequest> {
        self.tx
            .as_ref()
            .expect("UringWorker sender used after drop")
    }
}

impl Drop for UringWorker {
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

fn worker_thread_loop(device_path: String, rx: crossbeam::channel::Receiver<UringRequest>) {
    let mut open_opts = OpenOptions::new();
    open_opts.read(true).write(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.custom_flags(libc::O_DIRECT);
    }

    let file = match open_opts.open(&device_path) {
        Ok(f) => f,
        Err(_) => {
            // ENG-3 re-triage: error, not warn — in buffered mode nothing
            // ever flushes these writes (no fsync path on the data device
            // today), so acknowledged data can be lost on power failure.
            log::error!(
                "Failed to open NVMe device {:?} with O_DIRECT; falling back to buffered \
                 I/O — buffered device writes have no flush path, so acknowledged data \
                 may be lost on power failure",
                device_path
            );
            let mut open_opts = OpenOptions::new();
            open_opts.read(true).write(true);
            match open_opts.open(&device_path) {
                Ok(f) => f,
                Err(e) => {
                    log::error!("Failed to open NVMe device {:?}: {:?}", device_path, e);
                    return;
                }
            }
        }
    };

    let fd = file.as_raw_fd();

    let mut ring = match IoUring::new(1024) {
        Ok(r) => r,
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

    struct ActiveReq {
        response: UringResponse,
        free_ptr: Option<(FreePtrKind, SendPtr)>,
        _keep_alive: Option<bytes::Bytes>,
    }

    let mut active: Vec<Option<ActiveReq>> = Vec::with_capacity(1024);
    let mut free_slots: Vec<usize> = Vec::new();
    let mut active_count = 0;
    let mut disconnected = false;

    loop {
        let mut pushed = 0;
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
                } => {
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
                    });
                    if use_fixed {
                        opcode::Read::new(types::Fixed(0), buf_ptr.0, size as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    } else {
                        opcode::Read::new(Fd(fd), buf_ptr.0, size as _)
                            .offset(offset)
                            .build()
                            .user_data(slot_idx as u64)
                    }
                }
                UringRequest::Write { offset, data, tx } => {
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
                    active[slot_idx] = Some(ActiveReq {
                        response: UringResponse::Write { tx },
                        free_ptr,
                        _keep_alive: keep_alive,
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
            };

            let mut pushed_sqe = false;
            for retry in 0..3 {
                unsafe {
                    if let Ok(()) = ring.submission().push(&sqe) {
                        pushed_sqe = true;
                        break;
                    }
                }

                // If push failed, submission queue is full. Submit existing and drain.
                let _ = ring.submit();
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

                        match act.response {
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
                                let mapped =
                                    io_res.map(|_| ()).map_err(crate::error::SqueezefsError::Io);
                                let _ = tx.send(mapped);
                            }
                        }

                        if let Some((kind, p)) = act.free_ptr {
                            release_free_ptr(kind, p);
                        }
                    }
                    completed_slots.push(slot_idx);
                }

                for idx in completed_slots {
                    free_slots.push(idx);
                    active_count -= 1;
                }

                if retry == 1 {
                    if let Err(e) = ring.submit_and_wait(1) {
                        log::error!(
                            "Uring worker: submit_and_wait(1) failed on SQ full: {:?}",
                            e
                        );
                    }
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
            pushed += 1;
        }

        if pushed > 0 {
            if let Err(e) = ring.submit() {
                log::error!("io_uring submit failed: {:?}", e);
            }
        }

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

                    match act.response {
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
                            let mapped =
                                io_res.map(|_| ()).map_err(crate::error::SqueezefsError::Io);
                            let _ = tx.send(mapped);
                        }
                    }

                    if let Some((kind, p)) = act.free_ptr {
                        release_free_ptr(kind, p);
                    }
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
                let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shutting down".to_string(),
                )));
            }
            UringRequest::Read { tx, .. } => {
                let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                    "NvmeBlockDev worker shutting down".to_string(),
                )));
            }
        }
    }

    for slot in active.iter_mut() {
        if let Some(act) = slot.take() {
            let free_ptr = act.free_ptr;
            match act.response {
                UringResponse::Read { tx, .. } => {
                    let _ = tx.send(Err(crate::error::SqueezefsError::InvalidOperation(
                        "NvmeBlockDev worker shutting down".to_string(),
                    )));
                }
                UringResponse::Write { tx } => {
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

#[derive(Clone)]
pub struct NvmeBlockDev {
    pub device_path: String,
    worker: Arc<UringWorker>,
    node_probe: Arc<NodeProbe>,
    /// zcrx read lane session (docs/design-zcrx-read-lane.md §6): armed
    /// lazily on the first eligible read (`SQUEEZEFS_ZCRX_LANE=1` +
    /// capability probes), `None` cached on any arm refusal — the kernel
    /// path stays byte-identical. Shared across clones (one association
    /// per device node).
    lane: Arc<tokio::sync::OnceCell<Option<Arc<crate::zcrx_lane::LaneSession>>>>,
}

/// Capacity in bytes of a backing file OR block device (seek-to-end works
/// for both; `metadata.len()` is 0 for block devices).
pub fn device_capacity_bytes(path: &str) -> std::io::Result<u64> {
    use std::io::Seek;
    let mut f = std::fs::File::open(path)?;
    f.seek(std::io::SeekFrom::End(0))
}

/// Coarse monotonic milliseconds since process start (probe-TTL clock).
fn coarse_monotonic_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

impl NvmeBlockDev {
    pub fn new(device_path: &str) -> Self {
        Self {
            device_path: device_path.to_string(),
            worker: Arc::new(UringWorker::new(device_path.to_string())),
            node_probe: Arc::new(NodeProbe {
                at_ms: std::sync::atomic::AtomicU64::new(0),
                seen: std::sync::atomic::AtomicBool::new(false),
            }),
            lane: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// zcrx-lane read attempt (design §6). `Some(bytes)` = the lane served
    /// this read; `None` = ineligible / not armed / lane error — the caller
    /// proceeds on the kernel path unchanged (reads are idempotent, so the
    /// per-op fallback retry is safe by construction and counted in
    /// `zcrx_fill_fallbacks`, which must stay ≈ 0).
    async fn try_lane_read(&self, offset: u64, size: usize) -> Option<Result<bytes::Bytes>> {
        if !crate::zcrx_lane::lane_env_armed() {
            return None;
        }
        let sess = self
            .lane
            .get_or_init(|| crate::zcrx_lane::arm_for_device(&self.device_path))
            .await
            .as_ref()?;
        if sess.poisoned() || !sess.range_eligible(offset, size) {
            return None;
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
            Err(e) => {
                // Dropping `bytes` recycles the pooled buffer; the lane's
                // completion law guarantees no writer touches it after the
                // op resolves (timeout paths poison the queue first).
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

    pub async fn write_block(&self, offset: u64, data: bytes::Bytes) -> Result<()> {
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

        let rx_oneshot = if (data.as_ptr() as usize) % alignment == 0 && data_len % alignment == 0 {
            let data_type = WriteData::Aligned { data: data.clone() };
            let (tx, rx) = oneshot::channel();
            self.worker
                .sender()
                .try_send(UringRequest::Write {
                    offset,
                    data: data_type,
                    tx,
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
            if self
                .worker
                .sender()
                .try_send(UringRequest::Write {
                    offset,
                    data: data_type,
                    tx,
                })
                .is_err()
            {
                // Send failed; worker never took ownership. Release immediately.
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

        // zcrx read lane (design §6): eligible pooled fills may be served by
        // the userspace NVMe/TCP lane; `None` (disarmed / ineligible / lane
        // error) falls through to the kernel-path worker unchanged.
        if dest_addr.is_none() {
            if let Some(res) = self.try_lane_read(offset, size).await {
                let out = res?;
                if count_in_get_obj {
                    crate::fuse_client::METRICS
                        .get_obj
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                return Ok(out);
            }
        }

        let (buf_ptr, bytes) = if let Some(addr) = dest_addr {
            // SAFETY: destination address is pre-registered and pinned memory
            let b = unsafe {
                bytes::Bytes::from_static(std::slice::from_raw_parts(addr as *const u8, size))
            };
            (addr as *mut u8, b)
        } else {
            // Size-classed bounce (2026-07-25 ipc-miss-path fix): sub-block
            // windows ride the 64 KiB RANGED_BUF_POOL — a 4 KiB ranged read
            // checking out a 4 MiB whole-block backing exhausted that pool
            // at miss-path concurrency and paid a THP-zeroing fault + TLB
            // storm per excess op (see pool.rs::RANGED_BUF_POOL).
            crate::cache::pool::read_bounce_pool(size).alloc()
        };

        let (tx, rx_oneshot) = oneshot::channel();
        self.worker
            .sender()
            .try_send(UringRequest::Read {
                offset,
                buf_ptr: SendPtr(buf_ptr),
                size,
                bytes,
                tx,
                enq: std::time::Instant::now(),
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
        let res = match tokio::time::timeout(std::time::Duration::from_secs(30), rx_oneshot).await {
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
                        "NvmeBlockDev read timed out after 30s (offset={}, size={})",
                        offset, size
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
