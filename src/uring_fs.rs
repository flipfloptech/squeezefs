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
    ReadAt {
        path: PathBuf,
        offset: u64,
        size: usize,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    WriteAt {
        path: PathBuf,
        offset: u64,
        data: bytes::Bytes,
        tx: oneshot::Sender<Result<()>>,
    },
    Fdatasync {
        path: PathBuf,
        tx: oneshot::Sender<Result<()>>,
    },
}

struct UringFsWorker {
    tx: Option<crossbeam::channel::Sender<FsReq>>,
    _threads: Vec<std::thread::JoinHandle<()>>,
}

impl UringFsWorker {
    fn new() -> Self {
        let (tx, rx) = crossbeam::channel::bounded(URING_FS_QUEUE_CAP);
        // Pool of workers sharing one MPMC queue. A single worker serialized ALL
        // path I/O (journal/inode/xattr writes + every barrier) and each
        // `fdatasync` blocked it for a full device flush (~1 ms on NVMe), so meta
        // writes ran one-at-a-time and fsync coalescing could never engage (only
        // one barrier was ever in flight). With N workers a barrier-blocked worker
        // no longer stalls the others; a free worker pulls the next request.
        let count = worker_count();
        let mut threads = Vec::with_capacity(count);
        for i in 0..count {
            let rx = rx.clone();
            let t = std::thread::Builder::new()
                .name(format!("squeezefs-uring-fs-{i}"))
                .spawn(move || worker_loop(rx))
                .expect("spawn uring-fs worker");
            threads.push(t);
        }
        Self {
            tx: Some(tx),
            _threads: threads,
        }
    }

    fn sender(&self) -> &crossbeam::channel::Sender<FsReq> {
        self.tx.as_ref().expect("uring-fs worker sender")
    }
}

