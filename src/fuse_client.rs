use crate::dlm::DlmClient;
use crate::error::SqueezefsError;
use crate::meta_backend::Metadata;

use crate::routing::DataRouter;
use fuse3::raw::{
    prelude::*,
    reply::{DirectoryEntry, FileAttr, ReplyCopyFileRange, ReplyIoctl, ReplyLock},
    Request,
};
use fuse3::{Errno, Inode, MountOptions, Result as FuseResult, Timestamp};
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::runtime::Builder;

pub const CONFIG_INODE: u64 = 0xffff_ffff_ffff_fffe;
pub const STATS_INODE: u64 = 0xffff_ffff_ffff_fffd;

/// How often a mounted client refreshes its `client:{id}` registration on the
/// metadata volume (heartbeat), so peers can distinguish a live mount from a
/// crashed one.
pub const CLIENT_HEARTBEAT_INTERVAL_SECS: u64 = 10;
/// A `client:{id}` registration whose heartbeat is older than this is stale: the
/// client died (kill -9 / crash / power loss) without unregistering. Stale
/// registrations do NOT block `format` and are reaped on next format attempt.
pub const CLIENT_STALE_TTL_SECS: u64 = 45;

fn get_fuse_timeout() -> Duration {
    if let Ok(val) = std::env::var("SQUEEZEFS_TIMEOUT") {
        if let Ok(secs) = val.parse::<u64>() {
            return Duration::from_secs(secs);
        }
    }
    Duration::from_secs(30)
}

// `StripeLocks` moved to `crate::stripe_locks` so `meta_backend` can use it
// without a module cycle. Re-exported here for source compatibility (existing
// `crate::fuse_client::StripeLocks` / `squeezefs::fuse_client::StripeLocks` paths
// and the lock-order documentation continue to work).
pub use crate::stripe_locks::StripeLocks;

#[inline]
fn osstr_to_cow(name: &std::ffi::OsStr) -> std::borrow::Cow<'_, str> {
    name.to_str()
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| name.to_string_lossy())
}

/// POSIX `NAME_MAX`: max bytes in a single path component (excludes the trailing NUL).
/// Reported via `statfs.f_namelen` and enforced on every name-taking FUSE op so the
/// kernel gets `ENAMETOOLONG` instead of silently accepting oversize components.
pub(crate) const FUSE_NAME_MAX: usize = 255;

/// Reject a directory entry name longer than [`FUSE_NAME_MAX`].
#[inline]
pub(crate) fn check_component_name_len(name: &std::ffi::OsStr) -> Result<(), Errno> {
    if name.len() > FUSE_NAME_MAX {
        Err(Errno::from(libc::ENAMETOOLONG))
    } else {
        Ok(())
    }
}
pub static BLOCK_FLUSH_LOCKS: Lazy<StripeLocks<tokio::sync::Mutex<()>, 4096>> =
    Lazy::new(|| StripeLocks::new());

/// P1-8: how long the FUSE write path holds the per-inode write lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeWriteLockScope {
    /// Hold exclusive lock for the entire write (inline RMW or layout transition).
    EntireOp,
    /// Hold exclusive lock only for meta-prep (lease, size/type, quota, attr/meta
    /// size update). Data I/O runs without the inode write lock; per-block
    /// [`BLOCK_FLUSH_LOCKS`] serialize active-block mutation.
    MetaPrepOnly,
}

/// Decide inode write-lock scope for a FUSE write.
///
/// Already-striped files use block-level locks on the data path, so the full-inode
/// write lock need only cover short meta-prep. Inline and non-striped (layout
/// transition / whole-buffer RMW) paths keep the lock for the entire operation.
pub const MAX_INLINE_SIZE: u64 = 4096;

#[inline]
pub fn inode_write_lock_scope(fits_inline: bool, is_striped: bool) -> InodeWriteLockScope {
    if fits_inline || !is_striped {
        InodeWriteLockScope::EntireOp
    } else {
        InodeWriteLockScope::MetaPrepOnly
    }
}

/// §5.4 lease-severance boundary (zero-copy write-path design, PR 5).
///
/// FUSE_WRITE payloads arrive as zero-copy transport leases over the
/// registered FUSE-over-io_uring payload buffer; the ring ent is not
/// re-armed (COMMIT_AND_FETCH) until the lease drops. A lease that escapes
/// the write handler into a long-lived sink (`data_key`, the LRUs) parks
/// that ent forever — at `Q_DEPTH = 4`, a deterministic mount hang. The one
/// route that hands the payload to `DataRouter::write_file` — the
/// `use_router_write` branch top; PR 6 deleted the transitional
/// `is_aligned` second sever point together with its branch — therefore
/// materializes it lease-free first with one unconditional copy (the same
/// bytes the transport used to copy *twice* before PR 5, now paid only by
/// the small/staged routes — the hot striped route consumes the lease via
/// the accumulation merge and never severs). Unconditional rather than
/// lease-detecting: `bytes::Bytes` cannot cheaply introspect its owner, and
/// a copy on the cold routes beats a reachability argument every reviewer
/// must re-verify. Invariant, checkable in one place: a transport lease
/// never escapes the write handler's call graph — consumed by the
/// accumulation merge, the one-shot severing copy, or `sever_payload`,
/// all before the handler returns.
#[inline]
fn sever_payload(data: &bytes::Bytes) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(data)
}

struct ThreadLocalState {
    count: u64,
    target: *const AtomicU64,
}

