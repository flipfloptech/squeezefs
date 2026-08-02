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

/// The one session-poison transition (design §7 + the Z2 poison lattice):
/// idempotent; the winning transition counts the `zcrx_lane_poisoned`
/// tripwire and drops the `zcrx_lane_armed` gauge — the funnel routes
/// every subsequent op to the kernel path for the mount lifetime.
pub(crate) fn mark_session_poisoned(flag: &AtomicBool) {
    if !flag.swap(true, Ordering::SeqCst) {
        let m = &crate::fuse_client::METRICS;
        m.zcrx_lane_poisoned.fetch_add(1, Ordering::Relaxed);
        m.zcrx_lane_armed.store(0, Ordering::Relaxed);
    }
}

/// Raw destination pointer crossing into the reader task. SAFETY contract
/// (MEM-3 custody law): the pointee outlives every lane write because
/// EITHER the caller awaits the op's oneshot before releasing the buffer
/// (the timeout arm aborts AND joins the queue tasks first), OR the
/// pending entry owns a keep-alive on the destination allocation
/// ([`Pending::_keepalive`] — the [`LaneSession::read_into_pooled`] arm),
/// so a cancelled requester future can never let the allocation recycle
/// while the reader can still write it. Exactly one reader task writes
/// any given destination span (per-CID ownership).
struct SendMutPtr(*mut u8);
unsafe impl Send for SendMutPtr {}
unsafe impl Sync for SendMutPtr {}

