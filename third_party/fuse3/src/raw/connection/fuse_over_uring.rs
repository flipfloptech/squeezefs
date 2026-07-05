//! Kernel **FUSE-over-io_uring** (Linux 6.14+ / 7.x) — `linux/fuse.h` + libfuse `fuse_uring.c`.
//!
//! **Required** request transport. No userspace opt-out, no classical fallback after arm.
//! Mount fails if setup fails. The kernel module parameter `fuse.enable_uring` must
//! be Y (we try to enable it at start).
//!
//! The only classical `/dev/fuse` use is the single `FUSE_INIT` exchange — the kernel
//! rejects `REGISTER` until `fch->initialized`. After that, all requests/replies are
//! over-uring only.
//!
//! Tuning only:
//! ```text
//! SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH=4   # optional, per-queue depth
//! SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES=N    # optional, default = min(nproc, 8)
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

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use io_uring::squeue::Entry128;
use io_uring::{cqueue, opcode, squeue, types, IoUring};
use tracing::{debug, error, info, warn};

/// `FUSE_OVER_IO_URING` (1ULL<<41) → `flags2` bit 9.
pub const FUSE_OVER_IO_URING_FLAGS2: u32 = 1u32 << 9;

pub const FUSE_URING_IN_OUT_HEADER_SZ: usize = 128;
pub const FUSE_URING_OP_IN_OUT_SZ: usize = 128;

const FUSE_IO_URING_CMD_REGISTER: u32 = 1;
const FUSE_IO_URING_CMD_COMMIT_AND_FETCH: u32 = 2;
const FUSE_IN_HEADER_SIZE: usize = 40;

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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FuseUringCmdReq {
    flags: u64,
    commit_id: u64,
    qid: u16,
    padding: [u8; 6],
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
}

struct CommitMsg {
    ent_idx: u16,
    commit_id: u64,
    header: Vec<u8>,
    reply_body: Bytes,
}

struct QueueHandle {
    /// Unbounded: a bounded sync_channel can block the session reply task if the
    /// queue worker is briefly not draining, freezing *all* fuse replies.
    commit_tx: std::sync::mpsc::Sender<CommitMsg>,
    /// Wake the queue thread (commit or shutdown).
    wake_fd: RawFd,
    /// Keep OwnedFd alive.
    _wake: OwnedFd,
    payload_buffers: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
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

    fn push(&self, req: InboundUringReq) {
        let _ = self.tx.send(req);
    }

    /// Pop with timeout (async)
    async fn pop_timeout(&self, active: &AtomicBool, timeout: Duration) -> Option<InboundUringReq> {
        let mut rx_guard = self.rx.lock().await;
        if !active.load(Ordering::Relaxed) {
            return None;
        }
        match tokio::time::timeout(timeout, rx_guard.recv()).await {
            Ok(Some(r)) => Some(r),
            _ => None,
        }
    }

    fn notify_all(&self) {
        // Async queues don't need condvar notify
    }
}

/// Process-wide FUSE-over-io_uring controller (one per fuse session / mount).
pub struct FuseOverUring {
    /// Session may drain the inbound queue only when true (all queues REGISTERed).
    ready: AtomicBool,
    /// False after shutdown; workers exit and session falls through to errors.
    active: AtomicBool,
    /// Number of queue workers that have submitted their initial REGISTERs.
    queues_registered: AtomicU64,
    pub(crate) nqueues: u16, // used for diagnostics
    inbound: Vec<Arc<InboundQueue>>,
    /// unique → (qid, ent_idx, commit_id)
    pending: Mutex<HashMap<u64, (u16, u16, u64)>>,
    queues: Vec<QueueHandle>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    fuse_fd: RawFd,
    payload_sz: usize,
    // metrics
    pub stats_requests: AtomicU64,
    pub stats_replies: AtomicU64,
    pub stats_cqe_err: AtomicU64,
    pub stats_register: AtomicU64,
}

