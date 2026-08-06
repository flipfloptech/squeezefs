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
//! a **runtime capability probe** ([`kmbuf_surface`]): an opcode LADDER
//! (never a kernel-version check — portable-by-default law), because
//! the register opcode is per kernel track: 37/38 on the 6.19.14-sqz
//! field fleet, 38/39 on the 7.1-sqz track (upstream 7.1 took 37 for
//! `IORING_REGISTER_BPF_FILTER`). A rung reads Present only on the full
//! kmbuf signature (register 0 + identical-repeat `EEXIST` +
//! kmbuf-offset mmap — the discrimination proof lives on
//! `kmbuf_surface`); on stock kernels every rung refuses
//! ([`KmbufSurface::Absent`]) and the transport runs today's
//! userspace-ent path byte-identically, contract-pinned like every
//! capability gate.
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
//! - `SQUEEZEFS_FUSE_ZC=1` — arm the `FUSE_URING_ZERO_COPY` serve
//!   integration (K1 kill, 2026-08-06 — `zc.rs` carries the serve
//!   design): READ_FIXED/WRITE_FIXED against the sparse request-page
//!   slots, direct device leg for eligible cold reads, memfd bounce for
//!   everything else. Requires the kmbuf surface, the kmbuf lever, and
//!   CAP_SYS_ADMIN; every decline is loud (never a silent no-op), and a
//!   kernel that refuses the zc REGISTER degrades to BufRing loudly.

#![cfg(all(target_os = "linux", feature = "tokio-runtime"))]

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use tracing::warn;

// ---------------------------------------------------------------------
// uapi surface of the carried series (SERIES.md; io_uring + fuse halves)
// ---------------------------------------------------------------------

/// One kernel track's `IORING_(UN)REGISTER_KMBUF_RING` opcode pair.
///
/// The register opcode number is **per kernel track** (series patch 03
/// vs its 7.1 rebase — `docker/kernel-sqz/patches-7.1/`): upstream 7.1
/// allocated 37 to `IORING_REGISTER_BPF_FILTER`, so the two sqz tracks
/// occupy different numbers and the daemon resolves the pair by the
/// probe ladder ([`kmbuf_surface`]) — never a kernel-version check
/// (portable-by-default law).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KmbufOpcodes {
    /// `IORING_REGISTER_KMBUF_RING` on this track.
    pub register: u32,
    /// `IORING_UNREGISTER_KMBUF_RING` on this track (documented pair
    /// identity; teardown rides ring-fd close, so the daemon never
    /// issues it — the smoke probe exercises it).
    pub unregister: u32,
    /// Track label for the arm-time log line.
    pub track: &'static str,
}

/// The 6.19.14-sqz FIELD track (the deployed EL8 fleet): 37/38.
pub const KMBUF_OPCODES_SQZ_619: KmbufOpcodes = KmbufOpcodes {
    register: 37,
    unregister: 38,
    track: "6.19-sqz",
};
/// The 7.1-sqz track (`docker/kernel-sqz/patches-7.1/`): 38/39 —
/// upstream 7.1 took 37 for `IORING_REGISTER_BPF_FILTER`.
pub const KMBUF_OPCODES_SQZ_71: KmbufOpcodes = KmbufOpcodes {
    register: 38,
    unregister: 39,
    track: "7.1-sqz",
};
/// Probe order: FIELD track first — the deployed fleet resolves on
/// rung 1, and on 7.1 kernels rung 1's foreign occupant (BPF_FILTER)
/// refuses deterministically before any state change (see
/// [`kmbuf_surface`]'s discrimination proof).
pub const KMBUF_OPCODE_LADDER: [KmbufOpcodes; 2] = [KMBUF_OPCODES_SQZ_619, KMBUF_OPCODES_SQZ_71];

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
// Runtime capability probe — the opcode LADDER
// ---------------------------------------------------------------------

/// Probe verdict for the kmbuf io_uring surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KmbufSurface {
    /// A ladder rung confirmed — the sqz kernel, with THIS track's
    /// opcode pair (6.19-sqz = 37/38, 7.1-sqz = 38/39).
    Present(KmbufOpcodes),
    /// Every rung refused (`EINVAL`/foreign) or the probe failed —
    /// stock kernels; today's userspace-ent path, byte-identical.
    Absent,
}

