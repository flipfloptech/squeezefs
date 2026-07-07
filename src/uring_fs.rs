//! Process-wide generic file I/O via a pool of pipelined `io_uring` workers (P2-8).
//!
//! Use this for **path-based** reads/writes/fsync that are not the primary
//! block device (MetaLV sector + WAL I/O, GDS cache files, ad-hoc local
//! files). The primary block path remains [`crate::nvme_dev::NvmeBlockDev`]
//! (also io_uring).
//!
//! # Pipelined workers
//!
//! Each pool worker owns one ring and keeps **many operations in flight**:
//! requests are admitted from the shared queue in bursts, submitted together,
//! and completions are reaped as they arrive, with short read/write
//! continuations resubmitted from the completion handler. The previous
//! model (`submit_and_wait(1)` per request — one op in flight per worker,
//! two thread handoffs per 4 KiB sector) capped process-wide metadata I/O
//! at pool size and burned ~27% of daemon cycles in queue churn under
//! delete storms.
//!
//! [`write_at_batch`] lets one logical commit (WAL record + sector images)
//! travel as a single queue message that fans out into parallel SQEs.
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
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::oneshot;

const URING_FS_QUEUE_CAP: usize = 4096;
/// Ring SQ depth per worker.
const RING_ENTRIES: u32 = 512;
/// Max operations a worker keeps in flight; continuations reuse their slot,
/// so SQ pressure is bounded by this plus one burst of resubmits.
const ADMIT_CAP: usize = 256;
/// Open-file cache entries per worker: bounded by the process fd limit so
/// the pool can never EMFILE the process by itself (the pool's aggregate
/// cache stays under a quarter of RLIMIT_NOFILE), capped at 1024, floored
/// at 16. Production metadata I/O touches only a handful of distinct paths;
/// churny workloads simply re-open.
fn fd_cache_cap() -> usize {
    static CAP: Lazy<usize> = Lazy::new(|| {
        let mut rl = libc::rlimit {
            rlim_cur: 1024,
            rlim_max: 1024,
        };
        // SAFETY: plain getrlimit into a stack struct.
        let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
            rl.rlim_cur as usize
        } else {
            1024
        };
        (soft / 4 / worker_count().max(1)).clamp(16, 1024)
    });
    *CAP
}

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
    /// One logical commit: every `(offset, bytes)` lands (unordered between
    /// entries) before the single completion fires.
    WriteAtBatch {
        path: PathBuf,
        ops: Vec<(u64, bytes::Bytes)>,
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
        // Pool of pipelined workers sharing one MPMC queue: a worker blocked
        // on a device flush never stalls the others, and each worker keeps up
        // to ADMIT_CAP operations in flight on its own ring.
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

/// Size of the io_uring file-worker pool. Override with
/// `SQUEEZEFS_URING_FS_WORKERS`; defaults to `clamp(nproc, 4, 8)`.
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

fn queue_full_err<E: std::fmt::Debug>(e: E) -> SqueezefsError {
    crate::fuse_client::METRICS
        .uring_queue_full
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    SqueezefsError::InvalidOperation(format!("uring-fs queue full: {e:?}"))
}

fn worker_closed_err<E: std::fmt::Debug>(e: E) -> SqueezefsError {
    SqueezefsError::InvalidOperation(format!("uring-fs worker closed: {e:?}"))
}

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
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
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
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
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
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
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
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
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
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
}

/// Write every `(offset, bytes)` pair to `path` as **one** worker message:
/// the entries fan out into parallel SQEs on one ring and the call completes
/// when all have landed (any failure fails the whole batch, loudly). Entry
/// order is not an ordering guarantee — callers needing order between
/// batches issue separate calls.
pub async fn write_at_batch(path: impl AsRef<Path>, ops: Vec<(u64, bytes::Bytes)>) -> Result<()> {
    if ops.is_empty() {
        return Ok(());
    }
    let (tx, rx) = oneshot::channel();
    URING_FS
        .sender()
        .try_send(FsReq::WriteAtBatch {
            path: path.as_ref().to_path_buf(),
            ops,
            tx,
        })
        .map_err(queue_full_err)?;
    rx.await.map_err(worker_closed_err)?
}

