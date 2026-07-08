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
use std::time::{Duration, Instant};

use bytes::Bytes;
use io_uring::squeue::Entry128;
use io_uring::{cqueue, opcode, squeue, types, IoUring};
use tracing::{debug, error, info, warn};

/// Payload-lease re-arm protocol core (refs/parked publish-then-recheck).
/// `#[path]`-included so `loom-models` can model-check the exact shipped
/// code (SqueezeFS zero-copy write-path design §5.4, house extracted-core
/// convention).
#[path = "lease_core.rs"]
mod lease_core;
use lease_core::{CommitGate, EntLeaseState};

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
    /// The queue's payload arena, set once by the worker at startup. Held
    /// here so payload pointers handed out via `get_payload_buffer` stay
    /// valid for the pool's whole life, even after the worker exited.
    arena: std::sync::Mutex<Option<Arc<PayloadArena>>>,
}

/// Owns every registered payload buffer of one queue plus a dup of the
/// queue eventfd (§5.4). Payload allocations live here — not in the
/// worker-local `Ent` — so a payload lease outliving the worker (shutdown
/// with a pathological handler) keeps pointing at valid memory, and the
/// wake fd a late lease drop writes can never be closed/reused underneath
/// it. A leaked lease degrades to a leaked buffer, never a dangling
/// pointer.
struct PayloadArena {
    /// `*mut u8` stored as `usize` (one stable allocation per ring ent;
    /// never reallocated for the arena's life).
    bufs: Vec<usize>,
    layout: std::alloc::Layout,
    /// dup(2) of the queue eventfd: lease drops wake the worker through the
    /// arena so the fd is alive exactly as long as any lease can write it.
    wake: OwnedFd,
}

