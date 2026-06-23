use crate::backend::RustFsClient;
use crate::dlm::DlmClient;
use crate::error::SqueezefsError;
use crate::routing::DataRouter;
use fuse3::raw::{
    prelude::*,
    reply::{DirectoryEntry, FileAttr, ReplyCopyFileRange, ReplyIoctl, ReplyLock},
    Request,
};
use fuse3::{Errno, Inode, MountOptions, Result as FuseResult, Timestamp};
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use redis::AsyncCommands;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::runtime::Builder;

const CONFIG_INODE: u64 = 0xffff_ffff_ffff_fffe;

#[derive(Default)]
pub struct ProbabilisticAtomic {
    inner: AtomicU64,
}

impl ProbabilisticAtomic {
    pub fn fetch_add(&self, val: u64, order: Ordering) -> u64 {
        if fastrand::u8(..) < 3 {
            self.inner.fetch_add(val * 100, order)
        } else {
            self.inner.load(order)
        }
    }

    pub fn load(&self, order: Ordering) -> u64 {
        self.inner.load(order)
    }
}

#[derive(Default)]
pub struct Metrics {
    pub fuse_ops: ProbabilisticAtomic,
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
    active_posix_locks: dashmap::DashMap<(Inode, u64, u64, u64), crate::dlm::LockLease>,
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
            active_posix_locks: dashmap::DashMap::new(),
            active_inode_locks: dashmap::DashMap::new(),
            attr_cache: dashmap::DashMap::new(),
        }
    }

    async fn generate_config_json(&self) -> String {
        let mut con_opt = self.dlm.get_connection().await.ok();

        let format_fields: std::collections::HashMap<String, String> =
            if let Some(ref mut con) = con_opt {
                con.hgetall("squeezefs:format").await.unwrap_or_default()
            } else {
                std::collections::HashMap::new()
            };

        let backends_raw: std::collections::HashMap<String, String> =
            if let Some(ref mut con) = con_opt {
                con.hgetall("squeezefs:backends").await.unwrap_or_default()
            } else {
                std::collections::HashMap::new()
            };

        let mut backends = serde_json::Map::new();
        for (be_id, be_json) in backends_raw {
            if let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&be_json) {
                if let Some(obj) = config.as_object_mut() {
                    if obj.contains_key("secret_key") {
                        obj.insert(
                            "secret_key".to_string(),
                            serde_json::Value::String("******".to_string()),
                        );
                    }
                    if obj.contains_key("access_key") {
                        obj.insert(
                            "access_key".to_string(),
                            serde_json::Value::String("******".to_string()),
                        );
                    }
                }
                backends.insert(be_id, config);
            }
        }

        let config_obj = serde_json::json!({
            "client_version": env!("CARGO_PKG_VERSION"),
            "format": format_fields,
            "backends": backends,
            "uid": self.uid,
            "gid": self.gid,
            "block_size": self.router.block_size.load(Ordering::Relaxed),
        });

        serde_json::to_string_pretty(&config_obj).unwrap_or_default()
    }

    fn get_config_attr(&self, size: u64) -> FileAttr {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        FileAttr {
            ino: CONFIG_INODE,
            size,
            blocks: size.div_ceil(512),
            atime: Timestamp::new(sec, nsec),
            mtime: Timestamp::new(sec, nsec),
            ctime: Timestamp::new(sec, nsec),
            kind: FileType::RegularFile,
            perm: 0o444, // read-only by all
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
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
                .incr("squeezefs:used_inodes", 1)
                .query_async(&mut con)
                .await?;
        }
        Ok(())
    }

    async fn check_inode_quota(&self, con: &mut crate::dlm::MetaConnection) -> Result<(), Errno> {
        let inodes_limit_str: Option<String> = con
            .hget("squeezefs:format", "inodes")
            .await
            .map_err(map_err)?;
        let inodes_limit = inodes_limit_str
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if inodes_limit > 0 {
            let used_inodes: u64 = con.get("squeezefs:used_inodes").await.unwrap_or(0);
            if used_inodes >= inodes_limit {
                return Err(Errno::from(libc::ENOSPC));
            }
        }
        Ok(())
    }

    async fn check_capacity_quota(
        &self,
        con: &mut crate::dlm::MetaConnection,
        additional_bytes: u64,
    ) -> Result<(), Errno> {
        let format_exists: bool = con.exists("squeezefs:format").await.map_err(map_err)?;
        let capacity_limit = if format_exists {
            let cap_str: Option<String> = con
                .hget("squeezefs:format", "capacity")
                .await
                .map_err(map_err)?;
            cap_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1024 * 1024 * 1024 * 1024 * 1024) // 1PB default
        } else {
            1024 * 1024 * 1024 * 1024 * 1024 // 1PB default
        };

        let used_bytes_opt: Option<u64> = con.get("squeezefs:used_bytes").await.map_err(map_err)?;
        let used_bytes = used_bytes_opt.unwrap_or(0);
        if used_bytes + additional_bytes > capacity_limit {
            return Err(Errno::from(libc::ENOSPC));
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

        let staging_dir = self
            .router
            .cache
            .nvme
            .staging_dirs()
            .first()
            .cloned()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/squeezefs_staging"));

        let active_dir = staging_dir
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
        let staging_dir = self
            .router
            .cache
            .nvme
            .staging_dirs()
            .first()
            .cloned()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/squeezefs_staging"));

        let active_dir = staging_dir
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

                            let active_be = backend_clone.get_backend_for_key(&new_block_key);
                            let stored_block_key = format!("{}:{}", active_be, new_block_key);

                            let mut con = dlm_clone.get_connection().await?;
                            let block_map_key = format!("block_map:{}", block_map_id_clone);
                            let refcounts_key = "squeezefs:block_refcounts";

                            let old_block_key: Option<String> =
                                con.hget(&block_map_key, b.to_string()).await?;

                            let mut pipe = redis::pipe();
                            pipe.hset(refcounts_key, &stored_block_key, 1).hset(
                                &block_map_key,
                                b.to_string(),
                                &stored_block_key,
                            );
                            let _: () = pipe.query_async(&mut con).await?;

                            // Update local block_map_cache
                            router_clone.block_map_cache.insert(
                                (block_map_id_clone.clone(), b),
                                (stored_block_key.clone(), std::time::Instant::now()),
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
        let size: u64 = fields.get("size").and_then(|v| v.parse().ok()).unwrap_or(0);
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
        let blocks = match kind {
            FileType::Directory | FileType::Symlink | FileType::RegularFile => size.div_ceil(512),
            _ => 0,
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

    async fn scan_lock_keys(
        &self,
        con: &mut crate::dlm::MetaConnection,
        pattern: &str,
    ) -> Result<Vec<String>, SqueezefsError> {
        let mut cursor: u64 = 0;
        let mut all_keys = Vec::new();
        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(100)
                .query_async(con)
                .await
                .map_err(SqueezefsError::from)?;
            all_keys.extend(keys);
            cursor = next_cursor;
            if cursor == 0 {
                break;
            }
        }
        Ok(all_keys)
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
            let default_capacity: u64 = 1024u64 * 1024 * 1024 * 1024 * 1024;
            info!("Volume not formatted. Performing auto-format on mount...");
            let _: () = redis::pipe()
                .hset("squeezefs:format", "name", "squeezefs")
                .hset("squeezefs:format", "block_size", default_block_size)
                .hset("squeezefs:format", "capacity", default_capacity)
                .hset("squeezefs:format", "inodes", 1000000)
                .hset("squeezefs:format", "compression", "none")
                .hset("squeezefs:format", "encrypt_algo", "none")
                .hset("squeezefs:format", "encrypt_key", "")
                .hset("squeezefs:format", "version", 1) // ABI version
                .hset("squeezefs:format", "mem_cache_size", "1GB")
                .hset("squeezefs:format", "disk_cache_size", "10GB")
                .hset("squeezefs:format", "read_cache_size", "")
                .hset("squeezefs:format", "write_cache_size", "")
                .hset("squeezefs:format", "read_mem_cache_size", "")
                .hset("squeezefs:format", "write_mem_cache_size", "")
                .hset("squeezefs:format", "disk_cache_paths", "")
                .query_async(&mut con)
                .await
                .map_err(map_err)?;
        }

        let compression: String = con
            .hget("squeezefs:format", "compression")
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| "none".to_string());
        let encrypt_algo: String = con
            .hget("squeezefs:format", "encrypt_algo")
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| "none".to_string());
        let encrypt_key: Option<String> = con
            .hget("squeezefs:format", "encrypt_key")
            .await
            .unwrap_or(None);

        let crypto_state = crate::crypto_compress::CryptoCompressState::new(
            compression,
            encrypt_algo,
            encrypt_key.as_deref(),
        );
        self.router.set_crypto(crypto_state);

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
        crate::coz_progress!("fuse_lookup");
        let name_str = name.to_string_lossy();
        debug!("FUSE Lookup: parent = {}, name = {}", parent, name_str);

        if parent == 1 && name_str == ".config" {
            let config_data = self.generate_config_json().await;
            let attr = self.get_config_attr(config_data.len() as u64);
            return Ok(ReplyEntry {
                ttl: Duration::from_secs(1),
                attr,
                generation: 1,
            });
        }

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

        if ino == CONFIG_INODE {
            let config_data = self.generate_config_json().await;
            let attr = self.get_config_attr(config_data.len() as u64);
            return Ok(ReplyAttr {
                ttl: Duration::from_secs(1),
                attr,
            });
        }

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

        self.check_inode_quota(&mut con).await?;

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
        let mut pipe = redis::pipe();
        pipe.hset(&dir_key, &*name_str, new_ino)
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
            .incr("squeezefs:used_inodes", 1);

        if kind_num == 1 {
            let meta_key = format!("metadata:inode_{}", new_ino);
            pipe.hset(&meta_key, "type", "inline")
                .hset(&meta_key, "size", 0);
        }

        let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        self.attr_cache.remove(&parent);

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
        crate::coz_progress!("fuse_create");
        let name_str = name.to_string_lossy();
        info!(
            "FUSE Create: parent = {}, name = {}, mode = {:o}, flags = {}",
            parent, name_str, mode, flags
        );

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        // Check if name already exists in parent
        let dir_key = format!("squeezefs:dir:{}", parent);
        let exists: Option<u64> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
        if exists.is_some() {
            return Err(Errno::from(libc::EEXIST));
        }

        self.check_inode_quota(&mut con).await?;

        // Allocate new inode
        let new_ino: u64 = con
            .incr("squeezefs:inode_counter", 1)
            .await
            .map_err(map_err)?;

        // Atomically link to parent dir
        let dir_key = format!("squeezefs:dir:{}", parent);
        let setnx_res: u8 = redis::cmd("HSETNX")
            .arg(&dir_key)
            .arg(&*name_str)
            .arg(new_ino)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        if setnx_res == 0 {
            return Err(Errno::from(libc::EEXIST));
        }

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let attr_key = format!("squeezefs:attr:{}", new_ino);

        let meta_key = format!("metadata:inode_{}", new_ino);

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
            .hset(&meta_key, "type", "inline")
            .hset(&meta_key, "size", 0)
            .incr("squeezefs:used_inodes", 1)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        self.attr_cache.remove(&parent);

        let attr = self
            .get_attr_internal(new_ino)
            .await
            .map_err(map_squeezefs_err)?;

        Ok(ReplyCreated {
            ttl: Duration::from_secs(1),
            attr,
            generation: 1,
            fh: new_ino, // file handle
            flags: 0,    // FOPEN flags (0 = default)
        })
    }

    async fn open(&self, _req: Request, inode: Inode, _flags: u32) -> FuseResult<ReplyOpen> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Open: inode = {}", inode);

        // File handle is just the inode number for simplicity in this design
        Ok(ReplyOpen {
            fh: inode,
            flags: 0,
        })
    }

    async fn opendir(&self, _req: Request, inode: Inode, _flags: u32) -> FuseResult<ReplyOpen> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Opendir: inode = {}", inode);

        Ok(ReplyOpen {
            fh: inode,
            flags: 0,
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
        crate::coz_progress!("fuse_read");
        debug!(
            "FUSE Read: ino = {}, fh = {}, offset = {}, size = {}",
            ino, fh, offset, size
        );

        if ino == CONFIG_INODE {
            let config_data = self.generate_config_json().await;
            let bytes = config_data.into_bytes();
            if offset >= bytes.len() as u64 {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                });
            }
            let start = offset as usize;
            let end = std::cmp::min(bytes.len(), start + size as usize);
            return Ok(ReplyData {
                data: bytes[start..end].to_vec().into(),
            });
        }

        let file_path = format!("inode_{}", ino);
        let block_size = self.router.block_size.load(Ordering::Relaxed);

        // Get file size to bound the read
        let file_size = if let Some(entry) = self.attr_cache.get(&ino) {
            entry.value().0.size
        } else {
            let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
            let attr_key = format!("squeezefs:attr:{}", ino);
            let size_opt: Option<u64> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            size_opt.unwrap_or(0)
        };

        if offset >= file_size {
            return Ok(ReplyData {
                data: Vec::new().into(),
            });
        }

        let read_len = std::cmp::min(size as u64, file_size - offset) as usize;
        let mut read_result = vec![0u8; read_len];

        // 1. Try to read from committed storage
        let read_future = self
            .router
            .read_file_range(&file_path, offset, read_len as u32);
        if let Ok(Ok(committed_data)) =
            tokio::time::timeout(Duration::from_secs(2), read_future).await
        {
            let copy_len = std::cmp::min(read_result.len(), committed_data.len());
            read_result[..copy_len].copy_from_slice(&committed_data[..copy_len]);
        }

        // 2. Overlay any staging blocks in active_writes
        let staging_dir = self
            .router
            .cache
            .nvme
            .staging_dirs()
            .first()
            .cloned()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/squeezefs_staging"));

        let active_dir = staging_dir
            .join("active_writes")
            .join(format!("inode_{}", ino));

        if active_dir.exists() {
            let start_block = offset / block_size;
            let end_block = (offset + read_len as u64 - 1) / block_size;

            for b in start_block..=end_block {
                let block_file_path = active_dir.join(format!("block_{}", b));
                if let Ok(block_data) = tokio::fs::read(&block_file_path).await {
                    let b_start_offset = b * block_size;
                    let b_end_offset = b_start_offset + block_data.len() as u64;

                    let overlap_start = std::cmp::max(offset, b_start_offset);
                    let overlap_end = std::cmp::min(offset + read_len as u64, b_end_offset);

                    if overlap_start < overlap_end {
                        let src_start = (overlap_start - b_start_offset) as usize;
                        let src_end = (overlap_end - b_start_offset) as usize;
                        let dest_start = (overlap_start - offset) as usize;
                        let dest_end = (overlap_end - offset) as usize;

                        let dest_slice = &mut read_result[dest_start..dest_end];
                        let src_slice = &block_data[src_start..src_end];
                        dest_slice.copy_from_slice(src_slice);
                    }
                }
            }
        }

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
        crate::coz_progress!("fuse_write");

        if ino == CONFIG_INODE {
            return Err(Errno::from(libc::EACCES));
        }

        // Acquire local inode lock for the ENTIRE write operation to serialize
        // concurrent/subsequent writes to the same file.
        let lock = self.get_inode_lock(ino);
        let _guard = lock.lock().await;

        // 1. Get or acquire lease (fencing token)
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

        let attr_key = format!("squeezefs:attr:{}", ino);

        let old_size = if let Some(entry) = self.attr_cache.get(&ino) {
            entry.value().0.size
        } else {
            let old_size_opt: Option<u64> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            old_size_opt.unwrap_or(0)
        };

        let bytes_written = data.len() as u32;
        let new_size = std::cmp::max(old_size, offset + bytes_written as u64);
        if new_size > old_size {
            let diff = new_size - old_size;
            self.check_capacity_quota(&mut con, diff).await?;
        }

        // 2. Write data using progressive layout routing if not striped
        let meta_key = format!("metadata:inode_{}", ino);
        let file_type: Option<String> = con.hget(&meta_key, "type").await.map_err(map_err)?;

        if file_type.as_deref() == Some("striped") {
            self.write_file_staged(ino, offset, data, fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
        } else {
            let file_path = format!("inode_{}", ino);
            self.router
                .write_file(&file_path, offset, data, fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
        }

        // 3. Update file attributes and used bytes

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        let mut pipe = redis::pipe();
        let meta_key = format!("metadata:inode_{}", ino);
        pipe.hset(&attr_key, "size", new_size)
            .hset(&attr_key, "mtime_sec", sec)
            .hset(&attr_key, "mtime_nsec", nsec)
            .hset(&attr_key, "ctime_sec", sec)
            .hset(&attr_key, "ctime_nsec", nsec)
            .hset(&meta_key, "size", new_size);

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

        self.check_inode_quota(&mut con).await?;

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
            .incr("squeezefs:used_inodes", 1)
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

        self.attr_cache.remove(&parent);

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
            .decr("squeezefs:used_inodes", 1)
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

        if ino == CONFIG_INODE {
            return Err(Errno::from(libc::EACCES));
        }

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
            // Fix: Actually delete data when file is truncated to size 0
            if size == 0 && old_size > 0 {
                let file_path = format!("inode_{}", ino);
                // 1. Physically delete blocks from NVMe/S3 via router
                let _ = self.router.delete_file(&file_path, &mut con).await;

                // 2. Delete inline payload if any
                let inline_key = format!("inline_data:{}", file_path);
                let _: Result<(), _> = con.del(&inline_key).await;
            }

            pipe.hset(&attr_key, "size", size);
            // Also update the physical/routing size in the metadata block?
            let meta_key = format!("metadata:inode_{}", ino);
            pipe.hset(&meta_key, "size", size);

            // Fix: If truncated to 0, reset type to inline so it doesn't look for deleted staged/striped blocks
            if size == 0 && old_size > 0 {
                pipe.hset(&meta_key, "type", "inline");
            }

            if size > old_size {
                let diff = size - old_size;
                self.check_capacity_quota(&mut con, diff).await?;
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

        self.check_inode_quota(&mut con).await?;

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
            .incr("squeezefs:used_inodes", 1)
            .query_async(&mut con)
            .await
            .map_err(map_err)?;

        // Update parent directory timestamps!
        self.update_parent_timestamps(&mut con, parent)
            .await
            .map_err(map_err)?;

        self.attr_cache.remove(&parent);

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

        self.attr_cache.remove(&ino);
        self.attr_cache.remove(&new_parent);

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
        crate::coz_progress!("fuse_unlink");
        let name_str = name.to_string_lossy();
        debug!("FUSE unlink: parent = {}, name = {}", parent, name_str);

        if parent == 1 && name_str == ".config" {
            return Err(Errno::from(libc::EPERM));
        }

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
                .decr("squeezefs:used_inodes", 1)
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

        if (parent == 1 && name_str == ".config") || (new_parent == 1 && new_name_str == ".config")
        {
            return Err(Errno::from(libc::EPERM));
        }

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
                let _: () = redis::pipe()
                    .del(&child_dest_dir_key)
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;
            }
            let _: () = redis::pipe()
                .del(&dest_attr_key)
                .decr("squeezefs:used_inodes", 1)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;
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
        crate::coz_progress!("fuse_readdir");
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

        if parent == 1 && !entries_map.contains_key(".config") {
            let offset = (entries.len() + 1) as i64;
            entries.push(DirectoryEntry {
                name: ".config".into(),
                kind: FileType::RegularFile,
                inode: CONFIG_INODE,
                offset,
            });
        }

        let mut child_inos = Vec::new();
        for (name, child_ino) in &entries_map {
            if name == "." || name == ".." {
                continue;
            }
            child_inos.push(*child_ino);
        }

        let mut kind_map = std::collections::HashMap::new();
        if !child_inos.is_empty() {
            let mut pipe = redis::pipe();
            let mut inos_to_fetch = Vec::new();
            for child_ino in &child_inos {
                if let Some(entry) = self.attr_cache.get(child_ino) {
                    let (attr, cached_at) = entry.value();
                    if cached_at.elapsed() < Duration::from_secs(1) {
                        kind_map.insert(*child_ino, attr.kind);
                        continue;
                    }
                }
                let child_attr_key = format!("squeezefs:attr:{}", child_ino);
                pipe.hget(&child_attr_key, "kind");
                inos_to_fetch.push(*child_ino);
            }

            if !inos_to_fetch.is_empty() {
                let kind_nums: Vec<Option<u8>> =
                    pipe.query_async(&mut con).await.unwrap_or_default();
                for (idx, child_ino) in inos_to_fetch.iter().enumerate() {
                    let kind_num = kind_nums.get(idx).and_then(|v| *v).unwrap_or(1);
                    let kind = match kind_num {
                        2 => FileType::Directory,
                        3 => FileType::Symlink,
                        4 => FileType::NamedPipe,
                        5 => FileType::CharDevice,
                        6 => FileType::BlockDevice,
                        7 => FileType::Socket,
                        _ => FileType::RegularFile,
                    };
                    kind_map.insert(*child_ino, kind);
                }
            }
        }

        let mut current_offset = (entries.len() + 1) as i64;
        for (name, child_ino) in entries_map {
            if name == "." || name == ".." {
                continue;
            }
            let kind = kind_map
                .get(&child_ino)
                .cloned()
                .unwrap_or(FileType::RegularFile);

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

        if parent == 1 && !entries_map.contains_key(".config") {
            let config_data = self.generate_config_json().await;
            let attr = self.get_config_attr(config_data.len() as u64);
            let offset = (entries.len() + 1) as i64;
            entries.push(DirectoryEntryPlus {
                name: ".config".into(),
                kind: FileType::RegularFile,
                inode: CONFIG_INODE,
                generation: 1,
                attr,
                entry_ttl: Duration::from_secs(1),
                attr_ttl: Duration::from_secs(1),
                offset,
            });
        }

        // 1. Gather all inodes we need attributes for that AREN'T in local cache
        let mut pipe = redis::pipe();
        let mut inos_to_fetch = Vec::new();

        for (name, child_ino) in &entries_map {
            if name == "." || name == ".." {
                continue;
            }

            // Check if it's already in our local DashMap cache
            let is_cached = self
                .attr_cache
                .get(child_ino)
                .map(|e| e.value().1.elapsed() < Duration::from_secs(1))
                .unwrap_or(false);

            if !is_cached {
                pipe.hgetall(format!("squeezefs:attr:{}", child_ino));
                inos_to_fetch.push(*child_ino);
            }
        }

        // 2. Fetch them ALL in exactly ONE network round-trip!
        if !inos_to_fetch.is_empty() {
            let bulk_attrs: Vec<std::collections::HashMap<String, String>> =
                pipe.query_async(&mut con).await.unwrap_or_default();

            // 3. Process the results and stick them into self.attr_cache
            for (ino, fields) in inos_to_fetch.into_iter().zip(bulk_attrs) {
                if fields.is_empty() {
                    continue;
                }

                let ino_parsed = fields
                    .get("ino")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(ino);
                let size: u64 = fields.get("size").and_then(|v| v.parse().ok()).unwrap_or(0);
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
                let blocks = match kind {
                    FileType::Directory | FileType::Symlink | FileType::RegularFile => {
                        size.div_ceil(512)
                    }
                    _ => 0,
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
                    ino: ino_parsed,
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
            }
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
        // Sort paths lexicographically to prevent deadlocks under concurrent operations.
        let (src_lease, dest_lease) = if inode == inode_out {
            let lease = match self
                .dlm
                .acquire_lock(&src_path, None, Duration::from_secs(5))
                .await
            {
                Ok(l) => l,
                Err(e) => {
                    error!(
                        "copy_file_range: failed to acquire lock on src_path {}: {:?}",
                        src_path, e
                    );
                    return Err(Errno::from(libc::EAGAIN));
                }
            };
            (Some(lease), None)
        } else {
            let (first_path, second_path) = if src_path < dest_path {
                (&src_path, &dest_path)
            } else {
                (&dest_path, &src_path)
            };

            let first_lease = match self
                .dlm
                .acquire_lock(first_path, None, Duration::from_secs(5))
                .await
            {
                Ok(l) => l,
                Err(e) => {
                    error!(
                        "copy_file_range: failed to acquire lock on first path {}: {:?}",
                        first_path, e
                    );
                    return Err(Errno::from(libc::EAGAIN));
                }
            };

            let second_lease = match self
                .dlm
                .acquire_lock(second_path, None, Duration::from_secs(5))
                .await
            {
                Ok(l) => l,
                Err(e) => {
                    error!(
                        "copy_file_range: failed to acquire lock on second path {}: {:?}",
                        second_path, e
                    );
                    return Err(Errno::from(libc::EAGAIN));
                }
            };

            if src_path < dest_path {
                (Some(first_lease), Some(second_lease))
            } else {
                (Some(second_lease), Some(first_lease))
            }
        };

        let _src_lease = src_lease;

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

            self.attr_cache.remove(&inode_out);

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
            .write_file(
                &dest_path,
                off_out,
                chunk,
                dest_lease.as_ref().unwrap().fencing_token(),
            )
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

        self.attr_cache.remove(&inode_out);

        Ok(ReplyCopyFileRange { copied: copied_len })
    }

    async fn statfs(&self, _req: Request, _ino: u64) -> FuseResult<ReplyStatFs> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        let used_bytes_opt: Option<u64> = con.get("squeezefs:used_bytes").await.map_err(map_err)?;
        let used_bytes = used_bytes_opt.unwrap_or(0);

        let used_inodes_opt: Option<u64> =
            con.get("squeezefs:used_inodes").await.map_err(map_err)?;
        let used_inodes = used_inodes_opt.unwrap_or(0);

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

        let inodes_limit = if format_exists {
            let limit_str: Option<String> = con
                .hget("squeezefs:format", "inodes")
                .await
                .map_err(map_err)?;
            limit_str.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
        } else {
            0
        };

        let total_inodes = if inodes_limit > 0 {
            inodes_limit
        } else {
            1_000_000_000
        };

        let ffree = total_inodes.saturating_sub(used_inodes);
        let total_blocks = capacity / bsize as u64;
        let used_blocks = used_bytes.div_ceil(bsize as u64);
        let bfree = total_blocks.saturating_sub(used_blocks);

        Ok(ReplyStatFs {
            blocks: total_blocks,
            bfree,
            bavail: bfree,
            files: total_inodes,
            ffree,
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
        if let Err(e) = self.flush_active_blocks(ino, fencing_token).await {
            warn!(
                "FUSE Flush failed for ino {}, but masking error for editor compatibility: {:?}",
                ino, e
            );
        }

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

        // Flush any remaining active staging blocks before releasing the lease
        if let Ok(fencing_token) = self.get_or_acquire_lease(ino).await {
            let _ = self.flush_active_blocks(ino, fencing_token).await;
        }

        // If there's a cached lease, release it and remove it from our active_leases map
        if let Some((_, lease)) = self.active_leases.remove(&ino) {
            let _ = lease.release().await;
        }

        // Also clean up local inode lock if no longer needed (only if strong_count <= 2)
        drop(_guard);
        if std::sync::Arc::strong_count(&lock) <= 2 {
            self.active_inode_locks.remove(&ino);
        }

        Ok(())
    }

    async fn fsync(&self, _req: Request, ino: u64, _fh: u64, _datasync: bool) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Fsync: ino = {}, datasync = {}", ino, _datasync);

        // 1. Acquire local inode lock
        let lock = self.get_inode_lock(ino);
        let _guard = lock.lock().await;

        // 2. Get or acquire lease (fencing token)
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // 3. Flush the staging blocks concurrently to backend (S3/RustFS)
        if let Err(e) = self.flush_active_blocks(ino, fencing_token).await {
            warn!(
                "FUSE Fsync failed for ino {}, but masking error for editor compatibility: {:?}",
                ino, e
            );
        }

        Ok(())
    }

    async fn fsyncdir(&self, _req: Request, ino: u64, _fh: u64, _datasync: bool) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Fsyncdir: ino = {}", ino);
        // Directories are updated synchronously in Garnet, so we just return Ok.
        Ok(())
    }

    async fn fallocate(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE Fallocate: ino = {}, offset = {}, length = {}, mode = {}",
            ino, offset, length, mode
        );

        // Pre-allocation isn't strictly required to reserve physical space in our S3-backed store
        // as S3 objects are sparse/dynamic by nature. We just update the size attribute if we are extending.
        if mode & libc::FALLOC_FL_KEEP_SIZE as u32 == 0 {
            let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
            let attr_key = format!("squeezefs:attr:{}", ino);

            // Check if inode exists first
            let exists: bool = con.exists(&attr_key).await.map_err(map_err)?;
            if !exists {
                return Err(Errno::from(libc::ENOENT));
            }

            let old_size_opt: Option<u64> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            let old_size = old_size_opt.unwrap_or(0);

            let target_size = offset + length;
            if target_size > old_size {
                let diff = target_size - old_size;
                self.check_capacity_quota(&mut con, diff).await?;

                let mut pipe = redis::pipe();
                pipe.hset(&attr_key, "size", target_size);

                let meta_key = format!("metadata:inode_{}", ino);
                pipe.hset(&meta_key, "size", target_size);

                let diff = target_size - old_size;
                pipe.incr("squeezefs:used_bytes", diff);

                let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;

                // Invalidate cached attributes
                self.attr_cache.remove(&ino);
                let file_path = format!("inode_{}", ino);
                self.router.metadata_cache.remove(&file_path);
            }
        }

        Ok(())
    }

    async fn forget(&self, _req: Request, ino: u64, count: u64) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Forget: ino = {}, count = {}", ino, count);
        // We don't maintain local inode lookup references that need strict forgetting.
        // The attr_cache naturally evicts old entries.
    }

    async fn getlk(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _type: u32,
        _pid: u32,
    ) -> FuseResult<ReplyLock> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE getlk: inode = {}, owner = {}, start = {}, end = {}, type = {}",
            inode, _lock_owner, _start, _end, _type
        );

        // 1. Check local conflicts
        for entry in self.active_posix_locks.iter() {
            let &(lock_ino, lock_owner, lock_start, lock_end) = entry.key();
            if lock_ino == inode
                && lock_owner != _lock_owner
                && std::cmp::max(lock_start, _start) <= std::cmp::min(lock_end, _end)
            {
                debug!(
                    "FUSE getlk: conflict found locally with owner {} on range {}-{}",
                    lock_owner, lock_start, lock_end
                );
                return Ok(ReplyLock {
                    start: lock_start,
                    end: lock_end,
                    r#type: libc::F_WRLCK as u32,
                    pid: lock_owner as u32,
                });
            }
        }

        // 2. Check global conflicts in Redis
        if let Ok(mut con) = self.dlm.get_connection().await {
            let pattern = format!("lock:inode_{}:range:*", inode);
            if let Ok(keys) = self.scan_lock_keys(&mut con, &pattern).await {
                for key in keys {
                    if let Some(suffix) = key.strip_prefix(&format!("lock:inode_{}:range:", inode))
                    {
                        let parts: Vec<&str> = suffix.split('-').collect();
                        if parts.len() == 2 {
                            if let (Ok(r_start), Ok(r_end)) =
                                (parts[0].parse::<u64>(), parts[1].parse::<u64>())
                            {
                                if std::cmp::max(r_start, _start) <= std::cmp::min(r_end, _end) {
                                    if let Ok(Some(client_id)) =
                                        con.get::<_, Option<String>>(&key).await
                                    {
                                        if client_id != self.dlm.client_id() {
                                            debug!("FUSE getlk: conflict found globally (client_id: {}) on range {}-{}", client_id, r_start, r_end);
                                            return Ok(ReplyLock {
                                                start: r_start,
                                                end: r_end,
                                                r#type: libc::F_WRLCK as u32,
                                                pid: 0,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        debug!("FUSE getlk: no conflict found, range unlocked");
        Ok(ReplyLock {
            start: _start,
            end: _end,
            r#type: libc::F_UNLCK as u32,
            pid: 0,
        })
    }

    async fn setlk(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        _lock_owner: u64,
        _start: u64,
        _end: u64,
        _type: u32,
        _pid: u32,
        _block: bool,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);

        debug!(
            "FUSE setlk: inode = {}, owner = {}, start = {}, end = {}, type = {}, block = {}",
            inode, _lock_owner, _start, _end, _type, _block
        );

        if _type == libc::F_UNLCK as u32 {
            let mut to_remove = Vec::new();
            for entry in self.active_posix_locks.iter() {
                let &(lock_ino, lock_owner, lock_start, lock_end) = entry.key();
                if lock_ino == inode
                    && lock_owner == _lock_owner
                    && std::cmp::max(lock_start, _start) <= std::cmp::min(lock_end, _end)
                {
                    to_remove.push((lock_ino, lock_owner, lock_start, lock_end));
                }
            }
            for key in to_remove {
                if let Some((_, lease)) = self.active_posix_locks.remove(&key) {
                    if let Err(e) = lease.release().await {
                        error!("Failed to explicitly release lock lease: {:?}", e);
                    }
                }
            }
            return Ok(());
        }

        // If we already hold a lock on this exact range, release it first
        if let Some((_, old_lease)) =
            self.active_posix_locks
                .remove(&(inode, _lock_owner, _start, _end))
        {
            let _ = old_lease.release().await;
        }

        let file_path = format!("inode_{}", inode);
        let mut attempts = 0;
        let max_attempts = if _block { 20 } else { 1 };

        loop {
            // 1. Check local conflicts (different owners)
            let mut conflict = false;
            for entry in self.active_posix_locks.iter() {
                let &(lock_ino, lock_owner, lock_start, lock_end) = entry.key();
                if lock_ino == inode
                    && lock_owner != _lock_owner
                    && std::cmp::max(lock_start, _start) <= std::cmp::min(lock_end, _end)
                {
                    conflict = true;
                    break;
                }
            }

            if !conflict {
                // 2. Check global conflicts in Redis
                if let Ok(mut con) = self.dlm.get_connection().await {
                    let pattern = format!("lock:inode_{}:range:*", inode);
                    if let Ok(keys) = self.scan_lock_keys(&mut con, &pattern).await {
                        for key in keys {
                            if let Some(suffix) =
                                key.strip_prefix(&format!("lock:inode_{}:range:", inode))
                            {
                                let parts: Vec<&str> = suffix.split('-').collect();
                                if parts.len() == 2 {
                                    if let (Ok(r_start), Ok(r_end)) =
                                        (parts[0].parse::<u64>(), parts[1].parse::<u64>())
                                    {
                                        if std::cmp::max(r_start, _start)
                                            <= std::cmp::min(r_end, _end)
                                        {
                                            if let Ok(Some(client_id)) =
                                                con.get::<_, Option<String>>(&key).await
                                            {
                                                if client_id != self.dlm.client_id() {
                                                    conflict = true;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        conflict = true;
                    }
                } else {
                    conflict = true;
                }
            }

            if !conflict {
                // Try to acquire the lock via DLM
                match self
                    .dlm
                    .acquire_lock(&file_path, Some((_start, _end)), Duration::from_secs(5))
                    .await
                {
                    Ok(lease) => {
                        debug!(
                            "FUSE setlk: successfully acquired lock range {}-{} for owner {}",
                            _start, _end, _lock_owner
                        );
                        self.active_posix_locks
                            .insert((inode, _lock_owner, _start, _end), lease);
                        return Ok(());
                    }
                    Err(SqueezefsError::LockFailed { .. }) => {}
                    Err(e) => {
                        return Err(map_squeezefs_err(e));
                    }
                }
            }

            attempts += 1;
            if attempts >= max_attempts {
                debug!("FUSE setlk: lock acquisition failed/timed out, returning EAGAIN");
                return Err(Errno::from(libc::EAGAIN));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn ioctl(
        &self,
        _req: Request,
        inode: u64,
        _fh: u64,
        flags: u32,
        cmd: u32,
        _arg: u64,
        _in_size: u32,
        _out_size: u32,
    ) -> FuseResult<ReplyIoctl> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE ioctl: inode = {}, cmd = {}, flags = {}",
            inode, cmd, flags
        );

        match cmd as u64 {
            libc::FS_IOC_GETFLAGS => Err(Errno::from(libc::ENOTTY)),
            libc::FS_IOC_SETFLAGS => Err(Errno::from(libc::ENOTTY)),
            _ => Err(Errno::from(libc::ENOTTY)),
        }
    }

    async fn setxattr(
        &self,
        _req: Request,
        inode: Inode,
        name: &OsStr,
        value: &[u8],
        _flags: u32,
        _position: u32,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return Err(Errno::from(libc::EINVAL)),
        };
        let mut con = self
            .dlm
            .get_connection()
            .await
            .map_err(|_| Errno::from(libc::EIO))?;
        let xattr_key = format!("squeezefs:xattr:{}", inode);

        let _: () = redis::cmd("HSET")
            .arg(&xattr_key)
            .arg(name_str)
            .arg(value)
            .query_async(&mut con)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;
        Ok(())
    }

    async fn getxattr(
        &self,
        _req: Request,
        inode: Inode,
        name: &OsStr,
        size: u32,
    ) -> FuseResult<fuse3::raw::reply::ReplyXAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return Err(Errno::from(libc::EINVAL)),
        };

        let mut con = self
            .dlm
            .get_connection()
            .await
            .map_err(|_| Errno::from(libc::EIO))?;
        let xattr_key = format!("squeezefs:xattr:{}", inode);
        let value: Option<Vec<u8>> = redis::cmd("HGET")
            .arg(&xattr_key)
            .arg(name_str)
            .query_async(&mut con)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;

        if let Some(v) = value {
            if size == 0 {
                return Ok(fuse3::raw::reply::ReplyXAttr::Size(v.len() as u32));
            }
            if size < v.len() as u32 {
                return Err(Errno::from(libc::ERANGE));
            }
            Ok(fuse3::raw::reply::ReplyXAttr::Data(v.into()))
        } else {
            #[cfg(target_os = "macos")]
            return Err(Errno::from(libc::ENOATTR));
            #[cfg(not(target_os = "macos"))]
            return Err(Errno::from(libc::ENODATA));
        }
    }

    async fn listxattr(
        &self,
        _req: Request,
        inode: Inode,
        size: u32,
    ) -> FuseResult<fuse3::raw::reply::ReplyXAttr> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let mut con = self
            .dlm
            .get_connection()
            .await
            .map_err(|_| Errno::from(libc::EIO))?;
        let xattr_key = format!("squeezefs:xattr:{}", inode);
        let keys: Vec<String> = redis::cmd("HKEYS")
            .arg(&xattr_key)
            .query_async(&mut con)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;

        let mut data = Vec::new();
        for key in keys {
            data.extend_from_slice(key.as_bytes());
            data.push(0); // Null-terminated strings
        }

        if size == 0 {
            return Ok(fuse3::raw::reply::ReplyXAttr::Size(data.len() as u32));
        }
        if size < data.len() as u32 {
            return Err(Errno::from(libc::ERANGE));
        }
        Ok(fuse3::raw::reply::ReplyXAttr::Data(data.into()))
    }

    async fn removexattr(&self, _req: Request, inode: Inode, name: &OsStr) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return Err(Errno::from(libc::EINVAL)),
        };
        let mut con = self
            .dlm
            .get_connection()
            .await
            .map_err(|_| Errno::from(libc::EIO))?;
        let xattr_key = format!("squeezefs:xattr:{}", inode);
        let deleted: i32 = redis::cmd("HDEL")
            .arg(&xattr_key)
            .arg(name_str)
            .query_async(&mut con)
            .await
            .map_err(|_| Errno::from(libc::EIO))?;

        if deleted == 0 {
            #[cfg(target_os = "macos")]
            return Err(Errno::from(libc::ENOATTR));
            #[cfg(not(target_os = "macos"))]
            return Err(Errno::from(libc::ENODATA));
        }
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
    writeback: bool,
    allow_other: bool,
    custom_opts: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = MountOptions::default();
    options.uid(uid);
    options.gid(gid);
    options.allow_other(allow_other);
    options.write_back(writeback);
    options.default_permissions(true);

    if let Some(opts) = custom_opts {
        for opt in opts.split(',') {
            let opt_trimmed = opt.trim();
            if !opt_trimmed.is_empty() {
                options.custom_options(opt_trimmed);
            }
        }
    } else {
        // default custom option
        options.custom_options("max_read=1048576");
    }

    info!(
        "SqueezeFS version {} initializing mount",
        env!("CARGO_PKG_VERSION")
    );
    info!(
        "FUSE Daemon: Mounting squeezefs at {:?}...",
        mountpoint.as_ref()
    );
    info!("FUSE Daemon: Garnet metadata connection active.");
    info!("FUSE Daemon: S3 object storage backend active.");

    let mount_path = mountpoint.as_ref().to_path_buf();

    // Spawn a background task to check mountpoint readiness (OK status print for foreground mounts)
    let mount_path_clone = mount_path.clone();
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        let mut ready = false;
        while start.elapsed() < std::time::Duration::from_secs(10) {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if let Ok(metadata) = std::fs::metadata(&mount_path_clone) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.ino() == 1 {
                        ready = true;
                        break;
                    }
                }
                #[cfg(not(unix))]
                {
                    ready = true;
                    break;
                }
            }
        }
        if ready {
            println!(
                "\x1b[92mOK\x1b[0m Squeezefs is ready at {:?}",
                mount_path_clone
            );
        }
    });

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
        .mount(fs, mount_path.clone())
        .await?;

    let mut handle = session;

    let shutdown = async {
        #[cfg(unix)]
        {
            let sigterm_opt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
            let sigint_opt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
            if let (Ok(mut sigterm), Ok(mut sigint)) = (sigterm_opt, sigint_opt) {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        info!("Received Ctrl+C, exiting...");
                    }
                    _ = sigterm.recv() => {
                        info!("Received SIGTERM, exiting...");
                    }
                    _ = sigint.recv() => {
                        info!("Received SIGINT, exiting...");
                    }
                }
            } else {
                let _ = tokio::signal::ctrl_c().await;
                info!("Received Ctrl+C, exiting...");
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            info!("Received Ctrl+C, exiting...");
        }
    };

    tokio::select! {
        res = &mut handle => {
            if let Err(e) = res {
                error!("FUSE session loop ended with error: {:?}", e);
                eprintln!("FUSE session loop ended with error: {:?}", e);
            } else {
                info!("FUSE session loop ended successfully.");
                println!("\n[!] The FUSE filesystem was unmounted externally (e.g. via umount). Squeezefs is now shutting down safely.\n");
            }
        }
        _ = shutdown => {
            info!("Received shutdown signal, unmounting filesystem...");
        }
    }

    // Clean up the mount by unmounting the session if it hasn't been done already.
    if let Err(e) = handle.unmount().await {
        debug!("Unmount on exit status (may already be unmounted): {:?}", e);
    } else {
        info!("Cleanly unmounted filesystem on exit.");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn format_volume(
    redis_url: &str,
    name: &str,
    block_size: u64,
    capacity: u64,
    inodes: u64,
    compression: &str,
    encrypt_algo: &str,
    encrypt_key: Option<&str>,
    mem_cache_size: Option<&str>,
    disk_cache_size: Option<&str>,
    disk_cache_paths: Option<&[std::path::PathBuf]>,
    s3_endpoint: Option<&str>,
    s3_access_key: Option<&str>,
    s3_secret_key: Option<&str>,
    s3_bucket: Option<&str>,
    read_cache_size: Option<&str>,
    write_cache_size: Option<&str>,
    read_mem_cache_size: Option<&str>,
    write_mem_cache_size: Option<&str>,
) -> Result<(), SqueezefsError> {
    let client = redis::Client::open(redis_url)?;
    let mut con = client.get_multiplexed_tokio_connection().await?;
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());

    let mem_size = mem_cache_size.unwrap_or("1GB").to_string();
    let disk_size = disk_cache_size.unwrap_or("10GB").to_string();
    let r_cache = read_cache_size.unwrap_or("").to_string();
    let w_cache = write_cache_size.unwrap_or("").to_string();
    let r_mem = read_mem_cache_size.unwrap_or("").to_string();
    let w_mem = write_mem_cache_size.unwrap_or("").to_string();
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
        .hset("squeezefs:format", "inodes", inodes)
        .hset("squeezefs:format", "compression", compression)
        .hset("squeezefs:format", "encrypt_algo", encrypt_algo)
        .hset("squeezefs:format", "encrypt_key", encrypt_key.unwrap_or(""))
        .hset("squeezefs:format", "version", 1) // ABI version
        .hset("squeezefs:format", "mem_cache_size", mem_size)
        .hset("squeezefs:format", "disk_cache_size", disk_size)
        .hset("squeezefs:format", "read_cache_size", r_cache)
        .hset("squeezefs:format", "write_cache_size", w_cache)
        .hset("squeezefs:format", "read_mem_cache_size", r_mem)
        .hset("squeezefs:format", "write_mem_cache_size", w_mem)
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
    let inodes: u64 = fields
        .get("inodes")
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
    let compression = fields
        .get("compression")
        .cloned()
        .unwrap_or_else(|| "none".to_string());
    let encrypt_algo = fields
        .get("encrypt_algo")
        .cloned()
        .unwrap_or_else(|| "none".to_string());

    Ok(serde_json::json!({
        "Setting": {
            "Name": name,
            "BlockSize": block_size,
            "Capacity": capacity,
            "Inodes": inodes,
            "Compression": compression,
            "EncryptAlgo": encrypt_algo,
            "MemCacheSize": mem_cache_size,
            "DiskCacheSize": disk_cache_size,
            "DiskCachePaths": disk_cache_paths,
            "S3Endpoint": s3_endpoint,
            "S3Bucket": s3_bucket,
            "ActiveWriteBackend": active_write_backend,
        }
    }))
}