impl Drop for ThreadLocalState {
    fn drop(&mut self) {
        if self.count > 0 && !self.target.is_null() {
            unsafe {
                (*self.target).fetch_add(self.count, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Default)]
pub struct ProbabilisticAtomic {
    inner: AtomicU64,
}

impl ProbabilisticAtomic {
    pub fn fetch_add(&self, val: u64, order: Ordering) -> u64 {
        thread_local! {
            static STATE: std::cell::RefCell<ThreadLocalState> = std::cell::RefCell::new(ThreadLocalState {
                count: 0,
                target: std::ptr::null(),
            });
        }
        STATE.with(|s| {
            let mut state = s.borrow_mut();
            if state.target.is_null() {
                state.target = &self.inner as *const AtomicU64;
            }
            state.count += val;
            if state.count >= 128 {
                let to_add = state.count;
                state.count = 0;
                self.inner.fetch_add(to_add, order)
            } else {
                self.inner.load(order)
            }
        })
    }

    pub fn load(&self, order: Ordering) -> u64 {
        self.inner.load(order)
    }
}

#[repr(align(64))]
#[derive(Default)]
pub struct Align64<T>(pub T);

impl<T> std::ops::Deref for Align64<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Align64<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct LatencyHistogram {
    pub buckets: [AtomicU64; 26],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

impl LatencyHistogram {
    pub fn record(&self, duration: Duration) {
        let micros = duration.as_micros() as u64;
        let bucket_idx = if micros <= 1 {
            0
        } else {
            let idx = (micros - 1).ilog2() as usize + 1;
            std::cmp::min(idx, 25)
        };
        self.buckets[bucket_idx].fetch_add(1, Ordering::Relaxed);
    }

    pub fn to_json(&self) -> serde_json::Value {
        const LABELS: &[&str] = &[
            "<=1us", "<=2us", "<=4us", "<=8us", "<=16us", "<=32us", "<=64us", "<=128us", "<=256us",
            "<=512us", "<=1024us", "<=2ms", "<=4ms", "<=8ms", "<=16ms", "<=32ms", "<=64ms",
            "<=128ms", "<=256ms", "<=512ms", "<=1024ms", "<=2s", "<=4s", "<=8s", "<=16s", ">16s",
        ];
        let mut map = serde_json::Map::new();
        for (i, label) in LABELS.iter().enumerate() {
            let val = self.buckets[i].load(Ordering::Relaxed);
            map.insert(
                label.to_string(),
                serde_json::Value::Number(serde_json::Number::from(val)),
            );
        }
        serde_json::Value::Object(map)
    }
}

pub struct QueueDepthHistogram {
    pub buckets: [AtomicU64; 15],
}

impl Default for QueueDepthHistogram {
    fn default() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }
}

impl QueueDepthHistogram {
    pub fn record(&self, depth: usize) {
        let bucket_idx = if depth == 0 {
            0
        } else if depth == 1 {
            1
        } else if depth == 2 {
            2
        } else {
            let idx = (depth - 1).ilog2() as usize + 2;
            std::cmp::min(idx, 14)
        };
        self.buckets[bucket_idx].fetch_add(1, Ordering::Relaxed);
    }

    pub fn to_json(&self) -> serde_json::Value {
        const LABELS: &[&str] = &[
            "0", "1", "2", "<=4", "<=8", "<=16", "<=32", "<=64", "<=128", "<=256", "<=512",
            "<=1024", "<=2048", "<=4096", ">4096",
        ];
        let mut map = serde_json::Map::new();
        for (i, label) in LABELS.iter().enumerate() {
            let val = self.buckets[i].load(Ordering::Relaxed);
            map.insert(
                label.to_string(),
                serde_json::Value::Number(serde_json::Number::from(val)),
            );
        }
        serde_json::Value::Object(map)
    }
}

/// Process-wide counters (P3-1). All updates are `Relaxed` atomics — no locks on the hot path.
#[derive(Default)]
pub struct Metrics {
    pub fuse_ops: Align64<ProbabilisticAtomic>,
    pub meta_updates: Align64<AtomicU64>,
    pub put_obj: Align64<AtomicU64>,
    pub get_obj: Align64<AtomicU64>,
    pub del_obj: Align64<AtomicU64>,
    pub cache_hits: Align64<AtomicU64>,
    pub cache_misses: Align64<AtomicU64>,
    /// Layout mix (write path outcomes).
    pub layout_inline_writes: Align64<AtomicU64>,
    pub layout_staged_writes: Align64<AtomicU64>,
    pub layout_striped_writes: Align64<AtomicU64>,
    /// Best-effort background admission (see `bg_admit`).
    pub bg_spawn_admitted: Align64<AtomicU64>,
    pub bg_spawn_rejected: Align64<AtomicU64>,
    /// io_uring request queue backpressure (submit rejected as full).
    pub uring_queue_full: Align64<AtomicU64>,
    /// Block writes that missed `write_block`'s zero-copy `WriteData::Aligned`
    /// DMA branch and paid the bounce-buffer copy (zero-copy write-path design
    /// §5.6, PR 2 — the pooled-buffer alignment-contract violation detector).
    /// Pooled sources are 4 KiB-aligned by construction, so on aligned
    /// workloads this must stay 0; non-4 KiB-multiple payloads
    /// (compressed/encrypted output, tail blocks) are the only legitimate
    /// contributors.
    pub nvme_unaligned_write_fallbacks: Align64<AtomicU64>,
    /// DLM lease acquire outcomes (coarse lock-wait signal).
    pub lease_acquire_ok: Align64<AtomicU64>,
    pub lease_acquire_fail: Align64<AtomicU64>,
    /// Writeback path: durable flush hard failures (sticky).
    pub writeback_hard_failures: Align64<AtomicU64>,
    /// Copy-on-write duplications of an active-block accumulation buffer
    /// forced by a live reader snapshot (zero-copy write-path design §5.2).
    /// Sequential streams never pay this; spikes mean read/write contention
    /// on the same dirty block — the price of snapshot immutability, made
    /// observable.
    pub active_block_cow_copies: Align64<AtomicU64>,
    /// Content-complete blocks uploaded directly (crypto → allocate → DMA →
    /// block-map merge), bypassing the staging mmap + writeback round-trip
    /// (zero-copy write-path design §5.3, PR 4). Should ≈ the striped
    /// sequential write volume on healthy mounts.
    pub write_through_blocks: Align64<AtomicU64>,
    /// Bytes moved by write-through uploads (`write_through_blocks` ×
    /// block_size for the default shape).
    pub write_through_bytes: Align64<AtomicU64>,
    /// Write-throughs that degraded into the never-lossy staging fallback
    /// (device write failure / allocator failure / uring backpressure).
    /// ~0 on healthy mounts; sustained growth = device backpressure.
    pub write_through_fallbacks: Align64<AtomicU64>,
    /// Seed-time memset bytes elided by §5.3 coverage tracking: for every
    /// Fresh accumulation buffer reaching content-validity, the block size
    /// minus the complement bytes actually zeroed. Sequential fills elide
    /// the whole block.
    pub active_block_memset_elided_bytes: Align64<AtomicU64>,
    /// Meta-volume durability barriers actually issued (real `fdatasync` calls).
    /// A single FUSE fsync should raise this by exactly one (no redundant barrier).
    pub meta_device_syncs: Align64<AtomicU64>,
    /// Meta-volume barrier *requests* (callers of `sync_device_for_ino`). Under
    /// group commit `meta_sync_requests - meta_device_syncs` is the work saved by
    /// coalescing concurrent fsyncs into shared barriers.
    pub meta_sync_requests: Align64<AtomicU64>,
    /// Histograms for lock wait times and queue depths.
    pub write_lock_wait: Align64<LatencyHistogram>,
    pub block_lock_wait: Align64<LatencyHistogram>,
    pub lease_lock_wait: Align64<LatencyHistogram>,
    pub dlm_acquire_time: Align64<LatencyHistogram>,
    pub writeback_queue_depth: Align64<QueueDepthHistogram>,
    /// Sector-sharded commit observability (design §Observability, PR 7) —
    /// live regression signals for the transaction_lock removal rollout.
    ///
    /// Time a sector-locked commit spent acquiring its sector write locks.
    pub meta_sector_lock_wait_ns: Align64<LatencyHistogram>,
    /// Commits that blocked on at least one busy sector (same-sector contention).
    pub meta_sector_lock_contended: Align64<AtomicU64>,
    /// In-flight sector-locked `run_transaction`s right now (gauge).
    pub meta_tx_concurrency: Align64<AtomicU64>,
    /// High-watermark of the gauge — p50/peak > 1 under load proves the old
    /// global-transaction-lock concurrency cap is gone.
    pub meta_tx_concurrency_peak: Align64<AtomicU64>,
    /// Lost `fetch_or` attempts in the lock-free inode allocator's scan
    /// (bitmap-word contention / stale-hint occupancy).
    pub meta_inode_alloc_cas_retries: Align64<AtomicU64>,
    /// On-disk free-inode bitmap bits healed by table-derived reconciliation
    /// (mount / clean unmount). Non-zero on a clean mount ⇒ investigate.
    /// The quarantined range [1024, 1152) is masked out (§4.4) so this keeps
    /// meaning *unexplained* divergence.
    pub meta_inode_alloc_reconciled: Align64<AtomicU64>,
    /// Magic-valid inodes found inside the quarantined range [1024, 1152) at
    /// mount reconciliation — legacy xattr/journal-overlap victims (§4.4).
    /// Non-zero ⇒ operator notice: those files' xattr blocks are presumed
    /// corrupt (symlinks lost content; regular files lost xattrs).
    pub meta_quarantined_inodes: Align64<AtomicU64>,
    /// Sectors per commit apply batch (`write_blocks_direct_batch` fill) —
    /// the headroom signal that replaced the WAL worker's
    /// `meta_wal_batch_size` when the write-only journal was deleted
    /// (design-wal-crash-consistency §Observability; same-payload
    /// replacement, flagged as a breaking stats-field change in PR 4).
    pub meta_commit_sectors: Align64<QueueDepthHistogram>,
    /// Deferred-flusher device barriers issued (timer path), vs
    /// strict/fsync barriers which land in `meta_device_syncs` directly.
    pub meta_flush_deferred: Align64<AtomicU64>,
    /// Reclaim group-commit fill: doomed inos per batched `destroy_inodes`
    /// transaction (design §4.5) — headroom before `SQUEEZEFS_RECLAIM_BATCH`
    /// needs raising.
    pub meta_reclaim_batch_size: Align64<QueueDepthHistogram>,
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

#[cold]
#[inline(never)]
fn map_squeezefs_err(e: SqueezefsError) -> Errno {
    let errno = e.to_errno();
    // ENOENT is the normal grammar of POSIX lookups (negative dentries,
    // unlink/stat probes): logging it at ERROR buried real faults under
    // thousands of benign lines per bench/rsync run.
    if errno == libc::ENOENT {
        debug!("Squeezefs operational error: {:?}", e);
    } else {
        error!("Squeezefs operational error: {:?}", e);
    }
    Errno::from(errno)
}

#[inline]
fn as_timestamp(ns: u64) -> Timestamp {
    Timestamp::new((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
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

/// Max automatic retries for a single background writeback unit (P0-3).
const WRITEBACK_MAX_ATTEMPTS: u32 = 4;
/// P1-3: max partial blocks held only in RAM (not yet staged).
const MAX_ACTIVE_BLOCK_BUFFERS: usize = 256;

#[derive(Debug, Clone)]
pub struct WritebackRequest {
    pub ino: u64,
    pub block_idx: u32,
    pub fencing_token: u64,
    /// 0-based attempt count; re-queued failures increment this.
    pub attempts: u32,
}

/// Inodes with exhausted writeback retries (or last hard failure) until a
/// successful flush clears them. fsync consults this for durable error reporting.
pub static WRITEBACK_HARD_FAILURES: once_cell::sync::Lazy<
    dashmap::DashMap<u64, String, ahash::RandomState>,
> = once_cell::sync::Lazy::new(|| dashmap::DashMap::with_hasher(ahash::RandomState::new()));

/// Aggregated result of the dismount active-block force-flush: one report
/// per unmount, never a log line per block.
#[derive(Debug, Default)]
pub struct TeardownFlushSummary {
    pub attempted: usize,
    pub flushed: usize,
    pub failed: usize,
    /// First few failure reasons (bounded) for the aggregated log line.
    pub error_samples: Vec<String>,
}

pub struct SqueezefsFilesystem {
    pub router: DataRouter,
    dlm: DlmClient,
    pub meta_backend: Option<std::sync::Arc<crate::meta_backend::RoutedMetaBackend>>,
    uid: u32,
    gid: u32,
    active_leases: std::sync::Arc<dashmap::DashMap<u64, crate::dlm::LockLease, ahash::RandomState>>,
    lease_locks: std::sync::Arc<StripeLocks<tokio::sync::Mutex<()>, 4096>>,
    active_posix_locks:
        std::sync::Arc<dashmap::DashMap<(Inode, u64, u64, u64), PosixLock, ahash::RandomState>>,
    active_delegations:
        std::sync::Arc<dashmap::DashMap<Inode, crate::dlm::DelegationLease, ahash::RandomState>>,
    pub active_inode_locks: std::sync::Arc<StripeLocks<tokio::sync::RwLock<()>, 4096>>,
    /// P1-4: capacity-bounded attribute cache (moka TTL + max_capacity).
    pub attr_cache: moka::sync::Cache<u64, (FileAttr, std::time::Instant)>,
    pub dir_entry_cache: moka::sync::Cache<u64, std::sync::Arc<[(std::boxed::Box<str>, u64)]>>,
    pub dismount_wait: u64,
    /// Bounded writeback queue (P1-2). Full → synchronous flush of that block.
    writeback_tx: tokio::sync::mpsc::Sender<WritebackRequest>,
    writeback_rx:
        std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<WritebackRequest>>>>,
    pub writeback_queue_cap: usize,
    pub client_id: std::sync::Arc<std::sync::Mutex<String>>,
    pub mountpoint: std::sync::Arc<std::sync::Mutex<String>>,
    pub max_background_uploads: usize,
    /// Dirty in-RAM striped block accumulation buffers. Exclusive-owner CoW
    /// values ([`crate::cache::active_block::ActiveBlockBuf`]): all mutation
    /// happens under [`BLOCK_FLUSH_LOCKS`]; readers stay lock-free
    /// (`DashMap::get` + `snapshot()`), and a live snapshot forces the next
    /// writer to copy (never mutate) that memory.
    active_block_buffers: std::sync::Arc<
        dashmap::DashMap<String, crate::cache::active_block::ActiveBlockBuf, ahash::RandomState>,
    >,
    pub open_virtual_files: dashmap::DashMap<u64, Vec<u8>, ahash::RandomState>,
    pub next_virtual_fh: std::sync::atomic::AtomicU64,
    pub latest_stats_json: arc_swap::ArcSwap<Option<std::sync::Arc<Vec<u8>>>>,
    pub latest_config_json: arc_swap::ArcSwap<Option<std::sync::Arc<Vec<u8>>>>,
    pub inodes_limit: std::sync::Arc<std::sync::OnceLock<u64>>,
    pub session_connection:
        arc_swap::ArcSwap<Option<std::sync::Arc<fuse3::raw::connection::FuseConnection>>>,
    pub open_inodes: std::sync::Arc<dashmap::DashMap<u64, usize, ahash::RandomState>>,
    pub reclaim_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    /// FUSE-over-io_uring surfaces Destroy once per queue; teardown must
    /// run exactly once.
    dismount_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reclaim_tx: tokio::sync::mpsc::Sender<u64>,
    reclaim_rx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<u64>>>>,
    pub open_dir_streams: std::sync::Arc<
        dashmap::DashMap<u64, std::sync::Arc<[(std::boxed::Box<str>, u64)]>, ahash::RandomState>,
    >,
    pub next_dir_fh: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Clone for SqueezefsFilesystem {
    fn clone(&self) -> Self {
        Self {
            router: self.router.clone(),
            dlm: self.dlm.clone(),
            meta_backend: self.meta_backend.clone(),
            uid: self.uid,
            gid: self.gid,
            active_leases: self.active_leases.clone(),
            lease_locks: self.lease_locks.clone(),
            active_posix_locks: self.active_posix_locks.clone(),
            active_delegations: self.active_delegations.clone(),
            active_inode_locks: self.active_inode_locks.clone(),
            attr_cache: self.attr_cache.clone(),
            dir_entry_cache: self.dir_entry_cache.clone(),
            dismount_wait: self.dismount_wait,
            writeback_tx: self.writeback_tx.clone(),
            writeback_rx: self.writeback_rx.clone(),
            writeback_queue_cap: self.writeback_queue_cap,
            client_id: self.client_id.clone(),
            mountpoint: self.mountpoint.clone(),
            max_background_uploads: self.max_background_uploads,
            active_block_buffers: self.active_block_buffers.clone(),
            open_virtual_files: self.open_virtual_files.clone(),
            next_virtual_fh: std::sync::atomic::AtomicU64::new(
                self.next_virtual_fh.load(Ordering::Relaxed),
            ),
            latest_stats_json: arc_swap::ArcSwap::new(self.latest_stats_json.load_full()),
            latest_config_json: arc_swap::ArcSwap::new(self.latest_config_json.load_full()),
            inodes_limit: self.inodes_limit.clone(),
            session_connection: arc_swap::ArcSwap::new(self.session_connection.load_full()),
            open_inodes: self.open_inodes.clone(),
            reclaim_semaphore: self.reclaim_semaphore.clone(),
            dismount_once: self.dismount_once.clone(),
            reclaim_tx: self.reclaim_tx.clone(),
            reclaim_rx: self.reclaim_rx.clone(),
            open_dir_streams: self.open_dir_streams.clone(),
            next_dir_fh: self.next_dir_fh.clone(),
        }
    }
}

impl SqueezefsFilesystem {
    pub fn new(router: DataRouter, dlm: DlmClient, uid: u32, gid: u32) -> Self {
        let queue_cap = std::env::var("SQUEEZEFS_WRITEBACK_QUEUE_CAP")
            .ok()
            .and_then(|val| val.parse::<usize>().ok())
            .unwrap_or(4096);
        let reclaim_concurrency = std::env::var("SQUEEZEFS_RECLAIM_CONCURRENCY")
            .ok()
            .and_then(|val| val.parse::<usize>().ok())
            .unwrap_or_else(|| {
                let cores = std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(4);
                std::cmp::max(4, cores)
            });
        info!(
            "Dynamic reclaim concurrency limit configured: {}",
            reclaim_concurrency
        );
        let (writeback_tx, writeback_rx) = tokio::sync::mpsc::channel(queue_cap);
        let (reclaim_tx, reclaim_rx) = tokio::sync::mpsc::channel(100000);
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory();
        let dir_entry_capacity = std::cmp::max(50_000, total_memory / 200_000);
        let dir_entry_cache = moka::sync::Cache::builder()
            .max_capacity(dir_entry_capacity)
            .time_to_live(Duration::from_secs(300))
            .build();
        // P1-4: bound attr cache growth (was unbounded DashMap).
        let attr_capacity = std::cmp::max(10_000, total_memory / 100_000);
        let attr_cache = moka::sync::Cache::builder()
            .max_capacity(attr_capacity)
            .time_to_live(Duration::from_secs(300))
            .build();
        Self {
            router,
            dlm,
            meta_backend: None,
            uid,
            gid,
            active_leases: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            lease_locks: std::sync::Arc::new(StripeLocks::new()),
            active_posix_locks: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            active_delegations: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            active_inode_locks: std::sync::Arc::new(StripeLocks::new()),
            attr_cache,
            dir_entry_cache,
            dismount_wait: 10,
            writeback_tx,
            writeback_rx: std::sync::Arc::new(std::sync::Mutex::new(Some(writeback_rx))),
            writeback_queue_cap: queue_cap,
            client_id: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mountpoint: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            max_background_uploads: {
                let cores = std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(4);
                std::cmp::max(16, cores * 2)
            },
            active_block_buffers: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            open_virtual_files: dashmap::DashMap::with_hasher(ahash::RandomState::new()),
            next_virtual_fh: std::sync::atomic::AtomicU64::new(0x1000_0000_0000_0000),
            latest_stats_json: arc_swap::ArcSwap::new(std::sync::Arc::new(None)),
            latest_config_json: arc_swap::ArcSwap::new(std::sync::Arc::new(None)),
            inodes_limit: std::sync::Arc::new(std::sync::OnceLock::new()),
            session_connection: arc_swap::ArcSwap::new(std::sync::Arc::new(None)),
            open_inodes: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            reclaim_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                reclaim_concurrency,
            )),
            dismount_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reclaim_tx,
            reclaim_rx: std::sync::Arc::new(std::sync::Mutex::new(Some(reclaim_rx))),
            open_dir_streams: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            next_dir_fh: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                0x2000_0000_0000_0000,
            )),
        }
    }

    pub fn max_background_uploads(&self) -> usize {
        self.max_background_uploads
    }

    pub fn add_open(&self, ino: u64) {
        let mut entry = self.open_inodes.entry(ino).or_insert(0);
        *entry += 1;
    }

    pub fn remove_open(&self, ino: u64) {
        if let Some(mut entry) = self.open_inodes.get_mut(&ino) {
            if *entry > 0 {
                *entry -= 1;
            }
        }
    }

    pub fn is_open(&self, ino: u64) -> bool {
        if let Some(entry) = self.open_inodes.get(&ino) {
            *entry > 0
        } else {
            false
        }
    }

    pub fn queue_reclaim_inode(&self, ino: u64) {
        if ino <= 1 || ino == CONFIG_INODE || ino == STATS_INODE {
            return;
        }
        if self.is_open(ino) {
            return;
        }
        let tx = self.reclaim_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(ino).await;
        });
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

    async fn sync_runtime_config_to_daemon(&self) {
        let cfg = crate::config_ops::load_or_create_config();

        for (vol_name, status) in &cfg.data_volume_statuses {
            if status == "disabled" {
                self.router
                    .backend_router
                    .unhealthy_backends
                    .insert(vol_name.clone(), true);
            } else {
                self.router
                    .backend_router
                    .unhealthy_backends
                    .remove(vol_name);
            }
        }

        if let Some(ref meta) = self.meta_backend {
            for (vol_name, status) in &cfg.metadata_volume_statuses {
                if vol_name.starts_with("meta_volume_") {
                    if let Ok(idx) = vol_name["meta_volume_".len()..].parse::<usize>() {
                        if status == "disabled" {
                            meta.disabled_volumes.insert(idx, true);
                        } else {
                            meta.disabled_volumes.remove(&idx);
                        }
                    }
                }
            }

            for (from_vol, to_vol) in &cfg.metadata_volume_redirections {
                if from_vol.starts_with("meta_volume_") && to_vol.starts_with("meta_volume_") {
                    let from_idx = from_vol["meta_volume_".len()..].parse::<usize>();
                    let to_idx = to_vol["meta_volume_".len()..].parse::<usize>();
                    if let (Ok(from_i), Ok(to_i)) = (from_idx, to_idx) {
                        meta.redirections.insert(from_i, to_i);
                    }
                }
            }
        }
    }

    async fn generate_config_json(&self) -> String {
        self.sync_runtime_config_to_daemon().await;

        let mut format_fields = std::collections::HashMap::new();
        let active_dirs = self.router.cache.nvme.staging_dirs();
        let active_dirs_str = active_dirs
            .iter()
            .map(|d| d.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(",");
        format_fields.insert("disk_cache_paths".to_string(), active_dirs_str);

        let mut data_volumes = serde_json::Map::new();
        let mut data_vol_names = vec!["backend_0".to_string()];
        for item in self.router.backend_router.backends.iter() {
            data_vol_names.push(item.key().clone());
        }

        for name in data_vol_names {
            let is_unhealthy = self
                .router
                .backend_router
                .unhealthy_backends
                .contains_key(&name);
            let status = if is_unhealthy { "disabled" } else { "enabled" };
            let health = self.router.backend_router.get_backend_health(&name);
            data_volumes.insert(
                name.clone(),
                serde_json::json!({
                    "backing_dev": name,
                    "status": status,
                    "health": health,
                }),
            );
        }

        let mut metadata_volumes = serde_json::Map::new();
        if let Some(ref meta) = self.meta_backend {
            for (idx, vol) in meta.volumes.iter().enumerate() {
                let name = format!("meta_volume_{}", idx);
                let path_str = vol.storage.device_path().to_string_lossy().to_string();
                let is_disabled = meta.disabled_volumes.contains_key(&idx);
                let status = if is_disabled { "disabled" } else { "enabled" };
                let health = meta.get_volume_health(idx).await;
                metadata_volumes.insert(
                    name,
                    serde_json::json!({
                        "backing_dev": path_str,
                        "status": status,
                        "health": health,
                    }),
                );
            }
        }

        let config_obj = serde_json::json!({
            "client_version": env!("CARGO_PKG_VERSION"),
            "format": format_fields,
            "data_volumes": data_volumes,
            "metadata_volumes": metadata_volumes,
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

    pub async fn generate_stats_json(&self) -> String {
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

        #[cfg(target_os = "linux")]
        let fuse_over_uring_sessions = fuse3::over_uring_sessions_active();
        #[cfg(not(target_os = "linux"))]
        let fuse_over_uring_sessions = 0u64;
        #[cfg(target_os = "linux")]
        let (fou_req, fou_rep, fou_err, fou_reg) = fuse3::over_uring_stats();
        #[cfg(not(target_os = "linux"))]
        let (fou_req, fou_rep, fou_err, fou_reg) = (0u64, 0u64, 0u64, 0u64);
        // §5.4 transport payload-lease signals: adoption, parked-ent
        // pressure, and the severance-boundary enforcement pair
        // (outstanding hovers at in-flight write count and returns to 0 at
        // quiesce; max age is bounded by one handler invocation).
        #[cfg(target_os = "linux")]
        let (t_leases, t_parked, t_outstanding, t_max_age) = fuse3::transport_lease_stats();
        #[cfg(not(target_os = "linux"))]
        let (t_leases, t_parked, t_outstanding, t_max_age) = (0u64, 0u64, 0u64, 0u64);

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
                "layout_inline_writes": METRICS.layout_inline_writes.load(Ordering::Relaxed),
                "layout_staged_writes": METRICS.layout_staged_writes.load(Ordering::Relaxed),
                "layout_striped_writes": METRICS.layout_striped_writes.load(Ordering::Relaxed),
                "bg_spawn_admitted": METRICS.bg_spawn_admitted.load(Ordering::Relaxed),
                "bg_spawn_rejected": METRICS.bg_spawn_rejected.load(Ordering::Relaxed),
                "uring_queue_full": METRICS.uring_queue_full.load(Ordering::Relaxed),
                "nvme_unaligned_write_fallbacks": METRICS.nvme_unaligned_write_fallbacks.load(Ordering::Relaxed),
                "lease_acquire_ok": METRICS.lease_acquire_ok.load(Ordering::Relaxed),
                "lease_acquire_fail": METRICS.lease_acquire_fail.load(Ordering::Relaxed),
                "writeback_hard_failures": METRICS.writeback_hard_failures.load(Ordering::Relaxed),
                "active_block_cow_copies": METRICS.active_block_cow_copies.load(Ordering::Relaxed),
                "write_through_blocks": METRICS.write_through_blocks.load(Ordering::Relaxed),
                "write_through_bytes": METRICS.write_through_bytes.load(Ordering::Relaxed),
                "write_through_fallbacks": METRICS.write_through_fallbacks.load(Ordering::Relaxed),
                "active_block_memset_elided_bytes": METRICS.active_block_memset_elided_bytes.load(Ordering::Relaxed),
                "meta_device_syncs": METRICS.meta_device_syncs.load(Ordering::Relaxed),
                "meta_sync_requests": METRICS.meta_sync_requests.load(Ordering::Relaxed),
                "bg_admit_available_permits": crate::bg_admit::available_permits(),
                "bg_admit_capacity": crate::bg_admit::capacity(),
                "striped_block_concurrency": crate::bg_admit::striped_block_concurrency(),
                "fuse_over_uring_sessions_active": fuse_over_uring_sessions,
                "fuse_over_uring_requests": fou_req,
                "fuse_over_uring_replies": fou_rep,
                "fuse_over_uring_cqe_errors": fou_err,
                "fuse_over_uring_registers": fou_reg,
                "transport_payload_leases": t_leases,
                "transport_parked_commits": t_parked,
                "transport_leases_outstanding": t_outstanding,
                "transport_lease_max_age_ms": t_max_age,
                "write_lock_wait": METRICS.write_lock_wait.to_json(),
                "block_lock_wait": METRICS.block_lock_wait.to_json(),
                "lease_lock_wait": METRICS.lease_lock_wait.to_json(),
                "dlm_acquire_time": METRICS.dlm_acquire_time.to_json(),
                "writeback_queue_depth": METRICS.writeback_queue_depth.to_json(),
                "meta_sector_lock_wait_ns": METRICS.meta_sector_lock_wait_ns.to_json(),
                "meta_sector_lock_contended": METRICS.meta_sector_lock_contended.load(Ordering::Relaxed),
                "meta_tx_concurrency": METRICS.meta_tx_concurrency.load(Ordering::Relaxed),
                "meta_tx_concurrency_peak": METRICS.meta_tx_concurrency_peak.load(Ordering::Relaxed),
                "meta_inode_alloc_cas_retries": METRICS.meta_inode_alloc_cas_retries.load(Ordering::Relaxed),
                "meta_inode_alloc_reconciled": METRICS.meta_inode_alloc_reconciled.load(Ordering::Relaxed),
                "meta_quarantined_inodes": METRICS.meta_quarantined_inodes.load(Ordering::Relaxed),
                "meta_commit_sectors": METRICS.meta_commit_sectors.to_json(),
                "meta_flush_deferred": METRICS.meta_flush_deferred.load(Ordering::Relaxed),
                "meta_reclaim_batch_size": METRICS.meta_reclaim_batch_size.to_json(),
                // §4.6: per-volume mount-probe classification (design
                // §Observability — live signals over ad-hoc logging).
                "meta_volume_atomicity": self
                    .meta_backend
                    .as_ref()
                    .map(|mb| {
                        mb.volumes
                            .iter()
                            .map(|v| {
                                v.atomicity_class
                                    .get()
                                    .map(|c| c.as_str())
                                    .unwrap_or("unprobed")
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
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

    pub fn get_inode_lock(&self, ino: u64) -> &tokio::sync::RwLock<()> {
        self.active_inode_locks.get_inode_lock(ino)
    }

    pub fn get_inode_lock_ref(&self, ino: u64) -> &tokio::sync::RwLock<()> {
        self.active_inode_locks.get_inode_lock(ino)
    }

    /// Write/refresh this client's mount registration (`client:{id}` xattr on the
    /// root inode) with a fresh heartbeat timestamp. A peer reads the timestamp to
    /// tell a live mount from a crashed one: an entry older than
    /// [`CLIENT_STALE_TTL_SECS`] is treated as stale (kill -9 leaves no chance to
    /// unregister). Best-effort; never fails a caller.
    pub async fn refresh_client_registration(&self) {
        let client_id_str = self.client_id.lock().unwrap().clone();
        if client_id_str.is_empty() {
            return;
        }
        let Some(backend) = self.meta_backend.as_ref() else {
            return;
        };
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        // Compact JSON: {"ts":<unix_secs>,"pid":<pid>}. pid aids same-host diagnosis;
        // the timestamp is the authoritative cross-node liveness signal.
        let val = format!("{{\"ts\":{},\"pid\":{}}}", ts, std::process::id());
        let attr_name = format!("client:{}", client_id_str);
        let _ = backend.setxattr(1, &attr_name, val.as_bytes()).await;
    }

    async fn get_or_acquire_lease(&self, ino: u64) -> Result<u64, SqueezefsError> {
        // Hot path: return cached fencing token without re-validating the DLM map
        // on every write (was a lock/hash hit per op). Stale tokens are rejected by
        // write_file / save_metadata fencing checks; callers invalidate on that path.
        if let Some(lease) = self.active_leases.get(&ino) {
            return Ok(lease.fencing_token());
        }

        let lock_arc = self.lease_locks.get_lock(ino, 0);
        let start_lease_lock = std::time::Instant::now();
        let _guard = lock_arc.lock().await;
        METRICS.lease_lock_wait.record(start_lease_lock.elapsed());

        if let Some(lease) = self.active_leases.get(&ino) {
            return Ok(lease.fencing_token());
        }

        let file_path = crate::keys::inode_path(ino);
        let start_dlm = std::time::Instant::now();
        let lease = self
            .dlm
            .acquire_lock(&file_path, None, Duration::from_secs(5))
            .await?;
        METRICS.dlm_acquire_time.record(start_dlm.elapsed());
        let token = lease.fencing_token();
        self.active_leases.insert(ino, lease);
        Ok(token)
    }

    /// Drop a locally cached lease (e.g. after `FencingTokenExpired` or lock loss).
    pub fn invalidate_local_lease(&self, ino: u64) {
        if let Some((_, lease)) = self.active_leases.remove(&ino) {
            // Best-effort async release if runtime present.
            drop(lease);
        }
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
                    // Local mock DLM: no locks to load from Redis
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

    pub async fn flush_memory_buffers_for_inode(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let mut keys_to_flush = Vec::new();
        for r in self.active_block_buffers.iter() {
            let key = r.key();
            let prefix = crate::keys::active_block_ino_prefix(ino);
            if key.starts_with(prefix.as_str()) {
                keys_to_flush.push(key.clone());
            }
        }

        for key in keys_to_flush {
            let Some((_, b)) = Self::parse_active_block_key(&key) else {
                continue;
            };
            // Stage/upload exit under the victim's block lock (§5.3 exit 2;
            // normal await — this path holds no other block locks):
            // zero-complete Fresh buffers so recycled pool bytes never
            // reach staging or the device, and serialize against a
            // concurrent write's checkout of the same block.
            let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
            let block_guard = block_lock.lock().await;
            let Some((_, mut block_data)) = self.active_block_buffers.remove(&key) else {
                drop(block_guard);
                continue;
            };
            block_data.zero_complete();
            let nvme_clone = self.router.cache.nvme.clone();
            let key_clone = key.clone();
            let staging_copy = block_data.snapshot();
            let admitted = tokio::task::spawn_blocking(move || {
                nvme_clone.put_active_block(&key_clone, &staging_copy, fencing_token)
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

            if admitted {
                drop(block_guard);
                let req = WritebackRequest {
                    ino,
                    block_idx: b,
                    fencing_token,
                    attempts: 0,
                };
                self.enqueue_writeback(req).await?;
            } else {
                // Staging refused (never-lossy backpressure): this is the
                // fsync path, so make the block durable right now. The
                // escalation merges via the shared primitive
                // (INODE_META_LOCKS — after BLOCK_FLUSH_LOCKS in the P1-9
                // extended order), so holding the block guard is legal.
                upload_active_block_bytes(
                    ino,
                    b,
                    block_data.snapshot(),
                    fencing_token,
                    &self.router,
                )
                .await?;
                drop(block_guard);
            }
        }
        Ok(())
    }

    /// Staged/striped active-block write path. Safe to call without holding the
    /// per-inode write lock: mutates each block under [`BLOCK_FLUSH_LOCKS`].
    ///
    /// P1-10: the fencing check uses a short-lived meta connection that is dropped
    /// before any active-block / backend I/O (block tasks open their own connections).
    pub async fn write_file_staged(
        &self,
        ino: u64,
        offset: u64,
        data: bytes::Bytes,
        existing_size: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        {
            let current_fencing = self.dlm.get_fencing_token_ino(ino);
            if fencing_token < current_fencing {
                return Err(SqueezefsError::FencingTokenExpired {
                    token: fencing_token,
                    expected: current_fencing,
                });
            }
        }

        let block_size = self.router.block_size.load(Ordering::Relaxed);
        let start_block = offset / block_size;
        let end_block = (offset + data.len() as u64 - 1) / block_size;

        let mut futures = Vec::new();
        let mut data_cursor = 0usize;
        for b in start_block..=end_block {
            let b_start_offset = b * block_size;
            let b_end_offset = b_start_offset + block_size;

            let write_start = std::cmp::max(offset, b_start_offset);
            let write_end = std::cmp::min(offset + data.len() as u64, b_end_offset);
            let slice_len = (write_end - write_start) as usize;

            let file_data_slice = &data[data_cursor..data_cursor + slice_len];
            data_cursor += slice_len;

            let cache_key = crate::keys::active_block(ino, b as u64).to_string();
            let needs_existing_data = Self::block_write_needs_existing_data(
                existing_size,
                b_start_offset,
                b_end_offset,
                write_start,
                write_end,
            );

            let file_path = crate::keys::inode_path(ino);

            futures.push(async move {
                // 0. Acquire Block-level Lock to prevent concurrent modification to the same block
                let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b as u32);
                let start_block_lock = std::time::Instant::now();
                let block_guard = block_lock.lock().await;
                METRICS.block_lock_wait.record(start_block_lock.elapsed());

                // 1. Get existing block data (either from memory cache, NVMe staging cache, or read from backend/cache)
                let mut block_data = if let Some((_, buf)) =
                    self.active_block_buffers.remove(&cache_key)
                {
                    buf
                } else if let Some(d) = self.router.cache.nvme.read_staged(&cache_key) {
                    crate::cache::active_block::ActiveBlockBuf::seeded(&d, block_size as usize)
                } else if !needs_existing_data {
                    // Fresh entry: no existing data for this block, so the
                    // seed-time zero-fill is elided (§5.3) — the `covered`
                    // interval below keeps recycled pool bytes private, and
                    // the complement is zeroed lazily at the trigger or at
                    // any stage/upload exit.
                    crate::cache::active_block::ActiveBlockBuf::fresh(block_size as usize)
                } else {
                    // Try cache first
                    let mut block_map_id_opt = None;
                    if let Some(entry) = self.router.metadata_cache.get(&file_path) {
                        if entry.cached_at.elapsed() < Duration::from_secs(1) {
                            block_map_id_opt = entry.block_map_id.clone();
                        }
                    }

                    let mut block_map_id = block_map_id_opt.clone();
                    let mut block_map = None;
                    if block_map_id.is_none() {
                        if let Ok(meta) = self.router.fetch_metadata(&file_path).await {
                            block_map_id = meta.block_map_id.clone();
                            block_map = meta.block_map.clone();
                        }
                    }

                    let mut existing = bytes::Bytes::new();
                    // Read the existing block for the read-modify-write whenever the
                    // file has a block map — whether stored INLINE (`block_map`, the
                    // common <=32-block case) or via an INDIRECT block (`block_map_id`).
                    // Gating only on `block_map_id` skipped the existing-block read for
                    // inline maps, so a partial (non-block-aligned) overwrite of a
                    // striped file zeroed the un-overwritten bytes of the block.
                    if block_map_id.is_some() || block_map.is_some() {
                        let mut old_block_key: Option<String> = None;
                        if let Some(ref bm) = block_map {
                            old_block_key = bm.get(&(b as u32)).cloned();
                        }
                        if old_block_key.is_none() {
                            if let Ok(meta) = self.router.fetch_metadata(&file_path).await {
                                if let Some(ref bm) = meta.block_map {
                                    old_block_key = bm.get(&(b as u32)).cloned();
                                }
                            }
                        }

                        if let Some(bk) = old_block_key {
                            existing = if let Some(cached_block) =
                                self.router.cache.read_lru.get(&bk)
                            {
                                cached_block
                            } else if let Some(cached) =
                                self.router.cache.nvme.get_cached_read_block(&bk)
                            {
                                let cb = bytes::Bytes::from(cached);
                                self.router.cache.read_lru.put(&bk, cb.clone());
                                cb
                            } else {
                                // NVMe-oF backend read path
                                let get_res = async {
                                    let raw = self.router.read_nvme_block(&bk).await?;
                                    let decompressed =
                                        self.router.get_crypto().process_read_async(raw).await?;
                                    Ok::<bytes::Bytes, SqueezefsError>(decompressed)
                                }
                                .await;

                                let decompressed_bytes = get_res?;
                                self.router
                                    .cache
                                    .read_lru
                                    .put(&bk, decompressed_bytes.clone());
                                if decompressed_bytes.len() < 64 * 1024 {
                                    let _ = self
                                        .router
                                        .cache
                                        .nvme
                                        .cache_read_block(&bk, decompressed_bytes.clone());
                                } else {
                                    let nvme_clone = self.router.cache.nvme.clone();
                                    let bk_clone = bk.clone();
                                    let decompressed_clone = decompressed_bytes.clone();
                                    tokio::task::spawn_blocking(move || {
                                        let _ = nvme_clone
                                            .cache_read_block(&bk_clone, decompressed_clone);
                                    });
                                }
                                decompressed_bytes
                            };
                        }
                    }

                    crate::cache::active_block::ActiveBlockBuf::seeded(
                        &existing,
                        block_size as usize,
                    )
                };

                // 2. Merge the request slice. `make_mut` mutates only
                // provably-unique memory: a live reader snapshot forces a
                // copy-on-write instead of mutating aliased bytes (P0 fix,
                // zero-copy write-path design §5.2). Coverage bookkeeping
                // first (§5.3): a gap write zeroes the complement before
                // the merge lands.
                let rel_start = (write_start - b_start_offset) as usize;
                block_data.record_write(rel_start, rel_start + slice_len);
                block_data.make_mut()[rel_start..rel_start + slice_len]
                    .copy_from_slice(file_data_slice);

                // 3. Write-through when the block is content-complete
                // (normative trigger — byte-identical to the old staging
                // point: write_end == b_end_offset for every entry kind and
                // fill order), else keep in memory. §5.3: content-validity
                // is established AT the trigger (zero the uncovered
                // complement of a Fresh entry), never required before it.
                let is_block_complete = write_end == b_end_offset;
                if is_block_complete {
                    block_data.zero_complete();
                    match self
                        .upload_full_block(ino, b as u32, block_data.snapshot(), fencing_token)
                        .await
                    {
                        Ok(()) => {
                            METRICS.write_through_blocks.fetch_add(1, Ordering::Relaxed);
                            METRICS
                                .write_through_bytes
                                .fetch_add(block_size, Ordering::Relaxed);
                            std::mem::drop(block_guard);
                        }
                        Err(e @ SqueezefsError::FencingTokenExpired { .. }) => {
                            // A fenced-out writer must not publish anywhere —
                            // not even to staging. Propagate; the caller
                            // invalidates the local lease.
                            return Err(e);
                        }
                        Err(e) => {
                            // Never-lossy fallback (uring backpressure /
                            // allocator / device failure): degrade into
                            // today's staging + writeback path.
                            METRICS
                                .write_through_fallbacks
                                .fetch_add(1, Ordering::Relaxed);
                            warn!(
                                "write-through failed for ino {} block {} ({:?}); \
                                 falling back to staging",
                                ino, b, e
                            );
                            let nvme_clone = self.router.cache.nvme.clone();
                            let cache_key_clone = cache_key.clone();
                            let fencing_token_val = fencing_token;
                            let block_snapshot = block_data.snapshot();
                            let admitted = tokio::task::spawn_blocking(move || {
                                nvme_clone.put_active_block(
                                    &cache_key_clone,
                                    &block_snapshot,
                                    fencing_token_val,
                                )
                            })
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?;

                            std::mem::drop(block_guard);

                            if admitted {
                                let req = WritebackRequest {
                                    ino,
                                    block_idx: b as u32,
                                    fencing_token,
                                    attempts: 0,
                                };
                                self.enqueue_writeback(req).await?;
                            } else {
                                // Staging refused too (never-lossy
                                // backpressure): keep the block in RAM like
                                // a partial block; fsync's buffer flush
                                // re-attempts staging or uploads it durably.
                                self.insert_active_block_buffer(
                                    cache_key.clone(),
                                    block_data,
                                    fencing_token,
                                );
                            }
                        }
                    }
                } else {
                    self.insert_active_block_buffer(cache_key.clone(), block_data, fencing_token);
                    std::mem::drop(block_guard);
                }

                Ok::<(), SqueezefsError>(())
            });
        }

        futures::future::try_join_all(futures).await?;

        Ok(())
    }

    /// Upload a content-complete block directly: crypto → allocate → DMA →
    /// block-map merge (zero-copy write-path design §5.3, PR 4). Caller
    /// holds `BLOCK_FLUSH_LOCKS(ino, b)`; MUST NOT hold the inode write
    /// guard (striped scope is MetaPrepOnly, guard already dropped — P1-8)
    /// nor any pooled meta connection (P1-10: the meta connection opens
    /// after the DMA completed and closes before returning). Returns `Err`
    /// to request the caller's never-lossy staging fallback.
    ///
    /// `plaintext` MUST be lease-free (§5.4 severance boundary): always an
    /// `ActiveBlockBuf::snapshot()` — zero-completed, block-sized,
    /// 4096-aligned (guaranteed `WriteData::Aligned` zero-copy submit) —
    /// never a transport-payload `Bytes`.
    async fn upload_full_block(
        &self,
        ino: u64,
        b: u32,
        plaintext: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        // Passthrough returns the same `Bytes` (0 copy); non-passthrough
        // transforms into a fresh buffer (§5.7).
        let processed = self
            .router
            .get_crypto()
            .process_write_async(plaintext)
            .await?;
        let (_be_id, block_allocator, nvme_writer) =
            self.router.backend_router.get_active_backend()?;
        // Marks the key's incarnation unstable: racing validated cache fills
        // of a reused key fail their seqlock check instead of caching
        // pre-DMA bytes.
        let offset = block_allocator.allocate_block().await?;
        if let Err(e) = nvme_writer.write_block(offset, processed).await {
            let _ = block_allocator.free_block(offset).await;
            return Err(e);
        }
        // Publish after the device write (incarnation ordering). No
        // `read_lru.put` for the striped hot path — deliberately mirroring
        // `flush_single_active_block`'s `!is_striped` gate: a 10 GiB stream
        // would otherwise evict genuinely hot read data with 2,560 plaintext
        // blocks.
        block_allocator.publish_block(offset);
        let new_key = offset.to_string();
        // A no-put owner must PURGE the reused key's read tiers instead
        // (PR 6 equivalence with the deleted `write_striped` direct route,
        // whose unconditional fresh-plaintext put overwrote any stale
        // entry): block keys are offset strings, and a validated fill of
        // the key's DYING incarnation may legally publish its bytes in the
        // window between the previous owner's merge-purge and
        // `free_block`'s retire (the word is still stable there). Such a
        // poison publish strictly precedes our `allocate_block` above
        // (which bumped the incarnation word — later fill re-checks fail),
        // so purging here, after the DMA and before the map names the key,
        // leaves no interleaving that can serve the dead incarnation's
        // bytes for this block.
        self.router.cache.read_lru.remove(&new_key);
        self.router.cache.nvme.remove_cached_read_block(&new_key);

        // Block-map merge via the shared primitive (§5.3 one merge
        // discipline) under INODE_META_LOCKS: current-map RMW, fencing
        // revalidation, RAM cache republish, displaced-key tier purge. The
        // completed write ends exactly at the block end, so the file is at
        // least that large.
        let min_size = (b as u64 + 1) * self.router.block_size.load(Ordering::Relaxed);
        let entries = [(b, new_key)];
        let displaced = match self
            .router
            .merge_block_mappings(
                ino,
                crate::routing::BlockMapOp::Merge(&entries),
                min_size,
                crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                fencing_token,
            )
            .await
        {
            Ok(d) => d,
            Err(e) => {
                // The DMA'd block is unreachable (never published to the
                // map): free it before surfacing the error.
                let _ = block_allocator.free_block(offset).await;
                return Err(e);
            }
        };
        // Free displaced keys only after the new map is published (durable +
        // cached), so no reader can resolve a block to a key we are freeing.
        for bk in displaced {
            let _ = self.router.backend_router.free_block(&bk).await;
        }

        // Invalidate AFTER the meta publish: a read racing between DMA and
        // publish still hits the RAM snapshot (correct); after removal it
        // resolves via the published block map. Any stale queued
        // WritebackRequest for this key becomes a no-op (its staged source
        // is gone). A stale whole-file RAM snapshot would serve pre-write
        // bytes — drop it, as the routing striped merge does.
        let cache_key = crate::keys::active_block(ino, b as u64).to_string();
        self.active_block_buffers.remove(&cache_key);
        self.router.cache.nvme.remove_active_block(&cache_key);
        let file_path = crate::keys::inode_path(ino);
        self.router.cache.write_lru.remove(&file_path);
        self.router.cache.read_lru.remove(&file_path);
        Ok(())
    }

    async fn flush_active_blocks_with_retry(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let prefix = crate::keys::active_block_ino_prefix(ino);

        // Collect block indices from in-RAM partial buffers only. Do NOT scan the
        // entire staging key space (was O(staged_files) per fsync and dominated
        // small-file sync_all benches as n grew).
        let mut block_indices: Vec<u32> = Vec::new();
        for r in self.active_block_buffers.iter() {
            let key = r.key();
            if !key.starts_with(prefix.as_str()) {
                continue;
            }
            let b_str = key
                .trim_start_matches(prefix.as_str())
                .trim_start_matches("block_");
            if let Ok(b) = b_str.parse::<u32>() {
                block_indices.push(b);
            }
        }

        // Also flush complete active blocks already in the mmap staging segment
        // under known keys (without a full list_keys scan): probe block indices
        // that have a staged active_block entry via the in-RAM set above, plus
        // any indices still referenced by a pending writeback for this ino is
        // handled by the writeback worker. For pure file_id staged small files
        // there are no active_block keys — this returns immediately.
        if !block_indices.is_empty() {
            // Spill RAM buffers to staging first so flush_single can see them.
            self.flush_memory_buffers_for_inode(ino, fencing_token)
                .await?;
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

        WRITEBACK_HARD_FAILURES.remove(&ino);
        Ok(())
    }

    /// Public flush of staged active blocks + dirty layout for an inode.
    /// Propagates I/O errors so callers (fsync, tests) can fail the durable op.
    pub async fn flush_inode_to_backend(
        &self,
        ino: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        self.flush_memory_buffers_for_inode(ino, fencing_token)
            .await?;
        self.flush_active_blocks_with_retry(ino, fencing_token)
            .await?;
        let file_path = crate::keys::inode_path(ino);
        let file_id_opt = self
            .router
            .metadata_cache
            .get(&file_path)
            .and_then(|m| m.file_id.clone());

        let sync_data_fut = async {
            if let Some(file_id) = file_id_opt {
                let key_bytes = bytes::Bytes::copy_from_slice(file_id.as_bytes());
                self.router
                    .cache
                    .nvme
                    .staging_nvme_cache
                    .sync_key(&key_bytes)
                    .await?;
            }
            Ok::<(), SqueezefsError>(())
        };

        let sync_meta_fut = async {
            self.router
                .persist_dirty_layout_if_needed(&file_path, fencing_token)
                .await?;
            if let Some(backend) = self.meta_backend.as_ref() {
                backend.sync_device_for_ino(ino).await?;
            }
            Ok::<(), SqueezefsError>(())
        };

        tokio::try_join!(sync_data_fut, sync_meta_fut)?;
        Ok(())
    }

    /// Enqueue a writeback request; if the bounded queue is full, flush that block
    /// synchronously so work is never silently dropped (P1-2).
    async fn enqueue_writeback(&self, req: WritebackRequest) -> Result<(), SqueezefsError> {
        match self.writeback_tx.try_send(req) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(r)) => {
                Err(SqueezefsError::InvalidOperation(format!(
                    "writeback channel closed for ino {} block {}",
                    r.ino, r.block_idx
                )))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(r)) => {
                warn!(
                    "Writeback queue full; synchronous flush for ino {} block {}",
                    r.ino, r.block_idx
                );
                flush_due_active_blocks_for_inode(
                    r.ino,
                    vec![r.block_idx],
                    r.fencing_token,
                    &self.router,
                    &self.dlm,
                    &self.active_inode_locks,
                )
                .await
            }
        }
    }

    /// `(ino, block)` of an `active_block:inode_{ino}:block_{b}` key.
    fn parse_active_block_key(key: &str) -> Option<(u64, u32)> {
        let rest = key.strip_prefix("active_block:inode_")?;
        let (ino_str, block_str) = rest.split_once(":block_")?;
        Some((ino_str.parse().ok()?, block_str.parse().ok()?))
    }

    fn insert_active_block_buffer(
        &self,
        cache_key: String,
        block_data: crate::cache::active_block::ActiveBlockBuf,
        fencing_token: u64,
    ) {
        'spill: while self.active_block_buffers.len() >= MAX_ACTIVE_BLOCK_BUFFERS {
            // Spill a partial buffer to local NVMe staging to free RAM —
            // under the victim's block lock via try_lock, MANDATORY (§5.3):
            // the caller already holds the lock of the block being inserted,
            // and two stripe keys can collide on one shard, so a blocking
            // acquire here can self-deadlock. On contention pick a different
            // victim or stop — the cap is soft; keeping one extra buffer
            // beats deadlock. Candidate keys are snapshotted first so the
            // map is never mutated under a live iterator guard.
            let candidates: Vec<String> = self
                .active_block_buffers
                .iter()
                .take(16)
                .map(|r| r.key().clone())
                .collect();
            if candidates.is_empty() {
                break;
            }
            let mut spilled = false;
            for spill_key in candidates {
                let Some((v_ino, v_b)) = Self::parse_active_block_key(&spill_key) else {
                    continue;
                };
                let victim_lock = BLOCK_FLUSH_LOCKS.get_lock(v_ino, v_b);
                let Ok(_victim_guard) = victim_lock.try_lock() else {
                    // Contended (possibly by this very caller's shard): a
                    // writer/flusher owns this block right now — skip it.
                    continue;
                };
                let Some((_, mut data)) = self.active_block_buffers.remove(&spill_key) else {
                    continue; // checked out by a racing writer meanwhile
                };
                // Zero-complete Fresh victims under their lock: recycled
                // pool bytes must never reach staging (§5.3 exit 2).
                data.zero_complete();
                if !self.router.cache.nvme.put_active_block(
                    &spill_key,
                    data.as_slice(),
                    fencing_token,
                ) {
                    // Staging refused (never-lossy backpressure): keep the
                    // buffer in RAM — exceeding the soft cap beats losing
                    // dirty data. fsync drains it durably.
                    self.active_block_buffers.insert(spill_key, data);
                    break 'spill;
                }
                spilled = true;
                break;
            }
            if !spilled {
                // Every candidate was contended or vanished: soft cap.
                break;
            }
        }
        self.active_block_buffers.insert(cache_key, block_data);
    }

    pub async fn flush_all_memory_buffers_to_staging(&self) -> Result<(), SqueezefsError> {
        info!("FUSE Daemon: Force flushing all in-memory write buffers to local NVMe staging...");
        let keys_to_flush: Vec<String> = self
            .active_block_buffers
            .iter()
            .map(|r| r.key().clone())
            .collect();

        for key in keys_to_flush {
            let Some((ino, b)) = Self::parse_active_block_key(&key) else {
                continue;
            };
            // Stage/upload exit under the victim's block lock (§5.3 exit 2;
            // normal await — teardown holds no other block locks):
            // zero-complete Fresh buffers before they leave RAM.
            let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
            let block_guard = block_lock.lock().await;
            let Some((_, mut block_data)) = self.active_block_buffers.remove(&key) else {
                drop(block_guard);
                continue;
            };
            block_data.zero_complete();
            let fencing_token = self.dlm.get_fencing_token_ino(ino);

            let nvme_clone = self.router.cache.nvme.clone();
            let key_clone = key.clone();
            let staging_copy = block_data.snapshot();
            let admitted = match tokio::task::spawn_blocking(move || {
                nvme_clone.put_active_block(&key_clone, &staging_copy, fencing_token)
            })
            .await
            {
                Ok(admitted) => admitted,
                Err(e) => {
                    error!(
                        "Failed to write active block to NVMe staging during dismount: {:?}",
                        e
                    );
                    continue;
                }
            };

            if !admitted {
                // Staging refused (never-lossy backpressure): dismount must
                // not strand dirty RAM — upload the block durably right now
                // (the escalation merges via the shared primitive, legal
                // under the block guard per the P1-9 extended order).
                if let Err(e) = upload_active_block_bytes(
                    ino,
                    b,
                    block_data.snapshot(),
                    fencing_token,
                    &self.router,
                )
                .await
                {
                    error!(
                        "Dismount durable upload failed for ino {} block {}: {:?}",
                        ino, b, e
                    );
                }
                continue;
            }
            drop(block_guard);

            let req = WritebackRequest {
                ino,
                block_idx: b,
                fencing_token,
                attempts: 0,
            };
            if let Err(e) = self.enqueue_writeback(req).await {
                error!(
                    "Failed to enqueue writeback during dismount for ino {}: {:?}",
                    ino, e
                );
            }
        }
        info!("FUSE Daemon: All in-memory write buffers flushed to local NVMe staging.");
        Ok(())
    }

    pub async fn flush_all_staged_blocks_to_backend(&self) -> TeardownFlushSummary {
        info!("FUSE Daemon: Force flushing all staged active blocks to NVMe-oF backend...");
        let keys = self.router.cache.nvme.list_staged_files();

        let mut active_keys = Vec::new();
        for key in keys {
            if key.starts_with("active_block:") {
                active_keys.push(key);
            }
        }

        let mut summary = TeardownFlushSummary {
            attempted: active_keys.len(),
            ..Default::default()
        };
        if active_keys.is_empty() {
            info!("FUSE Daemon: No staged active blocks to flush.");
            return summary;
        }

        info!(
            "FUSE Daemon: Found {} staged active blocks to flush.",
            active_keys.len()
        );

        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
            crate::bg_admit::striped_block_concurrency(),
        ));
        let mut tasks = futures::stream::FuturesUnordered::new();

        for key in active_keys {
            let sem_clone = sem.clone();
            let router_clone = self.router.clone();
            let dlm_clone = self.dlm.clone();
            let locks_clone = self.active_inode_locks.clone();

            tasks.push(tokio::spawn(async move {
                let _permit = sem_clone.acquire().await.ok();

                let parts: Vec<&str> = key.split(":block_").collect();
                if parts.len() != 2 {
                    return Ok(());
                }
                let ino_parts: Vec<&str> = parts[0].split("inode_").collect();
                if ino_parts.len() != 2 {
                    return Ok(());
                }
                let ino = match ino_parts[1].parse::<u64>() {
                    Ok(i) => i,
                    Err(_) => return Ok(()),
                };
                let b = match parts[1].parse::<u32>() {
                    Ok(idx) => idx,
                    Err(_) => return Ok(()),
                };

                let meta = router_clone
                    .fetch_metadata_from_backend(ino)
                    .await?
                    .unwrap_or_default();
                let is_striped = meta.file_type == "striped";
                let fencing_token = dlm_clone.get_fencing_token_ino(ino);

                flush_single_active_block(
                    ino,
                    b,
                    fencing_token,
                    &router_clone,
                    &dlm_clone,
                    &locks_clone,
                    is_striped,
                    false,
                )
                .await?;

                Ok::<(), SqueezefsError>(())
            }));
        }

        use futures::StreamExt;
        const ERROR_SAMPLES: usize = 3;
        while let Some(res) = tasks.next().await {
            match res {
                Err(join_err) => {
                    summary.failed += 1;
                    if summary.error_samples.len() < ERROR_SAMPLES {
                        summary
                            .error_samples
                            .push(format!("task panicked: {join_err:?}"));
                    }
                }
                Ok(Err(e)) => {
                    summary.failed += 1;
                    if summary.error_samples.len() < ERROR_SAMPLES {
                        summary.error_samples.push(format!("{e:?}"));
                    }
                }
                Ok(Ok(())) => summary.flushed += 1,
            }
        }

        // One aggregated report, never a line per block: an unmount racing
        // deleted files or an offline backend produces thousands of
        // identical failures (orphan active blocks are dropped with the
        // segment either way).
        if summary.failed > 0 {
            warn!(
                "FUSE Daemon: dismount active-block flush: {} flushed, {} failed of {} (first errors: {:?})",
                summary.flushed, summary.failed, summary.attempted, summary.error_samples
            );
        } else {
            info!(
                "FUSE Daemon: Force flush of staged active blocks completed ({} flushed).",
                summary.flushed
            );
        }
        summary
    }

    pub async fn force_flush_all_staged_data(&self) -> Result<(), SqueezefsError> {
        let _ = self.flush_all_memory_buffers_to_staging().await;
        let _ = self.flush_all_staged_blocks_to_backend().await;
        Ok(())
    }

    /// Whether dismount teardown has begun (destroy runs once per unmount;
    /// duplicate per-queue invocations are no-ops).
    pub fn dismount_started(&self) -> bool {
        self.dismount_once.load(Ordering::Acquire)
    }

    fn mode_to_file_type(&self, mode: u32) -> FileType {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => FileType::Directory,
            libc::S_IFLNK => FileType::Symlink,
            libc::S_IFIFO => FileType::NamedPipe,
            libc::S_IFCHR => FileType::CharDevice,
            libc::S_IFBLK => FileType::BlockDevice,
            libc::S_IFSOCK => FileType::Socket,
            _ => FileType::RegularFile,
        }
    }

    fn inode_to_file_attr(&self, inode: &crate::meta_backend::Inode) -> FileAttr {
        FileAttr {
            ino: inode.ino,
            size: inode.size,
            blocks: inode.size.div_ceil(512),
            atime: as_timestamp(inode.atime),
            mtime: as_timestamp(inode.mtime),
            ctime: as_timestamp(inode.ctime),
            kind: self.mode_to_file_type(inode.mode),
            perm: (inode.mode & 0o7777) as u16,
            nlink: inode.nlink,
            uid: inode.uid,
            gid: inode.gid,
            rdev: 0,
            blksize: 4096,
        }
    }

    async fn get_attr_internal(&self, ino: u64) -> Result<FileAttr, SqueezefsError> {
        let mut attr = match self.attr_cache.get(&ino) {
            Some((attr, cached_at)) if cached_at.elapsed() < Duration::from_secs(1) => attr,
            _ => {
                let backend = self.meta_backend.as_ref().ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })?;
                let inode = backend.getattr(ino).await?;
                let attr = self.inode_to_file_attr(&inode);
                self.attr_cache
                    .insert(ino, (attr, std::time::Instant::now()));
                attr
            }
        };

        // Size coherency: `write_file` updates `router.metadata_cache` size
        // synchronously, but the durable inode/layout size only catches up on
        // the deferred flush. Trust the hot metadata cache as the authoritative
        // logical size for regular files (both write and truncate keep it
        // current) so a stat/read/mmap issued between a write and its flush
        // never observes a stale size (e.g. 0 on a freshly written file) — which
        // otherwise truncates reads to zero and SIGBUSes mmap under the FUSE
        // writeback cache.
        if attr.kind == FileType::RegularFile {
            let file_path = crate::keys::inode_path(ino);
            if let Some(m) = self.router.metadata_cache.get(&file_path) {
                if m.size != attr.size {
                    attr.size = m.size;
                    attr.blocks = m.size.div_ceil(512);
                    self.attr_cache
                        .insert(ino, (attr, std::time::Instant::now()));
                }
            }
        }
        debug!("get_attr_internal returning: {:?}", attr);
        Ok(attr)
    }

    pub async fn complete_active_multipart_upload_if_any(
        &self,
        _ino: u64,
    ) -> Result<(), SqueezefsError> {
        Ok(())
    }

    /// Reclaim a BATCH of orphaned inos (`nlink == 0`, FORGET'd) with one
    /// group-committed destroy transaction per volume (design §4.5, PR 5).
    ///
    /// Preserves today's per-ino split around the destroy:
    /// - admission re-checks (reserved / open / getattr / nlink) per ino —
    ///   the drain-time complement of `queue_reclaim_inode`'s enqueue check;
    /// - `router.delete_file` (data-path teardown) runs BEFORE admission,
    ///   log-and-proceed on failure exactly as today (its result was always
    ///   discarded; gating on it would leak the slot forever under a
    ///   persistently failing data teardown — the slot zero is the
    ///   authoritative reclaim, blocks are refcount-recoverable);
    /// - lease / POSIX-lock / cache teardown runs per ino AFTER the batch's
    ///   commit, on success AND failure alike (invalidating before a
    ///   now-deferred zero would let a straggling getattr repopulate
    ///   attr_cache from the still-valid slot and survive as a ghost).
    ///
    /// `pub` because it is the reclaim worker's unit of work and the
    /// integration seam the bisect tests drive directly.
    pub async fn reclaim_orphaned_batch(&self, inos: Vec<u64>) {
        let _permit = self.reclaim_semaphore.acquire().await.ok();
        let Some(backend) = self.meta_backend.as_ref() else {
            return;
        };

        let mut admitted = Vec::with_capacity(inos.len());
        for ino in inos {
            if ino <= 1 || ino == CONFIG_INODE || ino == STATS_INODE {
                continue;
            }
            if self.is_open(ino) {
                debug!("RECLAIM: ino = {} is currently open, skipping reclaim", ino);
                continue;
            }
            match backend.getattr(ino).await {
                Ok(inode) if inode.nlink > 0 => {
                    debug!(
                        "RECLAIM: ino = {} has nlink = {}, skipping reclaim",
                        ino, inode.nlink
                    );
                }
                Ok(_) => admitted.push(ino),
                Err(e) => {
                    debug!("RECLAIM: getattr({}) failed: {:?}", ino, e);
                }
            }
        }
        if admitted.is_empty() {
            return;
        }

        // Data-path teardown per ino, before admission — log-and-proceed.
        for &ino in &admitted {
            let file_path = crate::keys::inode_path(ino);
            match self.dlm.get_connection().await {
                Ok(mut con) => {
                    if let Err(e) = self.router.delete_file(&file_path, &mut con).await {
                        debug!(
                            "RECLAIM: delete_file({}) failed (proceeding to destroy): {:?}",
                            ino, e
                        );
                    }
                }
                Err(e) => {
                    debug!("RECLAIM: no DLM connection for delete_file({ino}): {e:?}");
                }
            }
        }

        self.destroy_batch_bisect(backend, &admitted).await;
    }

    /// Destroy `inos` as one batch; on commit failure bisect and retry the
    /// halves, terminating at size-1 sub-batches whose behavior is
    /// byte-for-byte today's per-ino path (§4.5: one persistently bad
    /// sector must wedge only its own ino, never 63 innocents). The per-ino
    /// teardown runs on BOTH edges — only `free()` (inside
    /// `destroy_inodes`) is withheld on failure.
    async fn destroy_batch_bisect(
        &self,
        backend: &std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
        inos: &[u64],
    ) {
        if inos.is_empty() {
            return;
        }
        match backend.destroy_inodes(inos).await {
            Ok(()) => {
                for &ino in inos {
                    self.reclaim_teardown(ino).await;
                }
            }
            Err(e) if inos.len() == 1 => {
                // Today's per-ino path discarded this error silently; the
                // batched path logs it (a strict logging improvement) and
                // still runs the teardown — leaving leases to TTL expiry and
                // stale cache entries would diverge from today's behavior.
                warn!(
                    "RECLAIM: destroy failed for ino {} (slot retained, free() withheld): {:?}",
                    inos[0], e
                );
                self.reclaim_teardown(inos[0]).await;
            }
            Err(_) => {
                let mid = inos.len() / 2;
                Box::pin(self.destroy_batch_bisect(backend, &inos[..mid])).await;
                Box::pin(self.destroy_batch_bisect(backend, &inos[mid..])).await;
            }
        }
    }

    /// Per-ino post-destroy teardown, exactly today's tail: lease release,
    /// POSIX-lock cleanup, metadata/attr cache invalidation.
    async fn reclaim_teardown(&self, ino: u64) {
        if let Some((_, lease)) = self.active_leases.remove(&ino) {
            let _ = lease.release().await;
        }
        self.active_posix_locks.retain(|key, _| key.0 != ino);
        self.router
            .metadata_cache
            .remove(&crate::keys::inode_path(ino));
        self.attr_cache.invalidate(&ino);
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
        info!(
            "FUSE Daemon: Initialized Squeezefs Filesystem mount (version {}).",
            env!("CARGO_PKG_VERSION")
        );
        // Also print to stderr so operators can confirm which binary is live
        // without enabling full logging (PATH often pointed at a stale install).
        eprintln!(
            "squeezefs: mount ready (version {}, exe hint: rebuild target/release and reinstall)",
            env!("CARGO_PKG_VERSION")
        );

        if let Some(ref backend) = self.meta_backend {
            if let Ok(Some(val)) = backend.getxattr(1, "user.squeezefs.format_config").await {
                if let Ok(config) = serde_json::from_slice::<crate::FormatConfig>(&val) {
                    self.router.set_block_size(config.block_size);
                    let crypto_state = crate::crypto_compress::CryptoCompressState::new(
                        config.compression.clone(),
                        config.encrypt_algo.clone(),
                        config.encrypt_key.as_deref(),
                    );
                    self.router.set_crypto(crypto_state);
                    let _ = self.inodes_limit.set(config.inodes);
                }
            }

            // Register this client as an active mount (heartbeat-timestamped so a
            // crashed client's entry expires instead of blocking format forever).
            self.refresh_client_registration().await;
        }

        // Start background active writes flusher task
        let mut rx_guard = self.writeback_rx.lock().unwrap();
        if let Some(writeback_rx) = rx_guard.take() {
            let router = self.router.clone();
            let dlm = self.dlm.clone();
            let active_inode_locks = self.active_inode_locks.clone();
            let max_uploads = self.max_background_uploads;
            let requeue_tx = self.writeback_tx.clone();
            tokio::spawn(async move {
                run_constant_writeback_worker(
                    writeback_rx,
                    requeue_tx,
                    router,
                    dlm,
                    active_inode_locks,
                    max_uploads,
                )
                .await;
            });
        }

        // Start background GC/reclaim worker pool
        let mut reclaim_rx_guard = self.reclaim_rx.lock().unwrap();
        if let Some(reclaim_rx) = reclaim_rx_guard.take() {
            let self_clone = self.clone();
            let reclaim_concurrency = self.reclaim_semaphore.available_permits();
            tokio::spawn(async move {
                run_reclaim_worker_pool(reclaim_rx, self_clone, reclaim_concurrency).await;
            });
        }

        Ok(ReplyInit {
            max_write: std::num::NonZeroU32::new(1048576).unwrap(), // 1MB absolute maximum write buffer size
        })
    }

    async fn destroy(&self, _req: Request) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        // FUSE-over-io_uring: every queue loop observes connection teardown
        // and calls destroy — only the first runs the teardown (the repeats
        // used to re-attempt thousands of orphan flushes, starve the uring
        // worker into failing health probes, and spam ~3k ERROR lines).
        if self.dismount_once.swap(true, Ordering::SeqCst) {
            debug!("FUSE Daemon: duplicate destroy (per-queue) ignored");
            return;
        }
        info!("FUSE Daemon: Destroying mount. Force flushing all staged and memory data...");

        // Phase 1 & 2: Force flush memory buffers to staging, then staged blocks to NVMe-oF backend
        let _ = self.force_flush_all_staged_data().await;

        // Gracefully wait up to self.dismount_wait seconds for background workers to drain staged writes and active writes to NVMe-oF backend
        let start_wait = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(self.dismount_wait);
        loop {
            let n = self
                .router
                .cache
                .nvme
                .staged_writes_in_flight
                .load(Ordering::Acquire);
            if n == 0 || start_wait.elapsed() >= max_wait {
                break;
            }
            let remaining = max_wait.saturating_sub(start_wait.elapsed());
            if remaining.is_zero() {
                break;
            }
            let notify = self.router.cache.nvme.staged_drained_notify.clone();
            let _ = tokio::time::timeout(remaining, notify.notified()).await;
        }

        // Gather final count for warnings/statistics
        let keys = self.router.cache.nvme.list_staged_files();
        let mut staged_count = 0;
        let mut active_writes_count = 0;
        for key in keys {
            if key.starts_with("active_block:") {
                active_writes_count += 1;
            } else {
                staged_count += 1;
            }
        }

        if staged_count > 0 || active_writes_count > 0 {
            warn!(
                "WARNING: SqueezeFS dismounted with unflushed data! Remaining local staged files: {}, active write directories: {}. Other nodes may see inconsistent filesystem state until these are recovered or flushed.",
                staged_count, active_writes_count
            );
        } else {
            info!("FUSE Daemon: Dismount clean. All write staged blocks successfully flushed to backend.");
        }

        // Unregister this client on dismount
        let client_id_str = self.client_id.lock().unwrap().clone();
        if !client_id_str.is_empty() {
            if let Some(ref backend) = self.meta_backend {
                let attr_name = format!("client:{}", client_id_str);
                let _ = backend.removexattr(1, &attr_name).await;
            }
        }

        // Reconcile the on-disk inode bitmap from the authoritative inode table on
        // clean unmount, so a subsequent mount by a pre-PR-8 binary
        // reads a correct bitmap (design PR 2b / review Issue 18).
        if let Some(ref backend) = self.meta_backend {
            for vol in &backend.volumes {
                if let Err(e) = vol.storage.refresh_bitmap_from_table().await {
                    warn!("Inode bitmap reconciliation on unmount failed: {:?}", e);
                }
            }
        }
    }

    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_lookup");
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!("FUSE Lookup: parent = {}, name = {}", parent, name_str);

        if parent == 1 && name_str == ".config" {
            let config_data = self.generate_config_json().await;
            let bytes = config_data.into_bytes();
            let size = bytes.len() as u64;
            self.latest_config_json
                .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
            let attr = self.get_config_attr(size);
            return Ok(ReplyEntry {
                ttl: Duration::from_secs(1),
                attr,
                generation: 1,
            });
        }

        if parent == 1 && name_str == ".stats" {
            let stats_data = self.generate_stats_json().await;
            let bytes = stats_data.into_bytes();
            let size = bytes.len() as u64;
            self.latest_stats_json
                .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
            let attr = self.get_stats_attr(size);
            return Ok(ReplyEntry {
                ttl: Duration::from_secs(0), // dynamic stats shouldn't be cached long
                attr,
                generation: 1,
            });
        }

        let lookup_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })
                .map_err(map_squeezefs_err)?;
            let inode = backend
                .lookup(parent, &name_str)
                .await
                .map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
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
            let bytes = config_data.into_bytes();
            let size = bytes.len() as u64;
            self.latest_config_json
                .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
            let attr = self.get_config_attr(size);
            return Ok(ReplyAttr {
                ttl: Duration::from_secs(1),
                attr,
            });
        }

        if ino == STATS_INODE {
            let stats_data = self.generate_stats_json().await;
            let bytes = stats_data.into_bytes();
            let size = bytes.len() as u64;
            self.latest_stats_json
                .store(std::sync::Arc::new(Some(std::sync::Arc::new(bytes))));
            let attr = self.get_stats_attr(size);
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
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!(
            "FUSE mknod: parent = {}, name = {}, mode = {:o}, rdev = {}",
            parent, name_str, mode, rdev
        );

        let mknod_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let inode = backend
                .create(parent, &name_str, mode, req.uid, req.gid)
                .await
                .map_err(map_squeezefs_err)?;
            let mut attr = self.inode_to_file_attr(&inode);
            attr.rdev = rdev;
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            self.dir_entry_cache.invalidate(&parent);
            self.attr_cache.invalidate(&parent);
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
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!(
            "FUSE Create: parent = {}, name = {}, mode = {:o}, flags = {}",
            parent, name_str, mode, flags
        );

        let create_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let inode = backend
                .create(parent, &name_str, mode, req.uid, req.gid)
                .await
                .map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            // Seed layout cache so the first write skips a cold meta backend fetch.
            let file_path = crate::keys::inode_path(inode.ino);
            self.router.metadata_cache.insert(
                file_path,
                crate::routing::CachedMetadata {
                    file_type: "inline".to_string(),
                    size: 0,
                    block_map_id: None,
                    block_prefix: None,
                    file_id: None,
                    cached_at: std::time::Instant::now(),
                    data_key: None,
                    block_map: None,
                    layout_dirty: false,
                },
            );
            self.dir_entry_cache.invalidate(&parent);
            // Keep parent attr in cache; only dir_entry listing is stale.
            self.add_open(inode.ino);
            Ok(ReplyCreated {
                ttl: Duration::from_secs(1),
                attr,
                generation: 1,
                fh: inode.ino,
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

        if inode == STATS_INODE || inode == CONFIG_INODE {
            let content = if inode == STATS_INODE {
                let old_val = self.latest_stats_json.swap(std::sync::Arc::new(None));
                if let Some(bytes_arc) = &*old_val {
                    (**bytes_arc).clone()
                } else {
                    self.generate_stats_json().await.into_bytes()
                }
            } else {
                let old_val = self.latest_config_json.swap(std::sync::Arc::new(None));
                if let Some(bytes_arc) = &*old_val {
                    (**bytes_arc).clone()
                } else {
                    self.generate_config_json().await.into_bytes()
                }
            };
            let fh = self.next_virtual_fh.fetch_add(1, Ordering::Relaxed);
            self.open_virtual_files.insert(fh, content);
            // FOPEN_DIRECT_IO: the payload is regenerated per open, but the
            // kernel clamps buffered reads to i_size from a PREVIOUS
            // generation's lookup — serving truncated (unparseable) JSON
            // once the stats payload grows between generations. Direct I/O
            // makes the kernel trust our read replies (short read = EOF)
            // instead of the stale size.
            const FOPEN_DIRECT_IO: u32 = 1 << 0;
            return Ok(ReplyOpen {
                fh,
                flags: FOPEN_DIRECT_IO,
            });
        }

        self.add_open(inode);
        // File handle is just the inode number for simplicity in this design
        Ok(ReplyOpen {
            fh: inode,
            flags: 0,
        })
    }

    async fn opendir(&self, _req: Request, inode: Inode, _flags: u32) -> FuseResult<ReplyOpen> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Opendir: inode = {}", inode);

        self.add_open(inode);

        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        let list = backend
            .readdir(inode, 0, 100000)
            .await
            .map_err(map_squeezefs_err)?;
        let mut sorted_entries: Vec<(std::boxed::Box<str>, u64)> = list
            .into_iter()
            .map(|d| (d.name.into_boxed_str(), d.ino))
            .collect();
        sorted_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let map_arc: std::sync::Arc<[(std::boxed::Box<str>, u64)]> =
            std::sync::Arc::from(sorted_entries.into_boxed_slice());

        let fh = self.next_dir_fh.fetch_add(1, Ordering::Relaxed);
        self.open_dir_streams.insert(fh, map_arc);

        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn releasedir(&self, _req: Request, ino: u64, fh: u64, _flags: u32) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Releasedir: ino = {}, fh = {}", ino, fh);

        self.open_dir_streams.remove(&fh);
        self.remove_open(ino);
        // Do not reclaim on releasedir: the directory is typically still linked.
        // Destruction runs from forget (and release of unlinked files) only, and
        // only after nlink==0 — avoids racing live parents under concurrent mkdir.
        Ok(())
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
            let bytes = if let Some(cached) = self.open_virtual_files.get(&fh) {
                cached.clone()
            } else {
                self.generate_config_json().await.into_bytes()
            };
            if offset >= bytes.len() as u64 {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                    backing: None,
                });
            }
            let start = offset as usize;
            let end = std::cmp::min(bytes.len(), start + size as usize);
            // SAFETY: start < bytes.len() checked on line 2144, and end is clamped to bytes.len()
            let slice = unsafe { bytes.get_unchecked(start..end) };
            return Ok(ReplyData {
                data: slice.to_vec().into(),
                backing: None,
            });
        }

        if ino == STATS_INODE {
            let bytes = if let Some(cached) = self.open_virtual_files.get(&fh) {
                cached.clone()
            } else {
                self.generate_stats_json().await.into_bytes()
            };
            if offset >= bytes.len() as u64 {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                    backing: None,
                });
            }
            let start = offset as usize;
            let end = std::cmp::min(bytes.len(), start + size as usize);
            // SAFETY: start < bytes.len() checked on line 2166, and end is clamped to bytes.len()
            let slice = unsafe { bytes.get_unchecked(start..end) };
            return Ok(ReplyData {
                data: slice.to_vec().into(),
                backing: None,
            });
        }

