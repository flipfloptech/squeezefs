//! The REAL zcrx receive backend (design §5): one lane queue = one raw
//! io_uring (DEFER_TASKRUN | SINGLE_ISSUER | CQE32) = one registered ifq
//! (`IORING_REGISTER_ZCRX_IFQ`) = one NIC RX queue = one TCP connection,
//! multishot `RECV_ZC` with the refill ring fed by the span-grant ledger,
//! command capsules as `SEND` SQEs on the same ring (design §3).
//!
//! **Field-owed execution**: this module needs a zcrx-capable NIC (HDS on,
//! kernel provider) — no such device exists on the dev box or CI, so the
//! machinery ABOVE the io_uring seam (parser, fill table, ledger, poison
//! lattice, steering state machine) is what local contracts pin; this
//! driver compiles everywhere, arms only behind the full probe ladder,
//! and its live evidence is the reformat-window bracket
//! (`.benchmarks/2026-08-04-zcrx-z2.md` names exactly what it owes).
//!
//! uAPI mirrors below follow `<linux/io_uring.h>` as shipped with the
//! field kernels (7.1.2 / 6.19.14-sqz — both carry the same zcrx ABI;
//! layout-asserted at the bottom).
//!
//! Threading law: SINGLE_ISSUER + DEFER_TASKRUN require every submit
//! (and the registration itself) to come from ONE task context — the
//! driver thread performs ring setup, ifq registration, arm, and the
//! whole serve loop; the arm ladder handshakes over std channels
//! (setup-ready → steer → go). NUMA: the thread pins to the NIC's node
//! before touching the ring or the area (design §8).

use super::area::{AreaSlice, ZcrxArea};
use super::area_queue::AreaShared;
use super::pdu_stream::{ParseEvent, StreamParser};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

// ------------------------------------------------------------------ uapi mirror

const SYS_IO_URING_SETUP: libc::c_long = 425;
const SYS_IO_URING_ENTER: libc::c_long = 426;
const SYS_IO_URING_REGISTER: libc::c_long = 427;

const IORING_SETUP_CQSIZE: u32 = 1 << 3;
const IORING_SETUP_CLAMP: u32 = 1 << 4;
const IORING_SETUP_CQE32: u32 = 1 << 11;
const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;
const IORING_SETUP_DEFER_TASKRUN: u32 = 1 << 13;

const IORING_ENTER_GETEVENTS: u32 = 1 << 0;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x8000000;
const IORING_OFF_SQES: i64 = 0x10000000;

const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;

const IORING_REGISTER_ZCRX_IFQ: u32 = 32;

const IORING_OP_SEND: u8 = 26;
const IORING_OP_READ: u8 = 22;
const IORING_RECV_MULTISHOT: u16 = 1 << 1;
const IORING_CQE_F_MORE: u32 = 1 << 1;

const IORING_MEM_REGION_TYPE_USER: u32 = 1;
const IORING_ZCRX_AREA_SHIFT: u64 = 48;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoSqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoCqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqringOffsets,
    cq_off: IoCqringOffsets,
}

/// 64-byte SQE (only the fields the lane submits).
#[repr(C)]
#[derive(Clone, Copy)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    zcrx_ifq_idx: u32,
    addr3: u64,
    pad2: u64,
}

