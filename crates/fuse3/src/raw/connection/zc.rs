//! **FUSE_URING_ZERO_COPY serve integration** (K1 kill, 2026-08-06 —
//! the read program's lever #1; ruling D13 sanctions the custom-kernel
//! requirement).
//!
//! # The kernel contract this module rides (patches 0019–0026, v4 series)
//!
//! On a zc-armed queue (`REGISTER init.flags |= FUSE_URING_ZERO_COPY`,
//! nonzero `init.queue_depth`, `CAP_SYS_ADMIN`, bufring mandatory) the
//! kernel installs the CLIENT's request pages into the ring's sparse
//! fixed-buffer table at the ent's slot (`io_buffer_register_bvec`,
//! ddir `ITER_DEST` for reads / `ITER_SOURCE` for writes) at delivery,
//! and **skips the folio copy at COMMIT for every paged request**
//! (`can_zero_copy_req = queue->use_zero_copy && (in_pages || out_pages)`
//! — there is NO per-request daemon opt-out). Consequences the design
//! here answers:
//!
//! * **Paged reply bodies can never ride the kmbuf** — the kernel will
//!   not copy them. Every out-paged reply (READ, READDIR[PLUS],
//!   READLINK) moves its bytes into the slot through the RING:
//!   - the **direct device leg** (`READ_FIXED` device-fd → slot): the
//!     K1 kill — device DMA straight into the app's pages, zero daemon
//!     CPU passes. Eligibility lives in the root crate's routing layer
//!     (cold + passthrough + 4 KiB-aligned); the transport only executes
//!     the descriptor.
//!   - the **bounce leg** (`READ_FIXED` memfd → slot): every other
//!     shape — warm/tier serves, transform volumes, unaligned windows,
//!     readdir/readlink — lands its bytes in the per-ent [`ZcBounce`]
//!     slot (one CPU pass, the same pass those serves paid into the
//!     kmbuf before) and the queue worker bridges bounce → slot with a
//!     kernel-side shmem copy replacing the K1 folio copy: copy-count
//!     parity, correctness everywhere.
//! * **Paged WRITE payloads are not in the kmbuf** — the daemon
//!   extracts them slot → bounce (`WRITE_FIXED` slot → memfd) before
//!   dispatch, then the §5.4 payload lease rides the bounce mapping
//!   (kernel shmem copy replaces the delivery-time folio copy: parity).
//! * Non-paged traffic (names, xattrs, small headers) keeps today's
//!   kmbuf shape byte-identically — `fuse_uring_req_has_copyable_payload`
//!   still selects a kmbuf buffer for it on zc queues.
//!
//! Slot addressing: bvec-registered buffers carry `imu->ubuf = 0`
//! (patch 0021 `io_kernel_buffer_init`), so `sqe.addr` is the byte
//! OFFSET into the slot (the ublk convention) and `sqe.buf_index` is
//! the ent's `fixed_buf_id`.
//!
//! # The opcode mirror (and why mistakes are loud, not corrupting)
//!
//! The daemon must mirror the kernel's per-opcode `in_pages`/`out_pages`
//! choice ([`out_paged`]/[`in_paged`]). The failure modes compose safely:
//! an op we think paged but the kernel served copyable fails its slot
//! fetch (no bvec registered → the ring op errors) and **falls back to
//! the ent's kmbuf attachment** ([`kmbuf::note_zc_fallback`]); an op we
//! think copyable but the kernel zc'd has no kmbuf attachment, so the
//! body-carrying reply hits the existing attachment-law EIO guard —
//! loud, never silent garbage. Ops with MIXED paged+copyable out args
//! (only FUSE_IOCTL's CUSE retry protocol) are not served with data by
//! this daemon (ENOTTY/ENOSYS — header-only, safe).

#![cfg(all(target_os = "linux", feature = "tokio-runtime"))]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::raw::abi::fuse_opcode;

/// Out-direction paged opcodes on a zc queue: the kernel builds these
/// requests with `args->out_pages = true`, so their reply bodies are
/// NEVER copied from the kmbuf at COMMIT — they must ride the slot.
/// (fs/fuse: `fuse_do_readfolio`/`fuse_readahead`/`fuse_direct_io` for
/// READ, `fuse_readdir_uncached` for READDIR/READDIRPLUS,
/// `fuse_readlink_folio` for READLINK.) A mirror miss is loud — see the
/// module doc.
pub fn out_paged(opcode: u32) -> bool {
    opcode == fuse_opcode::FUSE_READ as u32
        || opcode == fuse_opcode::FUSE_READDIR as u32
        || opcode == fuse_opcode::FUSE_READDIRPLUS as u32
        || opcode == fuse_opcode::FUSE_READLINK as u32
}

