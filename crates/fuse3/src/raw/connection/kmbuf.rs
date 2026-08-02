//! **kmbuf / FUSE-zc adoption surface** (fuse3 transport geometry + zc
//! adoption campaign, 2026-08-04) — THE severable module boundary.
//!
//! This module carries every constant, layout, probe, and per-queue
//! resource of the **carried v4 series' ABI** ("fuse/io-uring: add
//! kernel-managed buffer rings and zero-copy", Joanne Koong, 2026-01-16,
//! transplanted onto the sqz kernel — `docker/kernel-sqz/SERIES.md`).
//! Upstream applied the kmbuf infrastructure to for-7.1 and **dropped it
//! at the author's request (2026-03-30)**: the future upstream FUSE-zc
//! will be a *different* ABI (fuse-internal buffer management). The sqz
//! transplant is therefore the only live form of this ABI anywhere, and
//! everything here is a knowing throwaway against the eventual upstream
//! shape — which is exactly why it lives behind ONE module boundary and
//! a **runtime capability probe** ([`kmbuf_surface`]): on stock kernels
//! the probe returns [`KmbufSurface::Absent`] (`EINVAL` — opcode
//! unknown) and the transport runs today's userspace-ent path
//! byte-identically, contract-pinned like every capability gate.
//!
//! # What the bufring arm buys (counted terms)
//!
//! The classical userspace-ent path pays, per 4 KiB payload page, two
//! `req->waitq.lock` round-trips + one GUP (`fuse_copy_fill`:
//! `unlock_request` → `iov_iter_get_pages2` → `lock_request`) — counted
//! at **12.8 % (locks) + 2.6 % (fill/GUP/unpin)** of ALL client cycles
//! on the kern EXA read row (`.benchmarks/2026-08-02-interface-frontier.md`
//! §3 Row A). With `FUSE_URING_BUF_RING` the payload is a kernel address
//! (`cs->is_kaddr`), `fuse_copy_fill` early-returns, and the whole
//! lock/GUP term is DELETED in **both directions** — one memcpy per
//! folio remains (the K1 byte move; that one dies only on the
//! `FUSE_URING_ZERO_COPY` arm, whose negotiation face ships here and
//! whose serve integration is the staged follow-on — see the design
//! amendment §5.4c).
//!
//! # The buffer lifecycle (kernel side, mirrored by [`KmbufQueue`])
//!
//! - The daemon registers, per queue ring: a **fixed buffer at index 0**
//!   holding every ent header back-to-back
//!   (`FUSE_URING_FIXED_HEADERS_OFFSET`; ent `i`'s header at
//!   `i × sizeof(fuse_uring_req_header)`), and a **kernel-managed buffer
//!   ring** (`IORING_REGISTER_KMBUF_RING`, bgid
//!   [`FUSE_URING_RINGBUF_GROUP`], `buf_size` = the ent payload size,
//!   pow2 `ring_entries ≥ depth`). The kernel allocates the buffers; the
//!   daemon mmaps them once at `IORING_OFF_KMBUF_RING | (bgid << 16)` —
//!   buffer `bid` lives at `region + bid × buf_size`.
//! - REGISTER SQEs carry `init.flags = FUSE_URING_BUF_RING` and
//!   `sqe->buf_index = ent_idx` (the ent's `fixed_buf_id`); **no header/
//!   payload iovecs** (the kernel rejects mismatched modes per queue).
//! - At fetch the kernel attaches a buffer to the ent when the request
//!   needs payload space (`in_numargs > 1 || out_numargs`), REUSES the
//!   attached buffer across consecutive payload-carrying requests, and
//!   recycles it when a payload-less request lands. The delivery CQE
//!   carries the buffer id (`IORING_CQE_F_BUFFER`,
//!   `flags >> IORING_CQE_BUFFER_SHIFT`) **only on fresh selection** —
//!   the daemon-side attachment law lives in
//!   [`KmbufQueue::note_delivery`]:
//!   a flagged CQE re-points the ent; an unflagged one KEEPS the current
//!   attachment (the reuse case). A kernel-side recycle without a
//!   subsequent flagged re-selection can only precede payload-LESS
//!   deliveries (header-only replies never touch the payload region), so
//!   a briefly-stale attachment is unreachable by any body write —
//!   the next payload-carrying delivery is flagged by construction.
//! - The reply body is written INTO the attached buffer; the kernel's
//!   COMMIT copies out of it with zero page locking. The §5.4
//!   lease-severance law composes unchanged: a FUSE_WRITE payload lease
//!   points into the kmbuf region (held alive by the shared
//!   [`PayloadArena`] Arc), and the commit gate already defers
//!   COMMIT_AND_FETCH — which is the only trigger for a kernel-side
//!   recycle of the ent's buffer — until the lease drops. **Deferred
//!   re-arm ≡ deferred recycle: same boundary.**
//!
//! # Env levers
//!
//! - `SQUEEZEFS_FUSE_KMBUF=0` — disable the bufring arm even where the
//!   surface probes Present (the A/B lever; the probe/gauges stay alive
//!   on both sides). Default: arm when Present.
//! - `SQUEEZEFS_FUSE_ZC=1` — recognized and LOUDLY declined: the
//!   `FUSE_URING_ZERO_COPY` negotiation face (flags, `init.queue_depth`,
//!   the sparse-table registration shape) ships here, but the zc serve
//!   integration (READ_FIXED/WRITE_FIXED serves into the registered
//!   request folios) is the staged follow-on. Never a silent no-op.

