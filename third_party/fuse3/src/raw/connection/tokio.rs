#[cfg(target_os = "linux")]
use std::fs::File;
use std::fs::OpenOptions;
use std::io;

#[cfg(target_os = "linux")]
use io_uring::{opcode, types, IoUring};



#[cfg(target_os = "linux")]
#[derive(Copy, Clone)]
#[repr(transparent)]
struct SendIovec(libc::iovec);

#[cfg(target_os = "linux")]
unsafe impl Send for SendIovec {}

#[cfg(target_os = "linux")]
unsafe impl Sync for SendIovec {}

#[cfg(target_os = "linux")]
struct DebugUring(IoUring);

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
#[cfg(target_os = "linux")]
use std::io::Write;
use std::io::{IoSlice, IoSliceMut};
use std::ops::{Deref, DerefMut};
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd"
))]
use std::os::fd::OwnedFd;
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(target_os = "freebsd")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::os::unix::io::RawFd;
use std::pin::pin;
use std::sync::Arc;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::{ffi::OsString, path::Path};

use async_notify::Notify;
use futures_util::lock::Mutex;
use futures_util::{select, FutureExt};
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd"
))]
#[cfg(target_os = "freebsd")]
use nix::sys::uio;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use nix::{
    fcntl::{FcntlArg, OFlag},
    sys::socket::{self, AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType},
};
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
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

#[derive(Debug)]
pub struct FuseConnection {
    unmount_notify: Arc<Notify>,
    mode: ConnectionMode,
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
            })
        }
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
        })
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
        header_buf: Vec<u8>,
        data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        match &self.mode {
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
        }
    }

    pub async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
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
    read: Mutex<()>,
    write: Mutex<()>,
}

#[cfg(target_os = "linux")]
impl BlockFuseConnection {
    pub fn new() -> io::Result<Self> {
        const DEV_FUSE: &str = "/dev/fuse";

        let file = OpenOptions::new().write(true).read(true).open(DEV_FUSE)?;

        Ok(Self {
            file,
            read: Mutex::new(()),
            write: Mutex::new(()),
        })
    }

    async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        use std::io::Read;
        use std::mem::ManuallyDrop;
        use std::os::fd::{AsRawFd, FromRawFd};

        let _guard = self.read.lock().await;
        let fd = self.file.as_raw_fd();

        let ((header_buf, data_buf), res) = task::spawn_blocking(move || {
            // Safety: when we call read, the fd is still valid, when fd is closed and file is
            // dropped, the read operation will return error
            let file = unsafe { File::from_raw_fd(fd) };
            // avoid close the file
            let mut file = ManuallyDrop::new(file);

            let res = file.read_vectored(&mut [
                IoSliceMut::new(&mut header_buf),
                IoSliceMut::new(&mut data_buf),
            ]);

            ((header_buf, data_buf), res)
        })
        .await
        .unwrap();

        ((header_buf, data_buf), res)
    }

    async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        let _guard = self.write.lock().await;

        let res = {
            let body_extend_data = body_extend_data.as_deref();

            match body_extend_data {
                None => (&self.file).write_vectored(&[IoSlice::new(data.deref())]),

                Some(body_extend_data) => (&self.file)
                    .write_vectored(&[IoSlice::new(data.deref()), IoSlice::new(body_extend_data)]),
            }
        };

        match res {
            Err(err) => ((data, body_extend_data), Err(err)),
            Ok(n) => ((data, body_extend_data), Ok(n)),
        }
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
            let read_ring = IoUring::new(256)?;
            let write_ring = IoUring::new(256)?;

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

        loop {
            let cqe = {
                let mut guard = self.read_ring.lock().unwrap();
                let x = guard.0.completion().next();
                x
            };
            if let Some(cqe) = cqe {
                if cqe.user_data() == 0x01 {
                    let res = cqe.result();
                    let io_res = if res < 0 {
                        Err(io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };
                    return ((header_buf, data_buf), io_res);
                }
            }

            let mut fd_guard = match self.read_ring_fd.ready(Interest::READABLE).await {
                Err(err) => return ((header_buf, data_buf), Err(err)),
                Ok(guard) => guard,
            };

            let mut buf = [0u8; 8];
            let _ = unsafe {
                libc::read(
                    self.read_ring_fd.get_ref().as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    8,
                )
            };

            fd_guard.clear_ready();
        }
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

        loop {
            let cqe = {
                let mut guard = self.write_ring.lock().unwrap();
                let x = guard.0.completion().next();
                x
            };
            if let Some(cqe) = cqe {
                if cqe.user_data() == 0x02 {
                    let res = cqe.result();
                    let io_res = if res < 0 {
                        Err(io::Error::from_raw_os_error(-res))
                    } else {
                        Ok(res as usize)
                    };
                    return ((data, body_extend_data), io_res);
                }
            }

            let mut fd_guard = match self.write_ring_fd.ready(Interest::READABLE).await {
                Err(err) => return ((data, body_extend_data), Err(err)),
                Ok(guard) => guard,
            };

            let mut buf = [0u8; 8];
            let _ = unsafe {
                libc::read(
                    self.write_ring_fd.get_ref().as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    8,
                )
            };

            fd_guard.clear_ready();
        }
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
