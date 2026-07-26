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

#[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
use async_fs::read_dir;
#[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
use async_global_executor::{self as task, Task as JoinHandle};
#[cfg(all(
    target_os = "linux",
    not(feature = "tokio-runtime"),
    feature = "async-io-runtime",
    feature = "unprivileged"
))]
use async_process::Command;
use bincode::Options;
use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_util::future::{Either, FutureExt};
use futures_util::select;
use futures_util::sink::{Sink, SinkExt};
use futures_util::stream::StreamExt;
use nix::mount;
#[cfg(target_os = "freebsd")]
use nix::mount::MntFlags;
#[cfg(all(
    target_os = "linux",
    not(feature = "async-io-runtime"),
    feature = "tokio-runtime",
    feature = "unprivileged"
))]
use tokio::process::Command;
#[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
use tokio::task::JoinHandle;
#[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
use tokio::{fs::read_dir, task};
use tracing::{debug, debug_span, error, instrument, warn, Instrument, Span};

#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use crate::find_fusermount3;
use crate::helper::*;
use crate::notify::Notify;
use crate::raw::abi::*;
#[cfg(any(feature = "async-io-runtime", feature = "tokio-runtime"))]
use crate::raw::connection::FuseConnection;
use crate::raw::filesystem::Filesystem;
use crate::raw::reply::ReplyXAttr;
use crate::raw::request::Request;
use crate::raw::FuseData;
use crate::MountOptions;
use crate::{Errno, SetAttr};

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

            #[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
            {
                task::spawn(inner.inner_unmount()).detach();
            }

            #[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
            {
                task::spawn(inner.inner_unmount());
            }
        }
    }
}

#[derive(Debug)]
struct MountHandleInner {
    task: JoinHandle<IoResult<()>>,
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

        #[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
        {
            // wait destroy done
            self.task.await?;

            // TODO: freebsd mount is unprivileged, then unmount is unprivileged too?
            #[cfg(target_os = "freebsd")]
            {
                task::spawn_blocking(move || {
                    mount::unmount(&self.mount_path, MntFlags::MNT_SYNCHRONOUS)
                })
                .await?;
            }

            #[cfg(target_os = "linux")]
            {
                #[cfg(all(target_os = "linux", feature = "unprivileged"))]
                if self.unprivileged {
                    let binary_path = find_fusermount3()?;
                    let mut child = Command::new(binary_path)
                        .args([OsStr::new("-u"), self.mount_path.as_os_str()])
                        .spawn()?;
                    if !child.status().await?.success() {
                        return Err(IoError::new(
                            ErrorKind::Other,
                            "call fusermount3 -u to unmount failed",
                        ));
                    }

                    return Ok(());
                }

                task::spawn_blocking(move || mount::umount(&self.mount_path)).await?;
            }
        }

        #[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
        {
            // wait destroy done
            if !self.task.is_finished() {
                self.task.await.unwrap()?;
            }

            // TODO: freebsd mount is unprivileged, then unmount is unprivileged too?
            #[cfg(target_os = "freebsd")]
            {
                task::spawn_blocking(move || {
                    mount::unmount(&self.mount_path, MntFlags::MNT_SYNCHRONOUS)
                })
                .await
                .unwrap()?;
            }

            #[cfg(target_os = "linux")]
            {
                #[cfg(all(target_os = "linux", feature = "unprivileged"))]
                if self.unprivileged {
                    let binary_path = find_fusermount3()?;
                    let mut success = false;
                    for attempt in 0..10 {
                        if attempt > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        let mut child = Command::new(&binary_path)
                            .args([OsStr::new("-u"), self.mount_path.as_os_str()])
                            .spawn()?;
                        if child.wait().await?.success() {
                            success = true;
                            break;
                        }
                    }
                    if !success {
                        return Err(IoError::new(
                            ErrorKind::Other,
                            "call fusermount3 -u to unmount failed",
                        ));
                    }

                    return Ok(());
                }

                let mount_path = self.mount_path.clone();
                let mut success = false;
                for attempt in 0..10 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    let mp = mount_path.clone();
                    let res = task::spawn_blocking(move || mount::umount(&mp))
                        .await
                        .unwrap();
                    if res.is_ok() {
                        success = true;
                        break;
                    }
                }
                if !success {
                    return Err(IoError::new(
                        ErrorKind::Other,
                        "umount failed after retries",
                    ));
                }
            }
        }

        Ok(())
    }
}