impl Default for Sqe {
    fn default() -> Self {
        // SAFETY: all-zero is a valid SQE scaffold.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ZcrxOffsets {
    head: u32,
    tail: u32,
    rqes: u32,
    resv2: u32,
    resv: [u64; 2],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ZcrxAreaReg {
    addr: u64,
    len: u64,
    rq_area_token: u64,
    flags: u32,
    dmabuf_fd: u32,
    resv2: [u64; 2],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RegionDesc {
    user_addr: u64,
    size: u64,
    flags: u32,
    id: u32,
    mmap_offset: u64,
    resv: [u64; 4],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ZcrxIfqReg {
    if_idx: u32,
    if_rxq: u32,
    rq_entries: u32,
    flags: u32,
    area_ptr: u64,
    region_ptr: u64,
    offsets: ZcrxOffsets,
    zcrx_id: u32,
    rx_buf_len: u32,
    resv: [u64; 3],
}

// Layout law: byte-verified against a compiled C probe of the kernel
// headers (sizes/offsets from <linux/io_uring.h>, kernel 7.1 uapi).
const _: () = {
    assert!(std::mem::size_of::<Sqe>() == 64);
    assert!(std::mem::offset_of!(Sqe, user_data) == 32);
    assert!(std::mem::offset_of!(Sqe, zcrx_ifq_idx) == 44);
    assert!(std::mem::size_of::<IoUringParams>() == 120);
    assert!(std::mem::size_of::<ZcrxIfqReg>() == 96);
    assert!(std::mem::offset_of!(ZcrxIfqReg, offsets) == 32);
    assert!(std::mem::offset_of!(ZcrxIfqReg, zcrx_id) == 64);
    assert!(std::mem::size_of::<ZcrxOffsets>() == 32);
    assert!(std::mem::size_of::<ZcrxAreaReg>() == 48);
    assert!(std::mem::size_of::<RegionDesc>() == 64);
};

// ------------------------------------------------------------- ring plumbing

fn errno_str(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

struct Mmap {
    ptr: *mut u8,
    len: usize,
}

impl Mmap {
    fn ring(fd: RawFd, len: usize, off: i64) -> Result<Mmap, String> {
        // SAFETY: standard io_uring ring mmap.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                off,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(errno_str("ring mmap"));
        }
        Ok(Mmap {
            ptr: ptr as *mut u8,
            len,
        })
    }
    fn anon(len: usize) -> Result<Mmap, String> {
        // SAFETY: fresh anonymous mapping (the refill-ring region).
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(errno_str("region mmap"));
        }
        Ok(Mmap {
            ptr: ptr as *mut u8,
            len,
        })
    }
    /// Typed pointer at a byte offset.
    fn at<T>(&self, off: u32) -> *mut T {
        debug_assert!(off as usize + std::mem::size_of::<T>() <= self.len);
        // SAFETY: bounds-asserted; alignment guaranteed by kernel layout.
        unsafe { self.ptr.add(off as usize) as *mut T }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

/// One raw io_uring instance (driver-thread-owned; see threading law).
struct RawRing {
    fd: OwnedFd,
    sq: Mmap,
    sqes: Mmap,
    /// CQ view (shares `sq`'s mapping under FEAT_SINGLE_MMAP).
    cq_ptr: *mut u8,
    sq_off: IoSqringOffsets,
    cq_off: IoCqringOffsets,
    sq_entries: u32,
    /// Kept alive if the kernel required a split CQ mapping.
    _cq_map: Option<Mmap>,
}

impl RawRing {
    fn new(entries: u32) -> Result<RawRing, String> {
        let mut p = IoUringParams {
            flags: IORING_SETUP_CQE32
                | IORING_SETUP_SINGLE_ISSUER
                | IORING_SETUP_DEFER_TASKRUN
                | IORING_SETUP_CQSIZE
                | IORING_SETUP_CLAMP,
            cq_entries: entries * 4,
            ..Default::default()
        };
        // SAFETY: io_uring_setup with an out-param struct.
        let fd = unsafe { libc::syscall(SYS_IO_URING_SETUP, entries, &mut p) };
        if fd < 0 {
            return Err(errno_str("io_uring_setup"));
        }
        // SAFETY: fresh owned fd.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

        let cqe_sz = 32usize; // CQE32
        let sq_len = p.sq_off.array as usize + p.sq_entries as usize * 4;
        let cq_len = p.cq_off.cqes as usize + p.cq_entries as usize * cqe_sz;
        let single = p.features & IORING_FEAT_SINGLE_MMAP != 0;
        let sq = Mmap::ring(fd.as_raw_fd(), sq_len.max(cq_len), IORING_OFF_SQ_RING)?;
        let (cq_ptr, cq_map) = if single {
            (sq.ptr, None)
        } else {
            let m = Mmap::ring(fd.as_raw_fd(), cq_len, IORING_OFF_CQ_RING)?;
            (m.ptr, Some(m))
        };
        let sqes = Mmap::ring(
            fd.as_raw_fd(),
            p.sq_entries as usize * std::mem::size_of::<Sqe>(),
            IORING_OFF_SQES,
        )?;
        Ok(RawRing {
            fd,
            sq,
            sqes,
            cq_ptr,
            sq_off: p.sq_off,
            cq_off: p.cq_off,
            sq_entries: p.sq_entries,
            _cq_map: cq_map,
        })
    }

    /// Queue one SQE (single-issuer thread; no concurrent producers).
    fn push_sqe(&self, sqe: &Sqe) -> Result<(), String> {
        // SAFETY: all pointers derive from the kernel-described offsets.
        unsafe {
            let tail_p = self.sq.at::<AtomicU32>(self.sq_off.tail);
            let head_p = self.sq.at::<AtomicU32>(self.sq_off.head);
            let mask = *self.sq.at::<u32>(self.sq_off.ring_mask);
            let tail = (*tail_p).load(Ordering::Relaxed);
            let head = (*head_p).load(Ordering::Acquire);
            if tail.wrapping_sub(head) >= self.sq_entries {
                return Err("SQ ring full (lane driver overcommit)".into());
            }
            let idx = tail & mask;
            *(self.sqes.at::<Sqe>(idx * std::mem::size_of::<Sqe>() as u32)) = *sqe;
            *self.sq.at::<u32>(self.sq_off.array + idx * 4) = idx;
            (*tail_p).store(tail.wrapping_add(1), Ordering::Release);
        }
        Ok(())
    }

    /// Submit queued SQEs and wait for ≥ `wait` completions.
    fn enter(&self, to_submit: u32, wait: u32) -> Result<u32, String> {
        // SAFETY: plain io_uring_enter.
        let rc = unsafe {
            libc::syscall(
                SYS_IO_URING_ENTER,
                self.fd.as_raw_fd(),
                to_submit,
                wait,
                IORING_ENTER_GETEVENTS,
                std::ptr::null::<libc::c_void>(),
                0usize,
            )
        };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(0);
            }
            return Err(format!("io_uring_enter: {e}"));
        }
        Ok(rc as u32)
    }

    /// Pop one CQE `(user_data, res, flags, big0)`.
    fn pop_cqe(&self) -> Option<(u64, i32, u32, u64)> {
        // SAFETY: kernel-described CQ offsets; CQE32 stride.
        unsafe {
            let head_p = {
                let p = self.cq_ptr.add(self.cq_off.head as usize);
                &*(p as *const AtomicU32)
            };
            let tail_p = {
                let p = self.cq_ptr.add(self.cq_off.tail as usize);
                &*(p as *const AtomicU32)
            };
            let head = head_p.load(Ordering::Relaxed);
            if head == tail_p.load(Ordering::Acquire) {
                return None;
            }
            let mask = *(self.cq_ptr.add(self.cq_off.ring_mask as usize) as *const u32);
            let base = self
                .cq_ptr
                .add(self.cq_off.cqes as usize + ((head & mask) as usize) * 32);
            let user_data = *(base as *const u64);
            let res = *(base.add(8) as *const i32);
            let flags = *(base.add(12) as *const u32);
            let big0 = *(base.add(16) as *const u64);
            head_p.store(head.wrapping_add(1), Ordering::Release);
            Some((user_data, res, flags, big0))
        }
    }

    /// `io_uring_register` passthrough.
    fn register(&self, opcode: u32, arg: *const libc::c_void, nr: u32) -> Result<(), String> {
        // SAFETY: plain io_uring_register.
        let rc =
            unsafe { libc::syscall(SYS_IO_URING_REGISTER, self.fd.as_raw_fd(), opcode, arg, nr) };
        if rc < 0 {
            return Err(errno_str("io_uring_register"));
        }
        Ok(())
    }
}

/// The registered refill ring view (region memory is ours; offsets are
/// kernel-assigned at ifq registration).
struct RefillRing {
    region: Mmap,
    head_off: u32,
    tail_off: u32,
    rqes_off: u32,
    entries: u32,
    tail_cache: u32,
    area_token: u64,
}

impl RefillRing {
    /// Post one span return; `false` = ring momentarily full (caller
    /// retries next pass — with records ≤ entries this cannot wedge).
    fn post(&mut self, raw_off: u64, len: u32) -> bool {
        // SAFETY: kernel-assigned offsets into our own region mapping.
        unsafe {
            let head = (*self.region.at::<AtomicU32>(self.head_off)).load(Ordering::Acquire);
            if self.tail_cache.wrapping_sub(head) >= self.entries {
                return false;
            }
            let idx = (self.tail_cache & (self.entries - 1)) as usize;
            let rqe = self.region.at::<u8>(self.rqes_off).add(idx * 16);
            *(rqe as *mut u64) = raw_off;
            *(rqe.add(8) as *mut u32) = len;
            *(rqe.add(12) as *mut u32) = 0;
            self.tail_cache = self.tail_cache.wrapping_add(1);
            (*self.region.at::<AtomicU32>(self.tail_off)).store(self.tail_cache, Ordering::Release);
        }
        true
    }
}

// -------------------------------------------------------------- command lane

/// The requester→driver command lane (capsules) + eventfd doorbell.
pub(crate) struct RingCmd {
    q: crossbeam::queue::SegQueue<Vec<u8>>,
    doorbell: OwnedFd,
    closed: AtomicBool,
}

impl RingCmd {
    pub(crate) fn new() -> Result<Arc<RingCmd>, String> {
        // SAFETY: plain eventfd(2).
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(errno_str("eventfd"));
        }
        Ok(Arc::new(RingCmd {
            q: crossbeam::queue::SegQueue::new(),
            // SAFETY: fresh owned fd.
            doorbell: unsafe { OwnedFd::from_raw_fd(fd) },
            closed: AtomicBool::new(false),
        }))
    }

    pub(crate) fn send(&self, capsule: Vec<u8>) -> Result<(), ()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(());
        }
        self.q.push(capsule);
        self.ring_doorbell();
        Ok(())
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.ring_doorbell();
    }

