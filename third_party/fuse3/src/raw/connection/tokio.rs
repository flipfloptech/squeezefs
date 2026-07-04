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
            tracing::debug!(
                "fuse3: register_files(/dev/fuse) failed ({e:?}); using types::Fd"
            );
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


#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd"
))]
use std::io::ErrorKind;

#[cfg(target_os = "freebsd")]
use std::io::IoSlice;

#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd"
))]
use std::io::IoSliceMut;

use std::ops::{Deref, DerefMut};
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd"
))]
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
#[cfg(target_os = "freebsd")]
use nix::sys::uio;
#[cfg(target_os = "linux")]
use nix::fcntl::{FcntlArg, OFlag};
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use nix::sys::socket::{self, AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType};
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd"
))]
use tokio::io::{unix::AsyncFd, Interest};
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use tokio::process::Command;
#[cfg(target_os = "linux")]
use tokio::task;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use tracing::debug;
#[cfg(target_os = "freebsd")]
use tracing::warn;

use super::CompleteIoResult;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use crate::find_fusermount3;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use crate::MountOptions;

pub struct FuseConnection {
    unmount_notify: Arc<Notify>,
    mode: ConnectionMode,
    pub(crate) splice_read: std::sync::atomic::AtomicBool,
    pub(crate) splice_write: std::sync::atomic::AtomicBool,
    /// Optional kernel FUSE-over-io_uring pool (Linux 6.14+). Shared across multi-queue clones.
    #[cfg(target_os = "linux")]
    pub(crate) over_uring: std::sync::Arc<
        std::sync::Mutex<Option<std::sync::Arc<super::fuse_over_uring::FuseOverUring>>>,
    >,
    /// Uniques delivered via classical `/dev/fuse` (INIT + REGISTER handoff). Replies
    /// for these must use classical write even after the uring pool is armed.
    #[cfg(target_os = "linux")]
    classical_inflight: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
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
                splice_read: std::sync::atomic::AtomicBool::new(false),
                splice_write: std::sync::atomic::AtomicBool::new(false),
            })
        }

        #[cfg(target_os = "linux")]
        {
            let connection = BlockFuseConnection::new()?;

            Ok(Self {
                unmount_notify,
                mode: ConnectionMode::Block(connection),
                splice_read: std::sync::atomic::AtomicBool::new(false),
                splice_write: std::sync::atomic::AtomicBool::new(false),
                over_uring: std::sync::Arc::new(std::sync::Mutex::new(None)),
                classical_inflight: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::HashSet::new(),
                )),
            })
        }
    }

    /// Start kernel FUSE-over-io_uring workers after FUSE_INIT. Required transport.
    /// Shared with multi-queue clones via [`clone_connection`].
    #[cfg(target_os = "linux")]
    pub fn enable_fuse_over_uring(&self, max_write: usize) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // Already enabled (e.g. race with another enable call)
        if self.over_uring.lock().unwrap().is_some() {
            return Ok(());
        }
        let fd = self.as_fd().as_raw_fd();
        let pool = super::fuse_over_uring::FuseOverUring::try_start(fd, max_write).map_err(|e| {
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
            splice_read: std::sync::atomic::AtomicBool::new(false),
            splice_write: std::sync::atomic::AtomicBool::new(false),
            over_uring: std::sync::Arc::new(std::sync::Mutex::new(None)),
            classical_inflight: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
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
                    splice_read: std::sync::atomic::AtomicBool::new(self.splice_read.load(std::sync::atomic::Ordering::Relaxed)),
                    splice_write: std::sync::atomic::AtomicBool::new(self.splice_write.load(std::sync::atomic::Ordering::Relaxed)),
                    // Share over-uring pool so multi-queue session workers pull the same inbound queue.
                    over_uring: self.over_uring.clone(),
                    classical_inflight: self.classical_inflight.clone(),
                })
            }
            #[cfg(feature = "unprivileged")]
            ConnectionMode::NonBlock(_) => {
                use std::os::unix::fs::OpenOptionsExt;
                use std::os::fd::{AsRawFd, FromRawFd};
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

                let fd = file.into(); // OwnedFd
                let read_ring = build_io_uring(256)?;
                let write_ring = build_io_uring(256)?;

                let read_event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                if read_event_fd < 0 {
                    return Err(io::Error::last_os_error());
                }

                let write_event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                if write_event_fd < 0 {
                    unsafe { libc::close(read_event_fd); }
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
                    read: Mutex::new(()),
                    write: Mutex::new(()),
                };

                Ok(Self {
                    unmount_notify: self.unmount_notify.clone(),
                    mode: ConnectionMode::NonBlock(connection),
                    splice_read: std::sync::atomic::AtomicBool::new(self.splice_read.load(std::sync::atomic::Ordering::Relaxed)),
                    splice_write: std::sync::atomic::AtomicBool::new(self.splice_write.load(std::sync::atomic::Ordering::Relaxed)),
                    // Share over-uring pool so multi-queue session workers pull the same inbound queue.
                    over_uring: self.over_uring.clone(),
                    classical_inflight: self.classical_inflight.clone(),
                })
            }
            #[cfg(not(feature = "unprivileged"))]
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Cloning non-blocking connections is not supported",
            )),
        }
    }

    pub async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        header_buf: Vec<u8>,
        data_buf: T,
    ) -> Option<CompleteIoResult<(Vec<u8>, T), usize>> {
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
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        // After arm, the request path is FUSE-over-io_uring only — never classical.
        // Pre-arm (during INIT only) still uses /dev/fuse because the kernel rejects
        // REGISTER until fch->initialized.
        #[cfg(target_os = "linux")]
        {
            let pool = self.over_uring.lock().unwrap().clone();
            // After arm: uring-only. Before arm / when inactive: fall through.
            // - ready → drain uring inbound
            // - shut down (!active) → disconnect error
            // - not yet ready → classical (INIT only; REGISTER wait is inside enable)
            if let Some(pool) = pool.filter(|p| p.is_ready() || !p.is_active()) {
                if !pool.is_active() {
                    return (
                        (header_buf, data_buf),
                        Err(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "fuse-over-uring inactive (unmounted or aborted)",
                        )),
                    );
                }
                let pool2 = pool.clone();
                let inbound = match tokio::task::spawn_blocking(move || {
                    // Block until a request or inactivity timeout (retry while active).
                    loop {
                        if let Some(r) =
                            pool2.recv_inbound_timeout(std::time::Duration::from_millis(200))
                        {
                            return Ok(r);
                        }
                        if !pool2.is_active() {
                            return Err(io::Error::new(
                                io::ErrorKind::NotConnected,
                                "fuse-over-uring inactive (unmounted or aborted)",
                            ));
                        }
                    }
                })
                .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return ((header_buf, data_buf), Err(e)),
                    Err(e) => {
                        return (
                            (header_buf, data_buf),
                            Err(io::Error::other(format!("spawn_blocking: {e}"))),
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
                        (header_buf, data_buf),
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "short fuse-over-uring header",
                        )),
                    );
                }
                header_buf[..40].copy_from_slice(&inbound.header_and_op[..40]);
                let total_len =
                    u32::from_le_bytes(header_buf[0..4].try_into().unwrap()) as usize;
                let body_need = total_len.saturating_sub(40);
                let op_in = &inbound.header_and_op[40..];
                let payload = &inbound.payload;
                // Bytes of body that live in op_in (first in_arg); remainder in payload.
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
                return ((header_buf, data_buf), Ok(40 + filled));
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
        // Track unique so a reply that races mark_ready still uses classical write.
        #[cfg(target_os = "linux")]
        if let ((ref hdr, _), Ok(n)) = &result {
            if *n >= 16 {
                let unique = u64::from_le_bytes(hdr[8..16].try_into().unwrap_or([0; 8]));
                if unique != 0 {
                    self.classical_inflight.lock().unwrap().insert(unique);
                }
            }
        }
        result
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
                let mut reply = bytes::BytesMut::with_capacity(
                    data.deref().len()
                        + body_extend_data
                            .as_ref()
                            .map(|b| b.deref().len())
                            .unwrap_or(0),
                );
                reply.extend_from_slice(data.deref());
                if let Some(ref ext) = body_extend_data {
                    reply.extend_from_slice(ext.deref());
                }
                let reply = reply.freeze();
                // unique is at offset 8 in fuse_out_header (len u32, error i32, unique u64)
                let unique = if reply.len() >= 16 {
                    u64::from_le_bytes(reply[8..16].try_into().unwrap())
                } else {
                    0
                };
                // Notifications (unique==0) are not supported on over-uring yet.
                if unique == 0 {
                    return (
                        (data, body_extend_data),
                        Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "fuse notify not supported on FUSE-over-io_uring path",
                        )),
                    );
                }
                // Classical-delivered handoff requests must not use COMMIT.
                let is_classical = self
                    .classical_inflight
                    .lock()
                    .unwrap()
                    .remove(&unique);
                if is_classical {
                    // Fall through to classical write below.
                } else {
                    let len = reply.len();
                    match pool.submit_reply(unique, reply) {
                        Ok(()) => return ((data, body_extend_data), Ok(len)),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            // Double-reply or auto-COMMITed FORGET — do not classical-write
                            // (that path has stalled the single reply task under load).
                            debug!(
                                unique,
                                "fuse-over-uring COMMIT miss; drop (no classical fallback)"
                            );
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
            unsafe { libc::close(read_event_fd); }
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
                if e.kind() == ErrorKind::NotFound {
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
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "fusermount run failed",
            ));
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
                    return Err(io::Error::new(ErrorKind::Other, "no fuse fd"));
                }

                fds[0]
            } else {
                return Err(io::Error::new(ErrorKind::Other, "get fuse fd failed"));
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
            let read_ring = build_io_uring(256)?;
            let write_ring = build_io_uring(256)?;

            let read_event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if read_event_fd < 0 {
                return Err(io::Error::last_os_error());
            }

            let write_event_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if write_event_fd < 0 {
                unsafe { libc::close(read_event_fd); }
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
            let read_e = opcode::Readv::new(
                types::Fd(fd),
                iovecs.as_ptr() as *mut libc::iovec,
                iovecs.len() as u32,
            )
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
            let write_e = opcode::Writev::new(
                types::Fd(fd),
                iovecs.as_ptr() as *const libc::iovec,
                num_iovecs as u32,
            )
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