impl Future for MountHandle {
    type Output = IoResult<()>;

    #[cfg(feature = "async-io-runtime")]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.inner.as_mut().expect("inner should be Some()").task).poll(cx)
    }

    #[cfg(feature = "tokio-runtime")]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // The unwrap is necessary in order to provide the same API for both runtimes, and actually
        // unwrap should not panic, when MountHandle is canceled by unmount method, user has no
        // chance to poll again
        Pin::new(&mut self.inner.as_mut().expect("inner should be Some()").task)
            .poll(cx)
            .map(Result::unwrap)
    }
}

#[cfg(any(feature = "async-io-runtime", feature = "tokio-runtime"))]
/// fuse filesystem session, inode based.
pub struct Session<FS> {
    fuse_connection: Option<Arc<FuseConnection>>,
    filesystem: Option<Arc<FS>>,
    response_sender: UnboundedSender<FuseData>,
    response_receiver: Option<UnboundedReceiver<FuseData>>,
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

#[cfg(any(feature = "async-io-runtime", feature = "tokio-runtime"))]
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
        Notify::new(self.response_sender.clone())
    }

    pub fn get_payload_buffer(&self, unique: u64) -> Option<(u64, usize)> {
        let conn = self.fuse_connection.as_ref()?;
        conn.get_payload_buffer(unique)
    }

    pub fn connection(&self) -> Option<Arc<FuseConnection>> {
        self.fuse_connection.clone()
    }
}