fn map_io(e: std::io::Error) -> SqueezefsError {
    SqueezefsError::Io(e)
}

/// Completion sink: single ops answer their own oneshot; batch entries share
/// an aggregate that fires once when the last entry lands.
enum UnitDone {
    Single(oneshot::Sender<Result<()>>),
    Batch(Rc<std::cell::RefCell<BatchState>>),
}

struct BatchState {
    remaining: usize,
    first_err: Option<SqueezefsError>,
    tx: Option<oneshot::Sender<Result<()>>>,
}

impl UnitDone {
    fn complete(self, res: Result<()>) {
        match self {
            UnitDone::Single(tx) => {
                let _ = tx.send(res);
            }
            UnitDone::Batch(state) => {
                let mut st = state.borrow_mut();
                if let Err(e) = res {
                    if st.first_err.is_none() {
                        st.first_err = Some(e);
                    }
                }
                st.remaining -= 1;
                if st.remaining == 0 {
                    if let Some(tx) = st.tx.take() {
                        let _ = tx.send(match st.first_err.take() {
                            Some(e) => Err(e),
                            None => Ok(()),
                        });
                    }
                }
            }
        }
    }
}

/// One in-flight operation. Holds its own `Rc<File>` so fd-cache eviction can
/// never close a file with an outstanding SQE.
enum Pending {
    Read {
        file: Rc<File>,
        buf: Vec<u8>,
        file_offset: u64,
        filled: usize,
        want: usize,
        tx: oneshot::Sender<Result<bytes::Bytes>>,
    },
    Write {
        file: Rc<File>,
        data: bytes::Bytes,
        file_offset: u64,
        written: usize,
        done: UnitDone,
    },
    Fsync {
        file: Rc<File>,
        tx: oneshot::Sender<Result<()>>,
    },
}

/// Per-worker open-file cache: path → (shared fd, last-use generation).
struct FdCache {
    map: HashMap<PathBuf, (Rc<File>, u64)>,
    gen: u64,
}

impl FdCache {
    fn new() -> Self {
        Self {
            map: HashMap::with_capacity(fd_cache_cap()),
            gen: 0,
        }
    }

    fn touch(&mut self, path: &Path) -> Option<Rc<File>> {
        self.gen += 1;
        let gen = self.gen;
        self.map.get_mut(path).map(|(f, g)| {
            *g = gen;
            f.clone()
        })
    }

    fn insert(&mut self, path: PathBuf, file: File) -> Rc<File> {
        self.gen += 1;
        if self.map.len() >= fd_cache_cap() {
            // Evict the least-recently-used entry. In-flight ops hold their
            // own Rc clone, so eviction never closes a busy fd.
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, g))| *g)
                .map(|(p, _)| p.clone())
            {
                self.map.remove(&oldest);
            }
        }
        let rc = Rc::new(file);
        self.map.insert(path, (rc.clone(), self.gen));
        rc
    }
}

/// Open `path` for cached O_RDWR use (create per `create`), via the cache.
fn cached_open(cache: &mut FdCache, path: &Path, create: bool) -> Result<Rc<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(f) = cache.touch(path) {
        return Ok(f);
    }
    // O_RDWR (not O_WRONLY): this fd is cached and may later be reused by a
    // `ReadAt` on the same path. A write-only cached fd makes io_uring Read
    // return EBADF (fd not open for read).
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(map_io)?;
    Ok(cache.insert(path.to_path_buf(), f))
}

/// Reactor state for one worker thread.
struct Reactor {
    ring: io_uring::IoUring,
    slots: Vec<Option<Pending>>,
    free: Vec<usize>,
    inflight: usize,
    cache: FdCache,
}

impl Reactor {
    fn new(ring: io_uring::IoUring) -> Self {
        Self {
            ring,
            slots: Vec::new(),
            free: Vec::new(),
            inflight: 0,
            cache: FdCache::new(),
        }
    }

    fn claim_slot(&mut self, p: Pending) -> usize {
        self.inflight += 1;
        if let Some(i) = self.free.pop() {
            self.slots[i] = Some(p);
            i
        } else {
            self.slots.push(Some(p));
            self.slots.len() - 1
        }
    }