        let file_path = crate::keys::inode_path(ino);
        let lock = self.get_inode_lock_ref(ino);

        // Short critical section only: size bound + active-buffer hit.
        // Must NOT hold the inode read lock across flush or backend I/O —
        // flush_single_active_block upgrades to write() and tokio RwLock is
        // not re-entrant (self-deadlock under multi-block / active-block reads).
        let (file_size, active_hit) = {
            let _guard = lock.read().await;

            let mut file_size = if let Some((attr, _)) = self.attr_cache.get(&ino) {
                attr.size
            } else {
                let backend = self
                    .meta_backend
                    .as_ref()
                    .ok_or_else(|| {
                        SqueezefsError::InvalidOperation(
                            "Metadata backend not initialized".to_string(),
                        )
                    })
                    .map_err(map_squeezefs_err)?;
                backend
                    .getattr(ino)
                    .await
                    .map(|inode| inode.size)
                    .unwrap_or(0)
            };

            // Size coherency: prefer the router metadata cache, which the write
            // path updates synchronously. The durable inode/attr caches can lag
            // a just-committed write until its deferred flush, so without this a
            // read racing a write (kernel readahead under the writeback cache)
            // would observe a stale size (0 on a fresh file), return a short
            // read, and let the kernel cache zero pages — silent read-after-
            // write corruption.
            if let Some(m) = self.router.metadata_cache.get(&file_path) {
                file_size = m.size;
            }

            if offset >= file_size {
                return Ok(ReplyData {
                    data: Vec::new().into(),
                    backing: None,
                });
            }

            let read_len = std::cmp::min(size as u64, file_size - offset) as usize;
            let block_size = self.router.block_size.load(Ordering::Relaxed);
            let start_block = offset / block_size;
            let end_block = (offset + read_len as u64 - 1) / block_size;

            if start_block == end_block {
                let cache_key = crate::keys::active_block(ino, start_block).to_string();
                if let Some(buf) = self.active_block_buffers.get(&cache_key) {
                    let block_start = start_block * block_size;
                    let rel_offset = (offset - block_start) as usize;
                    let rel_end = rel_offset + read_len;
                    // Zero-copy CoW-stable snapshot: immutable for the
                    // reply's whole lifetime — a later write to this block
                    // copies instead of mutating these bytes (P0 fix).
                    // Coverage-aware (§5.3): snapshot + covered interval are
                    // read from the same entry, so the pair is consistent;
                    // memset elision means the uncovered range of a Fresh
                    // buffer holds recycled pool bytes that must NEVER be
                    // served through the kernel.
                    let (snapshot, covered) = buf.value().covered_snapshot();
                    let data = if covered.0 as usize <= rel_offset && rel_end <= covered.1 as usize
                    {
                        // Common case (every Seeded/content-valid entry and
                        // every sequential read): zero-copy slice.
                        snapshot.slice(rel_offset..rel_end)
                    } else {
                        // Rare sparse read overlapping uncovered bytes:
                        // build the reply in a fresh buffer — zeros plus
                        // covered ∩ range — WITHOUT mutating the shared
                        // buffer (zeroing in place here would be a mutation
                        // outside BLOCK_FLUSH_LOCKS).
                        let mut out = vec![0u8; read_len];
                        let is = (covered.0 as usize).max(rel_offset);
                        let ie = (covered.1 as usize).min(rel_end);
                        if is < ie {
                            out[is - rel_offset..ie - rel_offset]
                                .copy_from_slice(&snapshot[is..ie]);
                        }
                        bytes::Bytes::from(out)
                    };
                    return Ok(ReplyData {
                        data,
                        backing: None,
                    });
                }
                (file_size, false)
            } else {
                (file_size, true) // need flush of dirty active blocks first
            }
        };