#![cfg(all(target_os = "linux", feature = "tokio-runtime"))]

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use tracing::warn;

// ---------------------------------------------------------------------
// uapi surface of the carried series (SERIES.md; io_uring + fuse halves)
// ---------------------------------------------------------------------

/// `IORING_REGISTER_KMBUF_RING` (series patch 03). The whole capability
/// probe rides this opcode: stock kernels answer `EINVAL`.
pub const IORING_REGISTER_KMBUF_RING: u32 = 37;
/// mmap offset base for kmbuf buffer regions (series patch 04).
pub const IORING_OFF_KMBUF_RING: u64 = 0x8800_0000;
/// bgid shift inside the kmbuf mmap offset (series patch 04).
pub const IORING_OFF_KMBUF_SHIFT: u64 = 16;

/// `fuse_uring_cmd_req.init.flags` bit 0 (series patch 19; FUSE minor
/// stays 45 — negotiation rides the init flags, not a version bump).
pub const FUSE_URING_BUF_RING: u16 = 1 << 0;
/// `fuse_uring_cmd_req.init.flags` bit 1 (series patch 24;
/// `CAP_SYS_ADMIN`-gated kernel-side, requires the bufring too).
pub const FUSE_URING_ZERO_COPY: u16 = 1 << 1;
/// The buffer-group id FUSE hardcodes for the payload bufring.
pub const FUSE_URING_RINGBUF_GROUP: u16 = 0;
/// Fixed-table index of the headers buffer in bufring mode. In zc mode
/// the headers index moves to `queue_depth` (the sparse request-folio
/// slots occupy 0..depth) — see [`zc_headers_index`].
pub const FUSE_URING_FIXED_HEADERS_OFFSET: u16 = 0;

/// CQE `flags` bit: a provided/kernel-managed buffer was selected.
pub const IORING_CQE_F_BUFFER: u32 = 1;
/// CQE `flags` shift carrying the selected buffer id.
pub const IORING_CQE_BUFFER_SHIFT: u32 = 16;

/// `struct io_uring_buf_reg` in the series' shape: the leading field is
/// a union `{ __u64 ring_addr; __u32 buf_size; }` — kernel-managed
/// registrations pass `buf_size` (page-aligned) instead of a user ring
/// address.
#[repr(C)]
#[derive(Clone, Copy)]
pub union BufRegAddr {
    pub ring_addr: u64,
    pub buf_size: u32,
}

/// `struct io_uring_buf_reg` (40 bytes), series layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoUringBufReg {
    pub addr: BufRegAddr,
    pub ring_entries: u32,
    pub bgid: u16,
    pub flags: u16,
    pub resv: [u64; 3],
}

impl IoUringBufReg {
    /// A kernel-managed registration: `buf_size` bytes per buffer
    /// (must be page-aligned — the kernel refuses otherwise),
    /// `ring_entries` buffers (must be a power of two), group `bgid`.
    pub fn kernel_managed(buf_size: u32, ring_entries: u32, bgid: u16) -> Self {
        Self {
            addr: BufRegAddr { buf_size },
            ring_entries,
            bgid,
            flags: 0,
            resv: [0; 3],
        }
    }
}

