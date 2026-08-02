//! The NVMe/TCP mini-initiator behind the zcrx read lane
//! (`docs/design-zcrx-read-lane.md` §4): one admin queue held for the
//! association's lifetime plus N IO queues, each an owned TCP connection with
//! a reader task (PDU state machine → completion by CID) and a writer task
//! (capsule serialization). PR Z1 receive backend is the classic in-process
//! stream reader (`read_exact` into the destination — copy-parity with the
//! kernel path, the CONTRACT venue); the zcrx area backend lands in PR Z2
//! behind the same seam (`docs/design-zcrx-read-lane.md` §5/§10).
//!
//! Read-only by construction: see `pdu` — no write/reservation encoder
//! exists, so the D0/fencing surface is unreachable from this module.

use super::pdu;
use crate::error::{Result, SqueezefsError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Resolved lane target: identity + geometry, derived at discovery
/// (`probe::nvme_tcp_target_for`) — tests construct it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneTarget {
    pub traddr: String,
    pub trsvcid: String,
    pub subnqn: String,
    pub nsid: u32,
    pub lba_shift: u32,
    /// Per-command transfer cap (the kernel-resolved MDTS face:
    /// `queue/max_hw_sectors_kb × 1024`).
    pub max_xfer_bytes: u32,
    pub io_queues: u16,
    /// Requested per-queue depth; clamped by the controller's CAP.MQES.
    pub queue_depth: u16,
}

fn io_err(msg: String) -> SqueezefsError {
    SqueezefsError::Io(std::io::Error::other(msg))
}

/// Raw destination pointer crossing into the reader task. SAFETY contract:
/// the pointee outlives the op (the caller awaits the op's oneshot before
/// releasing the buffer), and exactly one reader task writes any given
/// destination span (per-CID ownership).
struct SendMutPtr(*mut u8);
unsafe impl Send for SendMutPtr {}
unsafe impl Sync for SendMutPtr {}

struct Pending {
    dest: SendMutPtr,
    len: usize,
    received: u64,
    /// CapsuleResp or SUCCESS-elision seen (completion condition).
    tx: Option<oneshot::Sender<Result<()>>>,
}

struct QueueShared {
    pending: Mutex<std::collections::HashMap<u16, Pending>>,
    free_cids: Mutex<Vec<u16>>,
    cid_gate: tokio::sync::Semaphore,
    poisoned: AtomicBool,
}

impl QueueShared {
    fn poison(&self, why: &str, session_poison: &AtomicBool) {
        if !self.poisoned.swap(true, Ordering::SeqCst) {
            log::error!("zcrx-lane: IO queue poisoned: {why} — lane disarms, kernel path serves");
            session_poison.store(true, Ordering::SeqCst);
        }
        // Fail every waiter loud; their ops retry on the kernel path
        // (reads are idempotent — design §6).
        if let Ok(mut map) = self.pending.try_lock() {
            for (_, mut p) in map.drain() {
                if let Some(tx) = p.tx.take() {
                    let _ = tx.send(Err(io_err(format!("lane queue poisoned: {why}"))));
                }
            }
        }
    }
}

struct IoQueue {
    shared: Arc<QueueShared>,
    to_writer: mpsc::UnboundedSender<Vec<u8>>,
    /// Reader + writer task handles — the timeout quiescence law
    /// ([`LaneSession::read_segment`]) aborts AND joins them before any
    /// timed-out destination buffer is released, so a zombie reader can
    /// never write into recycled pool memory.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// One armed lane association to a target subsystem.
pub struct LaneSession {
    target: LaneTarget,
    queues: Vec<IoQueue>,
    next_q: AtomicUsize,
    poisoned: Arc<AtomicBool>,
    /// Keeps the admin connection (and thus the association) alive.
    _admin_hold: tokio::task::JoinHandle<()>,
}

impl Drop for LaneSession {
    fn drop(&mut self) {
        self._admin_hold.abort();
    }
}

impl std::fmt::Debug for LaneSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaneSession")
            .field("subnqn", &self.target.subnqn)
            .field("nsid", &self.target.nsid)
            .field("queues", &self.queues.len())
            .field("poisoned", &self.poisoned())
            .finish()
    }
}

