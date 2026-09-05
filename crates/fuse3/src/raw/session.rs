use bytes::Bytes;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::mem;
use std::num::NonZeroU32;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use bincode::Options;
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_util::future::{Either, FutureExt};
use futures_util::select;
use futures_util::stream::StreamExt;
use nix::mount;
use nix::mount::MntFlags;
#[cfg(all(
    target_os = "linux",
    feature = "tokio-runtime",
    feature = "unprivileged"
))]
use std::process::Command;
use tracing::{debug, debug_span, error, instrument, warn, Instrument, Span};

#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use crate::find_fusermount3;
use crate::helper::*;
use crate::notify::Notify;
use crate::raw::abi::*;
#[cfg(feature = "tokio-runtime")]
use crate::raw::connection::FuseConnection;
use crate::raw::filesystem::Filesystem;
use crate::raw::reply::ReplyXAttr;
use crate::raw::request::{ReplySlot, Request};
use crate::raw::{FuseData, FuseReply};
use crate::MountOptions;
use crate::{Errno, SetAttr};

/// The session half of the **exactly-one-reply invariant** (spec FUSE-2):
/// the per-request reply handle every handler owns.
///
/// The transport's slot state machine guarantees that a delivered ring
/// slot leaves `Delivered` only by a submitted commit; this type
/// guarantees the *handler* side of the same law — that a commit is
/// always produced:
///
/// * **Row 1 (handler task panic).** Handler futures are dispatched onto
///   TPC lanes with `spawn_local` and their `JoinHandle` dropped, so a
///   panic is captured and discarded and the request's reply never
///   happens. `ReplyTx` owes a reply from construction; if it is dropped
///   still owing — panic, early `return`, a cancelled/dropped future —
///   its `Drop` synthesizes the error reply.
/// * **Row 2 (`reply_error_in_place`'s `let _ = …send()`).** A send that
///   the reply task can no longer receive is not silently discarded: for
///   a ring slot the reply is committed **directly** against the slot
///   through the connection (the transport's commit channel is
///   independent of the session's reply task), and only a genuinely
///   unaddressable reply lands on the must-stay-0
///   `transport_requests_abandoned` tripwire.
/// * **Row 3 (reply-task death).** Same mechanism: handlers dispatched
///   before the reply task died still reach the kernel.
///
/// Modeled on `TpcScheduler::dispatch` (spec §8: dead-lane detection,
/// re-dispatch, a counter, and failing loud rather than blackholing a
/// request).
pub(crate) struct ReplyTx {
    inner: UnboundedSender<FuseReply>,
    slot: ReplySlot,
    unique: u64,
    /// True while this request still owes the kernel exactly one reply.
    owed: bool,
    /// Direct-commit path for ring slots when the reply task is gone.
    #[cfg(feature = "tokio-runtime")]
    conn: Option<Arc<FuseConnection>>,
}

impl ReplyTx {
    /// A handle that owes a reply for `request` on `slot`.
    pub(crate) fn owing(
        inner: UnboundedSender<FuseReply>,
        request: &Request,
        slot: ReplySlot,
        #[cfg(feature = "tokio-runtime")] conn: Option<Arc<FuseConnection>>,
    ) -> Self {
        Self {
            inner,
            slot,
            unique: request.unique,
            owed: true,
            #[cfg(feature = "tokio-runtime")]
            conn,
        }
    }

    /// A handle for traffic the protocol defines as **no-reply**
    /// (FORGET/BATCH_FORGET, INTERRUPT — see FUSE-3i — and
    /// daemon-initiated notifications): it may still carry an error
    /// reply, but it owes nothing and its drop synthesizes nothing.
    pub(crate) fn no_reply(inner: UnboundedSender<FuseReply>) -> Self {
        Self {
            inner,
            slot: ReplySlot::Classical,
            unique: 0,
            owed: false,
            #[cfg(feature = "tokio-runtime")]
            conn: None,
        }
    }

    /// The reply was delivered by another arm (the P2 in-place
    /// READ/WRITE replies commit straight through the connection): this
    /// handle owes nothing and must not synthesize on drop.
    pub(crate) fn mark_replied(&mut self) {
        self.owed = false;
    }

    /// Synchronous fire-and-forget enqueue for daemon-initiated
    /// NOTIFICATIONS (no-reply handles): [`send`]'s happy path is exactly
    /// this one `unbounded_send` — nothing in it suspends — so
    /// notification callers need no task and no runtime (the 2026-08-05
    /// il-write-residual venue law: the daemon used to spawn onto the
    /// multi-thread runtime's global inject queue per size-growing ring
    /// write purely to `.await` this). Never flips `owed` (a no-reply
    /// handle owes nothing) and never falls back to a direct slot commit:
    /// a notification has no slot address (`ReplySlot::Classical`, `conn`
    /// `None`), and by the fire-and-forget contract a dead reply task
    /// (session teardown) DROPS it — the async form's `let _ =` posture,
    /// minus the request-abandoned count a notification never was.
    ///
    /// [`send`]: ReplyTx::send
    pub(crate) fn send_detached(&self, data: FuseData) {
        let _ = self.inner.unbounded_send(FuseReply {
            data,
            slot: self.slot,
        });
    }

    /// Send this request's reply. Never silently drops: a dead reply task
    /// falls back to a direct slot commit (rows 2 and 3).
    pub(crate) async fn send(&mut self, data: FuseData) -> Result<(), ()> {
        // op-trace `reply_commit` for every opcode WITHOUT an in-place
        // reply arm (the READ/WRITE arms stamp beside their own
        // transport-phase clock read): a clock read only for a traced op.
        if self.owed {
            crate::raw::op_trace::stamp_now(self.unique, crate::raw::op_trace::Stage::ReplyCommit);
        }
        self.owed = false;
        let Err(rejected) = self.inner.unbounded_send(FuseReply {
            data,
            slot: self.slot,
        }) else {
            return Ok(());
        };
        // The reply task is gone (row 3) or the session is tearing down.
        // A ring reply still has a live address: commit it directly
        // against its slot rather than discarding it the way
        // `let _ = …send()` did (row 2).
        let reply = rejected.into_inner();
        if self.commit_direct(reply.data) {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Last-resort delivery for a reply whose channel is gone. Returns
    /// true when the kernel will see it.
    fn commit_direct(&mut self, data: FuseData) -> bool {
        #[cfg(feature = "tokio-runtime")]
        {
            if let Some(conn) = self.conn.as_ref() {
                let (header, body) = match data {
                    Either::Left(header) => (header, Bytes::new()),
                    Either::Right((header, body, _backing)) => (header, body),
                };
                match conn.commit_reply_direct(self.slot, header, body) {
                    Ok(()) => {
                        crate::raw::connection::fuse_over_uring::note_reply_direct_commit();
                        return true;
                    }
                    Err(err) => {
                        error!(
                            unique = self.unique,
                            "fuse3: reply task gone and direct slot commit failed ({err}); \
                             the request is abandoned"
                        );
                    }
                }
            }
        }
        #[cfg(feature = "tokio-runtime")]
        crate::raw::connection::fuse_over_uring::note_request_abandoned();
        false
    }
}

impl Drop for ReplyTx {
    /// FUSE-2 row 1: a handler task that panicked (or was dropped) before
    /// replying leaves the kernel waiting forever — `spawn_local`'s
    /// `JoinHandle` is dropped, so the panic is captured and discarded
    /// and nothing else notices. The obligation dies with this handle, so
    /// this is where the synthesized reply is produced.
    fn drop(&mut self) {
        if !self.owed {
            return;
        }
        self.owed = false;
        error!(
            unique = self.unique,
            slot = ?self.slot,
            "fuse3: handler dropped without replying (panic or cancellation) — \
             synthesizing EIO so the caller is not left in uninterruptible sleep"
        );
        let out_header = fuse_out_header {
            len: FUSE_OUT_HEADER_SIZE as u32,
            error: libc::EIO.wrapping_neg(),
            unique: self.unique,
        };
        let data = get_bincode_config()
            .serialize(&out_header)
            .expect("fuse_out_header serializes");
        #[cfg(feature = "tokio-runtime")]
        crate::raw::connection::fuse_over_uring::note_reply_synthesized_by_guard();
        if self
            .inner
            .unbounded_send(FuseReply {
                data: Either::Left(data.clone()),
                slot: self.slot,
            })
            .is_err()
        {
            self.commit_direct(Either::Left(data));
        }
    }
}

impl Clone for ReplyTx {
    /// A clone never carries the reply obligation — exactly one handle
    /// owes the reply, so a duplicated handle can never produce a second
    /// one (the `Notify` handle is the only cloner).
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            slot: self.slot,
            unique: self.unique,
            owed: false,
            #[cfg(feature = "tokio-runtime")]
            conn: self.conn.clone(),
        }
    }
}

impl Debug for ReplyTx {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyTx")
            .field("unique", &self.unique)
            .field("slot", &self.slot)
            .field("owed", &self.owed)
            .finish()
    }
}

/// Kernel-advertised FUSE INIT capabilities, published by `handle_init`
/// before the filesystem's own `init` hook runs so capability probes
/// (e.g. FOPEN_NOFLUSH support = minor ≥ 35, atomic-open-class scans)
/// can consult the negotiated protocol from the daemon side.
///
/// `flags` is the full 64-bit capability word: classical `flags` in bits
/// 0..31 and the fuse ≥ 7.36 extended `flags2` in bits 32..63 (the
/// `FUSE_INIT_EXT` layout — `FUSE_OVER_IO_URING` is bit 41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelInit {
    /// Kernel FUSE protocol major (7 on every supported kernel).
    pub major: u32,
    /// Kernel FUSE protocol minor (e.g. 45 for Linux 7.1-class kernels).
    pub minor: u32,
    /// Folded 64-bit init capability word (`flags | flags2 << 32`).
    pub flags: u64,
}

impl KernelInit {
    /// Fold the classical `flags` word and the extended `flags2` word
    /// into the uapi's 64-bit capability layout (`flags2` carries bits
    /// 32..63 — e.g. `FUSE_OVER_IO_URING` = 1u64 << 41 arrives as bit 9
    /// of `flags2`).
    pub fn new(major: u32, minor: u32, flags: u32, flags2: u32) -> Self {
        Self {
            major,
            minor,
            flags: (flags as u64) | ((flags2 as u64) << 32),
        }
    }
}

/// One kernel per process: the first session's INIT wins (every mount in
/// a process talks to the same kernel, so the values are identical).
static KERNEL_INIT: std::sync::OnceLock<KernelInit> = std::sync::OnceLock::new();

/// The kernel INIT capabilities negotiated by this process's first FUSE
/// session; `None` until a session has processed `FUSE_INIT`.
pub fn kernel_init_info() -> Option<KernelInit> {
    KERNEL_INIT.get().copied()
}

/// The classical reply-flags word this process's first session actually
/// echoed back to the kernel (the intersection of the kernel's offer and
/// the daemon's implemented capabilities — one kernel, one negotiation
/// per process, like [`KERNEL_INIT`]).
static NEGOTIATED_REPLY_FLAGS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// The INIT reply flags this process advertised to the kernel (`None`
/// until a session has processed `FUSE_INIT`). The daemon's capability
/// gauges read this — e.g. `fuse_killpriv_negotiated` is
/// `flags & FUSE_HANDLE_KILLPRIV_V2` (the killpriv campaign), which
/// [`kernel_init_info`] alone cannot answer (that is the kernel's OFFER,
/// not what we accepted).
pub fn negotiated_reply_flags() -> Option<u32> {
    NEGOTIATED_REPLY_FLAGS.get().copied()
}

/// A Future which returns when a file system is unmounted
///
/// when drop the [`MountHandle`], it will unmount Filesystem in background task, if user want to
/// wait unmount completely, use [`MountHandle::unmount`]
#[derive(Debug)]
pub struct MountHandle {
    inner: Option<MountHandleInner>,
    pub fuse_connection: Option<Arc<crate::raw::connection::FuseConnection>>,
}

impl MountHandle {
    pub async fn unmount(mut self) -> IoResult<()> {
        self.inner
            .take()
            .expect("unmount call twice")
            .inner_unmount()
            .await
    }

    #[cfg(unix)]
    pub fn fd(&self) -> Option<std::os::fd::RawFd> {
        self.inner.as_ref().map(|inner| inner.fd)
    }

    pub fn connection(&self) -> Option<Arc<crate::raw::connection::FuseConnection>> {
        self.fuse_connection.clone()
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            if inner.task.is_finished() {
                return;
            }

            // Background unmount on a named OS thread: the drop path has
            // no executor, and the old fire-and-forget spawn discarded
            // the result the same way.
            std::thread::Builder::new()
                .name(crate::comm_core::comm_name("fuse3-unmount"))
                .spawn(move || {
                    let _ = crate::sqz_blocking::block_on(inner.inner_unmount());
                })
                .expect("fuse3-unmount thread spawns");
        }
    }
}

/// The mount task off tokio (rip-tokio-total): a dedicated named OS
/// thread runs `block_on(inner_mount())` and completes (a) the
/// `finished` latch — stored strictly BEFORE the result send, so
/// `is_finished() == true` implies the outcome exists (the two
/// `JoinHandle::is_finished` call sites only gate whether to await or
/// skip) — and (b) a oneshot carrying the mount result. A panicked
/// mount task drops the sender with the latch still false: the awaiting
/// side reads `RecvError` = "mount task panicked".
struct MountTask {
    finished: Arc<std::sync::atomic::AtomicBool>,
    result: crate::sqz_channel::oneshot::Receiver<IoResult<()>>,
}

impl std::fmt::Debug for MountTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountTask")
            .field("finished", &self.is_finished())
            .finish_non_exhaustive()
    }
}

impl MountTask {
    /// Spawn `fut` (the session's `inner_mount`) on the `fuse3-mount`
    /// OS thread.
    fn spawn(fut: impl Future<Output = IoResult<()>> + Send + 'static) -> Self {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = crate::sqz_channel::oneshot::channel();
        let latch = finished.clone();
        std::thread::Builder::new()
            .name(crate::comm_core::comm_name("fuse3-mount"))
            .spawn(move || {
                let res = crate::sqz_blocking::block_on(fut);
                // Latch BEFORE send: an `is_finished()` observer never
                // beats the result it implies.
                latch.store(true, std::sync::atomic::Ordering::Release);
                let _ = tx.send(res);
            })
            .expect("fuse3-mount thread spawns");
        Self {
            finished,
            result: rx,
        }
    }

    fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[derive(Debug)]
struct MountHandleInner {
    task: MountTask,
    mount_path: PathBuf,
    destroy_notify: Arc<async_notify::Notify>,
    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    unprivileged: bool,
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
}

impl MountHandleInner {
    async fn inner_unmount(self) -> IoResult<()> {
        self.destroy_notify.notify();

        #[cfg(feature = "tokio-runtime")]
        {
            // wait destroy done (RecvError = the mount task panicked —
            // the old `.await.unwrap()` re-panicked; an error keeps the
            // unmount path alive to report instead)
            if !self.task.is_finished() {
                self.task
                    .result
                    .await
                    .map_err(|_| IoError::other("mount task panicked"))??;
            }

            // TODO: freebsd mount is unprivileged, then unmount is unprivileged too?
            #[cfg(target_os = "freebsd")]
            {
                let mount_path = self.mount_path.clone();
                crate::sqz_blocking::run_blocking(move || {
                    mount::unmount(&mount_path, MntFlags::MNT_SYNCHRONOUS)
                })
                .await?;
            }

            #[cfg(target_os = "linux")]
            {
                #[cfg(all(target_os = "linux", feature = "unprivileged"))]
                if self.unprivileged {
                    let binary_path = find_fusermount3()?;
                    let mut success = false;
                    for attempt in 0..10 {
                        if attempt > 0 {
                            crate::sqz_time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        // std::process::Command spawn + wait on the blocking
                        // pool (fusermount3 is syscall-class subprocess work).
                        let binary_path = binary_path.clone();
                        let mount_path = self.mount_path.clone();
                        let status = crate::sqz_blocking::run_blocking(move || {
                            Command::new(&binary_path)
                                .args([OsStr::new("-u"), mount_path.as_os_str()])
                                .status()
                        })
                        .await?;
                        if status.success() {
                            success = true;
                            break;
                        }
                    }
                    if !success {
                        // A bystander holding a transient fd on the mount
                        // (desktop volume monitors inspect every new mount)
                        // makes the non-lazy unmount EBUSY for longer than the
                        // retry window. The daemon is exiting and the session
                        // is destroyed, so finish the way libfuse's own exit
                        // path does: detach lazily — the mount leaves the
                        // namespace now, the bystander's fd drains on its own.
                        let binary_path = binary_path.clone();
                        let mount_path = self.mount_path.clone();
                        let status = crate::sqz_blocking::run_blocking(move || {
                            Command::new(&binary_path)
                                .args([OsStr::new("-u"), OsStr::new("-z"), mount_path.as_os_str()])
                                .status()
                        })
                        .await?;
                        if !status.success() {
                            return Err(IoError::other(
                                "call fusermount3 -u (then -u -z) to unmount failed",
                            ));
                        }
                    }

                    return Ok(());
                }

                let mount_path = self.mount_path.clone();
                let mut success = false;
                for attempt in 0..10 {
                    if attempt > 0 {
                        crate::sqz_time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    let mp = mount_path.clone();
                    let res = crate::sqz_blocking::run_blocking(move || mount::umount(&mp)).await;
                    if res.is_ok() {
                        success = true;
                        break;
                    }
                }
                if !success {
                    // Same law as the fusermount3 path above: an exiting
                    // daemon finishes with a lazy detach (MNT_DETACH) when a
                    // bystander's transient fd keeps the non-lazy form EBUSY.
                    let mp = mount_path.clone();
                    crate::sqz_blocking::run_blocking(move || {
                        mount::umount2(&mp, MntFlags::MNT_DETACH)
                    })
                    .await
                    .map_err(|e| IoError::other(format!("umount (then MNT_DETACH) failed: {e}")))?;
                }
            }
        }

        Ok(())
    }
}

impl Future for MountHandle {
    type Output = IoResult<()>;

    #[cfg(feature = "tokio-runtime")]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // The expect actually should not fire: when MountHandle is canceled by the unmount
        // method, user has no chance to poll again
        Pin::new(
            &mut self
                .inner
                .as_mut()
                .expect("inner should be Some()")
                .task
                .result,
        )
        .poll(cx)
        .map(|r| r.unwrap_or_else(|_| Err(IoError::other("mount task panicked"))))
    }
}

#[cfg(feature = "tokio-runtime")]
/// fuse filesystem session, inode based.
pub struct Session<FS> {
    fuse_connection: Option<Arc<FuseConnection>>,
    filesystem: Option<Arc<FS>>,
    response_sender: UnboundedSender<FuseReply>,
    response_receiver: Option<UnboundedReceiver<FuseReply>>,
    mount_options: MountOptions,
}

impl<FS> Clone for Session<FS> {
    fn clone(&self) -> Self {
        Self {
            fuse_connection: self.fuse_connection.clone(),
            filesystem: self.filesystem.clone(),
            response_sender: self.response_sender.clone(),
            response_receiver: None,
            mount_options: self.mount_options.clone(),
        }
    }
}

enum ReadResult {
    Destroy,
    Request {
        in_header: IoResult<fuse_in_header>,
        header_buffer: Vec<u8>,
        data_buffer: Vec<u8>,
        uring_payload: Option<Bytes>,
        /// FUSE-3g: body bytes the transport actually WROTE into
        /// `data_buffer` (`read_vectored`'s `n` minus the 40-byte header;
        /// 0 on every error arm). The dispatch loop bounds `data_ref` by
        /// this, never by `in_header.len` — see [`validated_body`].
        filled: usize,
        /// Where this delivery's reply must be committed (FUSE-2 ⊕
        /// PERF-16) — the ring slot it arrived on, or `Classical`.
        reply_slot: ReplySlot,
    },
}

impl Debug for ReadResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadResult::Destroy => f.debug_struct("ReadResult::Destroy").finish(),
            ReadResult::Request { in_header, .. } => f
                .debug_struct("ReadResult::Request")
                .field("in_header", in_header)
                .finish_non_exhaustive(),
        }
    }
}

#[cfg(feature = "tokio-runtime")]
impl<FS> Session<FS> {
    /// new a fuse filesystem session.
    pub fn new(mount_options: MountOptions) -> Self {
        let (sender, receiver) = unbounded();

        Self {
            fuse_connection: None,
            filesystem: None,
            response_sender: sender,
            response_receiver: Some(receiver),
            mount_options,
        }
    }

    /// get a [`notify`].
    ///
    /// `pub` since PR L4-6 (the design's amended Non-Goal sanctions this
    /// additive exposure): the daemon captures a handle **before**
    /// `mount()` consumes the session and drives
    /// `FUSE_NOTIFY_INVAL_INODE` for ring-write invalidations (§5.6.2
    /// W1). The handle clones the session's reply channel, so it stays
    /// valid for the mount's lifetime and rides the classical
    /// connection's reply path even after FUSE-over-io_uring arms.
    ///
    /// [`notify`]: Notify
    pub fn get_notify(&self) -> Notify {
        Notify::new(ReplyTx::no_reply(self.response_sender.clone()))
    }

    pub fn get_payload_buffer(&self, slot: ReplySlot) -> Option<(u64, usize)> {
        let conn = self.fuse_connection.as_ref()?;
        conn.get_payload_buffer(slot)
    }

    /// A handle for traffic the FUSE protocol defines as **no-reply**
    /// (`FUSE_NOTIFY_REPLY`; FORGET/BATCH_FORGET and INTERRUPT do not
    /// even take one). It can still carry an error reply, but it owes
    /// nothing, so finishing silently is correct rather than a lost
    /// reply.
    fn no_reply_tx(&self) -> ReplyTx {
        ReplyTx::no_reply(self.response_sender.clone())
    }

    /// The one reply handle for `request` (FUSE-2): it owes the kernel
    /// exactly one reply from here until it is sent, marked delivered by
    /// an in-place arm, or synthesized by its `Drop`.
    fn reply_tx(&self, request: &Request) -> ReplyTx {
        ReplyTx::owing(
            self.response_sender.clone(),
            request,
            request.slot,
            // Only a ring reply has a direct-commit fallback; classical
            // replies have nowhere to go but the reply task's device
            // write (and pay no Arc clone here).
            #[cfg(feature = "tokio-runtime")]
            if request.slot.is_ring() {
                self.fuse_connection.clone()
            } else {
                None
            },
        )
    }

    pub fn connection(&self) -> Option<Arc<FuseConnection>> {
        self.fuse_connection.clone()
    }
}