impl PayloadArena {
    fn new(depth: usize, payload_sz: usize, wake_fd: RawFd) -> io::Result<Arc<Self>> {
        let dup = unsafe { libc::dup(wake_fd) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` just returned a fresh owned descriptor.
        let wake = unsafe { OwnedFd::from_raw_fd(dup) };
        let layout = std::alloc::Layout::from_size_align(payload_sz, 4096)
            .map_err(io::Error::other)?;
        let mut bufs = Vec::with_capacity(depth);
        for _ in 0..depth {
            // SAFETY: `layout` has non-zero size (payload_sz ≥ 8192).
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
            if ptr.is_null() {
                for &p in &bufs {
                    // SAFETY: allocated above with the same layout.
                    unsafe { std::alloc::dealloc(p as *mut u8, layout) };
                }
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "payload arena allocation failed",
                ));
            }
            bufs.push(ptr as usize);
        }
        Ok(Arc::new(Self { bufs, layout, wake }))
    }

    fn buf(&self, idx: usize) -> Option<*mut u8> {
        self.bufs.get(idx).map(|&p| p as *mut u8)
    }
}

impl Drop for PayloadArena {
    fn drop(&mut self) {
        for &p in &self.bufs {
            // SAFETY: allocated in `new` with `self.layout`; dropped once.
            unsafe { std::alloc::dealloc(p as *mut u8, self.layout) };
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
        // §5.4 severance-boundary enforcement, armed in debug/test builds:
        // a lease's lifetime is bounded by ONE handler invocation; anything
        // second-scale means a payload escaped toward a long-lived cache
        // and would park this ent's COMMIT_AND_FETCH indefinitely.
        debug_assert!(
            age_ms < 1000,
            "transport payload lease held {age_ms} ms (≥ 1 s) — a FUSE_WRITE \
             payload escaped its handler (lease-severance violation, §5.4)"
        );
        if self.state.release() {
            // Last lease gone with a commit parked: wake the queue worker.
            let one: u64 = 1;
            // SAFETY: writing 8 bytes to an eventfd we keep alive via
            // `self.arena.wake`.
            unsafe {
                libc::write(
                    self.arena.wake.as_raw_fd(),
                    &one as *const u64 as *const _,
                    8,
                )
            };
        }
    }
}

// SAFETY: the payload memory is owned by the arena (held alive by the Arc);
// reads are immutable while the lease lives (protocol above); drops can run
// on any thread (tokio workers) and only touch atomics + an eventfd write.
unsafe impl Send for EntPayloadLease {}
unsafe impl Sync for EntPayloadLease {}

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
// §5.4 transport payload-lease observability (SqueezeFS stats inode).
static TRANSPORT_PAYLOAD_LEASES: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_PARKED_COMMITS: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_LEASES_OUTSTANDING: AtomicU64 = AtomicU64::new(0);
static TRANSPORT_LEASE_MAX_AGE_MS: AtomicU64 = AtomicU64::new(0);

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
/// parked_commits, leases_outstanding, lease_max_age_ms)`.
/// `payload_leases` proves adoption (FUSE_WRITE rides leases, not copies);
/// `parked_commits` ≫ 0 means handlers hold payloads past their reply or
/// Q_DEPTH is too small; `leases_outstanding` returns to 0 at quiesce;
/// `lease_max_age_ms` is the severance-boundary high-water mark (bounded by
/// one handler invocation, hard-asserted in debug builds).
pub fn transport_lease_stats() -> (u64, u64, u64, u64) {
    (
        TRANSPORT_PAYLOAD_LEASES.load(Ordering::Relaxed),
        TRANSPORT_PARKED_COMMITS.load(Ordering::Relaxed),
        TRANSPORT_LEASES_OUTSTANDING.load(Ordering::Relaxed),
        TRANSPORT_LEASE_MAX_AGE_MS.load(Ordering::Relaxed),
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
            queue_handles.push(QueueHandle {
                commit_tx,
                wake_fd,
                _wake: wake,
                arena: std::sync::Mutex::new(None),
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
        let arena = q.arena.lock().unwrap().clone()?;
        let ptr = arena.buf(ent_idx as usize)?;
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
    /// Registered payload buffer — owned by the queue's [`PayloadArena`]
    /// (kept alive past worker exit by lease/pool Arcs).
    payload_ptr: *mut u8,
    payload_len: usize,
    iov: [libc::iovec; 2],
}

impl Ent {
    /// Immutable payload view (delivery-time copy for non-leased opcodes).
    fn payload(&self) -> &[u8] {
        // SAFETY: `payload_ptr..+payload_len` is one arena buffer, alive for
        // the worker's life; the kernel only writes it between re-arm and
        // the delivery CQE, and this view is taken after the CQE.
        unsafe { std::slice::from_raw_parts(self.payload_ptr, self.payload_len) }
    }

    /// Mutable payload view for reply application. Caller must hold the
    /// §5.4 gate proof: the ent's lease refs == 0 (CommitGate::Ready /
    /// try_unpark). Writing while a lease lives is the mutation-under-alias
    /// UB class the protocol exists to eliminate.
    fn payload_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, plus the caller-supplied refs == 0 proof that no
        // live `&[u8]` (lease) aliases the region.
        unsafe { std::slice::from_raw_parts_mut(self.payload_ptr, self.payload_len) }
    }
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

    // Payload memory lives in an Arc'd arena (not the worker-local Ent) so
    // FUSE_WRITE leases and `get_payload_buffer` pointers stay valid past
    // worker exit (§5.4).
    let arena = PayloadArena::new(depth, payload_sz, wake_fd)?;
    // One lease state per ring ent + the worker-local parked commit slots.
    let lease_states: Vec<Arc<EntLeaseState>> =
        (0..depth).map(|_| Arc::new(EntLeaseState::new())).collect();
    let mut parked_msgs: Vec<Option<CommitMsg>> = (0..depth).map(|_| None).collect();

    let mut ents: Vec<Ent> = (0..depth)
        .map(|idx| {
            let mut header = Box::new(FuseUringReqHeader::default());
            header.ring_ent_in_out.payload_sz = payload_sz as u32;
            Ent {
                header,
                payload_ptr: arena.buf(idx).expect("arena sized to depth"),
                payload_len: payload_sz,
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
            iov_base: ent.payload_ptr.cast(),
            iov_len: ent.payload_len,
        };
    }

    *pool.queues[qid as usize].arena.lock().unwrap() = Some(arena.clone());

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
        // Drain commits for this queue only (no demux). §5.4 re-arm gate: a
        // COMMIT_AND_FETCH both writes the reply into the ent payload and
        // re-arms the registered buffers for the kernel — never legal while
        // a payload lease is live. Gate every commit; park the message when
        // leased and rely on the lease drop's eventfd wake.
        while let Ok(msg) = commit_rx.try_recv() {
            let idx = msg.ent_idx as usize;
            if idx >= ents.len() {
                warn!("fuse-over-uring qid={qid}: commit for bad ent {idx}");
                continue;
            }
            match lease_states[idx].try_commit() {
                CommitGate::Ready => {
                    apply_reply(&mut ents[idx], &msg.header, &msg.reply_body);
                    push_cmd(
                        &mut ring,
                        FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
                        qid,
                        msg.commit_id,
                        None,
                        idx as u64,
                    )?;
                    ring.submit()?;
                }
                CommitGate::Parked => {
                    debug_assert!(
                        parked_msgs[idx].is_none(),
                        "two commits parked for one ring ent"
                    );
                    TRANSPORT_PARKED_COMMITS.fetch_add(1, Ordering::Relaxed);
                    parked_msgs[idx] = Some(msg);
                }
            }
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

        // Parked scan (runs on every wake path — lease-drop eventfd, new
        // CQEs, commit sends — and always before the worker can sleep in
        // submit_and_wait): un-park and commit every ent whose lease is
        // gone. try_unpark re-proves refs == 0, so the payload write below
        // cannot alias a live lease.
        for idx in 0..ents.len() {
            if parked_msgs[idx].is_some() && lease_states[idx].try_unpark() {
                let msg = parked_msgs[idx].take().expect("checked is_some");
                apply_reply(&mut ents[idx], &msg.header, &msg.reply_body);
                push_cmd(
                    &mut ring,
                    FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
                    qid,
                    msg.commit_id,
                    None,
                    idx as u64,
                )?;
                ring.submit()?;
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
                    // Delivery on this ent implies its previous commit passed
                    // the refs == 0 gate; header-only reply, payload untouched.
                    debug_assert!(!lease_states[ent_idx].leased());
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
                // FORGET payloads are copies (never leased) and this ent's
                // previous commit passed the refs == 0 gate: the immediate
                // auto-commit below cannot alias a live lease.
                debug_assert!(!lease_states[ent_idx].leased());
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
            let mut out = [0u8; 16];
            out[0..4].copy_from_slice(&16u32.to_le_bytes());
            out[4..8].copy_from_slice(&(-libc::EIO).to_le_bytes());
            out[8..16].copy_from_slice(&unique.to_le_bytes());
            // Header-only: apply_reply never touches the payload region when
            // the reply has no body beyond the 16-byte fuse_out_header.
            apply_reply(&mut ents[idx], &out, &Bytes::new());
        }
        let _ = push_cmd(
            &mut ring,
            FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
            qid,
            msg.commit_id,
            None,
            idx as u64,
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
        let n = extra.len().min(ent.payload_len);
        ent.payload_mut()[..n].copy_from_slice(&extra[..n]);
        payload_len = n;
    }

    let body_len = body.len().min(ent.payload_len - payload_len);
    if body_len > 0 && body.as_ptr() != unsafe { ent.payload_ptr.add(payload_len) as *const u8 } {
        ent.payload_mut()[payload_len..payload_len + body_len].copy_from_slice(&body[..body_len]);
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

    #[test]
    fn test_write_opcode_matches_abi() {
        assert_eq!(FUSE_WRITE_OPCODE, 16, "linux/fuse.h FUSE_WRITE");
    }

    /// Arena buffers: one stable, 4096-aligned, zeroed allocation per ring
    /// ent; out-of-range indexes refused; the dup'ed wake fd is distinct
    /// from (but signals) the original eventfd.
    #[test]
    fn test_payload_arena_buffers() {
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let efd_owned = unsafe { OwnedFd::from_raw_fd(efd) };
        let arena = PayloadArena::new(4, 8192, efd_owned.as_raw_fd()).unwrap();

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
        let w = unsafe {
            libc::write(
                arena.wake.as_raw_fd(),
                &one as *const u64 as *const _,
                8,
            )
        };
        assert_eq!(w, 8);
        let mut buf = [0u8; 8];
        let r = unsafe { libc::read(efd_owned.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
        assert_eq!(r, 8, "dup'ed wake fd must signal the queue eventfd");
    }

    /// A dropped payload lease releases its ref, records the outstanding
    /// gauge, and fires the queue eventfd when (and only when) a commit is
    /// parked.
    #[test]
    fn test_lease_drop_wakes_parked_worker() {
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(efd >= 0);
        let efd_owned = unsafe { OwnedFd::from_raw_fd(efd) };
        let arena = PayloadArena::new(1, 8192, efd_owned.as_raw_fd()).unwrap();
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
        assert_eq!(r, 8, "lease drop with a parked commit must fire the eventfd");
        assert!(state.try_unpark(), "commit releasable after the drop");
    }
}
