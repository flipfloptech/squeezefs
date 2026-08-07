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
//! * **Paged WRITE payloads are not in the kmbuf** — they stay HELD in
//!   the sparse slot (D14 write-side leg, 2026-08-06:
//!   dispatch-before-extraction). The request dispatches immediately
//!   with an empty placeholder + the held length published on the
//!   pool's [`ZcHeldTable`]; the handler consumes the payload through
//!   the slot source: the **direct leg** (`WRITE_FIXED(device fd ←
//!   slot)` — the W1 sole-owner patch class DMAs the caller's
//!   registered pages straight to the device, zero daemon copies) or
//!   the **lazy extraction** (`WRITE_FIXED(slot → memfd)` on demand —
//!   every other shape; the §5.4 payload lease rides the bounce
//!   mapping, kernel shmem copy replacing the delivery-time folio
//!   copy: parity). Slot lifetime: pages unregister at COMMIT, and
//!   every slot-sourcing DMA is handler-awaited strictly before the
//!   reply exists, so the ordering holds by construction.
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
use crate::raw::connection::kmbuf::KmbufTrack;

/// Out-direction paged opcodes on a zc queue, **keyed by kernel track**
/// (out-paged-mirror divergence fix, 2026-08-06): the kernel builds
/// these requests with `args->out_pages = true`, so their reply bodies
/// are NEVER copied from the kmbuf at COMMIT — they must ride the slot.
/// The per-opcode `out_pages` choice is a property of the running
/// kernel's FUSE TREE, not of the zc series, so each track carries its
/// own measured-and-source-verified set:
///
/// * **6.19-sqz** (elrepo EL8 base — VANILLA fuse readdir, kvmalloc
///   buffer): {READ, READLINK}. Field-measured 2026-08-06 (`7eccd53a`):
///   every body-carrying readdir reply arrives kmbuf-attached and the
///   kernel copies it (each bridged attempt failed its slot fetch and
///   fell back, +1 `fuse3_zc_fallbacks` per `ls`).
/// * **7.1-sqz** (CachyOS 7.1.6 base, whose fuse tree carries the
///   page-buffer readdir — `fuse_readdir_alloc_buf` sets
///   `ap->args.out_pages = true`): {READ, READLINK, READDIR,
///   READDIRPLUS}. Source-verified in the shipping package's own tree
///   AND live-proven: the un-keyed mirror EIO'd every `getdents64` on
///   an armed 7.1 mount (the loud no-attachment guard — the reverse
///   miss direction).
///
/// Uncertainty degrades SAFELY toward inclusion: an op listed here that
/// the running kernel actually serves copyable fails its slot fetch and
/// falls back to the kmbuf attachment (+1 fallback, output correct — a
/// vanilla-fuse 7.1 build would pay one clean fallback per readdir);
/// an op NOT listed that the kernel zc's hits the no-attachment EIO
/// guard — loud, never garbage, naming the op + track (see the module
/// doc).
pub fn out_paged(opcode: u32, track: KmbufTrack) -> bool {
    if opcode == fuse_opcode::FUSE_READ as u32 || opcode == fuse_opcode::FUSE_READLINK as u32 {
        return true;
    }
    match track {
        KmbufTrack::Sqz619 => false,
        KmbufTrack::Sqz71 => {
            opcode == fuse_opcode::FUSE_READDIR as u32
                || opcode == fuse_opcode::FUSE_READDIRPLUS as u32
        }
    }
}