    fn ring_doorbell(&self) {
        let one: u64 = 1;
        // SAFETY: 8-byte write to our eventfd.
        unsafe {
            libc::write(
                self.doorbell.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            )
        };
    }
}

// ---------------------------------------------------------------- the driver

/// Everything the driver thread needs (built by the arm ladder).
pub(crate) struct RingDriverConfig {
    pub ifindex: u32,
    pub rxq: u32,
    pub numa_node: Option<usize>,
    pub rq_entries: u32,
    /// Derived SQ depth: `next_pow2(queue_depth + 8)` — commands in
    /// flight + the recv/doorbell arms + partial-send resubmits. No
    /// fixed constants (design §8).
    pub sq_entries: u32,
    pub area: Arc<ZcrxArea>,
    pub shared: Arc<AreaShared>,
    pub cmds: Arc<RingCmd>,
    pub session_poison: Arc<AtomicBool>,
    /// The lane connection (established + IO-Connected on the async side).
    pub sock: std::net::TcpStream,
}

const TAG_RECV: u64 = 1;
const TAG_DOORBELL: u64 = 2;
const TAG_SEND_BASE: u64 = 0x1000;

/// Raw ledger refs the driver holds (grantable span records). Drop
/// releases every ref — the driver-exit arm of the recycle law (slots
/// embedded in pending fills release via the fill table's fail_all).
struct SlotBag {
    area: Arc<ZcrxArea>,
    slots: Vec<u32>,
}