/// One rung's verdict: probing ONE candidate register opcode with
/// fully-valid kmbuf-register args on a fresh scratch ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RungOutcome {
    /// The full kmbuf signature: register returned 0, the identical
    /// repeat answered `EEXIST` (a live bufring at the bgid), and the
    /// kmbuf-offset mmap produced the buffer region. Only the kmbuf
    /// machinery can satisfy the conjunction.
    Confirmed,
    /// register returned 0 but a confirmation failed — a FOREIGN opcode
    /// accepted our argument shape. Loud; never arms; the scratch ring
    /// is dropped so whatever it registered dies with the fd.
    ForeignSuccess,
    /// register refused with this errno. `EINVAL` = opcode unknown
    /// (stock dispatch) or a foreign occupant's validation (7.1's
    /// BPF_FILTER); `ENOENT` = a foreign lookup shape (kmbuf-UNREGISTER
    /// crossed: resv/flags pass, bgid lookup fails). Both quiet;
    /// anything else is loud. All read NOT-kmbuf at this number.
    Refused(i32),
}

/// Probe-argument geometry (shared by the rung probe and its pinned
/// tests): a tiny valid registration — 8 × page at bgid 7 — embedded in
/// a zeroed [`PROBE_ARG_SPAN`]-byte buffer so a foreign opcode reading a
/// WIDER struct (7.1's `io_uring_bpf` reads 72 bytes) sees deterministic
/// zeros, never stack garbage, never a page-boundary EFAULT.
const PROBE_RING_ENTRIES: u32 = 8;
const PROBE_BGID: u16 = 7;
const PROBE_ARG_SPAN: usize = 256;

/// The pure ladder decision table (unit-tested with injected outcomes):
/// first Confirmed rung wins and short-circuits; every other outcome
/// falls through; all rungs exhausted ⇒ Absent.
fn resolve_ladder(mut probe: impl FnMut(KmbufOpcodes) -> RungOutcome) -> KmbufSurface {
    for pair in KMBUF_OPCODE_LADDER {
        match probe(pair) {
            RungOutcome::Confirmed => return KmbufSurface::Present(pair),
            RungOutcome::ForeignSuccess => {
                warn!(
                    "kmbuf probe: opcode {} returned 0 WITHOUT the kmbuf \
                     signature (EEXIST-on-repeat + kmbuf-offset mmap) — a \
                     foreign opcode accepted the argument shape; rung {} \
                     reads NOT-kmbuf",
                    pair.register, pair.track
                );
            }
            RungOutcome::Refused(e) if e == libc::EINVAL || e == libc::ENOENT => {}
            RungOutcome::Refused(e) => {
                warn!(
                    "kmbuf probe: opcode {} refused ambiguously (errno {e}); \
                     rung {} reads NOT-kmbuf",
                    pair.register, pair.track
                );
            }
        }
    }
    KmbufSurface::Absent
}

/// Probe one rung live: a fresh scratch ring (a foreign opcode's
/// hypothetical side effects die with the fd), the padded valid
/// registration, then — only on a 0 return — the two confirmations.
fn probe_rung_live(pair: KmbufOpcodes) -> RungOutcome {
    use std::os::fd::AsRawFd;
    let ring = match io_uring::IoUring::new(8) {
        Ok(r) => r,
        Err(e) => {
            warn!("kmbuf probe: scratch io_uring_setup failed ({e})");
            return RungOutcome::Refused(e.raw_os_error().unwrap_or(libc::EINVAL));
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
    let reg = IoUringBufReg::kernel_managed(page, PROBE_RING_ENTRIES, PROBE_BGID);
    let mut arg = [0u8; PROBE_ARG_SPAN];
    // SAFETY: IoUringBufReg is 40 bytes of plain-old-data (repr(C),
    // fully initialized); the destination is in-bounds.
    unsafe {
        std::ptr::copy_nonoverlapping(
            &reg as *const IoUringBufReg as *const u8,
            arg.as_mut_ptr(),
            std::mem::size_of::<IoUringBufReg>(),
        );
    }
    let register = |fd: i32| -> Result<(), i32> {
        // SAFETY: live ring fd; the argument buffer outlives the call
        // (the kernel copies it).
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                fd,
                pair.register,
                arg.as_ptr() as *const libc::c_void,
                1u32,
            )
        };
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EINVAL))
        }
    };
    let fd = ring.as_raw_fd();
    if let Err(errno) = register(fd) {
        return RungOutcome::Refused(errno);
    }
    // Confirmation 1: the identical repeat must answer EEXIST — kmbuf's
    // io_alloc_new_buffer_list refuses a bgid with a live bufring.
    match register(fd) {
        Err(errno) if errno == libc::EEXIST => {}
        other => {
            warn!(
                "kmbuf probe: opcode {} repeat answered {other:?}, not EEXIST — \
                 foreign success on rung {}",
                pair.register, pair.track
            );
            return RungOutcome::ForeignSuccess;
        }
    }
    // Confirmation 2: only the kmbuf registration mints an mmap region
    // at IORING_OFF_KMBUF_RING | (bgid << shift); stock kernels have no
    // case for that offset.
    let span = PROBE_RING_ENTRIES as usize * page as usize;
    let off = IORING_OFF_KMBUF_RING | ((PROBE_BGID as u64) << IORING_OFF_KMBUF_SHIFT);
    // SAFETY: mapping the kernel-owned probe buffer region; unmapped
    // below; the ring fd is live.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            span,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            off as libc::off_t,
        )
    };
    if p == libc::MAP_FAILED {
        let e = io::Error::last_os_error();
        warn!(
            "kmbuf probe: opcode {} registered but the kmbuf-offset mmap \
             ({off:#x}) failed ({e}) — foreign success on rung {}",
            pair.register, pair.track
        );
        return RungOutcome::ForeignSuccess;
    }
    // SAFETY: unmapping the region mapped above.
    unsafe { libc::munmap(p, span) };
    // Ring drop (fd close) releases the probe registration.
    RungOutcome::Confirmed
}

