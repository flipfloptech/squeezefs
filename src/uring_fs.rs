//! Process-wide generic file I/O via a dedicated `io_uring` worker (P2-8).
//!
//! Use this for **path-based** reads/writes/fsync that are not the primary
//! block device (GDS cache files, ad-hoc local files). The primary block path
//! remains [`crate::nvme_dev::NvmeBlockDev`] (also io_uring).
//!
//! Staging/read-segment **mmap** paths intentionally stay on `mmap` — they are
//! already zero-syscall for the hot get/put path; flushing uses optional
//! `fdatasync` through this worker when requested.
//!
//! **Not** routed through uring (and should not be without a dedicated stack):
//! Garnet/Redis TCP, TLS/mTLS peer traffic, directory create/remove metadata.

use crate::error::{Result, SqueezefsError};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::oneshot;

const URING_FS_QUEUE_CAP: usize = 4096;

enum FsReq {
    WriteAll {
        path: PathBuf,
        data: bytes::Bytes,
        tx: oneshot::Sender<Result<()>>,
    },
    ReadAll {
        path: PathBuf,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    Fdatasync {
        path: PathBuf,
        tx: oneshot::Sender<Result<()>>,
    },
}

struct UringFsWorker {
    tx: Option<crossbeam::channel::Sender<FsReq>>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl UringFsWorker {
    fn new() -> Self {
        let (tx, rx) = crossbeam::channel::bounded(URING_FS_QUEUE_CAP);
        let thread = std::thread::Builder::new()
            .name("squeezefs-uring-fs".into())
            .spawn(move || worker_loop(rx))
            .expect("spawn uring-fs worker");
        Self {
            tx: Some(tx),
            _thread: Some(thread),
        }
    }

    fn sender(&self) -> &crossbeam::channel::Sender<FsReq> {
        self.tx.as_ref().expect("uring-fs worker sender")
    }
}

impl Drop for UringFsWorker {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(h) = self._thread.take() {
            let _ = h.join();
        }
    }
}

static URING_FS: Lazy<Arc<UringFsWorker>> = Lazy::new(|| Arc::new(UringFsWorker::new()));

/// Write `data` to `path` (create/truncate) via the process io_uring file worker.
pub async fn write_all(path: impl AsRef<Path>, data: impl Into<bytes::Bytes>) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::WriteAll {
            path: path.as_ref().to_path_buf(),
            data: data.into(),
            tx,
        })
        .map_err(|e| {
            crate::fuse_client::METRICS
                .uring_queue_full
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            SqueezefsError::InvalidOperation(format!("uring-fs queue full: {e:?}"))
        })?;
    rx.await
        .map_err(|e| SqueezefsError::InvalidOperation(format!("uring-fs worker closed: {e:?}")))?
}

/// Read entire file via the process io_uring file worker.
pub async fn read_all(path: impl AsRef<Path>) -> Result<bytes::Bytes> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::ReadAll {
            path: path.as_ref().to_path_buf(),
            tx,
        })
        .map_err(|e| {
            crate::fuse_client::METRICS
                .uring_queue_full
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            SqueezefsError::InvalidOperation(format!("uring-fs queue full: {e:?}"))
        })?;
    rx.await
        .map_err(|e| SqueezefsError::InvalidOperation(format!("uring-fs worker closed: {e:?}")))?
}

/// `fdatasync` an existing path via io_uring.
pub async fn fdatasync(path: impl AsRef<Path>) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::Fdatasync {
            path: path.as_ref().to_path_buf(),
            tx,
        })
        .map_err(|e| {
            crate::fuse_client::METRICS
                .uring_queue_full
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            SqueezefsError::InvalidOperation(format!("uring-fs queue full: {e:?}"))
        })?;
    rx.await
        .map_err(|e| SqueezefsError::InvalidOperation(format!("uring-fs worker closed: {e:?}")))?
}

fn map_io(e: std::io::Error) -> SqueezefsError {
    SqueezefsError::Io(e)
}

