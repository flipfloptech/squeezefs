//! Kernel **FUSE-over-io_uring** (Linux 6.14+ / 7.x) — protocol from `linux/fuse.h`
//! and libfuse `fuse_uring.c`.
//!
//! # Enable
//! - Default: **attempt** when the kernel negotiated `FUSE_OVER_IO_URING`.
//! - Force off: `SQUEEZEFS_FUSE_OVER_IO_URING=0`
//! - Queue depth: `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` (default 8)
//!
//! # Model
//! Per-CPU (capped) worker threads each own an `IoUring` (SQE128), register the
//! fuse connection fd as fixed file 0, submit `FUSE_IO_URING_CMD_REGISTER` with
//! header+payload iovecs, then loop on CQEs.
//!
//! Inbound requests are pushed to a channel for the fuse3 session read path.
//! Replies call [`FuseOverUring::submit_reply`] which issues
//! `FUSE_IO_URING_CMD_COMMIT_AND_FETCH`.

#![cfg(all(target_os = "linux", feature = "tokio-runtime"))]

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
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
    pub payload: Vec<u8>,
    #[allow(dead_code)]
    pub unique: u64,
}

struct CommitMsg {
    qid: u16,
    ent_idx: u16,
    commit_id: u64,
    reply: Bytes,
}

/// Process-wide optional FUSE-over-io_uring controller.
pub struct FuseOverUring {
    active: AtomicBool,
    inbound_tx: SyncSender<InboundUringReq>,
    inbound_rx: Mutex<Option<Receiver<InboundUringReq>>>,
    pending: Mutex<HashMap<u64, (u16, u16, u64)>>,
    commit_tx: SyncSender<CommitMsg>,
    /// One commit receiver shared — workers demux by qid.
    commit_rx: Arc<Mutex<Receiver<CommitMsg>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    fuse_fd: RawFd,
}

static ACTIVE: AtomicU64 = AtomicU64::new(0);

/// How many FUSE-over-io_uring sessions are currently active (tests/metrics).
pub fn over_uring_sessions_active() -> u64 {
    ACTIVE.load(Ordering::Relaxed)
}

/// Opt-in until the transport is battle-tested on production mounts.
/// Set `SQUEEZEFS_FUSE_OVER_IO_URING=1` (or `true`/`on`) to enable.
pub fn want_fuse_over_uring() -> bool {
    match std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING") {
        Ok(v) => matches!(
            v.to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        ),
        Err(_) => false,
    }
}

impl FuseOverUring {
    pub fn try_start(fuse_fd: RawFd, max_write: usize) -> io::Result<Arc<Self>> {
        let nprocs = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 32);
        let depth = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8usize)
            .clamp(2, 32);
        let payload_sz = (max_write + 8192).max(128 * 1024);

        let (inbound_tx, inbound_rx) = sync_channel(depth * nprocs * 4);
        let (commit_tx, commit_rx) = sync_channel(depth * nprocs * 4);
        let commit_rx = Arc::new(Mutex::new(commit_rx));

        let pool = Arc::new(Self {
            active: AtomicBool::new(true),
            inbound_tx,
            inbound_rx: Mutex::new(Some(inbound_rx)),
            pending: Mutex::new(HashMap::new()),
            commit_tx,
            commit_rx,
            workers: Mutex::new(Vec::new()),
            fuse_fd,
        });

        let mut handles = Vec::new();
        for qid in 0..nprocs as u16 {
            let pool_c = pool.clone();
            let h = std::thread::Builder::new()
                .name(format!("fuse-over-uring-{qid}"))
                .spawn(move || {
                    if let Err(e) = queue_worker(pool_c, qid, depth, payload_sz) {
                        error!("fuse-over-uring worker qid={qid}: {e}");
                    }
                })
                .map_err(io::Error::other)?;
            handles.push(h);
        }
        *pool.workers.lock().unwrap() = handles;
        ACTIVE.fetch_add(1, Ordering::Relaxed);
        info!(
            "FUSE-over-io_uring active: queues={nprocs} depth={depth} payload_sz={payload_sz} fd={fuse_fd}"
        );
        Ok(pool)
    }

    pub fn take_inbound(&self) -> Option<Receiver<InboundUringReq>> {
        self.inbound_rx.lock().unwrap().take()
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
        self.commit_tx
            .send(CommitMsg {
                qid,
                ent_idx,
                commit_id,
                reply,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "uring commit closed"))?;
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        if self.active.swap(false, Ordering::Relaxed) {
            ACTIVE.fetch_sub(1, Ordering::Relaxed);
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

fn queue_worker(pool: Arc<FuseOverUring>, qid: u16, depth: usize, payload_sz: usize) -> io::Result<()> {
    let mut ring: Ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder()
        .setup_cqsize((depth as u32) * 2)
        .build(depth as u32 + 8)
        .map_err(|e| io::Error::other(format!(
            "SQE128 IoUring required for FUSE-over-io_uring: {e}"
        )))?;

    ring.submitter()
        .register_files(&[pool.fuse_fd])
        .map_err(|e| io::Error::other(format!("register_files: {e}")))?;

    let mut ents: Vec<Ent> = (0..depth)
        .map(|_| {
            let mut header = Box::new(FuseUringReqHeader::default());
            let mut payload = vec![0u8; payload_sz];
            let iov = [
                libc::iovec {
                    iov_base: (&mut *header as *mut FuseUringReqHeader).cast(),
                    iov_len: std::mem::size_of::<FuseUringReqHeader>(),
                },
                libc::iovec {
                    iov_base: payload.as_mut_ptr().cast(),
                    iov_len: payload_sz,
                },
            ];
            Ent {
                header,
                payload,
                iov,
            }
        })
        .collect();

    // Re-bind iov after moves into Vec
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
    }
    ring.submit()?;
    debug!("fuse-over-uring qid={qid}: REGISTER submitted for {depth} entries");

    while pool.active.load(Ordering::Relaxed) {
        // Process commits for this qid (with short timeout so we still wait CQEs)
        loop {
            let msg = {
                let rx = pool.commit_rx.lock().unwrap();
                match rx.recv_timeout(Duration::from_millis(0)) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        pool.shutdown();
                        return Ok(());
                    }
                }
            };
            if msg.qid != qid {
                // Other queue's commit — put back (may reorder; rare under load)
                let _ = pool.commit_tx.send(msg);
                break;
            }
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
        for (user_data, res) in completed {
            let ent_idx = user_data as usize;
            if res < 0 {
                let err = -res;
                if err == libc::EAGAIN || err == libc::EINTR {
                    if ent_idx < ents.len() {
                        resubmit.push(ent_idx);
                    }
                    continue;
                }
                if err == libc::ENOTSUP || err == libc::EINVAL {
                    error!(
                        "fuse-over-uring: kernel rejected protocol (err={err}); disabling"
                    );
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

            if pool
                .inbound_tx
                .send(InboundUringReq {
                    header_and_op,
                    payload,
                    unique,
                })
                .is_err()
            {
                pool.shutdown();
                break;
            }
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
        // io_uring_sqe layout (linux uapi): addr at +16, len at +24.
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