/// Resolve the kmbuf surface once per process by the opcode LADDER —
/// field track (37/38) first, then the 7.1 track (38/39). Never a
/// kernel-version check (portable-by-default law).
///
/// # Why the ladder cannot mis-identify
///
/// **False Present is unreachable.** A rung arms only on the conjunction
/// register==0 ∧ identical-repeat==EEXIST ∧ kmbuf-offset-mmap-succeeds,
/// which only the kmbuf machinery satisfies. The characterized foreign
/// occupants can't even reach the confirmations:
/// - 7.1's `IORING_REGISTER_BPF_FILTER` (=37) imports the argument's
///   first u16 as `cmd_type` and requires `IO_URING_BPF_CMD_FILTER` (=1)
///   before touching state — a page-aligned `buf_size` has low 12 bits
///   zero, so it refuses EINVAL deterministically (pinned as arithmetic
///   in the tests); with `CONFIG_IO_URING_BPF=n` the stub returns EINVAL.
/// - kmbuf-UNREGISTER crossed with register args (probing 38 on
///   6.19-sqz — unreachable via ladder order, characterized anyway):
///   resv/flags validate, the bgid lookup on the fresh scratch ring
///   fails → ENOENT, before any state change.
/// - Stock dispatch refuses unknown opcodes EINVAL at the
///   `IORING_REGISTER_LAST` gate.
///
/// **False Absent is unreachable on the two sqz tracks**: the rung runs
/// the exact registration [`KmbufQueue::setup`] performs (valid args,
/// page-aligned `buf_size`, pow2 entries), so the track's own kernel
/// accepts it by construction; on 7.1-sqz rung 1's EINVAL falls through
/// to the confirming rung 2.
///
/// **Side-effect-free everywhere**: each rung uses a fresh scratch ring
/// dropped before the verdict returns (releasing every registration,
/// even a hypothetical foreign one), and every characterized foreign
/// refusal happens before any kernel state change.
pub fn kmbuf_surface() -> KmbufSurface {
    static PROBE: OnceLock<KmbufSurface> = OnceLock::new();
    *PROBE.get_or_init(|| resolve_ladder(probe_rung_live))
}

/// The resolved opcode pair, for the arm-time log line: e.g.
/// "37/38 (6.19-sqz)"; "absent" on stock kernels.
pub fn resolved_opcodes_label() -> String {
    match kmbuf_surface() {
        KmbufSurface::Present(p) => format!("{}/{} ({})", p.register, p.unregister, p.track),
        KmbufSurface::Absent => "absent".to_string(),
    }
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
    /// The zc arm (K1 kill, 2026-08-06): BufRing PLUS the sparse
    /// request-page slot table (`0..depth`, headers at index `depth`),
    /// `init.flags = FUSE_URING_BUF_RING | FUSE_URING_ZERO_COPY` with
    /// `init.queue_depth = depth`, and the per-queue memfd bounce arena
    /// — see `zc.rs` for the serve integration.
    ZeroCopy,
}

impl TransportBufferMode {
    /// True for every mode that registers a kmbuf ring (the zc arm is
    /// bufring-plus — the kernel refuses zc without a kmbuf ring).
    pub fn uses_kmbuf(self) -> bool {
        matches!(self, Self::BufRing | Self::ZeroCopy)
    }
}