fn worker_loop(rx: crossbeam::channel::Receiver<FsReq>) {
    use io_uring::{opcode, types, IoUring};
    use std::os::unix::fs::OpenOptionsExt;

    let mut ring = match IoUring::new(512) {
        Ok(r) => r,
        Err(e) => {
            log::error!("uring-fs: failed to create IoUring: {e:?}");
            // Drain with blocking fallback so callers still complete.
            while let Ok(req) = rx.recv() {
                match req {
                    FsReq::WriteAll { path, data, tx } => {
                        let _ = tx.send(std::fs::write(&path, &data).map_err(map_io).map(|_| ()));
                    }
                    FsReq::ReadAll { path, tx } => {
                        let _ =
                            tx.send(std::fs::read(&path).map(bytes::Bytes::from).map_err(map_io));
                    }
                    FsReq::Fdatasync { path, tx } => {
                        let res = OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .and_then(|f| f.sync_data())
                            .map_err(map_io);
                        let _ = tx.send(res);
                    }
                }
            }
            return;
        }
    };

    // Keep open files warm for fdatasync of staging segments.
    let mut open_cache: HashMap<PathBuf, File> = HashMap::new();

    while let Ok(req) = rx.recv() {
        match req {
            FsReq::WriteAll { path, data, tx } => {
                let res = (|| -> Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(map_io)?;
                    }
                    let file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .custom_flags(libc::O_CLOEXEC)
                        .open(&path)
                        .map_err(map_io)?;
                    let fd = file.as_raw_fd();
                    // Single write of full buffer (files are typically small-medium cache blobs).
                    let mut offset = 0u64;
                    let mut remaining = data.as_ref();
                    while !remaining.is_empty() {
                        let chunk = remaining;
                        let write_e =
                            opcode::Write::new(types::Fd(fd), chunk.as_ptr(), chunk.len() as u32)
                                .offset(offset)
                                .build()
                                .user_data(1);

                        unsafe {
                            ring.submission().push(&write_e).map_err(|e| {
                                SqueezefsError::Io(std::io::Error::other(format!(
                                    "uring push: {e:?}"
                                )))
                            })?;
                        }
                        ring.submit_and_wait(1).map_err(map_io)?;
                        let mut cq = ring.completion();
                        cq.sync();
                        let cqe = cq.next().ok_or_else(|| {
                            SqueezefsError::Io(std::io::Error::other("uring missing cqe"))
                        })?;
                        let n = cqe.result();
                        if n < 0 {
                            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(-n)));
                        }
                        let n = n as usize;
                        if n == 0 {
                            return Err(SqueezefsError::Io(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "uring write returned 0",
                            )));
                        }
                        offset += n as u64;
                        remaining = &remaining[n..];
                    }
                    // Keep handle for possible later fdatasync.
                    open_cache.insert(path, file);
                    Ok(())
                })();
                let _ = tx.send(res);
            }
            FsReq::ReadAll { path, tx } => {
                let res = (|| -> Result<bytes::Bytes> {
                    let file = OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_CLOEXEC)
                        .open(&path)
                        .map_err(map_io)?;
                    let meta = file.metadata().map_err(map_io)?;
                    let len = meta.len() as usize;
                    let mut buf = vec![0u8; len];
                    let fd = file.as_raw_fd();
                    let mut offset = 0u64;
                    let mut filled = 0usize;
                    while filled < len {
                        let slice = &mut buf[filled..];
                        let read_e = opcode::Read::new(
                            types::Fd(fd),
                            slice.as_mut_ptr(),
                            slice.len() as u32,
                        )
                        .offset(offset)
                        .build()
                        .user_data(2);
                        unsafe {
                            ring.submission().push(&read_e).map_err(|e| {
                                SqueezefsError::Io(std::io::Error::other(format!(
                                    "uring push: {e:?}"
                                )))
                            })?;
                        }
                        ring.submit_and_wait(1).map_err(map_io)?;
                        let mut cq = ring.completion();
                        cq.sync();
                        let cqe = cq.next().ok_or_else(|| {
                            SqueezefsError::Io(std::io::Error::other("uring missing cqe"))
                        })?;
                        let n = cqe.result();
                        if n < 0 {
                            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(-n)));
                        }
                        let n = n as usize;
                        if n == 0 {
                            break;
                        }
                        offset += n as u64;
                        filled += n;
                    }
                    buf.truncate(filled);
                    Ok(bytes::Bytes::from(buf))
                })();
                let _ = tx.send(res);
            }
            FsReq::Fdatasync { path, tx } => {
                let res = (|| -> Result<()> {
                    let file = if let Some(f) = open_cache.get(&path) {
                        f
                    } else {
                        let f = OpenOptions::new()
                            .write(true)
                            .custom_flags(libc::O_CLOEXEC)
                            .open(&path)
                            .map_err(map_io)?;
                        open_cache.insert(path.clone(), f);
                        open_cache.get(&path).unwrap()
                    };
                    let fd = file.as_raw_fd();
                    let sync_e = opcode::Fsync::new(types::Fd(fd))
                        .flags(types::FsyncFlags::DATASYNC)
                        .build()
                        .user_data(3);
                    unsafe {
                        ring.submission().push(&sync_e).map_err(|e| {
                            SqueezefsError::Io(std::io::Error::other(format!("uring push: {e:?}")))
                        })?;
                    }
                    ring.submit_and_wait(1).map_err(map_io)?;
                    let mut cq = ring.completion();
                    cq.sync();
                    let cqe = cq.next().ok_or_else(|| {
                        SqueezefsError::Io(std::io::Error::other("uring missing cqe"))
                    })?;
                    let n = cqe.result();
                    if n < 0 {
                        return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(-n)));
                    }
                    Ok(())
                })();
                let _ = tx.send(res);
            }
        }
    }
}

/// Expose for tests: queue capacity constant.
pub const QUEUE_CAP: usize = URING_FS_QUEUE_CAP;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_uring_fs_write_read_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        let payload = bytes::Bytes::from(vec![0xABu8; 12_345]);
        write_all(&path, payload.clone()).await.expect("write");
        let got = read_all(&path).await.expect("read");
        assert_eq!(got, payload);
        fdatasync(&path).await.expect("fdatasync");
    }

    #[tokio::test]
    async fn test_uring_fs_write_creates_parents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.dat");
        write_all(&path, b"hi".as_slice())
            .await
            .expect("nested write");
        assert_eq!(std::fs::read(&path).unwrap(), b"hi");
    }
}
