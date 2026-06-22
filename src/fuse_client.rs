use crate::backend::RustFsClient;
use crate::dlm::DlmClient;
use crate::error::SqueezefsError;
use crate::routing::DataRouter;
use fuse3::raw::{
    prelude::*,
    reply::{DirectoryEntry, FileAttr, ReplyCopyFileRange},
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
    pub router: DataRouter,
    dlm: DlmClient,
    uid: u32,
    gid: u32,
    active_leases: dashmap::DashMap<u64, crate::dlm::LockLease>,
    active_inode_locks: dashmap::DashMap<u64, std::sync::Arc<tokio::sync::Mutex<()>>>,
    pub attr_cache: dashmap::DashMap<u64, (FileAttr, std::time::Instant)>,
}

impl SqueezefsFilesystem {
    pub fn new(router: DataRouter, dlm: DlmClient, uid: u32, gid: u32) -> Self {
        Self {
            router,
            dlm,
            uid,
            gid,
            active_leases: dashmap::DashMap::new(),
            active_inode_locks: dashmap::DashMap::new(),
            attr_cache: dashmap::DashMap::new(),
        }
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
                .hset("squeezefs:attr:1", "uid", self.uid)
                .hset("squeezefs:attr:1", "gid", self.gid)
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

    fn get_inode_lock(&self, ino: u64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.active_inode_locks
            .entry(ino)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn get_or_acquire_lease(&self, ino: u64) -> Result<u64, SqueezefsError> {
        if let Some(lease) = self.active_leases.get(&ino) {
            return Ok(lease.fencing_token());
        }
        let file_path = format!("inode_{}", ino);
        let lease = self
            .dlm
            .acquire_lock(&file_path, None, Duration::from_secs(5))
            .await?;
        let token = lease.fencing_token();
        self.active_leases.insert(ino, lease);
        Ok(token)
    }

    async fn write_file_staged(
        &self,
        ino: u64,
        offset: u64,
        data: &[u8],
        _fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let block_size = self.router.block_size.load(Ordering::Relaxed);
        let start_block = offset / block_size;
        let end_block = (offset + data.len() as u64 - 1) / block_size;

        let active_dir = std::path::PathBuf::from("/tmp/squeezefs_staging")
            .join("active_writes")
            .join(format!("inode_{}", ino));

        tokio::fs::create_dir_all(&active_dir).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Failed to create active writes dir: {:?}",
                e
            )))
        })?;

        let mut data_cursor = 0usize;
        for b in start_block..=end_block {
            let b_start_offset = b * block_size;
            let b_end_offset = b_start_offset + block_size;

            let write_start = std::cmp::max(offset, b_start_offset);
            let write_end = std::cmp::min(offset + data.len() as u64, b_end_offset);
            let slice_len = (write_end - write_start) as usize;

            let file_data_slice = &data[data_cursor..data_cursor + slice_len];
            data_cursor += slice_len;

            let block_file_path = active_dir.join(format!("block_{}", b));

            // 1. Initialize block file on disk if it doesn't exist
            if !block_file_path.exists() {
                let file_path = format!("inode_{}", ino);

                // Try cache first
                let mut block_map_id_opt = None;
                if let Some(entry) = self.router.metadata_cache.get(&file_path) {
                    if entry.cached_at.elapsed() < Duration::from_secs(1) {
                        block_map_id_opt = entry.block_map_id.clone();
                    }
                }

                // If miss, query Garnet
                let block_map_id = match block_map_id_opt {
                    Some(id) => Some(id),
                    None => {
                        let meta_key = format!("metadata:{}", file_path);
                        let mut con = self.dlm.get_connection().await?;
                        let id_opt: Option<String> = con.hget(&meta_key, "block_map_id").await?;
                        id_opt
                    }
                };

                let mut existing_block_data = Vec::new();
                if let Some(block_map_id) = block_map_id {
                    let mut old_block_key = None;

                    // Try block_map_cache first
                    let cache_key = (block_map_id.clone(), b as u32);
                    if let Some(entry) = self.router.block_map_cache.get(&cache_key) {
                        let (bk, cached_at) = entry.value();
                        if cached_at.elapsed() < Duration::from_secs(1) {
                            old_block_key = Some(bk.clone());
                        }
                    }

                    // If miss, query Garnet
                    let old_block_key = match old_block_key {
                        Some(key) => Some(key),
                        None => {
                            let block_map_key = format!("block_map:{}", block_map_id);
                            let mut con = self.dlm.get_connection().await?;
                            let key_opt: Option<String> =
                                con.hget(&block_map_key, b.to_string()).await?;
                            key_opt
                        }
                    };

                    if let Some(bk) = old_block_key {
                        existing_block_data = if let Some(cached) =
                            self.router.cache.nvme.get_cached_read_block(&bk)
                        {
                            cached
                        } else {
                            let (be_id, real_key) = crate::backend::parse_backend_and_key(&bk);
                            self.router.backend.get_object(&be_id, &real_key).await?
                        };
                    }
                }

                tokio::fs::write(&block_file_path, &existing_block_data)
                    .await
                    .map_err(|e| {
                        SqueezefsError::Io(std::io::Error::other(format!(
                            "Failed to initialize staging block file: {:?}",
                            e
                        )))
                    })?;
            }

            // 2. Perform seek and write range directly on disk
            use tokio::fs::OpenOptions;
            use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};

            let mut file = OpenOptions::new()
                .write(true)
                .open(&block_file_path)
                .await
                .map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Failed to open staging block file for write: {:?}",
                        e
                    )))
                })?;

            let rel_start = write_start - b_start_offset;
            file.seek(SeekFrom::Start(rel_start)).await.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Failed to seek staging block file: {:?}",
                    e
                )))
            })?;

            file.write_all(file_data_slice).await.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Failed to write staging block file slice: {:?}",
                    e
                )))
            })?;
        }

        Ok(())
    }

    async fn flush_active_blocks(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let active_dir = std::path::PathBuf::from("/tmp/squeezefs_staging")
            .join("active_writes")
            .join(format!("inode_{}", ino));

        if !active_dir.exists() {
            return Ok(());
        }

        let mut entries = tokio::fs::read_dir(&active_dir).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Failed to read active writes dir: {:?}",
                e
            )))
        })?;

        let mut tasks = Vec::new();
        let file_path = format!("inode_{}", ino);
        let meta_key = format!("metadata:{}", file_path);

        let mut con = self.dlm.get_connection().await?;
        let block_map_id_opt: Option<String> = con.hget(&meta_key, "block_map_id").await?;
        let mut block_map_id = block_map_id_opt.unwrap_or_default();
        if block_map_id.is_empty() {
            block_map_id = uuid::Uuid::new_v4().to_string();
            let _: () = con.hset(&meta_key, "block_map_id", &block_map_id).await?;
        }

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Failed to read entry: {:?}",
                e
            )))
        })? {
            let path = entry.path();
            if path.is_file() {
                let file_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if file_name.starts_with("block_") {
                    let b_str = file_name.trim_start_matches("block_");
                    if let Ok(b) = b_str.parse::<u32>() {
                        let block_data = tokio::fs::read(&path).await.map_err(|e| {
                            SqueezefsError::Io(std::io::Error::other(format!(
                                "Failed to read block file for upload: {:?}",
                                e
                            )))
                        })?;

                        let backend_clone = self.router.backend.clone();
                        let dlm_clone = self.dlm.clone();
                        let block_map_id_clone = block_map_id.clone();
                        let router_clone = self.router.clone();

                        tasks.push(tokio::spawn(async move {
                            let file_uuid = uuid::Uuid::new_v4().to_string();
                            let block_write_uuid = uuid::Uuid::new_v4().to_string();
                            let new_block_key =
                                format!("blocks/{}/block_{}_{}", file_uuid, b, block_write_uuid);

                            backend_clone
                                .put_object(&new_block_key, block_data, fencing_token)
                                .await?;

                            let mut con = dlm_clone.get_connection().await?;
                            let block_map_key = format!("block_map:{}", block_map_id_clone);
                            let refcounts_key = "squeezefs:block_refcounts";

                            let old_block_key: Option<String> =
                                con.hget(&block_map_key, b.to_string()).await?;

                            let mut pipe = redis::pipe();
                            pipe.hset(refcounts_key, &new_block_key, 1).hset(
                                &block_map_key,
                                b.to_string(),
                                &new_block_key,
                            );
                            let _: () = pipe.query_async(&mut con).await?;

                            // Update local block_map_cache
                            router_clone.block_map_cache.insert(
                                (block_map_id_clone.clone(), b),
                                (new_block_key.clone(), std::time::Instant::now()),
                            );

                            if let Some(bk) = old_block_key {
                                let old_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
                                if let Some(mut r) = old_ref {
                                    r -= 1;
                                    if r <= 0 {
                                        let _: () = redis::pipe()
                                            .hdel(refcounts_key, &bk)
                                            .query_async(&mut con)
                                            .await?;
                                        let (be_id, real_key) =
                                            crate::backend::parse_backend_and_key(&bk);
                                        let _ =
                                            backend_clone.delete_object(&be_id, &real_key).await;
                                    } else {
                                        let _: () = con.hset(refcounts_key, &bk, r).await?;
                                    }
                                } else {
                                    let (be_id, real_key) =
                                        crate::backend::parse_backend_and_key(&bk);
                                    let _ = backend_clone.delete_object(&be_id, &real_key).await;
                                }
                            }
                            Ok::<_, SqueezefsError>(())
                        }));
                    }
                }
            }
        }

        for task in tasks {
            task.await.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Parallel block upload task panicked: {:?}",
                    e
                )))
            })??;
        }

        tokio::fs::remove_dir_all(&active_dir).await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Failed to remove active writes dir: {:?}",
                e
            )))
        })?;

        Ok(())
    }

    async fn get_attr_internal(&self, ino: u64) -> Result<FileAttr, SqueezefsError> {
        if let Some(entry) = self.attr_cache.get(&ino) {
            let (attr, cached_at) = entry.value();
            if cached_at.elapsed() < Duration::from_secs(1) {
                return Ok(*attr);
            }
        }

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
            .unwrap_or(self.uid);
        let gid = fields
            .get("gid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(self.gid);
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

        let attr = FileAttr {
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
        };
        self.attr_cache
            .insert(ino, (attr, std::time::Instant::now()));
        Ok(attr)
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

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let format_exists: bool = con.exists("squeezefs:format").await.map_err(map_err)?;

        if !format_exists {
            let default_block_size = 4 * 1024 * 1024;
            let default_capacity = 1024 * 1024 * 1024 * 1024 * 1024;
            info!("Volume not formatted. Performing auto-format on mount...");
            let _: () = redis::pipe()
                .hset("squeezefs:format", "name", "squeezefs")
                .hset("squeezefs:format", "block_size", default_block_size)
                .hset("squeezefs:format", "capacity", default_capacity)
                .hset("squeezefs:format", "version", 1) // ABI version
                .hset("squeezefs:format", "mem_cache_size", "1GB")
                .hset("squeezefs:format", "disk_cache_size", "10GB")
                .hset("squeezefs:format", "disk_cache_paths", "")
                .query_async(&mut con)
                .await
                .map_err(map_err)?;
        }

        let version_str: Option<String> = con
            .hget("squeezefs:format", "version")
            .await
            .map_err(map_err)?;
        let version: u64 = version_str.and_then(|v| v.parse().ok()).unwrap_or(1);
        if version > 1 {
            error!("Database ABI version ({}) is higher than client supported version (1). Rejecting mount.", version);
            return Err(Errno::from(libc::EPROTO));
        }

        let block_size_str: Option<String> = con
            .hget("squeezefs:format", "block_size")
            .await
            .map_err(map_err)?;
        let block_size: u64 = block_size_str
            .and_then(|v| v.parse().ok())
            .unwrap_or(4 * 1024 * 1024);
        self.router.set_block_size(block_size);

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
        let read_future = self.router.read_file_range(&file_path, offset, size);
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

        Ok(ReplyData {
            data: read_result.into(),
        })
    }

    async fn write(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> FuseResult<ReplyWrite> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);

        // 1. Acquire local inode lock
        let lock = self.get_inode_lock(ino);
        let _guard = lock.lock().await;

        // 2. Get or acquire lease (fencing token)
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // 3. Write data to local NVMe staging blocks
        self.write_file_staged(ino, offset, data, fencing_token)
            .await
            .map_err(map_squeezefs_err)?;

        let bytes_written = data.len() as u32;

        // 4. Update file attributes and used bytes
        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let attr_key = format!("squeezefs:attr:{}", ino);

        let old_size = if let Some(entry) = self.attr_cache.get(&ino) {
            entry.value().0.size
        } else {
            let old_size_opt: Option<u64> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            old_size_opt.unwrap_or(0)
        };

        let new_size = std::cmp::max(old_size, offset + bytes_written as u64);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let mut pipe = redis::pipe();
        pipe.hset(&attr_key, "size", new_size)
            .hset(&attr_key, "mtime_sec", sec)
            .hset(&attr_key, "mtime_nsec", nsec)
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec);

        if new_size > old_size {
            let diff = new_size - old_size;
            pipe.incr("squeezefs:used_bytes", diff);
        }
        let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;

        // Update local attr_cache
        if let Some(mut entry) = self.attr_cache.get_mut(&ino) {
            entry.value_mut().0.size = new_size;
            entry.value_mut().0.mtime = Timestamp::new(sec, nsec);
            entry.value_mut().0.ctime = Timestamp::new(sec, nsec);
            entry.value_mut().1 = std::time::Instant::now();
        }

        // Invalidate router metadata_cache to force reload of the new size on next read
        let file_path = format!("inode_{}", ino);
        self.router.metadata_cache.remove(&file_path);

        Ok(ReplyWrite {
            written: bytes_written,
        })
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

        // Invalidate caches
        self.attr_cache.remove(&ino);
        self.attr_cache.remove(&parent);
        let file_path = format!("inode_{}", ino);
        self.router.metadata_cache.remove(&file_path);

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

        let mut old_size = 0u64;
        if set_attr.size.is_some() {
            let old_size_opt: Option<u64> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            old_size = old_size_opt.unwrap_or(0);
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
            if size > old_size {
                let diff = size - old_size;
                pipe.incr("squeezefs:used_bytes", diff);
            } else if size < old_size {
                let diff = old_size - size;
                pipe.decr("squeezefs:used_bytes", diff);
            }
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

        // Invalidate cached attributes and router metadata
        self.attr_cache.remove(&ino);
        let file_path = format!("inode_{}", ino);
        self.router.metadata_cache.remove(&file_path);

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
            let file_size_opt: Option<u64> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            let file_size = file_size_opt.unwrap_or(0);
            let file_path = format!("inode_{}", ino);
            let inline_key = format!("inline_data:{}", file_path);
            let meta_key = format!("metadata:{}", file_path);
            let symlink_key = format!("squeezefs:symlink:{}", ino);

            self.router
                .delete_file(&file_path, &mut con)
                .await
                .map_err(map_squeezefs_err)?;

            let _: () = redis::pipe()
                .del(&attr_key)
                .del(&inline_key)
                .del(&meta_key)
                .del(&symlink_key)
                .decr("squeezefs:used_bytes", file_size)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;
        }

        // Invalidate attr_cache and router metadata_cache
        self.attr_cache.remove(&ino);
        self.attr_cache.remove(&parent);
        let file_path = format!("inode_{}", ino);
        self.router.metadata_cache.remove(&file_path);

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

        // Invalidate caches
        self.attr_cache.remove(&ino);
        self.attr_cache.remove(&parent);
        if parent != new_parent {
            self.attr_cache.remove(&new_parent);
        }
        if let Some(dest_ino) = dest_ino_opt {
            self.attr_cache.remove(&dest_ino);
            let dest_file_path = format!("inode_{}", dest_ino);
            self.router.metadata_cache.remove(&dest_file_path);
        }
        let file_path = format!("inode_{}", ino);
        self.router.metadata_cache.remove(&file_path);

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

    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        parent: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> FuseResult<ReplyDirectoryPlus<Self::DirEntryPlusStream<'a>>> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE readdirplus: parent = {}, offset = {}", parent, offset);

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let dir_key = format!("squeezefs:dir:{}", parent);
        let entries_map: std::collections::HashMap<String, u64> =
            con.hgetall(&dir_key).await.map_err(map_err)?;

        let mut entries = Vec::new();

        // Standard "." and ".." entries
        if !entries_map.contains_key(".") {
            let attr = self
                .get_attr_internal(parent)
                .await
                .map_err(map_squeezefs_err)?;
            entries.push(DirectoryEntryPlus {
                name: ".".into(),
                kind: FileType::Directory,
                inode: parent,
                generation: 1,
                attr,
                entry_ttl: Duration::from_secs(1),
                attr_ttl: Duration::from_secs(1),
                offset: 1,
            });
        }
        if !entries_map.contains_key("..") {
            let parent_parent = if parent == 1 {
                1
            } else {
                let child_dir_key = format!("squeezefs:dir:{}", parent);
                let p: Option<u64> = con.hget(&child_dir_key, "..").await.unwrap_or(None);
                p.unwrap_or(1)
            };
            let attr = self
                .get_attr_internal(parent_parent)
                .await
                .map_err(map_squeezefs_err)?;
            entries.push(DirectoryEntryPlus {
                name: "..".into(),
                kind: FileType::Directory,
                inode: parent_parent,
                generation: 1,
                attr,
                entry_ttl: Duration::from_secs(1),
                attr_ttl: Duration::from_secs(1),
                offset: 2,
            });
        }

        let mut current_offset = (entries.len() + 1) as i64;
        for (name, child_ino) in entries_map {
            if name == "." || name == ".." {
                continue;
            }
            let attr = match self.get_attr_internal(child_ino).await {
                Ok(a) => a,
                Err(e) => {
                    error!(
                        "readdirplus failed to get attr for child {}: {:?}",
                        child_ino, e
                    );
                    continue;
                }
            };
            let kind = attr.kind;

            entries.push(DirectoryEntryPlus {
                name: name.into(),
                kind,
                inode: child_ino,
                generation: 1,
                attr,
                entry_ttl: Duration::from_secs(1),
                attr_ttl: Duration::from_secs(1),
                offset: current_offset,
            });
            current_offset += 1;
        }

        let filtered_entries: Vec<DirectoryEntryPlus> =
            entries.into_iter().skip(offset as usize).collect();

        use futures::stream::{self, StreamExt};
        let stream = stream::iter(filtered_entries.into_iter().map(Ok)).boxed();

        Ok(ReplyDirectoryPlus { entries: stream })
    }

    #[allow(clippy::too_many_arguments)]
    async fn copy_file_range(
        &self,
        _req: Request,
        inode: u64,
        _fh_in: u64,
        off_in: u64,
        inode_out: u64,
        _fh_out: u64,
        off_out: u64,
        length: u64,
        _flags: u64,
    ) -> FuseResult<ReplyCopyFileRange> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE copy_file_range: src_ino = {}, off_in = {}, dest_ino = {}, off_out = {}, length = {}",
            inode, off_in, inode_out, off_out, length
        );

        let src_path = format!("inode_{}", inode);
        let dest_path = format!("inode_{}", inode_out);

        // 1. Acquire locks on both files to ensure consistency
        let _src_lease = match self
            .dlm
            .acquire_lock(&src_path, None, Duration::from_secs(5))
            .await
        {
            Ok(l) => l,
            Err(_) => return Err(Errno::from(libc::EAGAIN)),
        };
        let dest_lease = match self
            .dlm
            .acquire_lock(&dest_path, None, Duration::from_secs(5))
            .await
        {
            Ok(l) => l,
            Err(_) => return Err(Errno::from(libc::EAGAIN)),
        };

        // 2. Read sizes to check if we can perform metadata clone
        let src_size = self
            .router
            .get_file_size(&src_path)
            .await
            .map_err(map_squeezefs_err)?;
        let dest_size = self.router.get_file_size(&dest_path).await.unwrap_or(0);

        if off_in == 0 && off_out == 0 && length >= src_size && dest_size == 0 {
            // Drop locks before cloning, clone_file will re-acquire them.
            drop(_src_lease);
            drop(dest_lease);

            self.router
                .clone_file(&src_path, &dest_path)
                .await
                .map_err(map_squeezefs_err)?;

            // Update destination attributes size and times in Garnet
            if let Ok(mut con) = self.dlm.get_connection().await {
                let attr_key = format!("squeezefs:attr:{}", inode_out);
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO);
                let sec = now.as_secs() as i64;
                let nsec = now.subsec_nanos();
                let _: Result<(), redis::RedisError> = redis::pipe()
                    .hset(&attr_key, "size", src_size)
                    .hset(&attr_key, "mtime_sec", sec)
                    .hset(&attr_key, "mtime_nsec", nsec)
                    .hset(&attr_key, "ctime_sec", sec)
                    .hset(&attr_key, "ctime_nsec", nsec)
                    .query_async(&mut con)
                    .await;
            }

            return Ok(ReplyCopyFileRange { copied: src_size });
        }

        // 3. General copy: read range from source, write to destination
        let src_data = self
            .router
            .read_file(&src_path)
            .await
            .map_err(map_squeezefs_err)?;
        if off_in >= src_data.len() as u64 {
            return Ok(ReplyCopyFileRange { copied: 0 });
        }

        let start = off_in as usize;
        let end = std::cmp::min((off_in + length) as usize, src_data.len());
        let chunk = &src_data[start..end];

        if chunk.is_empty() {
            return Ok(ReplyCopyFileRange { copied: 0 });
        }

        // Perform write to destination
        self.router
            .write_file(&dest_path, off_out, chunk, dest_lease.fencing_token())
            .await
            .map_err(map_squeezefs_err)?;

        // Update destination size and times in Garnet
        let copied_len = chunk.len() as u64;
        let new_dest_size = std::cmp::max(dest_size, off_out + copied_len);

        if let Ok(mut con) = self.dlm.get_connection().await {
            let attr_key = format!("squeezefs:attr:{}", inode_out);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();
            let _: Result<(), redis::RedisError> = redis::pipe()
                .hset(&attr_key, "size", new_dest_size)
                .hset(&attr_key, "mtime_sec", sec)
                .hset(&attr_key, "mtime_nsec", nsec)
                .hset(&attr_key, "ctime_sec", sec)
                .hset(&attr_key, "ctime_nsec", nsec)
                .query_async(&mut con)
                .await;
        }

        Ok(ReplyCopyFileRange { copied: copied_len })
    }

    async fn statfs(&self, _req: Request, _ino: u64) -> FuseResult<ReplyStatFs> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let used_bytes_opt: Option<u64> = con.get("squeezefs:used_bytes").await.map_err(map_err)?;
        let used_bytes = used_bytes_opt.unwrap_or(0);

        let bsize = 4096;
        let format_exists: bool = con.exists("squeezefs:format").await.map_err(map_err)?;
        let capacity = if format_exists {
            let cap_str: Option<String> = con
                .hget("squeezefs:format", "capacity")
                .await
                .map_err(map_err)?;
            cap_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1024 * 1024 * 1024 * 1024 * 1024) // 1PB
        } else {
            1024 * 1024 * 1024 * 1024 * 1024 // 1PB
        };

        let total_blocks = capacity / bsize as u64;
        let used_blocks = used_bytes.div_ceil(bsize as u64);
        let bfree = total_blocks.saturating_sub(used_blocks);

        Ok(ReplyStatFs {
            blocks: total_blocks,
            bfree,
            bavail: bfree,
            files: 1_000_000_000,
            ffree: 999_999_000,
            bsize,
            namelen: 255,
            frsize: bsize,
        })
    }

    async fn flush(&self, _req: Request, ino: u64, _fh: u64, _lock_owner: u64) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Flush: ino = {}", ino);

        // 1. Acquire local inode lock
        let lock = self.get_inode_lock(ino);
        let _guard = lock.lock().await;

        // 2. Get or acquire lease (fencing token)
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // 3. Flush the staging blocks concurrently to backend (S3/RustFS)
        self.flush_active_blocks(ino, fencing_token)
            .await
            .map_err(map_squeezefs_err)?;

        Ok(())
    }

    async fn release(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Release: ino = {}", ino);

        // Acquire local inode lock
        let lock = self.get_inode_lock(ino);
        let _guard = lock.lock().await;

        // If there's a cached lease, release it and remove it from our active_leases map
        if let Some((_, lease)) = self.active_leases.remove(&ino) {
            let _ = lease.release().await;
        }

        // Also clean up local inode lock if no longer needed
        self.active_inode_locks.remove(&ino);

        Ok(())
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
    uid: u32,
    gid: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = MountOptions::default();
    options.uid(uid);
    options.gid(gid);
    options.allow_other(true);

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

    // Check for stale FUSE mount (ENOTCONN or EIO)
    #[cfg(target_os = "linux")]
    {
        let check_metadata = std::fs::metadata(&mount_path);
        let is_stale = match check_metadata {
            Err(e) => {
                let os_err = e.raw_os_error();
                os_err == Some(107)
                    || os_err == Some(5)
                    || e.kind() == std::io::ErrorKind::NotConnected
            }
            _ => false,
        };

        if is_stale {
            error!(
                "Stale mount point detected at {:?}.\nTo resolve this, please manually unmount it by running:\n    sudo umount -l {:?}",
                mount_path, mount_path
            );
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Transport endpoint is not connected",
            )));
        }
    }

    // Spawns the mount loop using fuse3 Session
    let session = fuse3::raw::Session::new(options)
        .mount(fs, mount_path)
        .await?;

    session.await?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn format_volume(
    redis_url: &str,
    name: &str,
    block_size: u64,
    capacity: u64,
    mem_cache_size: Option<&str>,
    disk_cache_size: Option<&str>,
    disk_cache_paths: Option<&[std::path::PathBuf]>,
    s3_endpoint: Option<&str>,
    s3_access_key: Option<&str>,
    s3_secret_key: Option<&str>,
    s3_bucket: Option<&str>,
) -> Result<(), SqueezefsError> {
    let client = redis::Client::open(redis_url)?;
    let mut con = client.get_multiplexed_tokio_connection().await?;
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    let mem_size = mem_cache_size.unwrap_or("1GB").to_string();
    let disk_size = disk_cache_size.unwrap_or("10GB").to_string();
    let paths_str = disk_cache_paths
        .map(|paths| {
            paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();

    let mut pipe = redis::pipe();
    pipe.hset("squeezefs:format", "name", name)
        .hset("squeezefs:format", "block_size", block_size)
        .hset("squeezefs:format", "capacity", capacity)
        .hset("squeezefs:format", "version", 1) // ABI version
        .hset("squeezefs:format", "mem_cache_size", mem_size)
        .hset("squeezefs:format", "disk_cache_size", disk_size)
        .hset("squeezefs:format", "disk_cache_paths", paths_str)
        .hset("squeezefs:format", "active_write_backend", "backend_0");

    let default_endpoint = s3_endpoint.unwrap_or("");
    let default_access_key = s3_access_key.unwrap_or("admin");
    let default_secret_key = s3_secret_key.unwrap_or("password");
    let default_bucket = s3_bucket.unwrap_or("squeezefs-data");

    let backend_json = serde_json::json!({
        "endpoint": default_endpoint,
        "access_key": default_access_key,
        "secret_key": default_secret_key,
        "bucket": default_bucket,
    })
    .to_string();

    pipe.hset("squeezefs:backends", "backend_0", backend_json);

    if !default_endpoint.is_empty() {
        pipe.hset("squeezefs:format", "s3_endpoint", default_endpoint)
            .hset("squeezefs:format", "s3_bucket", default_bucket);
    }

    let _: () = pipe.query_async(&mut con).await?;

    // Create/initialize bucket on S3 if endpoint is provided
    if !default_endpoint.is_empty() {
        let backend_client = RustFsClient::new_with_local_ips(
            Vec::new(),
            Some(default_endpoint.to_string()),
            Some(default_access_key.to_string()),
            Some(default_secret_key.to_string()),
            Some(default_bucket.to_string()),
        )
        .await;

        backend_client.init_bucket().await?;
    }

    Ok(())
}

pub async fn get_volume_status(redis_url: &str) -> Result<serde_json::Value, SqueezefsError> {
    let client = redis::Client::open(redis_url)?;
    let mut con = client.get_multiplexed_tokio_connection().await?;
    let fields: std::collections::HashMap<String, String> = con.hgetall("squeezefs:format").await?;
    if fields.is_empty() {
        return Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Volume not formatted",
        )));
    }
    let name = fields.get("name").cloned().unwrap_or_default();
    let block_size: u64 = fields
        .get("block_size")
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096);
    let capacity: u64 = fields
        .get("capacity")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mem_cache_size = fields.get("mem_cache_size").cloned().unwrap_or_default();
    let disk_cache_size = fields.get("disk_cache_size").cloned().unwrap_or_default();
    let disk_cache_paths_str = fields.get("disk_cache_paths").cloned().unwrap_or_default();
    let disk_cache_paths: Vec<String> = if disk_cache_paths_str.is_empty() {
        vec![]
    } else {
        disk_cache_paths_str
            .split(',')
            .map(|s| s.to_string())
            .collect()
    };

    let s3_endpoint = fields.get("s3_endpoint").cloned().unwrap_or_default();
    let s3_bucket = fields.get("s3_bucket").cloned().unwrap_or_default();
    let active_write_backend = fields
        .get("active_write_backend")
        .cloned()
        .unwrap_or_default();

    Ok(serde_json::json!({
        "Setting": {
            "Name": name,
            "BlockSize": block_size,
            "Capacity": capacity,
            "MemCacheSize": mem_cache_size,
            "DiskCacheSize": disk_cache_size,
            "DiskCachePaths": disk_cache_paths,
            "S3Endpoint": s3_endpoint,
            "S3Bucket": s3_bucket,
            "ActiveWriteBackend": active_write_backend,
        }
    }))
}
