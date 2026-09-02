use bytes::Bytes;
#[cfg(target_os = "linux")]
use std::fs::File;
use std::fs::OpenOptions;
use std::io;

#[cfg(target_os = "linux")]
use io_uring::{opcode, types, IoUring};

#[cfg(target_os = "linux")]
#[repr(transparent)]
struct SendIovec(libc::iovec);

#[cfg(target_os = "linux")]
unsafe impl Send for SendIovec {}

#[cfg(target_os = "linux")]
unsafe impl Sync for SendIovec {}

#[cfg(target_os = "linux")]
impl Drop for SendIovec {
    fn drop(&mut self) {}
}

#[cfg(target_os = "linux")]
struct DebugUring(IoUring);

/// Default SQ/CQ depth for FUSE /dev/fuse rings (P2-8). Overridable via
/// `SQUEEZEFS_FUSE_IO_URING_ENTRIES` (clamped to [64, 4096]).
#[cfg(target_os = "linux")]
fn fuse_uring_entries() -> u32 {
    std::env::var("SQUEEZEFS_FUSE_IO_URING_ENTRIES")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(1024)
        .clamp(64, 4096)
}

#[cfg(target_os = "linux")]
fn build_io_uring(entries: u32) -> io::Result<IoUring> {
    let sqpoll_idle = std::env::var("SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|idle| *idle > 0);

    if let Some(idle_ms) = sqpoll_idle {
        let mut builder = IoUring::builder();
        builder.setup_sqpoll(idle_ms);

        if let Some(cpu) = std::env::var("SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU")
            .ok()
            .and_then(|raw| raw.parse::<u32>().ok())
        {
            builder.setup_sqpoll_cpu(cpu);
        }

        if let Ok(ring) = builder.build(entries) {
            return Ok(ring);
        }
    }

    IoUring::new(entries)
}

/// Register `/dev/fuse` (or a cloned worker fd) as fixed file index 0 on a ring.
/// Returns whether subsequent SQEs may use `types::Fixed(0)`.
#[cfg(target_os = "linux")]
fn try_register_fuse_fd(ring: &IoUring, fd: RawFd) -> bool {
    match ring.submitter().register_files(&[fd]) {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!("fuse3: register_files(/dev/fuse) failed ({e:?}); using types::Fd");
            false
        }
    }
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for DebugUring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IoUring")
    }
}

#[cfg(target_os = "freebsd")]
use std::io::IoSlice;

#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd"
))]
use std::io::IoSliceMut;

use std::ops::{Deref, DerefMut};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::os::fd::OwnedFd;
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(target_os = "freebsd")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;
use std::pin::pin;
use std::sync::Arc;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::{ffi::OsString, path::Path};

use async_notify::Notify;
use futures_util::lock::Mutex;
use futures_util::{select, FutureExt};
#[cfg(target_os = "linux")]
use nix::fcntl::{FcntlArg, OFlag};
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use nix::sys::socket::{self, AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType};
#[cfg(target_os = "freebsd")]
use nix::sys::uio;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::process::Command;
#[cfg(target_os = "linux")]
use tracing::debug;
#[cfg(target_os = "freebsd")]
use tracing::warn;

use super::CompleteIoResult;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use crate::find_fusermount3;
#[cfg(target_os = "linux")]
use crate::raw::abi::FUSE_WRITE_IN_SIZE;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use crate::MountOptions;

pub struct FuseConnection {
    unmount_notify: Arc<Notify>,
    mode: ConnectionMode,
    /// Optional kernel FUSE-over-io_uring pool (Linux 6.14+). Shared across
    /// multi-queue clones. Lock-free slot (PERF-2): the request path reads
    /// it 4× per op (dispatch venue, reply venue, payload resolve, ready
    /// gate) while it is only written twice per session (install after
    /// INIT, teardown at disconnect/DESTROY) — the former process-global
    /// mutex was ~4 M lock RMWs/s of pure overhead at 1 M IOPS, one cache
    /// line shared by every connection clone. `OnceLock` makes install
    /// atomic-once; teardown NEVER empties the slot — liveness rides the
    /// pool's own `ready`/`active` atomics (`over_uring_slot_tests`
    /// contract 2), so a lock-free venue read can never race a tear-out.
    /// DELIBERATELY not arc-swap (P2 md-storm forensics, kept from the
    /// mutex era's rationale): arc_swap loads/stores share one
    /// process-global debt registry, so high-rate guard traffic from the
    /// transport threads lengthened every kv node-snapshot `store()`'s
    /// pay_all walk on the serialized conveyor path (−10 % create/del
    /// storms). `OnceLock::get` is a plain acquire load — no registry, no
    /// RMW.
    #[cfg(target_os = "linux")]
    over_uring:
        std::sync::Arc<std::sync::OnceLock<std::sync::Arc<super::fuse_over_uring::FuseOverUring>>>,
    #[cfg(target_os = "linux")]
    pub(crate) assigned_qid: Option<u16>,
    /// Post-arm classical sideband servicer (primary session only). The kernel
    /// keeps FORGET/BATCH_FORGET and INTERRUPT on the classical `/dev/fuse`
    /// queue even with FUSE-over-io_uring armed (`fuse_io_uring_ops` in
    /// fs/fuse/dev_uring.c), `fuse_resend` splices resends onto the classical
    /// `fiq->pending`, and regular requests can land classically in the
    /// unlocked `fiq->ops` switchover window. When set, this connection reads
    /// the classical device (io_uring `Readv`) instead of the uring inbound
    /// queues so that traffic is serviced, not stranded (stuck-request
    /// unmount wedge).
    #[cfg(target_os = "linux")]
    classical_sideband: std::sync::atomic::AtomicBool,
    /// Arrival stamp (transport epoch ns) of the most recent FUSE_READ
    /// this connection's dispatch loop popped from the uring inbound
    /// queue (`read_transport_phase_ns`): the dispatch loop is one
    /// sequential task per worker connection, and each worker owns its
    /// own clone, so a relaxed store-then-load pair per READ is exact.
    /// 0 = none (classical delivery) — `handle_read` skips the
    /// arrival-anchored phases.
    #[cfg(target_os = "linux")]
    last_read_arrival_ns: std::sync::atomic::AtomicU64,
    /// The FUSE_WRITE twin (`write_transport_phase_ns`): same sequential
    /// dispatch-loop exactness argument, consumed by `handle_write`.
    #[cfg(target_os = "linux")]
    last_write_arrival_ns: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for FuseConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuseConnection")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl FuseConnection {
    pub fn new(unmount_notify: Arc<Notify>) -> io::Result<Self> {
        #[cfg(target_os = "freebsd")]
        {
            let connection = NonBlockFuseConnection::new()?;

            Ok(Self {
                unmount_notify,
                mode: ConnectionMode::NonBlock(connection),
            })
        }

        #[cfg(target_os = "linux")]
        {
            let connection = BlockFuseConnection::new()?;

            Ok(Self {
                unmount_notify,
                mode: ConnectionMode::Block(connection),
                over_uring: std::sync::Arc::new(std::sync::OnceLock::new()),
                assigned_qid: None,
                classical_sideband: std::sync::atomic::AtomicBool::new(false),
                last_read_arrival_ns: std::sync::atomic::AtomicU64::new(0),
                last_write_arrival_ns: std::sync::atomic::AtomicU64::new(0),
            })
        }
    }

    #[cfg(target_os = "linux")]
    pub fn num_uring_queues(&self) -> Option<usize> {
        self.over_uring_pool().map(|p| p.nqueues as usize)
    }

    /// Read seam for the installed pool (post-INIT `mark_ready`,
    /// diagnostics). Hot paths go through the same slot — see
    /// `get_payload_buffer` / `over_uring_ready` / the venue filters in
    /// `inner_read_vectored` / `write_vectored`.
    #[cfg(target_os = "linux")]
    pub(crate) fn over_uring_pool(
        &self,
    ) -> Option<std::sync::Arc<super::fuse_over_uring::FuseOverUring>> {
        self.over_uring.get().cloned()
    }