/// In-direction paged opcodes on a zc queue, keyed by kernel track for
/// symmetry with [`out_paged`] (both tracks' fuse trees agree today):
/// the kernel registers the caller's SOURCE pages instead of copying
/// them into the kmbuf, so the payload announced by `payload_sz` lives
/// in the slot and must be extracted through the ring before dispatch.
/// (`fuse_fill_write_pages` / `fuse_direct_io` write / the writeback
/// path — `args->in_pages = true` — on BOTH trees.) FUSE_IOCTL's
/// in-paged shape is deliberately NOT extracted: this daemon serves no
/// data ioctls, and its ddir can be DEST (out wins), which would refuse
/// a source-direction ring op anyway — the delivery keeps an empty
/// payload and the tripwire counts it. FUSE_NOTIFY_REPLY (in-paged on
/// both trees) is unreachable: this daemon never issues retrieves.
pub fn in_paged(opcode: u32, track: KmbufTrack) -> bool {
    match track {
        // Both tracks agree today; the exhaustive match is what forces a
        // decision the day a track's fuse tree diverges in-direction.
        KmbufTrack::Sqz619 | KmbufTrack::Sqz71 => opcode == fuse_opcode::FUSE_WRITE as u32,
    }
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
    /// the direct read leg): the handler task parks on the oneshot; the
    /// CQE result is forwarded verbatim (`res` — negative errno, or
    /// bytes read). The slot's reply commit comes LATER via the normal
    /// path.
    HandlerFetch {
        done: tokio::sync::oneshot::Sender<i32>,
    },
    /// A handler-initiated device STORE (`WRITE_FIXED(device fd ←
    /// slot)`, the D14 write-side direct leg): the WRITE payload's
    /// registered pages DMA straight to the device — zero daemon
    /// copies, no extraction. The CQE result is forwarded verbatim;
    /// full-length success counts the `fuse3_zc_write_directs` ledger
    /// AT THE CONSUMING SITE (the root patch path), never here, so a
    /// caller-side validation failure can never leave a phantom count.
    HandlerStore {
        done: tokio::sync::oneshot::Sender<i32>,
    },
    /// A handler-requested LAZY payload extraction (`WRITE_FIXED(slot →
    /// memfd)`, dispatch-before-extraction): the delivered request was
    /// dispatched with its payload HELD in the slot; an ineligible
    /// shape materializes it here — the CQE mints the §5.4 lease over
    /// the bounce slot and answers the oneshot.
    LazyExtract {
        done: tokio::sync::oneshot::Sender<io::Result<bytes::Bytes>>,
        len: u32,
    },
}

/// The held-payload table (D14 write-side leg): one cell per
/// `(qid, ent)`, `nqueues × depth`, 0 = nothing held (a held length is
/// always > 0 — the delivery only holds payload-carrying WRITEs).
///
/// Lifecycle: SET at an armed WRITE's delivery (the payload stays in
/// the sparse slot — never extracted at delivery), READ by the
/// session's size validation and the handler's slot-source mint,
/// CLEARED by a completed extraction and OVERWRITTEN by the ent's next
/// delivery. Queries only ever race their own request's window: the
/// handler runs strictly before its reply, the reply strictly before
/// the ent's COMMIT, and the next delivery (the only re-pointing write)
/// strictly after that commit.
pub struct ZcHeldTable {
    cells: Vec<std::sync::atomic::AtomicU32>,
    depth: usize,
}

impl ZcHeldTable {
    pub fn new(nqueues: usize, depth: usize) -> Self {
        Self {
            cells: (0..nqueues * depth)
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            depth,
        }
    }

    fn cell(&self, qid: u16, ent_idx: usize) -> Option<&std::sync::atomic::AtomicU32> {
        if ent_idx >= self.depth {
            return None;
        }
        self.cells.get(qid as usize * self.depth + ent_idx)
    }

    /// Publish `len` bytes held in `(qid, ent)`'s slot (0 clears).
    /// Out-of-range coordinates are a loud no-op, never a panic.
    pub fn set(&self, qid: u16, ent_idx: usize, len: u32) {
        match self.cell(qid, ent_idx) {
            Some(c) => c.store(len, std::sync::atomic::Ordering::Release),
            None => {
                tracing::warn!("zc held table: set({qid}, {ent_idx}) out of range — ignored");
            }
        }
    }

    /// Clear `(qid, ent)`'s held payload (extraction completed).
    pub fn clear(&self, qid: u16, ent_idx: usize) {
        if let Some(c) = self.cell(qid, ent_idx) {
            c.store(0, std::sync::atomic::Ordering::Release);
        }
    }