/// The zc-mode fixed-table index of the headers buffer: the sparse
/// request-folio slots occupy `0..queue_depth`, headers ride at the tail
/// (`fuse_uring_headers_prep`: `headers_index += queue->zero_copy_depth`).
pub fn zc_headers_index(queue_depth: u16) -> u16 {
    FUSE_URING_FIXED_HEADERS_OFFSET + queue_depth
}

/// Compose the REGISTER `init.flags` for a buffer mode. The zc flag
/// NEVER rides without the bufring flag (kernel refuses zc without a
/// kmbuf ring — `fuse_uring_buf_ring_setup`).
pub fn init_flags(bufring: bool, zero_copy: bool) -> u16 {
    let mut f = 0;
    if bufring || zero_copy {
        f |= FUSE_URING_BUF_RING;
    }
    if zero_copy {
        f |= FUSE_URING_ZERO_COPY;
    }
    f
}

// ---------------------------------------------------------------------
// Runtime capability probe
// ---------------------------------------------------------------------

/// Probe verdict for the kmbuf io_uring surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KmbufSurface {
    /// `IORING_REGISTER_KMBUF_RING` accepted — the sqz kernel.
    Present,
    /// Opcode unknown (`EINVAL`) or probe failed — stock kernels;
    /// today's userspace-ent path, byte-identical.
    Absent,
}

/// Probe once per process: register a small, fully-valid kernel-managed
/// buffer ring on a scratch io_uring (the `kmbuf_smoke.c` shape —
/// `docker/kernel-sqz/probes/`), then drop the ring (releases the
/// registration). SUCCESS ⇒ Present; `EINVAL` ⇒ Absent (opcode
/// unknown); anything else ⇒ Absent, loudly (ambiguous surfaces never
/// arm).
pub fn kmbuf_surface() -> KmbufSurface {
    static PROBE: OnceLock<KmbufSurface> = OnceLock::new();
    *PROBE.get_or_init(|| {
        let ring = match io_uring::IoUring::new(8) {
            Ok(r) => r,
            Err(e) => {
                warn!("kmbuf probe: scratch io_uring_setup failed ({e}); surface Absent");
                return KmbufSurface::Absent;
            }
        };
        let page = {
            let sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
            if sz > 0 {
                sz as u32
            } else {
                4096
            }
        };
        let reg = IoUringBufReg::kernel_managed(page, 8, 7);
        use std::os::fd::AsRawFd;
        // SAFETY: `reg` is a fully-initialized 40-byte repr(C) struct;
        // the fd is a live io_uring; the kernel copies the argument.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                ring.as_raw_fd(),
                IORING_REGISTER_KMBUF_RING,
                &reg as *const IoUringBufReg as *const libc::c_void,
                1u32,
            )
        };
        if ret == 0 {
            // Ring drop (fd close) releases the probe registration.
            return KmbufSurface::Present;
        }
        let errno = io::Error::last_os_error();
        if errno.raw_os_error() != Some(libc::EINVAL) {
            warn!("kmbuf probe: REGISTER_KMBUF_RING refused ambiguously ({errno}); surface Absent");
        }
        KmbufSurface::Absent
    })
}

/// The session-level transport buffer mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportBufferMode {
    /// Today's path: userspace ent iovecs (headers + payload arena),
    /// REGISTER with 2 iovecs, per-page kernel lock/GUP on every copy.
    UserEnts,
    /// The kmbuf arm: fixed headers buffer + kernel-managed payload
    /// bufring, REGISTER with `init.flags = FUSE_URING_BUF_RING`.
    BufRing,
}

/// Resolve the mode once per session: the capability probe gated by the
/// `SQUEEZEFS_FUSE_KMBUF` lever (`0` ⇒ UserEnts — the A/B lever; the
/// probe and gauges stay alive on both sides). `SQUEEZEFS_FUSE_ZC=1` is
/// recognized and loudly declined (negotiation face present; the zc
/// serve integration is the staged follow-on).
pub fn resolve_buffer_mode() -> TransportBufferMode {
    let lever_off = std::env::var("SQUEEZEFS_FUSE_KMBUF")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    if std::env::var("SQUEEZEFS_FUSE_ZC")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        warn!(
            "SQUEEZEFS_FUSE_ZC=1: the FUSE_URING_ZERO_COPY negotiation face is \
             present but the zc serve integration is the staged follow-on \
             (design-zero-copy-write-path §5.4c) — declining zc, continuing \
             with the bufring/user-ent resolution"
        );
    }
    match (kmbuf_surface(), lever_off) {
        (KmbufSurface::Present, false) => TransportBufferMode::BufRing,
        (KmbufSurface::Present, true) => {
            warn!("kmbuf surface Present but SQUEEZEFS_FUSE_KMBUF=0 — userspace ents (A/B lever)");
            TransportBufferMode::UserEnts
        }
        (KmbufSurface::Absent, _) => TransportBufferMode::UserEnts,
    }
}