        let read_len = std::cmp::min(size as u64, file_size - offset) as usize;

        if active_hit {
            let fencing_token = self.dlm.get_fencing_token_ino(ino);
            let _ = self
                .flush_active_blocks_with_retry(ino, fencing_token)
                .await;
        }

        let conn_guard = self.session_connection.load();
        let dest_addr = conn_guard
            .as_ref()
            .as_ref()
            .and_then(|conn| conn.get_payload_buffer(_req.unique))
            .map(|(ptr, _sz)| ptr);

        // Backend / cache read without holding the inode lock (readers scale).
        let read_future =
            self.router
                .read_file_range_zero_copy(&file_path, offset, read_len as u32, dest_addr);
        let read_timeout = std::cmp::max(get_fuse_timeout(), Duration::from_secs(30));
        let (data, backing) = match tokio::time::timeout(read_timeout, read_future).await {
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
        data: bytes::Bytes,
        _write_flags: u32,
        _flags: u32,
    ) -> FuseResult<ReplyWrite> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_write");

        if ino == CONFIG_INODE || ino == STATS_INODE {
            return Err(Errno::from(libc::EACCES));
        }

        // Record writeback queue depth
        let queue_depth = self.writeback_queue_cap - self.writeback_tx.capacity();
        METRICS.writeback_queue_depth.record(queue_depth);