impl SlotBag {
    fn new(area: Arc<ZcrxArea>) -> SlotBag {
        SlotBag {
            area,
            slots: Vec::new(),
        }
    }
}

impl Drop for SlotBag {
    fn drop(&mut self) {
        for s in self.slots.drain(..) {
            // SAFETY: the bag holds exactly the raw refs the driver owns.
            unsafe { self.area.release_raw(s) };
        }
    }
}

/// In-flight SEND capsule buffers, keyed by user_data. Buffers removed
/// at CQE time drop normally (the kernel is done with them); whatever
/// remains at driver exit is FORGOTTEN deliberately — ring-fd close
/// cancels in-flight ops asynchronously in the kernel, so freeing a
/// possibly-still-referenced buffer is a use-after-free window. The
/// leak is bounded (≤ queue-depth tiny capsules) and paid at most once
/// per session death.
struct SendBufs(std::collections::HashMap<u64, (Vec<u8>, usize)>);

impl Drop for SendBufs {
    fn drop(&mut self) {
        for (_, (buf, _)) in self.0.drain() {
            std::mem::forget(buf);
        }
    }
}

/// Spawn the driver thread. `ready` resolves once ring + ifq registration
/// succeeded (or with the refusal); the driver then parks until `go`
/// (steering applied) before arming RECV_ZC.
pub(crate) fn spawn_ring_driver(
    cfg: RingDriverConfig,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
    go: std::sync::mpsc::Receiver<bool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("sqz-zcrx-q{}", cfg.rxq))
        .spawn(move || {
            let shared = Arc::clone(&cfg.shared);
            let poison = Arc::clone(&cfg.session_poison);
            if let Err(why) = drive(cfg, ready, go) {
                // drain=true: the driver thread is exiting — no further
                // events can reach the fill table from this queue.
                shared.poison(&why, &poison, true);
            }
        })
        .expect("spawn zcrx driver thread")
}