// ---------------------------------------------------------------------
// Engagement gauges (stats inode via the daemon)
// ---------------------------------------------------------------------

static KMBUF_NEGOTIATED: AtomicU64 = AtomicU64::new(0);
static ZC_REPLIES: AtomicU64 = AtomicU64::new(0);

/// Set the session's kmbuf negotiation state (0/1) — stored at arm time
/// by `try_start` (the worker-arm wiring), a level like the geometry
/// gauges. Part of the module's arming API.
pub fn set_kmbuf_negotiated(on: bool) {
    KMBUF_NEGOTIATED.store(u64::from(on), Ordering::Relaxed);
}

/// `fuse3_kmbuf_negotiated` (stats inode, 0/1): 1 ⇒ every queue of the
/// live session REGISTERed with `FUSE_URING_BUF_RING` accepted — the
/// kmbuf arm-proof gauge for the field window.
pub fn kmbuf_negotiated() -> u64 {
    KMBUF_NEGOTIATED.load(Ordering::Relaxed)
}

/// Count one zc-flagged reply commit. Structurally unreachable in this
/// build (the zc arm is negotiation-face-only — `resolve_buffer_mode`
/// declines `SQUEEZEFS_FUSE_ZC`); the counter ships WITH the face so the
/// follow-on's engagement is measured by the same instrument that
/// guards it (the read-inplace silent-disengagement lesson).
pub fn note_zc_reply() {
    ZC_REPLIES.fetch_add(1, Ordering::Relaxed);
}