#[cfg(feature = "tokio-runtime")]
impl<FS: Filesystem + Send + Sync + 'static> Session<FS> {
    async fn mount_empty_check(&self, mount_path: &Path) -> IoResult<()> {
        // std::fs::read_dir on the blocking pool (one-shot mount-time
        // probe; `Some(Ok(_))` = the directory has a readable first entry,
        // the tokio `next_entry() == Ok(Some(_))` shape).
        let mount_path_buf = mount_path.to_path_buf();
        if !self.mount_options.nonempty
            && crate::sqz_blocking::run_blocking(move || {
                std::fs::read_dir(mount_path_buf)
                    .map(|mut entries| matches!(entries.next(), Some(Ok(_))))
            })
            .await?
        {
            return Err(IoError::new(
                ErrorKind::AlreadyExists,
                "mount point is not empty",
            ));
        }

        Ok(())
    }

    /// mount the filesystem without root permission. This function will block
    /// until the filesystem is unmounted.
    // On FreeBSD, no special interface is required to mount unprivileged.
    // If vfs.usermount=1 and the user has access to the mountpoint, it will
    // just work.
    #[cfg(all(target_os = "freebsd", feature = "unprivileged"))]
    pub async fn mount_with_unprivileged<P: AsRef<Path>>(
        self,
        fs: FS,
        mount_path: P,
    ) -> IoResult<MountHandle> {
        self.mount(fs, mount_path).await
    }

    /// mount the filesystem without root permission.
    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    pub async fn mount_with_unprivileged<P: AsRef<Path>>(
        mut self,
        fs: FS,
        mount_path: P,
    ) -> IoResult<MountHandle> {
        let mount_path = mount_path.as_ref();

        self.mount_empty_check(mount_path).await?;

        let notify = Arc::new(async_notify::Notify::new());
        let fuse_connection = FuseConnection::new_with_unprivileged(
            self.mount_options.clone(),
            mount_path,
            notify.clone(),
        )
        .await?;

        let fd = fuse_connection.as_fd().as_raw_fd();
        self.fuse_connection.replace(Arc::new(fuse_connection));

        self.filesystem.replace(Arc::new(fs));

        debug!("mount {:?} success", mount_path);

        let fuse_conn_opt = self.fuse_connection.clone();
        Ok(MountHandle {
            inner: Some(MountHandleInner {
                task: MountTask::spawn(self.inner_mount()),
                mount_path: mount_path.to_path_buf(),
                destroy_notify: notify,
                unprivileged: true,
                #[cfg(unix)]
                fd,
            }),
            fuse_connection: fuse_conn_opt,
        })
    }

    /// mount the filesystem with root permission.
    #[cfg(target_os = "linux")]
    pub async fn mount<P: AsRef<Path>>(mut self, fs: FS, mount_path: P) -> IoResult<MountHandle> {
        let mount_path = mount_path.as_ref();

        self.mount_empty_check(mount_path).await?;

        let notify = Arc::new(async_notify::Notify::new());
        let fuse_connection = FuseConnection::new(notify.clone())?;

        let fd = fuse_connection.as_fd().as_raw_fd();

        let options = self.mount_options.build(fd);

        let fs_name = if let Some(fs_name) = self.mount_options.fs_name.as_ref() {
            Some(fs_name.as_str())
        } else {
            Some("fuse")
        };

        debug!("mount options {:?}", options);

        if let Err(err) = mount::mount(
            fs_name,
            mount_path,
            Some("fuse"),
            self.mount_options.flags(),
            Some(options.as_os_str()),
        ) {
            error!("mount {:?} failed", mount_path);

            return Err(err.into());
        }

        self.fuse_connection.replace(Arc::new(fuse_connection));

        self.filesystem.replace(Arc::new(fs));

        debug!("mount {:?} success", mount_path);

        let fuse_conn_opt = self.fuse_connection.clone();
        Ok(MountHandle {
            inner: Some(MountHandleInner {
                task: MountTask::spawn(self.inner_mount()),
                mount_path: mount_path.to_path_buf(),
                destroy_notify: notify,
                #[cfg(all(target_os = "linux", feature = "unprivileged"))]
                unprivileged: false,
                #[cfg(unix)]
                fd,
            }),
            fuse_connection: fuse_conn_opt,
        })
    }

    /// mount the filesystem
    #[cfg(target_os = "freebsd")]
    pub async fn mount<P: AsRef<Path>>(mut self, fs: FS, mount_path: P) -> IoResult<MountHandle> {
        let mount_path = mount_path.as_ref();

        self.mount_empty_check(mount_path).await?;

        let notify = Arc::new(async_notify::Notify::new());
        let fuse_connection = FuseConnection::new(notify.clone())?;

        let fd = fuse_connection.as_fd().as_raw_fd();

        {
            let mut nmount = self.mount_options.build();
            nmount
                .str_opt_owned(c"fspath", mount_path)
                .str_opt_owned(c"fd", format!("{}", fd).as_str());
            debug!("mount options {:?}", &nmount);

            if let Err(err) = nmount.nmount(self.mount_options.flags()) {
                error!("mount {} failed: {}", mount_path.display(), err);

                return Err(std::io::Error::from(err));
            }
        }

        self.fuse_connection.replace(Arc::new(fuse_connection));

        self.filesystem.replace(Arc::new(fs));

        debug!("mount {:?} success", mount_path);

        Ok(MountHandle {
            inner: Some(MountHandleInner {
                task: MountTask::spawn(self.inner_mount()),
                mount_path: mount_path.to_path_buf(),
                destroy_notify: notify,
            }),
            fuse_connection: self.fuse_connection.clone(),
        })
    }

    async fn inner_mount_worker(mut self, max_write: usize) -> IoResult<()> {
        let fuse_write_connection = self.fuse_connection.as_ref().unwrap().clone();
        let receiver = self.response_receiver.take().unwrap();

        let dispatch_task = self.dispatch_with_max_write(max_write).fuse();
        let mut dispatch_task = pin!(dispatch_task);

        // The reply task rides the session's own TPC lane machinery
        // (the venue the dispatch loop itself runs on); a oneshot
        // carries its result back. RecvError = the reply future
        // vanished (lane panic) — re-panic, parity with the old
        // `task::spawn(..).map(Result::unwrap)`.
        let (reply_done_tx, reply_done_rx) = crate::sqz_channel::oneshot::channel();
        {
            let fut = Self::reply_fuse(fuse_write_connection, receiver);
            crate::raw::session::tpc_spawn(async move {
                let _ = reply_done_tx.send(fut.await);
            });
        }
        let reply_task = async move {
            reply_done_rx
                .await
                .expect("reply_fuse task vanished (lane panic)")
        }
        .fuse();

        let mut reply_task = pin!(reply_task);

        select! {
            reply_result = reply_task => {
                reply_result?;
            }

            dispatch_result = dispatch_task => {
                dispatch_result?;
            }
        }

        Ok(())
    }

    async fn inner_mount(mut self) -> IoResult<()> {
        let fuse_connection = self.fuse_connection.clone().unwrap();
        let fs = self.filesystem.clone().expect("filesystem not init");

        let max_write = self.init_filesystem(&fs, &fuse_connection).await?.get() as usize;

        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        {
            // FUSE-over-io_uring armed: one worker session per uring queue
            // (including qid 0), while THIS primary session becomes the
            // classical sideband servicer. The kernel keeps FORGET/INTERRUPT/
            // resends and `fiq->ops` switchover stragglers on the classical
            // `/dev/fuse` queue even with the ring armed — a daemon that stops
            // reading it strands them forever (`fusectl waiting ≥ 1`, syncfs
            // blocks, umount EBUSY). The sideband read still rides io_uring
            // (`Readv` on `/dev/fuse`).
            if let Some(nqueues) = fuse_connection.num_uring_queues() {
                debug!("Multi-Queue FUSE: Spawning {} worker connections", nqueues);
                for qid in 0..nqueues {
                    let mut worker_session = self.clone();
                    let (tx, rx) = unbounded();
                    worker_session.response_sender = tx;
                    worker_session.response_receiver = Some(rx);

                    let mut cloned_conn = fuse_connection.clone_connection()?;
                    cloned_conn.assigned_qid = Some(qid as u16);
                    worker_session.fuse_connection = Some(Arc::new(cloned_conn));
                    worker_session.filesystem = self.filesystem.clone();

                    crate::raw::session::tpc_spawn(async move {
                        if let Err(e) = worker_session.inner_mount_worker(max_write).await {
                            error!("Multi-Queue FUSE worker exited with error: {:?}", e);
                        }
                    });
                }
                fuse_connection.set_classical_sideband();
                eprintln!(
                    "FUSE-over-io_uring: classical sideband servicer armed \
                     (primary session; FORGET/INTERRUPT/resend + switchover stragglers)"
                );

                // zc-write-fusion: register the fused-write dispatcher
                // with the transport pool — queue workers may now run
                // small armed WRITE handler futures on their own fused
                // lanes (zero cross-thread wakes on the store/extract
                // bridge round trip). The Weak connection breaks the
                // pool→dispatcher→conn→pool Arc cycle; deliveries that
                // raced this registration ride the classic dispatch
                // (counted as fusion demotions).
                {
                    use crate::raw::connection::fuse_over_uring::fused::{
                        FusedFuture, FusedWriteDispatch,
                    };
                    use crate::raw::connection::fuse_over_uring::InboundUringReq;
                    let fs_for_fused = fs.clone();
                    let conn_weak = Arc::downgrade(&fuse_connection);
                    let reply_sender = self.response_sender.clone();
                    let mint = move |req: InboundUringReq| -> FusedFuture {
                        Box::pin(fused_write_future(
                            fs_for_fused.clone(),
                            conn_weak.clone(),
                            reply_sender.clone(),
                            req,
                        ))
                    };
                    fuse_connection.set_fused_write_dispatcher(FusedWriteDispatch {
                        mint: Arc::new(mint),
                    });
                    // fused-lane-predicate (2026-08-08): the HOLD gate —
                    // the filesystem's W1-eligibility probe, registered
                    // INDEPENDENT of the fusion lever (holding an
                    // unconsumable shape is wrong on the classic
                    // dispatch too: its handler pays the lazy round
                    // trip instead of the batched at-delivery
                    // extraction). Unregistered/`false` ⇒ extract at
                    // delivery.
                    let fs_for_gate = fs.clone();
                    fuse_connection.set_zc_write_hold_gate(Arc::new(
                        move |ino: u64, offset: u64, len: u32, odirect: bool| {
                            fs_for_gate.zc_write_hold_eligible(ino, offset, len, odirect)
                        },
                    ));
                    // R-2 READ fast-dispatch: the filesystem's SYNC
                    // try-only probe + the READ-handler mint, so the
                    // queue workers can serve warm READs inline at the
                    // delivery CQE and hand cold ones straight to a
                    // lane. Same Weak-connection shape as the fused
                    // write mint; READs delivered before this
                    // registration ride the inbound queue.
                    {
                        use crate::raw::connection::fuse_over_uring::fast_dispatch::ReadFastDispatch;
                        let fs_for_probe = fs.clone();
                        let fs_for_mint = fs.clone();
                        let conn_weak = Arc::downgrade(&fuse_connection);
                        let reply_sender = self.response_sender.clone();
                        fuse_connection.set_read_fast_dispatcher(ReadFastDispatch {
                            probe: Arc::new(
                                move |ino: u64,
                                      fh: u64,
                                      offset: u64,
                                      size: u32,
                                      flags: u32,
                                      dest: Option<(u64, usize)>| {
                                    fs_for_probe.read_fast_probe(ino, fh, offset, size, flags, dest)
                                },
                            ),
                            mint: Arc::new(move |req: InboundUringReq| -> FusedFuture {
                                Box::pin(fast_read_future(
                                    fs_for_mint.clone(),
                                    conn_weak.clone(),
                                    reply_sender.clone(),
                                    req,
                                ))
                            }),
                        });
                    }
                }
            }
        }

        let fuse_write_connection = self.fuse_connection.as_ref().unwrap().clone();
        let receiver = self.response_receiver.take().unwrap();

        let dispatch_task = self.dispatch_with_max_write(max_write).fuse();
        let mut dispatch_task = pin!(dispatch_task);

        // The reply task rides the session's own TPC lane machinery
        // (the venue the dispatch loop itself runs on); a oneshot
        // carries its result back. RecvError = the reply future
        // vanished (lane panic) — re-panic, parity with the old
        // `task::spawn(..).map(Result::unwrap)`.
        let (reply_done_tx, reply_done_rx) = crate::sqz_channel::oneshot::channel();
        {
            let fut = Self::reply_fuse(fuse_write_connection, receiver);
            crate::raw::session::tpc_spawn(async move {
                let _ = reply_done_tx.send(fut.await);
            });
        }
        let reply_task = async move {
            reply_done_rx
                .await
                .expect("reply_fuse task vanished (lane panic)")
        }
        .fuse();

        let mut reply_task = pin!(reply_task);

        select! {
            reply_result = reply_task => {
                reply_result?;
            }

            dispatch_result = dispatch_task => {
                dispatch_result?;
            }
        }

        Ok(())
    }

    async fn reply_fuse(
        fuse_connection: Arc<FuseConnection>,
        mut response_receiver: UnboundedReceiver<FuseReply>,
    ) -> IoResult<()> {
        while let Some(FuseReply { data, slot }) = response_receiver.next().await {
            let (data, extend_data, backing) = match data {
                Either::Left(data) => (data, None, None),
                Either::Right((data, extend_data, backing)) => (data, Some(extend_data), backing),
            };

            // L3 lever A: replies route through `write_vectored` ONLY —
            // over-uring COMMIT_AND_FETCH for ring uniques (the hot path
            // after arm), classical vectored write for INIT / sideband /
            // handoff stragglers. The historical splice_reply fast path was
            // deleted: on an armed session every ring reply bounced off
            // `/dev/fuse` with ENOENT (the kernel holds ring uniques in the
            // uring ent, not fpq->processing) and paid a pipe2 + write +
            // vmsplice + splice + 2×close + fcntl block per READ reply
            // before falling back here anyway.
            if let Err(err) = fuse_connection
                .write_vectored(data, extend_data, slot)
                .await
                .1
            {
                if err.kind() == ErrorKind::NotFound {
                    warn!(
                        "may reply interrupted fuse request, ignore this error {}",
                        err
                    );

                    continue;
                }

                error!("reply fuse failed {}", err);

                return Err(err);
            }

            drop(backing);
        }

        Ok(())
    }

    #[instrument(level = "debug", skip(self, fs), ret, err)]
    async fn init_filesystem(
        &mut self,
        fs: &FS,
        fuse_connection: &FuseConnection,
    ) -> IoResult<NonZeroU32> {
        let header_buffer = vec![0; FUSE_IN_HEADER_SIZE];
        let data_buffer = vec![0; FUSE_MIN_READ_BUFFER_SIZE];

        let (data_buffer, in_header, filled) = match self
            .read_fuse_request(fuse_connection, header_buffer, data_buffer)
            .await
        {
            ReadResult::Destroy => {
                return Err(IoError::new(
                    ErrorKind::UnexpectedEof,
                    "init stage get destroy result",
                ));
            }

            ReadResult::Request {
                in_header,
                data_buffer,
                filled,
                ..
            } => {
                let in_header = in_header?;
                (data_buffer, in_header, filled)
            }
        };

        let request = Request::from(&in_header);

        let opcode = match fuse_opcode::try_from(in_header.opcode) {
            Err(err) => {
                debug!("receive unknown opcode {}", err.0);

                reply_error_in_place(libc::ENOSYS.into(), request, self.reply_tx(&request)).await;

                return Err(IoError::other(format!("receive unknown opcode {}", err.0)));
            }

            Ok(opcode) => opcode,
        };

        debug!("receive opcode {}", opcode);

        if opcode != fuse_opcode::FUSE_INIT {
            error!(?opcode, "received unexpected opcode");

            return Err(IoError::other(format!("unexpected opcode {opcode:?}")));
        }

        // FUSE-3g: bound INIT's body by the bytes that were filled. A
        // short/lying INIT header refuses the SESSION (there is nothing to
        // reply to yet — the mount fails loud rather than negotiating
        // against stale buffer bytes).
        let data_ref = match validated_body(in_header.len, filled, 0, data_buffer.len()) {
            BodyBounds::Valid(n) => &data_buffer[..n],
            refused => {
                error!(
                    "FUSE_INIT body bounds refused ({refused:?}): in_header.len {} filled {filled}",
                    in_header.len
                );
                return Err(IoError::new(
                    ErrorKind::InvalidData,
                    "FUSE_INIT declares a body the transport did not deliver",
                ));
            }
        };

        self.handle_init(request, data_ref, fuse_connection, fs)
            .await
    }

    #[instrument(level = "debug", skip(self, header_buffer, data_buffer), ret)]
    async fn read_fuse_request(
        &mut self,
        fuse_connection: &FuseConnection,
        mut header_buffer: Vec<u8>,
        mut data_buffer: Vec<u8>,
    ) -> ReadResult {
        let reply_slot;
        let (uring_payload, res) = match fuse_connection
            .read_vectored(header_buffer, data_buffer)
            .await
        {
            None => return ReadResult::Destroy,

            Some(((header_buf, data_buf, payload, slot), res)) => {
                header_buffer = header_buf;
                data_buffer = data_buf;
                reply_slot = slot;

                (payload, res)
            }
        };
        let n = match res {
            Err(err) => {
                // Kernel abort / unmount / fuse-over-uring pool shutdown.
                // Classical path: ENODEV (pre-FUSE_ABORT_ERROR) or ECONNABORTED.
                // Uring path: we surface ENOTCONN / NotConnected when the pool dies.
                let disconnect = match err.raw_os_error() {
                    Some(
                        libc::ENODEV
                        | libc::ECONNABORTED
                        | libc::ENOTCONN
                        | libc::EPIPE
                        | libc::EBADF
                        | libc::ESHUTDOWN,
                    ) => true,
                    _ => {
                        err.kind() == ErrorKind::NotConnected || err.kind() == ErrorKind::BrokenPipe
                    }
                };
                if disconnect {
                    debug!("fuse connection dead ({err}); ending session");
                    #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
                    fuse_connection.teardown_over_uring();
                    return ReadResult::Destroy;
                }

                error!("read from /dev/fuse failed {}", err);

                return ReadResult::Request {
                    in_header: Err(err),
                    header_buffer,
                    data_buffer,
                    uring_payload,
                    filled: 0,
                    reply_slot,
                };
            }

            Ok(n) => n,
        };

        debug!(n, "read fuse request done");

        if n < FUSE_IN_HEADER_SIZE {
            error!(
                n,
                FUSE_IN_HEADER_SIZE, "read_vectored n is less then FUSE_IN_HEADER_SIZE"
            );

            return ReadResult::Request {
                in_header: Err(IoError::other(
                    "read_vectored n is less then FUSE_IN_HEADER_SIZE",
                )),
                header_buffer,
                data_buffer,
                uring_payload,
                filled: 0,
                reply_slot,
            };
        }

        let in_header = match get_bincode_config().deserialize::<fuse_in_header>(&header_buffer) {
            Err(err) => {
                error!("deserialize fuse_in_header failed {}", err);

                return ReadResult::Request {
                    in_header: Err(IoError::other(err)),
                    header_buffer,
                    data_buffer,
                    uring_payload,
                    filled: 0,
                    reply_slot,
                };
            }

            Ok(in_header) => in_header,
        };

        ReadResult::Request {
            in_header: Ok(in_header),
            header_buffer,
            data_buffer,
            uring_payload,
            // FUSE-3g: `n` is header + body; the body is what the
            // transport FILLED, and the dispatch loop may never present
            // more than this to a handler.
            filled: n - FUSE_IN_HEADER_SIZE,
            reply_slot,
        }
    }

    async fn dispatch_with_max_write(&mut self, max_write: usize) -> IoResult<()> {
        // CLONE, never take: `handle_read`'s P2 in-place reply arm reads
        // `self.fuse_connection` — the historical `take()` left it None for
        // the whole dispatch loop, so every READ reply silently fell back
        // to the unbounded reply channel + reply-task hop the P2 commit
        // (`be82794`) had deleted (found by the 2026-08-01 serve-
        // decomposition campaign; `fuse3_read_inplace_replies` is the
        // engagement gauge that keeps this wired).
        let fuse_connection = self.fuse_connection.clone().unwrap();
        let fs = self.filesystem.take().expect("filesystem not init");
        let buffer_size = (max_write + FUSE_WRITE_IN_SIZE).max(FUSE_MIN_READ_BUFFER_SIZE);

        let mut header_buffer = vec![0; FUSE_IN_HEADER_SIZE];
        let mut data_buffer = vec![0; buffer_size];

        loop {
            let uring_payload;
            let reply_slot;
            let filled;
            let in_header = match self
                .read_fuse_request(&fuse_connection, header_buffer, data_buffer)
                .await
            {
                ReadResult::Destroy => {
                    fs.destroy(Request::default()).await;

                    return Ok(());
                }

                ReadResult::Request {
                    in_header,
                    header_buffer: header_buf,
                    data_buffer: data_buf,
                    uring_payload: payload,
                    filled: body_filled,
                    reply_slot: slot,
                } => {
                    header_buffer = header_buf;
                    data_buffer = data_buf;
                    uring_payload = payload;
                    filled = body_filled;
                    reply_slot = slot;

                    match in_header {
                        Err(_) => continue,

                        Ok(in_header) => in_header,
                    }
                }
            };

            // FUSE-2 ⊕ PERF-16: the request carries its own reply
            // address from here on — every reply path (handler,
            // in-place arm, error, drop-guard synthesis) commits against
            // this slot, and no `unique → slot` map exists to consult.
            let mut request = Request::from(&in_header);
            request.slot = reply_slot;
            let request = request;

            let opcode = match fuse_opcode::try_from(in_header.opcode) {
                Err(err) => {
                    debug!("receive unknown opcode {}", err.0);

                    reply_error_in_place(libc::ENOSYS.into(), request, self.reply_tx(&request))
                        .await;

                    continue;
                }

                Ok(opcode) => opcode,
            };

            debug!("receive opcode {}", opcode);

            // FUSE-3g: the body is the bytes the transport FILLED, never
            // what the header claims. `in_header.len` is kernel-supplied
            // input: below 40 it UNDERFLOWS the subtraction (release
            // profile: a ~2^64 slice length, an instant panic on this
            // dispatch task — every in-flight request on the session loses
            // its reply), and above `40 + filled` it extends the slice over
            // the PREVIOUS request's bytes still sitting in this reused
            // buffer. Either way: reply EINVAL, never present the slice.
            // (The §5.4 zero-copy WRITE body legitimately rides
            // `uring_payload` and is counted as available — see
            // `validated_body`. D14 held-slot WRITEs deliver an EMPTY
            // placeholder while the body sits in the transport's sparse
            // slot: the connection's held table carries the
            // authoritative length, which is AVAILABLE by construction
            // — `handle_write` re-validates it against the header.)
            let payload_len = {
                let placeholder_len = uring_payload.as_ref().map_or(0, Bytes::len);
                #[cfg(target_os = "linux")]
                {
                    if placeholder_len == 0
                        && uring_payload.is_some()
                        && in_header.opcode == fuse_opcode::FUSE_WRITE as u32
                    {
                        fuse_connection
                            .zc_write_held_len(request.slot)
                            .map_or(placeholder_len, |l| l as usize)
                    } else {
                        placeholder_len
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    placeholder_len
                }
            };
            let data_ref =
                match validated_body(in_header.len, filled, payload_len, data_buffer.len()) {
                    BodyBounds::Valid(n) => &data_buffer[..n],
                    refused => {
                        error!(
                            "delivery body bounds refused ({refused:?}): unique {} opcode {} \
                             in_header.len {} filled {filled} payload {payload_len}",
                            in_header.unique, in_header.opcode, in_header.len
                        );
                        reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request))
                            .await;

                        continue;
                    }
                };

            match opcode {
                fuse_opcode::FUSE_INIT => {
                    warn!("duplicated fuse init request");

                    self.handle_init(request, data_ref, &fuse_connection, &fs)
                        .await?;
                }

                fuse_opcode::FUSE_DESTROY => {
                    debug!("receive fuse destroy");

                    fs.destroy(request).await;

                    // Reply inline (synchronously, not via the async reply task) and
                    // only then shut the pool down: shutdown clears the pending map,
                    // so a reply routed after it would be dropped and the kernel
                    // would wait on DESTROY forever. `write_vectored` routes by
                    // delivery channel — COMMIT_AND_FETCH for uring-delivered
                    // DESTROYs (the shutdown drain flushes it), classical write for
                    // classically-delivered ones (sideband/switchover-race case).
                    #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
                    {
                        let mut hdr = vec![0u8; FUSE_OUT_HEADER_SIZE];
                        hdr[0..4].copy_from_slice(&(FUSE_OUT_HEADER_SIZE as u32).to_le_bytes());
                        hdr[8..16].copy_from_slice(&request.unique.to_le_bytes());
                        let _ = fuse_connection
                            .write_vectored::<_, Vec<u8>>(hdr, None, request.slot)
                            .await
                            .1;
                        fuse_connection.teardown_over_uring();
                    }
                    #[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
                    {
                        reply_none_in_place(request, self.reply_tx(&request)).await;
                    }

                    debug!("fuse destroyed");

                    return Ok(());
                }

                fuse_opcode::FUSE_LOOKUP => {
                    self.handle_lookup(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_FORGET => {
                    self.handle_forget(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_GETATTR => {
                    self.handle_getattr(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_SETATTR => {
                    self.handle_setattr(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_READLINK => {
                    self.handle_readlink(request, in_header, &fs).await;
                }

                fuse_opcode::FUSE_SYMLINK => {
                    self.handle_symlink(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_MKNOD => {
                    self.handle_mknod(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_MKDIR => {
                    self.handle_mkdir(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_UNLINK => {
                    self.handle_unlink(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_RMDIR => {
                    self.handle_rmdir(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_RENAME => {
                    self.handle_rename(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_LINK => {
                    self.handle_link(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_OPEN => {
                    self.handle_open(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_READ => {
                    self.handle_read(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_WRITE => {
                    self.handle_write(request, in_header, data_ref, uring_payload.clone(), &fs)
                        .await;
                }

                fuse_opcode::FUSE_STATFS => {
                    self.handle_statfs(request, in_header, &fs).await;
                }

                fuse_opcode::FUSE_RELEASE => {
                    self.handle_release(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_FSYNC => {
                    self.handle_fsync(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_SETXATTR => {
                    self.handle_setxattr(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_GETXATTR => {
                    self.handle_getxattr(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_LISTXATTR => {
                    self.handle_listxattr(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_REMOVEXATTR => {
                    self.handle_removexattr(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_FLUSH => {
                    self.handle_flush(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_OPENDIR => {
                    self.handle_opendir(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_READDIR => {
                    self.handle_readdir(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_RELEASEDIR => {
                    self.handle_releasedir(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_FSYNCDIR => {
                    self.handle_fsyncdir(request, in_header, data_ref, &fs)
                        .await;
                }

                #[cfg(feature = "file-lock")]
                fuse_opcode::FUSE_GETLK => {
                    self.handle_getlk(request, in_header, data_ref, &fs).await;
                }

                #[cfg(feature = "file-lock")]
                fuse_opcode::FUSE_SETLK | fuse_opcode::FUSE_SETLKW => {
                    self.handle_setlk(
                        request,
                        in_header,
                        data_ref,
                        opcode == fuse_opcode::FUSE_SETLKW,
                        &fs,
                    )
                    .await;
                }

                fuse_opcode::FUSE_ACCESS => {
                    self.handle_access(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_CREATE => {
                    self.handle_create(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_INTERRUPT => {
                    self.handle_interrupt(request, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_BMAP => {
                    self.handle_bmap(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_IOCTL => {
                    self.handle_ioctl(request, in_header, data_ref, &fs).await;
                }
                fuse_opcode::FUSE_POLL => {
                    self.handle_poll(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_NOTIFY_REPLY => {
                    self.handle_notify_reply(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_BATCH_FORGET => {
                    self.handle_batch_forget(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_FALLOCATE => {
                    self.handle_fallocate(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_READDIRPLUS => {
                    self.handle_readdirplus(request, in_header, data_ref, &fs)
                        .await;
                }

                fuse_opcode::FUSE_RENAME2 => {
                    self.handle_rename2(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_LSEEK => {
                    self.handle_lseek(request, in_header, data_ref, &fs).await;
                }

                fuse_opcode::FUSE_COPY_FILE_RANGE => {
                    self.handle_copy_file_range(request, in_header, data_ref, &fs)
                        .await;
                }

                #[cfg(target_os = "macos")]
                fuse_opcode::FUSE_SETVOLNAME => {}

                #[cfg(target_os = "macos")]
                fuse_opcode::FUSE_GETXTIMES => {}

                #[cfg(target_os = "macos")]
                fuse_opcode::FUSE_EXCHANGE => {} // fuse_opcode::CUSE_INIT => {}
            }
        }
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_init(
        &mut self,
        request: Request,
        data: &[u8],
        fuse_connection: &FuseConnection,
        fs: &FS,
    ) -> IoResult<NonZeroU32> {
        // fuse_init_in is deserialized as the 16-byte classical prefix;
        // fuse ≥ 7.36 kernels send the extended struct whose `flags2`
        // (init flag bits 32..63) sits at byte offset 16, valid when
        // FUSE_INIT_EXT is set in `flags`. Read it manually so the
        // published KernelInit carries the full 64-bit capability word.
        let init_in = match get_bincode_config().deserialize::<fuse_init_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_init_in failed {}, request unique {}",
                    err, request.unique
                );

                let init_out_header = fuse_out_header {
                    len: FUSE_OUT_HEADER_SIZE as u32,
                    error: libc::EINVAL,
                    unique: request.unique,
                };

                let init_out_header_data = get_bincode_config()
                    .serialize(&init_out_header)
                    .expect("won't happened");

                if let Err(err) = fuse_connection
                    .write_vectored::<_, Vec<u8>>(init_out_header_data, None, ReplySlot::Classical)
                    .await
                    .1
                {
                    error!("write error init out data to /dev/fuse failed {}", err);
                }

                return Err(IoError::from_raw_os_error(libc::EINVAL));
            }

            Ok(init_in) => init_in,
        };

        debug!("fuse_init {:?}", init_in);

        // Publish the kernel's advertised capabilities BEFORE calling
        // `fs.init` below, so the filesystem's own init hook can consult
        // `kernel_init_info()` (capability probes / log lines).
        let init_flags2: u32 = if init_in.flags & FUSE_INIT_EXT > 0 && data.len() >= 20 {
            u32::from_le_bytes(data[16..20].try_into().expect("4-byte slice"))
        } else {
            0
        };
        let _ = KERNEL_INIT.set(KernelInit::new(
            init_in.major,
            init_in.minor,
            init_in.flags,
            init_flags2,
        ));

        // Required: always advertise FUSE_OVER_IO_URING. No opt-out.
        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        let flags2 = {
            debug!("advertising FUSE_OVER_IO_URING in init flags2");
            crate::raw::connection::fuse_over_uring::FUSE_OVER_IO_URING_FLAGS2
        };
        // Non-Linux / non-tokio builds cannot use over-uring (SqueezeFS is Linux-only).
        #[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
        let flags2 = 0u32;

        // sqz FUSE_TIME_LIMITS (kernel-sqz patch 0027): when the kernel
        // offered folded capability bit 62, echo it and carry the daemon's
        // exact-round-trip inode-timestamp range so VFS
        // timestamp_truncate() clamps incore exactly where the daemon
        // clamps durable state (fstests generic/634 becomes expected-PASS
        // on sqz-kernel hosts; the fleet-kernel adjudication stays
        // pinned). Stock kernels never offer the bit: fields stay zero and
        // the reply is bit-identical to the pre-0027 daemon.
        let time_limits =
            negotiate_time_limits((init_in.flags as u64) | ((init_flags2 as u64) << 32));
        let flags2 = if time_limits.is_some() {
            debug!("advertising FUSE_TIME_LIMITS in init flags2 (sqz kernel offered bit 62)");
            flags2 | ((FUSE_TIME_LIMITS >> 32) as u32)
        } else {
            flags2
        };
        let (time_min, time_max) = time_limits.unwrap_or((0, 0));

        // FUSE-1: the reply minor and the FUSE_INIT_EXT header bit — what
        // makes the kernel's process_init_reply() fold `flags2` into the
        // capability word instead of discarding it (see negotiate_init_ext).
        let (reply_minor, init_ext) = negotiate_init_ext(init_in.minor, init_in.flags, flags2);
        let reply_flags = negotiate_reply_flags(init_in.flags, &self.mount_options) | init_ext;
        // Published BEFORE `fs.init` (like KERNEL_INIT above) so the
        // filesystem's init hook can gauge what was actually accepted
        // (fuse_killpriv_negotiated et al.). Carries the full advertised
        // word, FUSE_INIT_EXT included when it rides the reply.
        let _ = NEGOTIATED_REPLY_FLAGS.set(reply_flags);

        // TODO: pass init_in to init, so the file system will know which flags are in use.
        let reply = match fs.init(request).await {
            Err(err) => {
                let init_out_header = fuse_out_header {
                    len: FUSE_OUT_HEADER_SIZE as u32,
                    error: err.into(),
                    unique: request.unique,
                };

                let init_out_header_data = get_bincode_config()
                    .serialize(&init_out_header)
                    .expect("won't happened");

                if let Err(err) = fuse_connection
                    .write_vectored::<_, Vec<u8>>(init_out_header_data, None, ReplySlot::Classical)
                    .await
                    .1
                {
                    error!("write error init out data to /dev/fuse failed {}", err);
                }

                return Err(err.into());
            }

            Ok(reply) => reply,
        };

        // L1 (IOPS-parity program): resolve the session transport geometry
        // BEFORE serializing the INIT reply, so the `max_background` /
        // `congestion_threshold` the kernel learns here always describe the
        // FUSE-over-io_uring rings that step 2 below will register. The
        // classical DEFAULT_MAX_BACKGROUND=12 was one of the two
        // multiplicative in-flight gates (with per-queue depth 4) that held
        // rand-4k iodepth workloads to ~18 effective of 256 offered.
        //
        // Geometry law (2026-08-04): the INIT reply's max_write/max_pages
        // are the NEGOTIATED values the geometry plan stands on — the
        // filesystem's desire gated by `fs.fuse.max_pages_limit` and the
        // payload budget ladder. Advertising anything else (the historical
        // blanket `max_pages = u16::MAX`) lets the kernel's REGISTER bound
        // `ring->max_payload_sz` exceed the registered ents on any
        // raised-sysctl box: every REGISTER refuses and the mount fails.
        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        let transport_geom = crate::raw::connection::fuse_over_uring::TransportGeometry::resolve(
            reply.max_write.get() as usize,
            self.mount_options.transport_buffer_cap_bytes,
            self.mount_options.max_background,
            self.mount_options.congestion_threshold,
        );
        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        let (max_background, congestion_threshold, negotiated_max_write, advertised_max_pages) = (
            transport_geom.max_background,
            transport_geom.congestion_threshold,
            u32::try_from(transport_geom.max_write).unwrap_or(reply.max_write.get()),
            transport_geom.max_pages,
        );
        // Non-over-uring builds keep the classical libfuse-era defaults
        // (max_background 12, congestion ¾ of it, blanket max_pages) — the
        // L1/geometry policy is an over-uring statement and does not apply
        // without rings.
        #[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
        let (max_background, congestion_threshold, negotiated_max_write, advertised_max_pages) =
            (12u16, 9u16, reply.max_write.get(), u16::MAX);

        let init_out = fuse_init_out {
            major: FUSE_KERNEL_VERSION,
            minor: reply_minor,
            // FUSE-4d: echoed VERBATIM (never clamped) and deliberately
            // never consulted — see `negotiate_max_readahead`.
            max_readahead: negotiate_max_readahead(init_in.max_readahead),
            flags: reply_flags,
            max_background,
            congestion_threshold,
            max_write: negotiated_max_write,
            time_gran: DEFAULT_TIME_GRAN,
            max_pages: advertised_max_pages,
            map_alignment: DEFAULT_MAP_ALIGNMENT,
            flags2,
            max_stack_depth: 0,
            request_timeout: 0,
            unused: [0; 3],
            time_min,
            time_max,
        };

        debug!("fuse init out {:?}", init_out);

        let out_header = fuse_out_header {
            len: (FUSE_OUT_HEADER_SIZE + FUSE_INIT_OUT_SIZE) as u32,
            error: 0,
            unique: request.unique,
        };

        let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_INIT_OUT_SIZE);

        get_bincode_config()
            .serialize_into(&mut data, &out_header)
            .expect("won't happened");
        get_bincode_config()
            .serialize_into(&mut data, &init_out)
            .expect("won't happened");

        // 1) Classical INIT reply first. Kernel fuse_uring_cmd requires
        //    fch->initialized before REGISTER (returns -EAGAIN otherwise).
        if let Err(err) = fuse_connection
            .write_vectored::<_, Vec<u8>>(data, None, ReplySlot::Classical)
            .await
            .1
        {
            error!("write init out data to /dev/fuse failed {}", err);

            return Err(err);
        }

        // 2) REGISTER all CPU queues (blocks until kernel fiq→uring switch) and
        //    arm the session uring path. Classical requests that arrived during
        //    INIT→REGISTER — and the kernel's permanent classical traffic
        //    (FORGET/INTERRUPT/resends) — are serviced by the classical
        //    sideband session `inner_mount` keeps on the primary connection.
        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        {
            fuse_connection.enable_fuse_over_uring(transport_geom)?;
            if let Some(pool) = fuse_connection.over_uring_pool() {
                pool.mark_ready();
            }
            eprintln!("FUSE-over-io_uring transport armed for this session");
            tracing::info!("FUSE-over-io_uring transport armed for this session");
        }

        if let Ok(val) = std::env::var("SQUEEZEFS_DAEMON_PIPE") {
            if let Ok(fd_num) = val.parse::<i32>() {
                let msg = "ready\n";
                let _ = unsafe { libc::write(fd_num, msg.as_ptr() as *const _, msg.len()) };
            }
        }

        debug!("fuse init done");

        // The dispatch buffers must size to what the kernel was TOLD, not
        // to the filesystem's (possibly larger) desire.
        Ok(NonZeroU32::new(negotiated_max_write).unwrap_or(reply.max_write))
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_lookup(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let name = match get_first_null_position(data) {
            None => {
                error!("lookup body has no null, request unique {}", request.unique);

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_lookup"), request.unique, async move {
            debug!(
                "lookup unique {} name {:?} in parent {}",
                request.unique, name, in_header.nodeid
            );

            let data = match fs.lookup(request, in_header.nodeid, &name).await {
                Err(err) => {
                    let out_header = fuse_out_header {
                        len: FUSE_OUT_HEADER_SIZE as u32,
                        error: err.into(),
                        unique: request.unique,
                    };

                    get_bincode_config()
                        .serialize(&out_header)
                        .expect("won't happened")
                }

                Ok(entry) => {
                    let entry_out: fuse_entry_out = entry.into();

                    debug!("lookup response {:?}", entry_out);

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &entry_out)
                        .expect("won't happened");

                    data
                }
            };

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    /// if Ok(true), quit the dispatch
    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_forget(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let forget_in = match get_bincode_config().deserialize::<fuse_forget_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_forget_in failed {}, request unique {}",
                    err, request.unique
                );

                // No userspace reply: over-uring COMMITs FORGET in the queue worker.
                return;
            }

            Ok(forget_in) => forget_in,
        };

        let fs = fs.clone();

        spawn(debug_span!("fuse_forget"), request.unique, async move {
            debug!(
                "forget unique {} inode {} nlookup {}",
                request.unique, in_header.nodeid, forget_in.nlookup
            );

            // Over-uring: ring entry already COMMITed in the queue worker (noreply).
            // Classical: no write. Only nlookup accounting remains here.
            fs.forget(request, in_header.nodeid, forget_in.nlookup)
                .await
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_getattr(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let getattr_in = match get_bincode_config().deserialize::<fuse_getattr_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_forget_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(getattr_in) => getattr_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_getattr"), request.unique, async move {
            debug!(
                "getattr unique {} inode {}",
                request.unique, in_header.nodeid
            );

            let fh = if getattr_in.getattr_flags & FUSE_GETATTR_FH > 0 {
                Some(getattr_in.fh)
            } else {
                None
            };

            let data = match fs
                .getattr(request, in_header.nodeid, fh, getattr_in.getattr_flags)
                .await
            {
                Err(err) => {
                    let out_header = fuse_out_header {
                        len: FUSE_OUT_HEADER_SIZE as u32,
                        error: err.into(),
                        unique: request.unique,
                    };

                    get_bincode_config()
                        .serialize(&out_header)
                        .expect("won't happened")
                }

                Ok(attr) => {
                    let attr_out = fuse_attr_out {
                        attr_valid: attr.ttl.as_secs(),
                        attr_valid_nsec: attr.ttl.subsec_nanos(),
                        dummy: getattr_in.dummy,
                        attr: attr.attr.into(),
                    };

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &attr_out)
                        .expect("won't happened");

                    data
                }
            };

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_setattr(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let setattr_in = match get_bincode_config().deserialize::<fuse_setattr_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_setattr_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(setattr_in) => setattr_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_setattr"), request.unique, async move {
            let set_attr = SetAttr::from(&setattr_in);

            let fh = if setattr_in.valid & FATTR_FH > 0 {
                Some(setattr_in.fh)
            } else {
                None
            };

            debug!(
                "setattr unique {} inode {} set_attr {:?}",
                request.unique, in_header.nodeid, set_attr
            );

            let data = match fs.setattr(request, in_header.nodeid, fh, set_attr).await {
                Err(err) => {
                    let out_header = fuse_out_header {
                        len: FUSE_OUT_HEADER_SIZE as u32,
                        error: err.into(),
                        unique: request.unique,
                    };

                    get_bincode_config()
                        .serialize(&out_header)
                        .expect("won't happened")
                }

                Ok(attr) => {
                    let attr_out: fuse_attr_out = attr.into();

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ATTR_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &attr_out)
                        .expect("won't happened");

                    data
                }
            };

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, fs))]
    async fn handle_readlink(&mut self, request: Request, in_header: fuse_in_header, fs: &Arc<FS>) {
        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_readlink"), request.unique, async move {
            debug!(
                "readlink unique {} inode {}",
                request.unique, in_header.nodeid
            );

            let data = match fs.readlink(request, in_header.nodeid).await {
                Err(err) => {
                    let out_header = fuse_out_header {
                        len: FUSE_OUT_HEADER_SIZE as u32,
                        error: err.into(),
                        unique: request.unique,
                    };

                    Either::Left(
                        get_bincode_config()
                            .serialize(&out_header)
                            .expect("won't happened"),
                    )
                }

                Ok(data) => {
                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + data.data.len()) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data_buf = Vec::with_capacity(FUSE_OUT_HEADER_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data_buf, &out_header)
                        .expect("won't happened");

                    Either::Right((data_buf, data.data, data.backing))
                }
            };

            let _ = resp_sender.send(data).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_symlink(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let (name, first_null_index) = match get_first_null_position(data) {
            None => {
                error!("symlink has no null, request unique {}", request.unique);

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => (OsString::from_vec(data[..index].to_vec()), index),
        };

        data = &data[first_null_index + 1..];

        let link_name = match get_first_null_position(data) {
            None => {
                error!(
                    "symlink has no second null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_symlink"), request.unique, async move {
            debug!(
                "symlink unique {} parent {} name {:?} link {:?}",
                request.unique, in_header.nodeid, name, link_name
            );

            let data = match fs
                .symlink(request, in_header.nodeid, &name, &link_name)
                .await
            {
                Err(err) => {
                    let out_header = fuse_out_header {
                        len: FUSE_OUT_HEADER_SIZE as u32,
                        error: err.into(),
                        unique: request.unique,
                    };

                    get_bincode_config()
                        .serialize(&out_header)
                        .expect("won't happened")
                }

                Ok(entry) => {
                    let entry_out: fuse_entry_out = entry.into();

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &entry_out)
                        .expect("won't happened");

                    data
                }
            };

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_mknod(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let mknod_in = match get_bincode_config().deserialize::<fuse_mknod_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_mknod_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(mknod_in) => mknod_in,
        };

        data = &data[FUSE_MKNOD_IN_SIZE..];

        let name = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_mknod_in body doesn't have null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_mknod"), request.unique, async move {
            debug!(
                "mknod unique {} parent {} name {:?} {:?}",
                request.unique, in_header.nodeid, name, mknod_in
            );

            match fs
                .mknod(
                    request,
                    in_header.nodeid,
                    &name,
                    mknod_in.mode,
                    mknod_in.rdev,
                )
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;
                }

                Ok(entry) => {
                    let entry_out: fuse_entry_out = entry.into();

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &entry_out)
                        .expect("won't happened");

                    let _ = resp_sender.send(Either::Left(data)).await;
                }
            }
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_mkdir(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let mkdir_in = match get_bincode_config().deserialize::<fuse_mkdir_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_mknod_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(mkdir_in) => mkdir_in,
        };

        data = &data[FUSE_MKDIR_IN_SIZE..];

        let name = match get_first_null_position(data) {
            None => {
                error!(
                    "deserialize fuse_mknod_in doesn't have null unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_mkdir"), request.unique, async move {
            debug!(
                "mkdir unique {} parent {} name {:?} {:?}",
                request.unique, in_header.nodeid, name, mkdir_in
            );

            match fs
                .mkdir(
                    request,
                    in_header.nodeid,
                    &name,
                    mkdir_in.mode,
                    mkdir_in.umask,
                )
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;
                }

                Ok(entry) => {
                    let entry_out: fuse_entry_out = entry.into();

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &entry_out)
                        .expect("won't happened");

                    let _ = resp_sender.send(Either::Left(data)).await;
                }
            }
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_unlink(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let name = match get_first_null_position(data) {
            None => {
                error!(
                    "unlink body doesn't have null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_unlink"), request.unique, async move {
            debug!(
                "unlink unique {} parent {} name {:?}",
                request.unique, in_header.nodeid, name
            );

            let resp_value = if let Err(err) = fs.unlink(request, in_header.nodeid, &name).await {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_rmdir(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let name = match get_first_null_position(data) {
            None => {
                error!(
                    "rmdir body doesn't have null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_rmdir"), request.unique, async move {
            debug!(
                "rmdir unique {} parent {} name {:?}",
                request.unique, in_header.nodeid, name
            );

            let resp_value = if let Err(err) = fs.rmdir(request, in_header.nodeid, &name).await {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_rename(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let rename_in = match get_bincode_config().deserialize::<fuse_rename_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_rename_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(rename_in) => rename_in,
        };

        data = &data[FUSE_RENAME_IN_SIZE..];

        let (name, first_null_index) = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_rename_in body doesn't have null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => (OsString::from_vec(data[..index].to_vec()), index),
        };

        data = &data[first_null_index + 1..];

        let new_name = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_rename_in body doesn't have null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_rename"), request.unique, async move {
            debug!(
                "rename unique {} parent {} name {:?} new parent {} new name {:?}",
                request.unique, in_header.nodeid, name, rename_in.newdir, new_name
            );

            let resp_value = if let Err(err) = fs
                .rename(
                    request,
                    in_header.nodeid,
                    &name,
                    rename_in.newdir,
                    &new_name,
                )
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_link(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let link_in = match get_bincode_config().deserialize::<fuse_link_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_link_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(link_in) => link_in,
        };

        data = &data[FUSE_LINK_IN_SIZE..];

        let name = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_link_in body doesn't have null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_link"), request.unique, async move {
            debug!(
                "link unique {} inode {} new parent {} new name {:?}",
                request.unique, link_in.oldnodeid, in_header.nodeid, name
            );

            match fs
                .link(request, link_in.oldnodeid, in_header.nodeid, &name)
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;
                }

                Ok(entry) => {
                    let entry_out: fuse_entry_out = entry.into();

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &entry_out)
                        .expect("won't happened");

                    let _ = resp_sender.send(Either::Left(data)).await;
                }
            }
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_open(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let open_in = match get_bincode_config().deserialize::<fuse_open_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_open_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(open_in) => open_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_open"), request.unique, async move {
            debug!(
                "open unique {} inode {} flags {} open_flags {}",
                request.unique, in_header.nodeid, open_in.flags, open_in.open_flags
            );

            let opened = match fs
                .open(request, in_header.nodeid, open_in.flags, open_in.open_flags)
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(opened) => opened,
            };

            let open_out: fuse_open_out = opened.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &open_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_read(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        // read_transport_phase_ns: dispatch stamp + the uring arrival the
        // dispatch loop just parked (0 = classical delivery — the
        // arrival-anchored phases skip). Error replies record nothing.
        let dispatch_t0 = std::time::Instant::now();
        #[cfg(target_os = "linux")]
        let arrival_ns = self
            .fuse_connection
            .as_ref()
            .map(|c| c.take_read_arrival_ns())
            .unwrap_or(0);
        #[cfg(not(target_os = "linux"))]
        let arrival_ns = 0u64;

        let read_in = match get_bincode_config().deserialize::<fuse_read_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_read_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(read_in) => read_in,
        };

        let resp_sender = self.reply_tx(&request);
        let fs = fs.clone();
        // P2 per-op economy: on an armed over-uring session the READ reply
        // is completed in place from the handler task (a synchronous
        // COMMIT enqueue) instead of hopping through the unbounded reply
        // channel + the per-queue reply task — one task wake per op saved.
        let reply_conn = self.fuse_connection.clone();

        // Lever 1 (transport-ingress dispatch): same-lane spawn_local when
        // this dispatch loop already runs on a TPC lane — see `spawn_read`.
        spawn_read(
            debug_span!("fuse_read"),
            request.unique,
            read_handler_body(
                fs,
                reply_conn,
                resp_sender,
                request,
                in_header.nodeid,
                read_in,
                dispatch_t0,
                arrival_ns,
            ),
        );
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_write(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        uring_payload: Option<Bytes>,
        fs: &Arc<FS>,
    ) {
        // write_transport_phase_ns: dispatch stamp + the uring arrival the
        // dispatch loop just parked (0 = classical delivery — the
        // arrival-anchored phases skip). Error replies record nothing.
        let dispatch_t0 = std::time::Instant::now();
        #[cfg(target_os = "linux")]
        let arrival_ns = self
            .fuse_connection
            .as_ref()
            .map(|c| c.take_write_arrival_ns())
            .unwrap_or(0);
        #[cfg(not(target_os = "linux"))]
        let arrival_ns = 0u64;

        let write_in = match get_bincode_config().deserialize::<fuse_write_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_write_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(write_in) => write_in,
        };

        data = &data[FUSE_WRITE_IN_SIZE..];

        let payload = match uring_payload {
            // FUSE-over-io_uring delivery: the body was NOT copied into the
            // session buffer (transport zero-copy, §5.4) — it rides `p`,
            // possibly as a payload lease over the registered uring buffer.
            // Validate the header's size against the payload itself (the
            // kernel fills payload_sz from the same request).
            Some(p) => {
                if write_in.size as usize != p.len() {
                    // D14 held-slot delivery (dispatch-before-extraction):
                    // on a zc-armed session the WRITE payload stays in
                    // the transport's SPARSE SLOT — the placeholder here
                    // is empty and the connection's held table carries
                    // the authoritative length, which must agree with
                    // the header. The filesystem consumes the payload
                    // via the connection's slot source (store/extract).
                    // Every other mismatch stays the EINVAL it always
                    // was.
                    #[cfg(target_os = "linux")]
                    let held = p.is_empty()
                        && self.fuse_connection.as_ref().is_some_and(|c| {
                            c.zc_write_held_len(request.slot) == Some(write_in.size)
                        });
                    #[cfg(not(target_os = "linux"))]
                    let held = false;
                    if !held {
                        error!(
                            "fuse_write_in size {} != uring payload len {}",
                            write_in.size,
                            p.len()
                        );

                        reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request))
                            .await;

                        return;
                    }
                }

                p
            }
            // Classical delivery (FUSE_INIT window handoff only after arm):
            // the body is in the session buffer, exactly as before.
            None => {
                if write_in.size as usize != data.len() {
                    error!("fuse_write_in body len is invalid");

                    reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request))
                        .await;

                    return;
                }

                Bytes::copy_from_slice(data)
            }
        };

        let resp_sender = self.reply_tx(&request);
        let fs = fs.clone();
        // P2 per-op economy, WRITE twin (transport-ingress campaign): on
        // an armed over-uring session the WRITE reply is completed in
        // place from the handler task (a synchronous COMMIT enqueue;
        // the §5.4 commit gate still parks it while the payload lease
        // lives) instead of hopping through the unbounded reply channel
        // + the per-queue reply task — one cross-thread task wake per op
        // saved on the write wall's ACK chain.
        let reply_conn = self.fuse_connection.clone();

        spawn(
            debug_span!("fuse_write"),
            request.unique,
            write_handler_body(
                fs,
                reply_conn,
                resp_sender,
                request,
                in_header.nodeid,
                write_in,
                payload,
                dispatch_t0,
                arrival_ns,
            ),
        );
    }

    #[instrument(level = "debug", skip(self, fs))]
    async fn handle_statfs(&mut self, request: Request, in_header: fuse_in_header, fs: &Arc<FS>) {
        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_statfs"), request.unique, async move {
            debug!(
                "statfs unique {} inode {}",
                request.unique, in_header.nodeid
            );

            let fs_stat = match fs.statfs(request, in_header.nodeid).await {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(fs_stat) => fs_stat,
            };

            let statfs_out: fuse_statfs_out = fs_stat.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &statfs_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_release(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let release_in = match get_bincode_config().deserialize::<fuse_release_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_release_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(release_in) => release_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_release"), request.unique, async move {
            let flush = release_in.release_flags & FUSE_RELEASE_FLUSH > 0;

            debug!(
                "release unique {} inode {} fh {} flags {} lock_owner {} flush {}",
                request.unique,
                in_header.nodeid,
                release_in.fh,
                release_in.flags,
                release_in.lock_owner,
                flush
            );

            let resp_value = if let Err(err) = fs
                .release(
                    request,
                    in_header.nodeid,
                    release_in.fh,
                    release_in.flags,
                    release_in.lock_owner,
                    flush,
                )
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_fsync(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let fsync_in = match get_bincode_config().deserialize::<fuse_fsync_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_fsync_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(fsync_in) => fsync_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_fsync"), request.unique, async move {
            let data_sync = fsync_in.fsync_flags & 1 > 0;

            debug!(
                "fsync unique {} inode {} fh {} data_sync {}",
                request.unique, in_header.nodeid, fsync_in.fh, data_sync
            );

            let resp_value = if let Err(err) = fs
                .fsync(request, in_header.nodeid, fsync_in.fh, data_sync)
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_setxattr(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let setxattr_in = match get_bincode_config().deserialize::<fuse_setxattr_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_setxattr_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(setxattr_in) => setxattr_in,
        };

        data = &data[FUSE_SETXATTR_IN_SIZE..];

        let (name, first_null_index) = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_setxattr_in body has no null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => (OsString::from_vec(data[..index].to_vec()), index),
        };

        data = &data[first_null_index + 1..];

        // setxattr "size" field specifies size of only "Value" part of data
        if setxattr_in.size as usize != data.len() {
            error!(
                "fuse_setxattr_in value field data length is not right, request unique {} setxattr_in.size={} data.len={}", request.unique, setxattr_in.size, data.len());

            reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

            return;
        }

        let data = data.to_vec();

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_setxattr"), request.unique, async move {
            debug!(
                "setxattr unique {} inode {}",
                request.unique, in_header.nodeid
            );

            // TODO handle os X argument
            let resp_value = if let Err(err) = fs
                .setxattr(
                    request,
                    in_header.nodeid,
                    &name,
                    &data,
                    setxattr_in.flags,
                    0,
                )
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_getxattr(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let getxattr_in = match get_bincode_config().deserialize::<fuse_getxattr_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_getxattr_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(getxattr_in) => getxattr_in,
        };

        data = &data[FUSE_GETXATTR_IN_SIZE..];

        let name = match get_first_null_position(data) {
            None => {
                error!("fuse_getxattr_in body has no null {}", request.unique);

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_getxattr"), request.unique, async move {
            debug!(
                "getxattr unique {} inode {}",
                request.unique, in_header.nodeid
            );

            let xattr = match fs
                .getxattr(request, in_header.nodeid, &name, getxattr_in.size)
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(xattr) => xattr,
            };

            let data = match xattr {
                ReplyXAttr::Size(size) => {
                    let getxattr_out = fuse_getxattr_out { size, _padding: 0 };

                    // Size probe (caller passed size == 0): a SUCCESS reply
                    // carrying fuse_getxattr_out{size}. A positive errno here
                    // (this used to say ERANGE) is a malformed reply — the
                    // kernel rejects it and getxattr(2) fails with EINVAL,
                    // breaking every probing caller (getfattr, ls, rsync -X).
                    // ERANGE is only correct when the caller's buffer is too
                    // small, which the filesystem signals by returning
                    // Err(ERANGE) from its getxattr handler instead.
                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_GETXATTR_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &getxattr_out)
                        .expect("won't happened");

                    Either::Left(data)
                }

                ReplyXAttr::Data(xattr_data) => {
                    // TODO check is right way or not
                    // TODO should we check data length or not
                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + xattr_data.len()) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");

                    Either::Right((data, xattr_data, None))
                }
            };

            let _ = resp_sender.send(data).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_listxattr(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let listxattr_in = match get_bincode_config().deserialize::<fuse_getxattr_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_getxattr_in in listxattr failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(listxattr_in) => listxattr_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_listxattr"), request.unique, async move {
            debug!(
                "listxattr unique {} inode {} size {}",
                request.unique, in_header.nodeid, listxattr_in.size
            );

            let xattr = match fs
                .listxattr(request, in_header.nodeid, listxattr_in.size)
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(xattr) => xattr,
            };

            let data = match xattr {
                ReplyXAttr::Size(size) => {
                    let getxattr_out = fuse_getxattr_out { size, _padding: 0 };

                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + FUSE_GETXATTR_OUT_SIZE) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_STATFS_OUT_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &getxattr_out)
                        .expect("won't happened");

                    Either::Left(data)
                }

                ReplyXAttr::Data(xattr_data) => {
                    // TODO check is right way or not
                    // TODO should we check data length or not
                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + xattr_data.len()) as u32,
                        error: 0,
                        unique: request.unique,
                    };

                    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE);

                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");

                    Either::Right((data, xattr_data, None))
                }
            };

            let _ = resp_sender.send(data).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_removexattr(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let name = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse removexattr body has no null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(
            debug_span!("fuse_removexattr"),
            request.unique,
            async move {
                debug!(
                    "removexattr unique {} inode {}",
                    request.unique, in_header.nodeid
                );

                let resp_value =
                    if let Err(err) = fs.removexattr(request, in_header.nodeid, &name).await {
                        err.into()
                    } else {
                        0
                    };

                let out_header = fuse_out_header {
                    len: FUSE_OUT_HEADER_SIZE as u32,
                    error: resp_value,
                    unique: request.unique,
                };

                let data = get_bincode_config()
                    .serialize(&out_header)
                    .expect("won't happened");

                let _ = resp_sender.send(Either::Left(data)).await;
            },
        );
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_flush(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let flush_in = match get_bincode_config().deserialize::<fuse_flush_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_flush_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(flush_in) => flush_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_flush"), request.unique, async move {
            debug!(
                "flush unique {} inode {} fh {} lock_owner {}",
                request.unique, in_header.nodeid, flush_in.fh, flush_in.lock_owner
            );

            let resp_value = if let Err(err) = fs
                .flush(request, in_header.nodeid, flush_in.fh, flush_in.lock_owner)
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_opendir(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let open_in = match get_bincode_config().deserialize::<fuse_open_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_open_in in opendir failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(open_in) => open_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_opendir"), request.unique, async move {
            debug!(
                "opendir unique {} inode {} flags {}",
                request.unique, in_header.nodeid, open_in.flags
            );

            let reply_open = match fs.opendir(request, in_header.nodeid, open_in.flags).await {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(reply_open) => reply_open,
            };

            let open_out: fuse_open_out = reply_open.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &open_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_readdir(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        if self.mount_options.force_readdir_plus {
            reply_error_in_place(libc::ENOSYS.into(), request, self.reply_tx(&request)).await;

            return;
        }

        let read_in = match get_bincode_config().deserialize::<fuse_read_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_read_in in readdir failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(read_in) => read_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_readdir"), request.unique, async move {
            debug!(
                "readdir unique {} inode {} fh {} offset {}",
                request.unique, in_header.nodeid, read_in.fh, read_in.offset
            );

            let reply_readdir = match fs
                .readdir(request, in_header.nodeid, read_in.fh, read_in.offset as i64)
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(reply_readdir) => reply_readdir,
            };

            let max_size = read_in.size as usize;

            let mut entry_data = Vec::with_capacity(max_size);

            let entries = reply_readdir.entries;
            let mut entries = pin!(entries);

            while let Some(entry) = entries.next().await {
                let entry = match entry {
                    Err(err) => {
                        reply_error_in_place(err, request, resp_sender).await;

                        return;
                    }

                    Ok(entry) => entry,
                };

                let name = &entry.name;

                let dir_entry_size = FUSE_DIRENT_SIZE + name.len();

                let padding_size = get_padding_size(dir_entry_size);

                if entry_data.len() + dir_entry_size > max_size {
                    break;
                }

                let dir_entry = fuse_dirent {
                    ino: entry.inode,
                    off: entry.offset as u64,
                    namelen: name.len() as u32,
                    // learn from fuse-rs and golang bazil.org fuse DirentType
                    r#type: mode_from_kind_and_perm(entry.kind, 0) >> 12,
                };

                get_bincode_config()
                    .serialize_into(&mut entry_data, &dir_entry)
                    .expect("won't happened");

                entry_data.extend_from_slice(name.as_bytes());

                // padding
                entry_data.resize(entry_data.len() + padding_size, 0);
            }

            // TODO find a way to avoid multi allocate

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + entry_data.len()) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");

            let _ = resp_sender
                .send(Either::Right((data, entry_data.into(), None)))
                .await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_releasedir(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let release_in = match get_bincode_config().deserialize::<fuse_release_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_release_in in releasedir failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(release_in) => release_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_releasedir"), request.unique, async move {
            debug!(
                "releasedir unique {} inode {} fh {} flags {}",
                request.unique, in_header.nodeid, release_in.fh, release_in.flags
            );

            let resp_value = if let Err(err) = fs
                .releasedir(request, in_header.nodeid, release_in.fh, release_in.flags)
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_fsyncdir(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let fsync_in = match get_bincode_config().deserialize::<fuse_fsync_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_fsync_in in fsyncdir failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(fsync_in) => fsync_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_fsyncdir"), request.unique, async move {
            let data_sync = fsync_in.fsync_flags & 1 > 0;

            debug!(
                "fsyncdir unique {} inode {} fh {} data_sync {}",
                request.unique, in_header.nodeid, fsync_in.fh, data_sync
            );

            let resp_value = if let Err(err) = fs
                .fsyncdir(request, in_header.nodeid, fsync_in.fh, data_sync)
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[cfg(feature = "file-lock")]
    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_getlk(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let getlk_in = match get_bincode_config().deserialize::<fuse_lk_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_lk_in in getlk failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(getlk_in) => getlk_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_getlk"), request.unique, async move {
            debug!(
                "getlk unique {} inode {} {:?}",
                request.unique, in_header.nodeid, getlk_in
            );

            let reply_lock = match fs
                .getlk(
                    request,
                    in_header.nodeid,
                    getlk_in.fh,
                    getlk_in.owner,
                    getlk_in.lk.start,
                    getlk_in.lk.end,
                    getlk_in.lk.r#type,
                    getlk_in.lk.pid,
                )
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(reply_lock) => reply_lock,
            };

            let getlk_out: fuse_lk_out = reply_lock.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_LK_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_LK_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &getlk_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[cfg(feature = "file-lock")]
    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_setlk(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        block: bool,
        fs: &Arc<FS>,
    ) {
        let setlk_in = match get_bincode_config().deserialize::<fuse_lk_in>(data) {
            Err(err) => {
                let opcode = if block {
                    fuse_opcode::FUSE_SETLKW
                } else {
                    fuse_opcode::FUSE_SETLK
                };

                error!(
                    "deserialize fuse_lk_in in {:?} failed {}, request unique {}",
                    opcode, err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(setlk_in) => setlk_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_setlk"), request.unique, async move {
            debug!(
                "setlk unique {} inode {} block {} {:?}",
                request.unique, in_header.nodeid, block, setlk_in
            );

            let resp = if let Err(err) = fs
                .setlk(
                    request,
                    in_header.nodeid,
                    setlk_in.fh,
                    setlk_in.owner,
                    setlk_in.lk.start,
                    setlk_in.lk.end,
                    setlk_in.lk.r#type,
                    setlk_in.lk.pid,
                    block,
                )
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("can't serialize into vec");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_access(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let access_in = match get_bincode_config().deserialize::<fuse_access_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_access_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(access_in) => access_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_access"), request.unique, async move {
            debug!(
                "access unique {} inode {} mask {}",
                request.unique, in_header.nodeid, access_in.mask
            );

            let resp_value =
                if let Err(err) = fs.access(request, in_header.nodeid, access_in.mask).await {
                    err.into()
                } else {
                    0
                };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            debug!("access response {}", resp_value);

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_create(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let create_in = match get_bincode_config().deserialize::<fuse_create_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_create_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(create_in) => create_in,
        };

        data = &data[FUSE_CREATE_IN_SIZE..];

        let name = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_create_in body has no null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_create"), request.unique, async move {
            debug!(
                "create unique {} parent {} name {:?} mode {} flags {}",
                request.unique, in_header.nodeid, name, create_in.mode, create_in.flags
            );

            let created = match fs
                .create(
                    request,
                    in_header.nodeid,
                    &name,
                    create_in.mode,
                    create_in.flags,
                )
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(created) => created,
            };

            let (entry_out, open_out): (fuse_entry_out, fuse_open_out) = created.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE + FUSE_OPEN_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data =
                Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_ENTRY_OUT_SIZE + FUSE_OPEN_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &entry_out)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &open_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    /// FUSE_INTERRUPT is a **no-reply** request (`fs/fuse/dev.c`: the
    /// kernel's `fuse_dev_do_write` has no case for it, and libfuse's
    /// `do_interrupt` returns without a reply).
    ///
    /// FUSE-3i: this daemon used to reply anyway. The kernel treats a
    /// reply to an interrupt as a reply to an *unknown* unique, which
    /// means its `fc->no_interrupt` latch — the optimization that stops
    /// the kernel sending interrupts to daemons that ignore them — never
    /// engages, so every interruptible wait keeps paying interrupt
    /// traffic forever. The filesystem hook still runs; only the reply
    /// is gone (the handle is created in no-reply mode so the FUSE-2
    /// drop guard does not synthesize one either).
    async fn handle_interrupt(&mut self, request: Request, data: &[u8], fs: &Arc<FS>) {
        let interrupt_in = match get_bincode_config().deserialize::<fuse_interrupt_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_interrupt_in failed {}, request unique {}",
                    err, request.unique
                );

                return;
            }

            Ok(interrupt_in) => interrupt_in,
        };

        let fs = fs.clone();

        spawn(debug_span!("fuse_interrupt"), request.unique, async move {
            debug!(
                "interrupt_in unique {} interrupt unique {}",
                request.unique, interrupt_in.unique
            );

            if let Err(err) = fs.interrupt(request, interrupt_in.unique).await {
                debug!(
                    "interrupt hook for unique {} returned {err:?}; INTERRUPT is no-reply, \
                     nothing is sent to the kernel",
                    interrupt_in.unique
                );
            }
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_bmap(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let bmap_in = match get_bincode_config().deserialize::<fuse_bmap_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_bmap_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(bmap_in) => bmap_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_bmap"), request.unique, async move {
            debug!(
                "bmap unique {} inode {} block size {} idx {}",
                request.unique, in_header.nodeid, bmap_in.blocksize, bmap_in.block
            );

            let reply_bmap = match fs
                .bmap(request, in_header.nodeid, bmap_in.blocksize, bmap_in.block)
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(reply_bmap) => reply_bmap,
            };

            let bmap_out: fuse_bmap_out = reply_bmap.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_BMAP_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_BMAP_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &bmap_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_ioctl(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let ioctl_in = match get_bincode_config().deserialize::<fuse_ioctl_in>(data) {
            Err(err) => {
                error!("deserialize fuse_ioctl_in failed {}", err);
                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;
                return;
            }
            Ok(ioctl_in) => ioctl_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_ioctl"), request.unique, async move {
            debug!(
                "ioctl unique {} inode {} cmd {}",
                request.unique, in_header.nodeid, ioctl_in.cmd
            );

            let res = fs
                .ioctl(
                    request,
                    in_header.nodeid,
                    ioctl_in.fh,
                    ioctl_in.flags,
                    ioctl_in.cmd,
                    ioctl_in.arg,
                    ioctl_in.in_size,
                    ioctl_in.out_size,
                )
                .await;

            let data = match res {
                Err(err) => {
                    let out_header = fuse_out_header {
                        len: FUSE_OUT_HEADER_SIZE as u32,
                        error: err.into(),
                        unique: request.unique,
                    };
                    get_bincode_config()
                        .serialize(&out_header)
                        .expect("won't happened")
                }
                Ok(reply) => {
                    let out_header = fuse_out_header {
                        len: (FUSE_OUT_HEADER_SIZE + mem::size_of::<fuse_ioctl_out>()) as u32,
                        error: 0,
                        unique: request.unique,
                    };
                    let ioctl_out = fuse_ioctl_out {
                        result: reply.result,
                        flags: reply.flags,
                        in_iovs: reply.in_iovs,
                        out_iovs: reply.out_iovs,
                    };
                    let mut data =
                        Vec::with_capacity(FUSE_OUT_HEADER_SIZE + mem::size_of::<fuse_ioctl_out>());
                    get_bincode_config()
                        .serialize_into(&mut data, &out_header)
                        .expect("won't happened");
                    get_bincode_config()
                        .serialize_into(&mut data, &ioctl_out)
                        .expect("won't happened");
                    data
                }
            };
            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_poll(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let poll_in = match get_bincode_config().deserialize::<fuse_poll_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_poll_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(poll_in) => poll_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        let notify = self.get_notify();

        spawn(debug_span!("fuse_poll"), request.unique, async move {
            debug!(
                "poll unique {} inode {} {:?}",
                request.unique, in_header.nodeid, poll_in
            );

            let kh = if poll_in.flags & FUSE_POLL_SCHEDULE_NOTIFY > 0 {
                Some(poll_in.kh)
            } else {
                None
            };

            let reply_poll = match fs
                .poll(
                    request,
                    in_header.nodeid,
                    poll_in.fh,
                    kh,
                    poll_in.flags,
                    poll_in.events,
                    &notify,
                )
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(reply_poll) => reply_poll,
            };

            let poll_out: fuse_poll_out = reply_poll.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_POLL_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_POLL_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &poll_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_notify_reply(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        // FUSE_NOTIFY_REPLY (opcode 41) is the kernel's answer to OUR
        // `NOTIFY_RETRIEVE`; the protocol expects no reply, so this
        // handle owes nothing and must not synthesize one when the
        // handler finishes silently. (Its error arm below is today's
        // behavior, kept verbatim.)
        let resp_sender = self.no_reply_tx();

        let notify_retrieve_in =
            match get_bincode_config().deserialize::<fuse_notify_retrieve_in>(data) {
                Err(err) => {
                    error!(
                        "deserialize fuse_notify_retrieve_in failed {}, request unique {}",
                        err, request.unique
                    );

                    // FUSE_NOTIFY_REPLY carries no reply (it IS the
                    // answer to our own NOTIFY_RETRIEVE).
                    return;
                }

                Ok(notify_retrieve_in) => notify_retrieve_in,
            };

        let Some(body) = notify_retrieve_body(data, notify_retrieve_in.size as usize) else {
            error!(
                "fuse_notify_retrieve unique {} body is short ({} bytes, need {} + {}); ignoring",
                request.unique,
                data.len(),
                FUSE_NOTIFY_RETRIEVE_IN_SIZE,
                notify_retrieve_in.size
            );

            return;
        };
        data = body;

        let data = data.to_vec();

        let fs = fs.clone();

        spawn(
            debug_span!("fuse_notify_reply"),
            request.unique,
            async move {
                if let Err(err) = fs
                    .notify_reply(
                        request,
                        in_header.nodeid,
                        notify_retrieve_in.offset,
                        data.into(),
                    )
                    .await
                {
                    reply_error_in_place(err, request, resp_sender).await;
                }
            },
        );
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_batch_forget(
        &mut self,
        request: Request,
        _in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let batch_forget_in = match get_bincode_config().deserialize::<fuse_batch_forget_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_batch_forget_in failed {}, request unique {}",
                    err, request.unique
                );

                // No userspace reply: over-uring COMMITs in the queue worker.
                return;
            }

            Ok(batch_forget_in) => batch_forget_in,
        };

        let mut forgets = vec![];

        let Some(rest) = batch_forget_body(data) else {
            error!(
                "fuse_batch_forget unique {} body is short ({} bytes, need {}); ignoring",
                request.unique,
                data.len(),
                FUSE_BATCH_FORGET_IN_SIZE
            );

            return;
        };
        data = rest;

        while data.len() >= FUSE_FORGET_ONE_SIZE {
            match get_bincode_config().deserialize::<fuse_forget_one>(data) {
                Err(err) => {
                    error!("deserialize fuse_batch_forget_in body fuse_forget_one failed {}, request unique {}", err, request.unique);

                    return;
                }

                Ok(forget_one) => {
                    data = &data[FUSE_FORGET_ONE_SIZE..];

                    forgets.push(forget_one);
                }
            }
        }

        if forgets.len() != batch_forget_in.count as usize {
            error!(
                "fuse_forget_one count != fuse_batch_forget_in.count, request unique {}",
                request.unique
            );

            return;
        }

        let fs = fs.clone();

        spawn(
            debug_span!("fuse_batch_forget"),
            request.unique,
            async move {
                // FUSE-3k: carry the PER-ENTRY nlookup. `fuse_forget_one` is
                // `{ nodeid, nlookup }` and the second word used to be dropped
                // here, so a filesystem keeping lookup references could not
                // honor them on the one path that returns them in bulk.
                let inodes = forgets
                    .into_iter()
                    .map(|forget_one| (forget_one.nodeid, forget_one.nlookup))
                    .collect::<Vec<_>>();

                debug!("batch_forget unique {} inodes {:?}", request.unique, inodes);

                // Over-uring: ring entry already COMMITed in the queue worker (noreply).
                fs.batch_forget(request, &inodes).await
            },
        );
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_fallocate(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let fallocate_in = match get_bincode_config().deserialize::<fuse_fallocate_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_fallocate_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(fallocate_in) => fallocate_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_fallocate"), request.unique, async move {
            debug!(
                "fallocate unique {} inode {} {:?}",
                request.unique, in_header.nodeid, fallocate_in
            );

            let resp_value = if let Err(err) = fs
                .fallocate(
                    request,
                    in_header.nodeid,
                    fallocate_in.fh,
                    fallocate_in.offset,
                    fallocate_in.length,
                    fallocate_in.mode,
                )
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_readdirplus(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let readdirplus_in = match get_bincode_config().deserialize::<fuse_read_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_read_in in readdirplus failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(readdirplus_in) => readdirplus_in,
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(
            debug_span!("fuse_readdirplus"),
            request.unique,
            async move {
                debug!(
                    "readdirplus unique {} parent {} {:?}",
                    request.unique, in_header.nodeid, readdirplus_in
                );

                let directory_plus = match fs
                    .readdirplus(
                        request,
                        in_header.nodeid,
                        readdirplus_in.fh,
                        readdirplus_in.offset,
                        readdirplus_in.lock_owner,
                    )
                    .await
                {
                    Err(err) => {
                        reply_error_in_place(err, request, resp_sender).await;

                        return;
                    }

                    Ok(directory_plus) => directory_plus,
                };

                let max_size = readdirplus_in.size as usize;

                let mut entry_data = Vec::with_capacity(max_size);

                let entries = directory_plus.entries;
                let mut entries = pin!(entries);

                while let Some(entry) = entries.next().await {
                    let entry = match entry {
                        Err(err) => {
                            reply_error_in_place(err, request, resp_sender).await;

                            return;
                        }

                        Ok(entry) => entry,
                    };

                    let name = &entry.name;

                    let dir_entry_size = FUSE_DIRENTPLUS_SIZE + name.len();

                    let padding_size = get_padding_size(dir_entry_size);

                    if entry_data.len() + dir_entry_size > max_size {
                        break;
                    }

                    let attr = entry.attr;

                    let dir_entry = fuse_direntplus {
                        entry_out: fuse_entry_out {
                            nodeid: attr.ino,
                            generation: entry.generation,
                            entry_valid: entry.entry_ttl.as_secs(),
                            attr_valid: entry.attr_ttl.as_secs(),
                            entry_valid_nsec: entry.entry_ttl.subsec_nanos(),
                            attr_valid_nsec: entry.attr_ttl.subsec_nanos(),
                            attr: attr.into(),
                        },
                        dirent: fuse_dirent {
                            ino: entry.inode,
                            off: entry.offset as u64,
                            namelen: name.len() as u32,
                            // learn from fuse-rs and golang bazil.org fuse DirentType
                            r#type: mode_from_kind_and_perm(entry.kind, 0) >> 12,
                        },
                    };

                    get_bincode_config()
                        .serialize_into(&mut entry_data, &dir_entry)
                        .expect("won't happened");

                    entry_data.extend_from_slice(name.as_bytes());

                    // padding
                    entry_data.resize(entry_data.len() + padding_size, 0);
                }

                // TODO find a way to avoid multi allocate

                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + entry_data.len()) as u32,
                    error: 0,
                    unique: request.unique,
                };

                let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("won't happened");

                let _ = resp_sender
                    .send(Either::Right((data, entry_data.into(), None)))
                    .await;
            },
        );
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_rename2(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let rename2_in = match get_bincode_config().deserialize::<fuse_rename2_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_rename2_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(rename2_in) => rename2_in,
        };

        data = &data[FUSE_RENAME2_IN_SIZE..];

        let (old_name, index) = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_rename2_in body doesn't have null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => (OsString::from_vec(data[..index].to_vec()), index),
        };

        data = &data[index + 1..];

        let new_name = match get_first_null_position(data) {
            None => {
                error!(
                    "fuse_rename2_in body doesn't have second null, request unique {}",
                    request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.reply_tx(&request);
        let fs = fs.clone();

        spawn(debug_span!("fuse_rename2"), request.unique, async move {
            debug!(
                "rename2 unique {} parent {} name {:?} new parent {} new name {:?} flags {}",
                request.unique,
                in_header.nodeid,
                old_name,
                rename2_in.newdir,
                new_name,
                rename2_in.flags
            );

            let resp_value = if let Err(err) = fs
                .rename2(
                    request,
                    in_header.nodeid,
                    &old_name,
                    rename2_in.newdir,
                    &new_name,
                    rename2_in.flags,
                )
                .await
            {
                err.into()
            } else {
                0
            };

            let out_header = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: resp_value,
                unique: request.unique,
            };

            let data = get_bincode_config()
                .serialize(&out_header)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_lseek(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let mut resp_sender = self.reply_tx(&request);

        let lseek_in = match get_bincode_config().deserialize::<fuse_lseek_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_lseek_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(lseek_in) => lseek_in,
        };

        let fs = fs.clone();

        spawn(debug_span!("fuse_lseek"), request.unique, async move {
            debug!(
                "lseek unique {} inode {} {:?}",
                request.unique, in_header.nodeid, lseek_in
            );

            let reply_lseek = match fs
                .lseek(
                    request,
                    in_header.nodeid,
                    lseek_in.fh,
                    lseek_in.offset,
                    lseek_in.whence,
                )
                .await
            {
                Err(err) => {
                    reply_error_in_place(err, request, resp_sender).await;

                    return;
                }

                Ok(reply_lseek) => reply_lseek,
            };

            let lseek_out: fuse_lseek_out = reply_lseek.into();

            let out_header = fuse_out_header {
                len: (FUSE_OUT_HEADER_SIZE + FUSE_LSEEK_OUT_SIZE) as u32,
                error: 0,
                unique: request.unique,
            };

            let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_OPEN_OUT_SIZE);

            get_bincode_config()
                .serialize_into(&mut data, &out_header)
                .expect("won't happened");
            get_bincode_config()
                .serialize_into(&mut data, &lseek_out)
                .expect("won't happened");

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(level = "debug", skip(self, data, fs))]
    async fn handle_copy_file_range(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let mut resp_sender = self.reply_tx(&request);

        let copy_file_range_in = match get_bincode_config()
            .deserialize::<fuse_copy_file_range_in>(data)
        {
            Err(err) => {
                error!(
                    "deserialize fuse_copy_file_range_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, self.reply_tx(&request)).await;

                return;
            }

            Ok(copy_file_range_in) => copy_file_range_in,
        };

        let fs = fs.clone();

        spawn(
            debug_span!("fuse_copy_file_range"),
            request.unique,
            async move {
                debug!(
                    "reply_copy_file_range unique {} inode {} {:?}",
                    request.unique, in_header.nodeid, copy_file_range_in
                );

                let reply_copy_file_range = match fs
                    .copy_file_range(
                        request,
                        in_header.nodeid,
                        copy_file_range_in.fh_in,
                        copy_file_range_in.off_in,
                        copy_file_range_in.nodeid_out,
                        copy_file_range_in.fh_out,
                        copy_file_range_in.off_out,
                        copy_file_range_in.len,
                        copy_file_range_in.flags,
                    )
                    .await
                {
                    Err(err) => {
                        reply_error_in_place(err, request, resp_sender).await;

                        return;
                    }

                    Ok(reply_copy_file_range) => reply_copy_file_range,
                };

                let write_out: fuse_write_out = reply_copy_file_range.into();

                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE) as u32,
                    error: 0,
                    unique: request.unique,
                };

                let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("won't happened");
                get_bincode_config()
                    .serialize_into(&mut data, &write_out)
                    .expect("won't happened");

                let _ = resp_sender.send(Either::Left(data)).await;
            },
        );
    }
}

/// The FUSE_WRITE handler body — everything after `handle_write`'s
/// parse/validation: run the filesystem write, then commit the reply
/// in place (armed sessions) or through the reply channel (INIT-phase/
/// classical). Extracted (zc-write-fusion campaign, 2026-08-07) so the
/// SAME body runs from both venues: the classic handler-lane spawn and
/// the queue worker's fused lane — fusion changes WHERE the future is
/// polled, never what it does.
#[allow(clippy::too_many_arguments)] // the parse results + the reply plumbing, verbatim
async fn write_handler_body<FS: Filesystem + Send + Sync + 'static>(
    fs: Arc<FS>,
    reply_conn: Option<Arc<FuseConnection>>,
    mut resp_sender: ReplyTx,
    request: Request,
    nodeid: u64,
    write_in: fuse_write_in,
    payload: Bytes,
    dispatch_t0: std::time::Instant,
    arrival_ns: u64,
) {
    crate::raw::read_phase::write_transport_phase_record(
        crate::raw::read_phase::TransportPhase::DispatchLag,
        dispatch_t0.elapsed(),
    );
    debug!(
        "write unique {} inode {} {:?}",
        request.unique, nodeid, write_in
    );

    let reply_write = match fs
        .write(
            request,
            nodeid,
            write_in.fh,
            write_in.offset,
            payload,
            write_in.write_flags,
            write_in.flags,
        )
        .await
    {
        Err(err) => {
            reply_error_in_place(err, request, resp_sender).await;

            return;
        }

        Ok(reply_write) => reply_write,
    };

    let reply_t0 = std::time::Instant::now();
    crate::raw::op_trace::stamp(
        request.unique,
        crate::raw::op_trace::Stage::HandlerReturn,
        reply_t0,
    );
    let write_out: fuse_write_out = reply_write.into();

    let out_header = fuse_out_header {
        len: (FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE) as u32,
        error: 0,
        unique: request.unique,
    };

    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_WRITE_OUT_SIZE);

    get_bincode_config()
        .serialize_into(&mut data, &out_header)
        .expect("won't happened");
    get_bincode_config()
        .serialize_into(&mut data, &write_out)
        .expect("won't happened");

    // In-place reply on an armed session; the channel path stays
    // for INIT-phase/classical sessions (pool not ready) and any
    // clone without a connection handle. Error semantics mirror
    // `reply_fuse`: NotFound = interrupted/double reply (benign);
    // anything else is logged loud — the session's dispatch task
    // observes a dead connection through its own read path.
    match reply_conn.filter(|c| c.over_uring_ready()) {
        Some(conn) => {
            resp_sender.mark_replied();
            if let Err(err) = conn
                .write_vectored(data, None::<Bytes>, request.slot)
                .await
                .1
            {
                if err.kind() == ErrorKind::NotFound {
                    warn!(
                        "may reply interrupted fuse request, ignore this error {}",
                        err
                    );
                } else {
                    error!("in-place write reply failed {}", err);
                }
            }
            crate::raw::read_phase::note_write_inplace_reply();
        }
        None => {
            let _ = resp_sender.send(Either::Left(data)).await;
        }
    }
    // `reply_commit`: handler returned → reply committed to the
    // transport (in-place arm: the synchronous COMMIT enqueue;
    // the channel arm measures the hand-off — INIT-phase only). ONE
    // clock read closes reply_commit, transport_total and the op-trace
    // `reply_commit` stamp (audit A2's one-read law).
    let now = std::time::Instant::now();
    crate::raw::read_phase::write_transport_phase_record(
        crate::raw::read_phase::TransportPhase::ReplyCommit,
        now.saturating_duration_since(reply_t0),
    );
    if arrival_ns > 0 {
        crate::raw::read_phase::write_transport_phase_record(
            crate::raw::read_phase::TransportPhase::TransportTotal,
            now.saturating_duration_since(crate::raw::read_phase::transport_instant(arrival_ns)),
        );
    }
    crate::raw::op_trace::stamp(
        request.unique,
        crate::raw::op_trace::Stage::ReplyCommit,
        now,
    );
}

/// The FUSE_READ handler body — everything after `handle_read`'s parse:
/// run the filesystem read, then commit the reply in place (armed
/// sessions: the zc prefilled arm or the synchronous COMMIT enqueue) or
/// through the reply channel (INIT-phase/classical). Extracted (R-2 READ
/// fast-dispatch) so the SAME body runs from both venues — the dispatch
/// loop's same-lane spawn and the queue worker's direct lane mint for a
/// demoted READ. `dispatch_t0` anchors `dispatch_lag` (the dispatch pop,
/// or the worker's mint instant); `arrival_ns` the `transport_total`
/// anchor (0 = classical delivery — the arrival-anchored phases skip).
#[allow(clippy::too_many_arguments)] // the parse results + the reply plumbing, verbatim
async fn read_handler_body<FS: Filesystem + Send + Sync + 'static>(
    fs: Arc<FS>,
    reply_conn: Option<Arc<FuseConnection>>,
    mut resp_sender: ReplyTx,
    request: Request,
    nodeid: u64,
    read_in: fuse_read_in,
    dispatch_t0: std::time::Instant,
    arrival_ns: u64,
) {
    crate::raw::read_phase::read_transport_phase_record(
        crate::raw::read_phase::TransportPhase::DispatchLag,
        dispatch_t0.elapsed(),
    );
    debug!(
        "read unique {} inode {} {:?}",
        request.unique, nodeid, read_in
    );

    let (mut reply_data, backing, zc_prefilled, zc_fd_body) = match fs
        .read(
            request,
            nodeid,
            read_in.fh,
            read_in.offset,
            read_in.size,
            read_in.flags,
        )
        .await
    {
        Err(err) => {
            reply_error_in_place(err, request, resp_sender).await;

            return;
        }

        Ok(reply_data) => (
            reply_data.data,
            reply_data.backing,
            reply_data.zc_prefilled,
            reply_data.zc_fd_body,
        ),
    };

    // zc direct leg (K1 kill): the payload already sits in the
    // request's pages — commit header + length, no body move.
    // Prefilled replies exist only on zc-armed sessions (the
    // handler mints them against the live connection), so a
    // missing/un-armed connection here is a bug: fail the request
    // loud rather than fabricate a body-less classical reply.
    if let Some(n) = zc_prefilled {
        let n = n.min(read_in.size);
        let out_header = fuse_out_header {
            len: (FUSE_OUT_HEADER_SIZE + n as usize) as u32,
            error: 0,
            unique: request.unique,
        };
        let mut data_buf = Vec::with_capacity(FUSE_OUT_HEADER_SIZE);
        get_bincode_config()
            .serialize_into(&mut data_buf, &out_header)
            .expect("won't happened");
        match reply_conn.filter(|c| c.over_uring_ready()) {
            Some(conn) => {
                resp_sender.mark_replied();
                if let Err(err) = conn.submit_reply_prefilled(request.slot, data_buf, n) {
                    if err.kind() == ErrorKind::NotFound {
                        warn!(
                            "may reply interrupted fuse request, ignore this error {}",
                            err
                        );
                    } else {
                        error!("zc prefilled read reply failed {}", err);
                    }
                }
                crate::raw::read_phase::note_read_inplace_reply();
                drop(backing);
            }
            None => {
                error!(
                    "zc prefilled reply with no armed connection (unique {}) — EIO",
                    request.unique
                );
                reply_error_in_place(libc::EIO.into(), request, resp_sender).await;
            }
        }
        if arrival_ns > 0 {
            let now_ns = crate::raw::read_phase::transport_now_ns();
            crate::raw::read_phase::read_transport_phase_record(
                crate::raw::read_phase::TransportPhase::TransportTotal,
                std::time::Duration::from_nanos(now_ns.saturating_sub(arrival_ns)),
            );
            crate::raw::op_trace::stamp(
                request.unique,
                crate::raw::op_trace::Stage::ReplyCommit,
                crate::raw::read_phase::transport_instant(now_ns),
            );
        }
        return;
    }

    let reply_t0 = std::time::Instant::now();
    crate::raw::op_trace::stamp(
        request.unique,
        crate::raw::op_trace::Stage::HandlerReturn,
        reply_t0,
    );
    if reply_data.len() > read_in.size as _ {
        reply_data.truncate(read_in.size as _);
    }

    let out_header = fuse_out_header {
        len: (FUSE_OUT_HEADER_SIZE + reply_data.len()) as u32,
        error: 0,
        unique: request.unique,
    };

    let mut data_buf = Vec::with_capacity(FUSE_OUT_HEADER_SIZE);

    get_bincode_config()
        .serialize_into(&mut data_buf, &out_header)
        .expect("won't happened");

    // In-place reply on an armed session; the channel path stays
    // for INIT-phase/classical sessions (pool not ready) and any
    // clone without a connection handle. Error semantics mirror
    // `reply_fuse`: NotFound = interrupted/double reply (benign);
    // anything else is logged loud — the session's dispatch task
    // observes a dead connection through its own read path.
    match reply_conn.filter(|c| c.over_uring_ready()) {
        // R-4 fd-source body: the body travels VERBATIM with its fd
        // address (the `write_vectored` path would heap-copy it) — the
        // worker bridges it into the request's pages from the memfd.
        Some(conn) if zc_fd_body.is_some() && !reply_data.is_empty() => {
            let (fd, off) = zc_fd_body.expect("checked above");
            resp_sender.mark_replied();
            if let Err(err) = conn.submit_reply_fd_body(request.slot, data_buf, reply_data, fd, off)
            {
                if err.kind() == ErrorKind::NotFound {
                    warn!(
                        "may reply interrupted fuse request, ignore this error {}",
                        err
                    );
                } else {
                    error!("fd-source read reply failed {}", err);
                }
            }
            crate::raw::read_phase::note_read_inplace_reply();
            drop(backing);
        }
        Some(conn) => {
            resp_sender.mark_replied();
            if let Err(err) = conn
                .write_vectored(data_buf, Some(reply_data), request.slot)
                .await
                .1
            {
                if err.kind() == ErrorKind::NotFound {
                    warn!(
                        "may reply interrupted fuse request, ignore this error {}",
                        err
                    );
                } else {
                    error!("in-place read reply failed {}", err);
                }
            }
            crate::raw::read_phase::note_read_inplace_reply();
            drop(backing);
        }
        None => {
            let _ = resp_sender
                .send(Either::Right((data_buf, reply_data, backing)))
                .await;
        }
    }
    // `reply_commit`: handler returned → reply committed to the
    // transport (in-place arm: the synchronous COMMIT enqueue; the
    // channel arm measures the hand-off — INIT-phase only). ONE
    // clock read closes reply_commit, transport_total and the
    // op-trace `reply_commit` stamp (audit A2's one-read law;
    // `Instant` and the transport epoch are the same
    // CLOCK_MONOTONIC on Linux).
    let now = std::time::Instant::now();
    crate::raw::read_phase::read_transport_phase_record(
        crate::raw::read_phase::TransportPhase::ReplyCommit,
        now.saturating_duration_since(reply_t0),
    );
    if arrival_ns > 0 {
        crate::raw::read_phase::read_transport_phase_record(
            crate::raw::read_phase::TransportPhase::TransportTotal,
            now.saturating_duration_since(crate::raw::read_phase::transport_instant(arrival_ns)),
        );
    }
    crate::raw::op_trace::stamp(
        request.unique,
        crate::raw::op_trace::Stage::ReplyCommit,
        now,
    );
}

/// One DEMOTED READ handler invocation (R-2 fast dispatch): the dispatch
/// loop's prelude — header/`fuse_read_in` parse — followed by
/// [`read_handler_body`], as ONE future the queue worker mints at the
/// delivery CQE and hands straight to a handler lane. The inbound queue
/// and the session dispatch task are not on this path: `queue_wait` is
/// recorded as an exact zero at the first poll (the count keeps closing
/// against the READ population), `dispatch_lag` anchors on the worker's
/// mint instant (= the arrival stamp — one clock read on the worker).
/// Every parse refusal replies EINVAL through the same `ReplyTx`
/// discipline (FUSE-2 holds on this venue by the same machinery as the
/// handler lanes: a dropped/panicked future's drop-guard synthesizes).
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
async fn fast_read_future<FS: Filesystem + Send + Sync + 'static>(
    fs: Arc<FS>,
    conn: std::sync::Weak<FuseConnection>,
    reply_sender: UnboundedSender<FuseReply>,
    req: crate::raw::connection::fuse_over_uring::InboundUringReq,
) {
    let arrived_ns = req.arrived_ns;
    let mint_t0 = crate::raw::read_phase::transport_instant(arrived_ns);
    crate::raw::read_phase::read_transport_phase_record(
        crate::raw::read_phase::TransportPhase::QueueWait,
        std::time::Duration::ZERO,
    );
    let Some(conn) = conn.upgrade() else {
        // Teardown raced the mint: the worker's row-8 drain owns the
        // slot (this future never took a ReplyTx, so nothing double
        // synthesizes).
        return;
    };
    let bare = |unique: u64, slot: crate::raw::ReplySlot| Request {
        unique,
        uid: 0,
        gid: 0,
        pid: 0,
        slot,
    };
    if req.header_and_op.len() < FUSE_IN_HEADER_SIZE {
        error!(
            "fast read: short header frame ({} B) for unique {}",
            req.header_and_op.len(),
            req.unique
        );
        let request = bare(req.unique, req.slot);
        let sender = ReplyTx::owing(reply_sender, &request, req.slot, Some(conn));
        reply_error_in_place(libc::EINVAL.into(), request, sender).await;
        return;
    }
    let in_header = match get_bincode_config()
        .deserialize::<fuse_in_header>(&req.header_and_op[..FUSE_IN_HEADER_SIZE])
    {
        Ok(h) => h,
        Err(err) => {
            error!("fast read: fuse_in_header deserialize failed {err}");
            let request = bare(req.unique, req.slot);
            let sender = ReplyTx::owing(reply_sender, &request, req.slot, Some(conn));
            reply_error_in_place(libc::EINVAL.into(), request, sender).await;
            return;
        }
    };
    let mut request = Request::from(&in_header);
    request.slot = req.slot;
    let request = request;
    let resp_sender = ReplyTx::owing(reply_sender, &request, req.slot, Some(conn.clone()));
    // Belt: the worker mints READ deliveries only.
    if in_header.opcode != fuse_opcode::FUSE_READ as u32 {
        error!(
            "fast read: non-READ opcode {} reached the fast-dispatch mint (unique {})",
            in_header.opcode, request.unique
        );
        reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;
        return;
    }
    let op = &req.header_and_op[FUSE_IN_HEADER_SIZE..];
    let read_in = match get_bincode_config().deserialize::<fuse_read_in>(op) {
        Ok(r) => r,
        Err(err) => {
            error!("fast read: fuse_read_in deserialize failed {err}");
            reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;
            return;
        }
    };
    // The op-trace ingress stamps the dispatch pop would have made:
    // arrival and dispatch at the SAME instant (the worker minted at the
    // reap) — `queue_wait` reads 0 in the stitch too.
    let traced = crate::raw::op_trace::traced(request.unique);
    if traced != 0 {
        crate::raw::op_trace::stamp(traced, crate::raw::op_trace::Stage::TransportRecv, mint_t0);
        crate::raw::op_trace::stamp(traced, crate::raw::op_trace::Stage::Dispatch, mint_t0);
    }
    handler_scope(
        request.unique,
        read_handler_body(
            fs,
            Some(conn),
            resp_sender,
            request,
            in_header.nodeid,
            read_in,
            mint_t0,
            arrived_ns,
        ),
    )
    .await
}

/// One FUSED WRITE handler invocation (zc-write-fusion campaign): the
/// dispatch-loop prelude — header/`fuse_write_in` parse, body-bounds
/// validation, the held-length agreement check — followed by
/// [`write_handler_body`], as ONE future the queue worker polls on its
/// fused lane. Every parse refusal replies EINVAL through the same
/// `ReplyTx` discipline (and a dropped/panicked future's drop-guard
/// synthesizes — FUSE-2 holds on the fused venue by the same machinery
/// as the handler lanes).
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
async fn fused_write_future<FS: Filesystem + Send + Sync + 'static>(
    fs: Arc<FS>,
    conn: std::sync::Weak<FuseConnection>,
    reply_sender: UnboundedSender<FuseReply>,
    req: crate::raw::connection::fuse_over_uring::InboundUringReq,
) {
    // `queue_wait`: CQE reap → first fused poll (the fused twin of the
    // dispatch pop's stamp; the venues stay comparable in the phase
    // tables).
    let mint_t0 = std::time::Instant::now();
    let arrived_ns = req.arrived_ns;
    if arrived_ns > 0 {
        let now_ns = crate::raw::read_phase::transport_now_ns();
        crate::raw::read_phase::write_transport_phase_record(
            crate::raw::read_phase::TransportPhase::QueueWait,
            std::time::Duration::from_nanos(now_ns.saturating_sub(arrived_ns)),
        );
    }
    let Some(conn) = conn.upgrade() else {
        // Teardown raced the mint: the worker's row-8 drain owns the
        // slot (this future never took a ReplyTx, so nothing double
        // synthesizes).
        return;
    };
    if req.header_and_op.len() < FUSE_IN_HEADER_SIZE {
        // The worker delivers ≥ 40 bytes by construction; a short frame
        // has no parseable identity — reply EINVAL against the carried
        // unique so the slot is not stranded.
        error!(
            "fused write: short header frame ({} B) for unique {}",
            req.header_and_op.len(),
            req.unique
        );
        let request = Request {
            unique: req.unique,
            uid: 0,
            gid: 0,
            pid: 0,
            slot: req.slot,
        };
        let sender = ReplyTx::owing(reply_sender, &request, req.slot, Some(conn));
        reply_error_in_place(libc::EINVAL.into(), request, sender).await;
        return;
    }
    let in_header = match get_bincode_config()
        .deserialize::<fuse_in_header>(&req.header_and_op[..FUSE_IN_HEADER_SIZE])
    {
        Ok(h) => h,
        Err(err) => {
            error!("fused write: fuse_in_header deserialize failed {err}");
            let request = Request {
                unique: req.unique,
                uid: 0,
                gid: 0,
                pid: 0,
                slot: req.slot,
            };
            let sender = ReplyTx::owing(reply_sender, &request, req.slot, Some(conn));
            reply_error_in_place(libc::EINVAL.into(), request, sender).await;
            return;
        }
    };
    let mut request = Request::from(&in_header);
    request.slot = req.slot;
    let request = request;
    let resp_sender = ReplyTx::owing(reply_sender, &request, req.slot, Some(conn.clone()));
    // Belt: the worker fuses WRITE deliveries only.
    if in_header.opcode != fuse_opcode::FUSE_WRITE as u32 {
        error!(
            "fused write: non-WRITE opcode {} reached the fused lane (unique {})",
            in_header.opcode, request.unique
        );
        reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;
        return;
    }
    let op = &req.header_and_op[FUSE_IN_HEADER_SIZE..];
    if op.len() < FUSE_WRITE_IN_SIZE {
        error!(
            "fused write: op frame too short for fuse_write_in ({} B, unique {})",
            op.len(),
            request.unique
        );
        reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;
        return;
    }
    let write_in =
        match get_bincode_config().deserialize::<fuse_write_in>(&op[..FUSE_WRITE_IN_SIZE]) {
            Ok(w) => w,
            Err(err) => {
                error!("fused write: fuse_write_in deserialize failed {err}");
                reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;
                return;
            }
        };
    // The dispatch loop's FUSE-3g/§5.4 payload agreement, fused twin:
    // the body rides `req.payload` (a lease over the bounce for
    // extracted deliveries, the EMPTY placeholder for held ones — the
    // connection's held table then carries the authoritative length,
    // which must agree with the header). Any other mismatch is EINVAL.
    let payload = req.payload;
    if write_in.size as usize != payload.len() {
        let held = payload.is_empty() && conn.zc_write_held_len(req.slot) == Some(write_in.size);
        if !held {
            error!(
                "fused write: fuse_write_in size {} != payload len {} (unique {})",
                write_in.size,
                payload.len(),
                request.unique
            );
            reply_error_in_place(libc::EINVAL.into(), request, resp_sender).await;
            return;
        }
    }
    // The fused venue has no `spawn`: the op-trace scope wraps the body
    // here (the dispatch-pop stamps ride the same `traced` decision).
    let traced = crate::raw::op_trace::traced(request.unique);
    if traced != 0 {
        if arrived_ns > 0 {
            crate::raw::op_trace::stamp(
                traced,
                crate::raw::op_trace::Stage::TransportRecv,
                crate::raw::read_phase::transport_instant(arrived_ns),
            );
        }
        crate::raw::op_trace::stamp(traced, crate::raw::op_trace::Stage::Dispatch, mint_t0);
    }
    handler_scope(
        request.unique,
        write_handler_body(
            fs,
            Some(conn),
            resp_sender,
            request,
            in_header.nodeid,
            write_in,
            payload,
            mint_t0,
            arrived_ns,
        ),
    )
    .await
}

/// Reply `err` for `request`. Consumes the request's one [`ReplyTx`], so
/// the error IS the reply (FUSE-2): the handle can no longer synthesize
/// a second one, and a dead reply task falls back to a direct slot
/// commit instead of the historical `let _ = …send()` (row 2).
async fn reply_error_in_place(err: Errno, request: Request, mut sender: ReplyTx) {
    let out_header = fuse_out_header {
        len: FUSE_OUT_HEADER_SIZE as u32,
        error: err.into(),
        unique: request.unique,
    };

    let data = get_bincode_config()
        .serialize(&out_header)
        .expect("won't happened");

    let _ = sender.send(Either::Left(data)).await;
}

/// Classical-only helper for no-reply opcodes (non-uring builds).
/// Over-uring COMMITs FORGET/BATCH_FORGET in the queue worker and DESTROY inline.
#[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
async fn reply_none_in_place(request: Request, mut sender: ReplyTx) {
    let out_header = fuse_out_header {
        len: FUSE_OUT_HEADER_SIZE as u32,
        error: 0,
        unique: request.unique,
    };

    let data = get_bincode_config()
        .serialize(&out_header)
        .expect("won't happened");

    let _ = sender.send(Either::Left(data)).await;
}

/// FUSE-3l: the `FUSE_NOTIFY_REPLY` body after its `fuse_notify_retrieve_in`
/// header, or `None` when the request is shorter than it claims.
///
/// This split runs on the **dispatch task**, not a handler task: an
/// unchecked `&data[FUSE_NOTIFY_RETRIEVE_IN_SIZE..]` panics the whole
/// session, so every in-flight request on that session loses its reply
/// at once (the FUSE-2 failure mode, triggered by one short message).
fn notify_retrieve_body(data: &[u8], size: usize) -> Option<&[u8]> {
    data.get(FUSE_NOTIFY_RETRIEVE_IN_SIZE..)
        .filter(|rest| rest.len() >= size)
        .map(|rest| &rest[..size])
}

/// FUSE-3l: the `BATCH_FORGET` body after its `fuse_batch_forget_in`
/// header, or `None` when the request is shorter than its own header.
/// Same dispatch-task blast radius as [`notify_retrieve_body`].
fn batch_forget_body(data: &[u8]) -> Option<&[u8]> {
    data.get(FUSE_BATCH_FORGET_IN_SIZE..)
}

/// FUSE-3g: the verdict on one delivery's body bounds — what the request
/// header CLAIMS versus what the transport actually FILLED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyBounds {
    /// The body is `.0` bytes of the session data buffer, and every one of
    /// them belongs to THIS request.
    Valid(usize),
    /// `in_header.len` is below the 40-byte `fuse_in_header` itself. The
    /// historical `in_header.len as usize - FUSE_IN_HEADER_SIZE` UNDERFLOWS
    /// here (release build: a ~2^64 `data_size` and an instant slice panic
    /// on the dispatch task, i.e. the whole session).
    ShortHeader,
    /// The header declares more body than the transport delivered. The
    /// historical code sized `data_ref` from the header anyway, so the
    /// tail of the slice was whatever the PREVIOUS request on this
    /// dispatch loop left in the reused buffer — stale bytes presented to
    /// a handler as this request's own.
    Overdeclared { declared: usize, available: usize },
}

/// FUSE-3g: bound one delivery's body by what was actually filled.
///
/// * `header_len` — `in_header.len` (whole request, header included).
/// * `filled` — body bytes the transport wrote into the session data
///   buffer (`read_vectored`'s `n` minus the header).
/// * `payload_len` — body bytes delivered OUT OF BAND instead: the §5.4
///   FUSE_WRITE zero-copy payload lease (`uring_payload`). The kernel
///   counts those in `in_header.len`, so they are legitimately "available"
///   even though they were never copied into the data buffer — but they
///   are NOT part of the returned slice, which is why the WRITE handler
///   reads its body from the payload `Bytes` and not from `data_ref`.
/// * `buf_len` — the data buffer's own capacity (the last bound).
/// Bench seam (microbench program 2026-08-04 — the `get_bincode_config`
/// precedent): the FUSE-3g per-request delivery-bounds decision, so
/// `benches/fuse3_hot_bench.rs` measures the SHIPPING function rather than
/// a lookalike. `Some(body_len)` = admitted, `None` = refused (EINVAL).
#[doc(hidden)]
pub fn delivery_body_bounds(
    header_len: u32,
    filled: usize,
    payload_len: usize,
    buf_len: usize,
) -> Option<usize> {
    match validated_body(header_len, filled, payload_len, buf_len) {
        BodyBounds::Valid(n) => Some(n),
        _ => None,
    }
}

fn validated_body(
    header_len: u32,
    filled: usize,
    payload_len: usize,
    buf_len: usize,
) -> BodyBounds {
    let Some(declared) = (header_len as usize).checked_sub(FUSE_IN_HEADER_SIZE) else {
        return BodyBounds::ShortHeader;
    };
    let available = filled.saturating_add(payload_len);
    if declared > available {
        return BodyBounds::Overdeclared {
            declared,
            available,
        };
    }
    BodyBounds::Valid(declared.min(filled).min(buf_len))
}

/// One handler-lane future (boxed for the lane channels).
type LaneFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// The per-core handler-lane venue: one OS thread per usable CPU, each
/// running a first-party [`crate::sqz_exec::LaneExec`] executor (design-
/// sqz-sync Stage 1b — the former current-thread-runtime + `LocalSet`
/// lanes rode tokio task delivery, the OQ-5 lost-task wedge class the
/// Stage-1 field attribution convicted; wake→queue→poll is now our code
/// plus the loom-verified `exec_core` state word).
///
/// **RES-18 (pre-RC engineering spec §7) — why the lane channels stay
/// unbounded** (recorded per the item's "a geometry-derived bound, or one
/// sentence of recorded reasoning" disposition):
///
/// The observation is correct — each lane's sqz-exec ready queue is
/// unbounded (as the dispatch channel it replaced was), so resident task
/// population is bounded only incidentally. But for the hot path that incidental bound IS a
/// transport-geometry bound: a request delivered over the ring holds its
/// ent slot from delivery until its reply commits, and the handler future's
/// lifetime is contained in that window, so concurrently resident
/// over-uring handler futures cannot exceed `queues × q_depth` — the
/// transport's own registered slot count (the same number the INIT reply's
/// `max_background` derives from). That is not a foreign subsystem's
/// number; it is the delivery capacity that produced the work, and
/// backpressure is applied where it belongs: the kernel stops delivering
/// when every slot is out, so nothing queues here.
///
/// The classical sideband is genuinely not slot-bounded, and bounding it
/// would be actively harmful: it carries FORGET/BATCH_FORGET **and
/// INTERRUPT** on ONE serialized reader, so a full bounded channel would
/// block the dispatch loop, stall the reader, and delay exactly the
/// INTERRUPT deliveries that exist to unstick requests — turning a
/// memory-pressure event into a liveness failure. Its real bound is the
/// reader: one request in flight per `Readv`, the kernel coalescing forgets
/// into BATCH_FORGET (one future for many inos), and — since RES-20 — a
/// daemon-side reclaim enqueue that spawns nothing per FORGET.
///
/// What must not regress: [`TpcScheduler::dispatch`]'s dead-lane
/// re-dispatch, and the `transport_requests_abandoned` must-stay-0
/// tripwire — the instrument that would actually observe a lane backlog
/// becoming stranded requests. If a measurement ever shows lane residency
/// mattering, the bound to add is `queues × q_depth` on the SIDEBAND
/// dispatch alone, never on the ring lanes.
/// One handler lane: the sqz-exec executor + its thread's liveness word
/// (design-sqz-sync Stage 1b — the lane venue runs FIRST-PARTY poll
/// delivery; the Stage-1 field attribution proved tokio task delivery is
/// the OQ-5 wedge class and no future-layer backstop can heal a task the
/// scheduler dropped).
struct Lane {
    exec: crate::sqz_exec::LaneExec,
    /// Flipped false when the lane THREAD exits (normal or panic) — the
    /// dead-lane re-dispatch gate. With sqz-exec a queue push cannot
    /// fail, so thread liveness is the honest signal the closed-channel
    /// error used to be.
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct TpcScheduler {
    lanes: Vec<Lane>,
    next_idx: std::sync::atomic::AtomicUsize,
    /// NUMA-affinity campaign (2026-07-31): lane indices grouped by the
    /// dense node of each lane's pinned core (`node_lanes[node]` — empty
    /// for nodes without lanes: CPU-less nodes, taskset-excluded
    /// sockets). Node-targeted dispatch ([`tpc_spawn_on_node`]) picks
    /// round-robin WITHIN a node's lanes and falls back to the global
    /// rotation whenever the node has none — locality is a preference,
    /// never a availability constraint. Single-node maps produce one
    /// group == the global set, so the whole mechanism degenerates to
    /// today's rotation (the structural no-op law).
    node_lanes: Vec<Vec<usize>>,
    /// Per-node rotation cursors (same cache-pressure shape as
    /// `next_idx`).
    node_next: Vec<std::sync::atomic::AtomicUsize>,
    /// cpu id → the lane HOMED on it (R-2 READ fast dispatch): the kernel
    /// routes a request to the queue of the requester's CPU, so on
    /// queue-per-CPU sessions a demoted READ minted by queue worker `qid`
    /// goes to the lane whose home core is that CPU — ONE lane per queue
    /// (the same-lane posture the retired dispatch task had), so a
    /// burst's handlers queue on one already-running lane instead of
    /// waking a different parked lane per op. `None` = no lane homes
    /// there (core 0, taskset holes, out-of-range).
    lane_of_cpu: Vec<Option<usize>>,
}

impl TpcScheduler {
    fn new() -> Self {
        // CPU ids come from the PROCESS affinity mask, never the calling
        // thread's (`affinity::process_cpus` — the Hang-1 pinned-first-
        // toucher lesson lives on its doc).
        let mut core_ids = crate::raw::affinity::process_cpus();
        if core_ids.len() > 1 {
            core_ids.remove(0); // Reserve Core 0 for OS kernel tasks
        }

        let mut lanes: Vec<Lane> = Vec::new();
        let core_count = if core_ids.is_empty() {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(16)
        } else {
            core_ids.len()
        };

        // Lane → node grouping for node-targeted dispatch: the node of
        // each lane's HOME core (homeless lanes — empty core_ids — join
        // no group and ride the global rotation only).
        let topo = crate::numa_core::topology();
        let mut node_lanes: Vec<Vec<usize>> = vec![Vec::new(); topo.len()];
        let mut node_next = Vec::with_capacity(topo.len());
        for _ in 0..topo.len() {
            node_next.push(std::sync::atomic::AtomicUsize::new(0));
        }
        let mut lane_of_cpu: Vec<Option<usize>> =
            vec![None; core_ids.iter().copied().max().map_or(0, |m| m + 1)];

        let scope = crate::raw::affinity::pin_scope();
        for i in 0..core_count {
            let exec = crate::sqz_exec::LaneExec::new();
            let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            lanes.push(Lane {
                exec: exec.clone(),
                alive: alive.clone(),
            });

            let home_cpu = if !core_ids.is_empty() {
                Some(core_ids[i % core_ids.len()])
            } else {
                None
            };
            if let Some(home) = home_cpu {
                if let Some(node) = topo.node_of_cpu(home) {
                    node_lanes[node].push(i);
                }
                // First lane homed on a core wins (lanes wrap when
                // core_count > cores — never in production).
                if lane_of_cpu[home].is_none() {
                    lane_of_cpu[home] = Some(i);
                }
            }

            // Affinity posture (transport-ingress campaign 2026-08-01):
            // node scope by DEFAULT — the lane keeps its home node (the
            // grouping above and `tpc_spawn_on_node` locality are exactly
            // as before) but may run on any available CPU of it, so a
            // cross-thread wake lands on the first idle core instead of
            // waiting ms-class for one specific busy core's runqueue (the
            // measured queue_wait/dispatch_lag mechanism). The
            // `SQUEEZEFS_FUSE_PIN_SCOPE=core` lever restores the
            // pre-campaign 1-CPU pin (A0 control + operational escape).
            let lane_cpus: Option<Vec<usize>> = home_cpu.map(|home| {
                crate::raw::affinity::scoped_affinity_cpus(
                    scope,
                    home,
                    &core_ids,
                    |c| topo.node_of_cpu(c),
                    |n| {
                        topo.nodes()
                            .get(n)
                            .map(|d| d.cpus.clone())
                            .unwrap_or_default()
                    },
                )
            });

            // Named explicitly (ingest-economy 2026-07-28): an unnamed
            // thread inherits the comm of whichever thread first touched
            // the lazy scheduler — on interception mounts that is an ipc
            // service thread, so every lane showed up in pidstat/perf as
            // "sqz-ipc-svcN" (the field capture mis-attributed lane CPU
            // to the service threads exactly this way).
            std::thread::Builder::new()
                .name(crate::comm_core::comm_name(&format!("fuse3-tpc{i}")))
                .spawn(move || {
                    // Liveness word: flipped on ANY exit path (normal or
                    // panic) so the dead-lane re-dispatch gate is honest.
                    struct DeadMark(std::sync::Arc<std::sync::atomic::AtomicBool>);
                    impl Drop for DeadMark {
                        fn drop(&mut self) {
                            self.0.store(false, std::sync::atomic::Ordering::Release);
                        }
                    }
                    let _dead_on_exit = DeadMark(alive);

                    if let Some(cpus) = lane_cpus {
                        crate::raw::affinity::set_current_affinity(&cpus);
                    }
                    // Same-lane dispatch context marks (lever 1): READ
                    // dispatchers probe these to push onto the CURRENT
                    // lane instead of paying the cross-lane hop.
                    IS_TPC_LANE.with(|c| c.set(true));
                    CURRENT_LANE.with(|c| *c.borrow_mut() = Some(exec.clone()));

                    // No ambient runtime: handler futures' timers ride
                    // the first-party `sqz_time` service and their aux
                    // spawns the sqz venues, so the lane needs no
                    // entered handle (the retired `fuse3-timerdrv`
                    // parked-runtime donation — rip-tokio-total). Task
                    // delivery (queue -> poll) is sqz-exec, first-party
                    // by construction.
                    exec.run();
                })
                .expect("fuse3 tpc lane thread spawns");
        }

        Self {
            lanes,
            next_idx: std::sync::atomic::AtomicUsize::new(0),
            node_lanes,
            node_next,
            lane_of_cpu,
        }
    }

    fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if self.lanes.is_empty() {
            // Degenerate ZERO-LANE config only (never a production
            // shape): a transient named OS thread drives the future to
            // completion — loud on spawn failure, never a silent drop,
            // and no ambient runtime requirement.
            std::thread::Builder::new()
                .name(crate::comm_core::comm_name("fuse3-tpc-fallback"))
                .spawn(move || crate::sqz_blocking::block_on(fut))
                .expect("fuse3-tpc-fallback thread spawns");
            return;
        }
        let idx = self
            .next_idx
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.lanes.len();
        Self::dispatch(&self.lanes, idx, Box::pin(fut));
    }

    /// Node-targeted spawn: round-robin WITHIN `node`'s lane group when
    /// it has lanes, the global rotation otherwise (locality is a
    /// preference — a node without lanes, an out-of-range index, or a
    /// single-node map all take exactly the [`Self::spawn`] path). The
    /// dead-lane re-dispatch walk still covers EVERY lane, so a dead
    /// node-local lane degrades cross-node before it ever blackholes.
    fn spawn_on_node<F>(&self, node: usize, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Some(group) = self.node_lanes.get(node) {
            if !group.is_empty() && !self.lanes.is_empty() {
                let k = self.node_next[node].fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % group.len();
                Self::dispatch(&self.lanes, group[k], Box::pin(fut));
                return;
            }
        }
        self.spawn(fut);
    }

    /// The R-2 fast dispatch's demote venue for an ALREADY-boxed READ
    /// handler: the lane HOMED on `cpu` (the requester's CPU = the queue
    /// worker's qid — one lane per queue, see `lane_of_cpu`), else the
    /// node-local round-robin, else the global rotation; no second
    /// `Box::pin` around the box. Zero-lane configs take the
    /// [`Self::spawn`] fallback thread.
    fn spawn_boxed_home(&self, cpu: Option<usize>, node: Option<usize>, fut: LaneFuture) {
        if self.lanes.is_empty() {
            self.spawn(fut);
            return;
        }
        if let Some(idx) = cpu.and_then(|c| self.lane_of_cpu.get(c).copied().flatten()) {
            Self::dispatch(&self.lanes, idx, fut);
            return;
        }
        if let Some(group) = node.and_then(|n| self.node_lanes.get(n)) {
            if !group.is_empty() {
                let n = node.expect("group came from node");
                let k = self.node_next[n].fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    % group.len();
                Self::dispatch(&self.lanes, group[k], fut);
                return;
            }
        }
        let idx = self
            .next_idx
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.lanes.len();
        Self::dispatch(&self.lanes, idx, fut);
    }

    /// Lane dispatch with **loud dead-lane re-dispatch** (shim-parity
    /// campaign 2026-07-28, ingest-economy board item 2): a lane whose
    /// receiver is gone (its OS thread died — an escaped panic outside a
    /// polled task) must never silently blackhole
    /// 1/N of all handler dispatches (each swallowed future is a FUSE
    /// request whose reply never happens: the kernel waiter parks in
    /// D-state forever and umount joins the wedge — the exact shape the
    /// transport-lease watchdog hang exhibited). A closed channel returns
    /// the future; we re-dispatch to the next lane, count it
    /// ([`tpc_lane_redispatches`] — a NONZERO value means a lane thread
    /// is dead and the daemon deserves investigation), and log loudly.
    /// ALL lanes dead = the entire handler venue is gone: abort rather
    /// than blackhole (a daemon that can never again answer a FUSE
    /// request must fail loud, not wedge every mount user).
    fn dispatch(lanes: &[Lane], first_idx: usize, fut: LaneFuture) {
        for attempt in 0..lanes.len() {
            let idx = (first_idx + attempt) % lanes.len();
            let lane = &lanes[idx];
            if !lane.alive.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            if attempt > 0 {
                TPC_LANE_REDISPATCHES
                    .fetch_add(attempt as u64, std::sync::atomic::Ordering::Relaxed);
                error!(
                    "fuse3: TPC lane {} is DEAD (thread exited) — dispatch \
                     re-routed to lane {idx}; a dead handler lane means a lane \
                     thread was lost (escaped panic / spawn failure) and deserves \
                     investigation",
                    (first_idx + attempt - 1) % lanes.len(),
                );
            }
            lane.exec.spawn_boxed(fut);
            return;
        }
        // Every lane thread is dead: no handler can ever run again on
        // this process — every future dispatch would strand a FUSE waiter
        // in D-state. Fail loud (the supervise/abort machinery restarts a
        // dead daemon; a silently wedged one strands the mount forever).
        error!(
            "fuse3: ALL {} TPC handler lanes are dead — aborting rather than \
             blackholing FUSE dispatch",
            lanes.len()
        );
        std::process::abort();
    }
}

/// Dead-lane re-dispatches (see [`TpcScheduler::dispatch`]): 0 on a
/// healthy daemon; any growth = a lane thread died and its traffic is
/// riding the survivors.
static TPC_LANE_REDISPATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The dead-lane re-dispatch counter (stats-inode surface).
pub fn tpc_lane_redispatches() -> u64 {
    TPC_LANE_REDISPATCHES.load(std::sync::atomic::Ordering::Relaxed)
}

static TPC_SCHEDULER: once_cell::sync::Lazy<TpcScheduler> =
    once_cell::sync::Lazy::new(TpcScheduler::new);

std::thread_local! {
    /// `true` exactly on `fuse3-tpcN` lane threads (set once in the lane
    /// body before its LocalSet runs) — the same-lane dispatch gate's
    /// context probe. A thread-local, not a runtime probe: the lane
    /// runtimes are `current_thread` and the probe must cost nothing on
    /// the per-op path.
    static IS_TPC_LANE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// The CURRENT lane's executor handle (set once in the lane thread
    /// body) — the same-lane dispatch venue: a push here is a local
    /// queue append, no cross-lane hop, and the task's delivery stays
    /// sqz-exec first-party.
    static CURRENT_LANE: std::cell::RefCell<Option<crate::sqz_exec::LaneExec>> =
        const { std::cell::RefCell::new(None) };
}

/// Transport-ingress lever 1 (2026-08-04 campaign; the §9.2 deferred
/// item, venue named by the cluster randread-kernel row — 412 µs
/// dispatch_lag at 256 in-flight): when the dispatcher already runs ON a
/// TPC lane thread, spawn the handler future onto the CURRENT lane's
/// LocalSet instead of round-robining it through another lane's channel
/// — deleting one unbounded-channel hop and one cross-thread wake per
/// op. The kernel already spreads load (qid ≈ submitting CPU ≈ lane), so
/// same-lane keeps the spread while making the hand-off a local queue
/// push. `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH=0` restores the rotation
/// (the A0 control + operational escape).
fn same_lane_dispatch_enabled() -> bool {
    static CELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        // The ONE boolean convention (env_knob_core; the daemon-side
        // registry gate in `squeezefs::env_knobs` refuses malformed
        // values at startup — this read only ever sees vetted input,
        // and defaults ON when absent).
        let raw = std::env::var("SQUEEZEFS_FUSE_SAME_LANE_DISPATCH").ok();
        crate::env_knob_core::parse_bool("SQUEEZEFS_FUSE_SAME_LANE_DISPATCH", raw.as_deref())
            .ok()
            .flatten()
            .unwrap_or(true)
    })
}

/// Spawn a handler future same-lane when legal (on a lane thread with
/// the lever on), else through the global rotation. READ-path dispatch
/// uses this; other opcodes keep the rotation until their venues are
/// measured.
///
/// `unique` is the request's op id (audit A2): the handler future runs
/// under an [`op_trace::scope`](crate::raw::op_trace::scope) bound to it, so
/// every hook below the handler — router, device funnel, conveyor — reads
/// the op it serves from the task-scoped current op; the scope's first
/// poll stamps `handler_entry`. An unsampled (or disarmed) op binds 0 and
/// the scope is a plain field compare per poll.
#[inline]
fn spawn_read<F>(span: Span, unique: u64, fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "tokio-runtime")]
    {
        let fut = handler_scope(unique, fut.instrument(span));
        if same_lane_dispatch_enabled() && IS_TPC_LANE.with(|c| c.get()) {
            let lane = CURRENT_LANE.with(|c| c.borrow().clone());
            if let Some(lane) = lane {
                lane.spawn(async move {
                    let _ = fut.await;
                });
                return;
            }
        }
        TPC_SCHEDULER.spawn(async move {
            let _ = fut.await;
        });
    }
}

/// See [`spawn_read`] for `unique`.
#[inline]
fn spawn<F>(span: Span, unique: u64, fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(feature = "tokio-runtime")]
    {
        let fut = handler_scope(unique, fut.instrument(span));
        TPC_SCHEDULER.spawn(async move {
            let _ = fut.await;
        });
    }
}

/// The handler future's op-trace scope (audit A2): bound to `unique`
/// iff the op is in the sample, stamping `handler_entry` on its first
/// poll.
#[inline]
fn handler_scope<F: Future>(unique: u64, fut: F) -> crate::raw::op_trace::OpScope<F> {
    crate::raw::op_trace::scope_with_entry(
        unique,
        Some(crate::raw::op_trace::Stage::HandlerEntry),
        fut,
    )
}

pub fn tpc_spawn<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    TPC_SCHEDULER.spawn(fut);
}

/// Node-targeted handler-lane spawn (NUMA-affinity campaign 2026-07-31):
/// prefer a lane pinned on dense node `node` (the caller's payload/arena
/// node — a `numa_core` dense index; both crates build the map from the
/// same sysfs, so indices agree by construction). Falls back to the
/// global rotation when the node has no lanes — locality is a
/// preference, never an availability constraint, and single-node maps
/// take exactly the [`tpc_spawn`] path (structural no-op).
pub fn tpc_spawn_on_node<F>(node: usize, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    TPC_SCHEDULER.spawn_on_node(node, fut);
}

/// Hand an already-boxed handler future to a lane (R-2 READ fast
/// dispatch's demote arm): the lane homed on `cpu` when known (one lane
/// per queue), else node-local round-robin, else the global rotation —
/// [`tpc_spawn_on_node`] without the second box.
pub(crate) fn tpc_dispatch_boxed(cpu: Option<usize>, node: Option<usize>, fut: LaneFuture) {
    TPC_SCHEDULER.spawn_boxed_home(cpu, node, fut);
}

pub fn tpc_thread_count() -> usize {
    TPC_SCHEDULER.lanes.len()
}

/// `true` exactly on `fuse3-tpcN` lane threads — the sqz-exec handler
/// venue witness (the ipc handoff venue contract pins on this instead of
/// the retired runtime-flavor proxy: lanes no longer run per-lane tokio
/// runtimes, so the flavor of the ENTERED handle says nothing about the
/// executing venue).
pub fn on_tpc_lane_thread() -> bool {
    IS_TPC_LANE.with(|c| c.get())
}

/// INIT reply-flags negotiation: the subset of the kernel's offered
/// `init_in.flags` capabilities this daemon actually implements (mount
/// options gate the optional ones). Pure — pinned by
/// `init_negotiation_tests`: a capability the daemon does not implement
/// must never be advertised back to the kernel.
/// The daemon's advertised inode-timestamp range for the sqz
/// `FUSE_TIME_LIMITS` INIT capability: ±9,223,372,036 s — the whole-second
/// interior of the i64-nanosecond storage word (the saturation law pinned
/// in `tests/attr_refresh_tests.rs::out_of_range_timestamps_saturate_*`).
/// Deliberately conservative by one second on the floor: the exact ns
/// floor is −9,223,372,037 s + 145,224,192 ns, so advertising
/// −9,223,372,036 keeps every kernel-clamped `(sec, nsec=0)` exactly
/// representable — incore and durable state can never disagree.
pub(crate) const TIME_LIMITS_MIN_SEC: i64 = -9_223_372_036;
/// See [`TIME_LIMITS_MIN_SEC`]; the i64-ns ceiling's whole-second interior.
pub(crate) const TIME_LIMITS_MAX_SEC: i64 = 9_223_372_036;

/// sqz `FUSE_TIME_LIMITS` negotiation (kernel-sqz patch 0027):
/// `Some((time_min, time_max))` iff the kernel offered folded capability
/// bit 62; `None` on stock kernels, keeping the INIT reply bit-identical
/// (fields zero, flag unechoed) — feature-absent must be unobservable.
fn negotiate_time_limits(kernel_capabilities: u64) -> Option<(i64, i64)> {
    if kernel_capabilities & FUSE_TIME_LIMITS != 0 {
        Some((TIME_LIMITS_MIN_SEC, TIME_LIMITS_MAX_SEC))
    } else {
        None
    }
}

/// FUSE-1 (docs/pre-rc-engineering-spec.md §4, pulled forward by
/// execution-plan ruling D6): the INIT reply's protocol minor and
/// `FUSE_INIT_EXT` header bit. Mainline `process_init_reply()` folds the
/// reply's `flags2` into the 64-bit capability word ONLY when the reply
/// sets `FUSE_INIT_EXT` in `flags` (fs/fuse/inode.c — and the fold's
/// semantics are the 7.36 extended-init contract), so a minor-31 /
/// no-`FUSE_INIT_EXT` reply makes the kernel DISCARD the daemon's entire
/// `flags2`: `FUSE_OVER_IO_URING` (bit 41) then engages only through the
/// kernel's `enable_uring` module-param side door with `fc->io_uring`
/// stuck at 0 (no `fuse_block_alloc` arm-window gating), and sqz
/// `FUSE_TIME_LIMITS` (bit 62, kernel-sqz patch 0027) has no side door at
/// all — the reset-v5 window's generic/634 row tests a structurally
/// disengaged arm.
///
/// Contract (pinned by `init_negotiation_tests`), returns
/// `(reply_minor, init_ext_bit)`:
/// - reply minor = `min(kernel_minor, FUSE_KERNEL_MINOR_VERSION)` — the
///   daemon never claims a protocol minor the kernel did not offer (the
///   kernel stores the reply verbatim as `fc->minor` and gates compat
///   behavior on it);
/// - `FUSE_INIT_EXT` rides the reply iff the kernel OFFERED it AND the
///   reply carries a nonzero `flags2` — never invented toward pre-7.36
///   kernels (they treat bit 30 as garbage), and a zero `flags2` has
///   nothing to fold.
fn negotiate_init_ext(kernel_minor: u32, kernel_flags: u32, reply_flags2: u32) -> (u32, u32) {
    let reply_minor = kernel_minor.min(FUSE_KERNEL_MINOR_VERSION);
    // The reply_minor >= 36 conjunct is the coherence guard: every real
    // kernel that offers FUSE_INIT_EXT is >= 7.36 (the bit did not exist
    // before), so it never fires against genuine offers — it only refuses
    // the degenerate shape of a kernel claiming minor < 36 while waving
    // bit 30 (a foreign-fork or garbage flags word extended-init cannot
    // stand on).
    let init_ext = if kernel_flags & FUSE_INIT_EXT != 0 && reply_flags2 != 0 && reply_minor >= 36 {
        FUSE_INIT_EXT
    } else {
        0
    };
    (reply_minor, init_ext)
}

/// FUSE-4d: the kernel's readahead limit, echoed VERBATIM into the INIT
/// reply and published for the stats inode.
///
/// `max_readahead` bounds the KERNEL's per-file readahead requests to the
/// daemon. The daemon's own R2 prefetch window is a DEVICE-side pipeline
/// depth (measured bandwidth × latency, clamped by the R5 memory budget),
/// so the two are different resources in different units and are
/// deliberately NOT coupled: clamping the pipeline by a page-cache limit
/// (or advertising a smaller limit because the pipeline is shallow) would
/// each throttle one plane by the other's unrelated bound. What the echo
/// must not do is shrink: a clamped echo caps kernel readahead for the
/// mount's life, on every file, invisibly.
#[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
fn negotiate_max_readahead(kernel_limit: u32) -> u32 {
    crate::raw::connection::fuse_over_uring::note_negotiated_max_readahead(kernel_limit);
    kernel_limit
}

#[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
fn negotiate_max_readahead(kernel_limit: u32) -> u32 {
    kernel_limit
}

fn negotiate_reply_flags(init_in_flags: u32, mount_options: &MountOptions) -> u32 {
    let mut reply_flags = 0;

    // TODO: most of these FUSE_* flags should be controllable by the consuming crate.
    if init_in_flags & FUSE_ASYNC_READ > 0 {
        debug!("enable FUSE_ASYNC_READ");

        reply_flags |= FUSE_ASYNC_READ;
    }

    // FUSE_POSIX_LOCKS is deliberately NOT echoed: POSIX fcntl locks stay
    // KERNEL-LOCAL (`posix_lock_file` — the canonical implementation).
    // A daemon-arbitrated table cannot satisfy the full surface:
    // unlock-on-close rides FLUSH's lock_owner, which the clean-handle
    // ENOSYS latch elides connection-wide (fstests generic/131 subtest
    // 27); OFD locks carry file-description owners the wire does not
    // represent (generic/478); /proc/locks shows only kernel-tracked
    // locks (generic/504's flock twin). Intra-mount arbitration is the
    // whole requirement under the D0 single-writer mount guard.

    if init_in_flags & FUSE_FILE_OPS > 0 {
        debug!("enable FUSE_FILE_OPS");

        reply_flags |= FUSE_FILE_OPS;
    }

    if init_in_flags & FUSE_ATOMIC_O_TRUNC > 0 {
        debug!("enable FUSE_ATOMIC_O_TRUNC");

        reply_flags |= FUSE_ATOMIC_O_TRUNC;
    }

    // FUSE-4b: advertising this makes the kernel encode `(nodeid,
    // generation)` into NFS handles and compare the generation a LOOKUP
    // returns against the handle's (`fuse_get_dentry` → ESTALE on
    // mismatch). It is honest only if the filesystem answers with a REAL
    // generation: the daemon derives it from the volume-set generation
    // identity (superblock uuids) rather than the historical hardcoded `1`,
    // so a handle minted before a `format` now gets ESTALE instead of a
    // different file with the same ino.
    if init_in_flags & FUSE_EXPORT_SUPPORT > 0 {
        debug!("enable FUSE_EXPORT_SUPPORT");

        reply_flags |= FUSE_EXPORT_SUPPORT;
    }

    if init_in_flags & FUSE_BIG_WRITES > 0 {
        debug!("enable FUSE_BIG_WRITES");

        reply_flags |= FUSE_BIG_WRITES;
    }

    if init_in_flags & FUSE_DONT_MASK > 0 && mount_options.dont_mask {
        debug!("enable FUSE_DONT_MASK");

        reply_flags |= FUSE_DONT_MASK;
    }

    // FUSE_SPLICE_{WRITE,MOVE,READ} are deliberately NOT echoed: the daemon
    // has no splice implementation (L3 lever A — reply routing is over-uring
    // COMMIT_AND_FETCH + classical vectored write only), and advertising a
    // capability nothing implements is how the ENOENT-bouncing splice reply
    // path shipped in the first place.

    // FUSE_FLOCK_LOCKS is deliberately NOT echoed: no daemon flock
    // handler exists, and advertising it made the kernel skip its own
    // canonical BSD-flock bookkeeping (`/proc/locks` empty for held
    // flocks — fstests generic/504). flock stays kernel-local; POSIX
    // fcntl locks stay daemon-arbitrated above (the file-lock feature).

    if init_in_flags & FUSE_HAS_IOCTL_DIR > 0 {
        debug!("enable FUSE_HAS_IOCTL_DIR");

        reply_flags |= FUSE_HAS_IOCTL_DIR;
    }

    if init_in_flags & FUSE_AUTO_INVAL_DATA > 0 {
        debug!("enable FUSE_AUTO_INVAL_DATA");

        reply_flags |= FUSE_AUTO_INVAL_DATA;
    }

    if init_in_flags & FUSE_DO_READDIRPLUS > 0 || mount_options.force_readdir_plus {
        debug!("enable FUSE_DO_READDIRPLUS");

        reply_flags |= FUSE_DO_READDIRPLUS;
    }

    if init_in_flags & FUSE_READDIRPLUS_AUTO > 0 && !mount_options.force_readdir_plus {
        debug!("enable FUSE_READDIRPLUS_AUTO");

        reply_flags |= FUSE_READDIRPLUS_AUTO;
    }

    if init_in_flags & FUSE_ASYNC_DIO > 0 {
        debug!("enable FUSE_ASYNC_DIO");

        reply_flags |= FUSE_ASYNC_DIO;
    }

    if init_in_flags & FUSE_WRITEBACK_CACHE > 0 && mount_options.write_back {
        debug!("enable FUSE_WRITEBACK_CACHE");

        reply_flags |= FUSE_WRITEBACK_CACHE;
    }

    if init_in_flags & FUSE_NO_OPEN_SUPPORT > 0 && mount_options.no_open_support {
        debug!("enable FUSE_NO_OPEN_SUPPORT");

        reply_flags |= FUSE_NO_OPEN_SUPPORT;
    }

    if init_in_flags & FUSE_PARALLEL_DIROPS > 0 {
        debug!("enable FUSE_PARALLEL_DIROPS");

        reply_flags |= FUSE_PARALLEL_DIROPS;
    }

    if init_in_flags & FUSE_HANDLE_KILLPRIV > 0 && mount_options.handle_killpriv {
        debug!("enable FUSE_HANDLE_KILLPRIV");

        reply_flags |= FUSE_HANDLE_KILLPRIV;
    }

    // Killpriv v2 (the 2026-07-28 campaign): deletes the kernel's
    // per-write(2) GETXATTR("security.capability") probe — the daemon
    // must implement the clearing law (suid always; sgid only when
    // group-executable; drop security.capability) on flagged
    // WRITE/OPEN/SETATTR, which is what the mount option attests.
    if init_in_flags & FUSE_HANDLE_KILLPRIV_V2 > 0 && mount_options.handle_killpriv_v2 {
        debug!("enable FUSE_HANDLE_KILLPRIV_V2");

        reply_flags |= FUSE_HANDLE_KILLPRIV_V2;
    }

    // FUSE_POSIX_ACL is deliberately NOT echoed: the daemon implements no
    // ACL semantics (fstests generic/099/319 posture — system.posix_acl_*
    // xattrs refuse), and negotiating it makes the kernel's
    // posix_acl_create probe the parent's default ACL on EVERY create —
    // any non-"absent" reply poisons file creation wholesale (the VL10
    // targeted-rerun blanket-EOPNOTSUPP regression). Kernel-side
    // default_permissions mode-bit checking is independent of this flag.

    if init_in_flags & FUSE_MAX_PAGES > 0 {
        debug!("enable FUSE_MAX_PAGES");

        reply_flags |= FUSE_MAX_PAGES;
    }

    // FUSE-4c: the kernel caches a symlink's target page for the inode's
    // lifetime and there is NO invalidation path — and none is needed,
    // which is the whole argument for echoing this. A target is written
    // exactly once, inside the POSIX-3 `symlink()` create transaction, and
    // can never be rewritten afterwards: POSIX has no retarget call, and
    // the daemon's VAL-2 xattr allowlist refuses the `system.symlink`
    // record through setxattr/removexattr (pinned in
    // `xattr_allowlist_tests`; the one-writer law is grep-guarded in
    // `negotiated_caps_tests`). Inode numbers are never reused (v3 allocates
    // monotonically), so a cached page cannot be re-pointed at another
    // file's target either. A NEW writer of that record would need
    // `notify_inval_inode` on the symlink before this flag could stay.
    if init_in_flags & FUSE_CACHE_SYMLINKS > 0 {
        debug!("enable FUSE_CACHE_SYMLINKS");

        reply_flags |= FUSE_CACHE_SYMLINKS;
    }

    if init_in_flags & FUSE_NO_OPENDIR_SUPPORT > 0 && mount_options.no_open_dir_support {
        debug!("enable FUSE_NO_OPENDIR_SUPPORT");

        reply_flags |= FUSE_NO_OPENDIR_SUPPORT;
    }

    reply_flags
}

/// FUSE-2 — the session half of the exactly-one-reply invariant: rows 1
/// (handler task panic), 2 (`reply_error_in_place`'s discarded send) and
/// 3 (reply-task death while handlers keep running).
#[cfg(test)]
mod reply_guard_tests {
    use super::*;
    use futures_util::StreamExt;

    fn request(unique: u64, slot: ReplySlot) -> Request {
        Request {
            unique,
            uid: 0,
            gid: 0,
            pid: 0,
            slot,
        }
    }

    fn ring_slot(ent_idx: u16, commit_id: u64) -> ReplySlot {
        ReplySlot::Ring {
            qid: 0,
            ent_idx,
            commit_id,
        }
    }

    /// `(len, error, unique)` off the wire — `fuse_out_header` is
    /// serialize-only, so the pins read the bytes the kernel would.
    fn out_header(reply: &FuseReply) -> (u32, i32, u64) {
        let bytes = match &reply.data {
            Either::Left(d) => d,
            Either::Right((d, _, _)) => d,
        };
        assert!(
            bytes.len() >= FUSE_OUT_HEADER_SIZE,
            "reply carries a header"
        );
        (
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        )
    }

    /// Row 1: a handler task that panics (or is simply dropped) before
    /// replying must not leave the kernel waiting. `spawn_local`'s
    /// `JoinHandle` is dropped, so nothing else in the process will ever
    /// notice — the obligation dies with the `ReplyTx`, which is exactly
    /// where the synthesized reply has to come from.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_an_owing_handle_synthesizes_the_reply() {
        let (tx, mut rx) = unbounded();
        let req = request(4242, ring_slot(3, 77));
        {
            let _guard = ReplyTx::owing(tx, &req, req.slot, None);
            // handler panics / returns early: no send happens
        }
        let reply = rx.next().await.expect("a reply must be synthesized");
        let (_len, error, unique) = out_header(&reply);
        assert_eq!(unique, 4242, "the synthesized reply addresses the request");
        assert_eq!(error, -libc::EIO, "an unanswered request fails EIO");
        assert_eq!(
            reply.slot, req.slot,
            "the synthesized reply commits against the request's own slot"
        );
    }

    /// The guard fires exactly ONCE: a handle that replied owes nothing,
    /// so its drop can never produce a second reply (a double COMMIT
    /// would be refused by the slot state machine, but the session must
    /// not generate one in the first place).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replied_handle_synthesizes_nothing_on_drop() {
        let (tx, mut rx) = unbounded();
        let req = request(9, ring_slot(1, 5));
        {
            let mut h = ReplyTx::owing(tx, &req, req.slot, None);
            let hdr = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: 0,
                unique: req.unique,
            };
            let data = get_bincode_config().serialize(&hdr).unwrap();
            h.send(Either::Left(data)).await.expect("send");
        }
        let first = rx.next().await.expect("the handler's reply");
        assert_eq!(out_header(&first).1, 0);
        assert!(
            rx.next().await.is_none(),
            "exactly one reply per request — the drop must add nothing"
        );
    }

    /// `reply_error_in_place` consumes the request's one handle, so the
    /// error IS the reply and the drop adds nothing (row 2's shape).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reply_error_in_place_is_the_one_reply() {
        let (tx, mut rx) = unbounded();
        let req = request(11, ring_slot(2, 6));
        reply_error_in_place(
            libc::ENOSYS.into(),
            req,
            ReplyTx::owing(tx, &req, req.slot, None),
        )
        .await;
        let reply = rx.next().await.expect("the error reply");
        assert_eq!(out_header(&reply).1, -libc::ENOSYS);
        assert!(rx.next().await.is_none(), "exactly one reply");
    }

    /// A no-reply handle (FORGET/BATCH_FORGET, INTERRUPT after FUSE-3i,
    /// daemon notifications) must synthesize nothing: the ring ent is
    /// auto-committed by the queue worker and the kernel expects no
    /// reply at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_no_reply_handle_stays_silent() {
        let (tx, mut rx) = unbounded();
        drop(ReplyTx::no_reply(tx));
        assert!(
            rx.next().await.is_none(),
            "no-reply traffic must never produce a synthesized reply"
        );
    }

    /// Rows 2 and 3: when the reply task is gone the send must not be
    /// discarded. Without a connection there is nowhere left to go, and
    /// the request lands on the must-stay-0 tripwire — loudly — rather
    /// than disappearing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_reply_task_is_reported_not_ignored() {
        let (tx, rx) = unbounded::<FuseReply>();
        drop(rx);
        let req = request(31, ring_slot(4, 8));
        let before = fuse3_abandoned();
        {
            let mut h = ReplyTx::owing(tx, &req, req.slot, None);
            let hdr = fuse_out_header {
                len: FUSE_OUT_HEADER_SIZE as u32,
                error: 0,
                unique: req.unique,
            };
            let data = get_bincode_config().serialize(&hdr).unwrap();
            assert!(
                h.send(Either::Left(data)).await.is_err(),
                "an undeliverable reply must be reported to its caller"
            );
        }
        assert!(
            fuse3_abandoned() > before,
            "a reply that reached neither the reply task nor a slot is an abandoned request"
        );
    }

    #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
    fn fuse3_abandoned() -> u64 {
        crate::raw::connection::fuse_over_uring::transport_reply_integrity_stats().1
    }

    #[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
    fn fuse3_abandoned() -> u64 {
        0
    }
}

/// FUSE-3l — the two unchecked slice splits in the DISPATCH task. A
/// short `FUSE_NOTIFY_REPLY` or `BATCH_FORGET` panicked the whole
/// session (not just one handler), so every in-flight request lost its
/// reply at once — one malformed message, a session-wide FUSE-2 event.
#[cfg(test)]
mod dispatch_bounds_tests {
    use super::*;

    #[test]
    fn short_notify_reply_body_is_rejected_not_split() {
        for len in 0..FUSE_NOTIFY_RETRIEVE_IN_SIZE {
            let data = vec![0u8; len];
            assert_eq!(
                notify_retrieve_body(&data, 0),
                None,
                "len {len}: a body shorter than its own header must be refused, not split"
            );
        }
        let exact = vec![0u8; FUSE_NOTIFY_RETRIEVE_IN_SIZE];
        assert_eq!(
            notify_retrieve_body(&exact, 0),
            Some(&[][..]),
            "an exactly-header-sized body yields an empty payload"
        );
        // Header present, payload shorter than the announced size.
        let short_payload = vec![0u8; FUSE_NOTIFY_RETRIEVE_IN_SIZE + 3];
        assert_eq!(
            notify_retrieve_body(&short_payload, 8),
            None,
            "an over-claimed size must be refused, never truncated silently"
        );
        assert_eq!(
            notify_retrieve_body(&short_payload, 3).map(<[u8]>::len),
            Some(3),
            "an honest size yields exactly that many bytes"
        );
    }

    #[test]
    fn short_batch_forget_body_is_rejected_not_split() {
        for len in 0..FUSE_BATCH_FORGET_IN_SIZE {
            let data = vec![0u8; len];
            assert_eq!(
                batch_forget_body(&data),
                None,
                "len {len}: a BATCH_FORGET shorter than its header must be refused"
            );
        }
        let ok = vec![0u8; FUSE_BATCH_FORGET_IN_SIZE + FUSE_FORGET_ONE_SIZE];
        assert_eq!(
            batch_forget_body(&ok).map(<[u8]>::len),
            Some(FUSE_FORGET_ONE_SIZE),
            "a well-formed batch yields its forget records"
        );
    }
}

/// FUSE-3g — a delivery's body is bounded by the bytes the transport
/// actually FILLED, never by what `in_header.len` claims. Two distinct
/// defects live in the historical `in_header.len as usize -
/// FUSE_IN_HEADER_SIZE`:
///
/// 1. `len < 40` UNDERFLOWS (release build), producing a ~2^64 length and
///    an immediate slice panic on the DISPATCH task — every in-flight
///    request on the session loses its reply (the FUSE-2 blast radius).
/// 2. `len > 40 + filled` hands the handler a slice whose tail is the
///    PREVIOUS request's bytes: the session data buffer is reused across
///    the whole dispatch loop, so the "body" of a clamped delivery is
///    stale data presented as this request's own.
#[cfg(test)]
mod delivery_bounds_tests {
    use super::*;

    #[test]
    fn a_header_shorter_than_the_in_header_never_underflows() {
        for len in 0..FUSE_IN_HEADER_SIZE as u32 {
            assert_eq!(
                validated_body(len, 4096, 0, 4096),
                BodyBounds::ShortHeader,
                "in_header.len {len} is below the header itself: refuse, never subtract"
            );
        }
        assert_eq!(
            validated_body(FUSE_IN_HEADER_SIZE as u32, 4096, 0, 4096),
            BodyBounds::Valid(0),
            "a header-only request has an EMPTY body, not a refusal"
        );
    }

    #[test]
    fn an_overdeclared_body_is_refused_not_clamped_over_stale_bytes() {
        // The clamped-delivery shape: the kernel/transport filled 8 body
        // bytes, the header claims 100. The historical code sliced 100.
        assert_eq!(
            validated_body(FUSE_IN_HEADER_SIZE as u32 + 100, 8, 0, 4096),
            BodyBounds::Overdeclared {
                declared: 100,
                available: 8,
            },
            "a header claiming more body than was filled must be refused"
        );
        // Exactly-filled is the ordinary case.
        assert_eq!(
            validated_body(FUSE_IN_HEADER_SIZE as u32 + 100, 100, 0, 4096),
            BodyBounds::Valid(100)
        );
    }

    #[test]
    fn the_write_lease_body_rides_the_payload_and_bounds_the_data_slice() {
        // §5.4 zero-copy FUSE_WRITE: `in_header.len` counts the whole 1 MiB
        // payload, the data buffer holds ONLY the fuse_write_in arg, and the
        // body itself rides the payload lease. Available must count the
        // payload (else every large write is refused) while the returned
        // SLICE stays bounded by what was filled (else the handler reads
        // past it).
        const MIB: usize = 1024 * 1024;
        assert_eq!(
            validated_body(
                (FUSE_IN_HEADER_SIZE + FUSE_WRITE_IN_SIZE + MIB) as u32,
                FUSE_WRITE_IN_SIZE,
                MIB,
                4096,
            ),
            BodyBounds::Valid(FUSE_WRITE_IN_SIZE),
            "the write arg is the whole data-buffer body; the payload is separate"
        );
        // A write whose payload is SHORTER than the header claims is still
        // a refusal (handle_write's own size check is the second gate).
        assert!(matches!(
            validated_body(
                (FUSE_IN_HEADER_SIZE + FUSE_WRITE_IN_SIZE + MIB) as u32,
                FUSE_WRITE_IN_SIZE,
                MIB - 1,
                4096,
            ),
            BodyBounds::Overdeclared { .. }
        ));
    }

    #[test]
    fn the_data_buffer_capacity_is_the_last_bound() {
        assert_eq!(
            validated_body(FUSE_IN_HEADER_SIZE as u32 + 4096, 4096, 0, 512),
            BodyBounds::Valid(512),
            "the slice can never exceed the buffer it comes from"
        );
    }
}

#[cfg(test)]
mod init_negotiation_tests {
    use super::*;

    /// L3 transport-economy lever A: the daemon implements NO splice reply
    /// path — on an armed FUSE-over-io_uring session every classical splice
    /// reply for a ring unique bounces off `/dev/fuse` with ENOENT (the
    /// kernel holds ring uniques in the uring ent, not `fpq->processing`),
    /// costing a `pipe2+write+vmsplice+splice+2×close+fcntl` block per READ
    /// reply before the vectored fallback delivers it anyway (measured
    /// 0.75 blocks/op, 100 % splice error rate —
    /// `.benchmarks/2026-07-18-l3-transport-economy.md`). A capability the
    /// daemon does not implement must never be advertised: the splice
    /// family never echoes, whatever the kernel offers.
    #[test]
    fn init_reply_never_advertises_splice() {
        // linux/fuse.h uapi bits (constants deleted with the splice code):
        // FUSE_SPLICE_WRITE = 1<<7, FUSE_SPLICE_MOVE = 1<<8,
        // FUSE_SPLICE_READ = 1<<9.
        const FUSE_SPLICE_FAMILY: u32 = (1 << 7) | (1 << 8) | (1 << 9);
        let opts = MountOptions::default();
        let flags = negotiate_reply_flags(u32::MAX, &opts);
        assert_eq!(
            flags & FUSE_SPLICE_FAMILY,
            0,
            "INIT reply advertised FUSE_SPLICE_* but the daemon has no splice \
             implementation (reply routing is over-uring COMMIT_AND_FETCH + \
             classical vectored write only)"
        );
    }

    /// fstests generic/504 (VL10 release gate): the daemon implements NO
    /// FLOCK handler, yet the INIT reply echoed `FUSE_FLOCK_LOCKS` — so
    /// the kernel skipped its own canonical BSD-flock bookkeeping
    /// (`/proc/locks` showed nothing for a held flock: "lock info not
    /// found"). A capability the daemon does not implement must never be
    /// advertised: flock stays KERNEL-LOCAL (correct semantics,
    /// /proc/locks visibility, and D0's single-writer mount guard is
    /// what bounds cross-mount exposure).
    #[test]
    fn init_reply_never_advertises_flock_locks() {
        let opts = MountOptions::default();
        let flags = negotiate_reply_flags(u32::MAX, &opts);
        assert_eq!(
            flags & FUSE_FLOCK_LOCKS,
            0,
            "INIT reply advertised FUSE_FLOCK_LOCKS but no flock handler exists \
             — BSD flock must stay kernel-local"
        );
    }

    /// Kernel-local POSIX byte-range locks (fstests generic/131 subtest
    /// 27, generic/478, generic/504 — the VL10 release gate): a
    /// daemon-arbitrated lock table cannot satisfy the full POSIX
    /// surface — unlock-on-close rides FLUSH's `lock_owner`, which the
    /// D2 clean-handle ENOSYS latch elides connection-wide (131's
    /// "close without unlocking" leg), OFD locks carry
    /// file-description owner semantics the wire does not represent
    /// (478), and `/proc/locks` only shows kernel-tracked locks (504's
    /// flock twin). The kernel's `posix_lock_file` is the canonical
    /// implementation, and intra-mount arbitration is the whole
    /// requirement under the D0 single-writer mount guard (cross-client
    /// POSIX locks never existed — the daemon's Remote lock variant was
    /// never constructed). The capability must never be echoed.
    #[test]
    fn init_reply_never_advertises_posix_locks() {
        let opts = MountOptions::default();
        let flags = negotiate_reply_flags(u32::MAX, &opts);
        assert_eq!(
            flags & FUSE_POSIX_LOCKS,
            0,
            "INIT reply advertised FUSE_POSIX_LOCKS — POSIX fcntl locks must \
             stay kernel-local"
        );
    }

    /// The no-ACL posture (fstests generic/099/319): negotiating
    /// FUSE_POSIX_ACL makes the kernel's `posix_acl_create` probe the
    /// parent's default ACL on EVERY create — with a daemon that refuses
    /// ACL xattrs, that poisons file creation wholesale (every
    /// open(O_CREAT)/mkdir returned EOPNOTSUPP — the VL10 targeted-rerun
    /// regression). The daemon implements no ACL semantics, so the
    /// capability must never be advertised, default_permissions or not.
    #[test]
    fn init_reply_never_advertises_posix_acl() {
        let mut opts = MountOptions::default();
        opts.default_permissions(true);
        let flags = negotiate_reply_flags(u32::MAX, &opts);
        assert_eq!(
            flags & FUSE_POSIX_ACL,
            0,
            "INIT reply advertised FUSE_POSIX_ACL but the daemon refuses ACL xattrs"
        );
    }

    /// sqz `FUSE_TIME_LIMITS` (kernel-sqz patch 0027, V2-CANDIDATES.md
    /// candidate 5): the capability rides flags2 high space — folded
    /// capability bit 62 = flags2 bit 30, deliberately far above
    /// upstream's bit-42 watermark. The fold placement is the ABI: a
    /// kernel offering flags2 bit 30 must read back as capability bit 62.
    #[test]
    fn time_limits_capability_is_folded_bit_62() {
        assert_eq!(FUSE_TIME_LIMITS, 1u64 << 62, "the sqz-private bit");
        assert_eq!(
            (FUSE_TIME_LIMITS >> 32) as u32,
            1u32 << 30,
            "bit 62 must arrive as flags2 bit 30"
        );
        let ki = KernelInit::new(7, 45, 0, 1 << 30);
        assert_ne!(
            ki.flags & FUSE_TIME_LIMITS,
            0,
            "KernelInit fold must surface flags2 bit 30 as capability bit 62"
        );
    }

    /// Offered ⇒ populated: the daemon answers with the exact-round-trip
    /// ±9,223,372,036 s range (the whole-second interior of the i64-ns
    /// storage word — the saturation law pinned in
    /// `tests/attr_refresh_tests.rs`), so the kernel's incore clamp
    /// (`timestamp_truncate`) and the daemon's durable clamp can never
    /// disagree.
    #[test]
    fn time_limits_populate_when_offered() {
        assert_eq!(
            negotiate_time_limits(FUSE_TIME_LIMITS | (1 << 41) | 0xdead),
            Some((TIME_LIMITS_MIN_SEC, TIME_LIMITS_MAX_SEC)),
            "kernel offered bit 62: the reply must carry the timestamp range"
        );
        assert_eq!(TIME_LIMITS_MAX_SEC, 9_223_372_036);
        assert_eq!(TIME_LIMITS_MIN_SEC, -9_223_372_036);
        // Exact representability of the kernel-clamped extremes (sec, 0)
        // inside the daemon's i64-ns word — the floor is conservative by
        // one second BY DESIGN (the exact ns floor has nsec 145,224,192,
        // which a kernel clamp to (floor_sec, 0) could not round-trip).
        assert!(TIME_LIMITS_MAX_SEC.checked_mul(1_000_000_000).is_some());
        assert!(TIME_LIMITS_MIN_SEC.checked_mul(1_000_000_000).is_some());
    }

    /// Stock kernel (bit not offered) ⇒ `None`: fields stay zero, the
    /// flag is not echoed — the INIT reply must be BIT-IDENTICAL to the
    /// pre-0027 daemon (feature-absent is unobservable; the kernel
    /// additionally gates on nonzero `time_max`, so zeros are inert even
    /// against a misbehaving offer).
    #[test]
    fn time_limits_absent_without_offer() {
        assert_eq!(negotiate_time_limits(0), None);
        assert_eq!(
            negotiate_time_limits(!FUSE_TIME_LIMITS),
            None,
            "every other capability set must not conjure time limits"
        );
    }

    /// The INIT reply ABI (kernel-sqz patch 0027 uapi layout): the reply
    /// stays 64 bytes with `time_min`/`time_max` at byte offsets 48/56 —
    /// `unused[3]` precedes the i64 pair so both stay naturally aligned.
    /// This is the wire contract REGISTER-side kernels deserialize; a
    /// drifted offset silently corrupts `s_time_min/max`.
    #[test]
    fn init_out_abi_64_bytes_time_limits_at_48_56() {
        let out = fuse_init_out {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            max_background: 0,
            congestion_threshold: 0,
            max_write: 0,
            time_gran: 1,
            max_pages: 0,
            map_alignment: 0,
            flags2: (FUSE_TIME_LIMITS >> 32) as u32,
            max_stack_depth: 0,
            request_timeout: 0,
            unused: [0; 3],
            time_min: TIME_LIMITS_MIN_SEC,
            time_max: TIME_LIMITS_MAX_SEC,
        };
        let bytes = get_bincode_config()
            .serialize(&out)
            .expect("fuse_init_out must serialize");
        assert_eq!(bytes.len(), 64, "fuse_init_out must stay the uapi 64 bytes");
        assert_eq!(bytes.len(), FUSE_INIT_OUT_SIZE);
        assert_eq!(
            &bytes[48..56],
            &TIME_LIMITS_MIN_SEC.to_le_bytes(),
            "time_min must sit at byte offset 48"
        );
        assert_eq!(
            &bytes[56..64],
            &TIME_LIMITS_MAX_SEC.to_le_bytes(),
            "time_max must sit at byte offset 56"
        );
    }

    /// The negotiation still echoes the capabilities the daemon DOES
    /// implement (extraction sanity: the splice fix must not eat the rest
    /// of the capability word).
    #[test]
    fn init_reply_echoes_implemented_caps() {
        let opts = MountOptions::default();
        let flags = negotiate_reply_flags(u32::MAX, &opts);
        for (cap, name) in [
            (FUSE_ASYNC_READ, "FUSE_ASYNC_READ"),
            (FUSE_PARALLEL_DIROPS, "FUSE_PARALLEL_DIROPS"),
            (FUSE_MAX_PAGES, "FUSE_MAX_PAGES"),
            (FUSE_ASYNC_DIO, "FUSE_ASYNC_DIO"),
            (FUSE_AUTO_INVAL_DATA, "FUSE_AUTO_INVAL_DATA"),
        ] {
            assert!(
                flags & cap > 0,
                "{name} must echo when the kernel offers it"
            );
        }
        // Option-gated capabilities follow their mount option.
        assert_eq!(
            flags & FUSE_WRITEBACK_CACHE,
            0,
            "FUSE_WRITEBACK_CACHE requires MountOptions::write_back"
        );
        let mut wb = MountOptions::default();
        wb.write_back(true);
        assert!(negotiate_reply_flags(u32::MAX, &wb) & FUSE_WRITEBACK_CACHE > 0);
        // Nothing offered ⇒ nothing echoed (an unforced flag can never appear).
        assert_eq!(
            negotiate_reply_flags(0, &MountOptions::default()),
            0,
            "no kernel-offered capability may be invented by the daemon"
        );
    }

    /// FUSE_HANDLE_KILLPRIV_V2 (the 2026-07-28 killpriv campaign —
    /// `.benchmarks/2026-07-27-oq1-overwrite-op-economy.md` §4/§5): without
    /// it the kernel probes `GETXATTR("security.capability")` once per
    /// `write(2)` syscall (`file_remove_privs` — HALF of every
    /// write-syscall-bound stream's FUSE requests, answered ENODATA every
    /// time). Negotiating V2 transfers the suid/sgid/caps-killing
    /// obligation to the daemon (`FUSE_WRITE_KILL_SUIDGID` /
    /// `FUSE_OPEN_KILL_SUIDGID` / `FATTR_KILL_SUIDGID`) and deletes the
    /// probe. Option-gated: the daemon only advertises it when its
    /// handlers implement the clearing law (a capability the daemon does
    /// not implement must never be advertised).
    #[test]
    fn init_reply_advertises_handle_killpriv_v2_when_offered_and_enabled() {
        let mut opts = MountOptions::default();
        opts.handle_killpriv_v2(true);
        let flags = negotiate_reply_flags(u32::MAX, &opts);
        assert!(
            flags & FUSE_HANDLE_KILLPRIV_V2 > 0,
            "FUSE_HANDLE_KILLPRIV_V2 must echo when the kernel offers it and \
             the mount enables it (the per-write GETXATTR killpriv probe \
             economy)"
        );
        // V1 must never ride along uninvited: it is the coarser
        // unconditional-kill contract (no group-exec sgid preservation)
        // and the daemon deliberately adopts V2 only.
        assert_eq!(
            flags & FUSE_HANDLE_KILLPRIV,
            0,
            "FUSE_HANDLE_KILLPRIV (v1) must not echo — the daemon implements \
             the V2 clearing law, not v1's unconditional kill"
        );
        // Not offered ⇒ never invented (older kernels keep the classical
        // kernel-side killpriv probe — the correct degraded posture).
        assert_eq!(
            negotiate_reply_flags(!FUSE_HANDLE_KILLPRIV_V2, &opts) & FUSE_HANDLE_KILLPRIV_V2,
            0,
            "FUSE_HANDLE_KILLPRIV_V2 must never be invented when the kernel \
             does not offer it"
        );
    }

    /// The option gate: a mount that has not armed the daemon-side
    /// clearing handlers (`MountOptions::handle_killpriv_v2`) must never
    /// advertise the capability, whatever the kernel offers — advertising
    /// an unimplemented capability silently disables the kernel's own
    /// killpriv machinery (a security regression, not a perf bug).
    #[test]
    fn init_reply_never_advertises_handle_killpriv_v2_when_option_off() {
        let opts = MountOptions::default();
        let flags = negotiate_reply_flags(u32::MAX, &opts);
        assert_eq!(
            flags & FUSE_HANDLE_KILLPRIV_V2,
            0,
            "FUSE_HANDLE_KILLPRIV_V2 echoed without MountOptions::handle_killpriv_v2 \
             — the kernel would stop killing privs and nothing would"
        );
    }

    /// FUSE-1 (pre-rc spec §4, D6 pulled forward): every INIT reply
    /// carrying a nonzero `flags2` must set `FUSE_INIT_EXT` in `flags`
    /// AND report minor ≥ 36 — mainline `process_init_reply()` folds the
    /// reply's `flags2` into the capability word only under exactly that
    /// shape. Without both, the kernel discards the daemon's whole
    /// `flags2`: `FUSE_OVER_IO_URING` (bit 41) survives only via the
    /// module-param side door (`fc->io_uring` stays 0 — no
    /// `fuse_block_alloc` arm-window gating), and sqz `FUSE_TIME_LIMITS`
    /// (bit 62, patch 0027) has no side door at all.
    #[test]
    fn init_ext_and_minor_36_ride_every_reply_with_nonzero_flags2() {
        // The minor pin itself: 36 EXACTLY — the value the 31→36 audit
        // covered (no fc->minor-gated kernel behavior exists in (23, 36];
        // everything 7.32–7.36 rides INIT flags). Raising it further
        // requires a fresh 36→N audit of every `fc->minor` gate — do not
        // chase newer minors speculatively.
        assert_eq!(
            FUSE_KERNEL_MINOR_VERSION, 36,
            "FUSE_KERNEL_MINOR_VERSION must be exactly 36 (the audited value); \
             a different value needs its own kernel-gate audit"
        );

        // sqz-kernel shape: kernel offers INIT_EXT (all 7.36+ kernels do),
        // reply flags2 = OVER_IO_URING + the TIME_LIMITS echo.
        let flags2 = (1u32 << 9) | (1u32 << 30);
        let (minor, ext) = negotiate_init_ext(45, FUSE_INIT_EXT | 0x0fff_ffff, flags2);
        assert_eq!(
            ext, FUSE_INIT_EXT,
            "nonzero flags2 without FUSE_INIT_EXT in the reply is a discarded \
             capability word (bit 41 AND bit 62)"
        );
        assert!(
            minor >= 36,
            "the kernel honors the flags2 fold under ≥ 7.36 reply semantics; \
             got minor {minor}"
        );

        // Stock-kernel shape: OVER_IO_URING alone still needs the fold.
        let (minor, ext) = negotiate_init_ext(40, FUSE_INIT_EXT, 1u32 << 9);
        assert_eq!(ext, FUSE_INIT_EXT, "uring-only flags2 must still fold");
        assert!(minor >= 36);
    }

    /// FUSE-1 guard: `FUSE_INIT_EXT` must NEVER be set toward a kernel
    /// that did not offer it — pre-7.36 kernels never sent the bit and
    /// treat reply bit 30 as garbage in a flags word they parse
    /// classically. (The companion law: a capability the daemon does not
    /// implement must never be advertised; a header-mechanics bit the
    /// KERNEL does not speak must never be echoed either.)
    #[test]
    fn init_ext_never_invented_without_kernel_offer() {
        let (_, ext) = negotiate_init_ext(35, !FUSE_INIT_EXT, 1u32 << 9);
        assert_eq!(
            ext, 0,
            "FUSE_INIT_EXT invented toward a kernel that did not offer it"
        );
    }

    /// FUSE-1 guard: a zero `flags2` has nothing to fold — the reply must
    /// not set `FUSE_INIT_EXT` (the non-over-uring build shape; keeps the
    /// reply bit-identical to the classical posture there).
    #[test]
    fn init_ext_absent_when_reply_flags2_zero() {
        let (_, ext) = negotiate_init_ext(45, FUSE_INIT_EXT, 0);
        assert_eq!(ext, 0, "FUSE_INIT_EXT with nothing to fold");
    }

    /// FUSE-1: the reply minor is `min(kernel_minor, ours)` — the daemon
    /// never claims a protocol minor the kernel did not offer (the kernel
    /// stores the reply verbatim as `fc->minor` and keys compat behavior
    /// on it), and `FUSE_INIT_EXT` set implies ≥ 7.36 reply semantics
    /// (structural: only ≥ 7.36 kernels offer the bit, and min() keeps 36).
    #[test]
    fn reply_minor_never_exceeds_kernel_offer_min_semantics() {
        // Newer kernel than us: reply our own audited 36, never chase.
        let (minor, _) = negotiate_init_ext(45, FUSE_INIT_EXT, 1u32 << 9);
        assert_eq!(minor, 36, "kernel 7.45 offer must negotiate down to ours");
        // Equal: 36.
        let (minor, ext) = negotiate_init_ext(36, FUSE_INIT_EXT, 1u32 << 9);
        assert_eq!(minor, 36);
        assert_eq!(ext, FUSE_INIT_EXT);
        // Older kernel than us: never exceed the kernel's offer.
        for (km, kf, f2) in [
            (31u32, 0u32, 0u32),
            (31, !FUSE_INIT_EXT, 1 << 9),
            (35, !FUSE_INIT_EXT, (1 << 9) | (1 << 30)),
            (13, 0, 0),
        ] {
            let (minor, ext) = negotiate_init_ext(km, kf, f2);
            assert!(
                minor <= km,
                "reply minor {minor} exceeds the kernel's offered {km} — the \
                 kernel would store a minor it never spoke"
            );
            assert!(minor <= FUSE_KERNEL_MINOR_VERSION);
            assert_eq!(ext, 0, "pre-7.36 offers can never carry INIT_EXT");
        }
        // The coherence law over the whole shape space: INIT_EXT ⇒ the
        // kernel offered it, flags2 nonzero, and ≥ 7.36 reply semantics.
        for km in [13u32, 27, 31, 35, 36, 40, 45, 99] {
            for kf in [0u32, FUSE_INIT_EXT, u32::MAX, !FUSE_INIT_EXT] {
                for f2 in [0u32, 1 << 9, (1 << 9) | (1 << 30)] {
                    let (minor, ext) = negotiate_init_ext(km, kf, f2);
                    assert!(minor <= km && minor <= FUSE_KERNEL_MINOR_VERSION);
                    if ext != 0 {
                        assert_eq!(ext, FUSE_INIT_EXT);
                        assert!(kf & FUSE_INIT_EXT != 0 && f2 != 0);
                        assert!(minor >= 36, "INIT_EXT under sub-7.36 reply semantics");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod kernel_init_tests {
    use super::*;

    /// The extended-init fold must place `flags2` at bits 32..63 — the
    /// uapi layout the daemon's capability probes (FOPEN_NOFLUSH minor
    /// gate, atomic-open-class scan) read. FUSE_OVER_IO_URING (1u64<<41)
    /// arrives as bit 9 of flags2 and is the placement witness.
    #[test]
    fn kernel_init_folds_flags2_into_high_bits() {
        let ki = KernelInit::new(7, 45, 0x8000_0001, 1 << 9);
        assert_eq!(ki.major, 7);
        assert_eq!(ki.minor, 45);
        assert_eq!(
            ki.flags & 0xFFFF_FFFF,
            0x8000_0001,
            "classical flags in bits 0..31"
        );
        assert_eq!(
            ki.flags & (1u64 << 41),
            1u64 << 41,
            "flags2 bit 9 must land at capability bit 41 (FUSE_OVER_IO_URING)"
        );
    }
}

#[cfg(test)]
mod tpc_dispatch_tests {
    use super::*;

    /// Test lanes: each backed by a REAL sqz-exec executor thread; the
    /// dispatched futures signal a channel so delivery is observable.
    fn live_lane() -> (Lane, std::thread::JoinHandle<()>) {
        let exec = crate::sqz_exec::LaneExec::new();
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let ex2 = exec.clone();
        let jh = std::thread::spawn(move || ex2.run());
        (Lane { exec, alive }, jh)
    }

    /// A DEAD lane: its liveness word is down (the thread-exit mark —
    /// the shape the DeadMark drop guard produces when a lane thread is
    /// lost).
    fn dead_lane() -> Lane {
        let exec = crate::sqz_exec::LaneExec::new();
        Lane {
            exec,
            alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// One test (the redispatch counter is process-global): healthy
    /// lanes never pay the counter; a dead lane (liveness word down =
    /// the lane thread died, the ingest-economy board item 2 shape) must
    /// never blackhole a dispatch — the future re-routes to a live lane,
    /// counted loudly.
    #[test]
    fn dead_lane_redispatches_loudly_healthy_lanes_never() {
        // Phase 1 — healthy: no counter movement, everything delivered.
        let (l0, j0) = live_lane();
        let (l1, j1) = live_lane();
        let lanes = vec![l0, l1];
        let before = tpc_lane_redispatches();
        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        for first_idx in 0..4 {
            let tx = tx.clone();
            TpcScheduler::dispatch(
                &lanes,
                first_idx % 2,
                Box::pin(async move {
                    let _ = tx.send(first_idx);
                }),
            );
        }
        for _ in 0..4 {
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("healthy lanes deliver everything");
        }
        assert_eq!(
            tpc_lane_redispatches(),
            before,
            "healthy dispatch must not touch the dead-lane counter"
        );
        for l in &lanes {
            l.exec.shutdown();
        }
        j0.join().unwrap();
        j1.join().unwrap();

        // Phase 2 — kill lane 1: every dispatch still lands on SOME live
        // lane (a swallowed future is a FUSE reply that never happens —
        // the D-state wedge), and the re-route is counted.
        let (l0, j0) = live_lane();
        let (l2, j2) = live_lane();
        let lanes = vec![l0, dead_lane(), l2];
        let (tx, rx) = std::sync::mpsc::channel::<usize>();
        for first_idx in 0..3 {
            let tx = tx.clone();
            TpcScheduler::dispatch(
                &lanes,
                first_idx,
                Box::pin(async move {
                    let _ = tx.send(first_idx);
                }),
            );
        }
        for _ in 0..3 {
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("every dispatch must land on a live lane");
        }
        assert!(
            tpc_lane_redispatches() > before,
            "a dead-lane re-route must be counted (the loud half of the fix)"
        );
        for l in &lanes {
            l.exec.shutdown();
        }
        j0.join().unwrap();
        j2.join().unwrap();
    }
}

/// FUSE-4d — `max_readahead` is echoed VERBATIM and deliberately never
/// consulted, and the value the kernel was told is published so operators
/// can see both numbers.
///
/// The limit bounds the KERNEL's per-file readahead requests to the daemon.
/// The daemon's own R2 prefetch window is a device-side pipeline depth
/// (bandwidth × latency, clamped by the R5 memory budget): different
/// resource, different unit. Clamping one by the other would let a
/// page-cache limit throttle a device pipeline (or the reverse), which is
/// why the independence is a decision rather than an omission — and why the
/// echo must stay exact: negotiating a SMALLER value silently caps the
/// kernel's readahead on every mount.
#[cfg(test)]
mod readahead_negotiation_tests {
    use super::*;

    #[test]
    fn the_kernels_readahead_limit_is_echoed_verbatim_and_published() {
        for want in [0u32, 4096, 131_072, 4 * 1024 * 1024, u32::MAX] {
            let echoed = negotiate_max_readahead(want);
            assert_eq!(
                echoed, want,
                "max_readahead must be echoed verbatim — a clamped echo caps \
                 the kernel's readahead for the mount's life"
            );
            assert_eq!(
                crate::raw::connection::fuse_over_uring::negotiated_max_readahead(),
                u64::from(want),
                "the negotiated limit must be published for the stats inode \
                 (the operator's half of the documented independence)"
            );
        }
    }
}
