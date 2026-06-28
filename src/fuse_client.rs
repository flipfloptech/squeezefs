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
const STATS_INODE: u64 = 0xffff_ffff_ffff_fffd;

fn get_fuse_timeout() -> Duration {
    if let Ok(val) = std::env::var("SQUEEZEFS_TIMEOUT") {
        if let Ok(secs) = val.parse::<u64>() {
            return Duration::from_secs(secs);
        }
    }
    #[cfg(debug_assertions)]
    {
        Duration::from_secs(15)
    }
    #[cfg(not(debug_assertions))]
    {
        Duration::from_secs(2)
    }
}

pub static BLOCK_FLUSH_LOCKS: Lazy<
    dashmap::DashMap<(u64, u32), std::sync::Arc<tokio::sync::Mutex<()>>, ahash::RandomState>,
> = Lazy::new(|| dashmap::DashMap::with_hasher(ahash::RandomState::new()));

struct BlockFlushGuard {
    key: (u64, u32),
}

impl Drop for BlockFlushGuard {
    fn drop(&mut self) {
        BLOCK_FLUSH_LOCKS.remove_if(&self.key, |_, arc| std::sync::Arc::strong_count(arc) <= 2);
    }
}

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

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ClientInfo {
    pub client_id: String,
    pub hostname: String,
    pub pid: u32,
    pub mountpoint: String,
    pub mounted_at: u64,
    pub last_heartbeat: u64,
    pub stats: ClientStats,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ClientStats {
    pub fuse_ops: u64,
    pub meta_updates: u64,
    pub put_obj: u64,
    pub get_obj: u64,
    pub del_obj: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

fn get_hostname() -> String {
    if let Ok(mut f) = std::fs::File::open("/proc/sys/kernel/hostname") {
        use std::io::Read;
        let mut s = String::new();
        if f.read_to_string(&mut s).is_ok() {
            return s.trim().to_string();
        }
    }
    std::env::var("HOSTNAME")
        .unwrap_or_else(|_| std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string()))
}

fn map_err(e: redis::RedisError) -> Errno {
    error!("Garnet Database error: {:?}", e);
    Errno::from(libc::ECOMM)
}

fn map_squeezefs_err(e: SqueezefsError) -> Errno {
    error!("Squeezefs operational error: {:?}", e);
    Errno::from(e.to_errno())
}

pub enum PosixLock {
    Local,
    Global(Box<crate::dlm::LockLease>),
    Remote { client_id: String },
}

impl std::fmt::Debug for PosixLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PosixLock::Local => write!(f, "Local"),
            PosixLock::Global(_) => write!(f, "Global"),
            PosixLock::Remote { client_id } => f
                .debug_struct("Remote")
                .field("client_id", client_id)
                .finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WritebackRequest {
    pub ino: u64,
    pub block_idx: u32,
    pub fencing_token: u64,
}

pub struct SqueezefsFilesystem {
    pub router: DataRouter,
    dlm: DlmClient,
    uid: u32,
    gid: u32,
    active_leases: std::sync::Arc<dashmap::DashMap<u64, crate::dlm::LockLease, ahash::RandomState>>,
    lease_locks: std::sync::Arc<
        dashmap::DashMap<u64, std::sync::Arc<tokio::sync::Mutex<()>>, ahash::RandomState>,
    >,
    active_posix_locks:
        std::sync::Arc<dashmap::DashMap<(Inode, u64, u64, u64), PosixLock, ahash::RandomState>>,
    active_delegations:
        std::sync::Arc<dashmap::DashMap<Inode, crate::dlm::DelegationLease, ahash::RandomState>>,
    pub active_inode_locks: std::sync::Arc<
        dashmap::DashMap<u64, std::sync::Arc<tokio::sync::RwLock<()>>, ahash::RandomState>,
    >,
    pub attr_cache: dashmap::DashMap<u64, (FileAttr, std::time::Instant), ahash::RandomState>,
    pub dir_entry_cache:
        moka::sync::Cache<u64, std::sync::Arc<std::collections::HashMap<String, u64>>>,
    pub dismount_wait: u64,
    writeback_tx: tokio::sync::mpsc::Sender<WritebackRequest>,
    writeback_rx: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<WritebackRequest>>>,
    pub client_id: std::sync::Arc<std::sync::Mutex<String>>,
    pub mountpoint: std::sync::Arc<std::sync::Mutex<String>>,
    pub max_background_uploads: usize,
}

impl SqueezefsFilesystem {
    pub fn new(router: DataRouter, dlm: DlmClient, uid: u32, gid: u32) -> Self {
        let (writeback_tx, writeback_rx) = tokio::sync::mpsc::channel(10000);
        let dir_entry_cache = moka::sync::Cache::builder()
            .max_capacity(50000)
            .time_to_live(Duration::from_secs(1))
            .build();
        Self {
            router,
            dlm,
            uid,
            gid,
            active_leases: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            lease_locks: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            active_posix_locks: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            active_delegations: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            active_inode_locks: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            attr_cache: dashmap::DashMap::with_hasher(ahash::RandomState::new()),
            dir_entry_cache,
            dismount_wait: 10,
            writeback_tx,
            writeback_rx: std::sync::Mutex::new(Some(writeback_rx)),
            client_id: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mountpoint: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            max_background_uploads: 16,
        }
    }

    pub fn max_background_uploads(&self) -> usize {
        self.max_background_uploads
    }

    pub fn disable_background_writeback(&self) {
        let mut rx_guard = self.writeback_rx.lock().unwrap();
        let _ = rx_guard.take();
    }

    pub fn dlm(&self) -> &DlmClient {
        &self.dlm
    }

    pub fn active_posix_locks_count(&self) -> usize {
        self.active_posix_locks.len()
    }

    pub fn has_local_posix_lock(&self, inode: Inode, owner: u64, start: u64, end: u64) -> bool {
        if let Some(lock) = self.active_posix_locks.get(&(inode, owner, start, end)) {
            matches!(*lock, PosixLock::Local)
        } else {
            false
        }
    }

    pub fn has_global_posix_lock(&self, inode: Inode, owner: u64, start: u64, end: u64) -> bool {
        if let Some(lock) = self.active_posix_locks.get(&(inode, owner, start, end)) {
            matches!(*lock, PosixLock::Global(_))
        } else {
            false
        }
    }

    pub fn has_remote_posix_lock(&self, inode: Inode, owner: u64, start: u64, end: u64) -> bool {
        if let Some(lock) = self.active_posix_locks.get(&(inode, owner, start, end)) {
            matches!(*lock, PosixLock::Remote { .. })
        } else {
            false
        }
    }

    pub fn has_delegation(&self, inode: Inode) -> bool {
        self.active_delegations.contains_key(&inode)
    }

    async fn generate_config_json(&self) -> String {
        let mut con_opt = self.dlm.get_connection().await.ok();

        let mut format_fields: std::collections::HashMap<String, String> =
            if let Some(ref mut con) = con_opt {
                con.hgetall("squeezefs:format").await.unwrap_or_default()
            } else {
                std::collections::HashMap::new()
            };

        // Override disk_cache_paths in virtual config with the actual active isolated staging directories
        let active_dirs = self.router.cache.nvme.staging_dirs();
        let active_dirs_str = active_dirs
            .iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(",");
        format_fields.insert("disk_cache_paths".to_string(), active_dirs_str);

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

    fn get_stats_attr(&self, size: u64) -> FileAttr {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let sec = now.as_secs() as i64;
        let nsec = now.subsec_nanos();

        FileAttr {
            ino: STATS_INODE,
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

    async fn generate_stats_json(&self) -> String {
        let read_lru_keys = self.router.cache.read_lru.keys();
        let write_lru_keys = self.router.cache.write_lru.keys();
        let nvme_read_cache_block_keys = self.router.cache.nvme.list_cached_blocks();

        let mut nvme_staged_write_file_ids = Vec::new();
        let mut active_writes = serde_json::Map::new();

        for key in self.router.cache.nvme.list_staged_files() {
            if key.starts_with("active_block:") {
                let parts: Vec<&str> = key.split(':').collect();
                if parts.len() == 3 {
                    let inode_name = parts[1].to_string();
                    let block_name = parts[2].to_string();
                    active_writes
                        .entry(inode_name)
                        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::Value::String(block_name));
                }
            } else {
                nvme_staged_write_file_ids.push(key);
            }
        }

        let hits = METRICS.cache_hits.load(Ordering::Relaxed);
        let misses = METRICS.cache_misses.load(Ordering::Relaxed);
        let ratio = if hits + misses > 0 {
            hits as f64 / (hits + misses) as f64
        } else {
            0.0
        };

        let stats_obj = serde_json::json!({
            "read_lru_keys": read_lru_keys,
            "write_lru_keys": write_lru_keys,
            "nvme_staged_write_file_ids": nvme_staged_write_file_ids,
            "nvme_read_cache_block_keys": nvme_read_cache_block_keys,
            "active_writes": active_writes,
            "active_leases_count": self.active_leases.len(),
            "active_posix_locks_count": self.active_posix_locks.len(),
            "metrics": {
                "fuse_ops": METRICS.fuse_ops.load(Ordering::Relaxed),
                "meta_updates": METRICS.meta_updates.load(Ordering::Relaxed),
                "put_obj": METRICS.put_obj.load(Ordering::Relaxed),
                "get_obj": METRICS.get_obj.load(Ordering::Relaxed),
                "del_obj": METRICS.del_obj.load(Ordering::Relaxed),
                "cache_hits": hits,
                "cache_misses": misses,
                "cache_hit_ratio": ratio,
            },
            "cache_capacities": {
                "read_lru_current_bytes": self.router.cache.read_lru.current_bytes(),
                "read_lru_max_bytes": self.router.cache.read_lru.max_bytes(),
                "write_lru_current_bytes": self.router.cache.write_lru.current_bytes(),
                "write_lru_max_bytes": self.router.cache.write_lru.max_bytes(),
                "nvme_staging_current_bytes": self.router.cache.nvme.current_staged_write_bytes(),
                "nvme_staging_max_bytes": self.router.cache.nvme.max_write_bytes(),
                "nvme_read_cache_current_bytes": self.router.cache.nvme.current_read_cache_bytes(),
                "nvme_read_cache_max_bytes": self.router.cache.nvme.max_read_bytes(),
            },
            "internal_caches": {
                "metadata_cache_size": self.router.metadata_cache.entry_count(),
                "block_map_cache_size": self.router.block_map_cache.entry_count(),
            }
        });

        serde_json::to_string_pretty(&stats_obj).unwrap_or_default()
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
        let shard_count = self.dlm.shard_count();
        if shard_count > 1 {
            for i in 0..shard_count {
                let mut shard_con = self.dlm.get_connection_for_inode(i as u64).await?;
                let exists: bool = shard_con.exists("squeezefs:inode_counter").await?;
                if !exists {
                    let initial_counter = match i {
                        0 => shard_count as u64,
                        1 => 1 + shard_count as u64,
                        _ => i as u64,
                    };
                    let _: () = shard_con
                        .set("squeezefs:inode_counter", initial_counter)
                        .await?;
                }
            }
        }

        let mut con = self.dlm.get_connection_for_inode(1).await?;
        let exists: bool = con.exists("squeezefs:attr:1").await?;
        if !exists {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();

            let initial_counter = match shard_count {
                1 => 1,
                other => 1 + other as u64,
            };

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
                .set_nx("squeezefs:inode_counter", initial_counter)
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

    pub fn get_inode_lock(&self, ino: u64) -> std::sync::Arc<tokio::sync::RwLock<()>> {
        self.active_inode_locks
            .entry(ino)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::RwLock::new(())))
            .clone()
    }

    async fn get_or_acquire_lease(&self, ino: u64) -> Result<u64, SqueezefsError> {
        if let Some(lease) = self.active_leases.get(&ino) {
            return Ok(lease.fencing_token());
        }

        let lock_arc = self
            .lease_locks
            .entry(ino)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock_arc.lock().await;

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

    async fn ensure_delegation_held(&self, inode: Inode) -> Result<(), SqueezefsError> {
        if self.active_delegations.contains_key(&inode) {
            return Ok(());
        }

        let mut attempts = 0;
        let max_attempts = 40; // 2 seconds total timeout (40 * 50ms)

        loop {
            match self
                .dlm
                .acquire_delegation(inode, Duration::from_secs(5))
                .await?
            {
                crate::dlm::DelegationResult::Acquired(lease) => {
                    info!(
                        "ensure_delegation_held: Acquired delegation on inode {}",
                        inode
                    );
                    if let Err(e) =
                        load_locks_from_redis(inode, &self.dlm, &self.active_posix_locks).await
                    {
                        error!(
                            "ensure_delegation_held: Failed to load locks from Redis: {:?}",
                            e
                        );
                        let _ = lease.release().await;
                        return Err(e);
                    }
                    self.active_delegations.insert(inode, lease);
                    return Ok(());
                }
                crate::dlm::DelegationResult::HeldBy(holder) => {
                    info!("ensure_delegation_held: Inode {} delegation held by {}. Publishing recall...", inode, holder);
                    if let Err(e) = self.dlm.publish_recall(&holder, inode).await {
                        warn!(
                            "ensure_delegation_held: Failed to publish recall to {}: {:?}",
                            holder, e
                        );
                    }
                }
            }

            attempts += 1;
            if attempts >= max_attempts {
                return Err(SqueezefsError::LockFailed {
                    reason: format!("Timed out waiting to acquire delegation on inode {}", inode),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn block_write_needs_existing_data(
        existing_size: u64,
        block_start: u64,
        block_end: u64,
        write_start: u64,
        write_end: u64,
    ) -> bool {
        let existing_block_end = std::cmp::min(existing_size, block_end);
        existing_block_end > block_start
            && (write_start > block_start || write_end < existing_block_end)
    }

    async fn write_file_staged(
        &self,
        ino: u64,
        offset: u64,
        data: &[u8],
        existing_size: u64,
        _fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let block_size = self.router.block_size.load(Ordering::Relaxed);
        let start_block = offset / block_size;
        let end_block = (offset + data.len() as u64 - 1) / block_size;

        let mut data_cursor = 0usize;
        for b in start_block..=end_block {
            let b_start_offset = b * block_size;
            let b_end_offset = b_start_offset + block_size;

            let write_start = std::cmp::max(offset, b_start_offset);
            let write_end = std::cmp::min(offset + data.len() as u64, b_end_offset);
            let slice_len = (write_end - write_start) as usize;

            let file_data_slice = &data[data_cursor..data_cursor + slice_len];
            data_cursor += slice_len;

            let cache_key = format!("active_block:inode_{}:block_{}", ino, b);
            let needs_existing_data = Self::block_write_needs_existing_data(
                existing_size,
                b_start_offset,
                b_end_offset,
                write_start,
                write_end,
            );

            // 0. Acquire Block-level Lock to prevent concurrent modification to the same block
            let block_lock = BLOCK_FLUSH_LOCKS
                .entry((ino, b as u32))
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .value()
                .clone();
            let _block_guard = block_lock.lock().await;

            // 1. Get existing block data (either from staging_nvme_cache or read from S3/cache)
            let mut block_data = if let Some(d) = self.router.cache.nvme.read_staged(&cache_key) {
                d
            } else if !needs_existing_data {
                Vec::new()
            } else {
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
                        let mut con = self.dlm.get_connection_for_inode(ino).await?;
                        let id_opt: Option<String> = con.hget(&meta_key, "block_map_id").await?;
                        id_opt
                    }
                };

                let mut existing_block_data = Vec::new();
                if let Some(block_map_id) = block_map_id {
                    let mut old_block_key: Option<Option<String>> = None;

                    // Try block_map_cache first
                    let cache_key_tuple = (block_map_id.clone(), b as u32);
                    if let Some(entry) = self.router.block_map_cache.get(&cache_key_tuple) {
                        let (bk, cached_at) = &entry;
                        if cached_at.elapsed() < Duration::from_secs(1) {
                            old_block_key = Some(bk.clone());
                        }
                    }

                    // If miss, query Garnet
                    let old_block_key = match old_block_key {
                        Some(key_opt) => key_opt,
                        None => {
                            let block_map_key = format!("block_map:{}", block_map_id);
                            let mut con = self.dlm.get_connection_for_inode(ino).await?;
                            let key_opt: Option<String> =
                                con.hget(&block_map_key, b.to_string()).await?;
                            key_opt
                        }
                    };

                    if let Some(bk) = old_block_key {
                        existing_block_data = if let Some(cached_block) =
                            self.router.cache.read_lru.get(&bk)
                        {
                            cached_block.to_vec()
                        } else if let Some(cached) =
                            self.router.cache.nvme.get_cached_read_block(&bk)
                        {
                            self.router
                                .cache
                                .read_lru
                                .put(&bk, bytes::Bytes::from(cached.clone()));
                            cached
                        } else {
                            // S3 read path: lock is handled natively by block_lock

                            let get_res = async {
                                let raw = self.router.read_nvme_block(&bk).await?;
                                let decompressed = self.router.get_crypto().process_read(&raw)?;
                                Ok::<Vec<u8>, SqueezefsError>(decompressed)
                            }
                            .await;

                            let decompressed = get_res?;
                            self.router
                                .cache
                                .read_lru
                                .put(&bk, bytes::Bytes::from(decompressed.clone()));
                            let nvme_clone = self.router.cache.nvme.clone();
                            let bk_clone = bk.clone();
                            let decompressed_clone = decompressed.clone();
                            tokio::task::spawn_blocking(move || {
                                let _ = nvme_clone.cache_read_block(&bk_clone, &decompressed_clone);
                            });
                            decompressed
                        };
                    }
                }
                existing_block_data
            };

            // 2. Perform seek and write range directly in memory
            let rel_start = (write_start - b_start_offset) as usize;
            if block_data.len() < rel_start + slice_len {
                block_data.resize(rel_start + slice_len, 0);
            }
            block_data[rel_start..rel_start + slice_len].copy_from_slice(file_data_slice);

            // 3. Write back to staging_nvme_cache
            let nvme_clone = self.router.cache.nvme.clone();
            let cache_key_clone = cache_key.clone();
            let block_data_clone = block_data.clone();
            let fencing_token_val = _fencing_token;
            tokio::task::spawn_blocking(move || {
                nvme_clone.put_active_block(&cache_key_clone, &block_data_clone, fencing_token_val);
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

            let req = WritebackRequest {
                ino,
                block_idx: b as u32,
                fencing_token: _fencing_token,
            };
            let _ = self.writeback_tx.send(req).await;
        }

        Ok(())
    }

    async fn flush_active_blocks_with_retry(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let prefix = format!("active_block:inode_{}:", ino);

        let keys = self.router.cache.nvme.staging_nvme_cache.list_keys();

        let mut active_keys = Vec::new();
        for key_bytes in keys {
            let key_str = String::from_utf8(key_bytes.to_vec()).unwrap_or_default();
            if key_str.starts_with(&prefix) {
                active_keys.push(key_str);
            }
        }

        if !active_keys.is_empty() {
            let mut block_indices = Vec::new();
            for key_str in active_keys {
                let b_str = key_str
                    .trim_start_matches(&prefix)
                    .trim_start_matches("block_");
                if let Ok(b) = b_str.parse::<u32>() {
                    block_indices.push(b);
                }
            }

            flush_due_active_blocks_for_inode(
                ino,
                block_indices,
                fencing_token,
                &self.router,
                &self.dlm,
                &self.active_inode_locks,
            )
            .await?;
        }

        Ok(())
    }

    async fn get_attr_internal(&self, ino: u64) -> Result<FileAttr, SqueezefsError> {
        if let Some(entry) = self.attr_cache.get(&ino) {
            let (attr, cached_at) = entry.value();
            if cached_at.elapsed() < Duration::from_secs(1) {
                return Ok(*attr);
            }
        }

        let mut con = self.dlm.get_connection_for_inode(ino).await?;
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

    pub async fn complete_active_multipart_upload_if_any(
        &self,
        _ino: u64,
    ) -> Result<(), SqueezefsError> {
        Ok(())
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GdsReadArgs {
    pub vram_address: u64,
    pub offset: u64,
    pub size: u64,
}

pub const SQUEEZEFS_IOC_GDS_READ: u32 = 0x80186601;

// Implement fuse3 Raw Filesystem interface
impl Filesystem for SqueezefsFilesystem {
    type DirEntryStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntry>>;
    type DirEntryPlusStream<'a> = futures::stream::BoxStream<'a, FuseResult<DirectoryEntryPlus>>;

    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        info!("FUSE Daemon: Initialized Squeezefs Filesystem mount.");

        info!("FUSE init: getting connection...");
        let mut con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
        info!("FUSE init: connection obtained. Checking format existence...");
        let format_exists: bool = con.exists("squeezefs:format").await.map_err(map_err)?;
        info!("FUSE init: format_exists = {}", format_exists);

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
                .hset("squeezefs:format", "fuse_io_uring_sqpoll_idle_ms", "")
                .query_async(&mut con)
                .await
                .map_err(map_err)?;
        }

        info!("FUSE init: loading encryption and compression settings...");
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

        info!("FUSE init: checking database ABI version...");
        let version_str: Option<String> = con
            .hget("squeezefs:format", "version")
            .await
            .map_err(map_err)?;
        let version: u64 = version_str.and_then(|v| v.parse().ok()).unwrap_or(1);
        if version > 1 {
            error!("Database ABI version ({}) is higher than client supported version (1). Rejecting mount.", version);
            return Err(Errno::from(libc::EPROTO));
        }

        info!("FUSE init: loading block size...");
        let block_size_str: Option<String> = con
            .hget("squeezefs:format", "block_size")
            .await
            .map_err(map_err)?;
        let block_size: u64 = block_size_str
            .and_then(|v| v.parse().ok())
            .unwrap_or(4 * 1024 * 1024);
        self.router.set_block_size(block_size);

        // Initialize root directory attributes in Garnet if not present
        info!("FUSE init: initializing root inode...");
        if let Err(e) = self.init_root_inode().await {
            error!("Failed to initialize root inode in Garnet: {:?}", e);
            return Err(Errno::from(libc::EIO));
        }
        info!("FUSE init: root inode initialized successfully.");

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

        // Start background active writes flusher task
        let mut rx_guard = self.writeback_rx.lock().unwrap();
        if let Some(writeback_rx) = rx_guard.take() {
            let router = self.router.clone();
            let dlm = self.dlm.clone();
            let active_inode_locks = self.active_inode_locks.clone();
            let max_uploads = self.max_background_uploads;
            tokio::spawn(async move {
                run_constant_writeback_worker(
                    writeback_rx,
                    router,
                    dlm,
                    active_inode_locks,
                    max_uploads,
                )
                .await;
            });
        }

        // Start background recall listener task for POSIX lock delegations
        let dlm_clone = self.dlm.clone();
        let delegations_clone = self.active_delegations.clone();
        let posix_locks_clone = self.active_posix_locks.clone();

        tokio::spawn(async move {
            info!("FUSE init: Starting background POSIX lock recall listener...");
            match dlm_clone.get_pubsub_connection().await {
                Ok(mut pubsub) => {
                    let channel = format!("squeezefs:client:{}:recalls", dlm_clone.client_id());
                    if let Err(e) = pubsub.subscribe(&channel).await {
                        error!(
                            "FUSE recall listener: Failed to subscribe to channel {}: {:?}",
                            channel, e
                        );
                        return;
                    }
                    info!("FUSE recall listener: Subscribed to channel {}", channel);

                    use futures::StreamExt;
                    let mut message_stream = pubsub.on_message();
                    while let Some(msg) = message_stream.next().await {
                        let payload_res: Result<String, _> = msg.get_payload();
                        if let Ok(payload) = payload_res {
                            if let Ok(inode) = payload.parse::<u64>() {
                                info!("FUSE recall listener: Received recall for inode {}", inode);
                                let dlm_inner = dlm_clone.clone();
                                let delegations_inner = delegations_clone.clone();
                                let posix_locks_inner = posix_locks_clone.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_recall(
                                        inode,
                                        &dlm_inner,
                                        &delegations_inner,
                                        &posix_locks_inner,
                                    )
                                    .await
                                    {
                                        error!("FUSE recall listener: Failed to handle recall for inode {}: {:?}", inode, e);
                                    }
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "FUSE recall listener: Failed to get pub/sub connection: {:?}",
                        e
                    );
                }
            }
        });

        Ok(ReplyInit {
            max_write: std::num::NonZeroU32::new(1048576).unwrap(), // 1MB absolute maximum write buffer size
        })
    }

    async fn destroy(&self, _req: Request) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        info!("FUSE Daemon: Destroying mount. Checking for pending staged writes...");

        // Gracefully wait up to self.dismount_wait seconds for background workers to drain staged writes and active writes to S3
        let start_wait = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(self.dismount_wait);
        let (staged_count, active_writes_count) = loop {
            let keys = self.router.cache.nvme.list_staged_files();
            let mut current_staged = 0;
            let mut current_active = 0;
            for key in keys {
                if key.starts_with("active_block:") {
                    current_active += 1;
                } else {
                    current_staged += 1;
                }
            }
            if (current_staged == 0 && current_active == 0) || start_wait.elapsed() >= max_wait {
                break (current_staged, current_active);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };

        if staged_count > 0 || active_writes_count > 0 {
            warn!(
                "WARNING: SqueezeFS dismounted with unflushed data! Remaining local staged files: {}, active write directories: {}. Other nodes may see inconsistent filesystem state until these are recovered or flushed.",
                staged_count, active_writes_count
            );
        } else {
            info!("FUSE Daemon: Dismount clean. All write staged blocks successfully flushed to backend.");
        }
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

        if parent == 1 && name_str == ".stats" {
            let stats_data = self.generate_stats_json().await;
            let attr = self.get_stats_attr(stats_data.len() as u64);
            return Ok(ReplyEntry {
                ttl: Duration::from_secs(0), // dynamic stats shouldn't be cached long
                attr,
                generation: 1,
            });
        }

        let lookup_future = async {
            let child_ino = if let Some(cached_map) = self.dir_entry_cache.get(&parent) {
                if let Some(&ino) = cached_map.get(&*name_str) {
                    ino
                } else {
                    return Err(Errno::from(libc::ENOENT));
                }
            } else {
                let mut con = self
                    .dlm
                    .get_connection_for_inode(parent)
                    .await
                    .map_err(map_squeezefs_err)?;
                let dir_key = format!("squeezefs:dir:{}", parent);
                let entries_map: std::collections::HashMap<String, u64> =
                    con.hgetall(&dir_key).await.map_err(map_err)?;
                let entries_arc = std::sync::Arc::new(entries_map);
                self.dir_entry_cache.insert(parent, entries_arc.clone());
                if let Some(&ino) = entries_arc.get(&*name_str) {
                    ino
                } else {
                    return Err(Errno::from(libc::ENOENT));
                }
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
        };

        match tokio::time::timeout(get_fuse_timeout(), lookup_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE Lookup timeout parent = {}, name = {}",
                    parent, name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        if ino == STATS_INODE {
            let stats_data = self.generate_stats_json().await;
            let attr = self.get_stats_attr(stats_data.len() as u64);
            return Ok(ReplyAttr {
                ttl: Duration::from_secs(0),
                attr,
            });
        }

        let getattr_future = async {
            let attr = self
                .get_attr_internal(ino)
                .await
                .map_err(map_squeezefs_err)?;

            Ok(ReplyAttr {
                ttl: Duration::from_secs(1),
                attr,
            })
        };

        match tokio::time::timeout(get_fuse_timeout(), getattr_future).await {
            Ok(res) => res,
            Err(_) => {
                error!("FUSE Getattr timeout on inode {}", ino);
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let mknod_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;

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

                self.router.metadata_cache.insert(
                    format!("inode_{}", new_ino),
                    crate::routing::CachedMetadata {
                        file_type: "inline".to_string(),
                        size: 0,
                        block_map_id: None,
                        block_prefix: None,
                        file_id: None,
                        cached_at: std::time::Instant::now(),
                        data_key: None,
                    },
                );
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
        };

        match tokio::time::timeout(get_fuse_timeout(), mknod_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE mknod timeout parent = {}, name = {}",
                    parent, name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let create_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;

            let dir_key = format!("squeezefs:dir:{}", parent);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();

            let shard_count = self.dlm.shard_count() as u64;
            let (inodes_limit_str, used, new_ino): (Option<String>, Option<u64>, u64) =
                redis::pipe()
                    .cmd("HGET")
                    .arg("squeezefs:format")
                    .arg("inodes")
                    .cmd("GET")
                    .arg("squeezefs:used_inodes")
                    .cmd("INCRBY")
                    .arg("squeezefs:inode_counter")
                    .arg(shard_count)
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;

            let max_inodes = inodes_limit_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let max_inodes_val = if max_inodes > 0 { max_inodes } else { u64::MAX };

            if used.unwrap_or(0) >= max_inodes_val {
                let mut con_dec = self
                    .dlm
                    .get_connection_for_inode(parent)
                    .await
                    .map_err(map_squeezefs_err)?;
                let _: () = redis::cmd("DECRBY")
                    .arg("squeezefs:inode_counter")
                    .arg(shard_count)
                    .query_async(&mut con_dec)
                    .await
                    .map_err(map_err)?;
                return Err(Errno::from(libc::ENOSPC));
            }

            let inserted: bool = redis::cmd("HSETNX")
                .arg(&dir_key)
                .arg(&*name_str)
                .arg(new_ino)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;

            if !inserted {
                return Err(Errno::from(libc::EEXIST));
            }

            let attr_key = format!("squeezefs:attr:{}", new_ino);
            let meta_key = format!("metadata:inode_{}", new_ino);
            let parent_attr_key = format!("squeezefs:attr:{}", parent);

            let mut pipe = redis::pipe();
            pipe.atomic();
            pipe.hset_multiple(
                &attr_key,
                &[
                    ("ino", new_ino.to_string()),
                    ("size", "0".to_string()),
                    ("blocks", "0".to_string()),
                    ("kind", "1".to_string()),
                    ("perm", (mode as u16 & 0o7777).to_string()),
                    ("nlink", "1".to_string()),
                    ("uid", req.uid.to_string()),
                    ("gid", req.gid.to_string()),
                    ("atime_sec", sec.to_string()),
                    ("atime_nsec", nsec.to_string()),
                    ("mtime_sec", sec.to_string()),
                    ("mtime_nsec", nsec.to_string()),
                    ("ctime_sec", sec.to_string()),
                    ("ctime_nsec", nsec.to_string()),
                ],
            );
            pipe.hset_multiple(
                &meta_key,
                &[("type", "inline".to_string()), ("size", "0".to_string())],
            );
            pipe.cmd("INCRBY").arg("squeezefs:used_inodes").arg(1);
            pipe.hset_multiple(
                &parent_attr_key,
                &[
                    ("mtime_sec", sec.to_string()),
                    ("mtime_nsec", nsec.to_string()),
                    ("ctime_sec", sec.to_string()),
                    ("ctime_nsec", nsec.to_string()),
                ],
            );

            let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;

            self.attr_cache.remove(&parent);
            self.dir_entry_cache.invalidate(&parent);

            let attr = self
                .get_attr_internal(new_ino)
                .await
                .map_err(map_squeezefs_err)?;

            Ok(ReplyCreated {
                ttl: Duration::from_secs(1),
                attr,
                generation: 1,
                fh: new_ino,
                flags: 0,
            })
        };

        match tokio::time::timeout(get_fuse_timeout(), create_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE Create timeout parent = {}, name = {}",
                    parent, name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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
                    backing: None,
                });
            }
            let start = offset as usize;
            let end = std::cmp::min(bytes.len(), start + size as usize);
            return Ok(ReplyData {
                data: bytes[start..end].to_vec().into(),
                backing: None,
            });
        }

        if ino == STATS_INODE {
            let stats_data = self.generate_stats_json().await;
            let bytes = stats_data.into_bytes();
            if offset >= bytes.len() as u64 {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                    backing: None,
                });
            }
            let start = offset as usize;
            let end = std::cmp::min(bytes.len(), start + size as usize);
            return Ok(ReplyData {
                data: bytes[start..end].to_vec().into(),
                backing: None,
            });
        }

        let file_path = format!("inode_{}", ino);

        // Get file size to bound the read
        let file_size = if let Some(entry) = self.attr_cache.get(&ino) {
            entry.value().0.size
        } else {
            let mut con = self
                .dlm
                .get_connection_for_inode(ino)
                .await
                .map_err(map_squeezefs_err)?;
            let attr_key = format!("squeezefs:attr:{}", ino);
            let size_opt: Option<u64> = con.hget(&attr_key, "size").await.map_err(map_err)?;
            size_opt.unwrap_or(0)
        };

        if offset >= file_size {
            return Ok(ReplyData {
                data: Vec::new().into(),
                backing: None,
            });
        }

        let read_len = std::cmp::min(size as u64, file_size - offset) as usize;

        // 1. Try to read from committed/cached storage zero-copy
        let read_future =
            self.router
                .read_file_range_zero_copy(&file_path, offset, read_len as u32);
        let (data, backing) = match tokio::time::timeout(get_fuse_timeout(), read_future).await {
            Ok(Ok(res)) => res,
            Ok(Err(e)) => {
                error!("FUSE Read error: {:?}", e);
                return Err(map_squeezefs_err(e));
            }
            Err(_) => {
                error!("FUSE Read timeout");
                return Err(Errno::from(libc::ETIMEDOUT));
            }
        };

        Ok(ReplyData { data, backing })
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

        if ino == STATS_INODE {
            return Err(Errno::from(libc::EACCES));
        }

        let write_future = async {
            // Acquire local inode lock for the ENTIRE write operation to serialize
            // concurrent/subsequent writes to the same file.
            // 1. Get or acquire lease (fencing token)
            let fencing_token = self
                .get_or_acquire_lease(ino)
                .await
                .map_err(map_squeezefs_err)?;

            let mut con = self
                .dlm
                .get_connection_for_inode(ino)
                .await
                .map_err(map_squeezefs_err)?;

            let attr_key = format!("squeezefs:attr:{}", ino);
            let meta_key = format!("metadata:inode_{}", ino);
            let file_path = format!("inode_{}", ino);

            let cached_size = self.attr_cache.get(&ino).map(|e| e.value().0.size);
            let cached_meta = self.router.metadata_cache.get(&file_path);

            let file_type;

            let (old_size, is_striped) = match (cached_size, cached_meta.clone()) {
                (Some(s), Some(m)) => {
                    file_type = m.file_type.clone();
                    (s, m.file_type == "striped")
                }
                _ => {
                    let mut pipe = redis::pipe();
                    pipe.cmd("HGET").arg(&attr_key).arg("size");
                    pipe.cmd("HGET").arg(&meta_key).arg("type");
                    let (size_str, type_str): (Option<String>, Option<String>) =
                        pipe.query_async(&mut con).await.map_err(map_err)?;
                    let size = size_str.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                    file_type = type_str.unwrap_or_else(|| "inline".to_string());
                    let striped = file_type == "striped";
                    (size, striped)
                }
            };

            let bytes_written = data.len() as u32;
            let expected_new_size = std::cmp::max(old_size, offset + bytes_written as u64);
            if expected_new_size > old_size {
                let diff = expected_new_size - old_size;
                self.check_capacity_quota(&mut con, diff).await?;
            }

            let fits_inline =
                expected_new_size <= 65536 && file_type != "staged" && file_type != "striped";

            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();
            let block_size = self.router.block_size.load(Ordering::Relaxed);
            let num_blocks = if is_striped {
                expected_new_size.div_ceil(block_size).to_string()
            } else {
                "0".to_string()
            };

            let mut pipe = redis::pipe();
            pipe.atomic();

            if expected_new_size > old_size {
                let diff = expected_new_size - old_size;
                pipe.hset_multiple(
                    &attr_key,
                    &[
                        ("size", expected_new_size.to_string()),
                        ("mtime_sec", sec.to_string()),
                        ("mtime_nsec", nsec.to_string()),
                        ("ctime_sec", sec.to_string()),
                        ("ctime_nsec", nsec.to_string()),
                    ],
                );
                pipe.cmd("HSET")
                    .arg(&meta_key)
                    .arg("size")
                    .arg(expected_new_size.to_string());
                if num_blocks != "0" {
                    pipe.cmd("HSET")
                        .arg(&meta_key)
                        .arg("num_blocks")
                        .arg(&num_blocks);
                }
                pipe.cmd("INCRBY")
                    .arg("squeezefs:used_bytes")
                    .arg(diff.to_string());
            } else {
                pipe.hset_multiple(
                    &attr_key,
                    &[
                        ("mtime_sec", sec.to_string()),
                        ("mtime_nsec", nsec.to_string()),
                        ("ctime_sec", sec.to_string()),
                        ("ctime_nsec", nsec.to_string()),
                    ],
                );
            }

            if fits_inline {
                let mut final_data = if old_size == 0 && offset == 0 {
                    Vec::new()
                } else {
                    let inline_key = format!("inline_data:{}", file_path);
                    let bytes: Option<Vec<u8>> = con.get(&inline_key).await.map_err(map_err)?;
                    if let Some(b) = bytes {
                        self.router
                            .get_crypto()
                            .process_read(&b)
                            .map_err(map_squeezefs_err)?
                    } else {
                        Vec::new()
                    }
                };

                if offset as usize + data.len() > final_data.len() {
                    final_data.resize(offset as usize + data.len(), 0);
                }
                final_data[offset as usize..offset as usize + data.len()].copy_from_slice(data);

                let packed = self
                    .router
                    .get_crypto()
                    .process_write(bytes::Bytes::copy_from_slice(&final_data))
                    .map_err(map_squeezefs_err)?;
                let inline_key = format!("inline_data:{}", file_path);

                pipe.hset(&meta_key, "type", "inline");
                pipe.set(&inline_key, packed.as_ref());

                let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;
                drop(con);

                self.router.metadata_cache.insert(
                    file_path.clone(),
                    crate::routing::CachedMetadata {
                        file_type: "inline".to_string(),
                        size: expected_new_size,
                        block_map_id: None,
                        block_prefix: None,
                        file_id: None,
                        cached_at: std::time::Instant::now(),
                        data_key: None,
                    },
                );
            } else {
                let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;
                drop(con); // PREVENT DEADLOCK

                if is_striped {
                    self.write_file_staged(ino, offset, data, old_size, fencing_token)
                        .await
                        .map_err(map_squeezefs_err)?;
                    self.router.cache.write_lru.remove(&file_path);
                    self.router.cache.read_lru.remove(&file_path);
                } else {
                    self.router
                        .write_file(&file_path, offset, data, fencing_token)
                        .await
                        .map_err(map_squeezefs_err)?;
                }
                self.router.metadata_cache.remove(&file_path);
            }

            // Update local attr_cache securely by briefly acquiring the lock
            let lock = self.get_inode_lock(ino);
            let _guard = lock.write().await;
            if let Some(mut entry) = self.attr_cache.get_mut(&ino) {
                entry.value_mut().0.size = expected_new_size;
                entry.value_mut().0.blocks = expected_new_size.div_ceil(512);
                entry.value_mut().0.mtime = Timestamp::new(sec, nsec);
                entry.value_mut().0.ctime = Timestamp::new(sec, nsec);
                entry.value_mut().1 = std::time::Instant::now();
            }

            Ok(ReplyWrite {
                written: bytes_written,
            })
        };

        match tokio::time::timeout(get_fuse_timeout(), write_future).await {
            Ok(res) => res,
            Err(_) => {
                error!("FUSE Write timeout on inode {}", ino);
                Err(Errno::from(libc::ETIMEDOUT))
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

        let mkdir_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;

            let dir_key = format!("squeezefs:dir:{}", parent);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();

            let shard_count = self.dlm.shard_count() as u64;
            let (inodes_limit_str, used, new_ino): (Option<String>, Option<u64>, u64) =
                redis::pipe()
                    .cmd("HGET")
                    .arg("squeezefs:format")
                    .arg("inodes")
                    .cmd("GET")
                    .arg("squeezefs:used_inodes")
                    .cmd("INCRBY")
                    .arg("squeezefs:inode_counter")
                    .arg(shard_count)
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;

            let max_inodes = inodes_limit_str
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let max_inodes_val = if max_inodes > 0 { max_inodes } else { u64::MAX };

            if used.unwrap_or(0) >= max_inodes_val {
                let mut con_dec = self
                    .dlm
                    .get_connection_for_inode(parent)
                    .await
                    .map_err(map_squeezefs_err)?;
                let _: () = redis::cmd("DECRBY")
                    .arg("squeezefs:inode_counter")
                    .arg(shard_count)
                    .query_async(&mut con_dec)
                    .await
                    .map_err(map_err)?;
                return Err(Errno::from(libc::ENOSPC));
            }

            let inserted: bool = redis::cmd("HSETNX")
                .arg(&dir_key)
                .arg(&*name_str)
                .arg(new_ino)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;

            if !inserted {
                return Err(Errno::from(libc::EEXIST));
            }

            let attr_key = format!("squeezefs:attr:{}", new_ino);
            let meta_key = format!("metadata:inode_{}", new_ino);
            let child_dir_key = format!("squeezefs:dir:{}", new_ino);
            let parent_attr_key = format!("squeezefs:attr:{}", parent);

            let mut pipe = redis::pipe();
            pipe.atomic();
            pipe.hset_multiple(
                &attr_key,
                &[
                    ("ino", new_ino.to_string()),
                    ("size", "4096".to_string()),
                    ("blocks", "8".to_string()),
                    ("kind", "2".to_string()),
                    ("perm", (mode as u16 & 0o7777).to_string()),
                    ("nlink", "2".to_string()),
                    ("uid", req.uid.to_string()),
                    ("gid", req.gid.to_string()),
                    ("atime_sec", sec.to_string()),
                    ("atime_nsec", nsec.to_string()),
                    ("mtime_sec", sec.to_string()),
                    ("mtime_nsec", nsec.to_string()),
                    ("ctime_sec", sec.to_string()),
                    ("ctime_nsec", nsec.to_string()),
                ],
            );
            pipe.hset_multiple(
                &meta_key,
                &[("type", "inline".to_string()), ("size", "0".to_string())],
            );
            pipe.hset_multiple(
                &child_dir_key,
                &[(".", new_ino.to_string()), ("..", parent.to_string())],
            );
            pipe.cmd("INCRBY").arg("squeezefs:used_inodes").arg(1);
            pipe.cmd("HINCRBY")
                .arg(&parent_attr_key)
                .arg("nlink")
                .arg(1);
            pipe.hset_multiple(
                &parent_attr_key,
                &[
                    ("mtime_sec", sec.to_string()),
                    ("mtime_nsec", nsec.to_string()),
                    ("ctime_sec", sec.to_string()),
                    ("ctime_nsec", nsec.to_string()),
                ],
            );

            let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;

            self.attr_cache.remove(&parent);
            self.dir_entry_cache.invalidate(&parent);

            let attr = self
                .get_attr_internal(new_ino)
                .await
                .map_err(map_squeezefs_err)?;

            Ok(ReplyEntry {
                ttl: Duration::from_secs(1),
                attr,
                generation: 1,
            })
        };

        match tokio::time::timeout(get_fuse_timeout(), mkdir_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE mkdir timeout parent = {}, name = {}",
                    parent, name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
    }

    async fn rmdir(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        let name_str = name.to_string_lossy();
        debug!("FUSE rmdir: parent = {}, name = {}", parent, name_str);

        let rmdir_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;

            let dir_key = format!("squeezefs:dir:{}", parent);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();

            let ino_str: Option<String> = con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
            let ino = match ino_str {
                Some(s) => s.parse::<u64>().unwrap_or(0),
                None => return Err(Errno::from(libc::ENOENT)),
            };

            let child_dir_key = format!("squeezefs:dir:{}", ino);

            loop {
                let _: () = redis::cmd("WATCH")
                    .arg(&child_dir_key)
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;

                let size: u64 = redis::cmd("HLEN")
                    .arg(&child_dir_key)
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;
                if size > 2 {
                    let _: () = redis::cmd("UNWATCH")
                        .query_async(&mut con)
                        .await
                        .map_err(map_err)?;
                    return Err(Errno::from(libc::ENOTEMPTY));
                }

                let mut pipe = redis::pipe();
                pipe.atomic();
                pipe.cmd("HDEL").arg(&dir_key).arg(&*name_str);
                pipe.cmd("DEL").arg(&child_dir_key);

                let res: Option<Vec<i64>> = pipe.query_async(&mut con).await.map_err(map_err)?;
                if res.is_some() {
                    break;
                }
            }

            let child_attr_key = format!("squeezefs:attr:{}", ino);
            let child_meta_key = format!("metadata:inode_{}", ino);
            let child_inline_key = format!("inline_data:inode_{}", ino);

            let mut pipe = redis::pipe();
            pipe.cmd("DEL").arg(&child_attr_key);
            pipe.cmd("DEL").arg(&child_meta_key);
            pipe.cmd("DEL").arg(&child_inline_key);
            pipe.cmd("DECR").arg("squeezefs:used_inodes");
            let _: () = pipe.query_async(&mut con).await.map_err(map_err)?;

            let parent_attr_key = format!("squeezefs:attr:{}", parent);
            let parent_nlink: i64 = con.hget(&parent_attr_key, "nlink").await.unwrap_or(2);
            let mut new_nlink = parent_nlink - 1;
            if new_nlink < 1 {
                new_nlink = 1;
            }
            let _: () = redis::cmd("HSET")
                .arg(&parent_attr_key)
                .arg("nlink")
                .arg(new_nlink)
                .arg("mtime_sec")
                .arg(sec)
                .arg("mtime_nsec")
                .arg(nsec)
                .arg("ctime_sec")
                .arg(sec)
                .arg("ctime_nsec")
                .arg(nsec)
                .query_async(&mut con)
                .await
                .map_err(map_err)?;

            // Invalidate caches
            self.attr_cache.remove(&ino);
            self.attr_cache.remove(&parent);
            self.dir_entry_cache.invalidate(&parent);
            self.dir_entry_cache.invalidate(&ino);
            let file_path = format!("inode_{}", ino);
            self.router.metadata_cache.remove(&file_path);

            if let Some((_, lease)) = self.active_leases.remove(&ino) {
                let _ = lease.release().await;
            }
            self.active_inode_locks.remove(&ino);
            self.active_posix_locks.retain(|key, _| key.0 != ino);

            Ok(())
        };

        match tokio::time::timeout(get_fuse_timeout(), rmdir_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE rmdir timeout parent = {}, name = {}",
                    parent, name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let setattr_future = async {
            let lock = self.get_inode_lock(ino);
            let _guard = lock.write().await;

            let mut con = self
                .dlm
                .get_connection_for_inode(ino)
                .await
                .map_err(map_squeezefs_err)?;
            let attr_key = format!("squeezefs:attr:{}", ino);

            // Check if inode exists first
            let exists: bool = con.exists(&attr_key).await.map_err(map_err)?;
            if !exists {
                return Err(Errno::from(libc::ENOENT));
            }

            let mut old_size = 0u64;
            if set_attr.size.is_some() {
                let old_size_opt: Option<u64> =
                    con.hget(&attr_key, "size").await.map_err(map_err)?;
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

            drop(_guard);
            if std::sync::Arc::strong_count(&lock) <= 2 {
                self.active_inode_locks.remove(&ino);
            }

            Ok(ReplyAttr {
                ttl: Duration::from_secs(1),
                attr,
            })
        };

        match tokio::time::timeout(get_fuse_timeout(), setattr_future).await {
            Ok(res) => res,
            Err(_) => {
                error!("FUSE Setattr timeout on inode {}", ino);
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let symlink_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;

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
        };

        match tokio::time::timeout(get_fuse_timeout(), symlink_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE symlink timeout parent = {}, name = {}",
                    parent, name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
    }

    async fn readlink(&self, _req: Request, ino: u64) -> FuseResult<ReplyData> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let mut con = self
            .dlm
            .get_connection_for_inode(ino)
            .await
            .map_err(map_squeezefs_err)?;
        let symlink_key = format!("squeezefs:symlink:{}", ino);
        let target: Option<String> = con.get(&symlink_key).await.map_err(map_err)?;

        let target_str = match target {
            Some(t) => t,
            None => return Err(Errno::from(libc::ENOENT)),
        };

        Ok(ReplyData {
            data: target_str.into_bytes().into(),
            backing: None,
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

        let link_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(new_parent)
                .await
                .map_err(map_squeezefs_err)?;

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
        };

        match tokio::time::timeout(get_fuse_timeout(), link_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE link timeout ino = {}, new_parent = {}, new_name = {}",
                    ino, new_parent, new_name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let unlink_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;

            let dir_key = format!("squeezefs:dir:{}", parent);
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();

            let ino = if let Some(cached_ino) = self
                .dir_entry_cache
                .get(&parent)
                .and_then(|map| map.get(&*name_str).copied())
            {
                cached_ino
            } else {
                let ino_str: Option<String> =
                    con.hget(&dir_key, &*name_str).await.map_err(map_err)?;
                match ino_str {
                    Some(s) => s.parse::<u64>().unwrap_or(0),
                    None => return Err(Errno::from(libc::ENOENT)),
                }
            };

            let child_attr_key = format!("squeezefs:attr:{}", ino);
            let meta_key = format!("metadata:inode_{}", ino);

            let mut child_con = self
                .dlm
                .get_connection_for_inode(ino)
                .await
                .map_err(map_squeezefs_err)?;

            let mut pipe = redis::pipe();
            pipe.cmd("HGET").arg(&child_attr_key).arg("kind");
            pipe.cmd("HGET").arg(&child_attr_key).arg("size");
            pipe.cmd("HGET").arg(&meta_key).arg("type");
            let (kind_str, size_str, type_str): (Option<String>, Option<String>, Option<String>) =
                pipe.query_async(&mut child_con).await.map_err(map_err)?;

            let kind = kind_str.and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
            if kind == 2 {
                return Err(Errno::from(libc::EISDIR));
            }

            let deleted: i64 = con.hdel(&dir_key, &*name_str).await.map_err(map_err)?;
            if deleted == 0 {
                return Err(Errno::from(libc::ENOENT));
            }

            let mut new_nlink: i64 = child_con
                .hincr(&child_attr_key, "nlink", -1)
                .await
                .map_err(map_err)?;

            if new_nlink < 0 {
                new_nlink = 0;
                let _: () = child_con
                    .hset(&child_attr_key, "nlink", 0)
                    .await
                    .map_err(map_err)?;
            }

            let file_size = size_str.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let file_type = type_str.unwrap_or_else(|| "inline".to_string());

            let parent_attr_key = format!("squeezefs:attr:{}", parent);

            let mut parent_pipe = redis::pipe();
            parent_pipe.hset_multiple(
                &parent_attr_key,
                &[
                    ("mtime_sec", sec.to_string()),
                    ("mtime_nsec", nsec.to_string()),
                    ("ctime_sec", sec.to_string()),
                    ("ctime_nsec", nsec.to_string()),
                ],
            );
            let _: () = parent_pipe.query_async(&mut con).await.map_err(map_err)?;

            if new_nlink == 0 {
                // Delete metadata and data completely
                let file_path = format!("inode_{}", ino);
                let inline_key = format!("inline_data:{}", file_path);
                let meta_key_del = format!("metadata:{}", file_path);
                let symlink_key = format!("squeezefs:symlink:{}", ino);

                if file_type != "inline" {
                    self.router
                        .delete_file(&file_path, &mut child_con)
                        .await
                        .map_err(map_squeezefs_err)?;
                }

                let mut child_del_pipe = redis::pipe();
                child_del_pipe
                    .del(&child_attr_key)
                    .del(&inline_key)
                    .del(&meta_key_del)
                    .del(&symlink_key);
                let _: () = child_del_pipe
                    .query_async(&mut child_con)
                    .await
                    .map_err(map_err)?;

                let mut global_con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;
                let mut global_pipe = redis::pipe();
                global_pipe
                    .decr("squeezefs:used_bytes", file_size)
                    .decr("squeezefs:used_inodes", 1);
                let _: () = global_pipe
                    .query_async(&mut global_con)
                    .await
                    .map_err(map_err)?;

                if let Some((_, lease)) = self.active_leases.remove(&ino) {
                    let _ = lease.release().await;
                }
                self.active_inode_locks.remove(&ino);
                self.active_posix_locks.retain(|key, _| key.0 != ino);
            }

            // Invalidate attr_cache, router metadata_cache, and dir_entry_cache
            self.attr_cache.remove(&ino);
            self.attr_cache.remove(&parent);
            self.dir_entry_cache.invalidate(&parent);
            let file_path = format!("inode_{}", ino);
            self.router.metadata_cache.remove(&file_path);

            Ok(())
        };

        match tokio::time::timeout(get_fuse_timeout(), unlink_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE unlink timeout parent = {}, name = {}",
                    parent, name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let rename_future = async {
            let mut parent_con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;
            let mut new_parent_con = self
                .dlm
                .get_connection_for_inode(new_parent)
                .await
                .map_err(map_squeezefs_err)?;
            let mut global_con = self.dlm.get_connection().await.map_err(map_squeezefs_err)?;

            let src_dir_key = format!("squeezefs:dir:{}", parent);
            let dest_dir_key = format!("squeezefs:dir:{}", new_parent);

            let ino_opt: Option<u64> = parent_con
                .hget(&src_dir_key, &*name_str)
                .await
                .map_err(map_err)?;
            let ino = match ino_opt {
                Some(i) => i,
                None => return Err(Errno::from(libc::ENOENT)),
            };

            let attr_key = format!("squeezefs:attr:{}", ino);
            let src_kind: u8 = parent_con
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
                    let mut ancestor_con = self
                        .dlm
                        .get_connection_for_inode(ancestor)
                        .await
                        .map_err(map_squeezefs_err)?;
                    let parent_of_ancestor: Option<u64> = ancestor_con
                        .hget(&ancestor_dir_key, "..")
                        .await
                        .map_err(map_err)?;
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
            let dest_ino_opt: Option<u64> = new_parent_con
                .hget(&dest_dir_key, &*new_name_str)
                .await
                .map_err(map_err)?;
            if let Some(dest_ino) = dest_ino_opt {
                // Overwrite existing file or directory
                let dest_attr_key = format!("squeezefs:attr:{}", dest_ino);
                let dest_kind: u8 = new_parent_con
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

                let dest_file_path = format!("inode_{}", dest_ino);
                if dest_kind == 2 {
                    // If it is a directory, it must be empty
                    let child_dest_dir_key = format!("squeezefs:dir:{}", dest_ino);
                    let keys: Vec<String> = new_parent_con
                        .hkeys(&child_dest_dir_key)
                        .await
                        .map_err(map_err)?;
                    for k in keys {
                        if k != "." && k != ".." {
                            return Err(Errno::from(libc::ENOTEMPTY));
                        }
                    }
                    let _: () = redis::pipe()
                        .del(&child_dest_dir_key)
                        .query_async(&mut new_parent_con)
                        .await
                        .map_err(map_err)?;
                } else {
                    // Delete data blocks via router
                    let _ = self
                        .router
                        .delete_file(&dest_file_path, &mut global_con)
                        .await;
                }

                // Delete Redis keys
                let inline_key = format!("inline_data:{}", dest_file_path);
                let meta_key = format!("metadata:{}", dest_file_path);
                let symlink_key = format!("squeezefs:symlink:{}", dest_ino);

                // Fetch target size first to decrement used_bytes
                let file_size_opt: Option<u64> = new_parent_con
                    .hget(&dest_attr_key, "size")
                    .await
                    .map_err(map_err)?;
                let file_size = file_size_opt.unwrap_or(0);

                let mut pipe = redis::pipe();
                pipe.del(&dest_attr_key)
                    .del(&inline_key)
                    .del(&meta_key)
                    .del(&symlink_key);
                let _: () = pipe
                    .query_async(&mut new_parent_con)
                    .await
                    .map_err(map_err)?;

                let mut global_pipe = redis::pipe();
                if dest_kind != 2 {
                    global_pipe.decr("squeezefs:used_bytes", file_size);
                }
                global_pipe.decr("squeezefs:used_inodes", 1);
                let _: () = global_pipe
                    .query_async(&mut global_con)
                    .await
                    .map_err(map_err)?;

                // Invalidate caches
                self.attr_cache.remove(&dest_ino);
                self.router.metadata_cache.remove(&dest_file_path);

                // Clean up leases, inode locks, POSIX locks
                if let Some((_, lease)) = self.active_leases.remove(&dest_ino) {
                    let _ = lease.release().await;
                }
                self.active_inode_locks.remove(&dest_ino);
                self.active_posix_locks.retain(|key, _| key.0 != dest_ino);
            }

            // Perform rename atomically
            let _: () = parent_con
                .hdel(&src_dir_key, &*name_str)
                .await
                .map_err(map_err)?;
            let _: () = new_parent_con
                .hset(&dest_dir_key, &*new_name_str, ino)
                .await
                .map_err(map_err)?;

            // If renamed inode is a directory, update its ".." entry
            if src_kind == 2 {
                let child_dir_key = format!("squeezefs:dir:{}", ino);
                let _: () = parent_con
                    .hset(&child_dir_key, "..", new_parent)
                    .await
                    .map_err(map_err)?;

                // Adjust link counts if parents changed
                if parent != new_parent {
                    let old_parent_attr_key = format!("squeezefs:attr:{}", parent);
                    let new_parent_attr_key = format!("squeezefs:attr:{}", new_parent);
                    let _: Result<(), redis::RedisError> =
                        parent_con.hincr(&old_parent_attr_key, "nlink", -1).await;
                    let _: Result<(), redis::RedisError> =
                        new_parent_con.hincr(&new_parent_attr_key, "nlink", 1).await;
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
                .query_async(&mut parent_con)
                .await
                .map_err(map_err)?;

            // Update parent directory timestamps!
            self.update_parent_timestamps(&mut parent_con, parent)
                .await
                .map_err(map_err)?;
            if parent != new_parent {
                self.update_parent_timestamps(&mut new_parent_con, new_parent)
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
        };

        match tokio::time::timeout(get_fuse_timeout(), rename_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE rename timeout parent = {}, name = {}, new_parent = {}, new_name = {}",
                    parent, name_str, new_parent, new_name_str
                );
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let readdir_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;
            let entries_map = if let Some(cached_map) = self.dir_entry_cache.get(&parent) {
                cached_map
            } else {
                let dir_key = format!("squeezefs:dir:{}", parent);
                let map: std::collections::HashMap<String, u64> =
                    con.hgetall(&dir_key).await.map_err(map_err)?;
                let map_arc = std::sync::Arc::new(map);
                self.dir_entry_cache.insert(parent, map_arc.clone());
                map_arc
            };

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

            if parent == 1 && !entries_map.contains_key(".stats") {
                let offset = (entries.len() + 1) as i64;
                entries.push(DirectoryEntry {
                    name: ".stats".into(),
                    kind: FileType::RegularFile,
                    inode: STATS_INODE,
                    offset,
                });
            }

            let mut child_inos = Vec::new();
            for (name, child_ino) in entries_map.iter() {
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
            for (name, child_ino) in entries_map.iter() {
                if name == "." || name == ".." {
                    continue;
                }
                let kind = kind_map
                    .get(child_ino)
                    .cloned()
                    .unwrap_or(FileType::RegularFile);

                entries.push(DirectoryEntry {
                    name: name.clone().into(),
                    kind,
                    inode: *child_ino,
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
        };

        match tokio::time::timeout(get_fuse_timeout(), readdir_future).await {
            Ok(res) => res,
            Err(_) => {
                error!("FUSE readdir timeout parent = {}", parent);
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        let readdirplus_future = async {
            let mut con = self
                .dlm
                .get_connection_for_inode(parent)
                .await
                .map_err(map_squeezefs_err)?;
            let entries_map = if let Some(cached_map) = self.dir_entry_cache.get(&parent) {
                cached_map
            } else {
                let dir_key = format!("squeezefs:dir:{}", parent);
                let map: std::collections::HashMap<String, u64> =
                    con.hgetall(&dir_key).await.map_err(map_err)?;
                let map_arc = std::sync::Arc::new(map);
                self.dir_entry_cache.insert(parent, map_arc.clone());
                map_arc
            };

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

            if parent == 1 && !entries_map.contains_key(".stats") {
                let stats_data = self.generate_stats_json().await;
                let attr = self.get_stats_attr(stats_data.len() as u64);
                let offset = (entries.len() + 1) as i64;
                entries.push(DirectoryEntryPlus {
                    name: ".stats".into(),
                    kind: FileType::RegularFile,
                    inode: STATS_INODE,
                    generation: 1,
                    attr,
                    entry_ttl: Duration::from_secs(0),
                    attr_ttl: Duration::from_secs(0),
                    offset,
                });
            }

            // 1. Gather all inodes we need attributes for that AREN'T in local cache
            let mut pipe = redis::pipe();
            let mut inos_to_fetch = Vec::new();

            for (name, child_ino) in entries_map.iter() {
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
            for (name, child_ino) in entries_map.iter() {
                if name == "." || name == ".." {
                    continue;
                }
                let attr = match self.get_attr_internal(*child_ino).await {
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
                    name: name.clone().into(),
                    kind,
                    inode: *child_ino,
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
        };

        match tokio::time::timeout(get_fuse_timeout(), readdirplus_future).await {
            Ok(res) => res,
            Err(_) => {
                error!("FUSE readdirplus timeout parent = {}", parent);
                Err(Errno::from(libc::ETIMEDOUT))
            }
        }
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

        // 1. Acquire local locks on both inodes to ensure consistency and prevent deadlocks
        let src_lock_arc = self.get_inode_lock(inode);
        let dest_lock_arc = if inode != inode_out {
            Some(self.get_inode_lock(inode_out))
        } else {
            None
        };

        let _src_read_guard;
        let _src_write_guard;
        let _dest_write_guard;
        if inode == inode_out {
            _src_write_guard = Some(src_lock_arc.write().await);
            _src_read_guard = None;
            _dest_write_guard = None;
        } else if inode < inode_out {
            _src_read_guard = Some(src_lock_arc.read().await);
            _dest_write_guard = Some(dest_lock_arc.as_ref().unwrap().write().await);
            _src_write_guard = None;
        } else {
            _dest_write_guard = Some(dest_lock_arc.as_ref().unwrap().write().await);
            _src_read_guard = Some(src_lock_arc.read().await);
            _src_write_guard = None;
        }

        // Sort paths lexicographically to prevent deadlocks under concurrent operations.
        let (src_lease, dest_lease) = if inode == inode_out {
            if self.active_leases.contains_key(&inode) {
                (None, None)
            } else {
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
            }
        } else {
            let src_already_held = self.active_leases.contains_key(&inode);
            let dest_already_held = self.active_leases.contains_key(&inode_out);

            let (first_path, second_path) = if src_path < dest_path {
                (&src_path, &dest_path)
            } else {
                (&dest_path, &src_path)
            };

            let first_lease = if (src_path < dest_path && src_already_held)
                || (src_path >= dest_path && dest_already_held)
            {
                None
            } else {
                let l = match self
                    .dlm
                    .acquire_lock(first_path, None, Duration::from_secs(5))
                    .await
                {
                    Ok(l) => Some(l),
                    Err(e) => {
                        error!(
                            "copy_file_range: failed to acquire lock on first path {}: {:?}",
                            first_path, e
                        );
                        return Err(Errno::from(libc::EAGAIN));
                    }
                };
                l
            };

            let second_lease = if (src_path < dest_path && dest_already_held)
                || (src_path >= dest_path && src_already_held)
            {
                None
            } else {
                let l = match self
                    .dlm
                    .acquire_lock(second_path, None, Duration::from_secs(5))
                    .await
                {
                    Ok(l) => Some(l),
                    Err(e) => {
                        error!(
                            "copy_file_range: failed to acquire lock on second path {}: {:?}",
                            second_path, e
                        );
                        return Err(Errno::from(libc::EAGAIN));
                    }
                };
                l
            };

            if src_path < dest_path {
                (first_lease, second_lease)
            } else {
                (second_lease, first_lease)
            }
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
            drop(src_lease);
            drop(dest_lease);

            self.router
                .clone_file(&src_path, &dest_path)
                .await
                .map_err(map_squeezefs_err)?;

            // Update destination attributes size and times in Garnet
            if let Ok(mut con) = self.dlm.get_connection_for_inode(inode_out).await {
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
        let target_fencing_token = if let Some(ref dl) = dest_lease {
            dl.fencing_token()
        } else if let Some(ref sl) = src_lease {
            sl.fencing_token()
        } else if let Some(lease) = self.active_leases.get(&inode_out) {
            lease.fencing_token()
        } else if let Some(lease) = self.active_leases.get(&inode) {
            lease.fencing_token()
        } else {
            0
        };

        self.router
            .write_file(&dest_path, off_out, chunk, target_fencing_token)
            .await
            .map_err(map_squeezefs_err)?;

        // Update destination size and times in Garnet
        let copied_len = chunk.len() as u64;
        let new_dest_size = std::cmp::max(dest_size, off_out + copied_len);

        if let Ok(mut con) = self.dlm.get_connection_for_inode(inode_out).await {
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
        let shard_count = self.dlm.shard_count();
        let mut used_bytes = 0;
        let mut used_inodes = 0;
        for i in 0..shard_count {
            let mut shard_con = self
                .dlm
                .get_connection_for_inode(i as u64)
                .await
                .map_err(map_squeezefs_err)?;
            let ub: Option<u64> = shard_con
                .get("squeezefs:used_bytes")
                .await
                .map_err(map_err)?;
            let ui: Option<u64> = shard_con
                .get("squeezefs:used_inodes")
                .await
                .map_err(map_err)?;
            used_bytes += ub.unwrap_or(0);
            used_inodes += ui.unwrap_or(0);
        }

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
        // 1. Get or acquire lease (fencing token)
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // 2. Flush the staging blocks concurrently to backend (S3/RustFS)
        if let Err(e) = self
            .flush_active_blocks_with_retry(ino, fencing_token)
            .await
        {
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

        // Flush any remaining active staging blocks before releasing the lease
        if let Ok(fencing_token) = self.get_or_acquire_lease(ino).await {
            let _ = self
                .flush_active_blocks_with_retry(ino, fencing_token)
                .await;
        }

        if let Err(e) = self.complete_active_multipart_upload_if_any(ino).await {
            error!(
                "FUSE Release: Failed to complete multipart upload for inode {}: {:?}",
                ino, e
            );
        }

        // If there's a cached lease, release it and remove it from our active_leases map
        if let Some((_, lease)) = self.active_leases.remove(&ino) {
            let _ = lease.release().await;
        }

        // Release POSIX locks held by this lock owner on this inode
        let mut posix_to_remove = Vec::new();
        for entry in self.active_posix_locks.iter() {
            let &(lock_ino, lock_owner, lock_start, lock_end) = entry.key();
            if lock_ino == ino && lock_owner == _lock_owner {
                posix_to_remove.push((lock_ino, lock_owner, lock_start, lock_end));
            }
        }
        for key in posix_to_remove {
            if let Some((_, PosixLock::Global(lease))) = self.active_posix_locks.remove(&key) {
                let _ = lease.release().await;
            }
        }

        // Also clean up local inode lock if no longer needed (only if strong_count <= 2)
        let lock = self.get_inode_lock(ino);
        if std::sync::Arc::strong_count(&lock) <= 2 {
            self.active_inode_locks.remove(&ino);
        }

        Ok(())
    }

    async fn fsync(&self, _req: Request, ino: u64, _fh: u64, _datasync: bool) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Fsync: ino = {}, datasync = {}", ino, _datasync);

        // 1. Get or acquire lease (fencing token)
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // 2. Flush the staging blocks concurrently to backend (S3/RustFS)
        if let Err(e) = self
            .flush_active_blocks_with_retry(ino, fencing_token)
            .await
        {
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
            let mut con = self
                .dlm
                .get_connection_for_inode(ino)
                .await
                .map_err(map_squeezefs_err)?;
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
        self.attr_cache.remove(&ino);
        self.active_inode_locks.remove(&ino);
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

        if let Err(e) = self.ensure_delegation_held(inode).await {
            error!(
                "FUSE getlk: Failed to ensure delegation held for inode {}: {:?}",
                inode, e
            );
            return Err(map_squeezefs_err(e));
        }

        // Check local conflicts
        for entry in self.active_posix_locks.iter() {
            let &(lock_ino, lock_owner, lock_start, lock_end) = entry.key();
            if lock_ino == inode {
                let is_conflict = match entry.value() {
                    PosixLock::Local | PosixLock::Global(_) => {
                        lock_owner != _lock_owner
                            && std::cmp::max(lock_start, _start) <= std::cmp::min(lock_end, _end)
                    }
                    PosixLock::Remote { .. } => {
                        std::cmp::max(lock_start, _start) <= std::cmp::min(lock_end, _end)
                    }
                };

                if is_conflict {
                    debug!(
                        "FUSE getlk: conflict found locally with owner {} on range {}-{}",
                        lock_owner, lock_start, lock_end
                    );
                    let conflict_pid = if lock_owner == u64::MAX {
                        0
                    } else {
                        lock_owner as u32
                    };
                    return Ok(ReplyLock {
                        start: lock_start,
                        end: lock_end,
                        r#type: libc::F_WRLCK as u32,
                        pid: conflict_pid,
                    });
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

        if let Err(e) = self.ensure_delegation_held(inode).await {
            error!(
                "FUSE setlk: Failed to ensure delegation held for inode {}: {:?}",
                inode, e
            );
            return Err(map_squeezefs_err(e));
        }

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
                if let Some((_, PosixLock::Global(lease))) = self.active_posix_locks.remove(&key) {
                    if let Err(e) = lease.release().await {
                        error!("Failed to explicitly release lock lease: {:?}", e);
                    }
                }
            }
            return Ok(());
        }

        // If we already hold a lock on this exact range, release it first
        if let Some((_, PosixLock::Global(old_lease))) =
            self.active_posix_locks
                .remove(&(inode, _lock_owner, _start, _end))
        {
            let _ = old_lease.release().await;
        }

        let mut attempts = 0;
        let max_attempts = if _block { 20 } else { 1 };

        loop {
            let mut conflict = false;
            for entry in self.active_posix_locks.iter() {
                let &(lock_ino, lock_owner, lock_start, lock_end) = entry.key();
                if lock_ino == inode {
                    let is_conflict = match entry.value() {
                        PosixLock::Local | PosixLock::Global(_) => {
                            lock_owner != _lock_owner
                                && std::cmp::max(lock_start, _start)
                                    <= std::cmp::min(lock_end, _end)
                        }
                        PosixLock::Remote { .. } => {
                            std::cmp::max(lock_start, _start) <= std::cmp::min(lock_end, _end)
                        }
                    };

                    if is_conflict {
                        conflict = true;
                        break;
                    }
                }
            }

            if !conflict {
                debug!(
                    "FUSE setlk: successfully acquired lock range {}-{} for owner {} locally",
                    _start, _end, _lock_owner
                );
                self.active_posix_locks
                    .insert((inode, _lock_owner, _start, _end), PosixLock::Local);
                return Ok(());
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

        match cmd {
            SQUEEZEFS_IOC_GDS_READ => {
                // 1. Read GdsReadArgs from client process memory
                let pid = _req.pid;
                let arg = _arg;
                let args_res = tokio::task::spawn_blocking(move || {
                    let mut bytes = [0u8; std::mem::size_of::<GdsReadArgs>()];
                    use std::os::unix::fs::FileExt;
                    let mem_file =
                        std::fs::File::open(format!("/proc/{}/mem", pid)).map_err(|e| {
                            error!(
                                "GDS ioctl: failed to open client memory file for pid {}: {:?}",
                                pid, e
                            );
                            Errno::from(libc::EFAULT)
                        })?;
                    mem_file.read_exact_at(&mut bytes, arg).map_err(|e| {
                        error!(
                            "GDS ioctl: failed to read client memory at 0x{:X}: {:?}",
                            arg, e
                        );
                        Errno::from(libc::EFAULT)
                    })?;
                    let args: GdsReadArgs =
                        unsafe { std::ptr::read(bytes.as_ptr() as *const GdsReadArgs) };
                    Ok::<GdsReadArgs, Errno>(args)
                })
                .await;

                let args = match args_res {
                    Ok(Ok(a)) => a,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err(Errno::from(libc::EIO)),
                };
                debug!("GDS ioctl args: {:?}", args);

                // 2. Lock inode
                let lock = self.get_inode_lock(inode);
                let _guard = lock.read().await;

                // 3. Fetch metadata
                let file_path = format!("inode_{}", inode);
                let meta = match self.router.fetch_metadata(&file_path).await {
                    Ok(m) => m,
                    Err(e) => {
                        error!(
                            "GDS ioctl: failed to fetch metadata for inode {}: {:?}",
                            inode, e
                        );
                        return Err(map_squeezefs_err(e));
                    }
                };

                if meta.file_type != "striped" {
                    error!(
                        "GDS ioctl: only striped files support GDS, got layout: {}",
                        meta.file_type
                    );
                    return Err(Errno::from(libc::EINVAL));
                }

                let block_size = self.router.block_size.load(Ordering::Relaxed);
                let file_size = meta.size;

                if args.offset >= file_size {
                    return Ok(ReplyIoctl {
                        result: 0,
                        flags: 0,
                        in_iovs: 0,
                        out_iovs: 0,
                    });
                }

                let end_offset = std::cmp::min(args.offset + args.size, file_size);
                let start_block = (args.offset / block_size) as u32;
                let end_block = ((end_offset - 1) / block_size) as u32;

                let block_keys = match self
                    .router
                    .load_striped_block_keys(&file_path, &meta, start_block, end_block)
                    .await
                {
                    Ok(keys) => keys,
                    Err(e) => {
                        error!("GDS ioctl: failed to load block keys: {:?}", e);
                        return Err(map_squeezefs_err(e));
                    }
                };

                for (b_idx, key_opt) in block_keys {
                    let b_key = match key_opt {
                        Some(key) => key,
                        None => continue, // Sparse hole
                    };

                    let b_start_offset = b_idx as u64 * block_size;
                    let b_end_offset = b_start_offset + block_size;

                    let read_start = std::cmp::max(args.offset, b_start_offset);
                    let read_end = std::cmp::min(end_offset, b_end_offset);
                    let block_read_offset = read_start - b_start_offset;
                    let block_read_size = (read_end - read_start) as usize;

                    let dest_vram_address = args.vram_address + (read_start - args.offset);

                    if let Err(e) = self
                        .router
                        .cache
                        .gds
                        .read_direct(
                            &b_key,
                            dest_vram_address,
                            block_read_offset,
                            block_read_size,
                            &self.router,
                        )
                        .await
                    {
                        error!(
                            "GDS ioctl: read_direct failed for block {} key {}: {:?}",
                            b_idx, b_key, e
                        );
                        return Err(map_squeezefs_err(e));
                    }
                }

                Ok(ReplyIoctl {
                    result: 0,
                    flags: 0,
                    in_iovs: 0,
                    out_iovs: 0,
                })
            }
            _ => match cmd as u64 {
                libc::FS_IOC_GETFLAGS => Err(Errno::from(libc::ENOTTY)),
                libc::FS_IOC_SETFLAGS => Err(Errno::from(libc::ENOTTY)),
                _ => Err(Errno::from(libc::ENOTTY)),
            },
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

pub fn parse_custom_options(opts: &str) -> std::ffi::OsString {
    let mut custom_opts = std::ffi::OsString::new();
    for opt in opts.split(',') {
        let opt_trimmed = opt.trim();
        if !opt_trimmed.is_empty() {
            let key = opt_trimmed.split('=').next().unwrap_or("").trim();
            if key == "entry_timeout" || key == "attr_timeout" || key == "negative_timeout" {
                continue;
            }
            if !custom_opts.is_empty() {
                custom_opts.push(",");
            }
            custom_opts.push(opt_trimmed);
        }
    }
    custom_opts
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
    if unsafe { libc::getuid() } == 0 {
        options.uid(uid);
        options.gid(gid);
    }
    options.allow_other(allow_other);
    options.write_back(writeback);
    options.default_permissions(true);

    if let Some(opts) = custom_opts {
        let parsed = parse_custom_options(&opts);
        options.custom_options(parsed);
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

    let dismount_wait = fs.dismount_wait;
    let nvme_cache = fs.router.cache.nvme.clone();

    let client_id_str = uuid::Uuid::new_v4().to_string();
    *fs.client_id.lock().unwrap() = client_id_str.clone();
    *fs.mountpoint.lock().unwrap() = mount_path.to_string_lossy().to_string();

    let dlm_clone = fs.dlm.clone();
    let client_id_heartbeat = client_id_str.clone();
    let hostname_val = get_hostname();
    let pid_val = std::process::id();
    let mountpoint_val = mount_path.to_string_lossy().to_string();
    let mounted_at_val = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Do initial HSET to register client
    if let Ok(mut con) = dlm_clone.meta_client().get_connection().await {
        let stats = ClientStats {
            fuse_ops: METRICS.fuse_ops.load(Ordering::Relaxed),
            meta_updates: METRICS.meta_updates.load(Ordering::Relaxed),
            put_obj: METRICS.put_obj.load(Ordering::Relaxed),
            get_obj: METRICS.get_obj.load(Ordering::Relaxed),
            del_obj: METRICS.del_obj.load(Ordering::Relaxed),
            cache_hits: METRICS.cache_hits.load(Ordering::Relaxed),
            cache_misses: METRICS.cache_misses.load(Ordering::Relaxed),
        };
        let info = ClientInfo {
            client_id: client_id_heartbeat.clone(),
            hostname: hostname_val.clone(),
            pid: pid_val,
            mountpoint: mountpoint_val.clone(),
            mounted_at: mounted_at_val,
            last_heartbeat: mounted_at_val,
            stats,
        };
        if let Ok(json_str) = serde_json::to_string(&info) {
            let _: Result<(), _> = con
                .hset("squeezefs:active_clients", &client_id_heartbeat, json_str)
                .await;
        }
    }

    // Spawn heartbeat worker
    let dlm_heartbeat = dlm_clone.clone();
    let client_id_loop = client_id_str.clone();
    let hostname_val_clone = hostname_val.clone();
    let mountpoint_val_clone = mountpoint_val.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            if let Ok(mut con) = dlm_heartbeat.meta_client().get_connection().await {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let stats = ClientStats {
                    fuse_ops: METRICS.fuse_ops.load(Ordering::Relaxed),
                    meta_updates: METRICS.meta_updates.load(Ordering::Relaxed),
                    put_obj: METRICS.put_obj.load(Ordering::Relaxed),
                    get_obj: METRICS.get_obj.load(Ordering::Relaxed),
                    del_obj: METRICS.del_obj.load(Ordering::Relaxed),
                    cache_hits: METRICS.cache_hits.load(Ordering::Relaxed),
                    cache_misses: METRICS.cache_misses.load(Ordering::Relaxed),
                };
                let info = ClientInfo {
                    client_id: client_id_loop.clone(),
                    hostname: hostname_val_clone.clone(),
                    pid: pid_val,
                    mountpoint: mountpoint_val_clone.clone(),
                    mounted_at: mounted_at_val,
                    last_heartbeat: now,
                    stats,
                };
                if let Ok(json_str) = serde_json::to_string(&info) {
                    let _: Result<(), _> = con
                        .hset("squeezefs:active_clients", &client_id_loop, json_str)
                        .await;
                }
            }
        }
    });

    // Spawns the mount loop using fuse3 Session
    let session = fuse3::raw::Session::new(options);

    #[cfg(target_os = "linux")]
    let mut handle = if unsafe { libc::getuid() } == 0 {
        session.mount(fs, mount_path.clone()).await?
    } else {
        session
            .mount_with_unprivileged(fs, mount_path.clone())
            .await?
    };

    #[cfg(not(target_os = "linux"))]
    let mut handle = session.mount(fs, mount_path.clone()).await?;

    println!("\x1b[92mOK\x1b[0m Squeezefs is ready at {:?}", mount_path);

    let mut should_exit = false;
    while !should_exit {
        let shutdown = async {
            #[cfg(unix)]
            {
                let sigterm_opt =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
                let sigint_opt =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
                if let (Ok(mut sigterm), Ok(mut sigint)) = (sigterm_opt, sigint_opt) {
                    loop {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {
                                eprintln!("\nWARNING: Ctrl+C pressed! If you really want to unmount/exit, hit Ctrl+C again.");
                                tokio::select! {
                                    _ = tokio::signal::ctrl_c() => {
                                        info!("Received second Ctrl+C, exiting...");
                                        break;
                                    }
                                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                                        eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                                    }
                                }
                            }
                            _ = sigterm.recv() => {
                                info!("Received SIGTERM, exiting...");
                                break;
                            }
                            _ = sigint.recv() => {
                                eprintln!("\nWARNING: SIGINT received! If you really want to unmount/exit, send SIGINT again.");
                                tokio::select! {
                                    _ = sigint.recv() => {
                                        info!("Received second SIGINT, exiting...");
                                        break;
                                    }
                                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                                        eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                                    }
                                }
                            }
                        }
                    }
                } else {
                    loop {
                        let _ = tokio::signal::ctrl_c().await;
                        eprintln!("\nWARNING: Ctrl+C pressed! If you really want to unmount/exit, hit Ctrl+C again.");
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {
                                info!("Received second Ctrl+C, exiting...");
                                break;
                            }
                            _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                                eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                            }
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            {
                loop {
                    let _ = tokio::signal::ctrl_c().await;
                    eprintln!("\nWARNING: Ctrl+C pressed! If you really want to unmount/exit, hit Ctrl+C again.");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            info!("Received second Ctrl+C, exiting...");
                            break;
                        }
                        _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                            eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                        }
                    }
                }
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
                should_exit = true;
            }
            _ = shutdown => {
                use colored::Colorize;
                use std::io::Write;
                use std::io::IsTerminal;

                info!("Received shutdown signal. Checking staging status...");
                let mut staged_count = 0;
                let mut active_writes_count = 0;
                for key in nvme_cache.list_staged_files() {
                    if key.starts_with("active_block:") {
                        active_writes_count += 1;
                    } else {
                        staged_count += 1;
                    }
                }

                let has_unflushed = staged_count > 0 || active_writes_count > 0;
                if has_unflushed && std::io::stdin().is_terminal() {
                    println!("\n{}", "WARNING: There are unflushed staged writes on this node!".red().bold());
                    println!("Remaining local staged files: {}", staged_count);
                    println!("Active write transaction directories: {}", active_writes_count);
                    println!("If you unmount now, other nodes will not see this data.");
                    println!("\nChoose an option:");
                    println!("  [w] Wait for staged files to drain/flush to S3");
                    println!("  [c] Continue/force unmount immediately (unsafe)");
                    println!("  [a] Abort unmount and continue running mount");
                    print!("Select option [w/c/a]: ");
                    let _ = std::io::stdout().flush();

                    let mut input = String::new();
                    let choice = if std::io::stdin().read_line(&mut input).is_ok() {
                        input.trim().to_lowercase()
                    } else {
                        "c".to_string()
                    };

                    if choice == "a" || choice == "abort" {
                        println!("Aborting exit. Resuming squeezefs mount.");
                        continue;
                    } else if choice == "w" || choice == "wait" {
                        println!("Waiting for staged writes to drain. Press Ctrl+C again to force exit.");
                        let start_wait = std::time::Instant::now();
                        let max_wait = std::time::Duration::from_secs(dismount_wait);
                        loop {
                            let mut current_staged = 0;
                            let mut current_active = 0;
                            for key in nvme_cache.list_staged_files() {
                                if key.starts_with("active_block:") {
                                    current_active += 1;
                                } else {
                                    current_staged += 1;
                                }
                            }
                            if current_staged == 0 && current_active == 0 {
                                println!("\nAll staged files and active writes drained cleanly!");
                                break;
                            }
                            if start_wait.elapsed() >= max_wait {
                                println!("\nGrace period expired. Unmounting with remaining staged files: {}, active writes: {}", current_staged, current_active);
                                break;
                            }
                            print!("\rRemaining staged files: {}, active writes: {}... (elapsed: {}s / limit: {}s)", current_staged, current_active, start_wait.elapsed().as_secs(), dismount_wait);
                            let _ = std::io::stdout().flush();
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        }
                    } else {
                        println!("Continuing with unmount.");
                    }
                }
                should_exit = true;
            }
        }
    }

    // Stop heartbeat task
    heartbeat_handle.abort();

    // Clean up active client registration
    if let Ok(mut con) = dlm_clone.meta_client().get_connection().await {
        let _: Result<(), _> = con.hdel("squeezefs:active_clients", &client_id_str).await;
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
    _nvme_target_path: Option<&str>,
    read_cache_size: Option<&str>,
    write_cache_size: Option<&str>,
    read_mem_cache_size: Option<&str>,
    write_mem_cache_size: Option<&str>,
) -> Result<(), SqueezefsError> {
    format_volume_ext(
        redis_url,
        name,
        block_size,
        capacity,
        inodes,
        compression,
        encrypt_algo,
        encrypt_key,
        mem_cache_size,
        disk_cache_size,
        disk_cache_paths,
        None,
        read_cache_size,
        write_cache_size,
        read_mem_cache_size,
        write_mem_cache_size,
        None,
        None,
        None,
        true,
    )
    .await
}

fn create_superblock(name: &str, capacity: u64, block_size: u64, inodes: u64) -> Vec<u8> {
    let mut sb = vec![0u8; 4096];
    let magic = b"SQUEEZEFS_SUPER\x00";
    sb[0..magic.len()].copy_from_slice(magic);
    let name_bytes = name.as_bytes();
    let name_len = std::cmp::min(name_bytes.len(), 63);
    sb[16..16 + name_len].copy_from_slice(&name_bytes[..name_len]);
    sb[80..88].copy_from_slice(&capacity.to_be_bytes());
    sb[88..96].copy_from_slice(&block_size.to_be_bytes());
    sb[96..104].copy_from_slice(&inodes.to_be_bytes());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    sb[104..112].copy_from_slice(&timestamp.to_be_bytes());
    sb
}

#[allow(clippy::too_many_arguments)]
pub async fn format_volume_ext(
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
    nvme_target_path: Option<&str>,
    read_cache_size: Option<&str>,
    write_cache_size: Option<&str>,
    read_mem_cache_size: Option<&str>,
    write_mem_cache_size: Option<&str>,
    dismount_wait: Option<&str>,
    upload_delay: Option<&str>,
    fuse_io_uring_sqpoll_idle_ms: Option<u32>,
    quick: bool,
) -> Result<(), SqueezefsError> {
    // 2. Compression algorithm check
    let comp = compression.to_lowercase();
    if comp != "none" && comp != "lz4" && comp != "zstd" && !comp.is_empty() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Unsupported compression algorithm: '{}'. Supported options are: none, lz4, zstd.",
            compression
        )));
    }

    // 3. Encryption algorithm check
    let enc = encrypt_algo.to_lowercase();
    if enc != "none" && enc != "aes256gcm-rsa" && enc != "chacha20-rsa" && !enc.is_empty() {
        return Err(SqueezefsError::InvalidOperation(format!(
            "Unsupported encryption algorithm: '{}'. Supported options are: none, aes256gcm-rsa, chacha20-rsa.",
            encrypt_algo
        )));
    }

    // 5. Human readable size limits validation
    if let Some(sz) = mem_cache_size.filter(|s| !s.is_empty() && *s != "none") {
        crate::cache::parse_size_string(sz, 1024 * 1024).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid mem_cache_size '{}': {:?}", sz, e))
        })?;
    }
    if let Some(sz) = disk_cache_size.filter(|s| !s.is_empty() && *s != "none") {
        crate::cache::parse_size_string(sz, 1024 * 1024).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid disk_cache_size '{}': {:?}", sz, e))
        })?;
    }
    if let Some(sz) = read_cache_size.filter(|s| !s.is_empty() && *s != "none") {
        crate::cache::parse_size_string(sz, 1024 * 1024).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid read_cache_size '{}': {:?}", sz, e))
        })?;
    }
    if let Some(sz) = write_cache_size.filter(|s| !s.is_empty() && *s != "none") {
        crate::cache::parse_size_string(sz, 1024 * 1024).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid write_cache_size '{}': {:?}", sz, e))
        })?;
    }
    if let Some(sz) = read_mem_cache_size.filter(|s| !s.is_empty() && *s != "none") {
        crate::cache::parse_size_string(sz, 1024 * 1024).map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "Invalid read_mem_cache_size '{}': {:?}",
                sz, e
            ))
        })?;
    }
    if let Some(sz) = write_mem_cache_size.filter(|s| !s.is_empty() && *s != "none") {
        crate::cache::parse_size_string(sz, 1024 * 1024).map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "Invalid write_mem_cache_size '{}': {:?}",
                sz, e
            ))
        })?;
    }

    // 6. Dismount wait validation
    if let Some(wait) = dismount_wait.filter(|s| !s.is_empty()) {
        wait.parse::<u64>().map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid dismount_wait '{}': {:?}", wait, e))
        })?;
    }

    // 7. Upload delay validation
    if let Some(delay) = upload_delay.filter(|s| !s.is_empty()) {
        crate::cache::parse_duration(delay).map_err(|e| {
            SqueezefsError::InvalidOperation(format!("Invalid upload_delay '{}': {:?}", delay, e))
        })?;
    }

    let meta_client = crate::dlm::MetaClient::new(redis_url)?;
    let mut shard_clients = Vec::new();
    match &meta_client {
        crate::dlm::MetaClient::Sharded { shards } => {
            for shard in shards {
                shard_clients.push(shard.clone());
            }
        }
        other => {
            shard_clients.push(other.clone());
        }
    }

    if shard_clients.is_empty() {
        return Err(SqueezefsError::InvalidOperation(
            "No database nodes found to format".to_string(),
        ));
    }

    if let Some(target_path) = nvme_target_path {
        if !target_path.is_empty() && capacity > 0 {
            let wipe_len = if quick {
                std::cmp::min(capacity, 32 * 1024 * 1024)
            } else {
                capacity
            };

            if quick {
                log::info!(
                    "Quick format: Wiping first {} bytes of NVMe target path: {}",
                    wipe_len,
                    target_path
                );
            } else {
                log::info!(
                    "Full format: Wiping entire capacity of {} bytes of NVMe target path: {}",
                    wipe_len,
                    target_path
                );
            }

            let file_result = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(target_path);

            match file_result {
                Ok(file) => {
                    use std::os::unix::fs::FileExt;
                    use std::sync::atomic::{AtomicU64, Ordering};
                    use std::sync::Arc;

                    let file = Arc::new(file);
                    let chunk_size = 4 * 1024 * 1024; // 4MB chunks
                    let total_chunks = wipe_len.div_ceil(chunk_size);
                    let next_chunk = Arc::new(AtomicU64::new(0));

                    let pb = indicatif::ProgressBar::new(wipe_len);
                    pb.set_style(
                        indicatif::ProgressStyle::default_bar()
                            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                            .unwrap()
                            .progress_chars("#>-"),
                    );
                    let pb = Arc::new(pb);

                    let num_threads = if wipe_len <= 32 * 1024 * 1024 {
                        1
                    } else {
                        8
                    };

                    let mut handles = vec![];
                    for _ in 0..num_threads {
                        let file_clone = file.clone();
                        let next_chunk_clone = next_chunk.clone();
                        let pb_clone = pb.clone();
                        let target_path_str = target_path.to_string();

                        let handle = std::thread::spawn(move || {
                            let zeros = vec![0u8; chunk_size as usize];
                            loop {
                                let chunk_idx = next_chunk_clone.fetch_add(1, Ordering::Relaxed);
                                if chunk_idx >= total_chunks {
                                    break;
                                }

                                let offset = chunk_idx * chunk_size;
                                let to_write = std::cmp::min(chunk_size, wipe_len - offset);

                                if let Err(e) = file_clone.write_at(&zeros[..to_write as usize], offset) {
                                    log::warn!(
                                        "Failed to write zero block to NVMe target {} at offset {}: {:?}",
                                        target_path_str,
                                        offset,
                                        e
                                    );
                                    break;
                                }
                                pb_clone.inc(to_write);
                            }
                        });
                        handles.push(handle);
                    }

                    let _ = tokio::task::spawn_blocking(move || {
                        for handle in handles {
                            let _ = handle.join();
                        }
                    })
                    .await;

                    if let Err(e) = file.sync_all() {
                        log::warn!("Failed to sync NVMe target: {:?}", e);
                    }
                    pb.finish_with_message("NVMe target wiped");
                }
                Err(e) => {
                    log::warn!("Failed to open NVMe target for wiping: {:?}", e);
                }
            }
        }
    }

    if let Some(target_path) = nvme_target_path {
        if !target_path.is_empty() {
            log::info!(
                "Writing SqueezeFS superblock signature to target: {}",
                target_path
            );
            let file_result = tokio::fs::OpenOptions::new()
                .write(true)
                .open(target_path)
                .await;
            match file_result {
                Ok(mut file) => {
                    use tokio::io::AsyncSeekExt;
                    use tokio::io::AsyncWriteExt;
                    let sb = create_superblock(name, capacity, block_size, inodes);
                    if file.seek(std::io::SeekFrom::Start(0)).await.is_ok() {
                        if let Err(e) = file.write_all(&sb).await {
                            log::warn!(
                                "Failed to write SqueezeFS superblock to NVMe target: {:?}",
                                e
                            );
                        } else {
                            let _ = file.sync_all().await;
                            log::info!(
                                "Successfully wrote SqueezeFS superblock to target {}",
                                target_path
                            );
                        }
                    }
                }
                Err(e) => {
                    log::warn!("Failed to open NVMe target for writing superblock: {:?}", e);
                }
            }
        }
    }

    let mut first_con = shard_clients[0].get_connection().await?;

    // Read existing format configuration before flushing
    let existing_format_fields: std::collections::HashMap<String, String> = first_con
        .hgetall("squeezefs:format")
        .await
        .unwrap_or_default();

    let mut dirs_to_wipe = std::collections::HashSet::new();

    // 1. Get directories from the existing format configuration in the database
    if let Some(paths_str) = existing_format_fields.get("disk_cache_paths") {
        if !paths_str.is_empty() {
            for p in paths_str.split(',') {
                dirs_to_wipe.insert(std::path::PathBuf::from(p));
            }
        }
    }

    // 2. Get directories from parameters
    if let Some(paths) = disk_cache_paths {
        for p in paths {
            dirs_to_wipe.insert(p.clone());
        }
    }

    // Wipe each unique directory
    for dir in dirs_to_wipe {
        if dir.exists() {
            log::info!("Wiping local staging/cache directory: {:?}", dir);
            if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
                log::warn!("Failed to wipe local cache directory {:?}: {:?}", dir, e);
            }
            if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                log::warn!(
                    "Failed to re-create local cache directory {:?}: {:?}",
                    dir,
                    e
                );
            }
        }
    }

    for shard_client in &shard_clients {
        let mut con = shard_client.get_connection().await?;
        let _: () = redis::cmd("FLUSHDB")
            .query_async(&mut con)
            .await
            .unwrap_or(());

        let mem_size = mem_cache_size.unwrap_or("1GB").to_string();
        let disk_size = disk_cache_size.unwrap_or("10GB").to_string();
        let r_cache = read_cache_size.unwrap_or("").to_string();
        let w_cache = write_cache_size.unwrap_or("").to_string();
        let r_mem = read_mem_cache_size.unwrap_or("").to_string();
        let w_mem = write_mem_cache_size.unwrap_or("").to_string();
        let d_wait = dismount_wait.unwrap_or("10").to_string();
        let u_delay = upload_delay.unwrap_or("500ms").to_string();
        let sqpoll_idle_ms = fuse_io_uring_sqpoll_idle_ms
            .filter(|value| *value > 0)
            .map(|value| value.to_string())
            .unwrap_or_default();
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
            .hset("squeezefs:format", "dismount_wait", d_wait)
            .hset("squeezefs:format", "upload_delay", u_delay)
            .hset(
                "squeezefs:format",
                "backing_dev",
                nvme_target_path.unwrap_or(""),
            )
            .hset(
                "squeezefs:format",
                "fuse_io_uring_sqpoll_idle_ms",
                sqpoll_idle_ms,
            )
            .hset("squeezefs:format", "active_write_backend", "backend_0");

        let _: () = pipe.query_async(&mut con).await?;
    }

    let shard_count = shard_clients.len();
    if shard_count > 1 {
        for (i, shard_client) in shard_clients.iter().enumerate() {
            let mut con = shard_client.get_connection().await?;
            let initial_counter = match i {
                0 => shard_count as u64,
                1 => 1 + shard_count as u64,
                _ => i as u64,
            };
            let _: () = con.set("squeezefs:inode_counter", initial_counter).await?;

            if i == 1 {
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
                    .hset("squeezefs:attr:1", "uid", 0)
                    .hset("squeezefs:attr:1", "gid", 0)
                    .hset("squeezefs:attr:1", "atime_sec", sec)
                    .hset("squeezefs:attr:1", "atime_nsec", nsec)
                    .hset("squeezefs:attr:1", "mtime_sec", sec)
                    .hset("squeezefs:attr:1", "mtime_nsec", nsec)
                    .hset("squeezefs:attr:1", "ctime_sec", sec)
                    .hset("squeezefs:attr:1", "ctime_nsec", nsec)
                    .set_nx("squeezefs:inode_counter", initial_counter)
                    .incr("squeezefs:used_inodes", 1)
                    .query_async(&mut con)
                    .await?;
            }
        }
    }

    Ok(())
}

pub async fn get_volume_status(redis_url: &str) -> Result<serde_json::Value, SqueezefsError> {
    let client = crate::dlm::MetaClient::new(redis_url)?;
    let mut con = client.get_connection().await?;
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

    let backends_map: std::collections::HashMap<String, String> =
        con.hgetall("squeezefs:backends").await.unwrap_or_default();
    let statuses_map: std::collections::HashMap<String, String> = con
        .hgetall("squeezefs:backend:status")
        .await
        .unwrap_or_default();

    let mut storage_backends = serde_json::Map::new();
    for (be_id, be_json_str) in backends_map {
        if let Ok(mut be_val) = serde_json::from_str::<serde_json::Value>(&be_json_str) {
            let status = statuses_map
                .get(&be_id)
                .cloned()
                .unwrap_or_else(|| "enabled".to_string());
            if let Some(obj) = be_val.as_object_mut() {
                obj.insert("status".to_string(), serde_json::Value::String(status));
            }
            storage_backends.insert(be_id, be_val);
        }
    }

    let s3_endpoint = fields.get("s3_endpoint").cloned().unwrap_or_default();
    let s3_bucket = fields.get("s3_bucket").cloned().unwrap_or_default();

    if !storage_backends.contains_key("backend_0") {
        let status = statuses_map
            .get("backend_0")
            .cloned()
            .unwrap_or_else(|| "enabled".to_string());
        let be_val = serde_json::json!({
            "endpoint": s3_endpoint,
            "access_key": "admin",
            "secret_key": "password",
            "bucket": s3_bucket,
            "status": status,
        });
        storage_backends.insert("backend_0".to_string(), be_val);
    }

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

    let raw_clients: std::collections::HashMap<String, String> = con
        .hgetall("squeezefs:active_clients")
        .await
        .unwrap_or_default();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut active_clients = Vec::new();
    for (cid, json_str) in raw_clients {
        if let Ok(info) = serde_json::from_str::<ClientInfo>(&json_str) {
            if now.saturating_sub(info.last_heartbeat) <= 6 {
                active_clients.push(info);
            } else {
                // Stale client cleanup
                let _: Result<(), _> = con.hdel("squeezefs:active_clients", &cid).await;
            }
        } else {
            // Invalid entry cleanup
            let _: Result<(), _> = con.hdel("squeezefs:active_clients", &cid).await;
        }
    }

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
            "StorageBackends": storage_backends,
            "ActiveWriteBackend": active_write_backend,
        },
        "Clients": active_clients,
    }))
}

async fn run_constant_writeback_worker(
    mut rx: tokio::sync::mpsc::Receiver<WritebackRequest>,
    router: DataRouter,
    dlm: DlmClient,
    active_inode_locks: std::sync::Arc<
        dashmap::DashMap<u64, std::sync::Arc<tokio::sync::RwLock<()>>, ahash::RandomState>,
    >,
    max_uploads: usize,
) {
    let upload_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_uploads));

    while let Some(req) = rx.recv().await {
        let router_clone = router.clone();
        let dlm_clone = dlm.clone();
        let locks_clone = active_inode_locks.clone();
        let sem_clone = upload_semaphore.clone();

        tokio::spawn(async move {
            let _permit = match sem_clone.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            };

            let file_path = format!("inode_{}", req.ino);
            let meta_key = format!("metadata:{}", file_path);
            let mut con = match dlm_clone.get_connection().await {
                Ok(c) => c,
                Err(_) => return,
            };

            let (file_type_opt, block_map_id_opt): (Option<String>, Option<String>) =
                match redis::pipe()
                    .hget(&meta_key, "type")
                    .hget(&meta_key, "block_map_id")
                    .query_async(&mut con)
                    .await
                {
                    Ok(r) => r,
                    Err(_) => return,
                };

            let file_type = file_type_opt.unwrap_or_else(|| "inline".to_string());
            let is_striped = file_type == "striped";
            let mut block_map_id = block_map_id_opt.unwrap_or_default();
            if block_map_id.is_empty() {
                block_map_id = uuid::Uuid::new_v4().to_string();
                let _: Result<(), _> = con.hset(&meta_key, "block_map_id", &block_map_id).await;
            }

            let block_map_key = format!("block_map:{}", block_map_id);
            let old_key: Option<String> =
                match con.hget(&block_map_key, req.block_idx.to_string()).await {
                    Ok(k) => k,
                    Err(_) => return,
                };

            let _ = flush_single_active_block(
                req.ino,
                req.block_idx,
                req.fencing_token,
                &router_clone,
                &dlm_clone,
                &locks_clone,
                is_striped,
                &block_map_id,
                old_key,
                false,
            )
            .await;
        });
    }
}

async fn flush_due_active_blocks_for_inode(
    ino: u64,
    block_indices: Vec<u32>,
    fencing_token: u64,
    router: &DataRouter,
    dlm: &DlmClient,
    active_inode_locks: &std::sync::Arc<
        dashmap::DashMap<u64, std::sync::Arc<tokio::sync::RwLock<()>>, ahash::RandomState>,
    >,
) -> Result<(), SqueezefsError> {
    use futures::stream::{self, StreamExt};

    let file_path = format!("inode_{}", ino);
    let meta_key = format!("metadata:{}", file_path);
    let mut con = dlm.get_connection_for_inode(ino).await?;
    let (file_type_opt, block_map_id_opt): (Option<String>, Option<String>) = redis::pipe()
        .hget(&meta_key, "type")
        .hget(&meta_key, "block_map_id")
        .query_async(&mut con)
        .await?;

    let file_type = file_type_opt.unwrap_or_else(|| "inline".to_string());
    let is_striped = file_type == "striped";
    let mut block_map_id = block_map_id_opt.unwrap_or_default();
    if block_map_id.is_empty() {
        block_map_id = uuid::Uuid::new_v4().to_string();
        let _: () = con.hset(&meta_key, "block_map_id", &block_map_id).await?;
    }

    let block_map_key = format!("block_map:{}", block_map_id);
    let mut pipe = redis::pipe();
    for &b in &block_indices {
        pipe.hget(&block_map_key, b.to_string());
    }
    let old_block_keys: Vec<Option<String>> = pipe.query_async(&mut con).await?;

    let router = router.clone();
    let dlm = dlm.clone();
    let active_inode_locks = active_inode_locks.clone();
    let block_map_id_val = block_map_id.clone();

    let mut flushes = stream::iter(block_indices.into_iter().enumerate().map(
        move |(idx, block_idx)| {
            let router = router.clone();
            let dlm = dlm.clone();
            let active_inode_locks = active_inode_locks.clone();
            let old_key = old_block_keys[idx].clone();
            let block_map_id_val = block_map_id_val.clone();
            async move {
                flush_single_active_block(
                    ino,
                    block_idx,
                    fencing_token,
                    &router,
                    &dlm,
                    &active_inode_locks,
                    is_striped,
                    &block_map_id_val,
                    old_key,
                    false,
                )
                .await
            }
        },
    ))
    .buffer_unordered(8);

    while let Some(result) = flushes.next().await {
        result?;
    }

    Ok(())
}

async fn flush_single_active_block(
    ino: u64,
    b: u32,
    _fencing_token: u64,
    router: &DataRouter,
    dlm: &DlmClient,
    active_inode_locks: &dashmap::DashMap<
        u64,
        std::sync::Arc<tokio::sync::RwLock<()>>,
        ahash::RandomState,
    >,
    is_striped: bool,
    block_map_id: &str,
    old_block_key: Option<String>,
    locked: bool,
) -> Result<(), SqueezefsError> {
    let cache_key = format!("active_block:inode_{}:block_{}", ino, b);

    let block_lock = BLOCK_FLUSH_LOCKS
        .entry((ino, b))
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .value()
        .clone();

    let _block_guard = block_lock.lock().await;
    let _cleanup_guard = BlockFlushGuard { key: (ino, b) };

    let lock_opt = if !locked {
        Some(
            active_inode_locks
                .entry(ino)
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::RwLock::new(())))
                .clone(),
        )
    } else {
        None
    };

    let mut _inode_guard = None;
    if let Some(ref l) = lock_opt {
        _inode_guard = Some(l.read().await);
    }

    let block_data = match router.cache.nvme.read_staged(&cache_key) {
        Some(d) => d,
        None => return Ok(()),
    };

    use redis::AsyncCommands;
    let mut con = dlm.get_connection_for_inode(ino).await?;

    let block_bytes = bytes::Bytes::from(block_data.clone());
    let processed_block = router.get_crypto().process_write(block_bytes.clone())?;
    let processed_len = processed_block.len();

    let offset = router.block_allocator.allocate_block().await?;

    if let Err(e) = router
        .nvme_writer
        .write_block(offset, &processed_block)
        .await
    {
        error!(
            "flush_single_active_block: Failed to upload block {} of inode {} to NVMe: {:?}",
            b, ino, e
        );
        let _ = router.block_allocator.free_block(offset).await;
        return Err(e);
    }

    let stored_block_key = offset.to_string();

    let block_map_key = format!("block_map:{}", block_map_id);

    let refcounts_key = "squeezefs:block_refcounts";
    let mut pipe = redis::pipe();
    pipe.hset(refcounts_key, &stored_block_key, 1)
        .hset(&block_map_key, b.to_string(), &stored_block_key)
        .hset(
            "squeezefs:block_sizes",
            &stored_block_key,
            format!("{}:{}", block_bytes.len(), processed_len),
        );
    let _: () = pipe.query_async(&mut con).await?;

    router.block_map_cache.insert(
        (block_map_id.to_string(), b),
        (Some(stored_block_key.clone()), std::time::Instant::now()),
    );

    // Cache in RAM (bypass entirely if file is striped layout)
    if !is_striped {
        router
            .cache
            .read_lru
            .put(&stored_block_key, block_bytes.clone());
    }

    if let Some(bk) = old_block_key {
        router.cache.read_lru.remove(&bk);
        let old_ref: Option<i32> = con.hget(refcounts_key, &bk).await?;
        if let Some(mut r) = old_ref {
            r -= 1;
            if r <= 0 {
                let _: () = redis::pipe()
                    .hdel(refcounts_key, &bk)
                    .hdel("squeezefs:block_sizes", &bk)
                    .query_async(&mut con)
                    .await?;
                if let Ok(old_offset) = bk.parse::<u64>() {
                    let _ = router.block_allocator.free_block(old_offset).await;
                }
            } else {
                let _: () = con.hset(refcounts_key, &bk, r).await?;
            }
        } else {
            let _: () = con.hdel("squeezefs:block_sizes", &bk).await.unwrap_or(());
            if let Ok(old_offset) = bk.parse::<u64>() {
                let _ = router.block_allocator.free_block(old_offset).await;
            }
        }
    }

    // ONLY remove active write block from cache if it hasn't been modified by a newer write
    let current_staged = router.cache.nvme.read_staged(&cache_key);
    if let Some(ref cur) = current_staged {
        if cur == &block_bytes[..] {
            router.cache.nvme.remove_active_block(&cache_key);
        }
    } else {
        router.cache.nvme.remove_active_block(&cache_key);
    }

    Ok(())
}
async fn handle_recall(
    inode: Inode,
    dlm: &DlmClient,
    delegations: &dashmap::DashMap<Inode, crate::dlm::DelegationLease, ahash::RandomState>,
    posix_locks: &dashmap::DashMap<(Inode, u64, u64, u64), PosixLock, ahash::RandomState>,
) -> Result<(), SqueezefsError> {
    info!("handle_recall: Recalling delegation for inode {}", inode);

    if let Some((_, lease)) = delegations.remove(&inode) {
        let mut to_promote = Vec::new();
        for entry in posix_locks.iter() {
            let &(lock_ino, owner, start, end) = entry.key();
            if lock_ino == inode {
                if let PosixLock::Local = entry.value() {
                    to_promote.push((lock_ino, owner, start, end));
                }
            }
        }

        for key in to_promote {
            info!("handle_recall: Flushing local lock {:?} to Redis", key);
            let file_path = format!("inode_{}", key.0);
            let lease = dlm
                .acquire_lock(&file_path, Some((key.2, key.3)), Duration::from_secs(5))
                .await?;
            posix_locks.insert(key, PosixLock::Global(Box::new(lease)));
        }

        lease.release().await?;
        info!(
            "handle_recall: Delegation for inode {} successfully released",
            inode
        );
    } else {
        warn!(
            "handle_recall: Received recall for inode {}, but delegation was not held locally",
            inode
        );
    }

    Ok(())
}

