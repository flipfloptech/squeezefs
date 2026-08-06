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

/// The ONE lane read timeout (both read paths + the park-fail bound
/// derive from it — never a fresh 30 s literal). The timeout arm stays
/// the LAST-RESORT poison for genuinely wedged queues; refill
/// starvation fails over at `uring_zcrx::park_fail_bound()` long before
/// this can fire.
pub(crate) const LANE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn io_err(msg: String) -> SqueezefsError {
    SqueezefsError::Io(std::io::Error::other(msg))
}

/// The one session-poison transition (design §7 + the Z2 poison lattice):
/// idempotent; the winning transition counts the `zcrx_lane_poisoned`
/// tripwire and drops the `zcrx_lane_armed` gauge — the funnel routes
/// every subsequent op to the kernel path for the mount lifetime.
pub(crate) fn mark_session_poisoned(flag: &AtomicBool, why: &str) {
    if !flag.swap(true, Ordering::SeqCst) {
        // The gauge/log structural tie (round 8, the third silent-poison
        // burn): the gauge moves ONLY here, and here ALWAYS logs the
        // canonical line — a poisoned increment without a greppable
        // reason is unrepresentable.
        log::error!("zcrx-lane: session poisoned: {why} — lane disarms, kernel path serves");
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
            log::error!("zcrx-lane: IO queue poisoned: {why}");
            mark_session_poisoned(session_poison, why);
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
    /// `Mutex<Option<…>>` so teardown can TAKE it and AWAIT its
    /// completion (stop-ship flake, 2026-08: `abort()` only requests
    /// cancellation — without the await, teardown returned while the
    /// watchdog's cancellation was still in flight and `is_finished()`
    /// raced the runtime).
    admin_hold: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The rxq-arbiter grant backing this session's ifqs (real backend
    /// only). Declared LAST: it drops after the queues close their
    /// doorbells, so the indices return to the per-NIC pool only once
    /// the session's teardown is underway. Mutex'd so [`Self::teardown`]
    /// can release it WITHOUT dropping the session (finding F: sessions
    /// live in OnceCell statics for the mount lifetime).
    rxq_lease: std::sync::Mutex<Option<Arc<super::rxq_alloc::RxqLease>>>,
    /// The finding-F teardown latch (idempotence + the registry's
    /// live-count instrument). Arc'd: the admin watchdog holds a clone
    /// (round 6 — an orderly post-teardown association close must be
    /// unreadable as poison).
    teardown_latch: Arc<AtomicBool>,
    /// The NIC this session's queues ride (real backend: the plan's
    /// ifindex; 0 = none — contract backends). Round 3: the per-NIC
    /// structural census key (the whole-NIC no-harm face).
    nic_ifindex: std::sync::atomic::AtomicU32,
}

impl LaneSession {
    /// Restore recorded NIC state (idempotent; disarm/unmount/quiesce).
    fn restore_steering(&self) {
        if let Ok(mut hold) = self.steering.lock() {
            if let Some(mut h) = hold.take() {
                h.restore_now();
            }
        }
    }
}

impl Drop for LaneSession {
    fn drop(&mut self) {
        if let Some(h) = self.admin_hold.lock().expect("admin hold lock").take() {
            h.abort();
        }
        // FINDING 3 teardown order (2026-08 field): the ifqs must be
        // gone BEFORE the steering restore re-includes the lane queues
        // in RSS — a still-bound ifq makes host flows on its queue
        // unreadable (recv = EFAULT). Close the ring doorbells, JOIN
        // the driver threads (prompt: the doorbell CQE wakes them and
        // the closed latch exits the loop), THEN restore. `quiesce()`
        // is the async path with the same order.
        for q in &self.queues {
            if let QueueHandle::Area(q) = q {
                if let super::area_queue::CommandSink::Ring(cmds) = &q.sink {
                    cmds.close();
                }
            }
        }
        for q in &self.queues {
            if let QueueHandle::Area(q) = q {
                let handle = q.driver.lock().expect("driver handle lock").take();
                if let Some(h) = handle {
                    let _ = h.join();
                }
            }
        }
        self.restore_steering();
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
            "{op} failed: {}",
            pdu::describe_status(cqe.status)
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
    /// One dedicated RX queue per lane IO queue — DISTINCT per session
    /// on a shared NIC (the rxq arbiter's grant; qid N rides
    /// `rx_queues[N - 1]`).
    pub rx_queues: Vec<u32>,
    /// The NIC RX ring's standing provider-pool demand in bytes
    /// (round 6: `ring_standing_bytes` over the arm-time ring/MTU
    /// probe) — added to every queue's area so the pool survives the
    /// driver's ring fill. 0 on the sim backend (no NIC ring exists).
    pub ring_fill_bytes: u64,
    /// The NIC's MTU (arm-time probe; `None` = probe failed). The
    /// engagement campaign's burst-occupancy input: the fills'
    /// delivery-slack allotment (`area::delivery_slack_bytes`) and the
    /// CQ's chunk-touch demand (`uring_zcrx::cq_entries_for`) both
    /// derive from it; unknown degrades to the occupancy-½ posture.
    pub mtu: Option<u32>,
    /// The arbiter lease backing `rx_queues` (RAII: the queues return to
    /// the NIC's pool when the session — and thus its ifqs — goes away).
    /// `None` only in direct unit constructions.
    pub rxq_lease: Option<Arc<super::rxq_alloc::RxqLease>>,
}

impl ZcrxPlan {
    /// The registration rxq for IO queue `qid` (1-based). This is the
    /// ONLY source of rxq indices at `REGISTER_ZCRX_IFQ` time — the
    /// arbiter's granted list riding the plan (field finding 2's
    /// plumbing pin: registration never derives a queue itself).
    pub fn rxq_for_qid(&self, qid: u16) -> Option<u32> {
        self.rx_queues.get(qid as usize - 1).copied()
    }
}

/// The armed NIC state a session must restore at disarm/unmount —
/// [`super::steering::ArmedSteering`] over the live NIC control: ONE
/// owner from birth, Drop-converging (field finding B — the arm future
/// is cancellable, so no path may strand the guard).
type SteeringHold = super::steering::ArmedSteering<super::ethtool::EthtoolNic>;

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

    /// Available admission units summed over the session's area queues —
    /// the round-8 decline-law instrument (0 = the window is fully
    /// admitted and the next read DECLINES to the kernel path).
    pub fn admission_units_available(&self) -> usize {
        self.queues
            .iter()
            .map(|q| match q {
                QueueHandle::Area(q) => q.shared.admission.available_permits(),
                QueueHandle::Classic(_) => 0,
            })
            .sum()
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

        // FINDING 3 + round-4 FINDING G (the exclusion barrier): RSS must
        // exclude the leased queues BEFORE any ifq binds one (a
        // zcrx-bound queue produces unreadable net_iov skbs — any HOST
        // flow RSS-hashed onto it gets recv = EFAULT), and — the round-4
        // refinement — before ANY of this session's TCP connects exist,
        // ADMIN included: the round-3 placement (post-admin, pre-qid)
        // left every admin/CSTS flow established while the session's own
        // queues were still RSS-included, and a hardware flow cache that
        // latches an established flow's queue keeps delivering there
        // after the indirection table changes — the surviving
        // ICResp-EFAULT window on staggered arms. Cross-session
        // visibility is the live-arm union law (peers' queues are
        // excluded by every arm's phase A, mid-bring-up included).
        // Failures between here and the bring-up scope converge via
        // ArmedSteering::Drop with ZERO ifqs bound — clean by
        // construction. Phase B (flow rules) stays post-connect
        // (ephemeral ports).
        let mut steering_hold: Option<SteeringHold> = None;
        if let LaneBackend::Zcrx(plan) = &backend {
            let ifname = plan.ifname.clone();
            let lane_queues = plan.rx_queues.clone();
            let hold = tokio::task::spawn_blocking(move || -> Result<SteeringHold> {
                let nic = super::ethtool::EthtoolNic::open(&ifname).map_err(io_err)?;
                super::steering::ArmedSteering::arm(nic, &lane_queues)
                    .map_err(|e| io_err(format!("zcrx steering arm (RSS exclusion): {e}")))
            })
            .await
            .map_err(|e| io_err(format!("steering join: {e}")))??;
            steering_hold = Some(hold);
        }

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

        // The whole post-phase-A bring-up unwinds through ONE failure
        // path (below): stop drivers → JOIN them → only then restore
        // steering — never RSS-restore over a still-bound ifq.
        let bring_up: Result<()> = async {
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
                        let rxq = plan.rxq_for_qid(qid).ok_or_else(|| {
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
                        // Sizing laws (rounds 6–7 + the 2026-08-06
                        // engagement campaign): the registered area =
                        // fill window (depth × max_xfer — admitted in
                        // FULL, the /2 retired) + the DERIVED
                        // delivery-slack allotment (burst occupancy
                        // from the arm-time MTU probe — the budget the
                        // /2 held implicitly) + the NIC RX ring's
                        // STANDING pool demand (the plan carries it).
                        // Admission clamps to the fill window — further
                        // shrunk (round 7, the either/or law) to what a
                        // kernel-max CQ can actually complete at the
                        // burst-occupancy chunk grain.
                        let chunk = super::area::chunk_bytes_default();
                        let fill_window =
                            super::area::area_bytes_per_queue(depth, target.max_xfer_bytes);
                        let slack = super::area::delivery_slack_bytes(fill_window, plan.mtu, chunk);
                        let area = super::area::ZcrxArea::new(
                            fill_window + slack + plan.ring_fill_bytes,
                            chunk,
                            plan.numa_node,
                        )?;
                        let sq = (depth as u32 + 8).next_power_of_two();
                        let cq = super::uring_zcrx::cq_entries_for(
                            depth,
                            target.max_xfer_bytes,
                            chunk,
                            sq,
                            plan.mtu,
                        );
                        let admit_window = fill_window.min(
                            super::uring_zcrx::cq_admitted_window_bytes(cq, sq, chunk, plan.mtu),
                        );
                        let shared =
                            super::area_queue::AreaShared::new(depth, &area, admit_window as usize);
                        let cmds = super::uring_zcrx::RingCmd::new().map_err(io_err)?;
                        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                        let (go_tx, go_rx) = std::sync::mpsc::channel();
                        let driver = super::uring_zcrx::spawn_ring_driver(
                            super::uring_zcrx::RingDriverConfig {
                                ifindex: plan.ifindex,
                                rxq,
                                numa_node: plan.numa_node,
                                rq_entries: super::area::rq_entries_for(area.chunk_count() as u64),
                                sq_entries: sq,
                                cq_entries: cq,
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

            // Phase B (real backend): the lane flows exist now (post-connect
            // ephemeral ports) — install the steering rules into the phase-A
            // guard, then release the parked drivers to arm RECV_ZC.
            if matches!(&backend, LaneBackend::Zcrx(_)) {
                let mut hold = steering_hold
                    .take()
                    .ok_or_else(|| io_err("zcrx phase-A steering hold missing".into()))?;
                let flows_owned = std::mem::take(&mut flows);
                // The hold comes BACK on both arms — a rules failure must
                // not drop the guard here (RSS has to stay excluded until
                // the unwind below has torn the ifqs down).
                let (hold_back, rules_res) = tokio::task::spawn_blocking(move || {
                    let res = hold
                        .flow_rules(&flows_owned)
                        .map_err(|e| io_err(format!("zcrx steering arm (flow rules): {e}")));
                    (hold, res)
                })
                .await
                .map_err(|e| io_err(format!("steering join: {e}")))?;
                steering_hold = Some(hold_back);
                rules_res?;
                for tx in &go_txs {
                    let _ = tx.send(true);
                }
            }
            Ok(())
        }
        .await;

        if let Err(e) = bring_up {
            // FINDING 3 unwind order: the ifqs must be GONE before the
            // phase-A RSS exclusion lifts. Parked drivers exit when
            // their go senders drop; armed ones exit on the doorbell
            // close inside drain(); JOIN them all, THEN restore.
            drop(go_txs);
            for q in &queues {
                if let QueueHandle::Area(q) = q {
                    q.drain().await;
                }
            }
            if let Some(mut hold) = steering_hold.take() {
                let _ = tokio::task::spawn_blocking(move || hold.restore_now()).await;
            }
            return Err(e);
        }

        // Park the admin socket: any read (data or EOF) after bring-up is an
        // association event — poison loud, the kernel path keeps serving.
        // EXCEPT during teardown (round 6): the latch makes the target's
        // orderly association unwind silent, never a poison.
        let teardown_latch = Arc::new(AtomicBool::new(false));
        let admin_poison = Arc::clone(&poisoned);
        let admin_latch = Arc::clone(&teardown_latch);
        let admin_hold = tokio::spawn(async move {
            let mut b = [0u8; 8];
            let event = admin.read(&mut b).await;
            if admin_latch.load(Ordering::SeqCst) {
                return; // orderly teardown — not an association event
            }
            let why = match &event {
                Ok(0) => "admin connection closed by target (association death; remount re-arms)"
                    .to_string(),
                Ok(_) => "unexpected admin PDU after bring-up".to_string(),
                Err(e) => format!("admin connection error: {e}"),
            };
            // The mark-site latch edge (belt): a teardown that began
            // AFTER the early check above must still not gain a poison
            // mark from this task. With teardown awaiting this handle,
            // a mark that wins here reflects a genuinely concurrent
            // pre-teardown association event — which IS poison.
            if !admin_latch.load(Ordering::SeqCst) {
                mark_session_poisoned(&admin_poison, &why);
            }
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
        let rxq_lease = match &backend {
            LaneBackend::Zcrx(plan) => plan.rxq_lease.clone(),
            _ => None,
        };
        let nic_ifindex = match &backend {
            LaneBackend::Zcrx(plan) => plan.ifindex,
            _ => 0,
        };
        let session = Arc::new(LaneSession {
            target,
            queues,
            next_q: AtomicUsize::new(0),
            poisoned,
            steering: std::sync::Mutex::new(steering_hold),
            admin_hold: std::sync::Mutex::new(Some(admin_hold)),
            rxq_lease: std::sync::Mutex::new(rxq_lease),
            teardown_latch,
            nic_ifindex: std::sync::atomic::AtomicU32::new(nic_ifindex),
        });
        super::register_session(&session);
        Ok(session)
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    /// Whether the admin watchdog task has finished (round-6 gauge-
    /// honesty instrument: teardown must RETIRE the watchdog before the
    /// target's orderly association close can be misread as poison).
    pub fn admin_watchdog_finished(&self) -> bool {
        // Retired = taken-and-awaited by teardown (None), or the task
        // itself completed (a real association event ran its course).
        self.admin_hold
            .lock()
            .expect("admin hold lock")
            .as_ref()
            .is_none_or(|h| h.is_finished())
    }

    /// Round-5 blast-radius law: any queue in a refill-starvation
    /// failover window ⇒ the lane declines NEW reads (kernel path
    /// serves, uncounted — the ineligibility class) while the driver
    /// recovers in the background. Poison stays reserved for real
    /// transport errors.
    pub fn refill_degraded(&self) -> bool {
        self.queues.iter().any(|q| match q {
            QueueHandle::Area(q) => q.shared.starved.load(Ordering::SeqCst),
            QueueHandle::Classic(_) => false,
        })
    }

    /// Contract seam (engagement round 2 — the bypass-accounting suite):
    /// latch/unlatch the round-5 degraded flag on every area queue,
    /// standing in for a live starvation episode (only the real ring
    /// driver's governor produces one; the sim backend has no governor).
    pub fn set_refill_degraded_for_test(&self, on: bool) {
        for q in &self.queues {
            if let QueueHandle::Area(q) = q {
                q.shared.starved.store(on, Ordering::SeqCst);
            }
        }
    }

    /// Contract seam (round 3 — the whole-NIC release suite): latch the
    /// structural flag on every area queue, standing in for a
    /// governor-proven structural starvation.
    pub fn set_refill_structural_for_test(&self) {
        for q in &self.queues {
            if let QueueHandle::Area(q) = q {
                q.shared.starved_structural.store(true, Ordering::SeqCst);
            }
        }
    }

    /// The NIC this session's queues ride (`None` on contract backends).
    pub fn nic_ifindex(&self) -> Option<u32> {
        match self.nic_ifindex.load(Ordering::Relaxed) {
            0 => None,
            ifx => Some(ifx),
        }
    }

    /// Contract seam (round 3): bind a contract-backend session to a
    /// test NIC so the census/sweep laws are exercisable without a
    /// real ifq.
    pub fn set_nic_for_test(&self, ifindex: u32) {
        self.nic_ifindex.store(ifindex, Ordering::Relaxed);
    }

    /// Round-8 no-harm escalation: any queue whose refill starvation
    /// proved STRUCTURAL (two consecutive failover windows without
    /// payload) — the funnel tears the whole session down so the RSS
    /// width restores; kernel path serves at full width.
    pub fn refill_structural(&self) -> bool {
        self.queues.iter().any(|q| match q {
            QueueHandle::Area(q) => q.shared.starved_structural.load(Ordering::SeqCst),
            QueueHandle::Classic(_) => false,
        })
    }

    /// Whether this session's NIC/lease state has been released (the
    /// finding-F teardown latch).
    pub fn torn_down(&self) -> bool {
        self.teardown_latch.load(Ordering::SeqCst)
    }

    /// Ordered full teardown — stop the wire, JOIN every driver, restore
    /// steering (the finding-3 order), then release the rxq lease.
    /// Idempotent; the daemon-shutdown and poison paths share it.
    pub async fn teardown(&self) {
        if self.teardown_latch.swap(true, Ordering::SeqCst) {
            return;
        }
        // Round 6 (gauge honesty): retire the admin watchdog FIRST —
        // quiescing the IO queues makes the target unwind the
        // association and close the admin connection, and that ORDERLY
        // close must never be markable as poison (the field's
        // poisoned=8-with-zero-poison-logs). The latch (checked by the
        // watchdog) is the belt for an EOF racing this abort. Take,
        // abort, AWAIT: retirement is an owned, completed edge before
        // teardown proceeds — `abort()` alone only REQUESTS cancellation
        // (the stop-ship flake: is_finished raced the runtime).
        let handle = self.admin_hold.lock().expect("admin hold lock").take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await; // JoinError::Cancelled is the expected arm
        }
        self.quiesce().await;
        // The lease releases only AFTER the ifqs are joined (quiesce),
        // so a successor arm re-registers a freed queue, not a live one.
        self.rxq_lease.lock().expect("rxq lease lock").take();
        // Round 3 — the whole-NIC no-harm face: a STRUCTURALLY-starved
        // session votes in the per-NIC census once (this body is
        // latch-idempotent), and a majority verdict sweeps the NIC's
        // remaining live sessions — the pool term is per-NIC physics,
        // so the survivors' RSS exclusion is pure rent (round-2 field:
        // 5 of 10 tore down, the other 5 held ~16 % of queue width at
        // ~0 engagement for the rest of the row). Runs AFTER this
        // session's own release (the finding-3 ordering stays
        // per-session); a swept peer is never structural itself, so
        // the sweep recursion ends at depth one.
        if self.refill_structural() {
            if let Some(ifx) = self.nic_ifindex() {
                if super::nic_census::note_structural(ifx) {
                    super::sweep_nic_sessions(ifx, self as *const _ as usize).await;
                }
            }
        }
    }

    /// Fire-and-forget teardown for the poison path (finding F: the
    /// funnel calls it on a poisoned session so ifqs/rules/RSS/lease
    /// release PROMPTLY instead of running the rest of the row at 75 %
    /// RSS width).
    pub fn spawn_teardown(self: &Arc<Self>) {
        if self.torn_down() {
            return;
        }
        let sess = Arc::clone(self);
        tokio::spawn(async move {
            sess.teardown().await;
        });
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
        // Segment plan FIRST — queue assignment (the rotor) happens at
        // plan time because admission is WHOLE-READ atomic (round 8,
        // finding I): every area segment's units are taken in ONE
        // try-acquire per queue before any capsule is issued. The
        // per-segment grain it replaces let racing multi-segment reads
        // shred the window with PARTIAL holds — each read holding one
        // segment's units while its sibling declined — so under
        // over-demand the window admitted FEWER whole reads than it can
        // genuinely hold (worst case zero). Atomic per-read admission
        // restores the arithmetic: a decline can only be witnessed
        // while a full window's worth of whole reads is admitted.
        let mut segs: Vec<(u64, SendMutPtr, usize, usize)> = Vec::with_capacity(len.div_ceil(cap));
        let mut done = 0usize;
        while done < len {
            let seg = cap.min(len - done);
            let seg_off = byte_offset + done as u64;
            let seg_dest = SendMutPtr(unsafe { dest.0.add(done) });
            let qi = self.next_q.fetch_add(1, Ordering::Relaxed) % self.queues.len();
            segs.push((seg_off, seg_dest, seg, qi));
            done += seg;
        }
        let mut admissions = self.admit_whole_read(&segs)?;
        let mut futs = Vec::with_capacity(segs.len());
        for (seg_off, seg_dest, seg, qi) in segs {
            let admission = match &self.queues[qi] {
                QueueHandle::Area(_) => Some(
                    admissions
                        .get_mut(&qi)
                        .and_then(|p| p.split(super::area_queue::admission_units(seg) as usize))
                        .ok_or_else(|| {
                            io_err("lane admission split under-provisioned (invariant)".into())
                        })?,
                ),
                QueueHandle::Classic(_) => None,
            };
            futs.push(self.read_segment(seg_off, seg_dest, seg, qi, admission, keepalive.clone()));
        }
        // The per-queue parents are fully split (0 permits left) —
        // dropping them here releases nothing; the segments' splits are
        // what ride the fills.
        drop(admissions);
        for r in futures::future::join_all(futs).await {
            r?;
        }
        // The gather law's accounting boundary (round 7): count at the
        // WHOLE-read success — per-segment passes sum to `len` on
        // success (byte-identical to the old per-segment counting), and
        // a torn multi-segment read contributes to NEITHER gather nor
        // fill, so gather ≡ fill holds unconditionally (the field's
        // +6.3 MB poisoned-row delta was completed segments of torn
        // reads counted with no fill). Area sessions only — the classic
        // (Z1 contract) backend performs no gather, and its rows must
        // stay gather-silent (the backend-distinction law;
        // `dest_serve_eligible` IS the all-area predicate).
        if self.dest_serve_eligible() {
            crate::fuse_client::METRICS
                .zcrx_gather_bytes
                .fetch_add(len as u64, Ordering::Relaxed);
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

    /// Whole-read atomic admission (design §4.3, round 8 finding I):
    /// bounded in-flight fills, and over-demand DECLINES to the kernel
    /// path immediately — declines are free; the old park-until-permits
    /// was the starvation churn (field: 648 waits vs 206 fills feeding
    /// 41 × 937 ms episodes of held reads). The read's TOTAL units are
    /// taken in ONE `try_acquire` per queue — never per segment: a
    /// partial hold denies window to a read that could complete while
    /// the holder itself goes on to decline (mutual window shredding —
    /// under over-demand the observed admitted-read count fell BELOW
    /// the window's genuine capacity, worst case zero). On any queue's
    /// refusal every already-held permit drops (all-or-nothing across
    /// queues; `try_acquire` never waits, so no deadlock) and the read
    /// declines, counted ONCE per read in `zcrx_area_admission_waits`.
    /// The OWNED permits are split per segment and ride the entries →
    /// the fills (MEM-3): admission accounting stays exact under
    /// cancellation — released when the fill drops, never early.
    fn admit_whole_read(
        &self,
        segs: &[(u64, SendMutPtr, usize, usize)],
    ) -> Result<std::collections::HashMap<usize, tokio::sync::OwnedSemaphorePermit>> {
        use super::area_queue::admission_units;
        let mut totals: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
        for &(_, _, seg, qi) in segs {
            if matches!(&self.queues[qi], QueueHandle::Area(_)) {
                *totals.entry(qi).or_insert(0) += admission_units(seg);
            }
        }
        let mut held = std::collections::HashMap::with_capacity(totals.len());
        for (qi, units) in totals {
            let QueueHandle::Area(q) = &self.queues[qi] else {
                continue;
            };
            match Arc::clone(&q.shared.admission).try_acquire_many_owned(units) {
                Ok(p) => {
                    held.insert(qi, p);
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    // `held` drops here — every already-admitted queue's
                    // units release immediately (nothing rode a fill yet).
                    crate::fuse_client::METRICS
                        .zcrx_area_admission_waits
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "lane admission window full — declined to the kernel path",
                    )));
                }
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(io_err("lane queue closed".into()));
                }
            }
        }
        Ok(held)
    }

    async fn read_segment(
        &self,
        byte_offset: u64,
        dest: SendMutPtr,
        len: usize,
        qi: usize,
        admission: Option<tokio::sync::OwnedSemaphorePermit>,
        keepalive: Option<bytes::Bytes>,
    ) -> Result<()> {
        match &self.queues[qi] {
            QueueHandle::Classic(q) => {
                self.read_segment_classic(q, byte_offset, dest, len, keepalive)
                    .await
            }
            // Area backends never hand `dest` to a driver task — the
            // requester gathers at completion — so the entry needs no
            // destination keep-alive.
            QueueHandle::Area(q) => {
                let admission = admission.ok_or_else(|| {
                    io_err("area segment issued without whole-read admission (invariant)".into())
                })?;
                self.read_segment_area(q, byte_offset, dest, len, admission)
                    .await
            }
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
        admission: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        let shared = &q.shared;
        if shared.poisoned.load(Ordering::SeqCst) {
            return Err(io_err("lane queue poisoned".into()));
        }
        // Admission was taken WHOLE-READ atomic by the caller
        // ([`Self::admit_whole_read`], round 8 finding I); this
        // segment's split of the OWNED permit rides the entry → the
        // fill (MEM-3): admission accounting stays exact under
        // cancellation — released when the fill drops (after the
        // requester's gather, or inside the dead completion channel on
        // a dropped future), never early.
        //
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

        match tokio::time::timeout(LANE_READ_TIMEOUT, rx).await {
            Ok(Ok(Ok(fill))) => {
                // The gather law (design §4.4): ONE pass from refcounted
                // area chunks into the destination; dropping the fill
                // releases the refs → chunks recycle to the refill path.
                // SAFETY: `dest..dest+len` is the caller's exclusive
                // destination window (read-path serve buffer); spans were
                // bounds-validated at record time (`on_c2h_span`).
                unsafe { fill.gather_into(dest.0) };
                Ok(())
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(io_err("lane completion channel dropped".into())),
            Err(_) => {
                // Quiescence law: drain (abort-and-join tasks / close +
                // join the ring driver) so no lane context survives
                // holding chunk refs, then poison loud (drain=true: the
                // drivers are provably joined). No pre-store: the poison
                // funnel owns the flag flip AND the log (round 8 — the
                // pre-store suppressed the first-swap log: silent
                // poison). drain() needs no flag — it closes the sink.
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

        match tokio::time::timeout(LANE_READ_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(io_err("lane completion channel dropped".into())),
            Err(_) => {
                // Quiescence law: the reader task holds raw pointers into
                // this (and other) destination buffers. Before this frame
                // returns — and the caller's buffer can be dropped/recycled
                // — abort AND join the queue's tasks so no writer survives;
                // only then drain (dropping entry keep-alives).
                // No pre-store (round 8): the poison funnel owns the
                // flag flip and the log; abort+join needs no flag.
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
                            "lane read failed: {}",
                            pdu::describe_status(cqe.status)
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
