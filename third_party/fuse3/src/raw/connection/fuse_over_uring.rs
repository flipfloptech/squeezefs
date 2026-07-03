//! Kernel **FUSE-over-io_uring** (Linux 6.14+ / 7.x) — `linux/fuse.h` + libfuse `fuse_uring.c`.
//!
//! # Enable (default on)
//! ```text
//! # default: attempt FUSE-over-io_uring after INIT; fall back to classical on failure
//! SQUEEZEFS_FUSE_OVER_IO_URING=0           # force classical /dev/fuse path only
//! SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH=8   # optional, per-queue depth
//! SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES=N    # optional, default = min(nproc, 32)
//! ```
//!
//! # Design (hardened)
//! - **Per-qid commit channel** — no shared demux / re-queue races.
//! - **Shared inbound work queue** + condvar — multi-queue session workers all pop requests.
//! - **eventfd** per queue — wake workers on commit / shutdown (no busy poll).
//! - Metrics: register/commit/cqe errors, inflight uniques.

#![cfg(all(target_os = "linux", feature = "tokio-runtime"))]

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
    pub payload: Vec<u8>,
    pub unique: u64,
}

struct CommitMsg {
    ent_idx: u16,
    commit_id: u64,
    reply: Bytes,
}

struct QueueHandle {
    commit_tx: std::sync::mpsc::SyncSender<CommitMsg>,
    /// Wake the queue thread (commit or shutdown).
    wake_fd: RawFd,
    /// Keep OwnedFd alive.
    _wake: OwnedFd,
}

/// Shared work queue for all session workers (primary + multi-queue clones).
struct InboundQueue {
    q: Mutex<VecDeque<InboundUringReq>>,
    cv: Condvar,
}

impl InboundQueue {
    fn new() -> Self {
        Self {
            q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
        }
    }

    fn push(&self, req: InboundUringReq) {
        self.q.lock().unwrap().push_back(req);
        self.cv.notify_one();
    }

    /// Blocking pop with timeout; returns None if inactive and empty.
    fn pop_timeout(&self, active: &AtomicBool, timeout: Duration) -> Option<InboundUringReq> {
        let mut g = self.q.lock().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(r) = g.pop_front() {
                return Some(r);
            }
            if !active.load(Ordering::Relaxed) {
                return None;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let (gg, _) = self
                .cv
                .wait_timeout(g, deadline.saturating_duration_since(now))
                .unwrap();
            g = gg;
        }
    }

    fn notify_all(&self) {
        self.cv.notify_all();
    }
}

/// Process-wide optional FUSE-over-io_uring controller (one per fuse session / mount).
pub struct FuseOverUring {
    active: AtomicBool,
    inbound: Arc<InboundQueue>,
    /// unique → (qid, ent_idx, commit_id)
    pending: Mutex<HashMap<u64, (u16, u16, u64)>>,
    queues: Vec<QueueHandle>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    fuse_fd: RawFd,
    // metrics
    pub stats_requests: AtomicU64,
    pub stats_replies: AtomicU64,
    pub stats_cqe_err: AtomicU64,
    pub stats_register: AtomicU64,
}

static ACTIVE_SESSIONS: AtomicU64 = AtomicU64::new(0);

pub fn over_uring_sessions_active() -> u64 {
    ACTIVE_SESSIONS.load(Ordering::Relaxed)
}