/// Size of the io_uring file-worker pool. Each worker owns its own ring and pulls
/// from the shared MPMC queue, so a worker blocked in `fdatasync` never stalls the
/// rest. Override with `SQUEEZEFS_URING_FS_WORKERS`; defaults to `clamp(nproc, 4, 8)`.
fn worker_count() -> usize {
    if let Ok(v) = std::env::var("SQUEEZEFS_URING_FS_WORKERS") {
        if let Ok(n) = v.parse::<usize>() {
            return n.clamp(1, 64);
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(4, 8)
}

impl Drop for UringFsWorker {
    fn drop(&mut self) {
        // Close the channel so every worker sees a recv error and exits.
        drop(self.tx.take());
        for h in self._threads.drain(..) {
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

/// Read `size` bytes at `offset` from `path` via io_uring.
pub async fn read_at(path: impl AsRef<Path>, offset: u64, size: usize) -> Result<bytes::Bytes> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::ReadAt {
            path: path.as_ref().to_path_buf(),
            offset,
            size,
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

/// Write `data` at `offset` to `path` via io_uring.
pub async fn write_at(
    path: impl AsRef<Path>,
    offset: u64,
    data: impl Into<bytes::Bytes>,
) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::WriteAt {
            path: path.as_ref().to_path_buf(),
            offset,
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
                    FsReq::ReadAt {
                        path,
                        offset,
                        size,
                        tx,
                    } => {
                        let res = OpenOptions::new()
                            .read(true)
                            .open(&path)
                            .and_then(|f| {
                                use std::os::unix::fs::FileExt;
                                let mut buf = vec![0u8; size];
                                f.read_exact_at(&mut buf, offset)?;
                                Ok(bytes::Bytes::from(buf))
                            })
                            .map_err(map_io);
                        let _ = tx.send(res);
                    }
                    FsReq::WriteAt {
                        path,
                        offset,
                        data,
                        tx,
                    } => {
                        let res = OpenOptions::new()
                            .write(true)
                            .create(true)
                            .open(&path)
                            .and_then(|f| {
                                use std::os::unix::fs::FileExt;
                                f.write_all_at(&data, offset)?;
                                f.sync_all()?;
                                Ok(())
                            })
                            .map_err(map_io);
                        let _ = tx.send(res);
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
    let mut lru_keys: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();

    while let Ok(req) = rx.recv() {
        match req {
            FsReq::WriteAll { path, data, tx } => {
                let res = (|| -> Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(map_io)?;
                    }
                    // O_RDWR (not O_WRONLY): this fd is cached in `open_cache` and
                    // may later be reused by a `ReadAt` on the same path. A write-only
                    // cached fd makes io_uring Read return EBADF (fd not open for read).
                    let file = OpenOptions::new()
                        .read(true)
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
                    if open_cache.len() >= 1024 {
                        if let Some(oldest) = lru_keys.pop_front() {
                            open_cache.remove(&oldest);
                        }
                    }
                    if let Some(pos) = lru_keys.iter().position(|p| p == &path) {
                        lru_keys.remove(pos);
                    }
                    open_cache.insert(path.clone(), file);
                    lru_keys.push_back(path);
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
            FsReq::ReadAt {
                path,
                offset,
                size,
                tx,
            } => {
                let res = (|| -> Result<bytes::Bytes> {
                    let file = if let Some(f) = open_cache.get(&path) {
                        if let Some(pos) = lru_keys.iter().position(|p| p == &path) {
                            lru_keys.remove(pos);
                        }
                        lru_keys.push_back(path.clone());
                        f
                    } else {
                        let f = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .custom_flags(libc::O_CLOEXEC)
                            .open(&path)
                            .map_err(map_io)?;
                        if open_cache.len() >= 1024 {
                            if let Some(oldest) = lru_keys.pop_front() {
                                open_cache.remove(&oldest);
                            }
                        }
                        open_cache.insert(path.clone(), f);
                        lru_keys.push_back(path.clone());
                        open_cache.get(&path).unwrap()
                    };
                    let fd = file.as_raw_fd();
                    let mut buf = vec![0u8; size];
                    let mut cur_offset = offset;
                    let mut filled = 0usize;
                    while filled < size {
                        let slice = &mut buf[filled..];
                        let read_e = opcode::Read::new(
                            types::Fd(fd),
                            slice.as_mut_ptr(),
                            slice.len() as u32,
                        )
                        .offset(cur_offset)
                        .build()
                        .user_data(4);
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
                        cur_offset += n as u64;
                        filled += n;
                    }
                    buf.truncate(filled);
                    Ok(bytes::Bytes::from(buf))
                })();
                let _ = tx.send(res);
            }
            FsReq::WriteAt {
                path,
                offset,
                data,
                tx,
            } => {
                let res = (|| -> Result<()> {
                    let file = if let Some(f) = open_cache.get(&path) {
                        if let Some(pos) = lru_keys.iter().position(|p| p == &path) {
                            lru_keys.remove(pos);
                        }
                        lru_keys.push_back(path.clone());
                        f
                    } else {
                        let f = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .custom_flags(libc::O_CLOEXEC)
                            .open(&path)
                            .map_err(map_io)?;
                        if open_cache.len() >= 1024 {
                            if let Some(oldest) = lru_keys.pop_front() {
                                open_cache.remove(&oldest);
                            }
                        }
                        open_cache.insert(path.clone(), f);
                        lru_keys.push_back(path.clone());
                        open_cache.get(&path).unwrap()
                    };
                    let fd = file.as_raw_fd();
                    let mut cur_offset = offset;
                    let mut remaining = data.as_ref();
                    while !remaining.is_empty() {
                        let write_e = opcode::Write::new(
                            types::Fd(fd),
                            remaining.as_ptr(),
                            remaining.len() as u32,
                        )
                        .offset(cur_offset)
                        .build()
                        .user_data(5);
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
                        cur_offset += n as u64;
                        remaining = &remaining[n..];
                    }
                    Ok(())
                })();
                let _ = tx.send(res);
            }
            FsReq::Fdatasync { path, tx } => {
                let res = (|| -> Result<()> {
                    let file = if let Some(f) = open_cache.get(&path) {
                        if let Some(pos) = lru_keys.iter().position(|p| p == &path) {
                            lru_keys.remove(pos);
                        }
                        lru_keys.push_back(path.clone());
                        f
                    } else {
                        // O_RDWR (not O_WRONLY): this fd is cached in `open_cache`
                        // and may be reused by a later `ReadAt` on the same path.
                        // A write-only cached fd makes io_uring Read return EBADF.
                        let f = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .custom_flags(libc::O_CLOEXEC)
                            .open(&path)
                            .map_err(map_io)?;
                        if open_cache.len() >= 1024 {
                            if let Some(oldest) = lru_keys.pop_front() {
                                open_cache.remove(&oldest);
                            }
                        }
                        open_cache.insert(path.clone(), f);
                        lru_keys.push_back(path.clone());
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

    /// The live worker is a pool, not a single thread — otherwise a blocked
    /// `fdatasync` serializes all meta I/O (the small-write bottleneck).
    #[test]
    fn test_uring_fs_runs_a_worker_pool() {
        assert!(
            URING_FS._threads.len() >= 4,
            "uring-fs must run a worker pool (got {} threads)",
            URING_FS._threads.len()
        );
    }

    /// Open-mode regression: `fdatasync` then `read_at` on the same path must not
    /// return EBADF. `fdatasync` caches its fd; if opened write-only, the cached
    /// fd is unreadable and io_uring Read fails. Repeated across many paths so it
    /// exercises the pooled cache-hit path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_uring_fs_fdatasync_then_read_no_ebadf() {
        let dir = tempdir().unwrap();
        let mut handles = Vec::new();
        for i in 0..64u32 {
            let path = dir.path().join(format!("fsync_then_read_{i}.bin"));
            handles.push(tokio::spawn(async move {
                let payload = bytes::Bytes::from(vec![(i % 251) as u8; 4096]);
                write_at(&path, 0, payload.clone()).await.expect("write_at");
                fdatasync(&path).await.expect("fdatasync");
                let got = read_at(&path, 0, payload.len()).await.expect("read_at");
                assert_eq!(got, payload, "read after fdatasync mismatch for {i}");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    /// Many concurrent write_at + fdatasync + read_at ops must not corrupt each
    /// other when serviced by different pool workers (own ring / open_cache each).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_uring_fs_concurrent_ops_integrity() {
        let dir = tempdir().unwrap();
        let mut handles = Vec::new();
        for i in 0..64u32 {
            let path = dir.path().join(format!("concurrent_{i}.bin"));
            handles.push(tokio::spawn(async move {
                let len = 4096 + i as usize;
                let payload = bytes::Bytes::from(vec![(i % 251) as u8; len]);
                write_at(&path, 0, payload.clone()).await.expect("write_at");
                fdatasync(&path).await.expect("fdatasync");
                let got = read_at(&path, 0, len).await.expect("read_at");
                assert_eq!(got, payload, "data corrupted for file {i} under pool");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }
}
