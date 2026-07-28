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
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use tokio::io::{unix::AsyncFd, Interest};
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use tokio::process::Command;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use tokio::task;
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

/// Uniques delivered via classical `/dev/fuse` (INIT, the REGISTER
/// handoff window, the post-arm classical sideband). Replies for these
/// must use classical write even after the uring pool is armed.
///
/// Per-op economy (P2): on an armed steady-state session this set is
/// (almost always) EMPTY — only sideband stragglers enter it — yet every
/// reply paid the global mutex + hash probe. `len` (bumped AFTER a
/// successful insert, decremented AFTER a successful remove) gates the
/// probe: a reply observing `len == 0` may skip the lock because a
/// unique's own classical insert is ordered strictly before its reply
/// (delivery → session dispatch → handler → reply crosses synchronized
/// channel/task edges, so the bump is visible by reply time); other
/// uniques' racing entries are irrelevant to this unique's verdict — the
/// set is keyed by unique and only the owner ever removes its entry.
#[cfg(target_os = "linux")]
struct ClassicalInflight {
    len: std::sync::atomic::AtomicUsize,
    set: std::sync::Mutex<std::collections::HashSet<u64>>,
}

#[cfg(target_os = "linux")]
impl ClassicalInflight {
    fn new() -> Self {
        Self {
            len: std::sync::atomic::AtomicUsize::new(0),
            set: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    fn insert(&self, unique: u64) {
        if self.set.lock().unwrap().insert(unique) {
            self.len.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    fn remove(&self, unique: u64) -> bool {
        if self.len.load(std::sync::atomic::Ordering::Acquire) == 0 {
            return false;
        }
        let removed = self.set.lock().unwrap().remove(&unique);
        if removed {
            self.len.fetch_sub(1, std::sync::atomic::Ordering::Release);
        }
        removed
    }
}

pub struct FuseConnection {
    unmount_notify: Arc<Notify>,
    mode: ConnectionMode,
    /// Optional kernel FUSE-over-io_uring pool (Linux 6.14+). Shared across multi-queue clones.
    #[cfg(target_os = "linux")]
    pub(crate) over_uring: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<super::fuse_over_uring::FuseOverUring>>>,
    >,
    /// Uniques delivered via classical `/dev/fuse` (INIT, the REGISTER handoff,
    /// and the post-arm classical sideband). Replies for these must use
    /// classical write even after the uring pool is armed.
    #[cfg(target_os = "linux")]
    classical_inflight: std::sync::Arc<ClassicalInflight>,
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
                over_uring: std::sync::Arc::new(std::sync::Mutex::new(None)),
                classical_inflight: std::sync::Arc::new(ClassicalInflight::new()),
                assigned_qid: None,
                classical_sideband: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    #[cfg(target_os = "linux")]
    pub fn num_uring_queues(&self) -> Option<usize> {
        self.over_uring
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| p.nqueues as usize)
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
        if self.over_uring.lock().unwrap().is_some() {
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
        *self.over_uring.lock().unwrap() = Some(pool);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn get_payload_buffer(&self, unique: u64) -> Option<(u64, usize)> {
        // Plain uncontended mutex, DELIBERATELY not arc-swap (P2 md-storm
        // forensics): arc_swap loads/stores share one process-global debt
        // registry, so high-rate guard traffic from the transport threads
        // lengthened every kv node-snapshot `store()`'s pay_all walk on
        // the serialized conveyor path (−10% create/del storms). The
        // contended per-op lock was `pending` (now sharded); this one is
        // uncontended.
        let pool = self.over_uring.lock().unwrap().clone()?;
        pool.get_payload_buffer(unique)
    }

    /// True once the FUSE-over-io_uring pool is armed and ready — the
    /// gate for in-place replies (P2 per-op economy): an armed session's
    /// reply is a synchronous COMMIT enqueue (`submit_reply`), safe to
    /// run from the handler task itself instead of paying an unbounded-
    /// channel hop + reply-task wake per op.
    #[cfg(target_os = "linux")]
    pub fn over_uring_ready(&self) -> bool {
        self.over_uring
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.is_ready())
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
            over_uring: std::sync::Arc::new(std::sync::Mutex::new(None)),
            classical_inflight: std::sync::Arc::new(ClassicalInflight::new()),
            assigned_qid: None,
            classical_sideband: std::sync::atomic::AtomicBool::new(false),
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
                    classical_inflight: self.classical_inflight.clone(),
                    assigned_qid: None,
                    classical_sideband: std::sync::atomic::AtomicBool::new(false),
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

                let read_ring_fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(read_event_fd) })?;
                let write_ring_fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(write_event_fd) })?;

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
                    classical_inflight: self.classical_inflight.clone(),
                    assigned_qid: None,
                    classical_sideband: std::sync::atomic::AtomicBool::new(false),
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

    pub async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        header_buf: Vec<u8>,
        data_buf: T,
    ) -> Option<((Vec<u8>, T, Option<Bytes>), io::Result<usize>)> {
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
    ) -> ((Vec<u8>, T, Option<Bytes>), io::Result<usize>) {
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
                self.over_uring.lock().unwrap().clone()
            };
            // After arm: uring-only. Before arm / when inactive: fall through.
            // - ready → drain uring inbound
            // - shut down (!active) → disconnect error
            // - not yet ready → classical (INIT only; REGISTER wait is inside enable)
            if let Some(pool) = pool.filter(|p| p.is_ready() || !p.is_active()) {
                let pool2 = pool.clone();
                let qid = self.assigned_qid.unwrap_or(0);
                // Pure event-driven pull (L3 lever C): parks on the queue
                // channel / shutdown notify — no 200 ms poll cadence, no
                // per-pull timer registration. `None` means the pool shut
                // down (or a structurally-unreachable qid): fail loud so the
                // session worker exits, never poll-park.
                let inbound = match pool2.recv_inbound(qid).await {
                    Some(r) => r,
                    None => {
                        return (
                            (header_buf, data_buf, None),
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
                        (header_buf, data_buf, None),
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
                if opcode == crate::raw::abi::fuse_opcode::FUSE_WRITE as u32
                    && body_need >= FUSE_WRITE_IN_SIZE
                    && !payload.is_empty()
                {
                    let n = FUSE_WRITE_IN_SIZE.min(op_in.len()).min(data_buf.len());
                    data_buf[..n].copy_from_slice(&op_in[..n]);
                    return ((header_buf, data_buf, Some(inbound.payload)), Ok(40 + n));
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
                    (header_buf, data_buf, Some(inbound.payload)),
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
        // Track unique so the reply uses classical write (both the mark_ready
        // race window and the post-arm sideband). FORGET/BATCH_FORGET carry a
        // unique but are never replied to — tracking them would leak the set.
        #[cfg(target_os = "linux")]
        if let ((ref hdr, _), Ok(n)) = &result {
            if *n >= 16 {
                let unique = u64::from_le_bytes(hdr[8..16].try_into().unwrap_or([0; 8]));
                let op = u32::from_le_bytes(hdr[4..8].try_into().unwrap_or([0; 4]));
                if self
                    .classical_sideband
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    super::fuse_over_uring::note_classical_sideband();
                }
                if super::fuse_over_uring::transport_debug() {
                    eprintln!("[XPORT] classical-deliver unique={unique} op={op}");
                }
                // FUSE_FORGET = 2, FUSE_BATCH_FORGET = 42: no reply exists.
                if unique != 0 && !matches!(op, 2 | 42) {
                    self.classical_inflight.insert(unique);
                }
            }
        }
        let (buffers, res) = result;
        let (hdr, data) = buffers;
        ((hdr, data, None), res)
    }

    pub async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        // After arm: uring-delivered requests reply via COMMIT_AND_FETCH.
        // Requests that were still on the classical device queue during the
        // REGISTER handoff must be completed with a classical write — if we only
        // try COMMIT they miss the pending map, stay in kernel `waiting`, and
        // plain `umount` returns EBUSY forever.
        #[cfg(target_os = "linux")]
        {
            let pool = self.over_uring.lock().unwrap().clone();
            if let Some(pool) = pool.filter(|p| p.is_ready()) {
                // unique is at offset 8 in fuse_out_header (len u32, error i32, unique u64)
                let unique = if data.deref().len() >= 16 {
                    u64::from_le_bytes(data.deref()[8..16].try_into().unwrap())
                } else {
                    0
                };
                // Notifications (unique == 0, e.g. FUSE_NOTIFY_INVAL_INODE
                // for the L4 W1 handoff) have NO over-uring mechanism: the
                // kernel's COMMIT protocol keys on a request unique, and
                // notifies are daemon-initiated. They ride the classical
                // device write below — the same kernel-mandated classical
                // sideband the post-arm FORGET/INTERRUPT traffic uses,
                // never a hot-path fallback. (Pre-L4-6 this arm errored
                // Unsupported, which the session loop treated as FATAL —
                // the first live notify on an armed session killed the
                // mount; the gate's notify-delivery row pins the fix.)
                let is_classical = unique == 0 || self.classical_inflight.remove(unique);
                if is_classical {
                    if super::fuse_over_uring::transport_debug() {
                        eprintln!("[XPORT] classical-reply unique={unique}");
                    }
                    // Fall through to classical write below.
                } else {
                    let body_bytes = if let Some(ref ext) = body_extend_data {
                        let slice = ext.deref();
                        let dest_addr = pool
                            .get_payload_buffer(unique)
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
                    match pool.submit_reply(unique, data.deref().to_vec(), body_bytes) {
                        Ok(()) => return ((data, body_extend_data), Ok(len)),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            // Double-reply or auto-COMMITed FORGET — do not classical-write
                            // (that path has stalled the single reply task under load).
                            debug!(
                                unique,
                                "fuse-over-uring COMMIT miss; drop (no classical fallback)"
                            );
                            if super::fuse_over_uring::transport_debug() {
                                eprintln!("[XPORT] reply-dropped-notfound unique={unique}");
                            }
                            return ((data, body_extend_data), Ok(len));
                        }
                        Err(e) => return ((data, body_extend_data), Err(e)),
                    }
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
    read_ring_fd: AsyncFd<OwnedFd>,
    write_ring: std::sync::Mutex<DebugUring>,
    write_ring_fd: AsyncFd<OwnedFd>,
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

        // Set non-blocking to allow AsyncFd polling on eventfd completions
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

        let read_ring_fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(read_event_fd) })?;
        let write_ring_fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(write_event_fd) })?;

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

            let mut fd_guard = match self.read_ring_fd.ready(Interest::READABLE).await {
                Err(err) => break Err(err),
                Ok(guard) => guard,
            };

            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.read_ring_fd.get_ref().as_raw_fd(),
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
                        fd_guard.clear_ready();
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

            let mut fd_guard = match self.write_ring_fd.ready(Interest::READABLE).await {
                Err(err) => break Err(err),
                Ok(guard) => guard,
            };

            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.write_ring_fd.get_ref().as_raw_fd(),
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
                        fd_guard.clear_ready();
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
    fd: AsyncFd<OwnedFd>,
    #[cfg(target_os = "linux")]
    fd: OwnedFd,

    #[cfg(target_os = "linux")]
    read_ring: std::sync::Mutex<DebugUring>,
    #[cfg(target_os = "linux")]
    read_ring_fd: AsyncFd<OwnedFd>,

    #[cfg(target_os = "linux")]
    write_ring: std::sync::Mutex<DebugUring>,
    #[cfg(target_os = "linux")]
    write_ring_fd: AsyncFd<OwnedFd>,

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
                fd: AsyncFd::new(file.into())?,
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
        let mut child = Command::new(binary_path)
            .env(ENV, fd0.to_string())
            .args(vec![OsString::from("-o"), options, mount_path])
            .spawn()?;

        if !child.wait().await?.success() {
            return Err(io::Error::other("fusermount run failed"));
        }

        let fd1 = sock1.as_raw_fd();
        let fd = task::spawn_blocking(move || {
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
        .await
        .unwrap()?;

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

            let read_ring_fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(read_event_fd) })?;
            let write_ring_fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(write_event_fd) })?;

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
                fd: AsyncFd::new(fd)?,
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