    /// Start kernel FUSE-over-io_uring workers after FUSE_INIT. Required transport.
    /// Shared with multi-queue clones via [`clone_connection`].
    ///
    /// `geom` is the session geometry resolved by
    /// [`super::fuse_over_uring::TransportGeometry::resolve`] BEFORE the
    /// INIT reply was serialized — the rings registered here always match
    /// the `max_background`/`congestion_threshold` the kernel was told.
    #[cfg(target_os = "linux")]
    pub fn enable_fuse_over_uring(
        &self,
        geom: super::fuse_over_uring::TransportGeometry,
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // Already enabled (e.g. race with another enable call)
        if self.over_uring.get().is_some() {
            return Ok(());
        }
        let fd = self.as_fd().as_raw_fd();
        let pool = super::fuse_over_uring::FuseOverUring::try_start(fd, geom).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "FUSE-over-io_uring is required but setup failed: {e} \
                     (need root + CONFIG_FUSE_IO_URING; enable_uring will be set to Y)"
                ),
            )
        })?;
        if let Err(loser) = self.install_over_uring(pool) {
            // Another enable won the install race (never on the live INIT
            // path — enables are sequential there): shut the fresh pool
            // down instead of orphaning armed workers on the session fd.
            loser.shutdown();
        }
        Ok(())
    }

    /// Install-once seam for the session's FUSE-over-io_uring pool —
    /// shared across every [`Self::clone_connection`] worker clone. At
    /// most ONE pool is ever installed per connection family: the first
    /// caller wins, later callers get their pool back untouched
    /// (`Err(pool)`) and own its teardown. Public for the sim venue
    /// ([`super::fuse_over_uring::FuseOverUring::sim_inert`] — slot
    /// protocol tests + the reply-send prelude bench).
    #[cfg(target_os = "linux")]
    pub fn install_over_uring(
        &self,
        pool: std::sync::Arc<super::fuse_over_uring::FuseOverUring>,
    ) -> Result<(), std::sync::Arc<super::fuse_over_uring::FuseOverUring>> {
        self.over_uring.set(pool)
    }

    /// Teardown seam (session disconnect + FUSE_DESTROY): shut the
    /// installed pool down. Idempotent — racing teardowns from multiple
    /// worker clones are safe (`FuseOverUring::shutdown` gates its
    /// side effects on the `active` swap). The installed pool stays
    /// OBSERVABLE afterwards: hot-path venue reads derive liveness from
    /// the pool's own `ready`/`active` atomics, never from slot
    /// emptiness, so a concurrent lock-free venue probe can never race a
    /// slot tear-out.
    #[cfg(target_os = "linux")]
    pub fn teardown_over_uring(&self) {
        if let Some(pool) = self.over_uring.get() {
            pool.shutdown();
        }
    }

    #[cfg(target_os = "linux")]
    pub fn get_payload_buffer(&self, slot: crate::raw::ReplySlot) -> Option<(u64, usize)> {
        // Lock-free slot read (PERF-2; the field doc carries the
        // not-arc-swap rationale) — a plain acquire load, then a direct
        // (qid, ent_idx) index: no map probe, no mutex (PERF-16).
        let pool = self.over_uring.get()?;
        pool.get_payload_buffer(slot)
    }

    /// FUSE-2 rows 2 and 3: commit a reply against its ring slot
    /// **directly**, bypassing the session's reply task.
    ///
    /// The transport's per-queue commit channel is independent of the
    /// reply task, so a handler whose reply task has died (or that
    /// panicked before replying) still reaches the kernel instead of
    /// leaving the caller in uninterruptible sleep. Errors here are the
    /// genuinely unaddressable cases and are counted by the caller.
    #[cfg(target_os = "linux")]
    pub(crate) fn commit_reply_direct(
        &self,
        slot: crate::raw::ReplySlot,
        header: Vec<u8>,
        body: bytes::Bytes,
    ) -> io::Result<()> {
        let pool = self
            .over_uring
            .get()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no over-uring pool"))?;
        pool.submit_reply(slot, header, body)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn commit_reply_direct(
        &self,
        _slot: crate::raw::ReplySlot,
        _header: Vec<u8>,
        _body: bytes::Bytes,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no FUSE-over-io_uring transport on this platform",
        ))
    }

    /// MEM-1: claim a DMA-destination owner token over `[addr, addr+len)`
    /// (see [`super::fuse_over_uring::FuseOverUring::lease_dest_window`]).
    /// The daemon's mount arm path wraps this into the `nvme_dev`
    /// dest-resolver registry; the token is held by the DEVICE WORKER for
    /// the SQE's lifetime, parking the ent's re-arm until the DMA cannot
    /// land anymore.
    #[cfg(target_os = "linux")]
    pub fn lease_dest_window(
        &self,
        addr: u64,
        len: usize,
    ) -> Option<super::fuse_over_uring::DestDmaLease> {
        // PERF-2: a plain acquire load on the install-once `OnceLock` —
        // no RMW on the request path (the pool is installed once at arm
        // and never replaced; liveness rides the pool's own atomics).
        let pool = self.over_uring.get()?;
        pool.lease_dest_window(addr, len)
    }

    /// True once the FUSE-over-io_uring pool is armed and ready — the
    /// gate for in-place replies (P2 per-op economy): an armed session's
    /// reply is a synchronous COMMIT enqueue (`submit_reply`), safe to
    /// run from the handler task itself instead of paying an unbounded-
    /// channel hop + reply-task wake per op.
    #[cfg(target_os = "linux")]
    pub fn over_uring_ready(&self) -> bool {
        self.over_uring.get().is_some_and(|p| p.is_ready())
    }

    /// True when the session's request hot path runs the
    /// `FUSE_URING_ZERO_COPY` arm (K1 kill) — the gate the daemon's READ
    /// handler consults before minting a device-fetch descriptor.
    #[cfg(target_os = "linux")]
    pub fn zc_armed(&self) -> bool {
        self.over_uring.get().is_some_and(|p| p.zc_armed())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn zc_armed(&self) -> bool {
        false
    }

    /// zc direct leg (K1 kill): DMA `len` bytes from `fd@off` straight
    /// into the requesting slot's registered pages — see
    /// [`super::fuse_over_uring::FuseOverUring::zc_device_fetch`].
    #[cfg(target_os = "linux")]
    pub async fn zc_device_fetch(
        &self,
        slot: crate::raw::ReplySlot,
        fd: std::os::fd::RawFd,
        off: u64,
        len: u32,
    ) -> io::Result<u32> {
        let pool = self
            .over_uring
            .get()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no over-uring pool"))?;
        pool.zc_device_fetch(slot, fd, off, len).await
    }

    /// D14 write-side: the HELD WRITE-payload length of `slot`'s
    /// request (`Some(len)` ⇒ the payload sits in the transport's
    /// sparse slot, consumable via [`Self::zc_write_store`] /
    /// [`Self::zc_write_extract`]) — see
    /// [`super::fuse_over_uring::FuseOverUring::zc_write_held_len`].
    #[cfg(target_os = "linux")]
    pub fn zc_write_held_len(&self, slot: crate::raw::ReplySlot) -> Option<u32> {
        self.over_uring.get()?.zc_write_held_len(slot)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn zc_write_held_len(&self, _slot: crate::raw::ReplySlot) -> Option<u32> {
        None
    }

    /// D14 write-side direct leg: DMA the request's held WRITE payload
    /// slot→device — see
    /// [`super::fuse_over_uring::FuseOverUring::zc_write_store`].
    #[cfg(target_os = "linux")]
    pub async fn zc_write_store(
        &self,
        slot: crate::raw::ReplySlot,
        fd: std::os::fd::RawFd,
        dev_off: u64,
    ) -> io::Result<u32> {
        let pool = self
            .over_uring
            .get()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no over-uring pool"))?;
        pool.zc_write_store(slot, fd, dev_off).await
    }

    /// D14 write-side lazy extraction: materialize the request's held
    /// WRITE payload as a §5.4 lease over the bounce — see
    /// [`super::fuse_over_uring::FuseOverUring::zc_write_extract`].
    #[cfg(target_os = "linux")]
    pub async fn zc_write_extract(&self, slot: crate::raw::ReplySlot) -> io::Result<Bytes> {
        let pool = self
            .over_uring
            .get()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no over-uring pool"))?;
        pool.zc_write_extract(slot).await
    }

    /// ACK-early (0029): arm RETAIN for this slot's next commit —
    /// `true` iff the session is retention-armed and the flag stuck.
    /// See [`super::fuse_over_uring::FuseOverUring::zc_commit_retain`].
    #[cfg(target_os = "linux")]
    pub fn zc_commit_retain(&self, slot: crate::raw::ReplySlot) -> bool {
        self.over_uring
            .get()
            .map(|p| p.zc_commit_retain(slot))
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn zc_commit_retain(&self, _slot: crate::raw::ReplySlot) -> bool {
        false
    }

    /// ACK-early (0029): release a COMMIT_RETAIN'd slot — the store
    /// continuation's one obligation. See
    /// [`super::fuse_over_uring::FuseOverUring::zc_release_payload`].
    #[cfg(target_os = "linux")]
    pub fn zc_release_payload(&self, slot: crate::raw::ReplySlot) -> io::Result<()> {
        let pool = self
            .over_uring
            .get()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no over-uring pool"))?;
        pool.zc_release_payload(slot)
    }

    /// Register the session's fused-write dispatcher (zc-write-fusion
    /// campaign): the mint that builds one delivery's WRITE handler
    /// future plus the runtime handle fused polls enter — see
    /// [`super::fuse_over_uring::FuseOverUring::set_fused_write_dispatcher`].
    /// No-op when no over-uring pool exists (classical sessions).
    #[cfg(target_os = "linux")]
    pub fn set_fused_write_dispatcher(&self, d: super::fuse_over_uring::fused::FusedWriteDispatch) {
        if let Some(pool) = self.over_uring.get() {
            pool.set_fused_write_dispatcher(d);
        }
    }

    /// Register the zc-write HOLD gate (fused-lane-predicate campaign,
    /// 2026-08-08) — see
    /// [`super::fuse_over_uring::FuseOverUring::set_zc_write_hold_gate`].
    /// No-op when no over-uring pool exists (classical sessions).
    #[cfg(target_os = "linux")]
    pub fn set_zc_write_hold_gate(&self, g: super::fuse_over_uring::fused::ZcHoldGate) {
        if let Some(pool) = self.over_uring.get() {
            pool.set_zc_write_hold_gate(g);
        }
    }

    /// Commit a reply whose payload already sits in the request's pages
    /// (the zc direct leg) — see
    /// [`super::fuse_over_uring::FuseOverUring::submit_reply_prefilled`].
    #[cfg(target_os = "linux")]
    pub fn submit_reply_prefilled(
        &self,
        slot: crate::raw::ReplySlot,
        header: Vec<u8>,
        payload_len: u32,
    ) -> io::Result<()> {
        let pool = self
            .over_uring
            .get()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no over-uring pool"))?;
        pool.submit_reply_prefilled(slot, header, payload_len)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn submit_reply_prefilled(
        &self,
        _slot: crate::raw::ReplySlot,
        _header: Vec<u8>,
        _payload_len: u32,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no FUSE-over-io_uring transport on this platform",
        ))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn over_uring_ready(&self) -> bool {
        false
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    pub async fn new_with_unprivileged(
        mount_options: MountOptions,
        mount_path: impl AsRef<Path>,
        unmount_notify: Arc<Notify>,
    ) -> io::Result<Self> {
        let connection =
            NonBlockFuseConnection::new_with_unprivileged(mount_options, mount_path).await?;

        Ok(Self {
            unmount_notify,
            mode: ConnectionMode::NonBlock(connection),
            over_uring: std::sync::Arc::new(std::sync::OnceLock::new()),
            assigned_qid: None,
            classical_sideband: std::sync::atomic::AtomicBool::new(false),
            last_read_arrival_ns: std::sync::atomic::AtomicU64::new(0),
            last_write_arrival_ns: std::sync::atomic::AtomicU64::new(0),
        })
    }

    #[cfg(target_os = "linux")]
    pub fn clone_connection(&self) -> io::Result<Self> {
        match &self.mode {
            ConnectionMode::Block(_) => {
                use std::os::fd::AsRawFd;
                let primary_fd = self.as_fd().as_raw_fd();
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .read(true)
                    .open("/dev/fuse")?;

                let worker_fd = file.as_raw_fd();
                let mut session_fd = primary_fd as libc::c_int;

                nix::ioctl_read!(fuse_dev_clone, 229, 0, libc::c_int);
                unsafe {
                    fuse_dev_clone(worker_fd, &mut session_fd)?;
                }

                let connection = BlockFuseConnection::from_file(file)?;

                Ok(Self {
                    unmount_notify: self.unmount_notify.clone(),
                    mode: ConnectionMode::Block(connection),
                    // Share over-uring pool so multi-queue session workers pull the same inbound queue.
                    over_uring: self.over_uring.clone(),
                    assigned_qid: None,
                    classical_sideband: std::sync::atomic::AtomicBool::new(false),
                    last_read_arrival_ns: std::sync::atomic::AtomicU64::new(0),
                    last_write_arrival_ns: std::sync::atomic::AtomicU64::new(0),
                })
            }
            #[cfg(feature = "unprivileged")]
            ConnectionMode::NonBlock(_) => {
                use std::os::fd::{AsRawFd, FromRawFd};
                use std::os::unix::fs::OpenOptionsExt;
                let primary_fd = self.as_fd().as_raw_fd();
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open("/dev/fuse")?;
                let worker_fd = file.as_fd().as_raw_fd();
                let mut session_fd = primary_fd as libc::c_int;

                nix::ioctl_read!(fuse_dev_clone, 229, 0, libc::c_int);
                unsafe {
                    fuse_dev_clone(worker_fd, &mut session_fd)?;
                }

                let fd: std::os::fd::OwnedFd = file.into();
                let entries = fuse_uring_entries();
                let read_ring = build_io_uring(entries)?;
                let write_ring = build_io_uring(entries)?;

                let read_use_fixed = try_register_fuse_fd(&read_ring, fd.as_raw_fd());
                let write_use_fixed = try_register_fuse_fd(&write_ring, fd.as_raw_fd());

                let read_event_fd =
                    unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                if read_event_fd < 0 {
                    return Err(io::Error::last_os_error());
                }

                let write_event_fd =
                    unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                if write_event_fd < 0 {
                    unsafe {
                        libc::close(read_event_fd);
                    }
                    return Err(io::Error::last_os_error());
                }

                if let Err(e) = read_ring.submitter().register_eventfd(read_event_fd) {
                    unsafe {
                        libc::close(read_event_fd);
                        libc::close(write_event_fd);
                    }
                    return Err(e);
                }

                if let Err(e) = write_ring.submitter().register_eventfd(write_event_fd) {
                    unsafe {
                        libc::close(read_event_fd);
                        libc::close(write_event_fd);
                    }
                    return Err(e);
                }

                let read_ring_fd = unsafe { OwnedFd::from_raw_fd(read_event_fd) };
                let write_ring_fd = unsafe { OwnedFd::from_raw_fd(write_event_fd) };

                let read_ring = DebugUring(read_ring);
                let write_ring = DebugUring(write_ring);

                let connection = NonBlockFuseConnection {
                    fd,
                    read_ring: std::sync::Mutex::new(read_ring),
                    read_ring_fd,
                    write_ring: std::sync::Mutex::new(write_ring),
                    write_ring_fd,
                    read_use_fixed,
                    write_use_fixed,
                    read: Mutex::new(()),
                    write: Mutex::new(()),
                };

                Ok(Self {
                    unmount_notify: self.unmount_notify.clone(),
                    mode: ConnectionMode::NonBlock(connection),
                    // Share over-uring pool so multi-queue session workers pull the same inbound queue.
                    over_uring: self.over_uring.clone(),
                    assigned_qid: None,
                    classical_sideband: std::sync::atomic::AtomicBool::new(false),
                    last_read_arrival_ns: std::sync::atomic::AtomicU64::new(0),
                    last_write_arrival_ns: std::sync::atomic::AtomicU64::new(0),
                })
            }
        }
    }

    /// Turn this (primary) connection into the post-arm classical sideband
    /// servicer: its session keeps reading the classical `/dev/fuse` device
    /// (io_uring `Readv`) after FUSE-over-io_uring is armed, because the
    /// kernel still routes FORGET/BATCH_FORGET, INTERRUPT, NOTIFY_RESEND
    /// resends, and `fiq->ops` switchover-window stragglers there. Without a
    /// reader those strand forever (`fusectl waiting >= 1`, syncfs blocks,
    /// umount EBUSY).
    #[cfg(target_os = "linux")]
    pub fn set_classical_sideband(&self) {
        self.classical_sideband
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Consume the arrival stamp of the READ this connection's dispatch
    /// loop just popped (`read_transport_phase_ns` — see the field doc).
    /// `swap(0)` so a classically-delivered READ never anchors against a
    /// stale uring arrival. 0 = no uring arrival recorded.
    #[cfg(target_os = "linux")]
    pub(crate) fn take_read_arrival_ns(&self) -> u64 {
        self.last_read_arrival_ns
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    /// The FUSE_WRITE twin: consume the arrival stamp of the WRITE this
    /// connection's dispatch loop just popped (`write_transport_phase_ns`).
    /// `swap(0)` so a classically-delivered WRITE never anchors against a
    /// stale uring arrival. 0 = no uring arrival recorded.
    #[cfg(target_os = "linux")]
    pub(crate) fn take_write_arrival_ns(&self) -> u64 {
        self.last_write_arrival_ns
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }

    /// Read one request. The fourth tuple element is the reply address
    /// of the delivery (FUSE-2 ⊕ PERF-16): the ring slot it arrived on,
    /// or [`crate::raw::ReplySlot::Classical`]. Carrying it out of the
    /// read is what lets the reply commit against its slot with no
    /// `unique → slot` map in between.
    pub async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        header_buf: Vec<u8>,
        data_buf: T,
    ) -> Option<(
        (Vec<u8>, T, Option<Bytes>, crate::raw::ReplySlot),
        io::Result<usize>,
    )> {
        let mut unmount_fut = pin!(self.unmount_notify.notified().fuse());
        let mut read_fut = pin!(self.inner_read_vectored(header_buf, data_buf).fuse());

        select! {
            _ = unmount_fut => None,
            res = read_fut => Some(res)
        }
    }

    async fn inner_read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> (
        (Vec<u8>, T, Option<Bytes>, crate::raw::ReplySlot),
        io::Result<usize>,
    ) {
        // After arm, the request hot path is FUSE-over-io_uring. The classical
        // device is still read pre-arm (INIT — the kernel rejects REGISTER until
        // fch->initialized) and, post-arm, by the dedicated classical sideband
        // session (`classical_sideband`): the kernel keeps FORGET/INTERRUPT/
        // resends and `fiq->ops` switchover stragglers on the classical queue
        // even when the ring is armed. Both classical reads ride io_uring
        // (`Readv` on `/dev/fuse` below).
        #[cfg(target_os = "linux")]
        {
            let pool = if self
                .classical_sideband
                .load(std::sync::atomic::Ordering::Acquire)
            {
                // Sideband servicer: never drain the uring inbound queues.
                None
            } else {
                // Lock-free slot read; borrow — the slot never empties
                // (teardown contract 2), so no refcount RMW per pull.
                self.over_uring.get()
            };
            // After arm: uring-only. Before arm / when inactive: fall through.
            // - ready → drain uring inbound
            // - shut down (!active) → disconnect error
            // - not yet ready → classical (INIT only; REGISTER wait is inside enable)
            if let Some(pool) = pool.filter(|p| p.is_ready() || !p.is_active()) {
                let qid = self.assigned_qid.unwrap_or(0);
                // Pure event-driven pull (L3 lever C): parks on the queue
                // channel / shutdown notify — no 200 ms poll cadence, no
                // per-pull timer registration. `None` means the pool shut
                // down (or a structurally-unreachable qid): fail loud so the
                // session worker exits, never poll-park.
                let inbound = match pool.recv_inbound(qid).await {
                    Some(r) => r,
                    None => {
                        return (
                            (header_buf, data_buf, None, crate::raw::ReplySlot::Classical),
                            Err(io::Error::new(
                                io::ErrorKind::NotConnected,
                                "fuse-over-uring inactive (unmounted or aborted)",
                            )),
                        );
                    }
                };

                // Reconstruct classical fuse framing for the session dispatcher:
                //   [fuse_in_header 40][arg0 in op_in][arg1+ in payload]
                // Kernel puts in_args[0] into the fixed op_in slot and the rest into
                // the payload buffer. Classical handlers expect a contiguous body
                // after the header (e.g. LOOKUP name). Use in_header.len to size it.
                if inbound.header_and_op.len() < 40 || header_buf.len() < 40 {
                    return (
                        (header_buf, data_buf, None, crate::raw::ReplySlot::Classical),
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "short fuse-over-uring header",
                        )),
                    );
                }
                header_buf[..40].copy_from_slice(&inbound.header_and_op[..40]);
                let total_len = u32::from_le_bytes(header_buf[0..4].try_into().unwrap()) as usize;
                let body_need = total_len.saturating_sub(40);
                let op_in = &inbound.header_and_op[40..];
                let payload = &inbound.payload;
                // FUSE_WRITE (§5.4 transport zero-copy): the body already
                // rides `payload` — as a zero-copy lease over the registered
                // uring buffer — and `handle_write` consumes exactly that
                // `Bytes`. Copying the body into the session buffer here
                // would spend a second 1 MiB memcpy per request (audit #2)
                // for bytes nothing reads. Copy only the fuse_write_in arg
                // from op_in; every other opcode keeps the reconstruction
                // below verbatim.
                let opcode = u32::from_le_bytes(inbound.header_and_op[4..8].try_into().unwrap());
                // read/write_transport_phase_ns `queue_wait`: CQE reap →
                // this dispatch pop. The arrival stamp is parked on the
                // connection for the handler's `transport_total` anchor —
                // this dispatch loop is one sequential task and each
                // worker owns its own connection clone, so the
                // store-then-read pairing per op is exact.
                //
                // op-trace (audit A2): `transport_recv` = the arrival
                // stamp already in hand, `dispatch` = the pop instant.
                // READ/WRITE read the clock for `queue_wait` anyway; any
                // other opcode reads it only when the op is in the
                // sample (one pointer load otherwise).
                let is_read = opcode == crate::raw::abi::fuse_opcode::FUSE_READ as u32;
                let is_write = opcode == crate::raw::abi::fuse_opcode::FUSE_WRITE as u32;
                let traced = crate::raw::op_trace::traced(inbound.unique);
                if is_read || is_write || traced != 0 {
                    let now_ns = crate::raw::read_phase::transport_now_ns();
                    let queue_wait =
                        std::time::Duration::from_nanos(now_ns.saturating_sub(inbound.arrived_ns));
                    if is_read {
                        crate::raw::read_phase::read_transport_phase_record(
                            crate::raw::read_phase::TransportPhase::QueueWait,
                            queue_wait,
                        );
                        self.last_read_arrival_ns
                            .store(inbound.arrived_ns, std::sync::atomic::Ordering::Relaxed);
                    } else if is_write {
                        crate::raw::read_phase::write_transport_phase_record(
                            crate::raw::read_phase::TransportPhase::QueueWait,
                            queue_wait,
                        );
                        self.last_write_arrival_ns
                            .store(inbound.arrived_ns, std::sync::atomic::Ordering::Relaxed);
                    }
                    if traced != 0 {
                        crate::raw::op_trace::stamp(
                            traced,
                            crate::raw::op_trace::Stage::TransportRecv,
                            crate::raw::read_phase::transport_instant(inbound.arrived_ns),
                        );
                        crate::raw::op_trace::stamp(
                            traced,
                            crate::raw::op_trace::Stage::Dispatch,
                            crate::raw::read_phase::transport_instant(now_ns),
                        );
                    }
                }
                // D14: held-slot WRITE deliveries carry an EMPTY
                // placeholder payload (the body stays in the sparse
                // slot), so the fast arm keys on the header shape alone
                // — a 0-size WRITE takes it too (empty payload, size 0:
                // the session validation is a tautology there).
                if opcode == crate::raw::abi::fuse_opcode::FUSE_WRITE as u32
                    && body_need >= FUSE_WRITE_IN_SIZE
                {
                    let n = FUSE_WRITE_IN_SIZE.min(op_in.len()).min(data_buf.len());
                    data_buf[..n].copy_from_slice(&op_in[..n]);
                    return (
                        (header_buf, data_buf, Some(inbound.payload), inbound.slot),
                        Ok(40 + n),
                    );
                }
                // Bytes of body that live in op_in (first in_arg); remainder in payload.
                // NOTE: We keep payload as Bytes for zero-copy writes!
                let from_op = body_need.saturating_sub(payload.len()).min(op_in.len());
                let from_payload = body_need.saturating_sub(from_op).min(payload.len());
                let mut filled = 0usize;
                if from_op > 0 && filled < data_buf.len() {
                    let n = from_op.min(data_buf.len() - filled);
                    data_buf[filled..filled + n].copy_from_slice(&op_in[..n]);
                    filled += n;
                }
                if from_payload > 0 && filled < data_buf.len() {
                    let n = from_payload.min(data_buf.len() - filled);
                    data_buf[filled..filled + n].copy_from_slice(&payload[..n]);
                    filled += n;
                }
                // If in_header.len was wrong/zero (some uring paths), fall back to
                // payload-first then op_in — LOOKUP often has name only in payload.
                if filled == 0 {
                    let n = payload.len().min(data_buf.len());
                    if n > 0 {
                        data_buf[..n].copy_from_slice(&payload[..n]);
                        filled = n;
                    }
                    if filled < data_buf.len() {
                        let n2 = op_in
                            .iter()
                            .position(|&b| b == 0)
                            .map(|i| i + 1)
                            .unwrap_or(0)
                            .min(data_buf.len() - filled);
                        if n2 > 0 && op_in.iter().any(|&b| b != 0) {
                            data_buf[filled..filled + n2].copy_from_slice(&op_in[..n2]);
                            filled += n2;
                        }
                    }
                }
                return (
                    (header_buf, data_buf, Some(inbound.payload), inbound.slot),
                    Ok(40 + filled),
                );
            }
        }

        // Classical device path (FUSE_INIT only; after arm the branch above is used).
        let result = match &self.mode {
            #[cfg(target_os = "linux")]
            ConnectionMode::Block(connection) => {
                connection.read_vectored(header_buf, data_buf).await
            }
            #[cfg(any(
                all(target_os = "linux", feature = "unprivileged"),
                target_os = "freebsd"
            ))]
            ConnectionMode::NonBlock(connection) => {
                connection.read_vectored(header_buf, data_buf).await
            }
        };
        // Sideband accounting only. The `classical_inflight` set this
        // used to feed is GONE (FUSE-2 ⊕ PERF-16): a reply is classical
        // because its request carries `ReplySlot::Classical`, not
        // because a shared `HashSet<u64>` behind a global mutex says so.
        // That deletes the per-reply set probe AND the whole leak class
        // FUSE-3h describes — a never-removed unique (opcode 41
        // `FUSE_NOTIFY_REPLY`, which is never replied to) used to make
        // every reply on every queue take that mutex forever.
        #[cfg(target_os = "linux")]
        if let ((ref hdr, _), Ok(n)) = &result {
            if *n >= 16 {
                if self
                    .classical_sideband
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    super::fuse_over_uring::note_classical_sideband();
                }
                if super::fuse_over_uring::transport_debug() {
                    let unique = u64::from_le_bytes(hdr[8..16].try_into().unwrap_or([0; 8]));
                    let op = u32::from_le_bytes(hdr[4..8].try_into().unwrap_or([0; 4]));
                    eprintln!("[XPORT] classical-deliver unique={unique} op={op}");
                }
            }
        }
        let (buffers, res) = result;
        let (hdr, data) = buffers;
        // Classical delivery: the reply rides the device write.
        ((hdr, data, None, crate::raw::ReplySlot::Classical), res)
    }

    /// generic/451 — the SYNCHRONOUS kernel page-invalidation push: write
    /// one `FUSE_NOTIFY_INVAL_INODE` for `[offset, offset+len)` through
    /// the device and return only after the kernel processed it
    /// (`fuse_dev_do_write` runs `fuse_reverse_inval_inode` synchronously
    /// inside the device write). This is the ORDERING primitive the
    /// DIO-write page-coherence law needs: a reply committed AFTER this
    /// returns — over-uring COMMIT included — is guaranteed to follow the
    /// page invalidation, which the async
    /// [`crate::notify::Notify::invalid_inode`] enqueue cannot promise
    /// (it queues on the session reply channel while the request's own
    /// ack rides the ring-commit lane and can overtake it — the residual
    /// generic/451 window found live, 2026-08-03).
    ///
    /// **Venue law (the live wedge, 2026-08-03, kernel stacks on file):**
    /// this write BLOCKS in `invalidate_inode_pages2_range` →
    /// `folio_wait_bit_common` until every in-flight READ covering the
    /// range completes, so it must never occupy a request-servicing lane.
    /// The first cut submitted it through `write_vectored`'s classical
    /// io_uring write from the WRITE handler's own task — inline-issued
    /// at submit (`io_submit_sqes` → `io_write` → `fuse_dev_write`), it
    /// parked the submitting thread inside `io_uring_enter` waiting on a
    /// folio whose READ that very lane family had to service: writer,
    /// readers and daemon all D-state. A **blocking-pool `write(2)` on a
    /// dup'd device fd** keeps the folio wait off every handler lane; the
    /// concurrently-running lanes serve the pending READs, the folios
    /// unlock, the invalidation completes, and only then does the caller
    /// get to reply. (This is deliberately NOT an exception to
    /// always-use-io_uring's spirit: the write is a kernel-side ORDERING
    /// BARRIER that can lawfully sleep for a full READ round trip, not an
    /// I/O hot path — the same class as the mandated classical sideband.)
    ///
    /// `Ok(true)` = invalidated; `Ok(false)` = the kernel no longer knows
    /// the ino (`-ENOENT` — a benign race with eviction: no inode means
    /// no pages to serve stale).
    #[cfg(target_os = "linux")]
    pub async fn notify_inval_inode_sync(
        &self,
        inode: u64,
        offset: i64,
        len: i64,
    ) -> io::Result<bool> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let frame = crate::notify::inval_inode_frame(inode, offset, len);
        // Dup so the blocking task owns a fd that stays valid even if
        // this future is cancelled and the connection torn down before
        // the pool thread runs (a raw-fd capture could alias a reused
        // number).
        let dup = unsafe { libc::dup(self.as_fd().as_raw_fd()) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` is a fresh fd this task exclusively owns.
        let owned = unsafe { OwnedFd::from_raw_fd(dup) };
        let res = crate::sqz_blocking::run_blocking(move || {
            loop {
                // SAFETY: one whole-frame write of an initialized buffer
                // on an owned /dev/fuse fd; the device consumes exactly
                // one message per write (no partial-write handling).
                let n =
                    unsafe { libc::write(owned.as_raw_fd(), frame.as_ptr().cast(), frame.len()) };
                if n >= 0 {
                    return Ok(());
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        })
        .await;
        match res {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Reply to one request. `slot` is the address the request was
    /// delivered on (FUSE-2 ⊕ PERF-16): a ring slot commits against that
    /// ent, [`crate::raw::ReplySlot::Classical`] takes the device write.
    ///
    /// Routing by SLOT rather than by "is the pool ready and is this
    /// unique in the classical set?" is what closes FUSE-2 row 9: a
    /// post-shutdown ring reply can no longer fall through to a classical
    /// write that returns `ENOENT` and loses the reply.
    pub async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
        slot: crate::raw::ReplySlot,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        // After arm: uring-delivered requests reply via COMMIT_AND_FETCH
        // against their own slot. Classical deliveries (INIT, the
        // kernel-mandated sideband, switchover-window stragglers) and
        // daemon-initiated notifications carry `ReplySlot::Classical` and
        // must be completed with a device write — if they tried COMMIT
        // they would stay in kernel `waiting` and plain `umount` would
        // return EBUSY forever.
        #[cfg(target_os = "linux")]
        if slot.is_ring() {
            // Lock-free slot read; borrow — the slot never empties
            // (teardown contract 2), so no refcount RMW per reply.
            if let Some(pool) = self.over_uring.get() {
                // unique is at offset 8 in fuse_out_header (len u32, error i32, unique u64)
                let unique = if data.deref().len() >= 16 {
                    u64::from_le_bytes(data.deref()[8..16].try_into().unwrap())
                } else {
                    0
                };
                let body_bytes = if let Some(ref ext) = body_extend_data {
                    let slice = ext.deref();
                    let dest_addr = pool
                        .get_payload_buffer(slot)
                        .map(|(ptr, _)| ptr as *const u8);
                    if let Some(addr) = dest_addr {
                        if slice.as_ptr() == addr {
                            bytes::Bytes::from_owner(UringBufOwner {
                                ptr: slice.as_ptr(),
                                len: slice.len(),
                            })
                        } else {
                            bytes::Bytes::copy_from_slice(slice)
                        }
                    } else {
                        bytes::Bytes::copy_from_slice(slice)
                    }
                } else {
                    bytes::Bytes::new()
                };
                let len = data.deref().len() + body_bytes.len();
                match pool.submit_reply(slot, data.deref().to_vec(), body_bytes) {
                    Ok(()) => return ((data, body_extend_data), Ok(len)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        // The slot no longer holds this request (teardown,
                        // or a double reply the state machine refused).
                        // FUSE-3b: this reply was NOT delivered — it must
                        // not be reported as `Ok(len)` and counted on the
                        // reply gauge. Count it apart and tell the caller.
                        super::fuse_over_uring::note_reply_dropped_no_slot();
                        debug!(
                            unique,
                            "fuse-over-uring COMMIT miss; drop (no classical fallback)"
                        );
                        if super::fuse_over_uring::transport_debug() {
                            eprintln!("[XPORT] reply-dropped-notfound unique={unique}");
                        }
                        return ((data, body_extend_data), Err(e));
                    }
                    Err(e) => return ((data, body_extend_data), Err(e)),
                }
            }
        }

        match &self.mode {
            #[cfg(target_os = "linux")]
            ConnectionMode::Block(connection) => {
                connection.write_vectored(data, body_extend_data).await
            }
            #[cfg(any(
                all(target_os = "linux", feature = "unprivileged"),
                target_os = "freebsd"
            ))]
            ConnectionMode::NonBlock(connection) => {
                connection.write_vectored(data, body_extend_data).await
            }
        }
    }
}

#[derive(Debug)]
enum ConnectionMode {
    #[cfg(target_os = "linux")]
    Block(BlockFuseConnection),
    #[cfg(any(
        all(target_os = "linux", feature = "unprivileged"),
        target_os = "freebsd"
    ))]
    NonBlock(NonBlockFuseConnection),
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct BlockFuseConnection {
    file: File,
    read_ring: std::sync::Mutex<DebugUring>,
    read_ring_fd: OwnedFd,
    write_ring: std::sync::Mutex<DebugUring>,
    write_ring_fd: OwnedFd,
    /// When true, SQEs use `types::Fixed(0)` for the fuse device (P2-8).
    read_use_fixed: bool,
    write_use_fixed: bool,
    read: Mutex<()>,
    write: Mutex<()>,
}

#[cfg(target_os = "linux")]
impl BlockFuseConnection {
    pub fn new() -> io::Result<Self> {
        const DEV_FUSE: &str = "/dev/fuse";
        let file = OpenOptions::new().write(true).read(true).open(DEV_FUSE)?;
        Self::from_file(file)
    }

    pub fn from_file(file: File) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::io::FromRawFd;

        let fd = file.as_raw_fd();

        // Set non-blocking so the sqz_fdwatch-driven eventfd drains never park
        let flags = nix::fcntl::fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;

        let entries = fuse_uring_entries();
        let read_ring = build_io_uring(entries)?;
        let write_ring = build_io_uring(entries)?;

        // P2-8: register /dev/fuse (or cloned worker fd) as fixed file index 0.
        let read_use_fixed = try_register_fuse_fd(&read_ring, fd);
        let write_use_fixed = try_register_fuse_fd(&write_ring, fd);

        let read_event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if read_event_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let write_event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if write_event_fd < 0 {
            unsafe {
                libc::close(read_event_fd);
            }
            return Err(io::Error::last_os_error());
        }

        if let Err(e) = read_ring.submitter().register_eventfd(read_event_fd) {
            unsafe {
                libc::close(read_event_fd);
                libc::close(write_event_fd);
            }
            return Err(e);
        }

        if let Err(e) = write_ring.submitter().register_eventfd(write_event_fd) {
            unsafe {
                libc::close(read_event_fd);
                libc::close(write_event_fd);
            }
            return Err(e);
        }

        let read_ring_fd = unsafe { OwnedFd::from_raw_fd(read_event_fd) };
        let write_ring_fd = unsafe { OwnedFd::from_raw_fd(write_event_fd) };

        let read_ring = DebugUring(read_ring);
        let write_ring = DebugUring(write_ring);

        Ok(Self {
            file,
            read_ring: std::sync::Mutex::new(read_ring),
            read_ring_fd,
            write_ring: std::sync::Mutex::new(write_ring),
            write_ring_fd,
            read_use_fixed,
            write_use_fixed,
            read: Mutex::new(()),
            write: Mutex::new(()),
        })
    }

    async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        use std::os::fd::AsRawFd;
        let _guard = self.read.lock().await;

        let fd = self.file.as_raw_fd();
        let iovecs = [
            SendIovec(libc::iovec {
                iov_base: header_buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: header_buf.len(),
            }),
            SendIovec(libc::iovec {
                iov_base: data_buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: data_buf.len(),
            }),
        ];

        {
            let mut guard = self.read_ring.lock().unwrap();
            let ring = &mut guard.0;
            let read_e = if self.read_use_fixed {
                opcode::Readv::new(
                    types::Fixed(0),
                    iovecs.as_ptr() as *mut libc::iovec,
                    iovecs.len() as u32,
                )
                .build()
                .user_data(0x01)
            } else {
                opcode::Readv::new(
                    types::Fd(fd),
                    iovecs.as_ptr() as *mut libc::iovec,
                    iovecs.len() as u32,
                )
                .build()
                .user_data(0x01)
            };

            unsafe {
                ring.submission()
                    .push(&read_e)
                    .expect("Failed to push readv to io_uring");
            }

            ring.submit().expect("Failed to submit readv to io_uring");
        }

        let io_res = loop {
            let cqe = {
                let mut guard = self.read_ring.lock().unwrap();
                let x = guard.0.completion().next();
                x
            };
            if let Some(cqe) = cqe {
                if cqe.user_data() == 0x01 {
                    let res = cqe.result();
                    break if res < 0 {
                        Err(io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };
                }
            }

            // First-party fd watch (sqz_fdwatch): level-triggered per await,
            // so the nonblocking eventfd drain below plus re-await replaces
            // the old AsyncFd guard/clear_ready protocol 1:1 (a `Replaced`
            // event is treated as Ready — drain and CQ probe re-check).
            if let Err(err) = crate::sqz_fdwatch::readable(self.read_ring_fd.as_raw_fd()).await {
                break Err(err);
            }

            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.read_ring_fd.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        8,
                    )
                };
                if res < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    if err.kind() == io::ErrorKind::WouldBlock {
                        // Drained; the next `readable().await` re-arms.
                        break;
                    } else {
                        read_err = Some(err);
                        break;
                    }
                } else if res == 0 {
                    break;
                } else {
                    continue;
                }
            }

            if let Some(err) = read_err {
                break Err(err);
            }
        };

        ((header_buf, data_buf), io_res)
    }

    async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        use std::os::fd::AsRawFd;
        let _guard = self.write.lock().await;

        let fd = self.file.as_raw_fd();
        let body_extend_data_ref = body_extend_data.as_deref();

        let mut iovecs = Vec::with_capacity(2);
        iovecs.push(SendIovec(libc::iovec {
            iov_base: data.deref().as_ptr() as *mut libc::c_void,
            iov_len: data.deref().len(),
        }));

        if let Some(extend) = body_extend_data_ref {
            iovecs.push(SendIovec(libc::iovec {
                iov_base: extend.as_ptr() as *mut libc::c_void,
                iov_len: extend.len(),
            }));
        }

        {
            let mut guard = self.write_ring.lock().unwrap();
            let ring = &mut guard.0;
            let write_e = if self.write_use_fixed {
                opcode::Writev::new(
                    types::Fixed(0),
                    iovecs.as_ptr() as *mut libc::iovec,
                    iovecs.len() as u32,
                )
                .build()
                .user_data(0x01)
            } else {
                opcode::Writev::new(
                    types::Fd(fd),
                    iovecs.as_ptr() as *mut libc::iovec,
                    iovecs.len() as u32,
                )
                .build()
                .user_data(0x01)
            };

            unsafe {
                ring.submission()
                    .push(&write_e)
                    .expect("Failed to push writev to io_uring");
            }

            ring.submit().expect("Failed to submit writev to io_uring");
        }

        let io_res = loop {
            let cqe = {
                let mut guard = self.write_ring.lock().unwrap();
                let x = guard.0.completion().next();
                x
            };
            if let Some(cqe) = cqe {
                if cqe.user_data() == 0x01 {
                    let res = cqe.result();
                    break if res < 0 {
                        Err(io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };
                }
            }

            // First-party fd watch (sqz_fdwatch): level-triggered per await,
            // so the nonblocking eventfd drain below plus re-await replaces
            // the old AsyncFd guard/clear_ready protocol 1:1 (a `Replaced`
            // event is treated as Ready — drain and CQ probe re-check).
            if let Err(err) = crate::sqz_fdwatch::readable(self.write_ring_fd.as_raw_fd()).await {
                break Err(err);
            }

            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.write_ring_fd.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        8,
                    )
                };
                if res < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    if err.kind() == io::ErrorKind::WouldBlock {
                        // Drained; the next `readable().await` re-arms.
                        break;
                    } else {
                        read_err = Some(err);
                        break;
                    }
                } else if res == 0 {
                    break;
                } else {
                    continue;
                }
            }

            if let Some(err) = read_err {
                break Err(err);
            }
        };

        ((data, body_extend_data), io_res)
    }
}

#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd"
))]
#[derive(Debug)]
struct NonBlockFuseConnection {
    #[cfg(target_os = "freebsd")]
    fd: OwnedFd,
    #[cfg(target_os = "linux")]
    fd: OwnedFd,

    #[cfg(target_os = "linux")]
    read_ring: std::sync::Mutex<DebugUring>,
    #[cfg(target_os = "linux")]
    read_ring_fd: OwnedFd,

    #[cfg(target_os = "linux")]
    write_ring: std::sync::Mutex<DebugUring>,
    #[cfg(target_os = "linux")]
    write_ring_fd: OwnedFd,

    #[cfg(target_os = "linux")]
    read_use_fixed: bool,
    #[cfg(target_os = "linux")]
    write_use_fixed: bool,

    read: Mutex<()>,
    write: Mutex<()>,
}

#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd"
))]
impl NonBlockFuseConnection {
    #[cfg(target_os = "freebsd")]
    fn new() -> io::Result<Self> {
        const DEV_FUSE: &str = "/dev/fuse";

        match OpenOptions::new()
            .write(true)
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(DEV_FUSE)
        {
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    warn!("Cannot open /dev/fuse.  Is the module loaded?");
                }
                Err(e)
            }
            Ok(file) => Ok(Self {
                fd: file.into(),
                read: Mutex::new(()),
                write: Mutex::new(()),
            }),
        }
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    async fn new_with_unprivileged(
        mount_options: MountOptions,
        mount_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};

        let (sock0, sock1) = match socket::socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        ) {
            Err(err) => return Err(err.into()),

            Ok((sock0, sock1)) => (sock0, sock1),
        };

        let binary_path = find_fusermount3()?;

        const ENV: &str = "_FUSE_COMMFD";

        let options = mount_options.build_with_unprivileged();

        debug!("mount options {:?}", options);

        let mount_path = mount_path.as_ref().as_os_str().to_os_string();

        let fd0 = sock0.as_raw_fd();
        // std::process::Command spawn + wait on the blocking pool (the
        // fusermount3 subprocess is syscall-class work, not async I/O).
        let status = crate::sqz_blocking::run_blocking(move || {
            Command::new(binary_path)
                .env(ENV, fd0.to_string())
                .args(vec![OsString::from("-o"), options, mount_path])
                .status()
        })
        .await?;

        if !status.success() {
            return Err(io::Error::other("fusermount run failed"));
        }

        let fd1 = sock1.as_raw_fd();
        let fd = crate::sqz_blocking::run_blocking(move || {
            // let mut buf = vec![0; 10000]; // buf should large enough
            let mut buf = vec![]; // it seems 0 len still works well

            let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

            let mut bufs = [IoSliceMut::new(&mut buf)];

            let msg = match socket::recvmsg::<()>(
                fd1,
                &mut bufs[..],
                Some(&mut cmsg_buf),
                MsgFlags::empty(),
            ) {
                Err(err) => return Err(err.into()),

                Ok(msg) => msg,
            };

            let fd = if let Some(ControlMessageOwned::ScmRights(fds)) = msg.cmsgs()?.next() {
                if fds.is_empty() {
                    return Err(io::Error::other("no fuse fd"));
                }

                fds[0]
            } else {
                return Err(io::Error::other("get fuse fd failed"));
            };

            Ok(fd)
        })
        .await?;

        Self::set_fd_non_blocking(fd)?;

        // Safety: fd is valid
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let raw_fd = fd.as_raw_fd();
            let entries = fuse_uring_entries();
            let read_ring = build_io_uring(entries)?;
            let write_ring = build_io_uring(entries)?;

            let read_use_fixed = try_register_fuse_fd(&read_ring, raw_fd);
            let write_use_fixed = try_register_fuse_fd(&write_ring, raw_fd);

            let read_event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if read_event_fd < 0 {
                return Err(io::Error::last_os_error());
            }

            let write_event_fd =
                unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if write_event_fd < 0 {
                unsafe {
                    libc::close(read_event_fd);
                }
                return Err(io::Error::last_os_error());
            }

            if let Err(e) = read_ring.submitter().register_eventfd(read_event_fd) {
                unsafe {
                    libc::close(read_event_fd);
                    libc::close(write_event_fd);
                }
                return Err(e);
            }

            if let Err(e) = write_ring.submitter().register_eventfd(write_event_fd) {
                unsafe {
                    libc::close(read_event_fd);
                    libc::close(write_event_fd);
                }
                return Err(e);
            }

            let read_ring_fd = unsafe { OwnedFd::from_raw_fd(read_event_fd) };
            let write_ring_fd = unsafe { OwnedFd::from_raw_fd(write_event_fd) };

            let read_ring = DebugUring(read_ring);
            let write_ring = DebugUring(write_ring);

            Ok(Self {
                fd,
                read_ring: std::sync::Mutex::new(read_ring),
                read_ring_fd,
                write_ring: std::sync::Mutex::new(write_ring),
                write_ring_fd,
                read_use_fixed,
                write_use_fixed,
                read: Mutex::new(()),
                write: Mutex::new(()),
            })
        }

        #[cfg(target_os = "freebsd")]
        {
            Ok(Self {
                fd,
                read: Mutex::new(()),
                write: Mutex::new(()),
            })
        }
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    fn set_fd_non_blocking(fd: RawFd) -> io::Result<()> {
        let flags = nix::fcntl::fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;

        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;

        nix::fcntl::fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;

        Ok(())
    }

    #[cfg(target_os = "linux")]
    async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        use std::os::fd::AsRawFd;
        let _guard = self.read.lock().await;

        let fd = self.fd.as_raw_fd();
        let iovecs = [
            SendIovec(libc::iovec {
                iov_base: header_buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: header_buf.len(),
            }),
            SendIovec(libc::iovec {
                iov_base: data_buf.as_mut_ptr() as *mut libc::c_void,
                iov_len: data_buf.len(),
            }),
        ];

        {
            let mut guard = self.read_ring.lock().unwrap();
            let ring = &mut guard.0;
            let read_e = if self.read_use_fixed {
                opcode::Readv::new(
                    types::Fixed(0),
                    iovecs.as_ptr() as *mut libc::iovec,
                    iovecs.len() as u32,
                )
            } else {
                opcode::Readv::new(
                    types::Fd(fd),
                    iovecs.as_ptr() as *mut libc::iovec,
                    iovecs.len() as u32,
                )
            }
            .build()
            .user_data(0x01);

            unsafe {
                ring.submission()
                    .push(&read_e)
                    .expect("Failed to push readv to io_uring");
            }

            ring.submit().expect("Failed to submit readv to io_uring");
        }

        let io_res = loop {
            let cqe = {
                let mut guard = self.read_ring.lock().unwrap();
                let x = guard.0.completion().next();
                x
            };
            if let Some(cqe) = cqe {
                if cqe.user_data() == 0x01 {
                    let res = cqe.result();
                    break if res < 0 {
                        Err(io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };
                }
            }

            // First-party fd watch (sqz_fdwatch): level-triggered per await,
            // so the nonblocking eventfd drain below plus re-await replaces
            // the old AsyncFd guard/clear_ready protocol 1:1 (a `Replaced`
            // event is treated as Ready — drain and CQ probe re-check).
            if let Err(err) = crate::sqz_fdwatch::readable(self.read_ring_fd.as_raw_fd()).await {
                break Err(err);
            }

            // Loop reading eventfd until EAGAIN (WouldBlock) to ensure no lost wakeups
            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.read_ring_fd.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        8,
                    )
                };
                if res < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    if err.kind() == io::ErrorKind::WouldBlock {
                        // Drained; the next `readable().await` re-arms.
                        break;
                    } else {
                        read_err = Some(err);
                        break;
                    }
                } else if res == 0 {
                    break;
                } else {
                    continue;
                }
            }

            if let Some(err) = read_err {
                break Err(err);
            }
        };

        // Explicitly keep iovecs alive until completion of the I/O
        drop(iovecs);

        ((header_buf, data_buf), io_res)
    }

    #[cfg(target_os = "freebsd")]
    async fn read_vectored<T: DerefMut<Target = [u8]> + Send>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        let _guard = self.read.lock().await;

        loop {
            // First-party fd watch: await readability, then attempt the
            // nonblocking readv; WouldBlock re-awaits (the level-triggered
            // per-await registration replaces the AsyncFd guard/try_io
            // protocol; HUP/ERR surface as Ready and the readv reports).
            if let Err(err) = {
                use std::os::fd::AsRawFd;
                crate::sqz_fdwatch::readable(self.fd.as_raw_fd()).await
            } {
                return ((header_buf, data_buf), Err(err));
            }

            let result = uio::readv(
                &self.fd,
                &mut [
                    IoSliceMut::new(&mut header_buf),
                    IoSliceMut::new(&mut data_buf),
                ],
            )
            .map_err(io::Error::from);
            match result {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                other => return ((header_buf, data_buf), other),
            }
        }
    }

    #[cfg(target_os = "linux")]
    async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        use std::os::fd::AsRawFd;
        let _guard = self.write.lock().await;

        let fd = self.fd.as_raw_fd();
        let mut iovecs = [
            SendIovec(libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            }),
            SendIovec(libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            }),
        ];
        let num_iovecs = if let Some(ref ext) = body_extend_data {
            iovecs[0] = SendIovec(libc::iovec {
                iov_base: data.as_ptr() as *mut libc::c_void,
                iov_len: data.len(),
            });
            iovecs[1] = SendIovec(libc::iovec {
                iov_base: ext.as_ptr() as *mut libc::c_void,
                iov_len: ext.len(),
            });
            2
        } else {
            iovecs[0] = SendIovec(libc::iovec {
                iov_base: data.as_ptr() as *mut libc::c_void,
                iov_len: data.len(),
            });
            1
        };

        {
            let mut guard = self.write_ring.lock().unwrap();
            let ring = &mut guard.0;
            let write_e = if self.write_use_fixed {
                opcode::Writev::new(
                    types::Fixed(0),
                    iovecs.as_ptr() as *const libc::iovec,
                    num_iovecs as u32,
                )
            } else {
                opcode::Writev::new(
                    types::Fd(fd),
                    iovecs.as_ptr() as *const libc::iovec,
                    num_iovecs as u32,
                )
            }
            .build()
            .user_data(0x02);

            unsafe {
                ring.submission()
                    .push(&write_e)
                    .expect("Failed to push writev to io_uring");
            }

            ring.submit().expect("Failed to submit writev to io_uring");
        }

        let io_res = loop {
            let cqe = {
                let mut guard = self.write_ring.lock().unwrap();
                let x = guard.0.completion().next();
                x
            };
            if let Some(cqe) = cqe {
                if cqe.user_data() == 0x02 {
                    let res = cqe.result();
                    break if res < 0 {
                        Err(io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };
                }
            }

            // First-party fd watch (sqz_fdwatch): level-triggered per await,
            // so the nonblocking eventfd drain below plus re-await replaces
            // the old AsyncFd guard/clear_ready protocol 1:1 (a `Replaced`
            // event is treated as Ready — drain and CQ probe re-check).
            if let Err(err) = crate::sqz_fdwatch::readable(self.write_ring_fd.as_raw_fd()).await {
                break Err(err);
            }

            // Loop reading eventfd until EAGAIN (WouldBlock) to ensure no lost wakeups
            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.write_ring_fd.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        8,
                    )
                };
                if res < 0 {
                    let err = io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    if err.kind() == io::ErrorKind::WouldBlock {
                        // Drained; the next `readable().await` re-arms.
                        break;
                    } else {
                        read_err = Some(err);
                        break;
                    }
                } else if res == 0 {
                    break;
                } else {
                    continue;
                }
            }

            if let Some(err) = read_err {
                break Err(err);
            }
        };

        // Explicitly keep iovecs alive until completion of the I/O
        drop(iovecs);

        ((data, body_extend_data), io_res)
    }

    #[cfg(target_os = "freebsd")]
    async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        let _guard = self.write.lock().await;

        let res = {
            let body_extend_data = body_extend_data.as_deref();

            match body_extend_data {
                None => uio::writev(&self.fd, &[IoSlice::new(data.deref())]),

                Some(body_extend_data) => uio::writev(
                    &self.fd,
                    &[IoSlice::new(data.deref()), IoSlice::new(body_extend_data)],
                ),
            }
        };

        match res {
            Err(err) => ((data, body_extend_data), Err(err.into())),
            Ok(n) => ((data, body_extend_data), Ok(n)),
        }
    }
}

impl AsFd for FuseConnection {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.mode {
            #[cfg(target_os = "linux")]
            ConnectionMode::Block(connection) => connection.file.as_fd(),

            #[cfg(any(
                all(target_os = "linux", feature = "unprivileged"),
                target_os = "freebsd"
            ))]
            ConnectionMode::NonBlock(connection) => connection.fd.as_fd(),
        }
    }
}

/// PERF-2 (pre-rc spec §9) — the session pool slot's contract, pinned
/// BEFORE the mutex→lock-free swap so the swap is provably semantics-
/// preserving. The slot is taken 4× per READ on the request path
/// (`inner_read_vectored`, `write_vectored`, `get_payload_buffer`,
/// `over_uring_ready`) while it is only ever WRITTEN twice per session
/// (install after INIT, teardown at disconnect/DESTROY). The contract:
///
/// 1. **Install-once**: at most one pool per connection family, first
///    caller wins, losers get their pool back and own its teardown.
/// 2. **Liveness rides the pool's atomics, never slot emptiness**: venue
///    reads gate on `is_ready()`/`is_active()`; after teardown the
///    installed pool stays observable (shut down) so a lock-free venue
///    probe can never race a slot tear-out. Post-teardown routing is
///    identical either way — `ready == false` ⇒ classical reply venue,
///    `active == false` ⇒ dispatch pulls fail loud (NotConnected).
/// 3. **Teardown is idempotent**: every worker clone may call it on the
///    disconnect path; `FuseOverUring::shutdown` gates side effects on
///    the `active` swap.
///
/// Sim venue: [`FuseOverUring::sim_inert`] — the shipped liveness
/// machinery with no kernel session (the `KmbufQueue::sim_anon`
/// precedent).
#[cfg(all(test, target_os = "linux"))]
mod over_uring_slot_tests {
    use super::super::fuse_over_uring::FuseOverUring;
    use super::FuseConnection;
    use async_notify::Notify;
    use std::sync::Arc;

    fn conn() -> FuseConnection {
        FuseConnection::new(Arc::new(Notify::new()))
            .expect("/dev/fuse (0666) + io_uring must be available on the test box")
    }

    /// Empty slot (pre-INIT): every venue read routes classical and no
    /// payload buffer resolves — the arm-window posture.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_slot_routes_classical() {
        let c = conn();
        assert!(
            !c.over_uring_ready(),
            "no pool installed — reply venue must be classical"
        );
        assert_eq!(c.num_uring_queues(), None, "no pool — no queue geometry");
        assert_eq!(
            c.get_payload_buffer(crate::raw::ReplySlot::Ring {
                qid: 0,
                ent_idx: 7,
                commit_id: 7,
            }),
            None,
            "no pool — no payload buffer can resolve"
        );
    }

    /// Contract 1: the second install is refused and the caller gets its
    /// pool back untouched (it owns the teardown); the first pool's
    /// geometry stays the slot's answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_once_second_pool_refused() {
        let c = conn();
        let winner = FuseOverUring::sim_inert(3);
        let loser = FuseOverUring::sim_inert(5);
        assert!(
            c.install_over_uring(winner).is_ok(),
            "first install must win"
        );
        let returned = c
            .install_over_uring(loser)
            .expect_err("second install must be refused");
        assert_eq!(
            returned.nqueues, 5,
            "the refused install must hand the LOSER back (caller owns its teardown)"
        );
        assert_eq!(
            c.num_uring_queues(),
            Some(3),
            "the slot must keep answering with the winner's geometry"
        );
        returned.shutdown();
    }

    /// Contract 1 under a real race: N concurrent installs admit exactly
    /// one pool, every loser is returned, and every observer agrees on
    /// the winner's geometry afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_installs_admit_exactly_one_pool() {
        let c = conn();
        let mut wins = 0usize;
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8u16)
                .map(|i| {
                    let c = &c;
                    s.spawn(move || {
                        let pool = FuseOverUring::sim_inert(i + 1);
                        match c.install_over_uring(pool) {
                            Ok(()) => None,
                            Err(loser) => {
                                let n = loser.nqueues;
                                loser.shutdown();
                                Some(n)
                            }
                        }
                    })
                })
                .collect();
            let mut losers = Vec::new();
            for h in handles {
                match h.join().expect("installer thread") {
                    None => wins += 1,
                    Some(n) => losers.push(n),
                }
            }
            assert_eq!(wins, 1, "exactly one install may win");
            assert_eq!(losers.len(), 7, "every loser must be handed back");
        });
        let winner_nq = c.num_uring_queues().expect("winner installed") as u16;
        assert!(
            (1..=8).contains(&winner_nq),
            "the observed geometry must be one racer's pool"
        );
    }

    /// Contract 2, live side: the reply venue follows the pool's own
    /// ready/active word — pre-ready classical, ready over-uring, shut
    /// down classical again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reply_venue_follows_pool_liveness() {
        let c = conn();
        let pool = FuseOverUring::sim_inert(2);
        assert!(
            c.install_over_uring(Arc::clone(&pool)).is_ok(),
            "first install must win"
        );
        assert!(
            !c.over_uring_ready(),
            "installed-but-not-ready must stay classical (the INIT→REGISTER window)"
        );
        pool.mark_ready();
        assert!(c.over_uring_ready(), "ready pool arms the uring venue");
        pool.shutdown();
        assert!(
            !c.over_uring_ready(),
            "shutdown pool must route classical again"
        );
    }

    /// Contract 2 + 3, teardown side: after `teardown_over_uring` the
    /// pool is DEAD by its own atomics but STAYS observable through the
    /// slot — liveness is never derived from slot emptiness, so lock-free
    /// venue reads cannot race a tear-out. Teardown is idempotent, and
    /// the slot never reopens for a second install.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn teardown_keeps_pool_reachable_for_liveness_routing() {
        let c = conn();
        let pool = FuseOverUring::sim_inert(3);
        assert!(
            c.install_over_uring(Arc::clone(&pool)).is_ok(),
            "first install must win"
        );
        pool.mark_ready();

        c.teardown_over_uring();
        assert!(!pool.is_ready(), "teardown must drop ready");
        assert!(!pool.is_active(), "teardown must drop active");
        assert!(
            !c.over_uring_ready(),
            "post-teardown reply venue must be classical"
        );
        assert_eq!(
            c.num_uring_queues(),
            Some(3),
            "teardown must NOT empty the slot — liveness rides the pool's \
             ready/active atomics, never slot emptiness (contract 2)"
        );

        // Contract 3: every worker clone may race the teardown path.
        c.teardown_over_uring();
        assert_eq!(
            c.num_uring_queues(),
            Some(3),
            "teardown must stay idempotent and non-emptying"
        );

        // Install-once is for the CONNECTION's lifetime: teardown never
        // reopens the slot (no live path re-installs after teardown; a
        // late enable must not resurrect a dead session's transport).
        let late = FuseOverUring::sim_inert(4);
        let returned = c
            .install_over_uring(late)
            .expect_err("teardown must not reopen the install-once slot");
        returned.shutdown();
    }
}

struct UringBufOwner {
    ptr: *const u8,
    len: usize,
}
unsafe impl Send for UringBufOwner {}
unsafe impl Sync for UringBufOwner {}
impl AsRef<[u8]> for UringBufOwner {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl Drop for UringBufOwner {
    fn drop(&mut self) {}
}