/// In-direction paged opcodes on a zc queue: the kernel registers the
/// caller's SOURCE pages instead of copying them into the kmbuf, so the
/// payload announced by `payload_sz` lives in the slot and must be
/// extracted through the ring before dispatch. (`fuse_send_write` /
/// the writeback path — `args->in_pages = true`.) FUSE_IOCTL's
/// in-paged shape is deliberately NOT extracted: this daemon serves no
/// data ioctls, and its ddir can be DEST (out wins), which would refuse
/// a source-direction ring op anyway — the delivery keeps an empty
/// payload and the tripwire counts it.
pub fn in_paged(opcode: u32) -> bool {
    opcode == fuse_opcode::FUSE_WRITE as u32
}

/// One queue's zc bounce arena: a **memfd-backed** twin of the classical
/// payload arena — ent-indexed, stride-spaced — mapped `MAP_SHARED` so
/// the SAME bytes are reachable two ways:
///
/// * by VA (CPU serves write into it exactly like the classical arena:
///   dest serves, `apply_reply` body copies, WRITE payload leases), and
/// * by FD (the queue worker's ring bridges it against the sparse slot:
///   `READ_FIXED(memfd → slot)` for replies, `WRITE_FIXED(slot → memfd)`
///   for WRITE extraction).
///
/// The memfd is the whole point — an anonymous mapping has no fd for the
/// ring ops to name.
pub struct ZcBounce {
    fd: OwnedFd,
    base: usize,
    span: usize,
    stride: usize,
    depth: usize,
}

// SAFETY: the raw region pointer is a plain integer; access discipline is
// the worker/lease protocol documented on each accessor (one owner per
// ent slot between two commits, same argument as the classical arena).
unsafe impl Send for ZcBounce {}
unsafe impl Sync for ZcBounce {}

impl ZcBounce {
    /// Create the arena: one memfd of `depth × stride` bytes (stride =
    /// the ent payload size, page-rounded by the geometry law), mapped
    /// shared read/write. Any refusal is an error — the caller fails the
    /// mount loudly (zc arms only after the capability probe; post-probe
    /// refusals never silently downgrade).
    pub fn new(depth: usize, payload_sz: usize) -> io::Result<Self> {
        let page = {
            let sz = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
            if sz > 0 {
                sz as usize
            } else {
                4096
            }
        };
        let stride = payload_sz
            .checked_next_multiple_of(page)
            .ok_or_else(|| io::Error::other("zc bounce stride overflow"))?;
        let span = stride
            .checked_mul(depth.max(1))
            .ok_or_else(|| io::Error::other("zc bounce span overflow"))?;
        // SAFETY: memfd_create with a static name; the fd is fresh.
        let raw = unsafe { libc::memfd_create(c"sqz-fuse-zc-bounce".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sizing our own fresh memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), span as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh shared RW mapping over the memfd we just sized.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                span,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "zc bounce mmap failed",
            ));
        }
        Ok(Self {
            fd,
            base: base as usize,
            span,
            stride,
            depth,
        })
    }

    /// The memfd the queue worker's ring ops name (`READ_FIXED` /
    /// `WRITE_FIXED` source/destination).
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Ent `idx`'s slot base VA (CPU side).
    pub fn buf_ptr(&self, idx: usize) -> Option<*mut u8> {
        (idx < self.depth).then(|| (self.base + idx * self.stride) as *mut u8)
    }

    /// Ent `idx`'s byte offset inside the memfd (ring side — the
    /// `offset` word of the bridge ops).
    pub fn offset_of(&self, idx: usize) -> Option<u64> {
        (idx < self.depth).then(|| (idx * self.stride) as u64)
    }

    /// Per-ent slot size.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// `(base, span, stride, depth)` — the arena view geometry (the
    /// classical `PayloadArena` wrap and the MEM-1 dest window ride it).
    pub fn geometry(&self) -> (usize, usize, usize, usize) {
        (self.base, self.span, self.stride, self.depth)
    }
}

impl Drop for ZcBounce {
    fn drop(&mut self) {
        // SAFETY: unmapping the region mapped in `new`; dropped once.
        // The memfd closes with the OwnedFd.
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.span) };
    }
}