async fn load_locks_from_redis(
    inode: Inode,
    dlm: &DlmClient,
    posix_locks: &dashmap::DashMap<(Inode, u64, u64, u64), PosixLock, ahash::RandomState>,
) -> Result<(), SqueezefsError> {
    use redis::AsyncCommands;
    let mut con = dlm.get_connection_for_inode(inode).await?;
    let pattern = format!("lock:inode_{}:range:*", inode);

    let mut keys = Vec::new();
    {
        let mut iter: redis::AsyncIter<String> = con
            .scan_match(&pattern)
            .await
            .map_err(|e| SqueezefsError::from(e))?;
        while let Some(key) = iter.next_item().await {
            keys.push(key);
        }
    }

    for key in keys {
        if let Some(suffix) = key.strip_prefix(&format!("lock:inode_{}:range:", inode)) {
            let parts: Vec<&str> = suffix.split('-').collect();
            if parts.len() == 2 {
                if let (Ok(start), Ok(end)) = (parts[0].parse::<u64>(), parts[1].parse::<u64>()) {
                    let holder: Option<String> =
                        con.get(&key).await.map_err(|e| SqueezefsError::from(e))?;
                    if let Some(holder_id) = holder {
                        if holder_id != dlm.client_id() {
                            posix_locks.insert(
                                (inode, u64::MAX, start, end),
                                PosixLock::Remote {
                                    client_id: holder_id,
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