/// One boolean knob read under the shared ENG-10 convention: any of
/// `1/true/yes/on` / `0/false/no/off` (case-insensitive), absent or
/// malformed keeps `default` — a transport lever must never abort a mount
/// over a typo, and the daemon's startup gate has already refused one.
fn env_bool(key: &str, default: bool) -> bool {
    crate::env_knob_core::parse_bool(key, std::env::var(key).ok().as_deref())
        .ok()
        .flatten()
        .unwrap_or(default)
}

/// Resolve the mode once per session: the capability probe gated by the
/// `SQUEEZEFS_FUSE_KMBUF` lever (`0` ⇒ UserEnts — the A/B lever; the
/// probe and gauges stay alive on both sides). `SQUEEZEFS_FUSE_ZC=1`
/// arms the FUSE_URING_ZERO_COPY serve integration (K1 kill, 2026-08-06)
/// where the surface admits it: kmbuf Present (zc is bufring-plus — the
/// kernel refuses zc without a kmbuf ring), the kmbuf lever on, and
/// euid 0 (the kernel gate is `capable(CAP_SYS_ADMIN)`; probing it here
/// keeps the refusal loud AT RESOLUTION instead of a per-queue REGISTER
/// EINVAL). Every decline is loud — never a silent no-op — and the
/// worker's own REGISTER failure path degrades zc → BufRing loudly too
/// (a stock kernel that probes kmbuf-Present but lacks the zc flag).
pub fn resolve_buffer_mode() -> TransportBufferMode {
    // ENG-10: one boolean convention — `0/false/no/off` all disable, and a
    // malformed value keeps the documented default (announced by the
    // daemon's startup gate, which refuses it outright).
    let lever_off = !env_bool("SQUEEZEFS_FUSE_KMBUF", true);
    let zc_wanted = env_bool("SQUEEZEFS_FUSE_ZC", false);
    match (kmbuf_surface(), lever_off) {
        (KmbufSurface::Present(_), false) => {
            if zc_wanted {
                // SAFETY: geteuid has no failure mode.
                let euid = unsafe { libc::geteuid() };
                if euid == 0 {
                    return TransportBufferMode::ZeroCopy;
                }
                warn!(
                    "SQUEEZEFS_FUSE_ZC=1 but euid={euid}: the kernel's zc REGISTER \
                     requires CAP_SYS_ADMIN — declining zc, continuing on the \
                     bufring path (fuse3_zc_replies stays 0)"
                );
            }
            TransportBufferMode::BufRing
        }
        (KmbufSurface::Present(_), true) => {
            warn!("kmbuf surface Present but SQUEEZEFS_FUSE_KMBUF=0 — userspace ents (A/B lever)");
            if zc_wanted {
                warn!(
                    "SQUEEZEFS_FUSE_ZC=1 declined: zc is bufring-plus and the kmbuf \
                     lever is off (fuse3_zc_replies stays 0)"
                );
            }
            TransportBufferMode::UserEnts
        }
        (KmbufSurface::Absent, _) => {
            if zc_wanted {
                warn!(
                    "SQUEEZEFS_FUSE_ZC=1 declined: the kmbuf/zc io_uring surface is \
                     Absent on this kernel — continuing on the userspace-ent path \
                     (fuse3_zc_replies stays 0; the sqz kernel series carries the \
                     surface)"
                );
            }
            TransportBufferMode::UserEnts
        }
    }
}

// ---------------------------------------------------------------------
// Engagement gauges (stats inode via the daemon)
// ---------------------------------------------------------------------

static KMBUF_NEGOTIATED: AtomicU64 = AtomicU64::new(0);
static ZC_NEGOTIATED: AtomicU64 = AtomicU64::new(0);
static ZC_REPLIES: AtomicU64 = AtomicU64::new(0);
static ZC_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static ZC_SLOT_PAYLOAD_SKIPS: AtomicU64 = AtomicU64::new(0);
static ZC_WRITE_EXTRACTIONS: AtomicU64 = AtomicU64::new(0);
static ZC_WRITE_EXTRACT_BYTES: AtomicU64 = AtomicU64::new(0);

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

/// Set the session's zc negotiation state (0/1) — stored at arm time
/// like [`set_kmbuf_negotiated`]; the worker downgrades it (with a loud
/// line) if the kernel refuses the zc REGISTER on a kmbuf-Present
/// surface.
pub fn set_zc_negotiated(on: bool) {
    ZC_NEGOTIATED.store(u64::from(on), Ordering::Relaxed);
}