/// `fuse3_zc_replies` (stats inode): replies whose payload rode the
/// `FUSE_URING_ZERO_COPY` fixed-buffer path (K1 deleted both
/// directions). 0 by construction until the staged zc serve integration
/// arms.
pub fn zc_replies() -> u64 {
    ZC_REPLIES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------
// Per-queue kmbuf resources
// ---------------------------------------------------------------------

/// `sizeof(struct fuse_uring_req_header)` — 128 (in_out) + 128 (op_in) +
/// 32 (ring_ent_in_out) — the fixed headers buffer strides by this.
pub const REQ_HEADER_SZ: usize = 288;

/// Attachment sentinel: no buffer attached to the ent.
const NO_BUF: u64 = u64::MAX;

/// One queue's kmbuf-side resources: the headers region (registered as
/// fixed buffer index 0), the mmap'd kernel buffer region, and the
/// per-ent attachment table (the daemon half of the buffer lifecycle —
/// see the module doc's attachment law).
pub struct KmbufQueue {
    /// Headers region base (anon mmap, `depth × REQ_HEADER_SZ`,
    /// page-rounded; registered with the kernel — must stay mapped for
    /// the queue's life).
    headers_base: usize,
    headers_span: usize,
    /// Kernel buffer region base (mmap of the kmbuf registration).
    region_base: usize,
    region_span: usize,
    buf_size: usize,
    ring_entries: u32,
    /// Per-ent attached buffer id (`NO_BUF` = none). Written by the
    /// queue worker at delivery, read by `get_payload_buffer` (handler
    /// tasks) — release/acquire pairs with the inbound-queue handoff.
    attached: Vec<AtomicU64>,
}

// SAFETY: raw region pointers are plain integers here; access discipline
// is the worker/lease protocol documented on each accessor.
unsafe impl Send for KmbufQueue {}
unsafe impl Sync for KmbufQueue {}

impl KmbufQueue {
    /// Register the queue's kmbuf resources on `ring` and mmap the
    /// kernel buffer region:
    ///
    /// 1. anon headers region (`depth × REQ_HEADER_SZ`) registered as
    ///    THE fixed buffer (index [`FUSE_URING_FIXED_HEADERS_OFFSET`]);
    /// 2. `IORING_REGISTER_KMBUF_RING` with `buf_size = payload_sz`
    ///    (page-aligned by the geometry law) and pow2 entries ≥ depth;
    /// 3. mmap of the buffer region at
    ///    `IORING_OFF_KMBUF_RING | (bgid << IORING_OFF_KMBUF_SHIFT)`.
    ///
    /// Any refusal is an error — the caller fails the mount loudly (the
    /// capability gate is the PROBE; post-probe refusals are never
    /// silently downgraded).
    pub fn setup(
        ring: &io_uring::IoUring<io_uring::squeue::Entry128>,
        depth: usize,
        payload_sz: usize,
    ) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let page = {
            let sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
            if sz > 0 {
                sz as usize
            } else {
                4096
            }
        };
        if payload_sz % page != 0 {
            return Err(io::Error::other(format!(
                "kmbuf buf_size {payload_sz} not page-aligned (page {page}) — \
                 the geometry law guarantees page-multiple ents; refusing"
            )));
        }
        let headers_span = (depth * REQ_HEADER_SZ).next_multiple_of(page);
        // SAFETY: fresh anonymous RW mapping, kernel-validated length.
        let headers_base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                headers_span,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if headers_base == libc::MAP_FAILED {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "kmbuf headers region mmap failed",
            ));
        }
        let headers_base = headers_base as usize;
        let headers_iov = libc::iovec {
            iov_base: headers_base as *mut libc::c_void,
            iov_len: headers_span,
        };
        // SAFETY: the iovec references the mapping above, which this
        // struct keeps alive until drop; registered buffers are pinned by
        // the ring and released at ring teardown.
        if let Err(e) = unsafe { ring.submitter().register_buffers(&[headers_iov]) } {
            // SAFETY: unmapping the region mapped above (error path).
            unsafe { libc::munmap(headers_base as *mut libc::c_void, headers_span) };
            return Err(io::Error::other(format!(
                "kmbuf headers fixed-buffer register failed: {e}"
            )));
        }

        let ring_entries = depth.next_power_of_two().max(1) as u32;
        let reg = IoUringBufReg::kernel_managed(
            payload_sz as u32,
            ring_entries,
            FUSE_URING_RINGBUF_GROUP,
        );
        // SAFETY: fully-initialized 40-byte argument; live ring fd.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                ring.as_raw_fd(),
                IORING_REGISTER_KMBUF_RING,
                &reg as *const IoUringBufReg as *const libc::c_void,
                1u32,
            )
        };
        if ret != 0 {
            let e = io::Error::last_os_error();
            // SAFETY: error-path unmap of our own mapping.
            unsafe { libc::munmap(headers_base as *mut libc::c_void, headers_span) };
            return Err(io::Error::other(format!(
                "IORING_REGISTER_KMBUF_RING(bgid={FUSE_URING_RINGBUF_GROUP}, \
                 buf_size={payload_sz}, entries={ring_entries}) failed: {e} \
                 — surface probed Present; refusing (no silent downgrade)"
            )));
        }

        let region_span = ring_entries as usize * payload_sz;
        let mmap_off =
            IORING_OFF_KMBUF_RING | ((FUSE_URING_RINGBUF_GROUP as u64) << IORING_OFF_KMBUF_SHIFT);
        // SAFETY: mapping the kernel-owned buffer region the registration
        // above created; MAP_SHARED per the pbuf-ring convention.
        let region_base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                region_span,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                ring.as_raw_fd(),
                mmap_off as libc::off_t,
            )
        };
        if region_base == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            // SAFETY: error-path unmap of our own mapping.
            unsafe { libc::munmap(headers_base as *mut libc::c_void, headers_span) };
            return Err(io::Error::other(format!(
                "kmbuf buffer-region mmap (off {mmap_off:#x}, {region_span} B) failed: {e}"
            )));
        }

        Ok(Self {
            headers_base,
            headers_span,
            region_base: region_base as usize,
            region_span,
            buf_size: payload_sz,
            ring_entries,
            attached: (0..depth).map(|_| AtomicU64::new(NO_BUF)).collect(),
        })
    }

    /// SIM venue (tests + the 2026-08-04 microbench program): the same
    /// two regions as [`KmbufQueue::setup`] mapped ANONYMOUSLY — no
    /// io_uring, no `IORING_REGISTER_KMBUF_RING`, no kernel surface —
    /// so the attachment-law state machine ([`KmbufQueue::note_delivery`]
    /// / [`KmbufQueue::attached_ptr`]) runs over real mapped memory on
    /// stock kernels. Never a product constructor: the daemon only ever
    /// reaches a `KmbufQueue` through the probed `setup` path.
    #[doc(hidden)]
    pub fn sim_anon(depth: usize, payload_sz: usize) -> io::Result<Self> {
        let page = {
            let sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
            if sz > 0 {
                sz as usize
            } else {
                4096
            }
        };
        if payload_sz % page != 0 {
            return Err(io::Error::other(format!(
                "kmbuf sim buf_size {payload_sz} not page-aligned (page {page})"
            )));
        }
        let ring_entries = depth.next_power_of_two().max(1) as u32;
        let headers_span = (depth * REQ_HEADER_SZ).next_multiple_of(page);
        let region_span = ring_entries as usize * payload_sz;
        let map_anon = |span: usize| -> io::Result<usize> {
            // SAFETY: fresh anonymous RW mapping, kernel-validated length.
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    span,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "kmbuf sim region mmap failed",
                ));
            }
            Ok(p as usize)
        };
        let headers_base = map_anon(headers_span)?;
        let region_base = match map_anon(region_span) {
            Ok(b) => b,
            Err(e) => {
                // SAFETY: error-path unmap of the mapping created above.
                unsafe { libc::munmap(headers_base as *mut libc::c_void, headers_span) };
                return Err(e);
            }
        };
        Ok(Self {
            headers_base,
            headers_span,
            region_base,
            region_span,
            buf_size: payload_sz,
            ring_entries,
            attached: (0..depth).map(|_| AtomicU64::new(NO_BUF)).collect(),
        })
    }

    /// Registered buffer entries (pow2 ≥ depth) — the true kernel-side
    /// payload allocation this queue pins (`entries × buf_size`), for
    /// honest arena gauging.
    pub fn ring_entries(&self) -> u32 {
        self.ring_entries
    }

    /// The mmap'd buffer region's `(base, span, stride, buffer_count)` —
    /// the payload-arena view's geometry (bid-indexed).
    pub fn region_geometry(&self) -> (usize, usize, usize, usize) {
        (
            self.region_base,
            self.region_span,
            self.buf_size,
            self.ring_entries as usize,
        )
    }

    /// Ent `idx`'s header slot inside the fixed headers region.
    pub fn header_ptr(&self, idx: usize) -> *mut u8 {
        debug_assert!(idx < self.attached.len());
        (self.headers_base + idx * REQ_HEADER_SZ) as *mut u8
    }

    /// Buffer `bid`'s base inside the mmap'd kernel region.
    pub fn buf_ptr(&self, bid: u32) -> Option<*mut u8> {
        (bid < self.ring_entries)
            .then(|| (self.region_base + bid as usize * self.buf_size) as *mut u8)
    }

    /// MEM-1 dest-claim reverse lookup: the ent currently attached to
    /// `bid` (`None` = no ent holds it). Sound at claim time because the
    /// claiming request's handler has not replied yet, so its ent's
    /// attachment cannot be re-pointed (the kernel re-points/recycles
    /// only at fetch, which our COMMIT_AND_FETCH triggers — and that
    /// commit is exactly what the claimed lease parks). `NO_BUF` is
    /// `u64::MAX`, unreachable by a geometry-derived bid.
    pub fn ent_of_bid(&self, bid: u64) -> Option<usize> {
        self.attached
            .iter()
            .position(|a| a.load(Ordering::Acquire) == bid)
    }

    /// Apply the delivery-CQE attachment law for ent `idx` (see the
    /// module doc): a flagged CQE re-points the attachment; an unflagged
    /// one keeps it (buffer reuse across consecutive payload-carrying
    /// requests). Returns the ent's current payload buffer pointer, or
    /// `None` when nothing was ever attached.
    pub fn note_delivery(&self, idx: usize, cqe_flags: u32) -> Option<(*mut u8, usize)> {
        if cqe_flags & IORING_CQE_F_BUFFER != 0 {
            let bid = cqe_flags >> IORING_CQE_BUFFER_SHIFT;
            if bid >= self.ring_entries {
                return None; // out-of-range bid: caller treats as protocol breach
            }
            self.attached[idx].store(bid as u64, Ordering::Release);
        }
        self.attached_ptr(idx)
    }

    /// The ent's currently-attached payload buffer, if any (read side of
    /// the attachment table — `get_payload_buffer` serves through this).
    pub fn attached_ptr(&self, idx: usize) -> Option<(*mut u8, usize)> {
        let bid = self.attached.get(idx)?.load(Ordering::Acquire);
        if bid == NO_BUF {
            return None;
        }
        self.buf_ptr(bid as u32).map(|p| (p, self.buf_size))
    }
}