        let write_future = async {
            let lock = self.get_inode_lock_ref(ino);
            let start_wait = std::time::Instant::now();
            let guard = lock.write().await;
            METRICS.write_lock_wait.record(start_wait.elapsed());

            // 1. Get or acquire lease (fencing token)
            let fencing_token = self
                .get_or_acquire_lease(ino)
                .await
                .map_err(map_squeezefs_err)?;

            let file_path = crate::keys::inode_path(ino);
            let block_size = self.router.block_size.load(Ordering::Relaxed);
            // Prefer hot caches for path selection (avoids meta RTT on every small write).
            // write_file still loads authoritative layout when it mutates data.
            let (old_size, file_type) = if let Some(m) = self.router.metadata_cache.get(&file_path)
            {
                (m.size, m.file_type.clone())
            } else if let Some((attr, cached_at)) = self.attr_cache.get(&ino) {
                if cached_at.elapsed() < Duration::from_secs(1) {
                    let ft = if attr.size > block_size {
                        "striped".to_string()
                    } else if attr.size > MAX_INLINE_SIZE {
                        "staged".to_string()
                    } else {
                        "inline".to_string()
                    };
                    (attr.size, ft)
                } else {
                    let meta = self
                        .router
                        .fetch_metadata(&file_path)
                        .await
                        .map_err(map_squeezefs_err)?;
                    (meta.size, meta.file_type.clone())
                }
            } else {
                let meta = self
                    .router
                    .fetch_metadata(&file_path)
                    .await
                    .map_err(map_squeezefs_err)?;
                (meta.size, meta.file_type.clone())
            };
            let is_striped = file_type == "striped";

            let bytes_written = data.len() as u32;
            let expected_new_size = std::cmp::max(old_size, offset + bytes_written as u64);

            let fits_inline = expected_new_size <= MAX_INLINE_SIZE
                && file_type != "staged"
                && file_type != "striped";
            let fits_staged = expected_new_size <= block_size
                && !self.router.cache.nvme.staging_dirs().is_empty()
                && file_type != "striped";

            let use_router_write =
                fits_inline || fits_staged || file_type == "inline" || file_type == "staged";
            let lock_scope = if file_type == "staged" && expected_new_size <= block_size {
                InodeWriteLockScope::MetaPrepOnly
            } else {
                inode_write_lock_scope(use_router_write, is_striped)
            };

            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let sec = now.as_secs() as i64;
            let nsec = now.subsec_nanos();

            // Publish size/mtime to attr_cache under the write lock
            if let Some((mut attr, _)) = self.attr_cache.get(&ino) {
                attr.size = expected_new_size;
                attr.blocks = expected_new_size.div_ceil(512);
                attr.mtime = Timestamp::new(sec, nsec);
                attr.ctime = Timestamp::new(sec, nsec);
                self.attr_cache
                    .insert(ino, (attr, std::time::Instant::now()));
            }

            if use_router_write {
                // §5.4 lease-severance boundary — the single sever route:
                // the router's inline/staged commits retain the payload
                // `Bytes` unboundedly (`data_key`, `write_lru`, `read_lru`)
                // and its internal promotions/striped section slice it
                // across device writes (and, post-PR 6, retain full-coverage
                // slices in the read LRU) — a transport payload lease here
                // would park the ring ent's COMMIT_AND_FETCH for as long as
                // the cache holds it (deterministic mount hang at
                // Q_DEPTH=4). Materialize a private copy before anything
                // reaches `DataRouter::write_file`.
                let data_bytes = sever_payload(&data);
                if lock_scope == InodeWriteLockScope::MetaPrepOnly {
                    drop(guard);
                    if let Err(e) = self
                        .router
                        .write_file(&file_path, offset, data_bytes, fencing_token)
                        .await
                    {
                        if matches!(e, SqueezefsError::FencingTokenExpired { .. }) {
                            self.invalidate_local_lease(ino);
                        }
                        return Err(map_squeezefs_err(e));
                    }
                } else {
                    if let Err(e) = self
                        .router
                        .write_file(&file_path, offset, data_bytes, fencing_token)
                        .await
                    {
                        if matches!(e, SqueezefsError::FencingTokenExpired { .. }) {
                            self.invalidate_local_lease(ino);
                        }
                        return Err(map_squeezefs_err(e));
                    }
                    drop(guard);
                }
            } else {
                // `use_router_write` is unconditionally true for
                // `file_type == "inline" || "staged"` (the condition names
                // those values verbatim), so this branch only ever sees
                // striped files — inline/staged→striped promotions resolve
                // inside `DataRouter::write_file` on the severed route
                // above. Every striped shape (aligned or not, complete
                // blocks or partial) funnels through `write_file_staged`,
                // whose per-block write-through (§5.3) uploads
                // content-complete blocks directly: the one striped write
                // path. PR 6 deleted the in-handler promotion block (dead:
                // its `file_type` guard could never hold here) and the
                // subsumed `is_aligned` direct leg.
                if expected_new_size > old_size {
                    self.router
                        .update_metadata_cache_size(&file_path, expected_new_size)
                        .await;
                }
                // Striped: drop inode write lock before long active-block
                // I/O (P1-8); per-block BLOCK_FLUSH_LOCKS serialize the
                // data path.
                drop(guard);
                if let Err(e) = self
                    .write_file_staged(ino, offset, data.clone(), old_size, fencing_token)
                    .await
                {
                    if matches!(e, SqueezefsError::FencingTokenExpired { .. }) {
                        self.invalidate_local_lease(ino);
                    }
                    return Err(map_squeezefs_err(e));
                }
            }

            Ok(ReplyWrite {
                written: bytes_written,
            })
        };

        let write_timeout = std::cmp::max(get_fuse_timeout(), Duration::from_secs(30));
        match tokio::time::timeout(write_timeout, write_future).await {
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
        umask: u32,
    ) -> FuseResult<ReplyEntry> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!(
            "FUSE mkdir: parent = {}, name = {}, mode = {:o}, umask = {:o}",
            parent, name_str, mode, umask
        );

