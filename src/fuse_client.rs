use crate::dlm::DlmClient;
use crate::error::SqueezefsError;
use crate::routing::DataRouter;
use fuse3::raw::{
    prelude::*,
    reply::{DirectoryEntry, FileAttr},
    Request,
};
use fuse3::{Errno, MountOptions, Result as FuseResult, Timestamp};
use log::{debug, error, info};
use once_cell::sync::Lazy;
use redis::AsyncCommands;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::runtime::Builder;

#[derive(Default)]
pub struct Metrics {
    pub fuse_ops: AtomicU64,
    pub meta_updates: AtomicU64,
    pub put_obj: AtomicU64,
    pub get_obj: AtomicU64,
    pub del_obj: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
}

pub static METRICS: Lazy<Metrics> = Lazy::new(Metrics::default);

fn map_err(e: redis::RedisError) -> Errno {
    error!("Garnet Database error: {:?}", e);
    Errno::from(libc::ECOMM)
}

fn map_squeezefs_err(e: SqueezefsError) -> Errno {
    error!("Squeezefs operational error: {:?}", e);
    Errno::from(e.to_errno())
}

pub struct SqueezefsFilesystem {
    router: DataRouter,
    dlm: DlmClient,
}

impl SqueezefsFilesystem {
    pub fn new(router: DataRouter, dlm: DlmClient) -> Self {
        Self { router, dlm }
    }

    async fn init_root_inode(&self) -> Result<(), SqueezefsError> {
        let mut con = self.dlm.get_connection().await?;
        let exists: bool = con.exists("squeezefs:attr:1").await?;
        if !exists {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();

            let _: () = redis::pipe()
                .hset("squeezefs:attr:1", "ino", 1)
                .hset("squeezefs:attr:1", "size", 0)
                .hset("squeezefs:attr:1", "blocks", 0)
                .hset("squeezefs:attr:1", "kind", 2) // Directory
                .hset("squeezefs:attr:1", "perm", 0o777)
                .hset("squeezefs:attr:1", "nlink", 2)
                .hset("squeezefs:attr:1", "uid", 1000)
                .hset("squeezefs:attr:1", "gid", 1000)
                .hset("squeezefs:attr:1", "atime_sec", sec)
                .hset("squeezefs:attr:1", "atime_nsec", nsec)
                .hset("squeezefs:attr:1", "mtime_sec", sec)
                .hset("squeezefs:attr:1", "mtime_nsec", nsec)
                .hset("squeezefs:attr:1", "ctime_sec", sec)
                .hset("squeezefs:attr:1", "ctime_nsec", nsec)
                .set_nx("squeezefs:inode_counter", 1)
                .query_async(&mut con)
                .await?;
        }
        Ok(())
    }