/// `fuse3_zc_negotiated` (stats inode, 0/1): 1 ⇒ every queue of the
/// live session REGISTERed with `FUSE_URING_ZERO_COPY` accepted — the
/// zc arm-proof gauge for the field window.
pub fn zc_negotiated() -> u64 {
    ZC_NEGOTIATED.load(Ordering::Relaxed)
}

/// Count one zc reply commit: a paged reply whose payload rode the
/// sparse-slot path (device-direct prefill or bounce bridge) — K1's
/// folio copy skipped by the kernel at COMMIT.
pub fn note_zc_reply() {
    ZC_REPLIES.fetch_add(1, Ordering::Relaxed);
}

/// `fuse3_zc_replies` (stats inode): replies whose payload rode the
/// `FUSE_URING_ZERO_COPY` fixed-buffer path (K1 deleted both
/// directions). 0 by construction until a session arms zc.
pub fn zc_replies() -> u64 {
    ZC_REPLIES.load(Ordering::Relaxed)
}

/// Count one zc bridge FAILURE that degraded to the kmbuf attachment or
/// a header-only EIO (the opcode-mirror safety net — see zc.rs module
/// doc). Steady growth means the mirror disagrees with the running
/// kernel for some opcode: stop and read the log lines naming it.
pub fn note_zc_fallback() {
    ZC_FALLBACKS.fetch_add(1, Ordering::Relaxed);
}

/// `fuse3_zc_fallbacks` (stats inode): must stay ≈ 0 on a healthy zc
/// session.
pub fn zc_fallbacks() -> u64 {
    ZC_FALLBACKS.load(Ordering::Relaxed)
}

/// Count one payload-announcing delivery on a zc queue whose payload was
/// neither kmbuf-covered nor an extractable WRITE (the FUSE_IOCTL-class
/// shape): delivered with an EMPTY payload, loudly. Nonzero names an
/// opcode the in-direction mirror must learn.
pub fn note_zc_slot_payload_skip() {
    ZC_SLOT_PAYLOAD_SKIPS.fetch_add(1, Ordering::Relaxed);
}

/// `fuse3_zc_slot_payload_skips` (stats inode): must stay 0 outside
/// data-carrying ioctls (which this daemon does not serve).
pub fn zc_slot_payload_skips() -> u64 {
    ZC_SLOT_PAYLOAD_SKIPS.load(Ordering::Relaxed)
}

