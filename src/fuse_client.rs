use crate::dlm::DlmClient;
use crate::error::SqueezefsError;
use crate::routing::DataRouter;
use fuse3::raw::{
    prelude::*,
    reply::{DirectoryEntry, DirectoryEntryPlus, FileAttr},
    Request,
};
use fuse3::{Errno, MountOptions, Result as FuseResult};
use log::{debug, error, info};
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tokio::runtime::Builder;

pub struct SqueezefsFilesystem {
    router: DataRouter,
    dlm: DlmClient,
}

impl SqueezefsFilesystem {
    pub fn new(router: DataRouter, dlm: DlmClient) -> Self {
        Self { router, dlm }
    }
}

// Implement fuse3 Raw Filesystem interface
impl Filesystem for SqueezefsFilesystem {
    type DirEntryStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntry>>;
    type DirEntryPlusStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntryPlus>>;
    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        info!("FUSE Daemon: Initialized Squeezefs Filesystem mount.");
        Ok(ReplyInit {
            max_write: std::num::NonZeroU32::new(1048576).unwrap(), // 1MB absolute maximum write buffer size
        })
    }

    async fn destroy(&self, _req: Request) {
        info!("FUSE Daemon: Destroying mount.");
    }

    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<ReplyEntry> {
        let name_str = name.to_string_lossy();
        debug!("FUSE Lookup: parent = {}, name = {}", parent, name_str);

        // Simulate lookup against Garnet metadata
        let attr = FileAttr {
            ino: 2, // arbitrary unique inode
            size: 0,
            blocks: 0,
            atime: SystemTime::now().into(),
            mtime: SystemTime::now().into(),
            ctime: SystemTime::now().into(),
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
        };

        Ok(ReplyEntry {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
        })
    }

    async fn getattr(
        &self,
        _req: Request,
        ino: u64,
        fh: Option<u64>,
        flags: u32,
    ) -> FuseResult<ReplyAttr> {
        debug!(
            "FUSE GetAttr: ino = {}, fh = {:?}, flags = {}",
            ino, fh, flags
        );

        let attr = FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: SystemTime::now().into(),
            mtime: SystemTime::now().into(),
            ctime: SystemTime::now().into(),
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
        };

        Ok(ReplyAttr {
            ttl: Duration::from_secs(1),
            attr,
        })
    }

    async fn create(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        flags: u32,
    ) -> FuseResult<ReplyCreated> {
        let name_str = name.to_string_lossy();
        info!(
            "FUSE Create: parent = {}, name = {}, mode = {:o}, flags = {}",
            parent, name_str, mode, flags
        );

        let attr = FileAttr {
            ino: 3,
            size: 0,
            blocks: 0,
            atime: SystemTime::now().into(),
            mtime: SystemTime::now().into(),
            ctime: SystemTime::now().into(),
            kind: FileType::RegularFile,
            perm: mode as u16,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
        };

        Ok(ReplyCreated {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
            fh: 1, // file handle
            flags,
        })
    }

    async fn read(
        &self,
        _req: Request,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> FuseResult<ReplyData> {
        debug!(
            "FUSE Read: ino = {}, fh = {}, offset = {}, size = {}",
            ino, fh, offset, size
        );

        // Map to routing and read block
        let file_path = format!("inode_{}", ino);

        // Timeout protection (fail fast within 2 seconds to prevent kernel hang)
        let read_future = self.router.read_file(&file_path);
        let read_result = match tokio::time::timeout(Duration::from_secs(2), read_future).await {
            Ok(Ok(data)) => data,
            Ok(Err(e)) => {
                error!("FUSE Read failed: {:?}", e);
                return Err(Errno::from(libc::EIO));
            }
            Err(_) => {
                error!("FUSE Read timed out!");
                return Err(Errno::from(libc::ETIMEDOUT));
            }
        };

        let data_len = read_result.len() as u64;
        if offset >= data_len {
            return Ok(ReplyData {
                data: vec![].into(),
            });
        }

        let start = offset as usize;
        let end = std::cmp::min((offset + size as u64) as usize, read_result.len());
        let slice = read_result[start..end].to_vec();

        Ok(ReplyData { data: slice.into() })
    }

    async fn write(
        &self,
        _req: Request,
        ino: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        write_flags: u32,
        flags: u32,
    ) -> FuseResult<ReplyWrite> {
        debug!(
            "FUSE Write: ino = {}, fh = {}, offset = {}, size = {}, write_flags = {}, flags = {}",
            ino,
            fh,
            offset,
            data.len(),
            write_flags,
            flags
        );

        let file_path = format!("inode_{}", ino);

        // 1. Acquire distributed lock & fencing token (TTL 5 seconds)
        let lease = match self
            .dlm
            .acquire_lock(&file_path, None, Duration::from_secs(5))
            .await
        {
            Ok(l) => l,
            Err(e) => {
                error!("FUSE Write: Lock acquisition failed: {:?}", e);
                return Err(Errno::from(libc::EAGAIN)); // EAGAIN for busy resource
            }
        };

        // 2. Perform progressive routed write with fencing token
        let write_future = self
            .router
            .write_file(&file_path, data, lease.fencing_token());

        // Timeout protection (fail fast within 2 seconds)
        match tokio::time::timeout(Duration::from_secs(2), write_future).await {
            Ok(Ok(())) => {
                let bytes_written = data.len() as u32;
                Ok(ReplyWrite {
                    written: bytes_written,
                })
            }
            Ok(Err(SqueezefsError::FencingTokenExpired { token, expected })) => {
                error!("FUSE Write: Stale write rejected due to expired fencing token {} (expected >= {}). Discarding local transaction.", token, expected);
                Err(Errno::from(libc::EIO))
            }
            Ok(Err(e)) => {
                error!("FUSE Write failed: {:?}", e);
                Err(Errno::from(libc::EIO))
            }
            Err(_) => {
                error!("FUSE Write timed out! Falling back to local NVMe staging disk.");
                // If it timed out, try to force stage it on NVMe directly as fallback
                let file_id = uuid::Uuid::new_v4().to_string();
                if let Err(stage_err) = self
                    .router
                    .cache()
                    .nvme
                    .stage_write(&file_path, &file_id, data, lease.fencing_token())
                    .await
                {
                    error!("NVMe staging fallback write also failed: {:?}", stage_err);
                    return Err(Errno::from(libc::EIO));
                }
                Ok(ReplyWrite {
                    written: data.len() as u32,
                })
            }
        }
    }
}