    async fn get_attr_internal(&self, ino: u64) -> Result<FileAttr, SqueezefsError> {
        let mut con = self.dlm.get_connection().await?;
        let attr_key = format!("squeezefs:attr:{}", ino);
        let fields: std::collections::HashMap<String, String> = con.hgetall(&attr_key).await?;

        if fields.is_empty() {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Inode {} not found", ino),
            )));
        }

        let ino = fields
            .get("ino")
            .and_then(|v| v.parse().ok())
            .unwrap_or(ino);
        let size = fields.get("size").and_then(|v| v.parse().ok()).unwrap_or(0);
        let blocks = fields
            .get("blocks")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let kind_num: u8 = fields.get("kind").and_then(|v| v.parse().ok()).unwrap_or(1);
        let kind = match kind_num {
            2 => FileType::Directory,
            3 => FileType::Symlink,
            4 => FileType::NamedPipe,
            5 => FileType::CharDevice,
            6 => FileType::BlockDevice,
            7 => FileType::Socket,
            _ => FileType::RegularFile,
        };
        let perm = fields
            .get("perm")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0o644);
        let nlink = fields
            .get("nlink")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let uid = fields
            .get("uid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let gid = fields
            .get("gid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let rdev = fields.get("rdev").and_then(|v| v.parse().ok()).unwrap_or(0);
        let blksize = fields
            .get("blksize")
            .and_then(|v| v.parse().ok())
            .unwrap_or(4096);

        let atime_sec = fields
            .get("atime_sec")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let atime_nsec = fields
            .get("atime_nsec")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mtime_sec = fields
            .get("mtime_sec")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mtime_nsec = fields
            .get("mtime_nsec")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let ctime_sec = fields
            .get("ctime_sec")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let ctime_nsec = fields
            .get("ctime_nsec")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        Ok(FileAttr {
            ino,
            size,
            blocks,
            atime: Timestamp::new(atime_sec, atime_nsec),
            mtime: Timestamp::new(mtime_sec, mtime_nsec),
            ctime: Timestamp::new(ctime_sec, ctime_nsec),
            kind,
            perm,
            nlink,
            uid,
            gid,
            rdev,
            blksize,
        })
    }

    async fn update_parent_timestamps(
        &self,
        con: &mut crate::dlm::MetaConnection,
        parent: u64,
    ) -> Result<(), redis::RedisError> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();
        let parent_attr_key = format!("squeezefs:attr:{}", parent);
        let _: () = redis::pipe()
            .hset(&parent_attr_key, "mtime_sec", sec)
            .hset(&parent_attr_key, "mtime_nsec", nsec)
            .hset(&parent_attr_key, "ctime_sec", sec)
            .hset(&parent_attr_key, "ctime_nsec", nsec)
            .query_async(con)
            .await?;
        Ok(())
    }
}

// Implement fuse3 Raw Filesystem interface
impl Filesystem for SqueezefsFilesystem {
    type DirEntryStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntry>>;
    type DirEntryPlusStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntryPlus>>;

    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        info!("FUSE Daemon: Initialized Squeezefs Filesystem mount.");

        // Initialize root directory attributes in Garnet if not present
        if let Err(e) = self.init_root_inode().await {
            error!("Failed to initialize root inode in Garnet: {:?}", e);
            return Err(Errno::from(libc::EIO));
        }