    fn release_slot(&mut self, i: usize) -> Pending {
        self.inflight -= 1;
        self.free.push(i);
        self.slots[i].take().expect("released empty uring-fs slot")
    }

    /// Build the SQE for the current state of slot `i`.
    fn sqe_for(&self, i: usize) -> io_uring::squeue::Entry {
        use io_uring::{opcode, types};
        match self.slots[i].as_ref().expect("sqe for empty slot") {
            Pending::Read {
                file,
                buf,
                file_offset,
                filled,
                want,
                ..
            } => {
                let ptr = buf[*filled..].as_ptr() as *mut u8;
                opcode::Read::new(types::Fd(file.as_raw_fd()), ptr, (*want - *filled) as u32)
                    .offset(*file_offset + *filled as u64)
                    .build()
                    .user_data(i as u64)
            }
            Pending::Write {
                file,
                data,
                file_offset,
                written,
                ..
            } => opcode::Write::new(
                types::Fd(file.as_raw_fd()),
                data[*written..].as_ptr(),
                (data.len() - *written) as u32,
            )
            .offset(*file_offset + *written as u64)
            .build()
            .user_data(i as u64),
            Pending::Fsync { file, .. } => opcode::Fsync::new(types::Fd(file.as_raw_fd()))
                .flags(types::FsyncFlags::DATASYNC)
                .build()
                .user_data(i as u64),
        }
    }