            let mut fd_guard = match self.read_ring_fd.ready(Interest::READABLE).await {
                Err(err) => break Err(err),
                Ok(guard) => guard,
            };

            // Loop reading eventfd until EAGAIN (WouldBlock) to ensure no lost wakeups
            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.read_ring_fd.get_ref().as_raw_fd(),
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
                        fd_guard.clear_ready();
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
            let mut read_guard = match self.fd.ready(Interest::READABLE | Interest::ERROR).await {
                Err(err) => return ((header_buf, data_buf), Err(err)),
                Ok(read_guard) => read_guard,
            };

            if let Ok(result) = read_guard.try_io(|fd| {
                uio::readv(
                    fd,
                    &mut [
                        IoSliceMut::new(&mut header_buf),
                        IoSliceMut::new(&mut data_buf),
                    ],
                )
                .map_err(io::Error::from)
            }) {
                return ((header_buf, data_buf), result);
            } else {
                continue;
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

            let mut fd_guard = match self.write_ring_fd.ready(Interest::READABLE).await {
                Err(err) => break Err(err),
                Ok(guard) => guard,
            };

            // Loop reading eventfd until EAGAIN (WouldBlock) to ensure no lost wakeups
            let mut read_err = None;
            loop {
                let mut buf = [0u8; 8];
                let res = unsafe {
                    libc::read(
                        self.write_ring_fd.get_ref().as_raw_fd(),
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
                        fd_guard.clear_ready();
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