/// One ent's parked zc work at the queue worker (the third pending
/// population beside lease-parked commits and REGISTER backoffs; a slot
/// carries at most one of these at a time because an ent serves one
/// request between two commits).
pub(crate) enum ZcPend {
    /// A reply whose body sits in the bounce slot: `READ_FIXED(memfd →
    /// slot)` is in flight; the ORIGINAL commit message is kept whole so
    /// a failed bridge can fall back to the kmbuf attachment (the
    /// opcode-mirror safety net) instead of losing the reply.
    BounceFetch {
        header: Vec<u8>,
        body: bytes::Bytes,
        commit_id: u64,
        len: u32,
    },
    /// A handler-initiated device fetch (`READ_FIXED(device → slot)`,
    /// the direct leg): the handler task parks on the oneshot; the CQE
    /// result is forwarded verbatim (`res` — negative errno, or bytes
    /// read). The slot's reply commit comes LATER via the normal path.
    HandlerFetch {
        done: tokio::sync::oneshot::Sender<i32>,
    },
    /// A WRITE payload extraction (`WRITE_FIXED(slot → memfd)`): the
    /// delivered request is held back until its payload bytes exist in
    /// the bounce; on completion the inbound push happens with a §5.4
    /// lease over the bounce slot.
    WriteExtract {
        header_and_op: Vec<u8>,
        unique: u64,
        commit_id: u64,
        len: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The opcode mirror, pinned against the ABI values (a silent ABI
    /// drift here is the corruption vector the module doc walks).
    #[test]
    fn test_paged_opcode_mirror() {
        assert!(out_paged(fuse_opcode::FUSE_READ as u32));
        assert!(out_paged(fuse_opcode::FUSE_READDIR as u32));
        assert!(out_paged(fuse_opcode::FUSE_READDIRPLUS as u32));
        assert!(out_paged(fuse_opcode::FUSE_READLINK as u32));
        assert!(!out_paged(fuse_opcode::FUSE_WRITE as u32));
        assert!(!out_paged(fuse_opcode::FUSE_GETXATTR as u32));
        assert!(!out_paged(fuse_opcode::FUSE_LISTXATTR as u32));
        assert!(!out_paged(fuse_opcode::FUSE_GETATTR as u32));

        assert!(in_paged(fuse_opcode::FUSE_WRITE as u32));
        assert!(!in_paged(fuse_opcode::FUSE_READ as u32));
        assert!(!in_paged(fuse_opcode::FUSE_SETXATTR as u32));
    }

    /// The bounce arena is real dual-face memory: bytes written by VA
    /// are readable through the FD (the ring's view) and vice versa —
    /// runs on ANY kernel (memfd + mmap only, no uring surface).
    #[test]
    fn test_bounce_dual_face() {
        let b = ZcBounce::new(4, 8192).expect("bounce arena");
        assert_eq!(b.stride() % 4096, 0, "stride page-rounded");
        assert_eq!(b.offset_of(0), Some(0));
        assert_eq!(b.offset_of(3), Some(3 * b.stride() as u64));
        assert!(b.offset_of(4).is_none(), "ent ≥ depth refused");
        assert!(b.buf_ptr(4).is_none());

        // VA write → FD read (what READ_FIXED(memfd → slot) consumes).
        let p = b.buf_ptr(2).unwrap();
        // SAFETY: ent 2's slot inside the freshly-mapped span.
        unsafe {
            std::ptr::write_bytes(p, 0xa7, 16);
        }
        let mut back = [0u8; 16];
        let n = unsafe {
            libc::pread(
                b.fd(),
                back.as_mut_ptr().cast(),
                16,
                b.offset_of(2).unwrap() as libc::off_t,
            )
        };
        assert_eq!(n, 16);
        assert_eq!(back, [0xa7; 16]);

        // FD write → VA read (what a WRITE extraction produces and the
        // §5.4 lease then serves).
        let payload = [0x5c_u8; 16];
        let n = unsafe {
            libc::pwrite(
                b.fd(),
                payload.as_ptr().cast(),
                16,
                b.offset_of(1).unwrap() as libc::off_t,
            )
        };
        assert_eq!(n, 16);
        let q = b.buf_ptr(1).unwrap();
        let got = unsafe { std::slice::from_raw_parts(q, 16) };
        assert_eq!(got, &payload);
    }

    /// Zero-depth degenerates safely (span floor of one stride, no ent
    /// reachable) — a planner bug surfaces as refused indexing, never an
    /// OOB map.
    #[test]
    fn test_bounce_zero_depth() {
        let b = ZcBounce::new(0, 4096).expect("bounce arena");
        assert!(b.buf_ptr(0).is_none());
        assert!(b.offset_of(0).is_none());
    }
}