/// RAII custody of one CID + its depth permit (MEM-3): held by the
/// PENDING ENTRY, not the requester — the CID and its `cid_gate` permit
/// return to the pool exactly when the entry is destroyed (driver
/// completion, send-failure cleanup, or poison drain), never on the
/// requester's happy path. A cancelled requester future therefore leaks
/// nothing: the entry survives it and the driver's completion returns
/// the custody.
pub(crate) struct CidSlot {
    cid: u16,
    pool: Arc<std::sync::Mutex<Vec<u16>>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl CidSlot {
    pub(crate) fn cid(&self) -> u16 {
        self.cid
    }
}

impl Drop for CidSlot {
    fn drop(&mut self) {
        self.pool.lock().expect("cid pool lock").push(self.cid);
    }
}

/// Acquire a depth permit + pop a CID as one RAII unit (see [`CidSlot`]).
pub(crate) async fn take_cid(
    gate: &Arc<tokio::sync::Semaphore>,
    pool: &Arc<std::sync::Mutex<Vec<u16>>>,
) -> Result<CidSlot> {
    let permit = Arc::clone(gate)
        .acquire_owned()
        .await
        .map_err(|_| io_err("lane queue closed".into()))?;
    let cid = pool
        .lock()
        .expect("cid pool lock")
        .pop()
        .ok_or_else(|| io_err("lane CID pool exhausted (permit/pool desync)".into()))?;
    Ok(CidSlot {
        cid,
        pool: Arc::clone(pool),
        _permit: permit,
    })
}

struct Pending {
    dest: SendMutPtr,
    len: usize,
    received: u64,
    /// CapsuleResp or SUCCESS-elision seen (completion condition).
    tx: Option<oneshot::Sender<Result<()>>>,
    /// Destination keep-alive (MEM-3): a handle on the allocation behind
    /// `dest`, held until the entry is destroyed — i.e. until no lane
    /// context can write the destination. `None` only on the raw
    /// [`LaneSession::read_into_ptr`] contract, where the CALLER owns
    /// the allocation's lifetime past cancellation.
    _keepalive: Option<bytes::Bytes>,
    /// CID + depth-permit custody (returns at entry destruction).
    _slot: CidSlot,
}

struct QueueShared {
    /// std mutex (MEM-3): every critical section is map ops + pointer
    /// pushes with no await inside (the FillTable precedent), and the
    /// poison drain must be reliable, not `try_lock`-lossy. Lock order
    /// where both are held: `pending` → `free_cids` (entry drops return
    /// CIDs while the map lock is held).
    pending: std::sync::Mutex<std::collections::HashMap<u16, Pending>>,
    /// Free CID pool (push/pop only; shared with [`CidSlot`] custody).
    free_cids: Arc<std::sync::Mutex<Vec<u16>>>,
    cid_gate: Arc<tokio::sync::Semaphore>,
    /// CID namespace size (== queue depth) — the `cid_slots` diagnostic's
    /// denominator (the MEM-3 no-leak instrument).
    cid_capacity: usize,
    poisoned: AtomicBool,
}

impl QueueShared {
    /// Queue poison. `drain_pending` MUST be true only when the reader
    /// task provably writes no destination afterwards (it is exiting, it
    /// was abort+JOINED, or it never started): draining drops the
    /// entries' keep-alives, which is what lets destination buffers
    /// recycle. A writer-side failure passes `false` — the reader is
    /// still live on the (soon-dead) socket and its own exit performs
    /// the drain; waiters it would have failed fall back via their own
    /// 30 s timeouts instead (bounded, loud, never a recycled write).
    fn poison(&self, why: &str, session_poison: &AtomicBool, drain_pending: bool) {
        if !self.poisoned.swap(true, Ordering::SeqCst) {
            log::error!("zcrx-lane: IO queue poisoned: {why} — lane disarms, kernel path serves");
            mark_session_poisoned(session_poison);
        }
        if !drain_pending {
            return;
        }
        // Fail every waiter loud; their ops retry on the kernel path
        // (reads are idempotent — design §6). Entry drops return CID +
        // permit custody and release the keep-alives.
        let mut map = self.pending.lock().expect("pending lock");
        for (_, mut p) in map.drain() {
            if let Some(tx) = p.tx.take() {
                let _ = tx.send(Err(io_err(format!("lane queue poisoned: {why}"))));
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

/// A lane IO queue under one of the receive backends.
enum QueueHandle {
    Classic(IoQueue),
    Area(super::area_queue::AreaQueue),
}

/// One armed lane association to a target subsystem.
pub struct LaneSession {
    target: LaneTarget,
    queues: Vec<QueueHandle>,
    next_q: AtomicUsize,
    poisoned: Arc<AtomicBool>,
    /// The recorded NIC steering to restore at disarm/unmount (real
    /// backend only; design §5 record-and-restore law).
    steering: std::sync::Mutex<Option<SteeringHold>>,
    /// Keeps the admin connection (and thus the association) alive.
    _admin_hold: tokio::task::JoinHandle<()>,
}

impl LaneSession {
    /// Restore recorded NIC state (idempotent; disarm/unmount/quiesce).
    fn restore_steering(&self) {
        if let Ok(mut hold) = self.steering.lock() {
            if let Some(mut h) = hold.take() {
                h.restore();
            }
        }
    }
}

impl Drop for LaneSession {
    fn drop(&mut self) {
        self._admin_hold.abort();
        // Ring drivers exit on their doorbell close (quiesce path) or
        // observe the poisoned flag; the steering restore must not wait
        // for either — ioctls are sync and fast.
        self.restore_steering();
        for q in &self.queues {
            if let QueueHandle::Area(q) = q {
                if let super::area_queue::CommandSink::Ring(cmds) = &q.sink {
                    cmds.close();
                }
            }
        }
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
            pdu::parse_cqe(&rest).map_err(frame_err)
        }
        other => Err(io_err(format!(
            "unexpected admin PDU type {other:#x} during bring-up"
        ))),
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

/// The AREA-SIM chunk geometry: page-sized by default (the kernel zcrx
/// net_iov granule); the `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK` TEST lever
/// shrinks it (rounded to pow2, clamp 64 B..PMD) so contract suites can
/// force header splits across chunk seams.
fn sim_chunk_bytes() -> usize {
    let default = super::area::chunk_bytes_default();
    match std::env::var("SQUEEZEFS_ZCRX_LANE_SIM_CHUNK") {
        Ok(v) => v
            .trim()
            .parse::<usize>()
            .ok()
            .map(|n| {
                n.next_power_of_two()
                    .clamp(64, super::area::PMD_BYTES as usize)
            })
            .unwrap_or(default),
        Err(_) => default,
    }
}

/// Which receive backend a lane queue runs (design §5/§6/§10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneBackend {
    /// Classic in-process recv into the destination (copy-parity with the
    /// kernel path — the PR Z1 contract venue; `FORCE_COPY` seam).
    Classic,
    /// The PR Z2 area backend with socket recv simulating NIC DMA into
    /// area chunks (`AREA_SIM` seam — the chunk-parser/refill/gather
    /// contract venue; never a product posture).
    AreaSim,
    /// The REAL zcrx receive backend (design §5): registered ifqs on
    /// dedicated NIC RX queues, RECV_ZC delivery, steering armed after
    /// connect and restored at disarm. Field-owed execution — the arm
    /// ladder reaches this only behind the full probe chain.
    Zcrx(ZcrxPlan),
}

/// The NIC facts the real arm resolved before bring-up (design §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZcrxPlan {
    pub ifname: String,
    pub ifindex: u32,
    pub numa_node: Option<usize>,
    /// One dedicated RX queue per lane IO queue (highest-indexed picks).
    pub rx_queues: Vec<u32>,
}

/// The armed NIC state a session must restore at disarm/unmount.
struct SteeringHold {
    nic: super::ethtool::EthtoolNic,
    guard: super::steering::SteeringGuard,
}

impl SteeringHold {
    fn restore(&mut self) {
        if let Err(e) = self.guard.restore(&mut self.nic) {
            log::error!("zcrx-lane: {e}");
        }
    }
}

impl LaneSession {
    /// [`Self::connect`] with an explicit receive backend.
    pub async fn connect_with(
        target: LaneTarget,
        backend: LaneBackend,
    ) -> Result<Arc<LaneSession>> {
        Self::connect_inner(target, backend).await
    }

    /// Free-CID diagnostics `(free, total)` summed over the session's
    /// queues — the MEM-3 no-leak instrument: at quiescence (no op in
    /// flight, cancelled ops completed by the target) `free == total`
    /// on a healthy queue; anything less is a leaked CID.
    pub fn cid_slots(&self) -> (usize, usize) {
        let mut free = 0;
        let mut total = 0;
        for q in &self.queues {
            match q {
                QueueHandle::Classic(q) => {
                    free += q.shared.free_cids.lock().expect("cid pool lock").len();
                    total += q.shared.cid_capacity;
                }
                QueueHandle::Area(q) => {
                    free += q.shared.free_cids.lock().expect("cid pool lock").len();
                    total += q.shared.cid_capacity;
                }
            }
        }
        (free, total)
    }

    /// Area-chunk diagnostics `(free, total)` summed over the session's
    /// area queues — the refill-discipline instrument; (0, 0) on classic
    /// backends (no area exists).
    pub fn area_chunks(&self) -> (usize, usize) {
        let mut free = 0;
        let mut total = 0;
        for q in &self.queues {
            if let QueueHandle::Area(q) = q {
                free += q.area.free_chunks();
                total += q.area.chunk_count();
            }
        }
        (free, total)
    }

    /// Abort AND join every queue driver — the poison-drain quiescence
    /// law (design §7): after this returns no lane task or thread holds
    /// destination pointers or area-chunk refs, and any recorded NIC
    /// steering has been restored.
    pub async fn quiesce(&self) {
        for q in &self.queues {
            match q {
                QueueHandle::Classic(q) => {
                    let mut ts = q.tasks.lock().await;
                    for t in ts.iter() {
                        t.abort();
                    }
                    for t in ts.drain(..) {
                        let _ = t.await;
                    }
                }
                QueueHandle::Area(q) => q.drain().await,
            }
        }
        self.restore_steering();
    }

    /// Bring up the association (design §4.2): ICReq/ICResp (digests-off
    /// law) → admin Connect (cntlid 0xFFFF, KATO 0) → CAP → CC.EN →
    /// CSTS.RDY → per-queue IO Connect. Every failure is loud and leaves
    /// the caller on the kernel path.
    pub async fn connect(target: LaneTarget) -> Result<Arc<LaneSession>> {
        Self::connect_inner(target, LaneBackend::Classic).await
    }

    async fn connect_inner(target: LaneTarget, backend: LaneBackend) -> Result<Arc<LaneSession>> {
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
        // Real-backend bring-up state (empty on the other backends).
        let mut go_txs: Vec<std::sync::mpsc::Sender<bool>> = Vec::new();
        let mut flows: Vec<super::steering::FlowRule> = Vec::new();
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
            match &backend {
                LaneBackend::Classic => {
                    queues.push(QueueHandle::Classic(spawn_queue(
                        s,
                        depth,
                        Arc::clone(&poisoned),
                    )));
                }
                LaneBackend::AreaSim => {
                    // Registration-order law (design §5): area exists and
                    // is bound BEFORE the queue serves (sim has no ifq/
                    // steering steps; NUMA is the real backend's — the
                    // sim recv is CPU-copy anyway).
                    let area = super::area::ZcrxArea::new(
                        super::area::area_bytes_per_queue(depth, target.max_xfer_bytes),
                        sim_chunk_bytes(),
                        None,
                    )?;
                    queues.push(QueueHandle::Area(super::area_queue::spawn_area_queue(
                        s,
                        depth,
                        area,
                        Arc::clone(&poisoned),
                    )));
                }
                LaneBackend::Zcrx(plan) => {
                    let rxq = *plan.rx_queues.get(qid as usize - 1).ok_or_else(|| {
                        io_err(format!(
                            "zcrx plan has {} rx queues for IO queue {qid}",
                            plan.rx_queues.len()
                        ))
                    })?;
                    let local = s
                        .local_addr()
                        .map_err(|e| io_err(format!("lane queue local addr: {e}")))?;
                    let peer = s
                        .peer_addr()
                        .map_err(|e| io_err(format!("lane queue peer addr: {e}")))?;
                    // Registration order (design §5): area (NUMA-bound) →
                    // ring + ifq registration ON the driver thread
                    // (SINGLE_ISSUER law) → steering after ALL connects →
                    // RECV_ZC arms on the go signal.
                    let area = super::area::ZcrxArea::new(
                        super::area::area_bytes_per_queue(depth, target.max_xfer_bytes),
                        super::area::chunk_bytes_default(),
                        plan.numa_node,
                    )?;
                    let shared = super::area_queue::AreaShared::new(depth, &area);
                    let cmds = super::uring_zcrx::RingCmd::new().map_err(io_err)?;
                    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                    let (go_tx, go_rx) = std::sync::mpsc::channel();
                    let driver = super::uring_zcrx::spawn_ring_driver(
                        super::uring_zcrx::RingDriverConfig {
                            ifindex: plan.ifindex,
                            rxq,
                            numa_node: plan.numa_node,
                            rq_entries: super::area::rq_entries_for(area.chunk_count() as u64),
                            sq_entries: (depth as u32 + 8).next_power_of_two(),
                            area: Arc::clone(&area),
                            shared: Arc::clone(&shared),
                            cmds: Arc::clone(&cmds),
                            session_poison: Arc::clone(&poisoned),
                            sock: s
                                .into_std()
                                .map_err(|e| io_err(format!("lane queue into_std: {e}")))?,
                        },
                        ready_tx,
                        go_rx,
                    );
                    // A refusal here (or any later `?`) unwinds cleanly:
                    // dropping go_tx makes every parked driver exit
                    // silently — nothing armed, NIC untouched (steering
                    // is the LAST step).
                    tokio::task::spawn_blocking(move || ready_rx.recv())
                        .await
                        .map_err(|e| io_err(format!("lane driver ready join: {e}")))?
                        .map_err(|_| io_err("lane driver died during setup".into()))?
                        .map_err(io_err)?;
                    go_txs.push(go_tx);
                    flows.push(super::steering::FlowRule {
                        src: local,
                        dst: peer,
                        queue: rxq,
                    });
                    queues.push(QueueHandle::Area(super::area_queue::AreaQueue {
                        shared,
                        sink: super::area_queue::CommandSink::Ring(cmds),
                        tasks: tokio::sync::Mutex::new(Vec::new()),
                        driver: std::sync::Mutex::new(Some(driver)),
                        area,
                    }));
                }
            }
        }

        // Real backend: steer the lane flows to their ZC queues (design
        // §5 — the LAST mutating step, so every earlier refusal leaves
        // the NIC byte-identical), then release the parked drivers to
        // arm RECV_ZC.
        let steering_hold = if let LaneBackend::Zcrx(plan) = &backend {
            let ifname = plan.ifname.clone();
            let flows_owned = std::mem::take(&mut flows);
            let hold = tokio::task::spawn_blocking(move || -> Result<SteeringHold> {
                let mut nic = super::ethtool::EthtoolNic::open(&ifname).map_err(io_err)?;
                let guard = super::steering::arm_steering(&mut nic, &flows_owned)
                    .map_err(|e| io_err(format!("zcrx steering arm: {e}")))?;
                Ok(SteeringHold { nic, guard })
            })
            .await
            .map_err(|e| io_err(format!("steering join: {e}")))??;
            for tx in &go_txs {
                let _ = tx.send(true);
            }
            Some(hold)
        } else {
            None
        };

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
            mark_session_poisoned(&admin_poison);
        });

        log::info!(
            "zcrx-lane armed: {} nsid={} lba_shift={} queues={} depth={} max_xfer={} (backend: {})",
            target.subnqn,
            target.nsid,
            target.lba_shift,
            target.io_queues,
            depth,
            target.max_xfer_bytes,
            match &backend {
                LaneBackend::Classic => "classic-recv contract venue — PR Z1",
                LaneBackend::AreaSim => "area-sim contract venue — PR Z2",
                LaneBackend::Zcrx(_) => "zcrx RECV_ZC — PR Z2",
            },
        );
        Ok(Arc::new(LaneSession {
            target,
            queues,
            next_q: AtomicUsize::new(0),
            poisoned,
            steering: std::sync::Mutex::new(steering_hold),
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
    /// SAFETY (raw contract): `dest..dest+len` must be writable and the
    /// caller must keep the backing allocation alive until the op
    /// COMPLETES — which under cancellation (this future dropped mid-op)
    /// extends past the drop until the session's driver finishes or the
    /// session quiesces. Product callers use [`Self::read_into_pooled`]
    /// (entry-held keep-alive — custody survives cancellation) instead;
    /// area-backend destinations are only ever written by THIS future
    /// (requester-side gather), so the raw contract is trivially met
    /// there. The pointer is wrapped before the async body so the
    /// returned future stays `Send` (funnel callers run on the
    /// multi-thread runtime).
    pub fn read_into_ptr(
        &self,
        byte_offset: u64,
        dest: *mut u8,
        len: usize,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let dest = SendMutPtr(dest);
        self.read_into_wrapped(byte_offset, dest, len, None)
    }

    /// Pool-backed destination read (the funnel's arm): `keepalive` is a
    /// handle on the allocation behind `dest` (a clone of the pooled
    /// `Bytes`); every classic-lane pending entry holds a clone until the
    /// driver is finished with its span — the MEM-3 cancellation-custody
    /// law (a dropped caller future must never let the pool recycle a
    /// buffer a reader task can still write).
    pub fn read_into_pooled(
        &self,
        byte_offset: u64,
        dest: *mut u8,
        len: usize,
        keepalive: bytes::Bytes,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let dest = SendMutPtr(dest);
        self.read_into_wrapped(byte_offset, dest, len, Some(keepalive))
    }

    /// Whether this session may serve REGISTERED-destination reads (the
    /// PR Z3 fused gather — design §4.4/§10): area-class backends only.
    /// The classic backend's reader task writes destinations from a
    /// FOREIGN task, which for registered uring ent / arena memory is the
    /// MEM-1 hazard class under cancellation; area backends gather on the
    /// REQUESTER — a dropped future gathers nothing, so a registered dest
    /// is never written after its op resolves, by construction.
    pub fn dest_serve_eligible(&self) -> bool {
        !self.queues.is_empty()
            && self
                .queues
                .iter()
                .all(|q| matches!(q, QueueHandle::Area(_)))
    }

    /// Registered-destination read — the PR Z3 GATHER FUSION arm: each
    /// sub-command's ONE completion gather (design §4.4) lands DIRECTLY
    /// in `dest`, deleting the Z2 intermediate (gather → pooled bounce →
    /// serve copy) on dest-carrying funnel shapes. Counted in
    /// `zcrx_dest_gather_bytes` (subset of `zcrx_gather_bytes` — the
    /// fused-serve engagement gauge); refuses on non-area backends
    /// ([`Self::dest_serve_eligible`]).
    ///
    /// SAFETY: `dest..dest+len` is pre-registered pinned memory writable
    /// for the duration of the call (the `read_block_with_dest` dest
    /// contract). Only THIS future writes it (requester-side gather), so
    /// cancellation ends all lane writes to it immediately.
    pub fn read_into_dest(
        &self,
        byte_offset: u64,
        dest: *mut u8,
        len: usize,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let dest = SendMutPtr(dest);
        async move {
            if !self.dest_serve_eligible() {
                return Err(io_err(
                    "lane dest-serve requires an area backend (classic reader \
                     writes from a foreign task — refused by law)"
                        .into(),
                ));
            }
            self.read_into_wrapped(byte_offset, dest, len, None).await?;
            crate::fuse_client::METRICS
                .zcrx_dest_gather_bytes
                .fetch_add(len as u64, Ordering::Relaxed);
            Ok(())
        }
    }

    async fn read_into_wrapped(
        &self,
        byte_offset: u64,
        dest: SendMutPtr,
        len: usize,
        keepalive: Option<bytes::Bytes>,
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
            futs.push(self.read_segment(seg_off, seg_dest, seg, keepalive.clone()));
            done += seg;
        }
        for r in futures::future::join_all(futs).await {
            r?;
        }
        Ok(())
    }

    /// Slice-destination convenience (tests + non-pooled callers):
    /// cancellation-safe by construction — the wire read lands in an
    /// op-owned allocation under the pooled custody law and the slice is
    /// filled only on success (one extra copy, priced acceptable for a
    /// convenience API; the funnel never rides this).
    pub async fn read_into_slice(&self, byte_offset: u64, dest: &mut [u8]) -> Result<()> {
        let len = dest.len();
        let mut owned = vec![0u8; len];
        let ptr = owned.as_mut_ptr();
        // Vec buffer address is stable across the move into the owner.
        let keep = bytes::Bytes::from_owner(owned);
        self.read_into_pooled(byte_offset, ptr, len, keep.clone())
            .await?;
        dest.copy_from_slice(&keep[..len]);
        Ok(())
    }

    async fn read_segment(
        &self,
        byte_offset: u64,
        dest: SendMutPtr,
        len: usize,
        keepalive: Option<bytes::Bytes>,
    ) -> Result<()> {
        let q = &self.queues[self.next_q.fetch_add(1, Ordering::Relaxed) % self.queues.len()];
        match q {
            QueueHandle::Classic(q) => {
                self.read_segment_classic(q, byte_offset, dest, len, keepalive)
                    .await
            }
            // Area backends never hand `dest` to a driver task — the
            // requester gathers at completion — so the entry needs no
            // destination keep-alive.
            QueueHandle::Area(q) => self.read_segment_area(q, byte_offset, dest, len).await,
        }
    }

    /// The area-backend segment read (design §4.3/§4.4): admission-bound
    /// the in-flight payload, issue the capsule, await the scatter fill,
    /// then run the ONE priced completion gather into `dest`. The queue
    /// driver never touches `dest` — the fill's chunk refs drop right
    /// here, which is what feeds the refill path.
    async fn read_segment_area(
        &self,
        q: &super::area_queue::AreaQueue,
        byte_offset: u64,
        dest: SendMutPtr,
        len: usize,
    ) -> Result<()> {
        use super::area_queue::admission_units;
        let shared = &q.shared;
        if shared.poisoned.load(Ordering::SeqCst) {
            return Err(io_err("lane queue poisoned".into()));
        }
        // Admission backpressure (design §4.3): bounded in-flight fills,
        // never a mid-stream stall. Parking is counted honest. The
        // OWNED permit rides the entry → the fill (MEM-3): admission
        // accounting stays exact under cancellation — released when the
        // fill drops (after the requester's gather, or inside the dead
        // completion channel on a dropped future), never early.
        let units = admission_units(len);
        let admission = match Arc::clone(&shared.admission).try_acquire_many_owned(units) {
            Ok(p) => p,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                crate::fuse_client::METRICS
                    .zcrx_area_admission_waits
                    .fetch_add(1, Ordering::Relaxed);
                Arc::clone(&shared.admission)
                    .acquire_many_owned(units)
                    .await
                    .map_err(|_| io_err("lane queue closed".into()))?
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(io_err("lane queue closed".into()));
            }
        };
        // CID + depth-permit custody lives in the pending entry (MEM-3):
        // it returns when the driver destroys the entry — completion,
        // send-failure cancel, or poison drain — never on this
        // requester's exits, so a dropped future leaks nothing.
        let slot = take_cid(&shared.cid_gate, &shared.free_cids).await?;
        let cid = slot.cid();
        let rx = shared.table.insert(cid, len, slot, Some(admission));

        let slba = byte_offset >> self.target.lba_shift;
        let nlb = (len >> self.target.lba_shift) as u32;
        let capsule = pdu::encode_read_capsule(cid, self.target.nsid, slba, nlb, len as u32);
        if q.sink.send(capsule).is_err() {
            // Capsule never reached the wire: destroy the entry (custody
            // returns with it).
            shared.table.cancel(cid);
            return Err(io_err("lane command sink gone".into()));
        }

        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(fill))) => {
                // The gather law (design §4.4): ONE pass from refcounted
                // area chunks into the destination; dropping the fill
                // releases the refs → chunks recycle to the refill path.
                // SAFETY: `dest..dest+len` is the caller's exclusive
                // destination window (read-path serve buffer); spans were
                // bounds-validated at record time (`on_c2h_span`).
                unsafe { fill.gather_into(dest.0) };
                crate::fuse_client::METRICS
                    .zcrx_gather_bytes
                    .fetch_add(len as u64, Ordering::Relaxed);
                Ok(())
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(io_err("lane completion channel dropped".into())),
            Err(_) => {
                // Quiescence law: drain (abort-and-join tasks / close +
                // join the ring driver) so no lane context survives
                // holding chunk refs, then poison loud (drain=true: the
                // drivers are provably joined).
                shared.poisoned.store(true, Ordering::SeqCst);
                mark_session_poisoned(&self.poisoned);
                q.drain().await;
                shared.poison("read timed out after 30 s", &self.poisoned, true);
                Err(io_err(format!(
                    "lane read timed out (offset={byte_offset}, len={len})"
                )))
            }
        }
    }