/// Default **on**. Opt out with `SQUEEZEFS_FUSE_OVER_IO_URING=0|false|off|no`.
/// Setup still falls back to the classical path if the kernel rejects the protocol.
pub fn want_fuse_over_uring() -> bool {
    match std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING") {
        Ok(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

impl FuseOverUring {
    pub fn try_start(fuse_fd: RawFd, max_write: usize) -> io::Result<Arc<Self>> {
        let nqueues = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            })
            .clamp(1, 32);
        let depth = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8usize)
            .clamp(2, 32);
        let payload_sz = (max_write + 8192).max(128 * 1024);

        let inbound = Arc::new(InboundQueue::new());
        let mut queue_handles = Vec::with_capacity(nqueues);
        let mut commit_rxs = Vec::with_capacity(nqueues);
        let mut wake_fds = Vec::with_capacity(nqueues);

        for _ in 0..nqueues {
            let (commit_tx, commit_rx) = std::sync::mpsc::sync_channel(depth * 4);
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
            });
            commit_rxs.push(commit_rx);
        }

        let pool = Arc::new(Self {
            active: AtomicBool::new(true),
            inbound,
            pending: Mutex::new(HashMap::new()),
            queues: queue_handles,
            workers: Mutex::new(Vec::new()),
            fuse_fd,
            stats_requests: AtomicU64::new(0),
            stats_replies: AtomicU64::new(0),
            stats_cqe_err: AtomicU64::new(0),
            stats_register: AtomicU64::new(0),
        });

        // Probe one ring before spawning all workers (fail fast if kernel rejects).
        probe_kernel_support(fuse_fd, payload_sz)?;

        let mut handles = Vec::new();
        for qid in 0..nqueues as u16 {
            let pool_c = pool.clone();
            let commit_rx = commit_rxs.remove(0);
            let wake_fd = wake_fds[qid as usize];
            let h = std::thread::Builder::new()
                .name(format!("fuse-over-uring-{qid}"))
                .spawn(move || {
                    if let Err(e) =
                        queue_worker(pool_c, qid, depth, payload_sz, commit_rx, wake_fd)
                    {
                        error!("fuse-over-uring worker qid={qid}: {e}");
                    }
                })
                .map_err(io::Error::other)?;
            handles.push(h);
        }
        *pool.workers.lock().unwrap() = handles;
        ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
        info!(
            "FUSE-over-io_uring active: queues={nqueues} depth={depth} payload_sz={payload_sz} fd={fuse_fd}"
        );
        Ok(pool)
    }

    /// Blocking pop for session read path (works from multi-queue workers).
    pub fn recv_inbound_timeout(&self, timeout: Duration) -> Option<InboundUringReq> {
        self.inbound.pop_timeout(&self.active, timeout)
    }

    pub fn submit_reply(&self, unique: u64, reply: Bytes) -> io::Result<()> {
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
                reply,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring commit closed"))?;
        // Wake queue thread
        let one: u64 = 1;
        let _ = unsafe { libc::write(q.wake_fd, &one as *const u64 as *const _, 8) };
        self.stats_replies.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        if self.active.swap(false, Ordering::Relaxed) {
            ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
            self.inbound.notify_all();
            let one: u64 = 1;
            for q in &self.queues {
                let _ = unsafe { libc::write(q.wake_fd, &one as *const u64 as *const _, 8) };
            }
        }
    }

    pub fn stats_snapshot(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.stats_requests.load(Ordering::Relaxed),
            self.stats_replies.load(Ordering::Relaxed),
            self.stats_cqe_err.load(Ordering::Relaxed),
            self.stats_register.load(Ordering::Relaxed),
            self.pending.lock().unwrap().len() as u64,
        )
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