/// Sequential request/response over the admin stream during bring-up (the
/// only admin traffic the lane ever issues; KATO 0 — design §4.2).
async fn admin_roundtrip(stream: &mut TcpStream, capsule: &[u8]) -> Result<pdu::Cqe> {
    stream
        .write_all(capsule)
        .await
        .map_err(|e| io_err(format!("admin capsule write: {e}")))?;
    loop {
        let mut ch_buf = [0u8; 8];
        stream
            .read_exact(&mut ch_buf)
            .await
            .map_err(|e| io_err(format!("admin CH read: {e}")))?;
        let ch = parse_ch(&ch_buf)?;
        match ch.pdu_type {
            pdu::PDU_CAPSULE_RESP => {
                let mut rest = vec![0u8; (ch.plen as usize).saturating_sub(8)];
                stream
                    .read_exact(&mut rest)
                    .await
                    .map_err(|e| io_err(format!("admin CQE read: {e}")))?;
                return pdu::parse_cqe(&rest).map_err(frame_err);
            }
            other => {
                return Err(io_err(format!(
                    "unexpected admin PDU type {other:#x} during bring-up"
                )));
            }
        }
    }
}

fn parse_ch(b: &[u8; 8]) -> Result<pdu::CommonHdr> {
    pdu::parse_common(b).map_err(frame_err)
}

fn frame_err(e: pdu::FrameError) -> SqueezefsError {
    crate::fuse_client::METRICS
        .zcrx_frame_violations
        .fetch_add(1, Ordering::Relaxed);
    io_err(format!("NVMe/TCP framing violation: {e}"))
}

async fn ic_exchange(stream: &mut TcpStream) -> Result<pdu::IcResp> {
    stream
        .write_all(&pdu::encode_icreq())
        .await
        .map_err(|e| io_err(format!("ICReq write: {e}")))?;
    let mut resp = [0u8; 128];
    stream
        .read_exact(&mut resp)
        .await
        .map_err(|e| io_err(format!("ICResp read: {e}")))?;
    pdu::parse_icresp(&resp).map_err(frame_err)
}

fn check_status(op: &str, cqe: &pdu::Cqe) -> Result<()> {
    if cqe.status != 0 {
        return Err(io_err(format!(
            "{op} failed: controller status {:#06x}",
            cqe.status
        )));
    }
    Ok(())
}

/// Which receive backend a lane queue runs (design §5/§6/§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneBackend {
    /// Classic in-process recv into the destination (copy-parity with the
    /// kernel path — the PR Z1 contract venue; `FORCE_COPY` seam).
    Classic,
    /// The PR Z2 area backend with socket recv simulating NIC DMA into
    /// area chunks (`AREA_SIM` seam — the chunk-parser/refill/gather
    /// contract venue; never a product posture).
    AreaSim,
}

impl LaneSession {
    /// [`Self::connect`] with an explicit receive backend.
    pub async fn connect_with(
        target: LaneTarget,
        backend: LaneBackend,
    ) -> Result<Arc<LaneSession>> {
        match backend {
            LaneBackend::Classic => Self::connect(target).await,
            LaneBackend::AreaSim => Err(io_err(
                "zcrx area backend not implemented (PR Z2 phase A)".into(),
            )),
        }
    }

    /// Area-chunk diagnostics `(free, total)` summed over the session's
    /// area queues — the refill-discipline instrument (0, 0) on classic
    /// backends (no area exists).
    pub fn area_chunks(&self) -> (usize, usize) {
        (0, 0) // Z2 phase A stub — contracts red
    }

    /// Abort AND join every queue task — the poison-drain quiescence law
    /// (design §7): after this returns no lane task holds destination
    /// pointers or area-chunk refs.
    pub async fn quiesce(&self) {
        for q in &self.queues {
            let mut ts = q.tasks.lock().await;
            for t in ts.iter() {
                t.abort();
            }
            for t in ts.drain(..) {
                let _ = t.await;
            }
        }
    }