        let mkdir_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let final_mode = ((mode & !umask) & 0o7777) | libc::S_IFDIR;
            let inode = backend
                .create(parent, &name_str, final_mode, req.uid, req.gid)
                .await
                .map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            self.dir_entry_cache.invalidate(&parent);
            self.attr_cache.invalidate(&parent);
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
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!("FUSE rmdir: parent = {}, name = {}", parent, name_str);

        let rmdir_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let current_inode = backend
                .lookup(parent, &name_str)
                .await
                .map_err(map_squeezefs_err)?;
            if current_inode.mode & libc::S_IFMT == libc::S_IFDIR {
                let list = backend
                    .readdir(current_inode.ino, 0, 100)
                    .await
                    .map_err(map_squeezefs_err)?;
                let has_other_entries = list.iter().any(|d| d.name != "." && d.name != "..");
                if has_other_entries {
                    return Err(Errno::from(libc::ENOTEMPTY));
                }
            } else {
                return Err(Errno::from(libc::ENOTDIR));
            }
            backend
                .unlink(parent, &name_str)
                .await
                .map_err(map_squeezefs_err)?;
            self.dir_entry_cache.invalidate(&parent);
            self.dir_entry_cache.invalidate(&current_inode.ino);
            self.attr_cache.invalidate(&parent);
            self.attr_cache.invalidate(&current_inode.ino);
            // Reclaim only after FUSE forget (or last release if unlinked-open).
            // Destroying before forget reuses ino numbers while the kernel still
            // holds the nodeid (generation always 1) → ESTALE under load.
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
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let current_inode = backend.getattr(ino).await.map_err(map_squeezefs_err)?;
            let mut size_to_set = None;
            let mut mode_to_set = None;
            if let Some(size) = set_attr.size {
                const MAX_FILE_SIZE: u64 = i64::MAX as u64;
                if size > MAX_FILE_SIZE {
                    return Err(Errno::from(libc::EFBIG));
                }
                size_to_set = Some(size);
            }
            let mut uid_to_set = None;
            let mut gid_to_set = None;
            let mut atime_to_set = None;
            let mut mtime_to_set = None;
            let mut ctime_to_set = None;
            if let Some(mode) = set_attr.mode {
                let new_mode = (current_inode.mode & libc::S_IFMT) | (mode & 0o7777);
                mode_to_set = Some(new_mode);
            }
            if let Some(uid) = set_attr.uid {
                uid_to_set = Some(uid);
            }
            if let Some(gid) = set_attr.gid {
                gid_to_set = Some(gid);
            }
            if let Some(atime) = set_attr.atime {
                atime_to_set = Some(atime.sec as u64 * 1_000_000_000 + atime.nsec as u64);
            }
            if let Some(mtime) = set_attr.mtime {
                mtime_to_set = Some(mtime.sec as u64 * 1_000_000_000 + mtime.nsec as u64);
            }
            if let Some(ctime) = set_attr.ctime {
                ctime_to_set = Some(ctime.sec as u64 * 1_000_000_000 + ctime.nsec as u64);
            }
            let _guard = if size_to_set.is_some() {
                Some(self.active_inode_locks.get_inode_lock(ino).write().await)
            } else {
                None
            };

            if let Some(new_size) = size_to_set {
                let fencing_token = self.dlm.get_fencing_token_ino(ino);
                self.router
                    .truncate_layout(ino, new_size, fencing_token)
                    .await
                    .map_err(map_squeezefs_err)?;
            }

            let inode = backend
                .setattr(
                    ino,
                    mode_to_set,
                    uid_to_set,
                    gid_to_set,
                    size_to_set,
                    atime_to_set,
                    mtime_to_set,
                    ctime_to_set,
                )
                .await
                .map_err(map_squeezefs_err)?;
            let mut attr = self.inode_to_file_attr(&inode);
            // A metadata-only setattr (chmod/chown/utimes — no `size` in the
            // request) must never change the file size. The durable inode can
            // lag a deferred (cached-but-not-yet-committed) write, so reconcile
            // against the freshest cached size and never regress it — otherwise
            // a chmod right after a write truncates the file to the stale
            // durable size (0), zeroing reads and causing SIGBUS on mmap
            // (LTP mmap02).
            if size_to_set.is_none() {
                if let Some((cached, _)) = self.attr_cache.get(&ino) {
                    if cached.size > attr.size {
                        attr.size = cached.size;
                        attr.blocks = cached.blocks;
                    }
                }
            }
            self.attr_cache
                .insert(ino, (attr, std::time::Instant::now()));
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
        check_component_name_len(name)?;
        if link.len() > 4096 {
            return Err(Errno::from(libc::ENAMETOOLONG));
        }
        let name_str = osstr_to_cow(name);
        let link_str = osstr_to_cow(link);
        debug!(
            "FUSE symlink: parent = {}, name = {}, link = {}",
            parent, name_str, link_str
        );