impl Drop for KmbufQueue {
    fn drop(&mut self) {
        // SAFETY: unmapping the two regions mapped in `setup`; dropped
        // once. Kernel-side pins (registered buffers / the kmbuf ring)
        // are released at ring-fd teardown independently of these vmas.
        unsafe {
            libc::munmap(self.region_base as *mut libc::c_void, self.region_span);
            libc::munmap(self.headers_base as *mut libc::c_void, self.headers_span);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The series' uapi layout: `io_uring_buf_reg` is 40 bytes with the
    /// leading `{ ring_addr | buf_size }` union — a mis-sized struct
    /// would EFAULT/EINVAL every registration on the sqz kernel.
    #[test]
    fn test_buf_reg_layout() {
        assert_eq!(std::mem::size_of::<IoUringBufReg>(), 40);
        let reg = IoUringBufReg::kernel_managed(4096, 8, 7);
        // SAFETY: reading the union member we just wrote.
        assert_eq!(unsafe { reg.addr.buf_size }, 4096);
        assert_eq!(reg.ring_entries, 8);
        assert_eq!(reg.bgid, 7);
        assert_eq!(reg.flags, 0);
        // The union's low 32 bits ARE buf_size (the kernel reads the
        // union member, so this holds by repr(C) union semantics
        // regardless of the u64 view).
    }

    /// Negotiation-face composition: zc never rides without the bufring
    /// (the kernel refuses zc without a kmbuf ring), and the headers
    /// index moves to the sparse-table tail in zc mode.
    #[test]
    fn test_init_flags_and_zc_headers_index() {
        assert_eq!(init_flags(false, false), 0);
        assert_eq!(init_flags(true, false), FUSE_URING_BUF_RING);
        assert_eq!(
            init_flags(true, true),
            FUSE_URING_BUF_RING | FUSE_URING_ZERO_COPY
        );
        assert_eq!(
            init_flags(false, true),
            FUSE_URING_BUF_RING | FUSE_URING_ZERO_COPY,
            "zc implies the bufring — never composed alone"
        );
        assert_eq!(zc_headers_index(32), 32);
        assert_eq!(zc_headers_index(4), 4);
    }

    /// The capability probe on THIS kernel: deterministic verdict,
    /// cached, and — on stock kernels — Absent via EINVAL with zero side
    /// effects (the contract that keeps today's path byte-identical
    /// everywhere the sqz kernel is not running).
    #[test]
    fn test_probe_is_deterministic_and_cached() {
        let a = kmbuf_surface();
        let b = kmbuf_surface();
        assert_eq!(a, b, "probe result must be cached per process");
        // Both verdicts are legal depending on the running kernel; what
        // must NEVER happen is a panic or an armed mode on Absent.
        if a == KmbufSurface::Absent {
            assert_eq!(
                resolve_buffer_mode(),
                TransportBufferMode::UserEnts,
                "Absent surface must resolve to today's userspace-ent path"
            );
        }
    }

    /// Region math: header slots stride by `REQ_HEADER_SZ`; buffer ids
    /// index `region + bid × buf_size`; out-of-range refused. Exercised
    /// against a real ring only where the surface probes Present (the
    /// sqz kernel); the math itself is pinned via a hand-built value.
    #[test]
    fn test_kmbuf_queue_indexing_math() {
        let q = KmbufQueue {
            headers_base: 0x10_0000,
            headers_span: 4096,
            region_base: 0x20_0000,
            region_span: 8 * 4096,
            buf_size: 4096,
            ring_entries: 8,
            attached: (0..4).map(|_| AtomicU64::new(NO_BUF)).collect(),
        };
        assert_eq!(q.header_ptr(0) as usize, 0x10_0000);
        assert_eq!(q.header_ptr(3) as usize, 0x10_0000 + 3 * REQ_HEADER_SZ);
        assert_eq!(q.buf_ptr(0).unwrap() as usize, 0x20_0000);
        assert_eq!(q.buf_ptr(7).unwrap() as usize, 0x20_0000 + 7 * 4096);
        assert!(q.buf_ptr(8).is_none(), "bid ≥ entries refused");

        // The attachment law: unflagged deliveries keep the attachment;
        // flagged ones re-point it; a fresh ent has none.
        assert!(q.attached_ptr(0).is_none(), "fresh ent: no attachment");
        assert!(
            q.note_delivery(0, 0).is_none(),
            "unflagged delivery on a fresh ent stays unattached \
             (payload-less requests)"
        );
        let (p, len) = q
            .note_delivery(0, IORING_CQE_F_BUFFER | (3 << IORING_CQE_BUFFER_SHIFT))
            .expect("flagged delivery attaches");
        assert_eq!(p as usize, 0x20_0000 + 3 * 4096);
        assert_eq!(len, 4096);
        let (p2, _) = q
            .note_delivery(0, 0)
            .expect("unflagged delivery REUSES the attachment");
        assert_eq!(p2, p);
        let (p3, _) = q
            .note_delivery(0, IORING_CQE_F_BUFFER | (5 << IORING_CQE_BUFFER_SHIFT))
            .expect("flagged delivery re-points");
        assert_eq!(p3 as usize, 0x20_0000 + 5 * 4096);
        assert!(
            q.note_delivery(0, IORING_CQE_F_BUFFER | (9 << IORING_CQE_BUFFER_SHIFT))
                .is_none(),
            "out-of-range bid is a protocol breach, never an OOB pointer"
        );
        // Forget the region before drop unmaps fake addresses.
        std::mem::forget(q);
    }

    /// Live setup contract, capability-conditional: where the surface is
    /// Present (sqz kernel) a real queue-shaped ring accepts the full
    /// registration ladder; where Absent, setup refuses with a loud
    /// error and today's path is untouched. Runs green on BOTH kernels —
    /// the branch it takes is the capability lattice itself.
    #[test]
    fn test_kmbuf_setup_follows_the_capability_lattice() {
        let ring: io_uring::IoUring<io_uring::squeue::Entry128> =
            io_uring::IoUring::builder().build(16).expect("SQE128 ring");
        let page = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } as usize;
        match kmbuf_surface() {
            KmbufSurface::Present => {
                let q = KmbufQueue::setup(&ring, 4, page).expect("Present surface must set up");
                assert_eq!(q.ring_entries(), 4);
                assert!(q.attached_ptr(0).is_none());
                // Buffers are real memory: write/read through bid 0.
                let p = q.buf_ptr(0).unwrap();
                // SAFETY: bid 0 is inside the freshly-mapped region.
                unsafe {
                    p.write(0xa5);
                    assert_eq!(p.read(), 0xa5);
                }
            }
            KmbufSurface::Absent => {
                let err = KmbufQueue::setup(&ring, 4, page)
                    .err()
                    .expect("Absent surface must refuse setup, never fake it");
                let msg = err.to_string();
                assert!(
                    msg.contains("KMBUF"),
                    "refusal must name the failing registration: {msg}"
                );
            }
        }
    }

    /// Unaligned payload sizes are refused before any registration (the
    /// kernel would EINVAL; the geometry law guarantees page multiples,
    /// so a violation here is a planner bug — fail loud, name it).
    #[test]
    fn test_kmbuf_setup_refuses_unaligned_payload() {
        let ring: io_uring::IoUring<io_uring::squeue::Entry128> =
            io_uring::IoUring::builder().build(16).expect("SQE128 ring");
        let err = KmbufQueue::setup(&ring, 4, 12345)
            .err()
            .expect("must refuse");
        assert!(err.to_string().contains("page-aligned"));
    }

    /// Gauges: negotiated is a settable level; zc replies count.
    #[test]
    fn test_gauges() {
        set_kmbuf_negotiated(true);
        assert_eq!(kmbuf_negotiated(), 1);
        set_kmbuf_negotiated(false);
        assert_eq!(kmbuf_negotiated(), 0);
        let z0 = zc_replies();
        note_zc_reply();
        assert_eq!(zc_replies(), z0 + 1);
    }
}