fn drive(
    cfg: RingDriverConfig,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
    go: std::sync::mpsc::Receiver<bool>,
) -> Result<(), String> {
    // NUMA: initiator thread on the NIC's node (design §8).
    if let Some(node) = cfg.numa_node {
        crate::numa_core::topology().pin_current_to_node(node);
    }

    // Ring + ifq registration ON this thread (SINGLE_ISSUER law).
    let setup = (|| -> Result<(RawRing, RefillRing, u32), String> {
        let ring = RawRing::new(cfg.sq_entries)?;
        let page = super::area::chunk_bytes_default();
        let region_len = (page + cfg.rq_entries as usize * 16).div_ceil(page) * page;
        let region = Mmap::anon(region_len)?;
        // The kernel writes back through area_ptr (rq_area_token),
        // region_ptr (mmap_offset) and the ifq struct itself (offsets,
        // clamped rq_entries, zcrx_id) — all three passed as mut.
        let mut area_reg = ZcrxAreaReg {
            addr: cfg.area.base() as u64,
            len: cfg.area.len() as u64,
            ..Default::default()
        };
        let mut region_desc = RegionDesc {
            user_addr: region.ptr as u64,
            size: region_len as u64,
            flags: IORING_MEM_REGION_TYPE_USER,
            ..Default::default()
        };
        let mut ifq = ZcrxIfqReg {
            if_idx: cfg.ifindex,
            if_rxq: cfg.rxq,
            rq_entries: cfg.rq_entries,
            area_ptr: &mut area_reg as *mut _ as u64,
            region_ptr: &mut region_desc as *mut _ as u64,
            ..Default::default()
        };
        ring.register(
            IORING_REGISTER_ZCRX_IFQ,
            &mut ifq as *mut _ as *const libc::c_void,
            1,
        )
        .map_err(|e| {
            format!(
                "REGISTER_ZCRX_IFQ (if_idx={}, rxq={}): {e}",
                cfg.ifindex, cfg.rxq
            )
        })?;
        let refill = RefillRing {
            region,
            head_off: ifq.offsets.head,
            tail_off: ifq.offsets.tail,
            rqes_off: ifq.offsets.rqes,
            entries: ifq.rq_entries,
            tail_cache: 0,
            area_token: area_reg.rq_area_token,
        };
        Ok((ring, refill, ifq.zcrx_id))
    })();

    let (ring, mut refill, zcrx_id) = match setup {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };

    // Park until steering is applied; a `false` (arm unwound) exits
    // silently — nothing armed, nothing to poison.
    if !matches!(go.recv(), Ok(true)) {
        return Ok(());
    }

    let sock_fd = cfg.sock.as_raw_fd();
    let mut doorbell_buf = 0u64;
    let arm_doorbell = |ring: &RawRing, buf: &mut u64| -> Result<(), String> {
        ring.push_sqe(&Sqe {
            opcode: IORING_OP_READ,
            fd: cfg.cmds.doorbell.as_raw_fd(),
            addr: buf as *mut u64 as u64,
            len: 8,
            user_data: TAG_DOORBELL,
            ..Default::default()
        })
    };
    let arm_recv = |ring: &RawRing| -> Result<(), String> {
        ring.push_sqe(&Sqe {
            opcode: super::probe::IORING_OP_RECV_ZC,
            fd: sock_fd,
            ioprio: IORING_RECV_MULTISHOT,
            zcrx_ifq_idx: zcrx_id,
            user_data: TAG_RECV,
            ..Default::default()
        })
    };
    arm_doorbell(&ring, &mut doorbell_buf)?;
    arm_recv(&ring)?;

    // Span records: the (raw_off, len) each grant slot carries until its
    // refs drop and the rqe posts (driver-owned; slot-indexed).
    let mut side: Vec<(u64, u32)> = vec![(0, 0); cfg.area.chunk_count()];
    // Grantable records (rqe already posted or fresh); raw ledger refs
    // released by the bags on ANY exit path.
    let mut ready_slots = SlotBag::new(Arc::clone(&cfg.area));
    // Slots whose rqe post bounced on a momentarily-full ring.
    let mut deferred_returns = SlotBag::new(Arc::clone(&cfg.area));
    let mut parser = StreamParser::new();
    let mut events: Vec<ParseEvent> = Vec::new();
    let mut inflight_sends = SendBufs(std::collections::HashMap::new());
    let mut send_seq: u64 = 0;
    let mut to_submit: u32 = 2; // doorbell + recv already queued

    let offset_mask: u64 = (1u64 << IORING_ZCRX_AREA_SHIFT) - 1;
    loop {
        if cfg.shared.poisoned.load(Ordering::SeqCst) || cfg.cmds.closed.load(Ordering::SeqCst) {
            // Drain path: closing the ring fd is the teardown — the
            // kernel restarts the NIC queue and reclaims the provider
            // (design §5 registration-order law, reverse).
            return Ok(());
        }

        // Returned spans → rqes → grantable records.
        for slot in std::mem::take(&mut deferred_returns.slots) {
            let (off, len) = side[slot as usize];
            if refill.post(off, len) {
                side[slot as usize] = (0, 0);
                ready_slots.slots.push(slot);
            } else {
                deferred_returns.slots.push(slot);
            }
        }
        while let Some(grant) = cfg.area.try_grant_chunk() {
            // The driver owns the ledger ref raw (into_raw_slot keeps
            // it while dropping the embedded area Arc properly).
            let slot = grant.into_raw_slot();
            let (off, len) = side[slot as usize];
            if len == 0 {
                ready_slots.slots.push(slot); // fresh — nothing to return
            } else if refill.post(off, len) {
                side[slot as usize] = (0, 0);
                ready_slots.slots.push(slot);
            } else {
                deferred_returns.slots.push(slot);
            }
        }

        // Commands → SEND SQEs.
        while let Some(capsule) = cfg.cmds.q.pop() {
            send_seq += 1;
            let ud = TAG_SEND_BASE + send_seq;
            ring.push_sqe(&Sqe {
                opcode: IORING_OP_SEND,
                fd: sock_fd,
                addr: capsule.as_ptr() as u64,
                len: capsule.len() as u32,
                op_flags: libc::MSG_NOSIGNAL as u32,
                user_data: ud,
                ..Default::default()
            })?;
            inflight_sends.0.insert(ud, (capsule, 0));
            to_submit += 1;
        }

        ring.enter(std::mem::take(&mut to_submit), 1)?;

        while let Some((ud, res, flags, big0)) = ring.pop_cqe() {
            match ud {
                TAG_RECV => {
                    if res > 0 {
                        let raw_off = big0;
                        if raw_off >> IORING_ZCRX_AREA_SHIFT
                            != refill.area_token >> IORING_ZCRX_AREA_SHIFT
                        {
                            crate::fuse_client::METRICS
                                .zcrx_frame_violations
                                .fetch_add(1, Ordering::Relaxed);
                            return Err("zcrx CQE names a foreign area (provenance)".into());
                        }
                        let off = (raw_off & offset_mask) as usize;
                        let len = res as usize;
                        if off + len > cfg.area.len() {
                            return Err("zcrx CQE span exceeds the area".into());
                        }
                        let slot = match ready_slots.slots.pop() {
                            Some(s) => s,
                            // Late grants may be sitting in the free
                            // stack (refs dropped since the loop-top
                            // drain) — take one whose return can post.
                            None => loop {
                                let Some(grant) = cfg.area.try_grant_chunk() else {
                                    return Err("zcrx span records exhausted".into());
                                };
                                let s = grant.into_raw_slot();
                                let (off, len) = side[s as usize];
                                if len == 0 {
                                    break s;
                                }
                                if refill.post(off, len) {
                                    side[s as usize] = (0, 0);
                                    break s;
                                }
                                deferred_returns.slots.push(s);
                            },
                        };
                        side[slot as usize] = (raw_off, res as u32);
                        // SAFETY: slot ownership transferred from the
                        // forgotten grant ref; released via AreaSlice drop.
                        let grant = unsafe { cfg.area.adopt_grant(slot) };
                        // SAFETY: span bounds checked against the area.
                        let ptr = unsafe { cfg.area.base().add(off) as *const u8 };
                        let slice = AreaSlice::new(grant, ptr, len);
                        events.clear();
                        if let Err(why) = parser.push(&slice, &mut events) {
                            crate::fuse_client::METRICS
                                .zcrx_frame_violations
                                .fetch_add(1, Ordering::Relaxed);
                            return Err(why);
                        }
                        drop(slice);
                        for ev in events.drain(..) {
                            if let Err(why) = cfg.shared.table.apply(ev) {
                                crate::fuse_client::METRICS
                                    .zcrx_frame_violations
                                    .fetch_add(1, Ordering::Relaxed);
                                return Err(why);
                            }
                        }
                    }
                    if flags & IORING_CQE_F_MORE == 0 {
                        if res == 0 {
                            if cfg.shared.table.is_empty() {
                                return Ok(());
                            }
                            return Err("connection closed mid-operation".into());
                        }
                        if res == -libc::ENOBUFS {
                            // rq starved: returns above free space; re-arm.
                            arm_recv(&ring)?;
                            to_submit += 1;
                        } else if res < 0 {
                            return Err(format!(
                                "RECV_ZC terminal: {}",
                                std::io::Error::from_raw_os_error(-res)
                            ));
                        } else {
                            arm_recv(&ring)?;
                            to_submit += 1;
                        }
                    }
                }
                TAG_DOORBELL => {
                    arm_doorbell(&ring, &mut doorbell_buf)?;
                    to_submit += 1;
                }
                ud if ud > TAG_SEND_BASE => {
                    let Some((capsule, sent)) = inflight_sends.0.remove(&ud) else {
                        return Err("SEND CQE for unknown buffer".into());
                    };
                    if res < 0 {
                        return Err(format!(
                            "capsule send: {}",
                            std::io::Error::from_raw_os_error(-res)
                        ));
                    }
                    let sent = sent + res as usize;
                    if sent < capsule.len() {
                        // Partial send: resubmit the remainder.
                        ring.push_sqe(&Sqe {
                            opcode: IORING_OP_SEND,
                            fd: sock_fd,
                            addr: capsule[sent..].as_ptr() as u64,
                            len: (capsule.len() - sent) as u32,
                            op_flags: libc::MSG_NOSIGNAL as u32,
                            user_data: ud,
                            ..Default::default()
                        })?;
                        inflight_sends.0.insert(ud, (capsule, sent));
                        to_submit += 1;
                    }
                }
                other => return Err(format!("unknown CQE user_data {other:#x}")),
            }
        }
    }
}