/// Fail fast if the kernel cannot accept a REGISTER cmd on this fuse fd.
fn probe_kernel_support(fuse_fd: RawFd, payload_sz: usize) -> io::Result<()> {
    let mut ring: Ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder()
        .build(8)
        .map_err(|e| io::Error::other(format!("probe ring: {e}")))?;
    ring.submitter()
        .register_files(&[fuse_fd])
        .map_err(|e| io::Error::other(format!("probe register_files: {e}")))?;

    let mut header = Box::new(FuseUringReqHeader::default());
    let mut payload = vec![0u8; payload_sz.min(4096)];
    let iov = [
        libc::iovec {
            iov_base: (&mut *header as *mut FuseUringReqHeader).cast(),
            iov_len: std::mem::size_of::<FuseUringReqHeader>(),
        },
        libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: payload.len(),
        },
    ];
    push_cmd(
        &mut ring,
        FUSE_IO_URING_CMD_REGISTER,
        0,
        0,
        Some((iov.as_ptr(), 2)),
        0,
    )?;
    ring.submit_and_wait(1)?;
    let mut cq = ring.completion();
    cq.sync();
    if let Some(cqe) = cq.next() {
        let res = cqe.result();
        if res < 0 {
            let err = -res;
            // EAGAIN is ok (no request yet after register)
            if err != libc::EAGAIN && err != 0 {
                if err == libc::ENOTSUP || err == libc::EINVAL || err == libc::ENOSYS {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("kernel rejected FUSE URING_CMD REGISTER (err={err})"),
                    ));
                }
                // Other errors: still try full start (some kernels return odd codes once)
                warn!("fuse-over-uring probe REGISTER returned {err}; continuing");
            }
        }
    }
    // Keep header/payload alive until after submit_and_wait
    drop(header);
    drop(payload);
    Ok(())
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

    let mut ring: Ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder()
        .setup_cqsize((depth as u32) * 2)
        .build(depth as u32 + 8)
        .map_err(|e| io::Error::other(format!("SQE128 IoUring: {e}")))?;

    ring.submitter()
        .register_files(&[pool.fuse_fd, wake_fd])
        .map_err(|e| io::Error::other(format!("register_files: {e}")))?;

    let mut ents: Vec<Ent> = (0..depth)
        .map(|_| {
            let header = Box::new(FuseUringReqHeader::default());
            let payload = vec![0u8; payload_sz];
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

    for (idx, ent) in ents.iter().enumerate() {
        push_cmd(
            &mut ring,
            FUSE_IO_URING_CMD_REGISTER,
            qid,
            0,
            Some((ent.iov.as_ptr(), 2)),
            idx as u64,
        )?;
        pool.stats_register.fetch_add(1, Ordering::Relaxed);
    }
    // Poll wake_fd (fixed index 1) so commits/shutdown wake submit_and_wait
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
    ring.submit()?;
    debug!("fuse-over-uring qid={qid}: registered {depth} entries + wake poll");

    while pool.active.load(Ordering::Relaxed) {
        // Drain commits for this queue only (no demux)
        while let Ok(msg) = commit_rx.try_recv() {
            let ent = &mut ents[msg.ent_idx as usize];
            apply_reply(ent, &msg.reply);
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

        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => return Err(e),
        }

        let completed: Vec<(u64, i32)> = {
            let mut cq = ring.completion();
            cq.sync();
            cq.map(|c| (c.user_data(), c.result())).collect()
        };

        let mut resubmit = Vec::new();
        let mut need_repoll = false;
        for (user_data, res) in completed {
            if user_data == u64::MAX {
                // wake_fd poll completed — re-arm
                need_repoll = true;
                continue;
            }
            let ent_idx = user_data as usize;
            if res < 0 {
                let err = -res;
                pool.stats_cqe_err.fetch_add(1, Ordering::Relaxed);
                if err == libc::EAGAIN || err == libc::EINTR {
                    if ent_idx < ents.len() {
                        resubmit.push(ent_idx);
                    }
                    continue;
                }
                if err == libc::ENOTSUP || err == libc::EINVAL || err == libc::ENOSYS {
                    error!("fuse-over-uring: kernel rejected protocol err={err}");
                    pool.shutdown();
                    return Err(io::Error::from_raw_os_error(err));
                }
                warn!("fuse-over-uring qid={qid} cqe err={err} ent={ent_idx}");
                continue;
            }
            if ent_idx >= ents.len() {
                continue;
            }
            let ent = &ents[ent_idx];
            let commit_id = ent.header.ring_ent_in_out.commit_id;
            if commit_id == 0 {
                continue;
            }
            let unique = u64::from_ne_bytes(ent.header.in_out[8..16].try_into().unwrap());
            let payload_sz = ent.header.ring_ent_in_out.payload_sz as usize;
            let mut header_and_op =
                Vec::with_capacity(FUSE_IN_HEADER_SIZE + FUSE_URING_OP_IN_OUT_SZ);
            header_and_op.extend_from_slice(&ent.header.in_out[..FUSE_IN_HEADER_SIZE]);
            header_and_op.extend_from_slice(&ent.header.op_in);
            let payload = ent.payload[..payload_sz.min(ent.payload.len())].to_vec();

            pool.pending
                .lock()
                .unwrap()
                .insert(unique, (qid, ent_idx as u16, commit_id));
            pool.stats_requests.fetch_add(1, Ordering::Relaxed);
            pool.inbound.push(InboundUringReq {
                header_and_op,
                payload,
                unique,
            });
        }
        if need_repoll {
            let poll_e = opcode::PollAdd::new(types::Fixed(1), libc::POLLIN as _)
                .build()
                .user_data(u64::MAX);
            let _ = unsafe { ring.submission().push(&Entry128::from(poll_e)) };
            let _ = ring.submit();
        }
        if !resubmit.is_empty() {
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
    Ok(())
}

fn apply_reply(ent: &mut Ent, reply: &Bytes) {
    let hdr_n = reply.len().min(FUSE_URING_IN_OUT_HEADER_SZ);
    ent.header.in_out[..hdr_n].copy_from_slice(&reply[..hdr_n]);
    if reply.len() > FUSE_URING_IN_OUT_HEADER_SZ {
        let body = &reply[FUSE_URING_IN_OUT_HEADER_SZ..];
        let n = body.len().min(ent.payload.len());
        ent.payload[..n].copy_from_slice(&body[..n]);
        ent.header.ring_ent_in_out.payload_sz = n as u32;
    } else {
        ent.header.ring_ent_in_out.payload_sz = 0;
    }
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
    unsafe {
        std::ptr::write(cmd.as_mut_ptr().cast::<FuseUringCmdReq>(), req);
    }

    let mut entry: Entry128 = opcode::UringCmd80::new(types::Fixed(0), cmd_op)
        .cmd(cmd)
        .build()
        .user_data(user_data);

    if let Some((ptr, len)) = iov {
        // libfuse: sqe.addr = iov, sqe.len = count for REGISTER.
        // io_uring_sqe: addr @ +16, len @ +24.
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
    fn test_want_env_opt_in() {
        // Don't assert global env; just ensure function is callable.
        let _ = want_fuse_over_uring();
    }

    #[test]
    fn test_header_sizes() {
        assert_eq!(std::mem::size_of::<FuseUringReqHeader>(), 128 + 128 + 32);
        assert_eq!(std::mem::size_of::<FuseUringCmdReq>(), 24);
    }
}