    async fn read_segment_classic(
        &self,
        q: &IoQueue,
        byte_offset: u64,
        dest: SendMutPtr,
        len: usize,
        keepalive: Option<bytes::Bytes>,
    ) -> Result<()> {
        if q.shared.poisoned.load(Ordering::SeqCst) {
            return Err(io_err("lane queue poisoned".into()));
        }
        // CID + depth-permit custody lives in the pending entry (MEM-3),
        // alongside the destination keep-alive: the reader task writes
        // only destinations whose entries exist, and the entry outlives
        // any cancelled requester — no recycled-buffer write, no leak.
        let slot = take_cid(&q.shared.cid_gate, &q.shared.free_cids).await?;
        let cid = slot.cid();

        let (tx, rx) = oneshot::channel();
        q.shared.pending.lock().expect("pending lock").insert(
            cid,
            Pending {
                dest,
                len,
                received: 0,
                tx: Some(tx),
                _keepalive: keepalive,
                _slot: slot,
            },
        );

        let slba = byte_offset >> self.target.lba_shift;
        let nlb = (len >> self.target.lba_shift) as u32;
        let capsule = pdu::encode_read_capsule(cid, self.target.nsid, slba, nlb, len as u32);
        if q.to_writer.send(capsule).is_err() {
            // Capsule never reached the wire: destroy the entry (custody
            // returns with it).
            q.shared.pending.lock().expect("pending lock").remove(&cid);
            return Err(io_err("lane writer task gone".into()));
        }

        match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(io_err("lane completion channel dropped".into())),
            Err(_) => {
                // Quiescence law: the reader task holds raw pointers into
                // this (and other) destination buffers. Before this frame
                // returns — and the caller's buffer can be dropped/recycled
                // — abort AND join the queue's tasks so no writer survives;
                // only then drain (dropping entry keep-alives).
                q.shared.poisoned.store(true, Ordering::SeqCst);
                mark_session_poisoned(&self.poisoned);
                let mut ts = q.tasks.lock().await;
                for t in ts.iter() {
                    t.abort();
                }
                for t in ts.drain(..) {
                    let _ = t.await;
                }
                drop(ts);
                q.shared
                    .poison("read timed out after 30 s", &self.poisoned, true);
                Err(io_err(format!(
                    "lane read timed out (offset={byte_offset}, len={len})"
                )))
            }
        }
    }
}