    /// The held payload length, `None` when nothing is held (or the
    /// coordinates are out of range).
    pub fn get(&self, qid: u16, ent_idx: usize) -> Option<u32> {
        let len = self
            .cell(qid, ent_idx)?
            .load(std::sync::atomic::Ordering::Acquire);
        (len > 0).then_some(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::connection::kmbuf::KmbufTrack;

    /// The opcode mirror is TRACK-KEYED (out-paged-mirror divergence
    /// fix, 2026-08-06): the kernel's per-opcode `out_pages` choice is a
    /// property of the RUNNING KERNEL'S fuse tree, not of the zc series
    /// — and the two sqz tracks genuinely differ. Pinned per track
    /// against the ABI values (a silent ABI drift here is the corruption
    /// vector the module doc walks):
    ///
    /// * **6.19-sqz** (elrepo EL8 base, VANILLA fuse readdir — kvmalloc
    ///   buffer, `fs/fuse/readdir.c fuse_readdir_uncached`): out-paged =
    ///   {READ, READLINK}. Field-measured 2026-08-06 (`7eccd53a`): every
    ///   body-carrying readdir reply arrives kmbuf-attached and the
    ///   kernel COPIES it.
    /// * **7.1-sqz** (CachyOS 7.1.6 base, whose fuse tree carries the
    ///   page-buffer readdir — `fuse_readdir_alloc_buf` sets
    ///   `ap->args.out_pages = true`): out-paged = {READ, READLINK,
    ///   READDIR, READDIRPLUS}. Source-verified in the running kernel's
    ///   build tree AND live-measured (the un-keyed mirror EIO'd every
    ///   `getdents64` on an armed 7.1 mount — the loud no-attachment
    ///   guard, never garbage).
    ///
    /// The in-direction mirror is {WRITE} on BOTH tracks (both trees:
    /// `fuse_fill_write_pages` / direct-io write / writeback set
    /// `in_pages`; NOTIFY_REPLY and data IOCTLs are excluded — this
    /// daemon issues no retrieves and serves no data ioctls).
    #[test]
    fn test_paged_opcode_mirror_by_track() {
        for track in [KmbufTrack::Sqz619, KmbufTrack::Sqz71] {
            assert!(out_paged(fuse_opcode::FUSE_READ as u32, track));
            assert!(out_paged(fuse_opcode::FUSE_READLINK as u32, track));
            assert!(!out_paged(fuse_opcode::FUSE_WRITE as u32, track));
            assert!(!out_paged(fuse_opcode::FUSE_GETXATTR as u32, track));
            assert!(!out_paged(fuse_opcode::FUSE_LISTXATTR as u32, track));
            assert!(!out_paged(fuse_opcode::FUSE_GETATTR as u32, track));

            assert!(in_paged(fuse_opcode::FUSE_WRITE as u32, track));
            assert!(!in_paged(fuse_opcode::FUSE_READ as u32, track));
            assert!(!in_paged(fuse_opcode::FUSE_SETXATTR as u32, track));
        }

        // The divergence itself — the whole point of the track key.
        assert!(
            !out_paged(fuse_opcode::FUSE_READDIR as u32, KmbufTrack::Sqz619),
            "6.19-sqz readdir is kmbuf-copied (field-measured, 7eccd53a)"
        );
        assert!(
            !out_paged(fuse_opcode::FUSE_READDIRPLUS as u32, KmbufTrack::Sqz619),
            "6.19-sqz readdirplus is kmbuf-copied (field-measured, 7eccd53a)"
        );
        assert!(
            out_paged(fuse_opcode::FUSE_READDIR as u32, KmbufTrack::Sqz71),
            "7.1-sqz (cachyos base) pages readdir — fuse_readdir_alloc_buf"
        );
        assert!(
            out_paged(fuse_opcode::FUSE_READDIRPLUS as u32, KmbufTrack::Sqz71),
            "7.1-sqz (cachyos base) pages readdirplus — fuse_readdir_alloc_buf"
        );
    }

    /// The held-payload table (D14 write-side leg): one cell per
    /// (qid, ent), set at an armed WRITE's delivery (the payload stays
    /// in the sparse slot — never extracted at delivery), queried by the
    /// session/handler (`zc_write_held_len`), cleared by extraction and
    /// overwritten by the next delivery. Out-of-range coordinates never
    /// panic — a bad slot reads None and a bad set is a loud no-op.
    #[test]
    fn test_zc_held_table() {
        let t = ZcHeldTable::new(2, 4);
        assert_eq!(t.get(0, 0), None, "fresh table holds nothing");
        t.set(1, 3, 4096);
        assert_eq!(t.get(1, 3), Some(4096));
        assert_eq!(t.get(0, 3), None, "cells are per (qid, ent)");
        t.clear(1, 3);
        assert_eq!(t.get(1, 3), None, "cleared");
        assert_eq!(t.get(2, 0), None, "qid out of range reads None");
        assert_eq!(t.get(0, 4), None, "ent out of range reads None");
        t.set(9, 9, 1); // out of range: loud no-op, never a panic
        assert_eq!(t.get(9, 9), None);
        t.set(0, 1, 0); // len 0 = clear (0 is the none sentinel)
        assert_eq!(t.get(0, 1), None);
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