    /// Queue the SQE for slot `i`; on a full SQ, complete the op with an
    /// error (should not happen under ADMIT_CAP, and must be loud if it does).
    fn push_slot(&mut self, i: usize) {
        let sqe = self.sqe_for(i);
        // SAFETY: buffers referenced by the SQE live in `self.slots[i]`, which
        // stays untouched until this SQE's completion is reaped.
        let res = unsafe { self.ring.submission().push(&sqe) };
        if res.is_err() {
            let p = self.release_slot(i);
            let err =
                || SqueezefsError::Io(std::io::Error::other("uring-fs submission queue overflow"));
            match p {
                Pending::Read { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
                Pending::Write { done, .. } => done.complete(Err(err())),
                Pending::Fsync { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
            }
        }
    }

    /// Admit one request: do the (rare, blocking) opens inline, then queue
    /// its first SQE(s).
    fn admit(&mut self, req: FsReq) {
        match req {
            FsReq::WriteAll { path, data, tx } => {
                let opened = (|| -> Result<Rc<File>> {
                    use std::os::unix::fs::OpenOptionsExt;
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(map_io)?;
                    }
                    let f = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .custom_flags(libc::O_CLOEXEC)
                        .open(&path)
                        .map_err(map_io)?;
                    Ok(self.cache.insert(path.clone(), f))
                })();
                match opened {
                    Ok(file) => {
                        let i = self.claim_slot(Pending::Write {
                            file,
                            data,
                            file_offset: 0,
                            written: 0,
                            done: UnitDone::Single(tx),
                        });
                        self.push_slot(i);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
            FsReq::ReadAll { path, tx } => {
                use std::os::unix::fs::OpenOptionsExt;
                let opened = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_CLOEXEC)
                    .open(&path)
                    .and_then(|f| f.metadata().map(|m| (f, m.len() as usize)))
                    .map_err(map_io);
                match opened {
                    Ok((f, len)) => {
                        let i = self.claim_slot(Pending::Read {
                            file: Rc::new(f),
                            buf: vec![0u8; len],
                            file_offset: 0,
                            filled: 0,
                            want: len,
                            tx,
                        });
                        if len == 0 {
                            // Nothing to read: complete immediately.
                            if let Pending::Read { tx, .. } = self.release_slot(i) {
                                let _ = tx.send(Ok(bytes::Bytes::new()));
                            }
                        } else {
                            self.push_slot(i);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
            FsReq::ReadAt {
                path,
                offset,
                size,
                tx,
            } => match cached_open(&mut self.cache, &path, true) {
                Ok(file) => {
                    let i = self.claim_slot(Pending::Read {
                        file,
                        buf: vec![0u8; size],
                        file_offset: offset,
                        filled: 0,
                        want: size,
                        tx,
                    });
                    if size == 0 {
                        if let Pending::Read { tx, .. } = self.release_slot(i) {
                            let _ = tx.send(Ok(bytes::Bytes::new()));
                        }
                    } else {
                        self.push_slot(i);
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            },
            FsReq::WriteAt {
                path,
                offset,
                data,
                tx,
            } => match cached_open(&mut self.cache, &path, true) {
                Ok(file) => {
                    if data.is_empty() {
                        let _ = tx.send(Ok(()));
                        return;
                    }
                    let i = self.claim_slot(Pending::Write {
                        file,
                        data,
                        file_offset: offset,
                        written: 0,
                        done: UnitDone::Single(tx),
                    });
                    self.push_slot(i);
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            },
            FsReq::WriteAtBatch { path, ops, tx } => {
                match cached_open(&mut self.cache, &path, true) {
                    Ok(file) => {
                        let entries: Vec<(u64, bytes::Bytes)> =
                            ops.into_iter().filter(|(_, d)| !d.is_empty()).collect();
                        if entries.is_empty() {
                            let _ = tx.send(Ok(()));
                            return;
                        }
                        let state = Rc::new(std::cell::RefCell::new(BatchState {
                            remaining: entries.len(),
                            first_err: None,
                            tx: Some(tx),
                        }));
                        for (offset, data) in entries {
                            let i = self.claim_slot(Pending::Write {
                                file: file.clone(),
                                data,
                                file_offset: offset,
                                written: 0,
                                done: UnitDone::Batch(state.clone()),
                            });
                            self.push_slot(i);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            }
            FsReq::Fdatasync { path, tx } => match cached_open(&mut self.cache, &path, false) {
                Ok(file) => {
                    let i = self.claim_slot(Pending::Fsync { file, tx });
                    self.push_slot(i);
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            },
        }
    }

    /// Advance slot `i` with its completion result; returns `true` if the
    /// slot needs its next SQE pushed (short read/write continuation).
    fn advance(&mut self, i: usize, res: i32) -> bool {
        enum Next {
            Done,
            Resubmit,
        }
        let next = {
            let p = self.slots[i].as_mut().expect("completion for empty slot");
            match p {
                Pending::Read { filled, want, .. } => {
                    if res < 0 {
                        Next::Done
                    } else if res == 0 {
                        Next::Done // EOF: short read, truncate below
                    } else {
                        *filled += res as usize;
                        if *filled < *want {
                            Next::Resubmit
                        } else {
                            Next::Done
                        }
                    }
                }
                Pending::Write { written, data, .. } => {
                    if res <= 0 {
                        Next::Done
                    } else {
                        *written += res as usize;
                        if *written < data.len() {
                            Next::Resubmit
                        } else {
                            Next::Done
                        }
                    }
                }
                Pending::Fsync { .. } => Next::Done,
            }
        };

        match next {
            Next::Resubmit => true,
            Next::Done => {
                let p = self.release_slot(i);
                match p {
                    Pending::Read {
                        mut buf,
                        filled,
                        tx,
                        ..
                    } => {
                        if res < 0 {
                            let _ = tx.send(Err(map_io(std::io::Error::from_raw_os_error(-res))));
                        } else {
                            buf.truncate(filled);
                            let _ = tx.send(Ok(bytes::Bytes::from(buf)));
                        }
                    }
                    Pending::Write { done, .. } => {
                        if res < 0 {
                            done.complete(Err(map_io(std::io::Error::from_raw_os_error(-res))));
                        } else if res == 0 {
                            done.complete(Err(map_io(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "uring write returned 0",
                            ))));
                        } else {
                            done.complete(Ok(()));
                        }
                    }
                    Pending::Fsync { tx, .. } => {
                        if res < 0 {
                            let _ = tx.send(Err(map_io(std::io::Error::from_raw_os_error(-res))));
                        } else {
                            let _ = tx.send(Ok(()));
                        }
                    }
                }
                false
            }
        }
    }
}

fn worker_loop(rx: crossbeam::channel::Receiver<FsReq>) {
    let ring = match io_uring::IoUring::new(RING_ENTRIES) {
        Ok(r) => r,
        Err(e) => {
            log::error!("uring-fs: failed to create IoUring: {e:?}");
            blocking_fallback_loop(rx);
            return;
        }
    };
    let mut r = Reactor::new(ring);
    let mut disconnected = false;

    loop {
        // Admission: block only when idle; otherwise burst-drain the queue.
        if r.inflight == 0 {
            if disconnected {
                return;
            }
            match rx.recv() {
                Ok(req) => r.admit(req),
                Err(_) => return,
            }
        }
        while r.inflight < ADMIT_CAP && !disconnected {
            match rx.try_recv() {
                Ok(req) => r.admit(req),
                Err(crossbeam::channel::TryRecvError::Empty) => break,
                Err(crossbeam::channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                }
            }
        }
        if r.inflight == 0 {
            continue;
        }

        // Submit everything queued and wait for at least one completion.
        match r.ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Ring is broken: fail every in-flight op loudly and drop to
                // the blocking fallback for the rest of the process lifetime.
                log::error!("uring-fs: submit_and_wait failed: {e:?}");
                for i in 0..r.slots.len() {
                    if r.slots[i].is_some() {
                        let p = r.release_slot(i);
                        let err = || map_io(std::io::Error::other("uring-fs ring failed"));
                        match p {
                            Pending::Read { tx, .. } => {
                                let _ = tx.send(Err(err()));
                            }
                            Pending::Write { done, .. } => done.complete(Err(err())),
                            Pending::Fsync { tx, .. } => {
                                let _ = tx.send(Err(err()));
                            }
                        }
                    }
                }
                blocking_fallback_loop(rx);
                return;
            }
        }

        // Reap all available completions; push continuations after the CQ
        // borrow ends (each continuation reuses a just-reaped SQ slot).
        let mut resubmit: Vec<usize> = Vec::new();
        {
            let mut cq = r.ring.completion();
            cq.sync();
            let completed: Vec<(usize, i32)> = (&mut cq)
                .map(|cqe| (cqe.user_data() as usize, cqe.result()))
                .collect();
            drop(cq);
            for (slot, res) in completed {
                if r.advance(slot, res) {
                    resubmit.push(slot);
                }
            }
        }
        for slot in resubmit {
            r.push_slot_continue(slot);
        }
    }
}

impl Reactor {
    /// Re-queue a continuation SQE for a slot that stays in flight (the slot
    /// was NOT released, so `inflight` is unchanged).
    fn push_slot_continue(&mut self, i: usize) {
        let sqe = self.sqe_for(i);
        // SAFETY: as in `push_slot` — the slot owns every buffer the SQE
        // references and is not touched until its completion arrives.
        let res = unsafe { self.ring.submission().push(&sqe) };
        if res.is_err() {
            let p = self.release_slot(i);
            let err = || {
                SqueezefsError::Io(std::io::Error::other(
                    "uring-fs submission queue overflow (continuation)",
                ))
            };
            match p {
                Pending::Read { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
                Pending::Write { done, .. } => done.complete(Err(err())),
                Pending::Fsync { tx, .. } => {
                    let _ = tx.send(Err(err()));
                }
            }
        }
    }
}

/// Classical blocking service loop, used only when the kernel cannot give us
/// a ring (or the ring died): callers still complete, loudly logged once.
fn blocking_fallback_loop(rx: crossbeam::channel::Receiver<FsReq>) {
    use std::os::unix::fs::FileExt;
    while let Ok(req) = rx.recv() {
        match req {
            FsReq::WriteAll { path, data, tx } => {
                let res = (|| -> std::io::Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, &data)
                })();
                let _ = tx.send(res.map_err(map_io));
            }
            FsReq::ReadAll { path, tx } => {
                let _ = tx.send(std::fs::read(&path).map(bytes::Bytes::from).map_err(map_io));
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
                        let mut buf = vec![0u8; size];
                        let mut filled = 0usize;
                        while filled < size {
                            let n = f.read_at(&mut buf[filled..], offset + filled as u64)?;
                            if n == 0 {
                                break;
                            }
                            filled += n;
                        }
                        buf.truncate(filled);
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
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&path)
                    .and_then(|f| f.write_all_at(&data, offset))
                    .map_err(map_io);
                let _ = tx.send(res);
            }
            FsReq::WriteAtBatch { path, ops, tx } => {
                let res = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&path)
                    .and_then(|f| {
                        for (offset, data) in &ops {
                            f.write_all_at(data, *offset)?;
                        }
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