fn spawn_queue(stream: TcpStream, depth: u16, session_poison: Arc<AtomicBool>) -> IoQueue {
    let shared = Arc::new(QueueShared {
        pending: std::sync::Mutex::new(std::collections::HashMap::new()),
        free_cids: Arc::new(std::sync::Mutex::new((0..depth).collect())),
        cid_gate: Arc::new(tokio::sync::Semaphore::new(depth as usize)),
        cid_capacity: depth as usize,
        poisoned: AtomicBool::new(false),
    });
    let (read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::unbounded_channel();

    let s2 = Arc::clone(&shared);
    let p2 = Arc::clone(&session_poison);
    let writer = tokio::spawn(async move {
        if let Err(why) = writer_loop(write_half, rx).await {
            // drain=false: the reader may still be mid-write on a live
            // entry — its own exit (or the timeout arm's abort+join)
            // performs the drain (MEM-3 drain discipline).
            s2.poison(&why, &p2, false);
        }
    });
    let s3 = Arc::clone(&shared);
    let reader = tokio::spawn(async move {
        if let Err(why) = reader_loop(read_half, &s3).await {
            // drain=true: the reader is exiting — no further destination
            // writes are possible from this queue.
            s3.poison(&why, &session_poison, true);
        }
    });

    IoQueue {
        shared,
        to_writer: tx,
        tasks: Mutex::new(vec![reader, writer]),
    }
}

pub(crate) async fn writer_loop(
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
            if shared.pending.lock().expect("pending lock").is_empty()
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

                // Guard scope-bounded: a std MutexGuard must provably end
                // before the socket await (the future stays Send).
                let span = {
                    let mut map = shared.pending.lock().expect("pending lock");
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
                    // destination is kept alive by the pending entry (or the
                    // raw-contract caller) and only this reader writes it.
                    unsafe {
                        std::slice::from_raw_parts_mut(
                            p.dest.0.add(c2h.datao as usize),
                            c2h.datal as usize,
                        )
                    }
                    // map lock ends here — never held across socket I/O
                };
                r.read_exact(span)
                    .await
                    .map_err(|e| format!("C2HData payload read: {e}"))?;

                {
                    let mut map = shared.pending.lock().expect("pending lock");
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
                let mut map = shared.pending.lock().expect("pending lock");
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