#[cfg(any(feature = "async-io-runtime", feature = "tokio-runtime"))]
impl<FS: Filesystem + Send + Sync + 'static> Session<FS> {
    async fn mount_empty_check(&self, mount_path: &Path) -> IoResult<()> {
        #[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
        if !self.mount_options.nonempty
            && matches!(read_dir(mount_path).await?.next_entry().await, Ok(Some(_)))
        {
            return Err(IoError::new(
                ErrorKind::AlreadyExists,
                "mount point is not empty",
            ));
        }

        #[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
        if !self.mount_options.nonempty && read_dir(mount_path).await?.next().await.is_some() {
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
                task: task::spawn(self.inner_mount()),
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
                task: task::spawn(self.inner_mount()),
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
                task: task::spawn(self.inner_mount()),
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

        #[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
        let reply_task = task::spawn(Self::reply_fuse(fuse_write_connection, receiver))
            .map(Result::unwrap)
            .fuse();
        #[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
        let reply_task = task::spawn(Self::reply_fuse(fuse_write_connection, receiver)).fuse();

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
            }
        }

        let fuse_write_connection = self.fuse_connection.as_ref().unwrap().clone();
        let receiver = self.response_receiver.take().unwrap();

        let dispatch_task = self.dispatch_with_max_write(max_write).fuse();
        let mut dispatch_task = pin!(dispatch_task);

        #[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
        let reply_task =
            task::spawn(async move { Self::reply_fuse(fuse_write_connection, receiver).await })
                .fuse();
        #[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
        let reply_task = task::spawn(Self::reply_fuse(fuse_write_connection, receiver))
            .map(Result::unwrap)
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
        mut response_receiver: UnboundedReceiver<FuseData>,
    ) -> IoResult<()> {
        while let Some(response) = response_receiver.next().await {
            let (data, extend_data, backing) = match response {
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
            if let Err(err) = fuse_connection.write_vectored(data, extend_data).await.1 {
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

        let (data_buffer, in_header) = match self
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
                ..
            } => {
                let in_header = in_header?;
                (data_buffer, in_header)
            }
        };

        let request = Request::from(&in_header);

        let opcode = match fuse_opcode::try_from(in_header.opcode) {
            Err(err) => {
                debug!("receive unknown opcode {}", err.0);

                reply_error_in_place(libc::ENOSYS.into(), request, &self.response_sender).await;

                return Err(IoError::new(
                    ErrorKind::Other,
                    format!("receive unknown opcode {}", err.0),
                ));
            }

            Ok(opcode) => opcode,
        };

        debug!("receive opcode {}", opcode);

        if opcode != fuse_opcode::FUSE_INIT {
            error!(?opcode, "received unexpected opcode");

            return Err(IoError::new(
                ErrorKind::Other,
                format!("unexpected opcode {opcode:?}"),
            ));
        }

        let data_size = in_header.len as usize - FUSE_IN_HEADER_SIZE;
        let data_ref = &data_buffer[..data_size];

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
        let (uring_payload, res) = match fuse_connection
            .read_vectored(header_buffer, data_buffer)
            .await
        {
            None => return ReadResult::Destroy,

            Some(((header_buf, data_buf, payload), res)) => {
                header_buffer = header_buf;
                data_buffer = data_buf;

                (payload, res)
            }
        };
        let n = match res {
            Err(err) => {
                // Kernel abort / unmount / fuse-over-uring pool shutdown.
                // Classical path: ENODEV (pre-FUSE_ABORT_ERROR) or ECONNABORTED.
                // Uring path: we surface ENOTCONN / NotConnected when the pool dies.
                let disconnect = match err.raw_os_error() {
                    Some(e)
                        if matches!(
                            e,
                            libc::ENODEV
                                | libc::ECONNABORTED
                                | libc::ENOTCONN
                                | libc::EPIPE
                                | libc::EBADF
                                | libc::ESHUTDOWN
                        ) =>
                    {
                        true
                    }
                    _ => {
                        err.kind() == ErrorKind::NotConnected || err.kind() == ErrorKind::BrokenPipe
                    }
                };
                if disconnect {
                    debug!("fuse connection dead ({err}); ending session");
                    #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
                    {
                        if let Some(pool) = fuse_connection.over_uring.lock().unwrap().take() {
                            pool.shutdown();
                        }
                    }
                    return ReadResult::Destroy;
                }

                error!("read from /dev/fuse failed {}", err);

                return ReadResult::Request {
                    in_header: Err(err),
                    header_buffer,
                    data_buffer,
                    uring_payload,
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
                in_header: Err(IoError::new(
                    ErrorKind::Other,
                    "read_vectored n is less then FUSE_IN_HEADER_SIZE",
                )),
                header_buffer,
                data_buffer,
                uring_payload,
            };
        }

        let in_header = match get_bincode_config().deserialize::<fuse_in_header>(&header_buffer) {
            Err(err) => {
                error!("deserialize fuse_in_header failed {}", err);

                return ReadResult::Request {
                    in_header: Err(IoError::new(ErrorKind::Other, err)),
                    header_buffer,
                    data_buffer,
                    uring_payload,
                };
            }

            Ok(in_header) => in_header,
        };

        ReadResult::Request {
            in_header: Ok(in_header),
            header_buffer,
            data_buffer,
            uring_payload,
        }
    }

    #[allow(dead_code)]
    async fn dispatch(&mut self) -> IoResult<()> {
        let fuse_connection = self.fuse_connection.clone().unwrap();
        let fs = self.filesystem.clone().expect("filesystem not init");
        let max_write = self.init_filesystem(&fs, &fuse_connection).await?.get() as usize;
        self.dispatch_with_max_write(max_write).await
    }

    async fn dispatch_with_max_write(&mut self, max_write: usize) -> IoResult<()> {
        let fuse_connection = self.fuse_connection.take().unwrap();
        let fs = self.filesystem.take().expect("filesystem not init");
        let buffer_size = (max_write + FUSE_WRITE_IN_SIZE).max(FUSE_MIN_READ_BUFFER_SIZE);

        let mut header_buffer = vec![0; FUSE_IN_HEADER_SIZE];
        let mut data_buffer = vec![0; buffer_size];

        loop {
            let uring_payload;
            let in_header = match self
                .read_fuse_request(&fuse_connection, header_buffer, data_buffer)
                .await
            {
                ReadResult::Destroy => {
                    fs.destroy(Request {
                        unique: 0,
                        uid: 0,
                        gid: 0,
                        pid: 0,
                    })
                    .await;

                    return Ok(());
                }

                ReadResult::Request {
                    in_header,
                    header_buffer: header_buf,
                    data_buffer: data_buf,
                    uring_payload: payload,
                } => {
                    header_buffer = header_buf;
                    data_buffer = data_buf;
                    uring_payload = payload;

                    match in_header {
                        Err(_) => continue,

                        Ok(in_header) => in_header,
                    }
                }
            };

            let request = Request::from(&in_header);

            let opcode = match fuse_opcode::try_from(in_header.opcode) {
                Err(err) => {
                    debug!("receive unknown opcode {}", err.0);

                    reply_error_in_place(libc::ENOSYS.into(), request, &self.response_sender).await;

                    continue;
                }

                Ok(opcode) => opcode,
            };

            debug!("receive opcode {}", opcode);

            let data_size = in_header.len as usize - FUSE_IN_HEADER_SIZE;
            let data_ref = &data_buffer[..data_size];

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
                            .write_vectored::<_, Vec<u8>>(hdr, None)
                            .await
                            .1;
                        if let Some(pool) = fuse_connection.over_uring.lock().unwrap().take() {
                            pool.shutdown();
                        }
                    }
                    #[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
                    {
                        reply_none_in_place(request, &self.response_sender).await;
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

    #[instrument(skip(self, data, fs))]
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
                    .write_vectored::<_, Vec<u8>>(init_out_header_data, None)
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

        let reply_flags = negotiate_reply_flags(init_in.flags, &self.mount_options);

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
                    .write_vectored::<_, Vec<u8>>(init_out_header_data, None)
                    .await
                    .1
                {
                    error!("write error init out data to /dev/fuse failed {}", err);
                }

                return Err(err.into());
            }

            Ok(reply) => reply,
        };

        // Required: always advertise FUSE_OVER_IO_URING. No opt-out.
        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        let flags2 = {
            debug!("advertising FUSE_OVER_IO_URING in init flags2");
            crate::raw::connection::fuse_over_uring::FUSE_OVER_IO_URING_FLAGS2
        };
        // Non-Linux / non-tokio builds cannot use over-uring (SqueezeFS is Linux-only).
        #[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
        let flags2 = 0u32;

        // L1 (IOPS-parity program): resolve the session transport geometry
        // BEFORE serializing the INIT reply, so the `max_background` /
        // `congestion_threshold` the kernel learns here always describe the
        // FUSE-over-io_uring rings that step 2 below will register. The
        // classical DEFAULT_MAX_BACKGROUND=12 was one of the two
        // multiplicative in-flight gates (with per-queue depth 4) that held
        // rand-4k iodepth workloads to ~18 effective of 256 offered.
        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        let transport_geom = crate::raw::connection::fuse_over_uring::TransportGeometry::resolve(
            reply.max_write.get() as usize,
            self.mount_options.transport_buffer_cap_bytes,
            self.mount_options.max_background,
            self.mount_options.congestion_threshold,
        );
        #[cfg(all(target_os = "linux", feature = "tokio-runtime"))]
        let (max_background, congestion_threshold) = (
            transport_geom.max_background,
            transport_geom.congestion_threshold,
        );
        // Non-over-uring builds keep the classical libfuse-era defaults
        // (max_background 12, congestion ¾ of it) — the L1 policy is an
        // over-uring geometry statement and does not apply without rings.
        #[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
        let (max_background, congestion_threshold) = (12u16, 9u16);

        let init_out = fuse_init_out {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: init_in.max_readahead,
            flags: reply_flags,
            max_background,
            congestion_threshold,
            max_write: reply.max_write.get(),
            time_gran: DEFAULT_TIME_GRAN,
            max_pages: DEFAULT_MAX_PAGES,
            map_alignment: DEFAULT_MAP_ALIGNMENT,
            flags2,
            max_stack_depth: 0,
            request_timeout: 0,
            unused: [0; 11],
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
            .write_vectored::<_, Vec<u8>>(data, None)
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
            if let Some(pool) = fuse_connection.over_uring.lock().unwrap().clone() {
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

        Ok(reply.max_write)
    }

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_lookup"), async move {
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
    #[instrument(skip(self, data, fs))]
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

        spawn(debug_span!("fuse_forget"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(getattr_in) => getattr_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_getattr"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(setattr_in) => setattr_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_setattr"), async move {
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

    #[instrument(skip(self, fs))]
    async fn handle_readlink(&mut self, request: Request, in_header: fuse_in_header, fs: &Arc<FS>) {
        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_readlink"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_symlink"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_mknod"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_mkdir"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_unlink"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_rmdir"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_rename"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_link"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(open_in) => open_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_open"), async move {
            debug!(
                "open unique {} inode {} flags {}",
                request.unique, in_header.nodeid, open_in.flags
            );

            let opened = match fs.open(request, in_header.nodeid, open_in.flags).await {
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

    #[instrument(skip(self, data, fs))]
    async fn handle_read(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let read_in = match get_bincode_config().deserialize::<fuse_read_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_read_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(read_in) => read_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();
        // P2 per-op economy: on an armed over-uring session the READ reply
        // is completed in place from the handler task (a synchronous
        // COMMIT enqueue) instead of hopping through the unbounded reply
        // channel + the per-queue reply task — one task wake per op saved.
        let reply_conn = self.fuse_connection.clone();

        spawn(debug_span!("fuse_read"), async move {
            debug!(
                "read unique {} inode {} {:?}",
                request.unique, in_header.nodeid, read_in
            );

            let (mut reply_data, backing) = match fs
                .read(
                    request,
                    in_header.nodeid,
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

                Ok(reply_data) => (reply_data.data, reply_data.backing),
            };

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
                Some(conn) => {
                    if let Err(err) = conn.write_vectored(data_buf, Some(reply_data)).await.1 {
                        if err.kind() == ErrorKind::NotFound {
                            warn!(
                                "may reply interrupted fuse request, ignore this error {}",
                                err
                            );
                        } else {
                            error!("in-place read reply failed {}", err);
                        }
                    }
                    drop(backing);
                }
                None => {
                    let _ = resp_sender
                        .send(Either::Right((data_buf, reply_data, backing)))
                        .await;
                }
            }
        });
    }

    #[instrument(skip(self, data, fs))]
    async fn handle_write(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        uring_payload: Option<Bytes>,
        fs: &Arc<FS>,
    ) {
        let write_in = match get_bincode_config().deserialize::<fuse_write_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_write_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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
                    error!(
                        "fuse_write_in size {} != uring payload len {}",
                        write_in.size,
                        p.len()
                    );

                    reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                    return;
                }

                p
            }
            // Classical delivery (FUSE_INIT window handoff only after arm):
            // the body is in the session buffer, exactly as before.
            None => {
                if write_in.size as usize != data.len() {
                    error!("fuse_write_in body len is invalid");

                    reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                    return;
                }

                Bytes::copy_from_slice(data)
            }
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_write"), async move {
            debug!(
                "write unique {} inode {} {:?}",
                request.unique, in_header.nodeid, write_in
            );

            let reply_write = match fs
                .write(
                    request,
                    in_header.nodeid,
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

            let _ = resp_sender.send(Either::Left(data)).await;
        });
    }

    #[instrument(skip(self, fs))]
    async fn handle_statfs(&mut self, request: Request, in_header: fuse_in_header, fs: &Arc<FS>) {
        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_statfs"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(release_in) => release_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_release"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(fsync_in) => fsync_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_fsync"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => (OsString::from_vec(data[..index].to_vec()), index),
        };

        data = &data[first_null_index + 1..];

        // setxattr "size" field specifies size of only "Value" part of data
        if setxattr_in.size as usize != data.len() {
            error!(
                "fuse_setxattr_in value field data length is not right, request unique {} setxattr_in.size={} data.len={}", request.unique, setxattr_in.size, data.len());

            reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

            return;
        }

        let data = data.to_vec();

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_setxattr"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(getxattr_in) => getxattr_in,
        };

        data = &data[FUSE_GETXATTR_IN_SIZE..];

        let name = match get_first_null_position(data) {
            None => {
                error!("fuse_getxattr_in body has no null {}", request.unique);

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_getxattr"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(listxattr_in) => listxattr_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_listxattr"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_removexattr"), async move {
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
        });
    }

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(flush_in) => flush_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_flush"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(open_in) => open_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_opendir"), async move {
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

    #[instrument(skip(self, data, fs))]
    async fn handle_readdir(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        if self.mount_options.force_readdir_plus {
            reply_error_in_place(libc::ENOSYS.into(), request, &self.response_sender).await;

            return;
        }

        let read_in = match get_bincode_config().deserialize::<fuse_read_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_read_in in readdir failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(read_in) => read_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_readdir"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(release_in) => release_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_releasedir"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(fsync_in) => fsync_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_fsyncdir"), async move {
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
    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(getlk_in) => getlk_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_getlk"), async move {
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
    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(setlk_in) => setlk_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_setlk"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(access_in) => access_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_access"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_create"), async move {
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

    #[instrument(skip(self, data, fs))]
    async fn handle_interrupt(&mut self, request: Request, data: &[u8], fs: &Arc<FS>) {
        let interrupt_in = match get_bincode_config().deserialize::<fuse_interrupt_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_interrupt_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(interrupt_in) => interrupt_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_interrupt"), async move {
            debug!(
                "interrupt_in unique {} interrupt unique {}",
                request.unique, interrupt_in.unique
            );

            let resp_value = if let Err(err) = fs.interrupt(request, interrupt_in.unique).await {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(bmap_in) => bmap_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_bmap"), async move {
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

    #[instrument(skip(self, data, fs))]
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
                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;
                return;
            }
            Ok(ioctl_in) => ioctl_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_ioctl"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(poll_in) => poll_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        let notify = self.get_notify();

        spawn(debug_span!("fuse_poll"), async move {
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

    #[instrument(skip(self, data, fs))]
    async fn handle_notify_reply(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        mut data: &[u8],
        fs: &Arc<FS>,
    ) {
        let resp_sender = self.response_sender.clone();

        let notify_retrieve_in =
            match get_bincode_config().deserialize::<fuse_notify_retrieve_in>(data) {
                Err(err) => {
                    error!(
                        "deserialize fuse_notify_retrieve_in failed {}, request unique {}",
                        err, request.unique
                    );

                    // TODO need to reply or not?
                    return;
                }

                Ok(notify_retrieve_in) => notify_retrieve_in,
            };

        data = &data[FUSE_NOTIFY_RETRIEVE_IN_SIZE..];

        if data.len() < notify_retrieve_in.size as usize {
            error!(
                "fuse_notify_retrieve unique {} data size is not right",
                request.unique
            );

            // TODO need to reply or not?
            return;
        }

        let data = data[..notify_retrieve_in.size as usize].to_vec();

        let fs = fs.clone();

        spawn(debug_span!("fuse_notify_reply"), async move {
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
        });
    }

    #[instrument(skip(self, data, fs))]
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

        data = &data[FUSE_BATCH_FORGET_IN_SIZE..];

        // TODO if has less data, should I return error?
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

        spawn(debug_span!("fuse_batch_forget"), async move {
            let inodes = forgets
                .into_iter()
                .map(|forget_one| forget_one.nodeid)
                .collect::<Vec<_>>();

            debug!("batch_forget unique {} inodes {:?}", request.unique, inodes);

            // Over-uring: ring entry already COMMITed in the queue worker (noreply).
            fs.batch_forget(request, &inodes).await
        });
    }

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(fallocate_in) => fallocate_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_fallocate"), async move {
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

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(readdirplus_in) => readdirplus_in,
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_readdirplus"), async move {
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
        });
    }

    #[instrument(skip(self, data, fs))]
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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

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

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Some(index) => OsString::from_vec(data[..index].to_vec()),
        };

        let mut resp_sender = self.response_sender.clone();
        let fs = fs.clone();

        spawn(debug_span!("fuse_rename2"), async move {
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

    #[instrument(skip(self, data, fs))]
    async fn handle_lseek(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let mut resp_sender = self.response_sender.clone();

        let lseek_in = match get_bincode_config().deserialize::<fuse_lseek_in>(data) {
            Err(err) => {
                error!(
                    "deserialize fuse_lseek_in failed {}, request unique {}",
                    err, request.unique
                );

                reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                return;
            }

            Ok(lseek_in) => lseek_in,
        };

        let fs = fs.clone();

        spawn(debug_span!("fuse_lseek"), async move {
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

    #[instrument(skip(self, data, fs))]
    async fn handle_copy_file_range(
        &mut self,
        request: Request,
        in_header: fuse_in_header,
        data: &[u8],
        fs: &Arc<FS>,
    ) {
        let mut resp_sender = self.response_sender.clone();

        let copy_file_range_in =
            match get_bincode_config().deserialize::<fuse_copy_file_range_in>(data) {
                Err(err) => {
                    error!(
                        "deserialize fuse_copy_file_range_in failed {}, request unique {}",
                        err, request.unique
                    );

                    reply_error_in_place(libc::EINVAL.into(), request, &self.response_sender).await;

                    return;
                }

                Ok(copy_file_range_in) => copy_file_range_in,
            };

        let fs = fs.clone();

        spawn(debug_span!("fuse_copy_file_range"), async move {
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
        });
    }
}

async fn reply_error_in_place<S>(err: Errno, request: Request, sender: S)
where
    S: Sink<FuseData>,
{
    let out_header = fuse_out_header {
        len: FUSE_OUT_HEADER_SIZE as u32,
        error: err.into(),
        unique: request.unique,
    };

    let data = get_bincode_config()
        .serialize(&out_header)
        .expect("won't happened");

    let _ = pin!(sender).send(Either::Left(data)).await;
}

/// Classical-only helper for no-reply opcodes (non-uring builds).
/// Over-uring COMMITs FORGET/BATCH_FORGET in the queue worker and DESTROY inline.
#[cfg(not(all(target_os = "linux", feature = "tokio-runtime")))]
async fn reply_none_in_place<S>(request: Request, sender: S)
where
    S: Sink<FuseData>,
{
    let out_header = fuse_out_header {
        len: FUSE_OUT_HEADER_SIZE as u32,
        error: 0,
        unique: request.unique,
    };

    let data = get_bincode_config()
        .serialize(&out_header)
        .expect("won't happened");

    let _ = pin!(sender).send(Either::Left(data)).await;
}

struct TpcScheduler {
    senders: Vec<
        tokio::sync::mpsc::UnboundedSender<
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        >,
    >,
    next_idx: std::sync::atomic::AtomicUsize,
}

impl TpcScheduler {
    /// CPU ids of the PROCESS affinity mask (the main thread's — tid == pid
    /// — which is never core-pinned), NOT the calling thread's.
    ///
    /// `TPC_SCHEDULER` is a `Lazy` first touched from a FUSE dispatch task,
    /// which the embedding daemon runs on a runtime worker pinned to ONE
    /// core. `core_affinity::get_core_ids()` consults the calling thread's
    /// mask, so sizing from it collapsed the whole handler pool to a single
    /// LocalSet thread — every handler future serialized onto it, and one
    /// synchronously parked handler wedged every FUSE request on the mount
    /// (the SqueezeFS Hang-1 fsx `copy_file_range` wedge).
    fn process_core_ids() -> Vec<core_affinity::CoreId> {
        // SAFETY: zeroed cpu_set_t is a valid empty set; sched_getaffinity
        // writes at most size_of::<cpu_set_t>() bytes into it.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(
                std::process::id() as libc::pid_t,
                std::mem::size_of::<libc::cpu_set_t>(),
                &mut set,
            ) == 0
            {
                let ids: Vec<core_affinity::CoreId> = (0..libc::CPU_SETSIZE as usize)
                    .filter(|&i| libc::CPU_ISSET(i, &set))
                    .map(|id| core_affinity::CoreId { id })
                    .collect();
                if !ids.is_empty() {
                    return ids;
                }
            }
        }
        core_affinity::get_core_ids().unwrap_or_default()
    }

    fn new() -> Self {
        let mut core_ids = Self::process_core_ids();
        if core_ids.len() > 1 {
            core_ids.remove(0); // Reserve Core 0 for OS kernel tasks
        }

        let mut senders = Vec::new();
        let core_count = if core_ids.is_empty() {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(16)
        } else {
            core_ids.len()
        };

        for i in 0..core_count {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<
                std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
            >();
            senders.push(tx);

            let core_id = if !core_ids.is_empty() {
                Some(core_ids[i % core_ids.len()])
            } else {
                None
            };

            std::thread::spawn(move || {
                if let Some(cid) = core_id {
                    core_affinity::set_for_current(cid);
                }

                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    while let Some(fut) = rx.recv().await {
                        tokio::task::spawn_local(fut);
                    }
                });
            });
        }

        Self {
            senders,
            next_idx: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if self.senders.is_empty() {
            tokio::task::spawn(fut);
            return;
        }
        let idx = self
            .next_idx
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.senders.len();
        let _ = self.senders[idx].send(Box::pin(fut));
    }
}

static TPC_SCHEDULER: once_cell::sync::Lazy<TpcScheduler> =
    once_cell::sync::Lazy::new(TpcScheduler::new);

#[inline]
fn spawn<F>(span: Span, fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(all(not(feature = "async-io-runtime"), feature = "tokio-runtime"))]
    {
        TPC_SCHEDULER.spawn(async move {
            let _ = fut.instrument(span).await;
        });
    }

    #[cfg(all(not(feature = "tokio-runtime"), feature = "async-io-runtime"))]
    task::spawn(fut.instrument(span)).detach()
}

pub fn tpc_spawn<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    TPC_SCHEDULER.spawn(fut);
}

pub fn tpc_thread_count() -> usize {
    TPC_SCHEDULER.senders.len()
}

/// INIT reply-flags negotiation: the subset of the kernel's offered
/// `init_in.flags` capabilities this daemon actually implements (mount
/// options gate the optional ones). Pure — pinned by
/// `init_negotiation_tests`: a capability the daemon does not implement
/// must never be advertised back to the kernel.
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