    /// Bring up the association (design §4.2): ICReq/ICResp (digests-off
    /// law) → admin Connect (cntlid 0xFFFF, KATO 0) → CAP → CC.EN →
    /// CSTS.RDY → per-queue IO Connect. Every failure is loud and leaves
    /// the caller on the kernel path.
    pub async fn connect(target: LaneTarget) -> Result<Arc<LaneSession>> {
        let (hostnqn, hostid) = super::probe::host_identity();
        let addr = format!("{}:{}", target.traddr, target.trsvcid);

        let mut admin = TcpStream::connect(&addr)
            .await
            .map_err(|e| io_err(format!("lane admin connect {addr}: {e}")))?;
        admin.set_nodelay(true).ok();
        ic_exchange(&mut admin).await?;

        let connect = pdu::encode_connect_capsule(
            0,
            31, // admin SQSIZE (0-based) — bring-up only, never data depth
            0,  // KATO 0: keep-alive disabled (design §4.2 / residual 4)
            0,
            &hostid,
            0xFFFF,
            &target.subnqn,
            &hostnqn,
        );
        let cqe = admin_roundtrip(&mut admin, &connect).await?;
        check_status("admin Connect", &cqe)?;
        let cntlid = (cqe.dw0 & 0xFFFF) as u16;

        let cap = admin_roundtrip(
            &mut admin,
            &pdu::encode_property_get(1, pdu::PROP_CAP, true),
        )
        .await?;
        check_status("Property Get CAP", &cap)?;
        let mqes = (cap.dw0 & 0xFFFF) as u16; // 0-based max queue entries

        let set_cc = pdu::encode_property_set(2, pdu::PROP_CC, pdu::CC_ENABLE_NVM, false);
        let cqe = admin_roundtrip(&mut admin, &set_cc).await?;
        check_status("Property Set CC.EN", &cqe)?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let csts = admin_roundtrip(
                &mut admin,
                &pdu::encode_property_get(3, pdu::PROP_CSTS, false),
            )
            .await?;
            check_status("Property Get CSTS", &csts)?;
            if csts.dw0 & 1 == 1 {
                break;
            }
            if csts.dw0 & 0b10 != 0 {
                return Err(io_err("controller reports CFS during lane enable".into()));
            }
            if std::time::Instant::now() > deadline {
                return Err(io_err("controller never reached CSTS.RDY (10 s)".into()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let poisoned = Arc::new(AtomicBool::new(false));
        let depth = target.queue_depth.min(mqes.saturating_add(1)).max(2);
        let mut queues = Vec::with_capacity(target.io_queues as usize);
        for qid in 1..=target.io_queues {
            let mut s = TcpStream::connect(&addr)
                .await
                .map_err(|e| io_err(format!("lane IO queue {qid} connect: {e}")))?;
            s.set_nodelay(true).ok();
            ic_exchange(&mut s).await?;
            let connect = pdu::encode_connect_capsule(
                qid,
                depth - 1,
                0,
                0,
                &hostid,
                cntlid,
                &target.subnqn,
                &hostnqn,
            );
            let cqe = admin_roundtrip(&mut s, &connect).await?;
            check_status("IO Connect", &cqe)?;
            queues.push(spawn_queue(s, depth, Arc::clone(&poisoned)));
        }

        // Park the admin socket: any read (data or EOF) after bring-up is an
        // association event — poison loud, the kernel path keeps serving.
        let admin_poison = Arc::clone(&poisoned);
        let admin_hold = tokio::spawn(async move {
            let mut b = [0u8; 8];
            match admin.read(&mut b).await {
                Ok(0) => log::error!(
                    "zcrx-lane: admin connection closed by target — session poisoned \
                     (kernel path serves; remount re-arms)"
                ),
                Ok(_) => {
                    log::error!("zcrx-lane: unexpected admin PDU after bring-up — session poisoned")
                }
                Err(e) => log::error!("zcrx-lane: admin connection error: {e} — session poisoned"),
            }
            admin_poison.store(true, Ordering::SeqCst);
        });

        log::info!(
            "zcrx-lane armed: {} nsid={} lba_shift={} queues={} depth={} max_xfer={} (backend: classic-recv contract venue — PR Z1)",
            target.subnqn,
            target.nsid,
            target.lba_shift,
            target.io_queues,
            depth,
            target.max_xfer_bytes,
        );
        Ok(Arc::new(LaneSession {
            target,
            queues,
            next_q: AtomicUsize::new(0),
            poisoned,
            _admin_hold: admin_hold,
        }))
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    pub fn target(&self) -> &LaneTarget {
        &self.target
    }

    /// Byte-range eligibility (design §6): LBA-aligned, nonzero.
    pub fn range_eligible(&self, byte_offset: u64, len: usize) -> bool {
        let mask = (1u64 << self.target.lba_shift) - 1;
        len > 0 && byte_offset & mask == 0 && (len as u64) & mask == 0
    }

    /// Read `len` bytes at `byte_offset` into `dest`, chunked at the target's
    /// transfer cap, sub-commands issued concurrently (idempotent reads).
    ///
    /// SAFETY: `dest..dest+len` must be writable and outlive this call; the
    /// caller keeps the backing allocation alive across the await (pooled
    /// `Bytes` in the funnel, `&mut [u8]` in [`Self::read_into_slice`]).
    /// The pointer is wrapped before the async body so the returned future
    /// stays `Send` (funnel callers run on the multi-thread runtime).
    pub fn read_into_ptr(
        &self,
        byte_offset: u64,
        dest: *mut u8,
        len: usize,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let dest = SendMutPtr(dest);
        self.read_into_wrapped(byte_offset, dest, len)
    }

    async fn read_into_wrapped(
        &self,
        byte_offset: u64,
        dest: SendMutPtr,
        len: usize,
    ) -> Result<()> {
        if self.poisoned() {
            return Err(io_err("lane session poisoned".into()));
        }
        if !self.range_eligible(byte_offset, len) {
            return Err(io_err(format!(
                "lane read not LBA-aligned: offset={byte_offset} len={len} shift={}",
                self.target.lba_shift
            )));
        }
        let cap = (self.target.max_xfer_bytes as usize).max(1 << self.target.lba_shift);
        let mut futs = Vec::with_capacity(len.div_ceil(cap));
        let mut done = 0usize;
        while done < len {
            let seg = cap.min(len - done);
            let seg_off = byte_offset + done as u64;
            let seg_dest = SendMutPtr(unsafe { dest.0.add(done) });
            futs.push(self.read_segment(seg_off, seg_dest, seg));
            done += seg;
        }
        for r in futures::future::join_all(futs).await {
            r?;
        }
        Ok(())
    }

    /// Slice-destination convenience (tests + non-pooled callers).
    pub async fn read_into_slice(&self, byte_offset: u64, dest: &mut [u8]) -> Result<()> {
        self.read_into_ptr(byte_offset, dest.as_mut_ptr(), dest.len())
            .await
    }

    async fn read_segment(&self, byte_offset: u64, dest: SendMutPtr, len: usize) -> Result<()> {
        let q = &self.queues[self.next_q.fetch_add(1, Ordering::Relaxed) % self.queues.len()];
        if q.shared.poisoned.load(Ordering::SeqCst) {
            return Err(io_err("lane queue poisoned".into()));
        }
        let _permit = q
            .shared
            .cid_gate
            .acquire()
            .await
            .map_err(|_| io_err("lane queue closed".into()))?;
        let cid = q
            .shared
            .free_cids
            .lock()
            .await
            .pop()
            .ok_or_else(|| io_err("lane CID pool exhausted (permit/pool desync)".into()))?;

        let (tx, rx) = oneshot::channel();
        q.shared.pending.lock().await.insert(
            cid,
            Pending {
                dest,
                len,
                received: 0,
                tx: Some(tx),
            },
        );

        let slba = byte_offset >> self.target.lba_shift;
        let nlb = (len >> self.target.lba_shift) as u32;
        let capsule = pdu::encode_read_capsule(cid, self.target.nsid, slba, nlb, len as u32);
        if q.to_writer.send(capsule).is_err() {
            q.shared.pending.lock().await.remove(&cid);
            q.shared.free_cids.lock().await.push(cid);
            return Err(io_err("lane writer task gone".into()));
        }

        let res = match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(io_err("lane completion channel dropped".into())),
            Err(_) => {
                // Quiescence law: the reader task holds raw pointers into
                // this (and other) destination buffers. Before this frame
                // returns — and the caller's buffer can be dropped/recycled
                // — abort AND join the queue's tasks so no writer survives.
                q.shared.poisoned.store(true, Ordering::SeqCst);
                self.poisoned.store(true, Ordering::SeqCst);
                let mut ts = q.tasks.lock().await;
                for t in ts.iter() {
                    t.abort();
                }
                for t in ts.drain(..) {
                    let _ = t.await;
                }
                drop(ts);
                q.shared.poison("read timed out after 30 s", &self.poisoned);
                Err(io_err(format!(
                    "lane read timed out (offset={byte_offset}, len={len})"
                )))
            }
        };
        if !q.shared.poisoned.load(Ordering::SeqCst) {
            q.shared.free_cids.lock().await.push(cid);
        }
        res
    }
}

fn spawn_queue(stream: TcpStream, depth: u16, session_poison: Arc<AtomicBool>) -> IoQueue {
    let shared = Arc::new(QueueShared {
        pending: Mutex::new(std::collections::HashMap::new()),
        free_cids: Mutex::new((0..depth).collect()),
        cid_gate: tokio::sync::Semaphore::new(depth as usize),
        poisoned: AtomicBool::new(false),
    });
    let (read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::unbounded_channel();

    let s2 = Arc::clone(&shared);
    let p2 = Arc::clone(&session_poison);
    let writer = tokio::spawn(async move {
        if let Err(why) = writer_loop(write_half, rx).await {
            s2.poison(&why, &p2);
        }
    });
    let s3 = Arc::clone(&shared);
    let reader = tokio::spawn(async move {
        if let Err(why) = reader_loop(read_half, &s3).await {
            s3.poison(&why, &session_poison);
        }
    });

    IoQueue {
        shared,
        to_writer: tx,
        tasks: Mutex::new(vec![reader, writer]),
    }
}

async fn writer_loop(
    mut w: OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> std::result::Result<(), String> {
    while let Some(capsule) = rx.recv().await {
        w.write_all(&capsule)
            .await
            .map_err(|e| format!("capsule write: {e}"))?;
    }
    Ok(())
}

/// The per-queue PDU state machine. C2HData payload is read **directly into
/// the destination span** (one kernel copy — parity with the kernel
/// initiator; the Z2 zcrx backend replaces this with area-chunk refs behind
/// the same completion law).
async fn reader_loop(
    mut r: OwnedReadHalf,
    shared: &QueueShared,
) -> std::result::Result<(), String> {
    let metrics = &crate::fuse_client::METRICS;
    let mut hdr = [0u8; 128];
    loop {
        if let Err(e) = r.read_exact(&mut hdr[..8]).await {
            // EOF with nothing pending = orderly teardown.
            if shared.pending.lock().await.is_empty()
                && e.kind() == std::io::ErrorKind::UnexpectedEof
            {
                return Ok(());
            }
            return Err(format!("CH read: {e}"));
        }
        let ch = pdu::parse_common(&hdr[..8]).map_err(|e| e.to_string())?;
        let hlen = ch.hlen as usize;
        if !(8..=128).contains(&hlen) {
            metrics
                .zcrx_frame_violations
                .fetch_add(1, Ordering::Relaxed);
            return Err(format!("CH hlen {hlen} out of range"));
        }
        r.read_exact(&mut hdr[8..hlen])
            .await
            .map_err(|e| format!("PSH read: {e}"))?;

        match ch.pdu_type {
            pdu::PDU_C2H_DATA => {
                let c2h = pdu::parse_c2h_data(ch, &hdr[..hlen]).map_err(|e| {
                    metrics
                        .zcrx_frame_violations
                        .fetch_add(1, Ordering::Relaxed);
                    e.to_string()
                })?;
                // PDO padding between header end and payload start.
                let mut pad = ch.pdo as usize - hlen;
                while pad > 0 {
                    let n = pad.min(hdr.len());
                    r.read_exact(&mut hdr[..n])
                        .await
                        .map_err(|e| format!("PDO pad read: {e}"))?;
                    pad -= n;
                }
                metrics
                    .zcrx_hdr_copy_bytes
                    .fetch_add(ch.pdo as u64, Ordering::Relaxed);

                let mut map = shared.pending.lock().await;
                let p = match map.get_mut(&c2h.cccid) {
                    Some(p) => p,
                    None => {
                        metrics
                            .zcrx_frame_violations
                            .fetch_add(1, Ordering::Relaxed);
                        return Err(format!("C2HData for unknown CID {}", c2h.cccid));
                    }
                };
                let end = c2h.datao as usize + c2h.datal as usize;
                if end > p.len {
                    metrics
                        .zcrx_frame_violations
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(format!(
                        "C2HData span {}..{} exceeds command length {}",
                        c2h.datao, end, p.len
                    ));
                }
                // SAFETY: per-CID span ownership (struct contract) — the
                // destination outlives the op and only this reader writes it.
                let span = unsafe {
                    std::slice::from_raw_parts_mut(
                        p.dest.0.add(c2h.datao as usize),
                        c2h.datal as usize,
                    )
                };
                drop(map); // never hold the map lock across socket I/O
                r.read_exact(span)
                    .await
                    .map_err(|e| format!("C2HData payload read: {e}"))?;

                let mut map = shared.pending.lock().await;
                if let Some(p) = map.get_mut(&c2h.cccid) {
                    p.received += c2h.datal as u64;
                    if c2h.success {
                        if !c2h.last {
                            metrics
                                .zcrx_frame_violations
                                .fetch_add(1, Ordering::Relaxed);
                            return Err("SUCCESS on a non-LAST C2HData".into());
                        }
                        let mut p = map.remove(&c2h.cccid).expect("checked above");
                        complete(&mut p, metrics);
                    }
                }
            }
            pdu::PDU_CAPSULE_RESP => {
                // The 16-byte CQE IS the PSH (hlen 24 = 8 CH + 16 CQE) —
                // reading past it desynchronizes the stream (the bug the
                // contract suite caught red: every CapsuleResp-completed
                // read hung to timeout while SUCCESS-elision paths passed).
                if hlen < 24 {
                    metrics
                        .zcrx_frame_violations
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(format!("CapsuleResp hlen {hlen} < 24"));
                }
                let cqe = pdu::parse_cqe(&hdr[8..24]).map_err(|e| e.to_string())?;
                let mut map = shared.pending.lock().await;
                let Some(mut p) = map.remove(&cqe.cid) else {
                    metrics
                        .zcrx_frame_violations
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(format!("CapsuleResp for unknown CID {}", cqe.cid));
                };
                if cqe.status != 0 {
                    if let Some(tx) = p.tx.take() {
                        let _ = tx.send(Err(io_err(format!(
                            "lane read failed: controller status {:#06x}",
                            cqe.status
                        ))));
                    }
                } else {
                    complete(&mut p, metrics);
                }
            }
            other => {
                metrics
                    .zcrx_frame_violations
                    .fetch_add(1, Ordering::Relaxed);
                return Err(format!("unexpected PDU type {other:#x} on IO queue"));
            }
        }
    }
}

/// Completion law: a successful read must have landed EXACTLY the command's
/// bytes (the nvme_dev exact-length contract, VL8 item 5 — short data with a
/// success status is a framing violation, never partial data).
fn complete(p: &mut Pending, metrics: &crate::fuse_client::Metrics) {
    let ok = p.received == p.len as u64;
    if !ok {
        metrics
            .zcrx_frame_violations
            .fetch_add(1, Ordering::Relaxed);
    }
    if let Some(tx) = p.tx.take() {
        let _ = tx.send(if ok {
            Ok(())
        } else {
            Err(io_err(format!(
                "lane read completed with {} of {} bytes",
                p.received, p.len
            )))
        });
    }
}