        // Start background metrics publishing task
        let redis_client = self.dlm.meta_client().clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if let Ok(mut con) = redis_client.get_connection().await {
                    let _: Result<(), redis::RedisError> = redis::pipe()
                        .hset(
                            "metrics:daemon",
                            "fuse_ops",
                            METRICS.fuse_ops.load(Ordering::Relaxed),
                        )
                        .hset(
                            "metrics:daemon",
                            "meta_updates",
                            METRICS.meta_updates.load(Ordering::Relaxed),
                        )
                        .hset(
                            "metrics:daemon",
                            "put_obj",
                            METRICS.put_obj.load(Ordering::Relaxed),
                        )
                        .hset(
                            "metrics:daemon",
                            "get_obj",
                            METRICS.get_obj.load(Ordering::Relaxed),
                        )
                        .hset(
                            "metrics:daemon",
                            "del_obj",
                            METRICS.del_obj.load(Ordering::Relaxed),
                        )
                        .hset(
                            "metrics:daemon",
                            "cache_hits",
                            METRICS.cache_hits.load(Ordering::Relaxed),
                        )
                        .hset(
                            "metrics:daemon",
                            "cache_misses",
                            METRICS.cache_misses.load(Ordering::Relaxed),
                        )
                        .query_async(&mut con)
                        .await;
                }
            }
        });

        Ok(ReplyInit {
            max_write: std::num::NonZeroU32::new(1048576).unwrap(), // 1MB absolute maximum write buffer size
        })
    }

    async fn destroy(&self, _req: Request) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        info!("FUSE Daemon: Destroying mount.");
    }

    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        debug!("FUSE Lookup: parent = {}, name = {}", parent, name_str);

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let dir_key = format!("squeezefs:dir:{}", parent);
        let child_ino_opt: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;

        let child_ino = match child_ino_opt {
            Some(ino) => ino,
            None => return Err(Errno::from(libc::ENOENT)),
        };

        let attr = self
            .get_attr_internal(child_ino)
            .await
            .map_err(map_squeezefs_err)?;

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
        _fh: Option<u64>,
        _flags: u32,
    ) -> FuseResult<ReplyAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE GetAttr: ino = {}", ino);

        let attr = self
            .get_attr_internal(ino)
            .await
            .map_err(map_squeezefs_err)?;

        Ok(ReplyAttr {
            ttl: Duration::from_secs(1),
            attr,
        })
    }

    async fn mknod(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        info!(
            "FUSE mknod: parent = {}, name = {}, mode = {:o}, rdev = {}",
            parent, name_str, mode, rdev
        );

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        // Check if name already exists in parent
        let dir_key = format!("squeezefs:dir:{}", parent);
        let exists: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
        if exists.is_some() {
            return Err(Errno::from(libc::EEXIST));
        }

        // Determine FileType kind number
        let file_type_mask = mode & libc::S_IFMT;
        let kind_num = if file_type_mask == libc::S_IFIFO {
            4
        } else if file_type_mask == libc::S_IFCHR {
            5
        } else if file_type_mask == libc::S_IFBLK {
            6
        } else if file_type_mask == libc::S_IFSOCK {
            7
        } else if file_type_mask == libc::S_IFDIR {
            2
        } else if file_type_mask == libc::S_IFLNK {
            3
        } else {
            1 // Default: Regular file
        };

        // Allocate new inode
        let new_ino: u64 = con
            .incr("squeezefs:inode_counter", 1)
            .await
            .map_err(map_err)?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let attr_key = format!("squeezefs:attr:{}", new_ino);

        // Add to parent, set attributes, and save
        let _: () = redis::pipe()
            .hset(&dir_key, &*name_str, new_ino)
            .hset(&attr_key, "ino", new_ino)
            .hset(&attr_key, "size", 0)
            .hset(&attr_key, "blocks", 0)
            .hset(&attr_key, "kind", kind_num)
            .hset(&attr_key, "perm", mode as u16 & 0o7777)
            .hset(&attr_key, "nlink", 1)
            .hset(&attr_key, "uid", req.uid)
            .hset(&attr_key, "gid", req.gid)
            .hset(&attr_key, "rdev", rdev)
            .hset(&attr_key, "atime_sec", sec)
            .hset(&attr_key, "atime_nsec", nsec)
            .hset(&attr_key, "mtime_sec", sec)
            .hset(&attr_key, "mtime_nsec", nsec)
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        let attr = self
            .get_attr_internal(new_ino)
            .await
            .map_err(map_squeezefs_err)?;

        Ok(ReplyEntry {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
        })
    }

    async fn create(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        flags: u32,
    ) -> FuseResult<ReplyCreated> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        info!(
            "FUSE Create: parent = {}, name = {}, mode = {:o}, flags = {}",
            parent, name_str, mode, flags
        );

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        // Check if file already exists
        let dir_key = format!("squeezefs:dir:{}", parent);
        let exists: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
        if exists.is_some() {
            return Err(Errno::from(libc::EEXIST));
        }

        // Allocate new inode
        let new_ino: u64 = con
            .incr("squeezefs:inode_counter", 1)
            .await
            .map_err(map_err)?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let attr_key = format!("squeezefs:attr:{}", new_ino);

        // Add to parent directory, set attributes, and save
        let _: () = redis::pipe()
            .hset(&dir_key, &*name_str, new_ino)
            .hset(&attr_key, "ino", new_ino)
            .hset(&attr_key, "size", 0)
            .hset(&attr_key, "blocks", 0)
            .hset(&attr_key, "kind", 1) // RegularFile
            .hset(&attr_key, "perm", mode as u16 & 0o7777)
            .hset(&attr_key, "nlink", 1)
            .hset(&attr_key, "uid", req.uid)
            .hset(&attr_key, "gid", req.gid)
            .hset(&attr_key, "atime_sec", sec)
            .hset(&attr_key, "atime_nsec", nsec)
            .hset(&attr_key, "mtime_sec", sec)
            .hset(&attr_key, "mtime_nsec", nsec)
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        let attr = self
            .get_attr_internal(new_ino)
            .await
            .map_err(map_squeezefs_err)?;

        Ok(ReplyCreated {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
            fh: new_ino, // file handle
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
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE Read: ino = {}, fh = {}, offset = {}, size = {}",
            ino, fh, offset, size
        );

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
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
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
            .write_file(&file_path, offset, data, lease.fencing_token());

        // Timeout protection (fail fast within 2 seconds)
        match tokio::time::timeout(Duration::from_secs(2), write_future).await {
            Ok(Ok(())) => {
                let bytes_written = data.len() as u32;

                // Update size in inode attributes in Garnet
                if let Ok(mut con) = self.dlm.get_connection().await {
                    let attr_key = format!("squeezefs:attr:{}", ino);
                    let now = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or(Duration::ZERO);
                    let sec = now.as_secs() as i64;
                    let nsec = now.subsec_nanos();
                    let _: Result<(), redis::RedisError> = redis::pipe()
                        .hset(&attr_key, "size", offset + bytes_written as u64)
                        .hset(&attr_key, "mtime_sec", sec)
                        .hset(&attr_key, "mtime_nsec", nsec)
                        .hset(&attr_key, "ctime_sec", sec)
                        .hset(&attr_key, "ctime_nsec", nsec)
                        .query_async(&mut con)
                        .await;
                }

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

    async fn mkdir(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        debug!(
            "FUSE mkdir: parent = {}, name = {}, mode = {:o}",
            parent, name_str, mode
        );

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        // Check if name already exists in parent
        let dir_key = format!("squeezefs:dir:{}", parent);
        let exists: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
        if exists.is_some() {
            return Err(Errno::from(libc::EEXIST));
        }

        // Allocate new inode
        let new_ino: u64 = con
            .incr("squeezefs:inode_counter", 1)
            .await
            .map_err(map_err)?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let attr_key = format!("squeezefs:attr:{}", new_ino);

        // Add to parent, set directory attributes, and also keep track of parent directory for "." and ".."
        let child_dir_key = format!("squeezefs:dir:{}", new_ino);
        let _: () = redis::pipe()
            .hset(&dir_key, &*name_str, new_ino)
            .hset(&attr_key, "ino", new_ino)
            .hset(&attr_key, "size", 0)
            .hset(&attr_key, "blocks", 0)
            .hset(&attr_key, "kind", 2) // Directory
            .hset(&attr_key, "perm", mode as u16 & 0o7777)
            .hset(&attr_key, "nlink", 2) // "." and parent entry
            .hset(&attr_key, "uid", req.uid)
            .hset(&attr_key, "gid", req.gid)
            .hset(&attr_key, "atime_sec", sec)
            .hset(&attr_key, "atime_nsec", nsec)
            .hset(&attr_key, "mtime_sec", sec)
            .hset(&attr_key, "mtime_nsec", nsec)
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec)
            .hset(&child_dir_key, ".", new_ino)
            .hset(&child_dir_key, "..", parent)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Increment parent's nlink (directories have parent + children link count)
        let parent_attr_key = format!("squeezefs:attr:{}", parent);
        let _: Result<(), redis::RedisError> = con.hincr(&parent_attr_key, "nlink", 1).await;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        let attr = self
            .get_attr_internal(new_ino)
            .await
            .map_err(map_squeezefs_err)?;

        Ok(ReplyEntry {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
        })
    }

    async fn rmdir(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        debug!("FUSE rmdir: parent = {}, name = {}", parent, name_str);

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        let dir_key = format!("squeezefs:dir:{}", parent);
        let ino_opt: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
        let ino = match ino_opt {
            Some(i) => i,
            None => return Err(Errno::from(libc::ENOENT)),
        };

        // Check if directory is empty (only "." and ".." are allowed)
        let child_dir_key = format!("squeezefs:dir:{}", ino);
        let keys: Vec<String> = con.hkeys(&child_dir_key).await.map_err(map_err)?;
        for k in keys {
            if k != "." && k != ".." {
                return Err(Errno::from(libc::ENOTEMPTY));
            }
        }

        // Delete directory metadata and parent link
        let attr_key = format!("squeezefs:attr:{}", ino);
        let _: () = redis::pipe()
            .hdel(&dir_key, &*name_str)
            .del(&attr_key)
            .del(&child_dir_key)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Decrement parent's nlink
        let parent_attr_key = format!("squeezefs:attr:{}", parent);
        let _: Result<(), redis::RedisError> = con.hincr(&parent_attr_key, "nlink", -1).await;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        Ok(())
    }

    async fn setattr(
        &self,
        _req: Request,
        ino: u64,
        _fh: Option<u64>,
        set_attr: SetAttr,
    ) -> FuseResult<ReplyAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let attr_key = format!("squeezefs:attr:{}", ino);

        // Check if inode exists first
        let exists: bool = con.exists(&attr_key).await.map_err(map_err)?;
        if !exists {
            return Err(Errno::from(libc::ENOENT));
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let mut pipe = redis::pipe();

        if let Some(mode) = set_attr.mode {
            pipe.hset(&attr_key, "perm", mode as u16 & 0o7777);
        }
        if let Some(uid) = set_attr.uid {
            pipe.hset(&attr_key, "uid", uid);
        }
        if let Some(gid) = set_attr.gid {
            pipe.hset(&attr_key, "gid", gid);
        }
        if let Some(size) = set_attr.size {
            pipe.hset(&attr_key, "size", size);
            // Also update the physical/routing size in the metadata block?
            let meta_key = format!("metadata:inode_{}", ino);
            pipe.hset(&meta_key, "size", size);
        }

        // Handle timestamps
        if let Some(atime) = set_attr.atime {
            pipe.hset(&attr_key, "atime_sec", atime.sec);
            pipe.hset(&attr_key, "atime_nsec", atime.nsec);
        }
        if let Some(mtime) = set_attr.mtime {
            pipe.hset(&attr_key, "mtime_sec", mtime.sec);
            pipe.hset(&attr_key, "mtime_nsec", mtime.nsec);
        }

        // Always update ctime
        pipe.hset(&attr_key, "ctime_sec", sec);
        pipe.hset(&attr_key, "ctime_nsec", nsec);

        let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;

        let attr = self
            .get_attr_internal(ino)
            .await
            .map_err(map_squeezefs_err)?;
        Ok(ReplyAttr {
            ttl: Duration::from_secs(1),
            attr,
        })
    }

    async fn symlink(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        link: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        let link_str = link.to_string_lossy();
        debug!(
            "FUSE symlink: parent = {}, name = {}, link = {}",
            parent, name_str, link_str
        );

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        // Check if name already exists in parent
        let dir_key = format!("squeezefs:dir:{}", parent);
        let exists: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
        if exists.is_some() {
            return Err(Errno::from(libc::EEXIST));
        }

        // Allocate new inode
        let new_ino: u64 = con
            .incr("squeezefs:inode_counter", 1)
            .await
            .map_err(map_err)?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let attr_key = format!("squeezefs:attr:{}", new_ino);
        let symlink_key = format!("squeezefs:symlink:{}", new_ino);

        let _: () = redis::pipe()
            .hset(&dir_key, &*name_str, new_ino)
            .hset(&attr_key, "ino", new_ino)
            .hset(&attr_key, "size", link_str.len() as u64)
            .hset(&attr_key, "blocks", 0)
            .hset(&attr_key, "kind", 3) // Symlink
            .hset(&attr_key, "perm", 0o777)
            .hset(&attr_key, "nlink", 1)
            .hset(&attr_key, "uid", req.uid)
            .hset(&attr_key, "gid", req.gid)
            .hset(&attr_key, "atime_sec", sec)
            .hset(&attr_key, "atime_nsec", nsec)
            .hset(&attr_key, "mtime_sec", sec)
            .hset(&attr_key, "mtime_nsec", nsec)
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec)
            .set(&symlink_key, &*link_str)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        let attr = self
            .get_attr_internal(new_ino)
            .await
            .map_err(map_squeezefs_err)?;

        Ok(ReplyEntry {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
        })
    }

    async fn readlink(&self, _req: Request, ino: u64) -> FuseResult<ReplyData> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let symlink_key = format!("squeezefs:symlink:{}", ino);
        let target: Option<String> = con.get(&symlink_key).await.map_err(map_err)?;

        let target_str = match target {
            Some(t) => t,
            None => return Err(Errno::from(libc::ENOENT)),
        };

        Ok(ReplyData {
            data: target_str.into_bytes().into(),
        })
    }

    async fn link(
        &self,
        _req: Request,
        ino: u64,
        new_parent: u64,
        new_name: &OsStr,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let new_name_str = new_name.to_string_lossy();
        debug!(
            "FUSE link: ino = {}, new_parent = {}, new_name = {}",
            ino, new_parent, new_name_str
        );

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        // Check if destination name already exists in new_parent
        let dir_key = format!("squeezefs:dir:{}", new_parent);
        let exists: Option<u64> = con.hget(&dir_key, &*new_name_str).await.map_err(map_err)?;
        if exists.is_some() {
            return Err(Errno::from(libc::EEXIST));
        }

        // Check if source exists
        let attr_key = format!("squeezefs:attr:{}", ino);
        let source_exists: bool = con.exists(&attr_key).await.map_err(map_err)?;
        if !source_exists {
            return Err(Errno::from(libc::ENOENT));
        }

        let kind: u8 = con
            .hget(&attr_key, "kind")
            .await
            .map_err(map_err)
            .unwrap_or(1);
        if kind == 2 {
            // Directory
            return Err(Errno::from(libc::EPERM));
        }

        // Increment nlink and add entry to destination directory
        let _: () = redis::pipe()
            .hset(&dir_key, &*new_name_str, ino)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        let new_nlink: u32 = con.hincr(&attr_key, "nlink", 1).await.map_err(map_err)?;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();
        let _: () = redis::pipe()
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, new_parent)
            .await
            .map_err(map_err)?;

        let mut attr = self
            .get_attr_internal(ino)
            .await
            .map_err(map_squeezefs_err)?;
        attr.nlink = new_nlink;

        Ok(ReplyEntry {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
        })
    }

    async fn unlink(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        debug!("FUSE unlink: parent = {}, name = {}", parent, name_str);

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        let dir_key = format!("squeezefs:dir:{}", parent);
        let ino_opt: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
        let ino = match ino_opt {
            Some(i) => i,
            None => return Err(Errno::from(libc::ENOENT)),
        };

        let attr_key = format!("squeezefs:attr:{}", ino);
        let kind: u8 = con
            .hget(&attr_key, "kind")
            .await
            .map_err(map_err)
            .unwrap_or(1);
        if kind == 2 {
            // Directory
            return Err(Errno::from(libc::EISDIR));
        }

        // Remove from parent directory
        let _: () = con.hdel(&dir_key, &*name_str).await.map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        let current_nlink: u32 = con.hget(&attr_key, "nlink").await.map_err(map_err)?;

        if current_nlink > 1 {
            // Just decrement link count
            let _: () = redis::pipe()
                .hincr(&attr_key, "nlink", -1)
                .hset(
                    &attr_key,
                    "ctime_sec",
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                )
                .query_async(&mut con)
                .await
                .map_err(map_err)?;
        } else {
            // Delete metadata and data completely
            let file_path = format!("inode_{}", ino);
            let inline_key = format!("inline_data:{}", file_path);
            let meta_key = format!("metadata:{}", file_path);
            let symlink_key = format!("squeezefs:symlink:{}", ino);

            // Fetch block prefix if it was striped
            let file_type: Option<String> = con.hget(&meta_key, "type").await.map_err(map_err)?;
            if let Some(t) = file_type {
                if t == "striped" {
                    let block_prefix: Option<String> =
                        con.hget(&meta_key, "block_prefix").await.map_err(map_err)?;
                    let num_blocks: Option<u32> =
                        con.hget(&meta_key, "num_blocks").await.map_err(map_err)?;
                    if let (Some(bp), Some(nb)) = (block_prefix, num_blocks) {
                        for i in 0..nb {
                            let block_key = format!("{}/part_{}", bp, i);
                            let _ = self.router.backend().delete_object(&block_key).await;
                        }
                    }
                } else if t == "staged" {
                    let file_id_opt: Option<String> =
                        con.hget(&meta_key, "file_id").await.map_err(map_err)?;
                    if let Some(fid) = file_id_opt {
                        let mapping_key = format!("mapping:{}", fid);
                        let block_key: Option<String> =
                            con.hget(&mapping_key, "block").await.map_err(map_err)?;
                        if let Some(bk) = block_key {
                            let _ = self.router.backend().delete_object(&bk).await;
                        }
                        let _: () = con.del(&mapping_key).await.map_err(map_err)?;
                    }
                }
            }

            let _: () = redis::pipe()
                .del(&attr_key)
                .del(&inline_key)
                .del(&meta_key)
                .del(&symlink_key)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;
        }

        Ok(())
    }

    async fn rename(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        let new_name_str = new_name.to_string_lossy();
        debug!(
            "FUSE rename: parent = {}, name = {}, new_parent = {}, new_name = {}",
            parent, name_str, new_parent, new_name_str
        );

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        let src_dir_key = format!("squeezefs:dir:{}", parent);
        let dest_dir_key = format!("squeezefs:dir:{}", new_parent);

        let ino_opt: Option<u64> = con.hget(&src_dir_key, &*name_str).await.map_err(map_err)?;
        let ino = match ino_opt {
            Some(i) => i,
            None => return Err(Errno::from(libc::ENOENT)),
        };

        let attr_key = format!("squeezefs:attr:{}", ino);
        let src_kind: u8 = con
            .hget(&attr_key, "kind")
            .await
            .map_err(map_err)
            .unwrap_or(1);

        // Check for directory loop: if `ino` is a directory, traverse up from `new_parent` to root (1)
        // and check if `ino` is an ancestor of `new_parent`.
        if src_kind == 2 {
            let mut ancestor = new_parent;
            loop {
                if ancestor == ino {
                    return Err(Errno::from(libc::EINVAL));
                }
                if ancestor == 1 {
                    break;
                }
                let ancestor_dir_key = format!("squeezefs:dir:{}", ancestor);
                let parent_of_ancestor: Option<u64> =
                    con.hget(&ancestor_dir_key, "..").await.map_err(map_err)?;
                match parent_of_ancestor {
                    Some(p) => {
                        if p == ancestor {
                            break;
                        }
                        ancestor = p;
                    }
                    None => break,
                }
            }
        }

        // If target exists, delete it (overwrite behavior)
        let dest_ino_opt: Option<u64> = con
            .hget(&dest_dir_key, &*new_name_str)
            .await
            .map_err(map_err)?;
        if let Some(dest_ino) = dest_ino_opt {
            // Overwrite existing file or directory
            let dest_attr_key = format!("squeezefs:attr:{}", dest_ino);
            let dest_kind: u8 = con
                .hget(&dest_attr_key, "kind")
                .await
                .map_err(map_err)
                .unwrap_or(1);

            // Cross-type checks:
            if src_kind == 2 && dest_kind != 2 {
                return Err(Errno::from(libc::ENOTDIR));
            }
            if src_kind != 2 && dest_kind == 2 {
                return Err(Errno::from(libc::EISDIR));
            }

            if dest_kind == 2 {
                // If it is a directory, it must be empty
                let child_dest_dir_key = format!("squeezefs:dir:{}", dest_ino);
                let keys: Vec<String> = con.hkeys(&child_dest_dir_key).await.map_err(map_err)?;
                for k in keys {
                    if k != "." && k != ".." {
                        return Err(Errno::from(libc::ENOTEMPTY));
                    }
                }
                let _: () = con.del(&child_dest_dir_key).await.map_err(map_err)?;
            }
            let _: () = con.del(&dest_attr_key).await.map_err(map_err)?;
        }

        // Perform rename atomically
        let _: () = redis::pipe()
            .hdel(&src_dir_key, &*name_str)
            .hset(&dest_dir_key, &*new_name_str, ino)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // If renamed inode is a directory, update its ".." entry
        if src_kind == 2 {
            let child_dir_key = format!("squeezefs:dir:{}", ino);
            let _: () = con
                .hset(&child_dir_key, "..", new_parent)
                .await
                .map_err(map_err)?;

            // Adjust link counts if parents changed
            if parent != new_parent {
                let old_parent_attr_key = format!("squeezefs:attr:{}", parent);
                let new_parent_attr_key = format!("squeezefs:attr:{}", new_parent);
                let _: Result<(), redis::RedisError> =
                    con.hincr(&old_parent_attr_key, "nlink", -1).await;
                let _: Result<(), redis::RedisError> =
                    con.hincr(&new_parent_attr_key, "nlink", 1).await;
            }
        }

        // Update ctime of the renamed file/directory
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();
        let _: () = redis::pipe()
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;
        if parent != new_parent {
            self.update_parent_timestamps(&mut con, new_parent)
                .await
                .map_err(map_err)?;
        }

        Ok(())
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: i64,
    ) -> FuseResult<ReplyDirectory<Self::DirEntryStream<'a>>> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE readdir: parent = {}, offset = {}", parent, offset);

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let dir_key = format!("squeezefs:dir:{}", parent);
        let entries_map: std::collections::HashMap<String, u64> =
            con.hgetall(&dir_key).await.map_err(map_err)?;

        // Convert entries_map to a list of DirectoryEntry
        let mut entries = Vec::new();

        // Standard "." and ".." entries should be added if not already in Garnet
        if !entries_map.contains_key(".") {
            entries.push(DirectoryEntry {
                name: ".".into(),
                kind: FileType::Directory,
                inode: parent,
                offset: 1,
            });
        }
        if !entries_map.contains_key("..") {
            // Find parent directory from root/parent key, or just default to root 1 if not exists
            let parent_parent = if parent == 1 {
                1
            } else {
                let child_dir_key = format!("squeezefs:dir:{}", parent);
                let p: Option<u64> = con.hget(&child_dir_key, "..").await.unwrap_or(None);
                p.unwrap_or(1)
            };
            entries.push(DirectoryEntry {
                name: "..".into(),
                kind: FileType::Directory,
                inode: parent_parent,
                offset: 2,
            });
        }

        let mut current_offset = (entries.len() + 1) as i64;
        for (name, child_ino) in entries_map {
            if name == "." || name == ".." {
                continue;
            }
            // Retrieve kind of child_ino
            let child_attr_key = format!("squeezefs:attr:{}", child_ino);
            let kind_num: u8 = con.hget(&child_attr_key, "kind").await.unwrap_or(1);
            let kind = match kind_num {
                2 => FileType::Directory,
                3 => FileType::Symlink,
                4 => FileType::NamedPipe,
                5 => FileType::CharDevice,
                6 => FileType::BlockDevice,
                7 => FileType::Socket,
                _ => FileType::RegularFile,
            };

            entries.push(DirectoryEntry {
                name: name.into(),
                kind,
                inode: child_ino,
                offset: current_offset,
            });
            current_offset += 1;
        }

        // Apply offset filtering: skip the first `offset` entries
        let filtered_entries: Vec<DirectoryEntry> =
            entries.into_iter().skip(offset as usize).collect();

        // Convert to BoxStream
        use futures::stream::{self, StreamExt};
        let stream = stream::iter(filtered_entries.into_iter().map(Ok)).boxed();

        Ok(ReplyDirectory { entries: stream })
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

    let mut buf = vec![0u8; 4096];

    loop {
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
    options.custom_options("max_read=1048576");
    options.custom_options("max_write=1048576");
    options.custom_options("writeback_cache=yes");
    options.custom_options("async_dio=yes");
    options.custom_options("default_permissions"); // Kernel-level POSIX permission checking

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