static ACTIVE_SESSIONS: AtomicU64 = AtomicU64::new(0);
static STATS_REQUESTS: AtomicU64 = AtomicU64::new(0);
static STATS_REPLIES: AtomicU64 = AtomicU64::new(0);
static STATS_CQE_ERR: AtomicU64 = AtomicU64::new(0);
static STATS_REGISTER: AtomicU64 = AtomicU64::new(0);

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
    pub fn try_start(fuse_fd: RawFd, max_write: usize) -> io::Result<Arc<Self>> {
        if !ensure_kernel_fuse_uring_enabled()? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "kernel fuse.enable_uring is off and could not be enabled \
                 (need CAP_SYS_ADMIN / root: echo Y > /sys/module/fuse/parameters/enable_uring)",
            ));
        }
        // Kernel fuse_uring_create() uses num_possible_cpus() for ring->nr_queues and
        // is_ring_ready() requires EVERY queue (except the current) to have ≥1 entry.
        // Registering fewer queues than that means the kernel never switches off the
        // classical path while we stop reading it → permanent hang.
        // Override only for testing; production must match the kernel.
        let kernel_nqueues = {
            let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
            if n > 0 {
                n as usize
            } else {
                std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(4)
            }
        };
        let nqueues = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(kernel_nqueues)
            .clamp(1, 512);
        if nqueues < kernel_nqueues {
            warn!(
                "SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES={nqueues} < kernel possible CPUs \
                 ({kernel_nqueues}); FUSE-over-io_uring will never become ready"
            );
        }
        // Per-queue ring depth. depth=1 is enough for kernel readiness but leaves no
        // slack when a COMMIT is in flight and a new request arrives on the same
        // CPU — under pjdfstest-style forget/open storms that contributed to stalls.
        // depth=4 keeps memory modest (nqueues * depth * payload_sz).
        let depth = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4usize)
            .clamp(1, 32);
        // Must be >= kernel ring->max_payload_sz:
        //   max(FUSE_MIN_READ_BUFFER, max_write, max_pages * PAGE_SIZE)
        // (fs/fuse/dev_uring.c). Kernel clamps max_pages to fuse_max_pages_limit (256).
        const FUSE_MIN_READ_BUFFER: usize = 8192;
        const KERNEL_MAX_PAGES_LIMIT: usize = 256;
        let page = 4096usize;
        let payload_sz = max_write
            .max(FUSE_MIN_READ_BUFFER)
            .max(KERNEL_MAX_PAGES_LIMIT * page);

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
            let payload_buffers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            queue_handles.push(QueueHandle {
                commit_tx,
                wake_fd,
                _wake: wake,
                payload_buffers,
            });
            commit_rxs.push(commit_rx);
        }

        let pool = Arc::new(Self {
            // Critical: stay not-ready until every queue has submitted REGISTER.
            // Otherwise the session stops classical /dev/fuse reads while the kernel
            // still delivers on the classical path → permanent hang.
            ready: AtomicBool::new(false),
            active: AtomicBool::new(true),
            queues_registered: AtomicU64::new(0),
            nqueues: nqueues as u16,
            inbound,
            pending: Mutex::new(HashMap::new()),
            queues: queue_handles,
            workers: Mutex::new(Vec::new()),
            fuse_fd,
            payload_sz,
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
        // switched fiq→uring *before* the session marks ready and stops classical
        // reads. (Serving classical across that switch deadlocks: session blocks
        // on classical while new requests only arrive on uring.)
        //
        // Requests that arrived on classical during this wait (e.g. parent
        // metadata() while daemon is still in INIT) are drained just after
        // mark_ready — see `drain_classical_stranded`.
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

        eprintln!(
            "FUSE-over-io_uring registered: queues={nqueues} depth={depth} payload_sz={payload_sz} fd={fuse_fd}"
        );
        info!(
            "FUSE-over-io_uring registered: queues={nqueues} depth={depth} \
             payload_sz={payload_sz} fd={fuse_fd}"
        );
        Ok(pool)
    }

    /// Non-blocking drain of classical `/dev/fuse` requests stranded during INIT→REGISTER.
    /// FORGET is completed by the read alone; other ops get a minimal ENOSYS reply so the
    /// client retries on the now-live uring path. Prevents permanent `waiting≥1` / EBUSY umount.
    pub fn drain_classical_stranded(fuse_fd: RawFd) {
        // Ensure non-blocking.
        let flags = unsafe { libc::fcntl(fuse_fd, libc::F_GETFL) };
        if flags >= 0 {
            let _ = unsafe { libc::fcntl(fuse_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
        let mut buf = vec![0u8; 8192];
        let mut drained = 0u32;
        loop {
            let n = unsafe {
                libc::read(
                    fuse_fd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::EAGAIN) {
                    break;
                }
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                warn!("drain_classical_stranded read: {e}");
                break;
            }
            if n == 0 {
                break;
            }
            if (n as usize) < 40 {
                break;
            }
            let opcode = u32::from_le_bytes(buf[4..8].try_into().unwrap());
            let unique = u64::from_le_bytes(buf[8..16].try_into().unwrap());
            drained += 1;
            // FUSE_FORGET=2, FUSE_BATCH_FORGET=42: no reply on classical.
            if matches!(opcode, 2 | 42) {
                continue;
            }
            if unique == 0 {
                continue;
            }
            // Minimal fuse_out_header: ENOSYS so client retries (now via uring).
            let mut out = [0u8; 16];
            out[0..4].copy_from_slice(&16u32.to_le_bytes());
            out[4..8].copy_from_slice(&(-libc::ENOSYS).to_le_bytes());
            out[8..16].copy_from_slice(&unique.to_le_bytes());
            let w = unsafe { libc::write(fuse_fd, out.as_ptr().cast(), out.len()) };
            if w < 0 {
                let e = io::Error::last_os_error();
                // ENOENT = already completed; fine.
                if e.raw_os_error() != Some(libc::ENOENT) {
                    warn!("drain_classical_stranded write unique={unique}: {e}");
                }
            }
        }
        if drained > 0 {
            info!("drained {drained} classical request(s) stranded during uring arm");
            eprintln!("FUSE-over-io_uring: drained {drained} classical handoff request(s)");
        }
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

    /// Async pop for session read path.
    pub async fn recv_inbound_timeout(&self, qid: u16, timeout: Duration) -> Option<InboundUringReq> {
        if (qid as usize) < self.inbound.len() {
            self.inbound[qid as usize].pop_timeout(&self.active, timeout).await
        } else {
            None
        }
    }

    pub fn submit_reply(&self, unique: u64, header: Vec<u8>, reply_body: Bytes) -> io::Result<()> {
        let (qid, ent_idx, commit_id) = self
            .pending
            .lock()
            .unwrap()
            .remove(&unique)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("uring: no pending unique={unique}"),
                )
            })?;
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
        // Wake queue thread
        let one: u64 = 1;
        let _ = unsafe { libc::write(q.wake_fd, &one as *const u64 as *const _, 8) };
        self.stats_replies.fetch_add(1, Ordering::Relaxed);
        STATS_REPLIES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn get_payload_buffer(&self, unique: u64) -> Option<(u64, usize)> {
        let (qid, ent_idx, _) = {
            let pending_guard = self.pending.lock().unwrap();
            pending_guard.get(&unique).cloned()?
        };
        let q = self.queues.get(qid as usize)?;
        let buffers = q.payload_buffers.lock().unwrap();
        let ptr = buffers.get(ent_idx as usize).cloned()?;
        Some((ptr as u64, self.payload_sz))
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
            info!(
                "FUSE-over-io_uring shutting down (fd={})",
                self.fuse_fd
            );
            // Drop any uncommitted request map entries; kernel already aborted them.
            self.pending.lock().unwrap().clear();
        }
        for iq in &self.inbound {
            iq.notify_all();
        }
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

struct Ent {
    header: Box<FuseUringReqHeader>,
    payload: Vec<u8>,
    iov: [libc::iovec; 2],
}

/// Poll `/dev/fuse` until the connection is aborted/closed or the pool shuts down.
fn connection_watch(pool: Arc<FuseOverUring>) {
    while pool.active.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd: pool.fuse_fd,
            events: (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) as i16,
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
        if rev & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) as i16 != 0 {
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
    // Best-effort pin to core qid
    let _ = core_affinity::set_for_current(core_affinity::CoreId { id: qid as usize });

    let sq_entries = (depth as u32 + 8).next_power_of_two().max(16);
    let mut ring: Ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder()
        .setup_cqsize(sq_entries * 2)
        .build(sq_entries)
        .map_err(|e| {
            io::Error::other(format!(
                "SQE128 IoUring build(sq={sq_entries}): {e} — need IORING_SETUP_SQE128"
            ))
        })?;

    ring.submitter()
        .register_files(&[pool.fuse_fd, wake_fd])
        .map_err(|e| {
            io::Error::other(format!(
                "register_files(fuse_fd={}, wake_fd={}): {e}",
                pool.fuse_fd, wake_fd
            ))
        })?;

    let mut ents: Vec<Ent> = (0..depth)
        .map(|_| {
            let mut header = Box::new(FuseUringReqHeader::default());
            let payload = vec![0u8; payload_sz];
            header.ring_ent_in_out.payload_sz = payload_sz as u32;
            Ent {
                header,
                payload,
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
            }
        })
        .collect();

    for ent in &mut ents {
        ent.iov[0] = libc::iovec {
            iov_base: (&mut *ent.header as *mut FuseUringReqHeader).cast(),
            iov_len: std::mem::size_of::<FuseUringReqHeader>(),
        };
        ent.iov[1] = libc::iovec {
            iov_base: ent.payload.as_mut_ptr().cast(),
            iov_len: ent.payload.len(),
        };
    }

    {
        let mut buffers = pool.queues[qid as usize].payload_buffers.lock().unwrap();
        *buffers = ents.iter_mut().map(|ent| ent.payload.as_mut_ptr() as usize).collect();
    }

    for (idx, ent) in ents.iter().enumerate() {
        push_cmd(
            &mut ring,
            FUSE_IO_URING_CMD_REGISTER,
            qid,
            0,
            Some((ent.iov.as_ptr(), 2)),
            idx as u64,
        )
        .map_err(|e| io::Error::other(format!("push REGISTER ent={idx}: {e}")))?;
        pool.stats_register.fetch_add(1, Ordering::Relaxed);
        STATS_REGISTER.fetch_add(1, Ordering::Relaxed);
    }
    {
        let poll_e = opcode::PollAdd::new(types::Fixed(1), libc::POLLIN as _)
            .build()
            .user_data(u64::MAX);
        unsafe {
            ring.submission()
                .push(&Entry128::from(poll_e))
                .map_err(|_| io::Error::other("sq full (poll)"))?;
        }
    }
    ring.submit()
        .map_err(|e| io::Error::other(format!("submit REGISTER batch: {e}")))?;
    pool.queues_registered.fetch_add(1, Ordering::AcqRel);

    while pool.active.load(Ordering::Relaxed) {
        // Drain commits for this queue only (no demux)
        while let Ok(msg) = commit_rx.try_recv() {
            let ent = &mut ents[msg.ent_idx as usize];
            apply_reply(ent, &msg.header, &msg.reply_body);
            push_cmd(
                &mut ring,
                FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
                qid,
                msg.commit_id,
                None,
                msg.ent_idx as u64,
            )?;
            ring.submit()?;
        }
        // Drain eventfd
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

        // Exit promptly when another worker/watch already shut us down (wake_fd).
        if !pool.active.load(Ordering::Relaxed) {
            break;
        }

        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) if FuseOverUring::is_disconnect_errno(e.raw_os_error().unwrap_or(0)) => {
                info!(
                    "fuse-over-uring qid={qid}: submit_and_wait disconnect ({e}); shutting down"
                );
                pool.shutdown();
                break;
            }
            Err(e) => return Err(e),
        }

        if !pool.active.load(Ordering::Relaxed) {
            break;
        }

        let completed: Vec<(u64, i32)> = {
            let mut cq = ring.completion();
            cq.sync();
            cq.map(|c| (c.user_data(), c.result())).collect()
        };

        let mut resubmit = Vec::new();
        let mut need_repoll = false;
        let mut disconnect = false;
        for (user_data, res) in completed {
            if user_data == u64::MAX {
                // wake_fd poll completed — re-arm (or exit if inactive)
                need_repoll = true;
                continue;
            }
            let ent_idx = user_data as usize;
            if res < 0 {
                let err = -res;
                pool.stats_cqe_err.fetch_add(1, Ordering::Relaxed);
                STATS_CQE_ERR.fetch_add(1, Ordering::Relaxed);
                if err == libc::EAGAIN || err == libc::EINTR {
                    if ent_idx < ents.len() {
                        resubmit.push(ent_idx);
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
                if err == libc::ENOTSUP || err == libc::EINVAL || err == libc::ENOSYS {
                    error!("fuse-over-uring: kernel rejected protocol err={err}");
                    pool.shutdown();
                    return Err(io::Error::from_raw_os_error(err));
                }
                // Drop any pending map entry for this ring slot and re-REGISTER so we
                // do not permanently lose queue capacity after a failed COMMIT/REGISTER.
                warn!("fuse-over-uring qid={qid} cqe err={err} ent={ent_idx}; reclaim entry");
                pool.pending.lock().unwrap().retain(|_, (q, e, _)| {
                    !(*q == qid && *e == ent_idx as u16)
                });
                if ent_idx < ents.len() {
                    resubmit.push(ent_idx);
                }
                continue;
            }
            if ent_idx >= ents.len() {
                continue;
            }
            // Kernel sets commit_id = unique when delivering a request.
            let unique = u64::from_le_bytes(ents[ent_idx].header.in_out[8..16].try_into().unwrap());
            let mut commit_id = ents[ent_idx].header.ring_ent_in_out.commit_id;
            if commit_id == 0 {
                // Fall back to unique — some paths only fill in_out.
                commit_id = unique;
            }
            if unique == 0 {
                // Prefer COMMIT with commit_id if the kernel filled it — re-REGISTER
                // alone leaves USERSPACE entries and permanent waiting/EBUSY umount.
                let cid = ents[ent_idx].header.ring_ent_in_out.commit_id;
                if cid != 0 {
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: unique=0 commit_id={cid}; force EIO COMMIT"
                    );
                    let mut out = [0u8; 16];
                    out[0..4].copy_from_slice(&16u32.to_le_bytes());
                    out[4..8].copy_from_slice(&(-libc::EIO).to_le_bytes());
                    out[8..16].copy_from_slice(&cid.to_le_bytes());
                    apply_reply(&mut ents[ent_idx], &out, &Bytes::new());
                    let _ = push_cmd(
                        &mut ring,
                        FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
                        qid,
                        cid,
                        None,
                        ent_idx as u64,
                    );
                    let _ = ring.submit();
                } else {
                    warn!(
                        "fuse-over-uring qid={qid} ent={ent_idx}: unique=0 commit_id=0; re-REGISTER"
                    );
                    resubmit.push(ent_idx);
                }
                continue;
            }
            let opcode =
                u32::from_le_bytes(ents[ent_idx].header.in_out[4..8].try_into().unwrap());
            let payload_sz = ents[ent_idx].header.ring_ent_in_out.payload_sz as usize;
            let mut header_and_op =
                Vec::with_capacity(FUSE_IN_HEADER_SIZE + FUSE_URING_OP_IN_OUT_SZ);
            header_and_op.extend_from_slice(&ents[ent_idx].header.in_out[..FUSE_IN_HEADER_SIZE]);
            header_and_op.extend_from_slice(&ents[ent_idx].header.op_in);
            let payload = Bytes::copy_from_slice(
                &ents[ent_idx].payload[..payload_sz.min(ents[ent_idx].payload.len())],
            );

            pool.stats_requests.fetch_add(1, Ordering::Relaxed);
            STATS_REQUESTS.fetch_add(1, Ordering::Relaxed);
            debug!(
                qid,
                ent_idx, unique, commit_id, payload_sz, opcode, "fuse-over-uring inbound request"
            );

            // FUSE_FORGET (2) / FUSE_BATCH_FORGET (42) are "no reply" on classical.
            // Over-uring still holds the ring entry in USERSPACE until COMMIT.
            // Commit *immediately* here (do not wait for the session task).
            // Session still runs forget accounting from `inbound`; it must not reply.
            const FUSE_FORGET: u32 = 2;
            const FUSE_BATCH_FORGET: u32 = 42;
            if matches!(opcode, FUSE_FORGET | FUSE_BATCH_FORGET) {
                // Deliver for nlookup accounting only — no pending map entry.
                pool.inbound[qid as usize].push(InboundUringReq {
                    header_and_op,
                    payload,
                    unique,
                });
                let mut out = [0u8; 16];
                out[0..4].copy_from_slice(&16u32.to_le_bytes());
                // error = 0
                out[8..16].copy_from_slice(&unique.to_le_bytes());
                apply_reply(&mut ents[ent_idx], &out, &Bytes::new());
                push_cmd(
                    &mut ring,
                    FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
                    qid,
                    commit_id,
                    None,
                    ent_idx as u64,
                )?;
                ring.submit()?;
                pool.stats_replies.fetch_add(1, Ordering::Relaxed);
                STATS_REPLIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            // CRITICAL: insert pending *before* exposing the request on `inbound`.
            // Otherwise a session worker can reply before the map entry exists,
            // submit_reply returns NotFound, we drop the reply, and the kernel
            // keeps `waiting≥1` forever → plain `umount` EBUSY with no openers
            // (seen after full pjdfstest).
            pool.pending
                .lock()
                .unwrap()
                .insert(unique, (qid, ent_idx as u16, commit_id));
            pool.inbound[qid as usize].push(InboundUringReq {
                header_and_op,
                payload,
                unique,
            });
        }
        if disconnect {
            pool.shutdown();
            break;
        }
        if !pool.active.load(Ordering::Relaxed) {
            break;
        }
        if need_repoll {
            let poll_e = opcode::PollAdd::new(types::Fixed(1), libc::POLLIN as _)
                .build()
                .user_data(u64::MAX);
            let _ = unsafe { ring.submission().push(&Entry128::from(poll_e)) };
            let _ = ring.submit();
        }
        // Never re-REGISTER after a disconnect; only while still active.
        if !resubmit.is_empty() && pool.active.load(Ordering::Relaxed) {
            for ent_idx in resubmit {
                let ent = &ents[ent_idx];
                let _ = push_cmd(
                    &mut ring,
                    FUSE_IO_URING_CMD_REGISTER,
                    qid,
                    0,
                    Some((ent.iov.as_ptr(), 2)),
                    ent_idx as u64,
                );
            }
            let _ = ring.submit();
        }
    }
    // Final drain of any pending commits (including FUSE_DESTROY reply) before exiting
    let mut final_commits = 0;
    while let Ok(msg) = commit_rx.try_recv() {
        let ent = &mut ents[msg.ent_idx as usize];
        apply_reply(ent, &msg.header, &msg.reply_body);
        let _ = push_cmd(
            &mut ring,
            FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
            qid,
            msg.commit_id,
            None,
            msg.ent_idx as u64,
        );
        final_commits += 1;
    }
    if final_commits > 0 {
        let _ = ring.submit_and_wait(final_commits);
    }
    debug!("fuse-over-uring qid={qid} worker exit");
    Ok(())
}

/// Place a classical fuse reply (`fuse_out_header` || body) into the ring entry
/// the way libfuse/`send_reply_uring` does: header in `in_out`, body in payload.
fn apply_reply(ent: &mut Ent, header: &[u8], body: &Bytes) {
    const OUT_HDR: usize = 16; // sizeof(fuse_out_header)
    // Clear header region so stale request bytes cannot leak into the reply.
    ent.header.in_out = [0; FUSE_URING_IN_OUT_HEADER_SZ];
    if header.len() < OUT_HDR {
        // Degenerate — treat as IO error header.
        ent.header.in_out[..4].copy_from_slice(&((OUT_HDR as u32).to_le_bytes()));
        ent.header.in_out[4..8].copy_from_slice(&((-libc::EIO as i32).to_le_bytes()));
        ent.header.ring_ent_in_out.payload_sz = 0;
        return;
    }
    ent.header.in_out[..OUT_HDR].copy_from_slice(&header[..OUT_HDR]);

    let mut payload_len = 0;
    if header.len() > OUT_HDR {
        let extra = &header[OUT_HDR..];
        let n = extra.len().min(ent.payload.len());
        ent.payload[..n].copy_from_slice(&extra[..n]);
        payload_len = n;
    }

    let body_len = body.len().min(ent.payload.len() - payload_len);
    if body_len > 0 && body.as_ptr() != unsafe { ent.payload.as_ptr().add(payload_len) } {
        ent.payload[payload_len..payload_len + body_len].copy_from_slice(&body[..body_len]);
    }
    payload_len += body_len;

    ent.header.ring_ent_in_out.payload_sz = payload_len as u32;
}

fn push_cmd(
    ring: &mut Ring,
    cmd_op: u32,
    qid: u16,
    commit_id: u64,
    iov: Option<(*const libc::iovec, u32)>,
    user_data: u64,
) -> io::Result<()> {
    let mut cmd = [0u8; 80];
    let req = FuseUringCmdReq {
        flags: 0,
        commit_id,
        qid,
        padding: [0; 6],
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

    unsafe {
        ring.submission()
            .push(&entry)
            .map_err(|_| io::Error::other("submission queue full"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flags2_bit() {
        assert_eq!(FUSE_OVER_IO_URING_FLAGS2, 1u32 << 9);
    }

    #[test]
    fn test_header_sizes() {
        assert_eq!(std::mem::size_of::<FuseUringReqHeader>(), 128 + 128 + 32);
        assert_eq!(std::mem::size_of::<FuseUringCmdReq>(), 24);
    }
}