/// Count one COMPLETED zc WRITE extraction (`WRITE_FIXED` slot → memfd
/// succeeded and the delivery was dispatched with its §5.4 lease over
/// the bounce slot) plus its payload bytes. Failed extractions ride
/// [`note_zc_fallback`], never this pair — the pair is the armed-mount
/// WRITE engagement face (write-bracket campaign, 2026-08-06): a write
/// row whose op count these deltas do not account is INVALID.
pub fn note_zc_write_extraction(bytes: u64) {
    ZC_WRITE_EXTRACTIONS.fetch_add(1, Ordering::Relaxed);
    ZC_WRITE_EXTRACT_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// `fuse3_zc_write_extractions` (stats inode): FUSE_WRITE payloads that
/// arrived via the slot→memfd extraction on a zc-armed queue. 0 by
/// construction until a session arms zc.
pub fn zc_write_extractions() -> u64 {
    ZC_WRITE_EXTRACTIONS.load(Ordering::Relaxed)
}

/// `fuse3_zc_write_extract_bytes` (stats inode): the byte face of
/// [`zc_write_extractions`] — closure vs a write row's user bytes is the
/// row-validity instrument.
pub fn zc_write_extract_bytes() -> u64 {
    ZC_WRITE_EXTRACT_BYTES.load(Ordering::Relaxed)
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
    ///    THE fixed buffer — table index [`FUSE_URING_FIXED_HEADERS_OFFSET`]
    ///    in bufring mode, or [`zc_headers_index`]`(depth)` on the zc arm,
    ///    where the table is `depth + 1` entries with `0..depth` left
    ///    SPARSE (zeroed iovecs — the kernel installs the client's
    ///    request pages there per request via `io_buffer_register_bvec`);
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
        zero_copy: bool,
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
        // The geometry law is kernel-independent (a violation is a
        // planner bug) — refuse it before anything else.
        if payload_sz % page != 0 {
            return Err(io::Error::other(format!(
                "kmbuf buf_size {payload_sz} not page-aligned (page {page}) — \
                 the geometry law guarantees page-multiple ents; refusing"
            )));
        }
        // The register opcode is per kernel track — the ladder's cached
        // verdict carries the resolved pair (never a blind number).
        let ops = match kmbuf_surface() {
            KmbufSurface::Present(ops) => ops,
            KmbufSurface::Absent => {
                return Err(io::Error::other(
                    "IORING_REGISTER_KMBUF_RING unavailable: the kmbuf opcode \
                     ladder probed Absent on this kernel — refusing setup \
                     (stock kernels run the userspace-ent path)",
                ));
            }
        };
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
        // Table shape: bufring = [headers] at index 0; zc = depth SPARSE
        // entries (zeroed iovecs — legal since 5.19's rsrc rework, they
        // stay empty until the kernel's per-request bvec install) with
        // headers at index `depth` (= `zc_headers_index(depth)` — the
        // kernel reads headers at `FUSE_URING_FIXED_HEADERS_OFFSET +
        // zero_copy_depth`).
        let table: Vec<libc::iovec> = if zero_copy {
            let mut t: Vec<libc::iovec> = (0..depth)
                .map(|_| libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                })
                .collect();
            t.push(headers_iov);
            t
        } else {
            vec![headers_iov]
        };
        // SAFETY: the headers iovec references the mapping above, which
        // this struct keeps alive until drop; sparse entries are null by
        // construction; registered buffers are pinned by the ring and
        // released at ring teardown.
        if let Err(e) = unsafe { ring.submitter().register_buffers(&table) } {
            // SAFETY: unmapping the region mapped above (error path).
            unsafe { libc::munmap(headers_base as *mut libc::c_void, headers_span) };
            return Err(io::Error::other(format!(
                "kmbuf headers fixed-buffer register failed (zc={zero_copy}): {e}"
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
                ops.register,
                &reg as *const IoUringBufReg as *const libc::c_void,
                1u32,
            )
        };
        if ret != 0 {
            let e = io::Error::last_os_error();
            // SAFETY: error-path unmap of our own mapping.
            unsafe { libc::munmap(headers_base as *mut libc::c_void, headers_span) };
            return Err(io::Error::other(format!(
                "IORING_REGISTER_KMBUF_RING(op={}, bgid={FUSE_URING_RINGBUF_GROUP}, \
                 buf_size={payload_sz}, entries={ring_entries}) failed: {e} \
                 — surface probed Present ({}); refusing (no silent downgrade)",
                ops.register, ops.track
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
            KmbufSurface::Present(_) => {
                let q =
                    KmbufQueue::setup(&ring, 4, page, false).expect("Present surface must set up");
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
                let err = KmbufQueue::setup(&ring, 4, page, false)
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

    /// The zc table shape (K1 kill): `depth` SPARSE entries + the headers
    /// buffer at index `depth`. The buffer-table registration itself is a
    /// stock io_uring surface (5.19+ sparse entries), so it must succeed
    /// on ANY modern kernel — what gates zc is the kmbuf ring + the
    /// REGISTER flag, probed separately. Runs the registration ladder
    /// wherever the kmbuf surface probes Present; on Absent kernels the
    /// sparse-table half is still exercised via a bare ring.
    #[test]
    fn test_zc_setup_table_shape() {
        let ring: io_uring::IoUring<io_uring::squeue::Entry128> =
            io_uring::IoUring::builder().build(16).expect("SQE128 ring");
        let page = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } as usize;
        match kmbuf_surface() {
            KmbufSurface::Present(_) => {
                let q =
                    KmbufQueue::setup(&ring, 4, page, true).expect("Present surface must set up");
                assert_eq!(q.ring_entries(), 4);
                // Header slots still stride the anon region — index math
                // is table-shape independent.
                assert_eq!(
                    q.header_ptr(1) as usize - q.header_ptr(0) as usize,
                    REQ_HEADER_SZ
                );
            }
            KmbufSurface::Absent => {
                // The sparse table registers on stock kernels; the kmbuf
                // ring refusal is what fails setup. Prove the sparse half
                // directly so the shape is pinned everywhere.
                let mut table: Vec<libc::iovec> = (0..4)
                    .map(|_| libc::iovec {
                        iov_base: std::ptr::null_mut(),
                        iov_len: 0,
                    })
                    .collect();
                let span = 4096usize;
                // SAFETY: fresh anon RW mapping for the headers entry.
                let base = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        span,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                assert_ne!(base, libc::MAP_FAILED);
                table.push(libc::iovec {
                    iov_base: base,
                    iov_len: span,
                });
                // SAFETY: iovecs valid for the registration's life (ring
                // dropped in this scope); sparse entries are null.
                unsafe { ring.submitter().register_buffers(&table) }
                    .expect("sparse buffer table (depth zeroed + headers tail) is stock 5.19+");
                drop(ring);
                // SAFETY: unmapping the region mapped above.
                unsafe { libc::munmap(base, span) };
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
        let err = KmbufQueue::setup(&ring, 4, 12345, false)
            .err()
            .expect("must refuse");
        assert!(err.to_string().contains("page-aligned"));
    }

    /// Gauges: negotiated is a settable level (both arms); zc replies /
    /// fallbacks / slot-payload skips count.
    #[test]
    fn test_gauges() {
        set_kmbuf_negotiated(true);
        assert_eq!(kmbuf_negotiated(), 1);
        set_kmbuf_negotiated(false);
        assert_eq!(kmbuf_negotiated(), 0);
        set_zc_negotiated(true);
        assert_eq!(zc_negotiated(), 1);
        set_zc_negotiated(false);
        assert_eq!(zc_negotiated(), 0);
        let z0 = zc_replies();
        note_zc_reply();
        assert_eq!(zc_replies(), z0 + 1);
        let f0 = zc_fallbacks();
        note_zc_fallback();
        assert_eq!(zc_fallbacks(), f0 + 1);
        let s0 = zc_slot_payload_skips();
        note_zc_slot_payload_skip();
        assert_eq!(zc_slot_payload_skips(), s0 + 1);
        // WRITE-extraction engagement pair (write-bracket campaign,
        // 2026-08-06): the slot→memfd extraction is the armed-mount WRITE
        // vehicle — a write row without its engagement face is INVALID.
        let w0 = zc_write_extractions();
        let wb0 = zc_write_extract_bytes();
        note_zc_write_extraction(4096);
        assert_eq!(zc_write_extractions(), w0 + 1);
        assert_eq!(zc_write_extract_bytes(), wb0 + 4096);
        note_zc_write_extraction(0);
        assert_eq!(
            zc_write_extractions(),
            w0 + 2,
            "zero-length still counts the op"
        );
        assert_eq!(zc_write_extract_bytes(), wb0 + 4096);
    }

    /// `uses_kmbuf` composition — the zc arm is bufring-plus.
    #[test]
    fn test_mode_uses_kmbuf() {
        assert!(!TransportBufferMode::UserEnts.uses_kmbuf());
        assert!(TransportBufferMode::BufRing.uses_kmbuf());
        assert!(TransportBufferMode::ZeroCopy.uses_kmbuf());
    }

    /// The two kernel tracks' opcode pairs are PINNED (they are deployed
    /// ABI): 6.19.14-sqz field fleet = 37/38; the patches-7.1 track =
    /// 38/39 (upstream 7.1 took 37 for IORING_REGISTER_BPF_FILTER).
    /// Field-first probe order: the deployed fleet resolves on rung 1.
    #[test]
    fn test_ladder_pins_the_two_tracks() {
        assert_eq!(KMBUF_OPCODES_SQZ_619.register, 37);
        assert_eq!(KMBUF_OPCODES_SQZ_619.unregister, 38);
        assert_eq!(KMBUF_OPCODES_SQZ_71.register, 38);
        assert_eq!(KMBUF_OPCODES_SQZ_71.unregister, 39);
        assert_eq!(KMBUF_OPCODE_LADDER[0], KMBUF_OPCODES_SQZ_619);
        assert_eq!(KMBUF_OPCODE_LADDER[1], KMBUF_OPCODES_SQZ_71);
    }

    /// The ladder decision table over injected rung outcomes — every
    /// kernel class plus the ambiguous shapes, with no syscalls:
    ///
    /// | kernel                | rung 37 (reg-args)        | rung 38 (reg-args) | verdict        |
    /// |-----------------------|---------------------------|--------------------|----------------|
    /// | 6.19.14-sqz           | Confirmed (0+EEXIST+mmap) | never probed       | Present(37/38) |
    /// | 7.1.6-sqz             | EINVAL (BPF import)       | Confirmed          | Present(38/39) |
    /// | stock (any)           | EINVAL                    | EINVAL             | Absent         |
    /// | foreign lookup shape  | ENOENT                    | EINVAL             | Absent         |
    /// | foreign 0-return      | ForeignSuccess (loud)     | per rung           | never Present via the foreign rung |
    #[test]
    fn test_opcode_ladder_decision_table() {
        use RungOutcome::*;
        fn run(script: &[RungOutcome]) -> (KmbufSurface, usize) {
            let mut i = 0;
            let s = resolve_ladder(|_pair| {
                let o = script[i];
                i += 1;
                o
            });
            (s, i)
        }
        // 6.19.14-sqz: rung 1 confirms; rung 2 is never probed — the
        // short-circuit is part of the contract (probing 38 on 6.19-sqz
        // would hit kmbuf-UNREGISTER, and while that is characterized
        // side-effect-free, the ladder must not rely on it).
        let (s, n) = run(&[Confirmed]);
        assert_eq!(s, KmbufSurface::Present(KMBUF_OPCODES_SQZ_619));
        assert_eq!(n, 1, "rung 1 Confirmed must short-circuit");
        // 7.1.6-sqz: rung 1 hits IORING_REGISTER_BPF_FILTER, whose import
        // reads the arg's first u16 as cmd_type (≠ 1 for any page-aligned
        // buf_size) → EINVAL before any state change; rung 2 confirms.
        let (s, n) = run(&[Refused(libc::EINVAL), Confirmed]);
        assert_eq!(s, KmbufSurface::Present(KMBUF_OPCODES_SQZ_71));
        assert_eq!(n, 2);
        // Stock kernels: opcode unknown everywhere.
        let (s, n) = run(&[Refused(libc::EINVAL), Refused(libc::EINVAL)]);
        assert_eq!(s, KmbufSurface::Absent);
        assert_eq!(n, 2, "Absent only after every rung refused");
        // Register-shaped args hitting a foreign LOOKUP opcode (the
        // kmbuf-UNREGISTER cross: resv/flags pass, bgid lookup on the
        // scratch ring fails) → ENOENT reads NOT-kmbuf, quietly.
        let (s, _) = run(&[Refused(libc::ENOENT), Refused(libc::EINVAL)]);
        assert_eq!(s, KmbufSurface::Absent);
        // A foreign opcode returning 0 WITHOUT the kmbuf signature
        // (identical-repeat EEXIST + kmbuf-offset mmap) can never arm
        // that rung — the conjunction is the false-Present proof.
        let (s, n) = run(&[ForeignSuccess, Confirmed]);
        assert_eq!(s, KmbufSurface::Present(KMBUF_OPCODES_SQZ_71));
        assert_eq!(n, 2, "a foreign success falls through to the next rung");
        let (s, _) = run(&[ForeignSuccess, ForeignSuccess]);
        assert_eq!(s, KmbufSurface::Absent);
        // Unexpected errnos are loud but never block later rungs, and
        // never read as Present themselves.
        let (s, _) = run(&[Refused(libc::EACCES), Confirmed]);
        assert_eq!(s, KmbufSurface::Present(KMBUF_OPCODES_SQZ_71));
        let (s, _) = run(&[Refused(libc::ENOMEM), Refused(libc::EPERM)]);
        assert_eq!(s, KmbufSurface::Absent);
    }

    /// The BPF_FILTER discriminator, pinned as arithmetic: 7.1 kernels
    /// (sqz AND stock CachyOS) dispatch opcode 37 to
    /// IORING_REGISTER_BPF_FILTER, whose import reads the argument's
    /// first u16 as `cmd_type` and requires IO_URING_BPF_CMD_FILTER (=1)
    /// before touching any state. Our register probe's first field is a
    /// page-aligned `buf_size` — low 12 bits zero — so the import refuses
    /// EINVAL deterministically: a false Present via BPF_FILTER is
    /// arithmetically unreachable for every plausible page size.
    #[test]
    fn test_probe_arg_first_u16_never_reads_as_bpf_cmd_filter() {
        for page in [4096u32, 8192, 16384, 65536] {
            let reg = IoUringBufReg::kernel_managed(page, PROBE_RING_ENTRIES, PROBE_BGID);
            // SAFETY: reading the union member we just wrote.
            let first_u16 = (unsafe { reg.addr.buf_size } & 0xffff) as u16;
            assert_ne!(
                first_u16, 1,
                "page-aligned buf_size {page} must never read as \
                 IO_URING_BPF_CMD_FILTER"
            );
        }
    }

    /// The live probe argument rides in a zero-padded buffer wider than
    /// any foreign opcode's struct (7.1's io_uring_bpf reads 72 bytes):
    /// bytes past our 40-byte reg read as deterministic zeros — never
    /// stack garbage, never a page-boundary EFAULT.
    #[test]
    fn test_probe_arg_padding_covers_foreign_readers() {
        assert_eq!(std::mem::size_of::<IoUringBufReg>(), 40);
        // Compile-time tripwire: shrinking the span below any foreign
        // reader's struct width (io_uring_bpf = 72 B) fails the build.
        const {
            assert!(PROBE_ARG_SPAN >= 128);
        }
    }
}