        let symlink_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })
                .map_err(map_squeezefs_err)?;

            let final_mode = 0o777 | libc::S_IFLNK;
            let inode = backend
                .create(parent, &name_str, final_mode, req.uid, req.gid)
                .await
                .map_err(map_squeezefs_err)?;
            backend
                .setxattr(inode.ino, "system.symlink", link_str.as_bytes())
                .await
                .map_err(map_squeezefs_err)?;
            let mut attr = self.inode_to_file_attr(&inode);
            attr.size = link_str.len() as u64;
            attr.blocks = 1;
            self.attr_cache
                .insert(inode.ino, (attr, std::time::Instant::now()));
            self.dir_entry_cache.invalidate(&parent);
            self.attr_cache.invalidate(&parent);
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
        let backend = self
            .meta_backend
            .as_ref()
            .ok_or_else(|| {
                SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
            })
            .map_err(map_squeezefs_err)?;
        let target = backend
            .getxattr(ino, "system.symlink")
            .await
            .map_err(map_squeezefs_err)?;
        let target_bytes = target.ok_or_else(|| Errno::from(libc::ENOENT))?;
        debug!(
            "FUSE readlink: ino = {}, target = {}",
            ino,
            String::from_utf8_lossy(&target_bytes)
        );
        Ok(ReplyData {
            data: target_bytes.into(),
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
        check_component_name_len(new_name)?;
        let new_name_str = osstr_to_cow(new_name);
        debug!(
            "FUSE link: ino = {}, new_parent = {}, new_name = {}",
            ino, new_parent, new_name_str
        );

        let link_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })
                .map_err(map_squeezefs_err)?;

            let inode = backend
                .link(ino, new_parent, &new_name_str)
                .await
                .map_err(map_squeezefs_err)?;
            let attr = self.inode_to_file_attr(&inode);
            self.attr_cache
                .insert(ino, (attr, std::time::Instant::now()));
            self.attr_cache.invalidate(&new_parent);
            self.dir_entry_cache.invalidate(&new_parent);
            return Ok(ReplyEntry {
                ttl: Duration::from_secs(1),
                attr,
                generation: 1,
            });
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
        check_component_name_len(name)?;
        let name_str = osstr_to_cow(name);
        debug!("FUSE unlink: parent = {}, name = {}", parent, name_str);

        if parent == 1 && name_str == ".config" {
            return Err(Errno::from(libc::EPERM));
        }

        let unlink_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let child_ino = backend
                .unlink(parent, &name_str)
                .await
                .map_err(map_squeezefs_err)?;
            self.dir_entry_cache.invalidate(&parent);
            self.attr_cache.invalidate(&parent);
            self.attr_cache.invalidate(&child_ino);
            // Defer destroy_inode until forget/release (see rmdir comment).
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
        check_component_name_len(name)?;
        check_component_name_len(new_name)?;
        let name_str = osstr_to_cow(name);
        let new_name_str = osstr_to_cow(new_name);
        debug!(
            "FUSE rename: parent = {}, name = {}, new_parent = {}, new_name = {}",
            parent, name_str, new_parent, new_name_str
        );

        if (parent == 1 && name_str == ".config") || (new_parent == 1 && new_name_str == ".config")
        {
            return Err(Errno::from(libc::EPERM));
        }

        let rename_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");
            let dest_ino = if let Ok(inode) = backend.lookup(new_parent, &new_name_str).await {
                Some(inode.ino)
            } else {
                None
            };
            backend
                .rename(parent, &name_str, new_parent, &new_name_str, 0)
                .await
                .map_err(map_squeezefs_err)?;
            self.dir_entry_cache.invalidate(&parent);
            self.dir_entry_cache.invalidate(&new_parent);
            self.attr_cache.invalidate(&parent);
            self.attr_cache.invalidate(&new_parent);
            if let Some(d_ino) = dest_ino {
                self.attr_cache.invalidate(&d_ino);
                // Reclaim overwritten target via forget, not here.
            }
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

    async fn rename2(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        flags: u32,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);
        check_component_name_len(name)?;
        check_component_name_len(new_name)?;
        let name_str = osstr_to_cow(name);
        let new_name_str = osstr_to_cow(new_name);
        debug!(
            "FUSE rename2: parent = {}, name = {}, new_parent = {}, new_name = {}, flags = {}",
            parent, name_str, new_parent, new_name_str, flags
        );

        if (parent == 1 && name_str == ".config") || (new_parent == 1 && new_name_str == ".config")
        {
            return Err(Errno::from(libc::EPERM));
        }

        let rename_future = async {
            let backend = self
                .meta_backend
                .as_ref()
                .expect("meta_backend must be configured");

            let src_ino = if let Ok(inode) = backend.lookup(parent, &name_str).await {
                Some(inode.ino)
            } else {
                None
            };
            let dest_ino = if let Ok(inode) = backend.lookup(new_parent, &new_name_str).await {
                Some(inode.ino)
            } else {
                None
            };

            backend
                .rename(parent, &name_str, new_parent, &new_name_str, flags)
                .await
                .map_err(map_squeezefs_err)?;

            self.dir_entry_cache.invalidate(&parent);
            self.dir_entry_cache.invalidate(&new_parent);
            self.attr_cache.invalidate(&parent);
            self.attr_cache.invalidate(&new_parent);
            if let Some(s_ino) = src_ino {
                self.attr_cache.invalidate(&s_ino);
            }
            if let Some(d_ino) = dest_ino {
                self.attr_cache.invalidate(&d_ino);
                // Overwritten target reclaimed on forget only (not RENAME_EXCHANGE).
            }
            Ok(())
        };

        match tokio::time::timeout(get_fuse_timeout(), rename_future).await {
            Ok(res) => res,
            Err(_) => {
                error!(
                    "FUSE rename2 timeout parent = {}, name = {}, new_parent = {}, new_name = {}",
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
        fh: u64,
        offset: i64,
    ) -> FuseResult<ReplyDirectory<Self::DirEntryStream<'a>>> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_readdir");
        debug!(
            "FUSE readdir: parent = {}, fh = {}, offset = {}",
            parent, fh, offset
        );

        let readdir_future = async {
            let entries_map = if let Some(stream) = self.open_dir_streams.get(&fh) {
                stream.clone()
            } else if let Some(cached_map) = self.dir_entry_cache.get(&parent) {
                cached_map
            } else {
                let backend = self
                    .meta_backend
                    .as_ref()
                    .expect("meta_backend must be configured");
                let list = backend
                    .readdir(parent, 0, 100000)
                    .await
                    .map_err(map_squeezefs_err)?;
                let mut sorted_entries: Vec<(std::boxed::Box<str>, u64)> = list
                    .into_iter()
                    .map(|d| (d.name.into_boxed_str(), d.ino))
                    .collect();
                sorted_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                let map_arc: std::sync::Arc<[(std::boxed::Box<str>, u64)]> =
                    std::sync::Arc::from(sorted_entries.into_boxed_slice());
                self.dir_entry_cache.insert(parent, map_arc.clone());
                map_arc
            };

            // Convert entries_map to a list of DirectoryEntry
            let mut entries = Vec::new();

            let has_dot = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp("."))
                .is_ok();
            let has_dotdot = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp(".."))
                .is_ok();
            let has_config = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp(".config"))
                .is_ok();
            let has_stats = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp(".stats"))
                .is_ok();

            // Standard "." and ".." entries should be added if not already in Garnet
            let mut next_offset = 1;
            if !has_dot {
                if next_offset > offset {
                    entries.push(DirectoryEntry {
                        name: ".".into(),
                        kind: FileType::Directory,
                        inode: parent,
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }
            if !has_dotdot {
                if next_offset > offset {
                    // Find parent directory from root/parent key, or just default to root 1 if not exists
                    let parent_parent = if parent == 1 {
                        1
                    } else if let Some(ref backend) = self.meta_backend {
                        backend
                            .lookup(parent, "..")
                            .await
                            .map(|inode| inode.ino)
                            .unwrap_or(1)
                    } else {
                        1
                    };
                    entries.push(DirectoryEntry {
                        name: "..".into(),
                        kind: FileType::Directory,
                        inode: parent_parent,
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }

            if parent == 1 && !has_config {
                if next_offset > offset {
                    entries.push(DirectoryEntry {
                        name: ".config".into(),
                        kind: FileType::RegularFile,
                        inode: CONFIG_INODE,
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }

            if parent == 1 && !has_stats {
                if next_offset > offset {
                    entries.push(DirectoryEntry {
                        name: ".stats".into(),
                        kind: FileType::RegularFile,
                        inode: STATS_INODE,
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }

            let mut current_offset = next_offset;
            let mut child_inos = Vec::new();
            for (name, child_ino) in entries_map.iter() {
                if name.as_ref() == "." || name.as_ref() == ".." {
                    continue;
                }
                if current_offset <= offset as i64 {
                    current_offset += 1;
                    continue;
                }
                child_inos.push(*child_ino);
                current_offset += 1;
            }

            let mut kind_map = std::collections::HashMap::new();
            if !child_inos.is_empty() {
                for child_ino in &child_inos {
                    if let Some((attr, cached_at)) = self.attr_cache.get(child_ino) {
                        if cached_at.elapsed() < Duration::from_secs(1) {
                            kind_map.insert(*child_ino, attr.kind);
                            continue;
                        }
                    }
                    if let Some(ref backend) = self.meta_backend {
                        if let Ok(inode) = backend.getattr(*child_ino).await {
                            let attr = self.inode_to_file_attr(&inode);
                            kind_map.insert(*child_ino, attr.kind);
                            self.attr_cache
                                .insert(*child_ino, (attr, std::time::Instant::now()));
                        }
                    }
                }
            }

            let mut current_offset = next_offset;
            for (name, child_ino) in entries_map.iter() {
                if name.as_ref() == "." || name.as_ref() == ".." {
                    continue;
                }
                if current_offset <= offset as i64 {
                    current_offset += 1;
                    continue;
                }
                let kind = kind_map
                    .get(child_ino)
                    .cloned()
                    .unwrap_or(FileType::RegularFile);

                entries.push(DirectoryEntry {
                    name: name.as_ref().into(),
                    kind,
                    inode: *child_ino,
                    offset: current_offset,
                });
                current_offset += 1;
            }

            // Convert to BoxStream
            use futures::stream::{self, StreamExt};
            let stream = stream::iter(entries.into_iter().map(Ok)).boxed();

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
        fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> FuseResult<ReplyDirectoryPlus<Self::DirEntryPlusStream<'a>>> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!(
            "FUSE readdirplus: parent = {}, fh = {}, offset = {}",
            parent, fh, offset
        );

        let readdirplus_future = async {
            let entries_map = if let Some(stream) = self.open_dir_streams.get(&fh) {
                stream.clone()
            } else if let Some(cached_map) = self.dir_entry_cache.get(&parent) {
                cached_map
            } else {
                let backend = self
                    .meta_backend
                    .as_ref()
                    .expect("meta_backend must be configured");
                let list = backend
                    .readdir(parent, 0, 100000)
                    .await
                    .map_err(map_squeezefs_err)?;
                let mut sorted_entries: Vec<(std::boxed::Box<str>, u64)> = list
                    .into_iter()
                    .map(|d| (d.name.into_boxed_str(), d.ino))
                    .collect();
                sorted_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                let map_arc: std::sync::Arc<[(std::boxed::Box<str>, u64)]> =
                    std::sync::Arc::from(sorted_entries.into_boxed_slice());
                self.dir_entry_cache.insert(parent, map_arc.clone());
                map_arc
            };

            let mut entries = Vec::new();

            let has_dot = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp("."))
                .is_ok();
            let has_dotdot = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp(".."))
                .is_ok();
            let has_config = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp(".config"))
                .is_ok();
            let has_stats = entries_map
                .binary_search_by(|(n, _)| n.as_ref().cmp(".stats"))
                .is_ok();

            // Standard "." and ".." entries
            let mut next_offset = 1;
            if !has_dot {
                if next_offset > offset as i64 {
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
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }
            if !has_dotdot {
                if next_offset > offset as i64 {
                    let parent_parent = if parent == 1 {
                        1
                    } else if let Some(ref backend) = self.meta_backend {
                        backend
                            .lookup(parent, "..")
                            .await
                            .map(|inode| inode.ino)
                            .unwrap_or(1)
                    } else {
                        1
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
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }

            if parent == 1 && !has_config {
                if next_offset > offset as i64 {
                    let config_data = self.generate_config_json().await;
                    let attr = self.get_config_attr(config_data.len() as u64);
                    entries.push(DirectoryEntryPlus {
                        name: ".config".into(),
                        kind: FileType::RegularFile,
                        inode: CONFIG_INODE,
                        generation: 1,
                        attr,
                        entry_ttl: Duration::from_secs(1),
                        attr_ttl: Duration::from_secs(1),
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }

            if parent == 1 && !has_stats {
                if next_offset > offset as i64 {
                    let stats_data = self.generate_stats_json().await;
                    let attr = self.get_stats_attr(stats_data.len() as u64);
                    entries.push(DirectoryEntryPlus {
                        name: ".stats".into(),
                        kind: FileType::RegularFile,
                        inode: STATS_INODE,
                        generation: 1,
                        attr,
                        entry_ttl: Duration::from_secs(0),
                        attr_ttl: Duration::from_secs(0),
                        offset: next_offset,
                    });
                }
                next_offset += 1;
            }

            let mut current_offset = next_offset;
            // 1. Gather all inodes we need attributes for that AREN'T in local cache

            let mut inos_to_fetch = Vec::new();

            for (name, child_ino) in entries_map.iter() {
                if name.as_ref() == "." || name.as_ref() == ".." {
                    continue;
                }
                if current_offset <= offset as i64 {
                    current_offset += 1;
                    continue;
                }

                // Check if it's already in our local DashMap cache
                let is_cached = self
                    .attr_cache
                    .get(child_ino)
                    .map(|(_, cached_at)| cached_at.elapsed() < Duration::from_secs(1))
                    .unwrap_or(false);

                if !is_cached {
                    inos_to_fetch.push(*child_ino);
                }
                current_offset += 1;
            }

            // 2. Fetch them ALL offline!
            if !inos_to_fetch.is_empty() {
                for child_ino in inos_to_fetch {
                    if let Some(ref backend) = self.meta_backend {
                        if let Ok(inode) = backend.getattr(child_ino).await {
                            let attr = self.inode_to_file_attr(&inode);
                            self.attr_cache
                                .insert(child_ino, (attr, std::time::Instant::now()));
                        }
                    }
                }
            }

            let mut current_offset = next_offset;
            for (name, child_ino) in entries_map.iter() {
                if name.as_ref() == "." || name.as_ref() == ".." {
                    continue;
                }
                if current_offset <= offset as i64 {
                    current_offset += 1;
                    continue;
                }
                let attr = match self.get_attr_internal(*child_ino).await {
                    Ok(a) => a,
                    Err(e) => {
                        error!(
                            "readdirplus failed to get attr for child {}: {:?}",
                            child_ino, e
                        );
                        current_offset += 1;
                        continue;
                    }
                };
                let kind = attr.kind;

                entries.push(DirectoryEntryPlus {
                    name: name.as_ref().into(),
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

            use futures::stream::{self, StreamExt};
            let stream = stream::iter(entries.into_iter().map(Ok)).boxed();

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

        let same_lock = if let Some(dest_lock) = dest_lock_arc {
            std::ptr::eq(src_lock_arc, dest_lock)
        } else {
            true
        };

        let _src_read_guard;
        let _src_write_guard;
        let _dest_write_guard;
        if same_lock {
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

        let src_token = if let Some(ref sl) = src_lease {
            sl.fencing_token()
        } else if let Some(lease) = self.active_leases.get(&inode) {
            lease.fencing_token()
        } else {
            0
        };

        let dest_token = if let Some(ref dl) = dest_lease {
            dl.fencing_token()
        } else if let Some(lease) = self.active_leases.get(&inode_out) {
            lease.fencing_token()
        } else {
            0
        };

        if off_in == 0 && off_out == 0 && length >= src_size && dest_size == 0 {
            self.router
                .clone_file(&src_path, &dest_path, Some(src_token), Some(dest_token))
                .await
                .map_err(map_squeezefs_err)?;

            // Update destination attributes size and times in metadata backend
            if let Some(ref backend) = self.meta_backend {
                let _ = backend
                    .setattr(
                        inode_out,
                        None,
                        None,
                        None,
                        Some(src_size),
                        None,
                        None,
                        None,
                    )
                    .await;
            }

            self.attr_cache.invalidate(&inode_out);

            return Ok(ReplyCopyFileRange { copied: src_size });
        }

        // 3. General copy: read range from source, write to destination
        let src_data = bytes::Bytes::from(
            self.router
                .read_file(&src_path)
                .await
                .map_err(map_squeezefs_err)?,
        );
        if off_in >= src_data.len() as u64 {
            return Ok(ReplyCopyFileRange { copied: 0 });
        }

        let start = off_in as usize;
        let end = std::cmp::min((off_in + length) as usize, src_data.len());
        // SAFETY: start < src_data.len() checked on line 4278, and end is clamped to src_data.len()
        let chunk = unsafe {
            let sub = src_data.get_unchecked(start..end);
            src_data.slice_ref(sub)
        };

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
            .write_file(&dest_path, off_out, chunk.clone(), target_fencing_token)
            .await
            .map_err(map_squeezefs_err)?;

        // Update destination size and times in metadata backend
        let copied_len = chunk.len() as u64;
        let new_dest_size = std::cmp::max(dest_size, off_out + copied_len);

        if let Some(ref backend) = self.meta_backend {
            let _ = backend
                .setattr(
                    inode_out,
                    None,
                    None,
                    None,
                    Some(new_dest_size),
                    None,
                    None,
                    None,
                )
                .await;
        }

        self.attr_cache.invalidate(&inode_out);

        Ok(ReplyCopyFileRange { copied: copied_len })
    }

    async fn statfs(&self, _req: Request, _ino: u64) -> FuseResult<ReplyStatFs> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        let bsize = 4096;
        let capacity = 1024 * 1024 * 1024 * 1024 * 1024; // 1PB
        let total_inodes = 1_000_000_000;
        let total_blocks = capacity / bsize as u64;
        let bfree = total_blocks;

        Ok(ReplyStatFs {
            blocks: total_blocks,
            bfree,
            bavail: bfree,
            files: total_inodes,
            ffree: total_inodes,
            bsize,
            namelen: 255,
            frsize: bsize,
        })
    }

    async fn flush(&self, _req: Request, ino: u64, _fh: u64, _lock_owner: u64) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Flush: ino = {}", ino);

        if ino == STATS_INODE || ino == CONFIG_INODE {
            return Ok(());
        }

        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // Soft flush path: do not block FUSE flush on MetaLV layout persist or
        // full active-block promotion. sync_all/fsync is the durable barrier.
        let _ = self
            .flush_memory_buffers_for_inode(ino, fencing_token)
            .await;

        Ok(())
    }

    async fn release(
        &self,
        _req: Request,
        ino: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Release: ino = {}", ino);

        if ino == STATS_INODE || ino == CONFIG_INODE {
            self.open_virtual_files.remove(&fh);
            return Ok(());
        }

        // Non-blocking: schedule layout/active flush in background so close is
        // cheap. fsync still waits. Staging mmap retains data for same-session reads.
        if let Ok(fencing_token) = self.get_or_acquire_lease(ino).await {
            let fs = self.clone();
            crate::bg_admit::spawn_bg(async move {
                let _ = fs.flush_memory_buffers_for_inode(ino, fencing_token).await;
                let _ = fs.flush_active_blocks_with_retry(ino, fencing_token).await;
                let file_path = crate::keys::inode_path(ino);
                let _ = fs
                    .router
                    .persist_dirty_layout_if_needed(&file_path, fencing_token)
                    .await;
            });
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

        // Static lock array does not need dynamic cleanup

        self.remove_open(ino);
        self.queue_reclaim_inode(ino);

        Ok(())
    }

    async fn fsync(&self, _req: Request, ino: u64, _fh: u64, _datasync: bool) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Fsync: ino = {}, datasync = {}", ino, _datasync);

        if ino == STATS_INODE || ino == CONFIG_INODE {
            return Ok(());
        }

        // P0-3: durable ops must not mask backend write failures.
        let fencing_token = self
            .get_or_acquire_lease(ino)
            .await
            .map_err(map_squeezefs_err)?;

        // Single path: memory → active flush → dirty layout, then ONE meta barrier.
        // `flush_inode_to_backend` already issues `sync_device_for_ino` for this
        // inode's volume; a second barrier here flushed nothing new (redundant
        // fdatasync = ~2x small-file fsync latency), so it is intentionally gone.
        // (The former FORCE_SYNC_TX scope here was verified inert end-to-end and
        // deleted in design-wal-crash-consistency PR 4 §4.3: no metadata
        // transaction ever executed inside this scope — the layout/size persist
        // is deliberately non-transactional — so its single reader never
        // observed `true`. Zero fsync behavior change; the single-barrier
        // suites are the regression guard.)
        if let Err(e) = self.flush_inode_to_backend(ino, fencing_token).await {
            error!("FUSE Fsync failed for ino {}: {:?}", ino, e);
            WRITEBACK_HARD_FAILURES.insert(ino, format!("{e:?}"));
            return Err(map_squeezefs_err(e));
        }

        if let Some(err_msg) = WRITEBACK_HARD_FAILURES.get(&ino) {
            error!(
                "FUSE Fsync: prior writeback hard failure for ino {}: {}",
                ino,
                err_msg.value()
            );
            return Err(Errno::from(libc::EIO));
        }

        Ok(())
    }

    async fn fsyncdir(&self, _req: Request, ino: u64, _fh: u64, _datasync: bool) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Fsyncdir: ino = {}", ino);
        if let Some(backend) = self.meta_backend.as_ref() {
            backend
                .sync_all_devices()
                .await
                .map_err(map_squeezefs_err)?;
        }
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

        // Pre-allocation isn't strictly required to reserve physical space in our NVMe-oF backend volume
        // as blocks are sparse/dynamic by nature. We just update the size attribute if we are extending.
        if mode & libc::FALLOC_FL_KEEP_SIZE as u32 == 0 {
            let backend = self
                .meta_backend
                .as_ref()
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
                })
                .map_err(map_squeezefs_err)?;

            let disk_inode = backend.getattr(ino).await.map_err(map_squeezefs_err)?;
            let old_size = disk_inode.size;

            let target_size = offset + length;
            if target_size > old_size {
                // Update in backend
                backend
                    .setattr(ino, None, None, None, Some(target_size), None, None, None)
                    .await
                    .map_err(map_squeezefs_err)?;

                // Update layout size. Striped files go through the §5.3
                // degenerate size-only merge: the old whole-meta save of a
                // stale snapshot under NO lock could rewrite the block map
                // "without mutating it", dropping mappings a concurrent
                // write-through just published. Inline/staged files keep
                // the whole-meta save — their RAM meta (dirty inline
                // payload / staged identity) is the truth a backend re-read
                // cannot carry, and they have no striped map to lose.
                let file_path = crate::keys::inode_path(ino);
                if let Ok(mut meta) = self.router.fetch_metadata(&file_path).await {
                    let fencing_token = self.dlm.get_fencing_token_ino(ino);
                    if meta.file_type == "striped" {
                        let _ = self
                            .router
                            .merge_block_mappings(
                                ino,
                                crate::routing::BlockMapOp::Merge(&[]),
                                target_size,
                                crate::routing::LayoutFlip::KeepLayout,
                                fencing_token,
                            )
                            .await;
                    } else {
                        meta.size = target_size;
                        let _ = self
                            .router
                            .save_metadata_to_backend(ino, &meta, fencing_token)
                            .await;
                    }
                }

                // Update cache
                if let Some((mut attr, _)) = self.attr_cache.get(&ino) {
                    attr.size = target_size;
                    attr.blocks = target_size.div_ceil(512);
                    self.attr_cache
                        .insert(ino, (attr, std::time::Instant::now()));
                }
            }
        }

        Ok(())
    }

    async fn forget(&self, _req: Request, ino: u64, count: u64) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Forget: ino = {}, count = {}", ino, count);
        self.attr_cache.invalidate(&ino);
        self.active_inode_locks.remove(&ino);
        // Reclaim inodes that reached nlink==0 while still open (unlink/14.t).
        self.queue_reclaim_inode(ino);
    }

    /// BATCH_FORGET (kernel mass evictions: memory pressure, drop_caches,
    /// pre-umount sweeps) must behave exactly like N FORGETs. fuse3's
    /// default impl is a NO-OP — leaving this unimplemented leaked every
    /// batch-evicted orphan's inode slot until the next mount's
    /// reconciliation.
    async fn batch_forget(&self, _req: Request, inodes: &[u64]) {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE BatchForget: {} inodes", inodes.len());
        for &ino in inodes {
            self.attr_cache.invalidate(&ino);
            self.active_inode_locks.remove(&ino);
            self.queue_reclaim_inode(ino);
        }
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
        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        backend
            .setxattr(inode, name_str, value)
            .await
            .map_err(map_squeezefs_err)?;
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

        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        let value = backend
            .getxattr(inode, name_str)
            .await
            .map_err(map_squeezefs_err)?;
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
        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        let keys = backend.listxattr(inode).await.map_err(map_squeezefs_err)?;
        let mut data = Vec::new();
        for key in keys {
            data.extend_from_slice(key.as_bytes());
            data.push(0);
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
        let backend = self
            .meta_backend
            .as_ref()
            .expect("meta_backend must be configured");
        backend
            .removexattr(inode, name_str)
            .await
            .map_err(map_squeezefs_err)?;
        Ok(())
    }
}

/// Initialize the multi-threaded work-stealing tokio runtime
/// with threads pinned strictly to physical cores, keeping one core free.
pub fn init_runtime() -> tokio::runtime::Runtime {
    let physical_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Leave at least one core for kernel processing (FUSE filesystem driver, Garnet, networking)
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

/// Historical debug helper — **not** used by production mounts.
///
/// Production FUSE transport is **fuse3** `BlockFuseConnection`: separate read/write
/// `IoUring` rings, optional SQPOLL (`SQUEEZEFS_FUSE_IO_URING_SQPOLL_*`), eventfd
/// completion wakeups, multi-queue `FUSE_DEV_IOC_CLONE` workers, and (P2-8) fixed-file
/// registration of `/dev/fuse` as `types::Fixed(0)` when the kernel allows.
///
/// Prefer that path over this loop, which only demonstrates a bare `Read` on an fd
/// and does not decode FUSE messages.
#[cfg(target_os = "linux")]
#[deprecated(
    note = "Production mounts use fuse3 BlockFuseConnection io_uring; this loop is a no-op debug stub"
)]
pub fn start_io_uring_polling_loop(
    _fuse_fd: std::os::fd::RawFd,
    _runtime: &tokio::runtime::Runtime,
) {
    info!(
        "FUSE Daemon: start_io_uring_polling_loop is deprecated; production I/O uses fuse3 uring rings"
    );
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

pub fn filter_kernel_mount_options(opts: &str) -> String {
    let mut kernel_opts = Vec::new();
    for opt in opts.split(',') {
        let opt_trimmed = opt.trim();
        if !opt_trimmed.is_empty() {
            let key = opt_trimmed.split('=').next().unwrap_or("").trim();
            if key == "max_read"
                || key == "blksize"
                || key == "default_permissions"
                || key == "allow_other"
            {
                kernel_opts.push(opt_trimmed);
            }
        }
    }
    kernel_opts.join(",")
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
    let is_root = unsafe { libc::getuid() } == 0;
    if is_root {
        options.uid(uid);
        options.gid(gid);
    }
    options.allow_other(allow_other);
    options.write_back(writeback);
    options.default_permissions(true);

    if let Some(ref opts) = custom_opts {
        for opt in opts.split(',') {
            let opt_trimmed = opt.trim();
            if !opt_trimmed.is_empty() {
                let parts: Vec<&str> = opt_trimmed.splitn(2, '=').collect();
                if parts.len() == 2 {
                    let key = parts[0].trim();
                    let val = parts[1].trim();
                    if key == "fsname" {
                        options.fs_name(val);
                    }
                }
            }
        }
    }

    if is_root {
        let filtered_opts = if let Some(ref opts) = custom_opts {
            filter_kernel_mount_options(opts)
        } else {
            "max_read=1048576".to_string()
        };
        options.custom_options(filtered_opts);
    } else {
        if let Some(opts) = custom_opts {
            let parsed = parse_custom_options(&opts);
            options.custom_options(parsed);
        } else {
            // default custom option
            options.custom_options("max_read=1048576,max_write=1048576,max_pages=256,max_readahead=4194304,max_background=64,congestion_threshold=48,async_read");
        }
    }

    info!(
        "SqueezeFS version {} initializing mount",
        env!("CARGO_PKG_VERSION")
    );
    info!(
        "FUSE Daemon: Mounting squeezefs at {:?}...",
        mountpoint.as_ref()
    );
    info!("FUSE Daemon: Metadata connection active.");

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
                "Stale mount point detected at {:?}.\nTo resolve this, run:\n    sudo squeezefs umount {:?}\n    # or: sudo umount -f {:?}\n    # last resort: sudo umount -l {:?}",
                mount_path, mount_path, mount_path, mount_path
            );
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Transport endpoint is not connected",
            )));
        }
    }

    let _dismount_wait = fs.dismount_wait;
    let nvme_cache = fs.router.cache.nvme.clone();

    let client_id_str = uuid::Uuid::new_v4().to_string();
    *fs.client_id.lock().unwrap() = client_id_str.clone();
    *fs.mountpoint.lock().unwrap() = mount_path.to_string_lossy().to_string();

    // Heartbeat: periodically refresh this client's mount registration so peers
    // can distinguish a live mount from a crashed one. If this process dies
    // ungracefully (kill -9), the heartbeat stops and the registration goes stale
    // after CLIENT_STALE_TTL_SECS, so it no longer blocks `format`.
    let heartbeat_handle = {
        let fs = fs.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                CLIENT_HEARTBEAT_INTERVAL_SECS,
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                fs.refresh_client_registration().await;
            }
        })
    };

    // Spawns the mount loop using fuse3 Session
    let session = fuse3::raw::Session::new(options);

    #[cfg(target_os = "linux")]
    let mut handle = if unsafe { libc::getuid() } == 0 {
        session.mount(fs.clone(), mount_path.clone()).await?
    } else {
        session
            .mount_with_unprivileged(fs.clone(), mount_path.clone())
            .await?
    };

    #[cfg(not(target_os = "linux"))]
    let mut handle = session.mount(fs.clone(), mount_path.clone()).await?;

    if let Some(conn) = handle.connection() {
        fs.session_connection.store(std::sync::Arc::new(Some(conn)));
    }

    println!("\x1b[92mOK\x1b[0m Squeezefs is ready at {:?}", mount_path);

    let mut should_exit = false;
    while !should_exit {
        let shutdown = async {
            #[cfg(unix)]
            {
                println!("[SIG] Registering signal handlers inside shutdown block...");
                let sigterm_opt =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
                let sigint_opt =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
                if let (Ok(mut sigterm), Ok(mut sigint)) = (sigterm_opt, sigint_opt) {
                    println!("[SIG] Signal handlers registered successfully.");
                    loop {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {
                                println!("[SIG] Ctrl+C pressed (ctrl_c future matched)!");
                                eprintln!("\nWARNING: Ctrl+C pressed! If you really want to unmount/exit, hit Ctrl+C again.");
                                tokio::select! {
                                    _ = tokio::signal::ctrl_c() => {
                                        println!("[SIG] Second Ctrl+C, exiting...");
                                        break;
                                    }
                                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                                        eprintln!("\nUnmount timeout elapsed. Resuming filesystem...");
                                    }
                                }
                            }
                            _ = sigterm.recv() => {
                                println!("[SIG] Received SIGTERM signal in sigterm.recv()!");
                                break;
                            }
                            _ = sigint.recv() => {
                                println!("[SIG] Received SIGINT signal in sigint.recv()!");
                                eprintln!("\nWARNING: SIGINT received! If you really want to unmount/exit, send SIGINT again.");
                                tokio::select! {
                                    _ = sigint.recv() => {
                                        println!("[SIG] Second SIGINT, exiting...");
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
                    println!("[SIG] Failed to register unix signal handlers. Falling back...");
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

                println!("[SIG] Shutdown future resolved. Running shutdown logic...");
                info!("Received shutdown signal. Force flushing memory buffers to staging...");
                // Phase 1: Force flush RAM buffers to staging (cancellation NOT allowed)
                if let Err(e) = fs.flush_all_memory_buffers_to_staging().await {
                    error!("Error flushing memory buffers to staging: {:?}", e);
                }

                // Check staging status
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
                if has_unflushed {
                    if std::io::stdin().is_terminal() {
                        println!("\n{}", "WARNING: There are unflushed staged writes on this node!".red().bold());
                        println!("Remaining local staged files: {}", staged_count);
                        println!("Active write transaction directories: {}", active_writes_count);
                        println!("If you unmount now, other nodes will not see this data.");
                        println!("\nChoose an option:");
                        println!("  [w] Wait for staged files to drain/flush to NVMe-oF backend");
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
                            tokio::select! {
                                summary = fs.flush_all_staged_blocks_to_backend() => {
                                    if summary.failed > 0 {
                                        error!(
                                            "Staged-block drain: {} flushed, {} failed (first errors: {:?})",
                                            summary.flushed, summary.failed, summary.error_samples
                                        );
                                    } else {
                                        println!("\nAll staged files and active writes drained cleanly!");
                                    }
                                }
                                _ = tokio::signal::ctrl_c() => {
                                    println!("\nCtrl+C received. Cancelling staging flush to NVMe-oF backend and exiting immediately...");
                                }
                            }
                        } else {
                            println!("Continuing with unmount.");
                        }
                    } else {
                        // Non-interactive/daemon mode: flush all staged files automatically before exiting
                        info!("FUSE Daemon: Non-interactive shutdown. Automatically flushing staged files to backend...");
                        let _ = fs.flush_all_staged_blocks_to_backend().await;
                    }
                }
                should_exit = true;
            }
        }
    }

    // Stop heartbeat task
    heartbeat_handle.abort();

    // Clean up the mount by unmounting the session if it hasn't been done already.
    if let Err(e) = handle.unmount().await {
        debug!("Unmount on exit status (may already be unmounted): {:?}", e);
    } else {
        info!("Cleanly unmounted filesystem on exit.");
    }

    Ok(())
}

pub async fn get_volume_status(meta_lv_path: &str) -> Result<serde_json::Value, SqueezefsError> {
    let storage = crate::meta_backend::storage::MetaLvStorage::open(meta_lv_path, 0)?;
    let backend = crate::meta_backend::MetaLvBackend::new(storage);
    let val_opt = backend.getxattr(1, "user.squeezefs.format_config").await?;
    let val = val_opt.ok_or_else(|| {
        SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Volume not formatted",
        ))
    })?;
    let config: crate::FormatConfig = serde_json::from_slice(&val).map_err(|e| {
        SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid format config: {:?}", e),
        ))
    })?;

    let mut storage_backends = serde_json::Map::new();
    if let Some(ref datalvs) = config.data_lv {
        for dl in datalvs {
            storage_backends.insert(
                dl.clone(),
                serde_json::json!({
                    "backing_dev": dl.clone(),
                    "status": "enabled",
                }),
            );
        }
    }

    Ok(serde_json::json!({
        "Setting": {
            "Name": config.name,
            "BlockSize": config.block_size,
            "Capacity": config.capacity,
            "Inodes": config.inodes,
            "Compression": config.compression,
            "EncryptAlgo": config.encrypt_algo,
            "MemCacheSize": config.mem_cache_size.unwrap_or_default(),
            "DiskCacheSize": config.disk_cache_size.unwrap_or_default(),
            "DiskCachePaths": config.disk_cache_paths.unwrap_or_default(),
            "StorageBackends": storage_backends,
            "ActiveWriteBackend": "",
        },
        "Clients": []
    }))
}

async fn run_constant_writeback_worker(
    mut rx: tokio::sync::mpsc::Receiver<WritebackRequest>,
    requeue_tx: tokio::sync::mpsc::Sender<WritebackRequest>,
    router: DataRouter,
    dlm: DlmClient,
    active_inode_locks: std::sync::Arc<StripeLocks<tokio::sync::RwLock<()>, 4096>>,
    max_uploads: usize,
) {
    let upload_semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_uploads));

    while let Some(req) = rx.recv().await {
        log::info!(
            "Constant Writeback: Received writeback request for ino {}, block {} (attempt {})",
            req.ino,
            req.block_idx,
            req.attempts
        );
        let router_clone = router.clone();
        let dlm_clone = dlm.clone();
        let locks_clone = active_inode_locks.clone();
        let sem_clone = upload_semaphore.clone();
        let requeue_tx = requeue_tx.clone();

        tokio::spawn(async move {
            let _permit = match sem_clone.acquire().await {
                Ok(p) => p,
                Err(e) => {
                    log::error!("Constant Writeback: Semaphore acquire failed: {:?}", e);
                    return;
                }
            };

            let file_path = format!("inode_{}", req.ino);
            let meta = match router_clone.fetch_metadata(&file_path).await {
                Ok(m) => m,
                Err(e) => {
                    log::error!(
                        "Constant Writeback: Failed to fetch metadata for {}: {:?}",
                        req.ino,
                        e
                    );
                    requeue_or_hard_fail(&requeue_tx, req, format!("fetch: {e:?}")).await;
                    return;
                }
            };

            let is_striped = meta.file_type == "striped";

            match flush_single_active_block(
                req.ino,
                req.block_idx,
                req.fencing_token,
                &router_clone,
                &dlm_clone,
                &locks_clone,
                is_striped,
                false,
            )
            .await
            {
                Ok(()) => {
                    WRITEBACK_HARD_FAILURES.remove(&req.ino);
                }
                Err(e) => {
                    log::error!(
                        "Constant Writeback: Failed to flush block {} of inode {} (attempt {}): {:?}",
                        req.block_idx,
                        req.ino,
                        req.attempts,
                        e
                    );
                    requeue_or_hard_fail(&requeue_tx, req, format!("{e:?}")).await;
                }
            }
        });
    }
}

