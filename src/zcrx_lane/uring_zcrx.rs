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

/// **Provenance gate** (design §5): a RECV_ZC completion's `big0` carries
/// `(area_token_high << IORING_ZCRX_AREA_SHIFT) | offset`. The high bits
/// must name OUR registered area — a mismatch means the CQE describes
/// memory this lane does not own, which is a frame violation, not a
/// recoverable error.
///
/// Extracted (spec §11 TEST-6) so the two gate-chain checks the driver
/// loop performs on every completion are reachable without a
/// zcrx-capable NIC.
fn cqe_area_matches(raw_off: u64, area_token: u64) -> bool {
    raw_off >> IORING_ZCRX_AREA_SHIFT == area_token >> IORING_ZCRX_AREA_SHIFT
}

/// **Span-bounds gate**: the completion's `(offset, len)` must land
/// entirely inside the registered area. Returns the in-area byte offset,
/// or `None` when the span escapes it (overflow included — a `res` near
/// `i32::MAX` at a high offset must not wrap into a valid-looking span).
fn cqe_span_offset(raw_off: u64, len: usize, area_len: usize) -> Option<usize> {
    let off = (raw_off & ((1u64 << IORING_ZCRX_AREA_SHIFT) - 1)) as usize;
    off.checked_add(len).filter(|end| *end <= area_len)?;
    Some(off)
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
                        if !cqe_area_matches(raw_off, refill.area_token) {
                            crate::fuse_client::METRICS
                                .zcrx_frame_violations
                                .fetch_add(1, Ordering::Relaxed);
                            return Err("zcrx CQE names a foreign area (provenance)".into());
                        }
                        let len = res as usize;
                        let Some(off) = cqe_span_offset(raw_off, len, cfg.area.len()) else {
                            return Err("zcrx CQE span exceeds the area".into());
                        };
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
                        // SAFETY (MEM-4): `grant` (adopted just above) pins
                        // the chunk for the slice's life and `ptr..ptr+len`
                        // was bounds-checked against the area two lines up.
                        let slice = unsafe { AreaSlice::new(grant, ptr, len) };
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

// ---------------------------------------------------------------------------
// Contracts (spec §11 TEST-6)
// ---------------------------------------------------------------------------
//
// This module was 869 LOC with 18 `unsafe` sites and ZERO test references
// anywhere — a D5 gate-chain link the lane must not ship default-on
// without. The reason it had none is real (a zcrx-capable NIC with HDS
// exists on no dev box or CI runner), but most of what is dangerous here
// is NOT NIC-dependent:
//
//   * the raw io_uring itself — SQ/CQ mmap arithmetic, the CQE32 stride,
//     the head/tail protocol, `IORING_SETUP_SINGLE_ISSUER |
//     DEFER_TASKRUN | CQE32` acceptance — runs on any io_uring kernel;
//   * `RefillRing::post` is pure ring arithmetic over a region WE own;
//   * `RingCmd` is an eventfd doorbell and a queue;
//   * the two per-completion gates (`cqe_area_matches`,
//     `cqe_span_offset`) are pure;
//   * `SlotBag`/`SendBufs` `Drop` are the exit-path laws (release every
//     ledger ref; deliberately FORGET in-flight send buffers because the
//     kernel may still reference them after ring-fd close);
//   * the arm ladder's REFUSAL path (`REGISTER_ZCRX_IFQ` on a
//     non-zcrx interface) is exactly what every dev box produces.
//
// What stays field-owed is the armed serve loop (`.benchmarks/
// 2026-08-04-zcrx-z2.md` names it). Everything below runs anywhere.
#[cfg(test)]
mod tests {
    use super::*;

    fn io_uring_available() -> bool {
        // A minimal probe: if the kernel refuses io_uring entirely
        // (seccomp, io_uring_disabled=2), the ring legs cannot run.
        RawRing::new(4).is_ok()
    }

    // -- the two per-completion gates ----------------------------------

    #[test]
    fn cqe_provenance_gate_rejects_a_foreign_area() {
        let token = 3u64 << IORING_ZCRX_AREA_SHIFT;
        assert!(
            cqe_area_matches(token | 4096, token),
            "our own area token must pass"
        );
        assert!(
            !cqe_area_matches((4u64 << IORING_ZCRX_AREA_SHIFT) | 4096, token),
            "a CQE naming another area is a frame violation, not a retry"
        );
        assert!(
            !cqe_area_matches(4096, token),
            "a zero token against a nonzero area must not pass"
        );
    }

    #[test]
    fn cqe_span_gate_bounds_every_completion_against_the_area() {
        const AREA: usize = 64 * 1024;
        let token = 1u64 << IORING_ZCRX_AREA_SHIFT;
        assert_eq!(cqe_span_offset(token, 4096, AREA), Some(0));
        assert_eq!(cqe_span_offset(token | 4096, 4096, AREA), Some(4096));
        assert_eq!(
            cqe_span_offset(token | (AREA as u64 - 4096), 4096, AREA),
            Some(AREA - 4096),
            "a span ending exactly at the area end is legal"
        );
        assert_eq!(
            cqe_span_offset(token | (AREA as u64 - 4095), 4096, AREA),
            None,
            "one byte past the area is a refusal"
        );
        assert_eq!(
            cqe_span_offset(token | AREA as u64, 1, AREA),
            None,
            "an offset at the area end cannot carry bytes"
        );
        // The overflow arm: a huge offset plus a huge length must not
        // wrap into a valid-looking span.
        assert_eq!(
            cqe_span_offset(token | (u64::MAX >> 16), usize::MAX, AREA),
            None,
            "offset+len must be checked, not computed"
        );
    }

    // -- the raw ring: mmap arithmetic, CQE32 stride, head/tail --------

    #[test]
    fn raw_ring_setup_accepts_the_lane_flags_and_round_trips_a_cqe() {
        if !io_uring_available() {
            eprintln!("io_uring unavailable on this kernel — ring legs skipped");
            return;
        }
        // The lane's exact setup: CQE32 + SINGLE_ISSUER + DEFER_TASKRUN.
        let ring = RawRing::new(8).expect("the lane's ring flags must be accepted");

        // Drive the doorbell arm the driver uses: READ 8 bytes from an
        // eventfd that already holds a count. This exercises push_sqe
        // (SQ array + tail store), enter (DEFER_TASKRUN needs GETEVENTS
        // to run task work), and pop_cqe (the CQE32 stride + big0).
        let cmds = RingCmd::new().expect("eventfd doorbell");
        cmds.send(vec![1, 2, 3]).expect("queue a capsule");
        let mut buf = 0u64;
        ring.push_sqe(&Sqe {
            opcode: IORING_OP_READ,
            fd: cmds.doorbell.as_raw_fd(),
            addr: &mut buf as *mut u64 as u64,
            len: 8,
            user_data: TAG_DOORBELL,
            ..Default::default()
        })
        .expect("push the doorbell SQE");
        ring.enter(1, 1).expect("submit + wait");
        let (ud, res, _flags, _big0) = ring.pop_cqe().expect("one CQE");
        assert_eq!(ud, TAG_DOORBELL, "user_data must survive the round trip");
        assert_eq!(res, 8, "the eventfd read returns its 8-byte counter");
        assert_eq!(buf, 1, "and the doorbell had been rung exactly once");
        assert!(ring.pop_cqe().is_none(), "the CQ head must have advanced");
    }

    #[test]
    fn raw_ring_sq_is_bounded_and_reports_full_rather_than_wrapping() {
        if !io_uring_available() {
            return;
        }
        let ring = RawRing::new(4).expect("ring");
        let mut buf = 0u64;
        let mut pushed = 0;
        // NOP-shaped SQEs (opcode 0) — never submitted, so nothing runs;
        // this exercises the SQ ring-full arithmetic only.
        for _ in 0..64 {
            let r = ring.push_sqe(&Sqe {
                opcode: 0,
                addr: &mut buf as *mut u64 as u64,
                user_data: 7,
                ..Default::default()
            });
            if r.is_err() {
                break;
            }
            pushed += 1;
        }
        assert!(
            pushed <= 64,
            "push_sqe must refuse past capacity, not wrap the tail \
             (pushed {pushed})"
        );
        assert!(pushed >= 4, "a 4-entry ring must accept at least 4");
    }

    // -- the refill ring: pure ring arithmetic over our own region -----

    /// Build a `RefillRing` over an anonymous region laid out the way the
    /// kernel assigns it (head/tail in the first page, rqes after).
    fn fake_refill(entries: u32) -> RefillRing {
        let page = 4096usize;
        let region_len = (page + entries as usize * 16).div_ceil(page) * page;
        let region = Mmap::anon(region_len).expect("anon region");
        RefillRing {
            region,
            head_off: 0,
            tail_off: 8,
            rqes_off: page as u32,
            entries,
            tail_cache: 0,
            area_token: 1u64 << IORING_ZCRX_AREA_SHIFT,
        }
    }

    #[test]
    fn refill_post_encodes_the_rqe_and_advances_the_tail() {
        let mut r = fake_refill(4);
        assert!(r.post(0x1234_5678, 4096), "an empty ring accepts a return");
        // SAFETY: reading back the rqe this call just wrote, in our own
        // region mapping.
        unsafe {
            let rqe = r.region.at::<u8>(r.rqes_off);
            assert_eq!(*(rqe as *const u64), 0x1234_5678, "raw_off at [0..8)");
            assert_eq!(*(rqe.add(8) as *const u32), 4096, "len at [8..12)");
            assert_eq!(*(rqe.add(12) as *const u32), 0, "the pad must be zeroed");
            let tail = (*r.region.at::<AtomicU32>(r.tail_off)).load(Ordering::Acquire);
            assert_eq!(tail, 1, "the published tail must match the cache");
        }
        assert_eq!(r.tail_cache, 1);
    }

    #[test]
    fn refill_post_refuses_when_full_and_recovers_when_the_kernel_consumes() {
        let mut r = fake_refill(4);
        for i in 0..4 {
            assert!(r.post(i as u64 * 4096, 4096), "entry {i} fits");
        }
        assert!(
            !r.post(99, 4096),
            "a full ring must REFUSE (the caller defers the return) — \
             overwriting an unconsumed rqe would hand the NIC a span the \
             lane still owns"
        );
        // The "kernel" consumes two.
        // SAFETY: our own region.
        unsafe {
            (*r.region.at::<AtomicU32>(r.head_off)).store(2, Ordering::Release);
        }
        assert!(r.post(99, 4096), "space freed ⇒ the deferred return posts");
        assert_eq!(r.tail_cache, 5);
    }

    #[test]
    fn refill_indices_wrap_by_mask_without_the_counters_wrapping_first() {
        let mut r = fake_refill(2);
        // Start both cursors near the u32 rollover: the ring must be
        // driven by wrapping_sub distance, not by raw comparison.
        r.tail_cache = u32::MAX - 1;
        // SAFETY: our own region.
        unsafe {
            (*r.region.at::<AtomicU32>(r.head_off)).store(u32::MAX - 1, Ordering::Release);
        }
        assert!(r.post(1, 16), "distance 0 ⇒ space");
        assert!(r.post(2, 16), "distance 1 ⇒ space");
        assert!(!r.post(3, 16), "distance 2 == entries ⇒ full");
        assert_eq!(r.tail_cache, 0, "the tail counter wrapped cleanly");
    }

    // -- the command lane ----------------------------------------------

    #[test]
    fn ring_cmd_queues_capsules_rings_the_doorbell_and_latches_closed() {
        let cmds = RingCmd::new().expect("eventfd");
        cmds.send(b"capsule-1".to_vec()).expect("open lane accepts");
        cmds.send(b"capsule-2".to_vec()).expect("open lane accepts");
        assert_eq!(cmds.q.pop().as_deref(), Some(&b"capsule-1"[..]), "FIFO");
        assert_eq!(cmds.q.pop().as_deref(), Some(&b"capsule-2"[..]));
        assert!(cmds.q.pop().is_none());

        // The doorbell counted every send (plus whatever close adds).
        let mut val = 0u64;
        // SAFETY: an 8-byte read from our own eventfd.
        let n = unsafe {
            libc::read(
                cmds.doorbell.as_raw_fd(),
                &mut val as *mut u64 as *mut libc::c_void,
                8,
            )
        };
        assert_eq!(n, 8, "the doorbell must have been rung");
        assert!(val >= 2, "one wake per send at minimum (got {val})");

        cmds.close();
        assert!(
            cmds.send(b"after-close".to_vec()).is_err(),
            "a closed lane must refuse — a capsule queued after teardown \
             would never be sent and its requester would hang"
        );
        assert!(cmds.q.pop().is_none(), "and nothing was queued");
    }

    #[test]
    fn ring_cmd_close_wakes_a_parked_driver() {
        let cmds = RingCmd::new().expect("eventfd");
        // Drain whatever creation left (nothing) and confirm close rings.
        cmds.close();
        let mut val = 0u64;
        // SAFETY: an 8-byte read from our own eventfd.
        let n = unsafe {
            libc::read(
                cmds.doorbell.as_raw_fd(),
                &mut val as *mut u64 as *mut libc::c_void,
                8,
            )
        };
        assert_eq!(n, 8, "close MUST ring the doorbell");
        assert!(val >= 1, "or a parked driver never observes the teardown");
    }

    // -- the exit-path laws --------------------------------------------

    #[test]
    fn slot_bag_drop_returns_every_ledger_ref() {
        let area = super::super::area::ZcrxArea::new(256 * 1024, 64 * 1024, None)
            .expect("a small anonymous area");
        let total = area.free_chunks();
        assert!(total >= 2, "need at least two chunks to be meaningful");
        {
            let mut bag = SlotBag::new(Arc::clone(&area));
            for _ in 0..total {
                let g = area.try_grant_chunk().expect("a free chunk");
                bag.slots.push(g.into_raw_slot());
            }
            assert_eq!(area.free_chunks(), 0, "the bag holds every ref");
            assert!(
                area.try_grant_chunk().is_none(),
                "an exhausted ledger grants nothing"
            );
        }
        assert_eq!(
            area.free_chunks(),
            total,
            "SlotBag::drop is the driver-exit arm of the recycle law — \
             every raw ref must return, or the lane leaks its area one \
             session death at a time"
        );
    }

    #[test]
    fn send_bufs_drop_forgets_in_flight_capsules_but_frees_completed_ones() {
        // The law (module docs): a buffer removed at CQE time drops
        // normally — the kernel is done with it. Whatever REMAINS at
        // driver exit is forgotten deliberately: ring-fd close cancels
        // in-flight ops asynchronously, so freeing a buffer the kernel
        // may still reference is a use-after-free window. The leak is
        // bounded by queue depth.
        let mut bufs = SendBufs(std::collections::HashMap::new());
        bufs.0.insert(TAG_SEND_BASE + 1, (vec![0xAAu8; 64], 0));
        bufs.0.insert(TAG_SEND_BASE + 2, (vec![0xBBu8; 64], 0));
        // A completion removes one: it drops here, normally.
        let (done, sent) = bufs.0.remove(&(TAG_SEND_BASE + 1)).expect("present");
        assert_eq!(sent, 0);
        assert_eq!(done.len(), 64);
        drop(done);
        assert_eq!(bufs.0.len(), 1, "one still in flight");
        // Dropping the map forgets the remaining one. Under ASan/valgrind
        // this is the difference between a bounded leak and a UAF.
        drop(bufs);
    }

    // -- the arm ladder's refusal path ---------------------------------

    #[test]
    fn driver_reports_the_ifq_registration_refusal_and_poisons_the_lane() {
        if !io_uring_available() {
            return;
        }
        // No dev box has a zcrx-capable NIC, so REGISTER_ZCRX_IFQ against
        // an interface index that cannot serve it is the universally
        // reachable arm. The contract: the refusal reaches the arm ladder
        // through `ready` (naming the interface), the driver exits, and
        // the lane is poisoned rather than left half-armed.
        let area =
            super::super::area::ZcrxArea::new(256 * 1024, 64 * 1024, None).expect("test area");
        let shared = super::super::area_queue::AreaShared::new(4, &area);
        let cmds = RingCmd::new().expect("eventfd");
        let session_poison = Arc::new(AtomicBool::new(false));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let addr = listener.local_addr().expect("addr");
        let sock = std::net::TcpStream::connect(addr).expect("loopback connect");
        let _accepted = listener.accept().expect("accept");

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (_go_tx, go_rx) = std::sync::mpsc::channel();
        let h = spawn_ring_driver(
            RingDriverConfig {
                // Interface index 0 is never a real NIC.
                ifindex: 0,
                rxq: 0,
                numa_node: None,
                rq_entries: 16,
                sq_entries: 16,
                area: Arc::clone(&area),
                shared: Arc::clone(&shared),
                cmds: Arc::clone(&cmds),
                session_poison: Arc::clone(&session_poison),
                sock,
            },
            ready_tx,
            go_rx,
        );
        let verdict = ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the arm ladder must always hear back from the driver");
        let why = verdict.expect_err("ifindex 0 cannot register a zcrx ifq");
        assert!(
            why.contains("REGISTER_ZCRX_IFQ"),
            "the refusal must name the failing step: {why}"
        );
        h.join().expect("the driver thread exits, never wedges");
        assert!(
            session_poison.load(Ordering::SeqCst),
            "a failed arm must poison the session — a half-armed lane that \
             reports neither ready nor poisoned strands every requester"
        );
    }

    #[test]
    fn driver_unwound_before_go_exits_clean_without_poisoning() {
        if !io_uring_available() {
            return;
        }
        // The `go = false` path: steering could not be applied, so the
        // arm unwinds. Nothing was armed, so nothing may be poisoned —
        // the lane must fall back to the kernel path silently.
        //
        // Registration fails first on a dev box, so this leg asserts the
        // weaker but still load-bearing half: the driver never leaves the
        // `ready` channel silent and never wedges its thread. (The
        // go=false arm proper is field-owed with the armed serve loop.)
        let area =
            super::super::area::ZcrxArea::new(256 * 1024, 64 * 1024, None).expect("test area");
        let shared = super::super::area_queue::AreaShared::new(4, &area);
        let cmds = RingCmd::new().expect("eventfd");
        let session_poison = Arc::new(AtomicBool::new(false));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let addr = listener.local_addr().expect("addr");
        let sock = std::net::TcpStream::connect(addr).expect("connect");
        let _accepted = listener.accept().expect("accept");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        let h = spawn_ring_driver(
            RingDriverConfig {
                ifindex: 0,
                rxq: 0,
                numa_node: None,
                rq_entries: 16,
                sq_entries: 16,
                area,
                shared,
                cmds,
                session_poison,
                sock,
            },
            ready_tx,
            go_rx,
        );
        let _ = go_tx.send(false);
        assert!(
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .is_ok(),
            "the driver must always answer the arm ladder"
        );
        h.join().expect("and its thread must always exit");
    }
}