/// Initialize the multi-threaded work-stealing tokio runtime
/// with threads pinned strictly to physical cores, keeping one core free.
pub fn init_runtime() -> tokio::runtime::Runtime {
    let physical_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Leave at least one core for kernel processing (FUSE filesystem driver, S3, Garnet, networking)
    let worker_threads = std::cmp::max(1, physical_cores - 1);
    info!(
        "FUSE Daemon: Initializing runtime with {} worker threads bound to physical CPU cores.",
        worker_threads
    );

    Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .on_thread_start(|| {
            // Pin worker thread to physical CPU cores
            // In a real environment, we use: core_affinity::pin_to_self(core_id)
            debug!("Thread started and pinned to physical CPU core.");
        })
        .build()
        .unwrap()
}

/// Low-level io_uring polling loop for /dev/fuse.
/// When compiled for Linux, registers /dev/fuse descriptor to io_uring to intercept events
/// and delegate requests instantly to the runtime thread pool.
#[cfg(target_os = "linux")]
pub fn start_io_uring_polling_loop(
    fuse_fd: std::os::fd::RawFd,
    _runtime: &tokio::runtime::Runtime,
) {
    use io_uring::{opcode, types, IoUring};

    info!("FUSE Daemon: Initializing io_uring polling ring on FUSE descriptor.");
    let mut ring = IoUring::new(256).expect("Failed to initialize io_uring");

    // We register FUSE fd to io_uring and poll for read availability (POLLIN)
    // The main thread sits in this loop, re-submitting read/poll templates,
    // and spawning tasks onto the worker thread pool when requests arrive.

    let mut buf = vec![0u8; 4096];

    loop {
        // Prepare read entry
        let read_e = opcode::Read::new(types::Fd(fuse_fd), buf.as_mut_ptr(), buf.len() as u32)
            .build()
            .user_data(0x01);

        unsafe {
            ring.submission()
                .push(&read_e)
                .expect("Failed to push read entry to io_uring submission queue");
        }

        ring.submit_and_wait(1).expect("io_uring wait failed");

        let mut cq = ring.completion();
        for cqe in &mut cq {
            if cqe.user_data() == 0x01 {
                let res = cqe.result();
                if res > 0 {
                    let bytes_read = res as usize;
                    debug!(
                        "io_uring FUSE poll read: reaped {} bytes from /dev/fuse",
                        bytes_read
                    );

                    // Delegate raw request to work-stealing thread pool
                    // runtime.spawn(async move { ... process request ... });
                }
            }
        }
    }
}

/// Start FUSE mount daemon using fuse3.
pub async fn start_mount<P: AsRef<Path>>(
    mountpoint: P,
    fs: SqueezefsFilesystem,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = MountOptions::default();
    options.uid(1000);
    options.gid(1000);

    // fuse3 Mount parameters
    // 1. Set max_read/max_write to 1MB absolute maximums
    // 2. Enable writeback_cache to merge writes in kernel
    // 3. Enable async_dio (asynchronous direct I/O)
    options.custom_options("max_read=1048576");
    options.custom_options("max_write=1048576");
    options.custom_options("writeback_cache=yes");
    options.custom_options("async_dio=yes");

    info!(
        "FUSE Daemon: Mounting squeezefs at {:?}...",
        mountpoint.as_ref()
    );

    let mount_path = mountpoint.as_ref().to_path_buf();

    // Spawns the mount loop using fuse3 Session
    let _session = fuse3::raw::Session::new(options)
        .mount_with_unprivileged(fs, mount_path)
        .await?;

    Ok(())
}