async fn requeue_or_hard_fail(
    requeue_tx: &tokio::sync::mpsc::Sender<WritebackRequest>,
    mut req: WritebackRequest,
    err_msg: String,
) {
    if req.attempts + 1 < WRITEBACK_MAX_ATTEMPTS {
        req.attempts += 1;
        let backoff_ms = 50u64.saturating_mul(1u64 << req.attempts.min(6));
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        // Bounded queue: try_send; if full, sticky-fail rather than blocking the worker forever.
        if let Err(e) = requeue_tx.try_send(req) {
            error!("Constant Writeback: requeue failed ({e}); marking hard failure: {err_msg}");
            // req moved into try_send Err variants
            match e {
                tokio::sync::mpsc::error::TrySendError::Full(r)
                | tokio::sync::mpsc::error::TrySendError::Closed(r) => {
                    WRITEBACK_HARD_FAILURES.insert(r.ino, err_msg);
                }
            }
        }
    } else {
        error!(
            "Constant Writeback: exhausted retries for ino {} block {}; sticky hard failure: {}",
            req.ino, req.block_idx, err_msg
        );
        WRITEBACK_HARD_FAILURES.insert(req.ino, err_msg);
        METRICS
            .writeback_hard_failures
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Upload one staged active block via the §5.5 write-only guard-backed DMA
/// source (zero staging copy). Resolves carrying keys/sizes only — the
/// staging guard is provably dead inside [`cache::nvme::write_block_from_staging`]
/// before this future completes, so the batch stage never carries guards in
/// its `results`.
async fn upload_single_active_block_data(
    ino: u64,
    b: u32,
    router: &DataRouter,
    cache_promotion_copy: bool,
) -> Result<(u32, u64), SqueezefsError> {
    let cache_key = crate::keys::active_block(ino, b as u64).to_string();

    let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
    let start_block_lock = std::time::Instant::now();
    let block_guard = block_lock.lock().await;
    METRICS.block_lock_wait.record(start_block_lock.elapsed());

    let block_data_source = match router.cache.nvme.staged_dma_source(&cache_key) {
        Some(s) => s,
        None => {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "active block buffer not found",
            )))
        }
    };
    // §5.5: guard-backed bytes never enter any cache — the promotion LRU
    // seed (not-yet-striped files only, cold path) is a bounded real copy
    // taken while the source is alive.
    let lru_copy = cache_promotion_copy.then(|| block_data_source.detached_copy());
    drop(block_guard);

    let (_be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
    let offset = block_allocator.allocate_block().await?;

    // Consumes the source (crypto transform severs the guard pre-DMA;
    // passthrough DMAs straight off the staging mmap and drops the guard on
    // completion — normative §5.5 sequencing).
    if let Err(e) = crate::cache::nvme::write_block_from_staging(
        router.get_crypto(),
        &nvme_writer,
        offset,
        block_data_source,
    )
    .await
    {
        error!(
            "upload_single_active_block_data: Failed to upload block {} of inode {} to NVMe: {:?}",
            b, ino, e
        );
        let _ = block_allocator.free_block(offset).await;
        return Err(e);
    }
    block_allocator.publish_block(offset);

    // Post-DMA, post-publish, pre-merge — the same put point as the routing
    // striped path (`routing.rs` per-block task): the key stays unreferenced
    // until the block-map merge below publishes it.
    match lru_copy {
        Some(copy) => router.cache.read_lru.put(&offset.to_string(), copy),
        None => {
            // No-put owner of a possibly-reused key: purge instead (same
            // dead-incarnation shielding as `upload_full_block`, PR 6).
            let new_key = offset.to_string();
            router.cache.read_lru.remove(&new_key);
            router.cache.nvme.remove_cached_read_block(&new_key);
        }
    }

    Ok((b, offset))
}

async fn flush_due_active_blocks_for_inode(
    ino: u64,
    block_indices: Vec<u32>,
    fencing_token: u64,
    router: &DataRouter,
    _dlm: &DlmClient,
    active_inode_locks: &StripeLocks<tokio::sync::RwLock<()>, 4096>,
) -> Result<(), SqueezefsError> {
    use futures::stream::{self, StreamExt};

    let file_path = crate::keys::inode_path(ino);
    let meta = router.fetch_metadata(&file_path).await?;

    let is_striped = meta.file_type == "striped";
    let router_clone = router.clone();
    // Promotion LRU seeding happens inside the per-block future with a
    // detached copy (§5.5): the batch results carry keys/sizes only, never
    // guard-backed bytes.
    let cache_promotion_copy = !is_striped;

    let mut flushes = stream::iter(block_indices.into_iter().map(move |block_idx| {
        let router = router_clone.clone();
        async move {
            upload_single_active_block_data(ino, block_idx, &router, cache_promotion_copy).await
        }
    }))
    .buffer_unordered(8);

    let mut results = Vec::new();
    while let Some(res) = flushes.next().await {
        match res {
            Ok(r) => results.push(r),
            Err(SqueezefsError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }

    if results.is_empty() {
        return Ok(());
    }

    // fsync-vs-write serialization (lock order position 1, left as-is); the
    // map RMW itself goes through the shared merge primitive below.
    let write_lock = active_inode_locks.get_inode_lock(ino);
    let _write_guard = write_lock.write().await;

    // One merge for the whole batch via the §5.3 primitive: current-map
    // RMW under INODE_META_LOCKS, freeing only the displaced-from-current
    // keys it returns (the old start-of-call snapshot frees could free a
    // block a concurrent writer just published).
    let entries: Vec<(u32, String)> = results
        .iter()
        .map(|&(b, offset)| (b, offset.to_string()))
        .collect();
    let displaced = router
        .merge_block_mappings(
            ino,
            crate::routing::BlockMapOp::Merge(&entries),
            0,
            crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
            fencing_token,
        )
        .await?;
    for bk in displaced {
        let _ = router.backend_router.free_block(&bk).await;
    }

    for &(b, _) in &results {
        let cache_key = crate::keys::active_block(ino, b as u64).to_string();
        let current_token = router.cache.nvme.get_staged_fencing_token(&cache_key);
        if let Some(tok) = current_token {
            if tok == fencing_token {
                router.cache.nvme.remove_active_block(&cache_key);
            }
        } else {
            router.cache.nvme.remove_active_block(&cache_key);
        }
    }

    Ok(())
}

/// Durable escalation when the staging segment refuses an active block
/// (never-lossy backpressure): upload the RAM copy straight to a backend
/// block and commit it into the inode's block map. Loud and slower than
/// staging, but the data is durable the moment this returns.
///
/// The map RMW goes through the §5.3 merge primitive (INODE_META_LOCKS) —
/// the old `active_inode_locks` write-guard merge was the second
/// serialization discipline whose coexistence with write-through would
/// have opened a lost-update window exactly under the staging-refusal
/// backpressure regime in which both fire concurrently. Callers may hold
/// the victim's `BLOCK_FLUSH_LOCKS` (P1-9 extended order: block locks →
/// INODE_META_LOCKS).
async fn upload_active_block_bytes(
    ino: u64,
    b: u32,
    block_bytes: bytes::Bytes,
    fencing_token: u64,
    router: &DataRouter,
) -> Result<(), SqueezefsError> {
    let processed_block = router
        .get_crypto()
        .process_write_async(block_bytes.clone())
        .await?;
    let (_be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
    let offset = block_allocator.allocate_block().await?;
    if let Err(e) = nvme_writer.write_block(offset, processed_block).await {
        let _ = block_allocator.free_block(offset).await;
        return Err(e);
    }
    block_allocator.publish_block(offset);
    let stored_block_key = offset.to_string();

    let entries = [(b, stored_block_key)];
    let displaced = router
        .merge_block_mappings(
            ino,
            crate::routing::BlockMapOp::Merge(&entries),
            0,
            crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
            fencing_token,
        )
        .await?;
    for bk in displaced {
        let _ = router.backend_router.free_block(&bk).await;
    }
    Ok(())
}

async fn flush_single_active_block(
    ino: u64,
    b: u32,
    fencing_token: u64,
    router: &DataRouter,
    _dlm: &DlmClient,
    active_inode_locks: &StripeLocks<tokio::sync::RwLock<()>, 4096>,
    is_striped: bool,
    locked: bool,
) -> Result<(), SqueezefsError> {
    let cache_key = crate::keys::active_block(ino, b as u64).to_string();

    let lock_opt = if !locked {
        Some(active_inode_locks.get_inode_lock(ino))
    } else {
        None
    };

    let mut _inode_guard = None;
    if let Some(ref l) = lock_opt {
        _inode_guard = Some(l.read().await);
    }

    let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
    let start_block_lock = std::time::Instant::now();
    let block_guard = block_lock.lock().await;
    METRICS.block_lock_wait.record(start_block_lock.elapsed());

    let block_data_source = match router.cache.nvme.staged_dma_source(&cache_key) {
        Some(s) => s,
        None => return Ok(()),
    };

    // §5.5: guard-backed bytes never enter any cache — the promotion LRU
    // seed (not-yet-striped files only, cold path) is a bounded real copy
    // taken while the source is alive; striped flushes never put.
    let lru_copy = (!is_striped).then(|| block_data_source.detached_copy());

    let (_be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
    let offset = block_allocator.allocate_block().await?;

    // Consumes the source (crypto transform severs the guard pre-DMA;
    // passthrough DMAs straight off the staging mmap). The guard is provably
    // dead when this returns — the meta merge and `remove_active_block`
    // below take same-shard write locks (normative §5.5 sequencing).
    if let Err(e) = crate::cache::nvme::write_block_from_staging(
        router.get_crypto(),
        &nvme_writer,
        offset,
        block_data_source,
    )
    .await
    {
        error!(
            "flush_single_active_block: Failed to upload block {} of inode {} to NVMe: {:?}",
            b, ino, e
        );
        let _ = block_allocator.free_block(offset).await;
        return Err(e);
    }
    block_allocator.publish_block(offset);

    let stored_block_key = offset.to_string();

    std::mem::drop(block_guard);
    std::mem::drop(_inode_guard);

    // fsync-vs-write serialization (lock order position 1, left as-is); the
    // map RMW itself goes through the shared merge primitive below.
    let write_lock = active_inode_locks.get_inode_lock(ino);
    let mut _write_guard = None;
    if !locked {
        _write_guard = Some(write_lock.write().await);
    }

    // §5.3 one merge discipline: current-map RMW under INODE_META_LOCKS.
    // Free only the displaced-from-current keys the primitive returns —
    // the old start-of-call `old_block_key` free was exactly the
    // stale-snapshot anti-pattern the routing merge comment forbids
    // (freeing it could free a block a concurrent writer just published).
    let entries = [(b, stored_block_key.clone())];
    let displaced = router
        .merge_block_mappings(
            ino,
            crate::routing::BlockMapOp::Merge(&entries),
            0,
            crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
            fencing_token,
        )
        .await?;

    // Cache in RAM (bypass entirely if file is striped layout). Promotion
    // puts a detached copy — never the guard-backed staging bytes (§5.5).
    match lru_copy {
        Some(copy) => router.cache.read_lru.put(&stored_block_key, copy),
        None => {
            // No-put owner of a possibly-reused key: purge instead (same
            // dead-incarnation shielding as `upload_full_block`, PR 6).
            router.cache.read_lru.remove(&stored_block_key);
            router
                .cache
                .nvme
                .remove_cached_read_block(&stored_block_key);
        }
    }

    for bk in displaced {
        let _ = router.backend_router.free_block(&bk).await;
    }

    // ONLY remove active write block from cache if it hasn't been modified by a newer write
    let current_token = router.cache.nvme.get_staged_fencing_token(&cache_key);
    if let Some(tok) = current_token {
        if tok == fencing_token {
            router.cache.nvme.remove_active_block(&cache_key);
        }
    } else {
        router.cache.nvme.remove_active_block(&cache_key);
    }

    Ok(())
}

/// Drain one reclaim batch: after the first ino arrives, hold a short
/// gather window so a FORGET storm coalesces into a REAL batch. Without the
/// window, a 1:1 unlink→FORGET storm outruns `try_recv` and every "batch"
/// degenerates to 1–2 inos — reclaim then interferes op-for-op with the
/// foreground delete path (measured: ~360 µs/unlink vs ~180 µs with reclaim
/// quiescent). Reclaim is background by definition; +`window` of slot-reuse
/// latency is free, and the batch's meta work amortizes ~cap× (§4.5).
/// Returns `None` when the channel closed with nothing pending.
async fn drain_reclaim_batch(
    rx: &mut tokio::sync::mpsc::Receiver<u64>,
    cap: usize,
    window: std::time::Duration,
) -> Option<Vec<u64>> {
    let first = rx.recv().await?;
    let mut batch = vec![first];
    if !window.is_zero() {
        let deadline = tokio::time::Instant::now() + window;
        while batch.len() < cap {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ino)) => batch.push(ino),
                Ok(None) | Err(_) => break, // closed or window elapsed
            }
        }
    }
    while batch.len() < cap {
        match rx.try_recv() {
            Ok(ino) => batch.push(ino),
            Err(_) => break,
        }
    }
    // FORGET can enqueue an ino more than once across sessions.
    batch.sort_unstable();
    batch.dedup();
    Some(batch)
}

async fn run_reclaim_worker_pool(
    mut rx: tokio::sync::mpsc::Receiver<u64>,
    fs: SqueezefsFilesystem,
    concurrency: usize,
) {
    // Group-commit batching (design §4.5): drain up to SQUEEZEFS_RECLAIM_BATCH
    // inos per unit of work — sequential allocation clusters doomed inos in
    // the same inode-table sectors, so a batch's slot zeroes merge into
    // shared sector images and one apply write.
    let batch_cap = std::env::var("SQUEEZEFS_RECLAIM_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, 1024))
        .unwrap_or(64);
    let window_ms = std::env::var("SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.min(1000))
        .unwrap_or(20);
    let window = std::time::Duration::from_millis(window_ms);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let fs_arc = std::sync::Arc::new(fs);
    while let Some(batch) = drain_reclaim_batch(&mut rx, batch_cap, window).await {
        let fs_clone = fs_arc.clone();
        let sem_clone = semaphore.clone();
        let permit = sem_clone.acquire_owned().await.unwrap();
        tokio::spawn(async move {
            let _permit = permit;
            fs_clone.reclaim_orphaned_batch(batch).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn component_name_len_accepts_name_max() {
        let name = "a".repeat(FUSE_NAME_MAX);
        assert!(check_component_name_len(OsStr::new(&name)).is_ok());
    }

    #[test]
    fn component_name_len_rejects_over_name_max() {
        let name = "a".repeat(FUSE_NAME_MAX + 1);
        let err = check_component_name_len(OsStr::new(&name)).unwrap_err();
        // fuse3::Errno stores the negated libc errno.
        assert_eq!(i32::from(err).unsigned_abs(), libc::ENAMETOOLONG as u32);
    }

    #[test]
    fn component_name_len_accepts_empty_and_short() {
        assert!(check_component_name_len(OsStr::new("")).is_ok());
        assert!(check_component_name_len(OsStr::new("x")).is_ok());
        assert!(check_component_name_len(OsStr::new(&"b".repeat(255))).is_ok());
    }
}
