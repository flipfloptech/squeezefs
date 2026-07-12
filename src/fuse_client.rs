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

/// §4.5 dir-entry-cache policy (PR K7): only directories with at most
/// this many entries are snapshotted into `dir_entry_cache_v3` — an
/// `Arc<[…]>` of a 1 M-entry listing is ~60 MB, and moka's capacity
/// accounting here counts *directories*, not entries. Larger directories
/// stream through `readdir(dir, offset, max)` instead.
pub const DIR_ENTRY_CACHE_MAX_ENTRIES: usize = 10_000;

/// Readdir cookie of the root's virtual `.config` entry on v3 volumes
/// (design §5.1, PR K7): real-entry cookies occupy
/// `[3, 3 + ((2^54−1)·2^8 + 255)] = [3, 2^62 + 2]`, so the virtual entries
/// ride strictly above the whole real space — still sign-bit-clear for
/// the `i64` FUSE surface. v2 volumes keep positional offsets (virtuals
/// first), untouched.
pub const READDIR_VIRTUAL_CONFIG_COOKIE: u64 = (1 << 62) + 3;

/// Readdir cookie of the root's virtual `.stats` entry on v3 volumes
/// (see [`READDIR_VIRTUAL_CONFIG_COOKIE`]).
pub const READDIR_VIRTUAL_STATS_COOKIE: u64 = (1 << 62) + 4;

/// PR K7: real entries fetched per FUSE `readdir` call on a v3 volume.
/// A kernel dirent buffer holds ~1–2 K entries, so one page bounds the
/// over-fetch waste while big directories stream page by page.
const V3_READDIR_PAGE: usize = 4096;

/// `readdirplus` pages smaller: every entry carries a full attribute
/// fetch (attr-cache-backed inode-tree point lookups).
const V3_READDIRPLUS_PAGE: usize = 1024;

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
    /// Striped serves whose block-index→key binding (or device-fill
    /// incarnation) moved between resolution and fetch and were re-resolved
    /// instead of served (the reused-key stale-fill family, `8e3995e`
    /// follow-up). Each increment is an averted wrong-block serve — the live
    /// detector for the free→reallocate ABA window.
    pub stale_binding_rebinds: Align64<AtomicU64>,
    /// Single-flight waiters served directly from their cohort's carried
    /// `FillResult` (R1a, docs/design-read-path.md §5.2) — the adoption
    /// signal that waiter correctness is publish-independent. Replaces the
    /// old probe-after-publish tier-recheck hits for cohort members.
    pub singleflight_waiter_result_serves: Align64<AtomicU64>,
    /// R4 hot-block RAM tier (docs/design-read-path.md §5.4): hits are the
    /// warm-read adoption signal for the > 256 KiB block population;
    /// misses count device-validated fills entering probation; evictions
    /// count victims leaving the clock ring; probation_drops counts
    /// never-read probation victims DROPPED by the dehydration gate —
    /// stays 0 until PR 4 flips the protected-only policy.
    pub hot_block_hits: Align64<AtomicU64>,
    pub hot_block_misses: Align64<AtomicU64>,
    pub hot_block_evictions: Align64<AtomicU64>,
    pub hot_block_probation_drops: Align64<AtomicU64>,
    /// Protected hot-tier victims dropped at the dehydration channel mouth
    /// because their bytes were already NVMe-tier-resident (duplicate-write
    /// dedupe, §5.3/§5.5).
    pub hot_block_dehydrate_skips: Align64<AtomicU64>,
    /// R3 ranged reads (docs/design-read-path.md §5.6): adoption counter —
    /// sub-block device reads served by `get_block_range_for_index`.
    pub ranged_reads: Align64<AtomicU64>,
    /// R3 amplification numerator: DEVICE bytes read by ranged ops (window
    /// bytes, ≥ the requested bytes only by 4 KiB rounding). Compare
    /// against user bytes for the random-row amplification bound.
    pub ranged_read_bytes: Align64<AtomicU64>,
    /// Ranged requests whose 4 KiB window rounding widened the request
    /// (unaligned edges) — ≫ 0 on O_DIRECT means the LBA assumption is
    /// wrong for the workload (§5.6 open-question follow-up trigger).
    pub ranged_read_unaligned_bounces: Align64<AtomicU64>,
    /// Ranged serves that hit binding/incarnation movement and re-resolved
    /// (the 074-family discipline on the ranged path).
    pub ranged_read_rebinds: Align64<AtomicU64>,
    /// R5 (§5.7 Yellow row): dehydration-worker victims dropped because the
    /// memory authority paused dehydration entirely (protected included —
    /// disk-tier warmth is the cheapest sacrifice under memory pressure).
    pub mem_budget_dehydrate_paused: Align64<AtomicU64>,
    /// R1b admission (docs/design-read-path.md §5.3): skipped ≈ streamed
    /// cold blocks (the tax kill's adoption signal — ≈ 0 on a streaming
    /// workload means the classifier/admission is broken); admissions ≈
    /// re-read blocks reaching the disk tier; ghost_hits = second-touch
    /// detections; streams_classified / odirect_requests are the
    /// classifier inputs made observable.
    pub read_fill_publishes_skipped: Align64<AtomicU64>,
    pub read_tier_admissions: Align64<AtomicU64>,
    pub read_tier_admission_ghost_hits: Align64<AtomicU64>,
    pub read_streams_classified: Align64<AtomicU64>,
    pub read_odirect_requests: Align64<AtomicU64>,
    /// R2 pipeline (docs/design-read-path.md §5.5): issued/completed/
    /// wasted account every prefetch task (wasted >> 0 = abandonment or
    /// mis-detection); inflight_bytes is the live gauge; window_hwm the
    /// growth witness; foreground_waits drive window growth and should
    /// decay in steady state; evicted_unconsumed is THE refetch-spiral
    /// detector (AIMD collapse trigger); active_streams exposes the
    /// contention-scaling denominator (two-epoch activity gauge).
    pub prefetch_issued: Align64<AtomicU64>,
    pub prefetch_completed: Align64<AtomicU64>,
    pub prefetch_wasted: Align64<AtomicU64>,
    pub prefetch_inflight_bytes: Align64<AtomicU64>,
    pub prefetch_window_hwm: Align64<AtomicU64>,
    pub prefetch_foreground_waits: Align64<AtomicU64>,
    pub prefetch_evicted_unconsumed: Align64<AtomicU64>,
    pub prefetch_active_streams: Align64<AtomicU64>,
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
    /// Deferred-flusher device barriers issued (timer path), vs
    /// strict/fsync barriers which land in `meta_device_syncs` directly.
    pub meta_flush_deferred: Align64<AtomicU64>,
    /// Reclaim group-commit fill: doomed inos per batched `destroy_inodes`
    /// transaction (design §4.5) — headroom before `SQUEEZEFS_RECLAIM_BATCH`
    /// needs raising.
    pub meta_reclaim_batch_size: Align64<QueueDepthHistogram>,
    /// Staging dirs whose content was discarded at init because it was
    /// stamped by a DEAD filesystem generation (or predated generation
    /// stamping) — the reformat-over-stale-staging guard (`cache::nvme::
    /// bind_staging_generation`). One increment per discarded dir; exactly
    /// once per dir after a reformat, 0 on every warm restart.
    pub staging_generation_discards: Align64<AtomicU64>,
    /// Reads of a `staged` file whose payload is GONE — no staging-ring
    /// entry (crash-torn → discarded by segment index recovery, or lost
    /// before a kill) and no promoted mapping. Served as size-consistent
    /// zeros per the D0 staging degrade contract (acked-unfsynced staged
    /// data MAY be lost, must never error). Nonzero after a crash remount
    /// = data loss happened and was degraded, not errored.
    pub staged_payload_lost_reads: Align64<AtomicU64>,
    /// Staged reads that lost a race with an identity transition (re-stage /
    /// promotion / spill / layout flip) and re-resolved the fresh identity
    /// instead of serving zeros or freed bytes (the fstests 074/127/616
    /// transient-zeros family). A live health signal, not an error: bounded
    /// retries that converge. Sustained growth without churn = investigate.
    pub staged_identity_retries: Align64<AtomicU64>,
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

/// Kernel ABI (include/uapi/linux/fuse.h, fuse ≥ 7.38 / Linux ≥ 6.2):
/// open-reply flag that lets the kernel take the inode lock SHARED instead
/// of EXCLUSIVE for non-extending O_DIRECT writes on this open. Older
/// kernels ignore unknown open flags, so advertising it is always safe.
const FOPEN_PARALLEL_DIRECT_WRITES: u32 = 1 << 6;

/// Open/create reply flags for REGULAR files (the virtual .stats/.config
/// opens reply `FOPEN_DIRECT_IO` separately — see `open`). Parallel direct
/// writes are safe under this daemon's lock model: the write handler
/// serializes per inode via `active_inode_locks` for meta-prep and
/// per-block `BLOCK_FLUSH_LOCKS` for data merges (lock order P1), so
/// kernel-parallel submission cannot reorder a block's merges; extending
/// writes stay kernel-exclusive regardless (fuse_dio_lock's past-EOF
/// check), preserving size-extension ordering.
const fn regular_open_reply_flags() -> u32 {
    FOPEN_PARALLEL_DIRECT_WRITES
}

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
    /// §4.5 (PR K7): snapshots of directories ≤
    /// [`DIR_ENTRY_CACHE_MAX_ENTRIES`], cookie-ascending
    /// `(name, ino, §5.1 cookie, file_type)` so cache-served pages keep
    /// the resume contract bit-for-bit. Larger directories stream and
    /// never enter it.
    pub dir_entry_cache_v3:
        moka::sync::Cache<u64, std::sync::Arc<[(std::boxed::Box<str>, u64, u64, u32)]>>,
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
    /// Shared across every `SqueezefsFilesystem` clone. `start_mount` publishes
    /// the live `FuseConnection` here *after* `session.mount(fs.clone(), …)` has
    /// already consumed the clone the request handlers run on, so the cell must
    /// be shared (`Arc`): a per-clone `ArcSwap` would leave the handler's clone
    /// permanently seeing `None`, silently disabling the read zero-copy payload
    /// destination (`get_payload_buffer` in `read`).
    pub session_connection: std::sync::Arc<
        arc_swap::ArcSwap<Option<std::sync::Arc<fuse3::raw::connection::FuseConnection>>>,
    >,
    pub open_inodes: std::sync::Arc<dashmap::DashMap<u64, usize, ahash::RandomState>>,
    pub reclaim_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    /// FUSE-over-io_uring surfaces Destroy once per queue; teardown must
    /// run exactly once.
    dismount_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reclaim_tx: tokio::sync::mpsc::Sender<u64>,
    reclaim_rx: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<u64>>>>,
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
            dir_entry_cache_v3: self.dir_entry_cache_v3.clone(),
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
            // Share the one cell — never split it per clone, or the mounted
            // handler clone would not observe the connection start_mount
            // publishes after mount (re-enables the read zero-copy dest).
            session_connection: self.session_connection.clone(),
            open_inodes: self.open_inodes.clone(),
            reclaim_semaphore: self.reclaim_semaphore.clone(),
            dismount_once: self.dismount_once.clone(),
            reclaim_tx: self.reclaim_tx.clone(),
            reclaim_rx: self.reclaim_rx.clone(),
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
                // Process parallelism, not the (possibly core-pinned)
                // constructor thread's mask — the Hang-1 sizing poison.
                std::cmp::max(4, crate::cpu::process_parallelism())
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
        let dir_entry_cache_v3 = moka::sync::Cache::builder()
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
            dir_entry_cache_v3,
            dismount_wait: 10,
            writeback_tx,
            writeback_rx: std::sync::Arc::new(std::sync::Mutex::new(Some(writeback_rx))),
            writeback_queue_cap: queue_cap,
            client_id: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            mountpoint: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
            max_background_uploads: std::cmp::max(16, crate::cpu::process_parallelism() * 2),
            active_block_buffers: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            open_virtual_files: dashmap::DashMap::with_hasher(ahash::RandomState::new()),
            next_virtual_fh: std::sync::atomic::AtomicU64::new(0x1000_0000_0000_0000),
            latest_stats_json: arc_swap::ArcSwap::new(std::sync::Arc::new(None)),
            latest_config_json: arc_swap::ArcSwap::new(std::sync::Arc::new(None)),
            inodes_limit: std::sync::Arc::new(std::sync::OnceLock::new()),
            session_connection: std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(
                None,
            ))),
            open_inodes: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            reclaim_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                reclaim_concurrency,
            )),
            dismount_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reclaim_tx,
            reclaim_rx: std::sync::Arc::new(std::sync::Mutex::new(Some(reclaim_rx))),
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

        let have_named_backends = !self.router.backend_router.backends.is_empty();
        for (vol_name, status) in &cfg.data_volume_statuses {
            // With named volumes registered, only statuses for REGISTERED
            // names apply. A stale runtime config (the file outlives daemon
            // generations) carrying a phantom `backend_0: disabled` entry
            // would otherwise mark the default slot unhealthy and fail every
            // legacy unprefixed/`backend_0://` key read through the alias.
            if have_named_backends
                && !self
                    .router
                    .backend_router
                    .backends
                    .contains_key(vol_name.as_str())
            {
                continue;
            }
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

        // The data-volume table lists EXACTLY the registered named volumes.
        // `backend_0` is a legacy key-resolution alias of the default slot,
        // not a volume: surfacing it here (pre-fix) made it a phantom entry
        // in `.config` on every multi-volume mount. Only a bare router (no
        // named registrations — offline tools, tests) still reports its
        // default slot under the legacy name.
        let mut data_volumes = serde_json::Map::new();
        let mut data_vol_names: Vec<String> = self
            .router
            .backend_router
            .backends
            .iter()
            .map(|item| item.key().clone())
            .collect();
        if data_vol_names.is_empty() {
            data_vol_names.push("backend_0".to_string());
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
                let path_str = vol.device_path().to_string_lossy().to_string();
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
        // Post-arm classical sideband deliveries (kernel-mandated FORGET/
        // INTERRUPT/resend traffic + fiq->ops switchover stragglers). Must
        // move under unlink storms; a permanent zero here while forgets flow
        // means the sideband is stranded — the stuck-request unmount wedge
        // class.
        #[cfg(target_os = "linux")]
        let t_classical_sideband = fuse3::over_uring_classical_sideband();
        #[cfg(not(target_os = "linux"))]
        let t_classical_sideband = 0u64;

        // PR K7 (design §10): the `meta_kv_*` family is emitted only when
        // a metadata volume is mounted (v3 is the only metadata format).
        let has_meta = self
            .meta_backend
            .as_ref()
            .is_some_and(|mb| !mb.volumes.is_empty());

        let mut stats_obj = serde_json::json!({
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
                "stale_binding_rebinds": METRICS.stale_binding_rebinds.load(Ordering::Relaxed),
                "singleflight_waiter_result_serves": METRICS.singleflight_waiter_result_serves.load(Ordering::Relaxed),
                "hot_block_hits": METRICS.hot_block_hits.load(Ordering::Relaxed),
                "hot_block_misses": METRICS.hot_block_misses.load(Ordering::Relaxed),
                "hot_block_evictions": METRICS.hot_block_evictions.load(Ordering::Relaxed),
                "hot_block_probation_drops": METRICS.hot_block_probation_drops.load(Ordering::Relaxed),
                "hot_block_dehydrate_skips": METRICS.hot_block_dehydrate_skips.load(Ordering::Relaxed),
                "ranged_reads": METRICS.ranged_reads.load(Ordering::Relaxed),
                "ranged_read_bytes": METRICS.ranged_read_bytes.load(Ordering::Relaxed),
                "ranged_read_unaligned_bounces": METRICS.ranged_read_unaligned_bounces.load(Ordering::Relaxed),
                "ranged_read_rebinds": METRICS.ranged_read_rebinds.load(Ordering::Relaxed),
                "mem_budget_bytes": crate::mem_budget::MEM_BUDGET.budget_bytes(),
                "mem_budget_pressure_bytes": crate::mem_budget::MEM_BUDGET.pressure_bytes(),
                "mem_budget_gauge_sum_bytes": crate::mem_budget::MEM_BUDGET.gauge_sum_bytes(),
                "mem_budget_level": crate::mem_budget::MEM_BUDGET.level() as u8,
                "mem_budget_yellow_events": crate::mem_budget::MEM_BUDGET.yellow_events(),
                "mem_budget_red_events": crate::mem_budget::MEM_BUDGET.red_events(),
                "mem_budget_floors_clamped": crate::mem_budget::MEM_BUDGET.floors_clamped(),
                "mem_budget_dehydrate_paused": METRICS.mem_budget_dehydrate_paused.load(Ordering::Relaxed),
                "mem_budget_components": crate::mem_budget::MEM_BUDGET
                    .stats_components()
                    .into_iter()
                    .map(|(name, current, floor, weight, sheds)| {
                        (
                            name.to_string(),
                            serde_json::json!({
                                "current": current,
                                "floor": floor,
                                "weight": weight,
                                "sheds": sheds,
                            }),
                        )
                    })
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                "hot_block_current_bytes": self.router.cache.hot_block.current_bytes(),
                "hot_block_max_bytes": self.router.cache.hot_block.max_bytes(),
                "read_fill_publishes_skipped": METRICS.read_fill_publishes_skipped.load(Ordering::Relaxed),
                "read_tier_admissions": METRICS.read_tier_admissions.load(Ordering::Relaxed),
                "read_tier_admission_ghost_hits": METRICS.read_tier_admission_ghost_hits.load(Ordering::Relaxed),
                "read_streams_classified": METRICS.read_streams_classified.load(Ordering::Relaxed),
                "read_odirect_requests": METRICS.read_odirect_requests.load(Ordering::Relaxed),
                "read_tier_admission_mode": format!("{:?}", self.router.tier_admission),
                "prefetch_issued": METRICS.prefetch_issued.load(Ordering::Relaxed),
                "prefetch_completed": METRICS.prefetch_completed.load(Ordering::Relaxed),
                "prefetch_wasted": METRICS.prefetch_wasted.load(Ordering::Relaxed),
                "prefetch_inflight_bytes": METRICS.prefetch_inflight_bytes.load(Ordering::Relaxed),
                "prefetch_window_hwm": METRICS.prefetch_window_hwm.load(Ordering::Relaxed),
                "prefetch_foreground_waits": METRICS.prefetch_foreground_waits.load(Ordering::Relaxed),
                "prefetch_evicted_unconsumed": METRICS.prefetch_evicted_unconsumed.load(Ordering::Relaxed),
                "prefetch_active_streams": METRICS.prefetch_active_streams.load(Ordering::Relaxed),
                "layout_inline_writes": METRICS.layout_inline_writes.load(Ordering::Relaxed),
                "layout_staged_writes": METRICS.layout_staged_writes.load(Ordering::Relaxed),
                "layout_striped_writes": METRICS.layout_striped_writes.load(Ordering::Relaxed),
                "bg_spawn_admitted": METRICS.bg_spawn_admitted.load(Ordering::Relaxed),
                "bg_spawn_rejected": METRICS.bg_spawn_rejected.load(Ordering::Relaxed),
                "uring_queue_full": METRICS.uring_queue_full.load(Ordering::Relaxed),
                "staging_generation_discards": METRICS.staging_generation_discards.load(Ordering::Relaxed),
                "staged_payload_lost_reads": METRICS.staged_payload_lost_reads.load(Ordering::Relaxed),
                "staged_identity_retries": METRICS.staged_identity_retries.load(Ordering::Relaxed),
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
                "transport_classical_sideband": t_classical_sideband,
                "write_lock_wait": METRICS.write_lock_wait.to_json(),
                "block_lock_wait": METRICS.block_lock_wait.to_json(),
                "lease_lock_wait": METRICS.lease_lock_wait.to_json(),
                "dlm_acquire_time": METRICS.dlm_acquire_time.to_json(),
                "writeback_queue_depth": METRICS.writeback_queue_depth.to_json(),
                "meta_flush_deferred": METRICS.meta_flush_deferred.load(Ordering::Relaxed),
                "meta_reclaim_batch_size": METRICS.meta_reclaim_batch_size.to_json(),
                // Per-volume atomicity fields (design §Observability —
                // live signals over ad-hoc logging). Resolved OQ 2
                // (design-cow-kv-metadata §4.10): TWO fields per volume —
                // `meta_volume_atomicity` is the contract class
                // ("cow-checksummed" by construction) and
                // `meta_volume_atomicity_physical` is the hardware probe,
                // kept alongside for operator visibility.
                "meta_volume_atomicity": self
                    .meta_backend
                    .as_ref()
                    .map(|mb| {
                        mb.volumes
                            .iter()
                            .map(|v| v.atomicity_contract())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                "meta_volume_atomicity_physical": self
                    .meta_backend
                    .as_ref()
                    .map(|mb| {
                        mb.volumes
                            .iter()
                            .map(|v| v.atomicity_physical())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
                // Constant "3" per volume (v3 is the only metadata
                // format); kept as a field because operators key on it.
                "meta_format_version": self
                    .meta_backend
                    .as_ref()
                    .map(|mb| {
                        mb.volumes
                            .iter()
                            .map(|_| "3".to_string())
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

        // PR K7 (design §10): format-scoped metric families (see has_v2 /
        // has_v3 above). Inserted post-macro so each family exists only
        // when a volume of its format is actually mounted.
        {
            use crate::meta_backend::kv as meta_kv;
            let metrics = stats_obj
                .get_mut("metrics")
                .and_then(|m| m.as_object_mut())
                .expect("stats JSON carries a metrics object");
            if has_meta {
                let load = |c: &std::sync::atomic::AtomicU64| -> serde_json::Value {
                    c.load(Ordering::Relaxed).into()
                };
                metrics.insert(
                    "meta_kv_node_cache_hits".into(),
                    load(&meta_kv::META_KV_NODE_CACHE_HITS),
                );
                metrics.insert(
                    "meta_kv_node_cache_misses".into(),
                    load(&meta_kv::META_KV_NODE_CACHE_MISSES),
                );
                metrics.insert(
                    "meta_kv_node_cache_evictions".into(),
                    load(&meta_kv::META_KV_NODE_CACHE_EVICTIONS),
                );
                metrics.insert(
                    "meta_kv_node_appends".into(),
                    load(&meta_kv::META_KV_NODE_APPENDS),
                );
                metrics.insert(
                    "meta_kv_node_append_bytes".into(),
                    load(&meta_kv::META_KV_NODE_APPEND_BYTES),
                );
                metrics.insert(
                    "meta_kv_node_rewrite_bytes".into(),
                    load(&meta_kv::META_KV_NODE_REWRITE_BYTES),
                );
                metrics.insert(
                    "meta_kv_node_compactions".into(),
                    load(&meta_kv::META_KV_NODE_COMPACTIONS),
                );
                metrics.insert(
                    "meta_kv_node_splits".into(),
                    load(&meta_kv::META_KV_NODE_SPLITS),
                );
                metrics.insert(
                    "meta_kv_journal_bytes".into(),
                    load(&meta_kv::META_KV_JOURNAL_BYTES),
                );
                metrics.insert(
                    "meta_kv_journal_entries".into(),
                    load(&meta_kv::META_KV_JOURNAL_ENTRIES),
                );
                metrics.insert(
                    "meta_kv_checkpoints".into(),
                    load(&meta_kv::META_KV_CHECKPOINTS),
                );
                metrics.insert(
                    "meta_kv_commit_smo_retries".into(),
                    load(&meta_kv::META_KV_COMMIT_SMO_RETRIES),
                );
                metrics.insert(
                    "meta_kv_node_dropped_tail_bsets".into(),
                    load(&meta_kv::META_KV_NODE_DROPPED_TAIL_BSETS),
                );
                metrics.insert(
                    "meta_kv_dentry_collision_overflows".into(),
                    load(&meta_kv::META_KV_DENTRY_COLLISION_OVERFLOWS),
                );
                metrics.insert(
                    "meta_kv_delta_orphans".into(),
                    load(&meta_kv::META_KV_DELTA_ORPHANS),
                );
                // Per-volume gauges (mount-scoped replay stats, allocator
                // occupancy, ring-admission parks), arrays parallel to
                // `meta_format_version`.
                let per_volume = |f: &dyn Fn(
                    &crate::meta_backend::kv::backend::KvMetaBackend,
                ) -> u64|
                 -> serde_json::Value {
                    self.meta_backend
                        .as_ref()
                        .map(|mb| {
                            serde_json::Value::Array(
                                mb.volumes.iter().map(|be| f(be).into()).collect(),
                            )
                        })
                        .unwrap_or_default()
                };
                metrics.insert(
                    "meta_kv_replay_entries".into(),
                    per_volume(&|be| be.replay_stats().entries),
                );
                metrics.insert(
                    "meta_kv_replay_dropped_torn".into(),
                    per_volume(&|be| be.replay_stats().dropped_torn),
                );
                metrics.insert(
                    "meta_kv_replay_ms".into(),
                    per_volume(&|be| be.replay_stats().replay_ms),
                );
                metrics.insert(
                    "meta_kv_journal_full_stalls".into(),
                    per_volume(&|be| be.journal_full_stalls()),
                );
                metrics.insert(
                    "meta_kv_free_extents".into(),
                    per_volume(&|be| be.free_extents()),
                );
                metrics.insert(
                    "meta_kv_pending_free".into(),
                    per_volume(&|be| be.pending_free_extents()),
                );
            }
        }

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
                let mut block_data =
                    if let Some((_, buf)) = self.active_block_buffers.remove(&cache_key) {
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

                        let mut existing: Option<crate::cache::pool::ReadBlockValue> = None;
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
                                // The seed is a cache FILL like any other: `bk`
                                // can be displaced, freed, and reallocated under
                                // the SAME key string while (or right after) we
                                // read it — including by THIS call, when the
                                // merged block completes and write-through
                                // displaces + frees `bk` before a detached tier
                                // publish of its old bytes lands. Route through
                                // the BINDING-VALIDATED fetch (the 8e3995e
                                // follow-up, closed): single-flight validated
                                // fill (read_lru → NVMe tier → device read
                                // under the incarnation seqlock) PLUS the
                                // block-index→key recheck once the bytes are
                                // in hand — a key reallocated to another block
                                // mid-seed would otherwise become this RMW's
                                // base and merge user data over a foreign
                                // block's bytes (persistent corruption). A
                                // hole rebind (concurrent truncate/punch)
                                // seeds zeros.
                                existing = self
                                    .router
                                    .get_block_for_index(&file_path, b as u32, Some(&bk))
                                    .await?;
                            }
                        }

                        crate::cache::active_block::ActiveBlockBuf::seeded(
                            existing.as_deref().unwrap_or(&[]),
                            block_size as usize,
                        )
                    };

                // ONE-AUTHORITY INVARIANT (generic/075.2): per block, the
                // newest content lives in exactly one overlay — the RAM
                // parked buffer XOR the staged `active_block:` entry. We now
                // own the RMW base (consumed the parked buffer, read the
                // staged entry, or seeded device/fresh) and are about to
                // mutate + re-park/upload it, so any staged sibling is
                // superseded: remove it under this block's lock. Leaving it
                // let flush_one_active_block upload the STALE staged copy
                // over the newer merge, and let router reads serve pre-write
                // bytes through the ACTIVE_STAGED tier once the RAM buffer
                // moved on (both observed live in the fsx-075 soak). Any
                // queued WritebackRequest for it becomes a clean no-op; the
                // durability promise transfers to this write's own
                // park/write-through path.
                //
                // spawn_blocking (same rule as every put_active_block call):
                // the staging-shard WRITE lock is a parking_lot lock, and
                // §5.5 DMA sources hold the shard READ lock ACROSS awaits —
                // parking an async worker on the writer side starves the
                // executor until no worker is left to poll the guard-holding
                // tasks (observed live: total daemon wedge under the fsx-075
                // harness, every worker parked in NvmeShard lock_shared/
                // lock_exclusive).
                {
                    let nvme = self.router.cache.nvme.clone();
                    let key = cache_key.clone();
                    tokio::task::spawn_blocking(move || nvme.remove_active_block(&key))
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }

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
                                )
                                .await;
                            }
                        }
                    }
                } else {
                    self.insert_active_block_buffer(cache_key.clone(), block_data, fencing_token)
                        .await;
                    std::mem::drop(block_guard);
                }

                Ok::<(), SqueezefsError>(())
            });
        }

        futures::future::try_join_all(futures).await?;

        Ok(())
    }

    /// Punch a hole: make `[offset, offset+length)` read back as zeros WITHOUT
    /// changing the file's logical size (POSIX `FALLOC_FL_PUNCH_HOLE`). A hole
    /// reads as zeros (an unwritten / punched / extended region), so this is the
    /// zero-or-unmap primitive shared by the fallocate handler.
    ///
    /// - **Striped:** whole covered blocks are unmapped + freed (real holes,
    ///   space reclaimed, incarnation-retired so a reused offset never reads
    ///   back through the stale map); partial edges are RMW-zeroed through the
    ///   write path (preserving the un-punched bytes of the edge block).
    /// - **inline / staged:** the covered sub-range is RMW-zeroed in place via
    ///   the router write path (which refreshes the whole-file cache too).
    ///
    /// Caller MUST hold the per-inode write guard (serializes against writes;
    /// makes any in-flight writeback for a dropped block a no-op — the worker
    /// skips a `NotFound` active block and takes the same guard before its map
    /// merge). Size is never grown (`end` is clamped to EOF).
    async fn punch_hole_range(
        &self,
        ino: u64,
        offset: u64,
        length: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let file_path = crate::keys::inode_path(ino);
        let meta = self.router.fetch_metadata(&file_path).await?;
        if length == 0 {
            return Ok(());
        }
        // Honor the kernel's punch range [offset, offset+length). Do NOT clamp
        // `end` to our own `meta.size`: under the FUSE writeback cache a
        // deferred write flush leaves `meta.size` (what fetch_metadata reports)
        // lagging the kernel `i_size`, so clamping would skip zeroing a tail the
        // kernel considers in-bounds — the punched range then reads its stale
        // prior bytes (the generic/616 residual; same laggable-size trap the
        // copy_file_range fix hit). The kernel only punches within `i_size`, so
        // zeroing the full range is safe and reconciles the lagging size.
        let end = offset + length;
        // Size floor for map/layout saves: never regress below the freshest
        // known size, and let the punched range extend it if our size lagged.
        let size_floor = meta.size.max(end);

        if meta.file_type == "striped" {
            let bs = self.router.block_size.load(Ordering::Relaxed);
            let start_b = offset / bs;
            let end_b = (end - 1) / bs;
            let mut whole_idxs: Vec<u32> = Vec::new();
            for b in start_b..=end_b {
                let b_start = b * bs;
                let b_end = b_start + bs;
                let cov_start = std::cmp::max(offset, b_start);
                let cov_end = std::cmp::min(end, b_end);
                if cov_start == b_start && cov_end == b_end {
                    // Whole block → hole. Drop the RAM + staged active-block
                    // copy under the block lock (serialize against a racing
                    // flush) before it is unmapped + freed below.
                    let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b as u32);
                    let block_guard = block_lock.lock().await;
                    let key = crate::keys::active_block(ino, b).to_string();
                    self.active_block_buffers.remove(&key);
                    // Blocking-pool hop: shard WRITE lock (invariant rule 2).
                    self.router
                        .cache
                        .nvme
                        .remove_active_block_async(key)
                        .await?;
                    drop(block_guard);
                    whole_idxs.push(b as u32);
                } else {
                    // Partial edge → RMW-zero exactly the covered sub-range via
                    // the striped write path: it consumes the active buffer /
                    // device block as the RMW base, overlays zeros in the
                    // covered range (recorded as covered), and preserves the
                    // un-punched bytes.
                    let zeros = bytes::Bytes::from(vec![0u8; (cov_end - cov_start) as usize]);
                    self.write_file_staged(ino, cov_start, zeros, size_floor, fencing_token)
                        .await?;
                }
            }
            // Unmap + free the whole blocks (holes); the size floor keeps the
            // logical size at least the freshest known / punched extent.
            self.router
                .punch_striped_blocks(ino, &whole_idxs, size_floor, fencing_token)
                .await?;
        } else {
            // inline / staged: RMW-zero the covered range in place through the
            // router write path. It reads the authoritative base (staging for
            // staged, data_key for inline) and rewrites the range as zeros.
            let zeros = bytes::Bytes::from(vec![0u8; (end - offset) as usize]);
            self.router
                .write_file(&file_path, offset, zeros, fencing_token)
                .await?;
        }
        Ok(())
    }

    /// Drop every active-block overlay — parked RAM `ActiveBlockBuf` and
    /// staged `active_block:` ring entry — whose block lies entirely at/after
    /// `new_size`. Truncate prunes those blocks from the map; an overlay that
    /// survives the prune is stale beyond-EOF content that would serve
    /// single-block reads, seed the next partial write's RMW, or re-merge
    /// into the map on the next flush (the generic/075.2 stale-data
    /// resurrection). Caller MUST hold the per-inode write guard; each
    /// removal runs under that block's `BLOCK_FLUSH_LOCKS` (lock order 1→3),
    /// making any in-flight writeback for the dropped block a NotFound no-op.
    async fn drop_active_block_overlays_beyond(&self, ino: u64, new_size: u64) {
        let bs = self.router.block_size.load(Ordering::Relaxed);
        if bs == 0 {
            return;
        }
        let first_dead_block = new_size.div_ceil(bs);
        let prefix = crate::keys::active_block_ino_prefix(ino);
        let mut dead: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for r in self.active_block_buffers.iter() {
            if let Some((i, b)) = Self::parse_active_block_key(r.key()) {
                if i == ino && (b as u64) >= first_dead_block {
                    dead.insert(b);
                }
            }
        }
        // Staged overlays can exist without a RAM buffer (RAM-cap spill,
        // write-through staging fallback awaiting writeback): sweep the
        // staging key space too. Truncate is a cold path; the key list is
        // bounded by the staging budget.
        for key in self.router.cache.nvme.list_staged_files() {
            if !key.starts_with(prefix.as_str()) {
                continue;
            }
            if let Some((i, b)) = Self::parse_active_block_key(&key) {
                if i == ino && (b as u64) >= first_dead_block {
                    dead.insert(b);
                }
            }
        }
        for b in dead {
            let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
            let _block_guard = block_lock.lock().await;
            let key = crate::keys::active_block(ino, b as u64).to_string();
            self.active_block_buffers.remove(&key);
            // spawn_blocking: the staging-shard WRITE lock must never park
            // an async worker (§5.5 read guards are held across DMA awaits;
            // see flush_one_active_block).
            let nvme = self.router.cache.nvme.clone();
            if tokio::task::spawn_blocking(move || nvme.remove_active_block(&key))
                .await
                .is_err()
            {
                continue;
            }
        }
    }

    /// The freshest known logical size of `ino`: the maximum of the durable
    /// inode size and the RAM-side caches the write path updates synchronously
    /// (`metadata_cache`, `attr_cache`). The durable size LAGS the truth —
    /// staged/inline writes defer their layout+size persist to the fsync/flush
    /// cadence — so any size-classifying decision (grow-vs-shrink, extend
    /// no-op) taken against the durable size alone misclassifies whenever
    /// unflushed writes grew the file (the generic/091 lost-write family).
    /// Taking the max never regresses: each source only ever runs behind the
    /// kernel-observed size, never ahead of it.
    async fn freshest_size(&self, ino: u64) -> Result<u64, SqueezefsError> {
        let backend = self.meta_backend.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;
        let mut size = backend.getattr(ino).await?.size;
        let file_path = crate::keys::inode_path(ino);
        if let Some(m) = self.router.metadata_cache.get(&file_path) {
            size = size.max(m.size);
        }
        if let Some((attr, _)) = self.attr_cache.get(&ino) {
            size = size.max(attr.size);
        }
        Ok(size)
    }

    /// Grow a file's logical size to `target_size` (a no-op if already at least
    /// that large). The newly exposed region [old_size, target_size) is a hole
    /// (reads zeros). Shared by the fallocate preallocate/extend and ZERO_RANGE
    /// paths.
    ///
    /// MUST never shrink: the no-op gate compares against the FRESHEST size
    /// (durable + RAM caches), not the laggable durable inode size, and the
    /// layout-size stores below only ever move the size up. Gating on the
    /// durable size alone let an in-bounds (interior) fallocate issued while
    /// staged writes were still unflushed clobber the logical size DOWN to
    /// `offset + length`, hiding all data beyond it — the generic/091
    /// "written data reads back zeros" corruption.
    async fn extend_file_size(
        &self,
        ino: u64,
        target_size: u64,
        fencing_token: u64,
    ) -> Result<(), SqueezefsError> {
        let backend = self.meta_backend.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;

        let old_size = self.freshest_size(ino).await?;
        if target_size <= old_size {
            return Ok(());
        }

        // Update in backend
        backend
            .setattr(ino, None, None, None, Some(target_size), None, None, None)
            .await?;

        // Update layout size. Striped files go through the §5.3 degenerate
        // size-only merge: the old whole-meta save of a stale snapshot under NO
        // lock could rewrite the block map "without mutating it", dropping
        // mappings a concurrent write-through just published. Inline/staged
        // files keep the whole-meta save — their RAM meta (dirty inline payload
        // / staged identity) is the truth a backend re-read cannot carry, and
        // they have no striped map to lose.
        let file_path = crate::keys::inode_path(ino);
        if let Ok(mut meta) = self.router.fetch_metadata(&file_path).await {
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
            } else if meta.size < target_size {
                meta.size = target_size;
                let _ = self
                    .router
                    .save_metadata_to_backend(ino, &meta, fencing_token)
                    .await;
            }
        }

        // Update cache (never regress a fresher attr size).
        if let Some((mut attr, _)) = self.attr_cache.get(&ino) {
            if attr.size < target_size {
                attr.size = target_size;
                attr.blocks = target_size.div_ceil(512);
                self.attr_cache
                    .insert(ino, (attr, std::time::Instant::now()));
            }
        }
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
        let (be_id, block_allocator, nvme_writer) =
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
        let new_key = self.router.backend_router.persist_block_key(&be_id, offset);
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
        self.router.cache.purge_block_key(&new_key);

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
        // Blocking-pool hop: shard WRITE lock (invariant rule 2).
        self.router
            .cache
            .nvme
            .remove_active_block_async(cache_key)
            .await?;
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

    /// R5 gauge: bytes parked in RAM as active block buffers (count ×
    /// block size — buffers are block-sized by construction).
    pub fn parked_buffer_bytes(&self) -> u64 {
        self.active_block_buffers.len() as u64 * self.router.block_size.load(Ordering::Relaxed)
    }

    /// §5.7 Red drain — "early `flush_memory_buffers_*`, the existing
    /// never-lossy staging path, just earlier": flush parked buffers
    /// inode-by-inode through the durable upload/staging path until the
    /// parked gauge is ≤ `target` bytes. The row-5 trace proved
    /// cap-halving alone cannot hold the line: it only gates INSERTS,
    /// while the backlog itself kept growing (spill-to-staging ran 42 %
    /// refused — a 4 MiB entry every ~10 MiB shard — and the rand-write
    /// revisit carousel pulled spilled entries straight back to RAM).
    /// Durability semantics untouched: every buffer goes through
    /// `flush_memory_buffers_for_inode` — upload or staged, never
    /// dropped.
    pub async fn drain_parked_toward(&self, target: u64) {
        let mut last_len = usize::MAX;
        while self.parked_buffer_bytes() > target {
            let len = self.active_block_buffers.len();
            if len == 0 || len >= last_len {
                // No forward progress (writers re-parking as fast as the
                // drain flushes): stop — the sampler re-fires next tick.
                break;
            }
            last_len = len;
            let mut inos: Vec<u64> = self
                .active_block_buffers
                .iter()
                .take(64)
                .filter_map(|r| Self::parse_active_block_key(r.key()).map(|(ino, _)| ino))
                .collect();
            inos.sort_unstable();
            inos.dedup();
            for ino in inos {
                let Ok(fencing_token) = self.get_or_acquire_lease(ino).await else {
                    continue;
                };
                let _ = self
                    .flush_memory_buffers_for_inode(ino, fencing_token)
                    .await;
                if self.parked_buffer_bytes() <= target {
                    break;
                }
            }
        }
    }

    async fn insert_active_block_buffer(
        &self,
        cache_key: String,
        block_data: crate::cache::active_block::ActiveBlockBuf,
        fencing_token: u64,
    ) {
        // R5 Red (§5.7): the spill threshold halves — parked dirty bytes
        // reach the existing never-lossy staging path at half the count
        // (the fast, guaranteed RSS reducer of the row-5 cage shape).
        let parked_cap = crate::mem_budget::effective_parked_cap(
            MAX_ACTIVE_BLOCK_BUFFERS,
            crate::mem_budget::level(),
        );
        'spill: while self.active_block_buffers.len() >= parked_cap {
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
                // Blocking-pool hop: shard WRITE lock (invariant rule 2).
                let admitted = self
                    .router
                    .cache
                    .nvme
                    .put_active_block_async(spill_key.clone(), data.snapshot(), fencing_token)
                    .await
                    .unwrap_or(false);
                if !admitted {
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

    /// PR K7 (§5.1): one streaming-readdir page of
    /// `(resume_cookie, entry)` pairs. Offsets at or above the
    /// virtual-entry cookies never reach the backend: nothing real lives
    /// there, and they do not decode as §5.1 cookies.
    async fn readdir_v3_page(
        &self,
        parent: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, crate::meta_backend::DirEntry)>, SqueezefsError> {
        let backend = self.meta_backend.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;
        if offset >= READDIR_VIRTUAL_CONFIG_COOKIE {
            return Ok(Vec::new());
        }
        backend.readdir_stream(parent, offset, max).await
    }

    /// [`Self::readdir_v3_page`] behind the §4.5 small-directory cache:
    /// directories ≤ [`DIR_ENTRY_CACHE_MAX_ENTRIES`] are snapshotted
    /// cookie-ascending on a listing start and pages are served as cache
    /// slices (the repeat-`readdir` hot shape); larger directories bypass
    /// and stream page-by-page — never OOMing the cache. Cache-served
    /// pages carry the stored §5.1 cookies, so the resume contract is
    /// identical on both paths.
    async fn v3_listing_page(
        &self,
        parent: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, crate::meta_backend::DirEntry)>, SqueezefsError> {
        let slice_page = |cached: &std::sync::Arc<[(std::boxed::Box<str>, u64, u64, u32)]>|
         -> Vec<(u64, crate::meta_backend::DirEntry)> {
            // §5.1 resume rule: offsets 0/1/2 ⇒ the start; c ≥ 3 ⇒
            // strictly after cookie c (entries are cookie-ascending).
            let start = if offset < 3 {
                0
            } else {
                cached.partition_point(|e| e.2 <= offset)
            };
            cached[start..]
                .iter()
                .take(max)
                .map(|(name, ino, cookie, ft)| {
                    (
                        *cookie,
                        crate::meta_backend::DirEntry {
                            ino: *ino,
                            name: name.to_string(),
                            file_type: *ft,
                        },
                    )
                })
                .collect()
        };
        if let Some(cached) = self.dir_entry_cache_v3.get(&parent) {
            return Ok(slice_page(&cached));
        }
        // Listing start on an uncached directory: probe up to the cache
        // cap + 1; small directories snapshot (with cookies), larger ones
        // hand back the streamed prefix and stay stream-only.
        if offset < 3 {
            let mut all: Vec<(u64, crate::meta_backend::DirEntry)> = Vec::new();
            let mut cursor = 0u64;
            loop {
                let page = self
                    .readdir_v3_page(parent, cursor, V3_READDIR_PAGE)
                    .await?;
                let short = page.len() < V3_READDIR_PAGE;
                if let Some((c, _)) = page.last() {
                    cursor = *c;
                }
                all.extend(page);
                if short || all.len() > DIR_ENTRY_CACHE_MAX_ENTRIES {
                    break;
                }
            }
            if all.len() <= DIR_ENTRY_CACHE_MAX_ENTRIES {
                let snapshot: std::sync::Arc<[(std::boxed::Box<str>, u64, u64, u32)]> = all
                    .iter()
                    .map(|(cookie, d)| {
                        (d.name.clone().into_boxed_str(), d.ino, *cookie, d.file_type)
                    })
                    .collect::<Vec<_>>()
                    .into();
                self.dir_entry_cache_v3.insert(parent, snapshot);
            }
            all.truncate(max);
            return Ok(all);
        }
        self.readdir_v3_page(parent, offset, max).await
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

        // R5 (§5.7): register the RAM consumers with the joint memory
        // authority and start its 1 Hz sampler. Registration is guarded
        // (one registry per process) — remounts in-process must not
        // duplicate components.
        {
            use crate::mem_budget::{Component, MEM_BUDGET};
            use std::sync::Arc;
            static REGISTERED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !REGISTERED.swap(true, Ordering::Relaxed) {
                const MIB: u64 = 1024 * 1024;
                let bufs = self.active_block_buffers.clone();
                let bs_atomic = self.router.block_size.clone();
                // §5.7 Red parked shed: cap-halving at admission gates NEW
                // parks; the DRAIN below flushes the existing backlog
                // through the durable path (the "early
                // flush_memory_buffers_*" mechanism — measured necessary:
                // the row-5 trace grew 250 → 1,750 parked buffers with the
                // cap alone). Shed closures are sync, the flush is async:
                // the closure posts the target and wakes a drain worker.
                let drain_target = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
                let drain_notify = Arc::new(tokio::sync::Notify::new());
                {
                    let fs = self.clone();
                    let target = drain_target.clone();
                    let notify = drain_notify.clone();
                    tokio::spawn(async move {
                        loop {
                            notify.notified().await;
                            let t = target.load(Ordering::Relaxed);
                            fs.drain_parked_toward(t).await;
                        }
                    });
                }
                let shed_target = drain_target.clone();
                MEM_BUDGET.register(Component::new(
                    "parked_write_buffers",
                    32 * 4 * MIB, // 32 parked blocks at the default 4 MiB
                    4,
                    Arc::new(move || bufs.len() as u64 * bs_atomic.load(Ordering::Relaxed)),
                    Arc::new(move |target| {
                        shed_target.store(target, Ordering::Relaxed);
                        drain_notify.notify_one();
                    }),
                ));
                let hot = self.router.cache.hot_block.clone();
                let hot_shed = self.router.cache.hot_block.clone();
                MEM_BUDGET.register(Component::new(
                    "hot_block_tier",
                    64 * MIB,
                    4,
                    Arc::new(move || hot.current_bytes()),
                    Arc::new(move |target| hot_shed.shed_to(target)),
                ));
                let rl = self.router.cache.read_lru.clone();
                let rl_shed = self.router.cache.read_lru.clone();
                MEM_BUDGET.register(Component::new(
                    "read_lru",
                    32 * MIB,
                    2,
                    Arc::new(move || rl.current_bytes()),
                    Arc::new(move |target| rl_shed.shed_to(target)),
                ));
                let wl = self.router.cache.write_lru.clone();
                let wl_shed = self.router.cache.write_lru.clone();
                MEM_BUDGET.register(Component::new(
                    "write_lru",
                    32 * MIB,
                    2,
                    Arc::new(move || wl.current_bytes()),
                    Arc::new(move |target| wl_shed.shed_to(target)),
                ));
                let lanes = self.router.stream_lanes.clone();
                MEM_BUDGET.register(Component::new(
                    "prefetch_inflight",
                    0,
                    1,
                    Arc::new(|| METRICS.prefetch_inflight_bytes.load(Ordering::Relaxed)),
                    // Red: plans cleared — lane invalidation is the existing
                    // abandonment semantics (in-flight fills settle wasted).
                    Arc::new(move |_| lanes.invalidate_all()),
                ));
                let staging = self.router.cache.nvme.clone();
                MEM_BUDGET.register(Component::new(
                    "staging_mmap",
                    0,
                    0, // weight 0: staging keeps its existing refusal behavior
                    Arc::new(move || staging.current_staged_write_bytes()),
                    Arc::new(|_| {}),
                ));
                let tier = self.router.cache.nvme.clone();
                MEM_BUDGET.register(Component::new(
                    "read_tier_mmap",
                    0,
                    0, // mmap residency is kernel-owned; reclaim_extent is churn-driven (§5.7)
                    Arc::new(move || tier.current_read_cache_bytes()),
                    Arc::new(|_| {}),
                ));
                MEM_BUDGET.register(Component::new(
                    "buffer_pool",
                    16 * 4 * MIB,
                    2,
                    Arc::new(|| crate::cache::pool::BUFFER_POOL.allocated_bytes()),
                    Arc::new(|target| crate::cache::pool::BUFFER_POOL.trim_to(target)),
                ));
                MEM_BUDGET.register(Component::new(
                    "aligned_buf_pool",
                    16 * 4 * MIB,
                    2,
                    Arc::new(|| crate::cache::pool::ALIGNED_BUF_POOL.allocated_bytes()),
                    Arc::new(|target| crate::cache::pool::ALIGNED_BUF_POOL.trim_to(target)),
                ));
            }
            crate::mem_budget::spawn_sampler();
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

        // Clean-unmount teardown (K6b): each volume runs a final
        // checkpoint and JOINS its checkpoint task (tail == head ⇒ empty
        // replay window; no leaked tasks — design §4.6 /
        // tests/dismount_teardown_tests.rs).
        if let Some(ref backend) = self.meta_backend {
            for vol in &backend.volumes {
                if let Err(e) = vol.shutdown().await {
                    warn!("Meta volume unmount teardown failed: {:?}", e);
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
            self.dir_entry_cache_v3.invalidate(&parent);
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
            self.dir_entry_cache_v3.invalidate(&parent);
            // Keep parent attr in cache; only dir_entry listing is stale.
            self.add_open(inode.ino);
            Ok(ReplyCreated {
                ttl: Duration::from_secs(1),
                attr,
                generation: 1,
                fh: inode.ino,
                flags: regular_open_reply_flags(),
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
            flags: regular_open_reply_flags(),
        })
    }

    async fn opendir(&self, _req: Request, inode: Inode, _flags: u32) -> FuseResult<ReplyOpen> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Opendir: inode = {}", inode);

        self.add_open(inode);

        // PR K7 (§5.1): directories stream by key cookie — a per-fh
        // whole-directory snapshot is both wasted work and an OOM hazard
        // at 1 M entries. The fh is still minted so releasedir
        // bookkeeping stays uniform.
        let fh = self.next_dir_fh.fetch_add(1, Ordering::Relaxed);
        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn releasedir(&self, _req: Request, ino: u64, fh: u64, _flags: u32) -> FuseResult<()> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        debug!("FUSE Releasedir: ino = {}, fh = {}", ino, fh);

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
        flags: u32,
    ) -> FuseResult<ReplyData> {
        METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
        crate::coz_progress!("fuse_read");
        debug!(
            "FUSE Read: ino = {}, fh = {}, offset = {}, size = {}, flags = {:#x}",
            ino, fh, offset, size, flags
        );
        // R1b classifier inputs (§5.3): the kernel sends the file's open
        // flags on every READ; O_DIRECT is counted here, and the request
        // feeds the file's offset lanes. Both are advisory/observability
        // in PR 4 (the publish decision is ghost-driven at the fill site);
        // PR 5's pipeline and PR 6's ranged dispatch build on them.
        if flags & (libc::O_DIRECT as u32) != 0 {
            METRICS
                .read_odirect_requests
                .fetch_add(1, Ordering::Relaxed);
        }

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
            self.dir_entry_cache_v3.invalidate(&parent);
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
            self.dir_entry_cache_v3.invalidate(&parent);
            self.dir_entry_cache_v3.invalidate(&current_inode.ino);
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
                // Classify grow-vs-shrink against the FRESHEST size, never the
                // durable inode size alone: staged/inline writes defer their
                // layout+size persist, so `current_inode.size` lags and a real
                // shrink would be misclassified as a grow (skipping the
                // straddle-zero below — the generic/075 stale-tail trap).
                let old_size = self
                    .freshest_size(ino)
                    .await
                    .map_err(map_squeezefs_err)?
                    .max(current_inode.size);

                // Striped shrink to a non-block-aligned size: the block that
                // straddles new_size survives the map removal below with stale
                // bytes in [new_size, block_end). A later re-extend would read
                // those instead of zeros (a hole must read zeros). RMW-zero that
                // tail now, while the file is still at its old size so the write
                // never grows it — the block is then stored clean. This consumes
                // (and thereby clips) any parked/staged overlay of the straddling
                // block as its RMW base.
                if new_size < old_size {
                    let bs = self.router.block_size.load(Ordering::Relaxed);
                    if bs > 0 && new_size % bs != 0 {
                        let file_path = crate::keys::inode_path(ino);
                        if let Ok(meta) = self.router.fetch_metadata(&file_path).await {
                            if meta.file_type == "striped" {
                                let block_end = (new_size / bs + 1) * bs;
                                let zero_to = std::cmp::min(old_size, block_end);
                                if zero_to > new_size {
                                    let zeros = bytes::Bytes::from(vec![
                                        0u8;
                                        (zero_to - new_size)
                                            as usize
                                    ]);
                                    self.write_file_staged(
                                        ino,
                                        new_size,
                                        zeros,
                                        old_size,
                                        fencing_token,
                                    )
                                    .await
                                    .map_err(map_squeezefs_err)?;
                                }
                            }
                        }
                    }
                }

                // Blocks entirely at/after new_size are GONE: their parked RAM
                // buffers and staged `active_block:` overlays must die with
                // them, or the stale overlay outlives the map prune — serving
                // pre-truncate bytes to single-block reads, seeding the next
                // partial write's RMW, and re-merging the whole stale block on
                // the next flush (the generic/075.2 resurrection). Purge BEFORE
                // the map prune so a racing writeback upload NotFound-skips;
                // one that already merged is pruned by truncate_layout below.
                self.drop_active_block_overlays_beyond(ino, new_size).await;

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
            self.dir_entry_cache_v3.invalidate(&parent);
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
            self.dir_entry_cache_v3.invalidate(&new_parent);
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
            self.dir_entry_cache_v3.invalidate(&parent);
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
            self.dir_entry_cache_v3.invalidate(&parent);
            self.dir_entry_cache_v3.invalidate(&new_parent);
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

            self.dir_entry_cache_v3.invalidate(&parent);
            self.dir_entry_cache_v3.invalidate(&new_parent);
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
            // PR K7 (§5.1): directories stream by key cookies.
            let off_u = offset.max(0) as u64;
            let page = self
                .v3_listing_page(parent, off_u, V3_READDIR_PAGE)
                .await
                .map_err(map_squeezefs_err)?;
            // A full page may have more real entries behind it; the
            // root virtuals ride above the whole real-cookie space and
            // are appended only once the real stream is exhausted.
            let more_reals = page.len() == V3_READDIR_PAGE;
            let mut entries = Vec::with_capacity(page.len() + 4);
            if off_u < 1 {
                entries.push(DirectoryEntry {
                    name: ".".into(),
                    kind: FileType::Directory,
                    inode: parent,
                    offset: 1,
                });
            }
            if off_u < 2 {
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
                    offset: 2,
                });
            }
            for (cookie, d) in page {
                entries.push(DirectoryEntry {
                    name: d.name.into(),
                    kind: self.mode_to_file_type(d.file_type),
                    inode: d.ino,
                    offset: cookie as i64,
                });
            }
            if parent == 1 && !more_reals {
                if off_u < READDIR_VIRTUAL_CONFIG_COOKIE {
                    entries.push(DirectoryEntry {
                        name: ".config".into(),
                        kind: FileType::RegularFile,
                        inode: CONFIG_INODE,
                        offset: READDIR_VIRTUAL_CONFIG_COOKIE as i64,
                    });
                }
                if off_u < READDIR_VIRTUAL_STATS_COOKIE {
                    entries.push(DirectoryEntry {
                        name: ".stats".into(),
                        kind: FileType::RegularFile,
                        inode: STATS_INODE,
                        offset: READDIR_VIRTUAL_STATS_COOKIE as i64,
                    });
                }
            }
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
            // PR K7 (§5.1): directories stream by key cookies.
            {
                let page = self
                    .v3_listing_page(parent, offset, V3_READDIRPLUS_PAGE)
                    .await
                    .map_err(map_squeezefs_err)?;
                let more_reals = page.len() == V3_READDIRPLUS_PAGE;
                let mut entries = Vec::with_capacity(page.len() + 4);
                if offset < 1 {
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
                if offset < 2 {
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
                        offset: 2,
                    });
                }
                for (cookie, d) in page {
                    let attr = match self.get_attr_internal(d.ino).await {
                        Ok(a) => a,
                        Err(e) => {
                            error!(
                                "readdirplus failed to get attr for child {}: {:?}",
                                d.ino, e
                            );
                            continue;
                        }
                    };
                    entries.push(DirectoryEntryPlus {
                        name: d.name.into(),
                        kind: attr.kind,
                        inode: d.ino,
                        generation: 1,
                        attr,
                        entry_ttl: Duration::from_secs(1),
                        attr_ttl: Duration::from_secs(1),
                        offset: cookie as i64,
                    });
                }
                if parent == 1 && !more_reals {
                    if offset < READDIR_VIRTUAL_CONFIG_COOKIE {
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
                            offset: READDIR_VIRTUAL_CONFIG_COOKIE as i64,
                        });
                    }
                    if offset < READDIR_VIRTUAL_STATS_COOKIE {
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
                            offset: READDIR_VIRTUAL_STATS_COOKIE as i64,
                        });
                    }
                }
                use futures::stream::{self, StreamExt};
                let stream = stream::iter(entries.into_iter().map(Ok)).boxed();
                Ok(ReplyDirectoryPlus { entries: stream })
            }
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

        // 0. Make the SOURCE fully visible to the router-level reads below.
        // The copy reads the source through `DataRouter::read_file` (and the
        // whole-file clone through the block map), which cannot see the
        // FUSE-layer parked `ActiveBlockBuf`s / staged `active_block:`
        // overlays a partial striped write leaves behind — the copy would
        // silently source pre-write bytes (zeros for a hole-extending write):
        // the generic/075.2 lost-copy corruption. Flush-merges them into the
        // map BEFORE taking the inode guards (the flush takes the write guard
        // internally when it has work; a no-overlay scan is one prefix pass).
        {
            let src_flush_token = self.dlm.get_fencing_token_ino(inode);
            let _ = self
                .flush_active_blocks_with_retry(inode, src_flush_token)
                .await;
        }

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

        // 3. General partial-range copy.
        //
        // The kernel (vfs_copy_file_range -> generic_copy_file_checks) has
        // already clamped `length` to the source's EOF (its cached `i_size`)
        // before dispatching, so a nonzero `length` here denotes bytes the
        // caller is entitled to copy. The handler must therefore ALWAYS make
        // forward progress: returning copied == 0 for a nonzero request wedges
        // a copy_file_range caller loop (fsx advances only on nr > 0 and never
        // breaks on nr == 0) — the live generic/616 (v3) / generic/112 (v2)
        // CRAWL. Honor the kernel's `length` directly; do NOT re-clamp against
        // our own `meta.size`. Under the FUSE writeback cache a deferred write
        // flush can race a truncate-extend and leave `meta.size` (what
        // get_file_size reports) lagging the kernel `i_size`, so clamping to it
        // would still return 0 for an off_in the kernel considers in-bounds
        // (reproduced live). A staged/inline file whose logical size outran its
        // physical data reads back short; treat that gap as a hole (zeros).
        if length == 0 {
            return Ok(ReplyCopyFileRange { copied: 0 });
        }

        // Bound per-call work/allocation; a short copy is legal and the caller
        // loops, so this keeps forward progress without an unbounded buffer.
        const CFR_MAX_CHUNK: u64 = 16 * 1024 * 1024;
        let effective_len = length.min(CFR_MAX_CHUNK) as usize;

        // O(chunk) source read — NEVER a whole-file materialization. The old
        // `read_file(&src_path)` assembled the entire source in RAM, which for
        // a huge sparse file (generic/285-scale: 8 GiB–16 TiB logical) meant
        // an O(logical size) allocation to serve a 64 KiB copy. The range
        // read serves interior holes as zeros at their correct positions;
        // `_src_backing` keeps any zero-copy mmap segment alive until the
        // chunk has been consumed below.
        let (src_data, _src_backing) = self
            .router
            .read_file_range_zero_copy(&src_path, off_in, effective_len as u32, None)
            .await
            .map_err(map_squeezefs_err)?;
        let phys = src_data.len();
        // `effective_len > 0` here (length > 0), so the chunk is always
        // non-empty regardless of the physical/logical gap.
        let chunk: bytes::Bytes = if phys >= effective_len {
            // Fully backed by physical data: zero-copy slice.
            src_data.slice(0..effective_len)
        } else {
            // Short read = the range runs past the physical tail into the
            // hole/EOF gap: assemble exactly effective_len bytes — real data
            // first, then zeros.
            let mut buf = vec![0u8; effective_len];
            buf[..phys].copy_from_slice(&src_data);
            bytes::Bytes::from(buf)
        };

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

        let copied_len = effective_len as u64;
        let new_dest_size = std::cmp::max(dest_size, off_out + copied_len);
        // Route the destination write the same way the WRITE handler does:
        // a STRIPED destination goes through `write_file_staged` — the one
        // striped write path — whose per-block RMW consumes the parked
        // `ActiveBlockBuf` overlays. `DataRouter::write_file`'s striped leg
        // RMWs from the map/read tiers only, so a copy into a block with a
        // parked partial write would base itself on stale bytes AND be
        // shadowed by the parked overlay on the next read/flush (the
        // generic/075.2 family). Inline/staged destinations keep the router
        // write (their authoritative bases live router-side, and layout
        // promotion happens there).
        let dest_is_striped = self
            .router
            .metadata_cache
            .get(&dest_path)
            .map(|m| m.file_type == "striped")
            .unwrap_or_else(|| {
                // Cold cache: classify by the freshest known size, exactly
                // like the WRITE handler's attr fallback.
                dest_size > self.router.block_size.load(Ordering::Relaxed)
            });
        if dest_is_striped {
            if new_dest_size > dest_size {
                self.router
                    .update_metadata_cache_size(&dest_path, new_dest_size)
                    .await;
            }
            self.write_file_staged(inode_out, off_out, chunk, dest_size, target_fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
        } else {
            self.router
                .write_file(&dest_path, off_out, chunk, target_fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
        }

        // Update destination size and times in metadata backend

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
        const PUNCH_HOLE: u32 = libc::FALLOC_FL_PUNCH_HOLE as u32;
        const KEEP_SIZE: u32 = libc::FALLOC_FL_KEEP_SIZE as u32;
        const ZERO_RANGE: u32 = libc::FALLOC_FL_ZERO_RANGE as u32;
        const COLLAPSE_RANGE: u32 = libc::FALLOC_FL_COLLAPSE_RANGE as u32;
        const INSERT_RANGE: u32 = libc::FALLOC_FL_INSERT_RANGE as u32;

        // COLLAPSE_RANGE / INSERT_RANGE shift file contents (not a hole op).
        // They are not implemented; reject them loudly so callers (and fsx)
        // fall back instead of silently corrupting via the extend path below.
        if mode & (COLLAPSE_RANGE | INSERT_RANGE) != 0 {
            return Err(Errno::from(libc::EOPNOTSUPP));
        }

        // PUNCH_HOLE / ZERO_RANGE: the range must read back as zeros (a hole).
        // PUNCH_HOLE always keeps the size; ZERO_RANGE may grow it when
        // KEEP_SIZE is clear. Zero the in-bounds portion durably, then extend.
        if mode & (PUNCH_HOLE | ZERO_RANGE) != 0 {
            if length == 0 {
                return Ok(());
            }
            // Lock order (P1-9): inode write guard (1) BEFORE the lease (2).
            let _guard = self.active_inode_locks.get_inode_lock(ino).write().await;
            let fencing_token = self
                .get_or_acquire_lease(ino)
                .await
                .map_err(map_squeezefs_err)?;

            // Zero the part that overlaps existing data (punch_hole_range clamps
            // to the current EOF); any region past EOF becomes a hole via the
            // size extension below and already reads zeros.
            self.punch_hole_range(ino, offset, length, fencing_token)
                .await
                .map_err(map_squeezefs_err)?;

            if mode & ZERO_RANGE != 0 && mode & KEEP_SIZE == 0 {
                let target_size = offset + length;
                self.extend_file_size(ino, target_size, fencing_token)
                    .await
                    .map_err(map_squeezefs_err)?;
            }
            return Ok(());
        }

        // Pre-allocation isn't strictly required to reserve physical space in our NVMe-oF backend volume
        // as blocks are sparse/dynamic by nature. We just update the size attribute if we are extending.
        if mode & libc::FALLOC_FL_KEEP_SIZE as u32 == 0 {
            // Serialize against writes/truncates (lock order 1) so the
            // freshest-size gate inside extend_file_size cannot race a
            // concurrent size change.
            let _guard = self.active_inode_locks.get_inode_lock(ino).write().await;
            let fencing_token = self.dlm.get_fencing_token_ino(ino);
            let target_size = offset + length;
            self.extend_file_size(ino, target_size, fencing_token)
                .await
                .map_err(map_squeezefs_err)?;
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
    // Read-only probe mount (no checkpoint task, nothing written): status
    // must be safe against a volume another process has live-mounted.
    let backend = crate::meta_backend::kv::backend::KvMetaBackend::open_probe(
        std::path::Path::new(meta_lv_path),
    )
    .await?;
    let val_opt = backend
        .getxattr(1, crate::meta_backend::kv::builder::FORMAT_CONFIG_XATTR)
        .await?;
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

/// Flush every listed staged active block of `ino` durably: one atomic
/// per-block unit each ([`flush_one_active_block`] — upload AND merge under
/// that block's `BLOCK_FLUSH_LOCKS`), streamed with bounded concurrency.
/// Distinct blocks merge independently (the §5.3 primitive serializes map
/// RMW under `INODE_META_LOCKS`); a missing staged source is a clean no-op.
///
/// Deliberately takes NO `active_inode_locks` guard: per-block atomicity
/// (hazard 1) and the layout-prune epoch (hazard 2) carry the correctness,
/// and callers reach here from under the inode WRITE guard (punch/truncate →
/// write_file_staged → enqueue_writeback full-queue fallback), where the old
/// batch-merge write().await self-deadlocked.
async fn flush_due_active_blocks_for_inode(
    ino: u64,
    block_indices: Vec<u32>,
    fencing_token: u64,
    router: &DataRouter,
    _dlm: &DlmClient,
    _active_inode_locks: &StripeLocks<tokio::sync::RwLock<()>, 4096>,
) -> Result<(), SqueezefsError> {
    use futures::stream::{self, StreamExt};

    let file_path = crate::keys::inode_path(ino);
    let meta = router.fetch_metadata(&file_path).await?;
    let is_striped = meta.file_type == "striped";

    let router_clone = router.clone();
    let mut flushes =
        stream::iter(block_indices.into_iter().map(move |block_idx| {
            let router = router_clone.clone();
            async move {
                flush_one_active_block(ino, block_idx, fencing_token, &router, is_striped).await
            }
        }))
        .buffer_unordered(8);

    while let Some(res) = flushes.next().await {
        res?;
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
    let (be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
    let offset = block_allocator.allocate_block().await?;
    if let Err(e) = nvme_writer.write_block(offset, processed_block).await {
        let _ = block_allocator.free_block(offset).await;
        return Err(e);
    }
    block_allocator.publish_block(offset);
    let stored_block_key = router.backend_router.persist_block_key(&be_id, offset);

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

/// Writeback-worker entry: the inode READ guard (fsync-vs-write ordering
/// courtesy) around the atomic per-block unit.
async fn flush_single_active_block(
    ino: u64,
    b: u32,
    fencing_token: u64,
    router: &DataRouter,
    _dlm: &DlmClient,
    active_inode_locks: &StripeLocks<tokio::sync::RwLock<()>, 4096>,
    is_striped: bool,
) -> Result<(), SqueezefsError> {
    let _inode_guard = active_inode_locks.get_inode_lock(ino).read().await;
    flush_one_active_block(ino, b, fencing_token, router, is_striped).await
}

/// ONE ATOMIC PER-BLOCK FLUSH UNIT: upload staged active block `b` and merge
/// it into the map. Two delayed-merge hazards force its shape (the
/// generic/075.2 stale-data / lost-write family):
///
/// 1. LOST UPDATE between concurrent flushes of the SAME block: reading the
///    source under the block lock but merging after dropping it lets two
///    flushes merge in reverse content order — the older upload's merge
///    displaces the newer key and the newest bytes silently vanish from the
///    map (observed live: writeback worker vs batch flush of one block).
///    Fix: hold the BLOCK lock across upload AND merge — the same discipline
///    as the write-through `upload_full_block` (block lock →
///    `INODE_META_LOCKS` is the established 3 → 3.5 extended order). The old
///    post-drop `active_inode_locks.write()` batch-merge acquisition is GONE:
///    it added no content protection and self-deadlocked when the flush was
///    reached from under the inode write guard (punch/truncate →
///    write_file_staged → enqueue_writeback full-queue fallback).
/// 2. PRUNE UNDO: a truncate/punch between capture and merge is silently
///    reverted by the merge (it re-inserts the pruned block with pre-prune
///    content). The layout-prune epoch is captured with the content and
///    revalidated inside the merge's critical section; on mismatch the
///    orphaned upload is freed and the flush re-captures — after a prune the
///    purged source is gone, so the retry no-ops.
async fn flush_one_active_block(
    ino: u64,
    b: u32,
    fencing_token: u64,
    router: &DataRouter,
    is_striped: bool,
) -> Result<(), SqueezefsError> {
    let cache_key = crate::keys::active_block(ino, b as u64).to_string();

    for _attempt in 0..8 {
        let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, b);
        let start_block_lock = std::time::Instant::now();
        let _block_guard = block_lock.lock().await;
        METRICS.block_lock_wait.record(start_block_lock.elapsed());

        let capture_epoch = crate::routing::layout_prune_epoch(ino);
        // Existence probe WITHOUT holding a shard guard across the meta-I/O
        // allocate below: the §5.5 DMA source is a staging-shard READ guard,
        // and a task suspended on `allocate_block().await` while holding it
        // parks every subsequent shard access behind parking_lot's queued-
        // writer fairness until the executor has no worker left to resume
        // this task — the observed total-wedge under the fsx-075 harness.
        // Allocate first (no guard), then take the source; the only await
        // under the guard is the DMA itself, whose request owns the guard.
        if router
            .cache
            .nvme
            .read_staged_zero_copy(&cache_key)
            .is_none()
        {
            return Ok(());
        }

        let (be_id, block_allocator, nvme_writer) = router.backend_router.get_active_backend()?;
        let offset = block_allocator.allocate_block().await?;

        let block_data_source = match router.cache.nvme.staged_dma_source(&cache_key) {
            Some(s) => s,
            None => {
                // Purged between probe and capture (truncate/punch/newer
                // write): nothing to flush.
                let _ = block_allocator.free_block(offset).await;
                return Ok(());
            }
        };

        // §5.5: guard-backed bytes never enter any cache — the promotion LRU
        // seed (not-yet-striped files only, cold path) is a bounded real copy
        // taken while the source is alive; striped flushes never put.
        let lru_copy = (!is_striped).then(|| block_data_source.detached_copy());

        // Consumes the source (crypto transform severs the guard pre-DMA;
        // passthrough DMAs straight off the staging mmap). The guard is provably
        // dead when this returns (normative §5.5 sequencing).
        if let Err(e) = crate::cache::nvme::write_block_from_staging(
            router.get_crypto(),
            &nvme_writer,
            offset,
            block_data_source,
        )
        .await
        {
            error!(
                "flush_one_active_block: Failed to upload block {} of inode {} to NVMe: {:?}",
                b, ino, e
            );
            let _ = block_allocator.free_block(offset).await;
            return Err(e);
        }
        block_allocator.publish_block(offset);

        let stored_block_key = router.backend_router.persist_block_key(&be_id, offset);

        // §5.3 one merge discipline: current-map RMW under INODE_META_LOCKS,
        // still under this block's lock (hazard 1). Free only the
        // displaced-from-current keys the primitive returns — the old
        // start-of-call `old_block_key` free was exactly the stale-snapshot
        // anti-pattern the routing merge comment forbids.
        let entries = [(b, stored_block_key.clone())];
        let Some(displaced) = router
            .merge_block_mappings_if_epoch(
                ino,
                crate::routing::BlockMapOp::Merge(&entries),
                0,
                crate::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                fencing_token,
                Some(capture_epoch),
            )
            .await?
        else {
            // A prune invalidated this capture (hazard 2): the uploaded
            // block is unreachable — free it and re-capture.
            let _ = block_allocator.free_block(offset).await;
            continue;
        };

        // Cache in RAM (bypass entirely if file is striped layout). Promotion
        // puts a detached copy — never the guard-backed staging bytes (§5.5).
        match lru_copy {
            Some(copy) => {
                // Put-owner of a possibly-reused key: the fresh put covers
                // the RAM tier; the NVMe read tier still needs the purge — a
                // validated fill of the key's dying incarnation may have
                // published there before our allocate, and it would serve
                // dead bytes once the RAM entry evicts (same
                // dead-incarnation shielding as `upload_full_block`, PR 6).
                router.cache.purge_block_key(&stored_block_key);
                router.cache.read_lru.put(&stored_block_key, copy);
            }
            None => {
                // No-put owner of a possibly-reused key: purge instead (same
                // dead-incarnation shielding as `upload_full_block`, PR 6).
                router.cache.purge_block_key(&stored_block_key);
            }
        }

        for bk in displaced {
            let _ = router.backend_router.free_block(&bk).await;
        }

        // ONLY remove active write block from cache if it hasn't been
        // modified by a newer write. spawn_blocking: the shard WRITE lock
        // must never park an async worker (see the probe comment above —
        // this exact remove was a parked frame in the observed wedge).
        {
            let nvme = router.cache.nvme.clone();
            let key = cache_key.clone();
            let token = fencing_token;
            tokio::task::spawn_blocking(move || {
                let current_token = nvme.get_staged_fencing_token(&key);
                if let Some(tok) = current_token {
                    if tok == token {
                        nvme.remove_active_block(&key);
                    }
                } else {
                    nvme.remove_active_block(&key);
                }
            })
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        }

        return Ok(());
    }
    Err(SqueezefsError::InvalidOperation(format!(
        "flush_one_active_block: layout-prune epoch kept moving for ino {ino} block {b}"
    )))
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

    /// Regular-file open/create replies must advertise
    /// FOPEN_PARALLEL_DIRECT_WRITES (kernel ABI bit 1 << 6, fuse ≥ 7.38 /
    /// Linux ≥ 6.2). Without it the kernel takes the inode lock EXCLUSIVE
    /// around every O_DIRECT write submission, so multi-threaded /
    /// iodepth>1 O_DIRECT writers to one file serialize in the kernel
    /// before the daemon ever sees a request (elbencho `-w -b 4k --iodepth
    /// 16 --direct`: 16 in-flight buys zero concurrency). Daemon-side
    /// correctness does not depend on that kernel lock: the write handler
    /// serializes per inode via `active_inode_locks` for meta-prep and
    /// per-block `BLOCK_FLUSH_LOCKS` for data merges, and concurrent
    /// overlapping O_DIRECT writes carry no POSIX atomicity guarantee.
    /// Extending writes stay kernel-exclusive regardless (fuse_dio_lock
    /// checks past-EOF), so size-extension ordering is unaffected.
    #[test]
    fn regular_open_reply_advertises_parallel_direct_writes() {
        assert_eq!(
            FOPEN_PARALLEL_DIRECT_WRITES,
            1 << 6,
            "kernel ABI value for FOPEN_PARALLEL_DIRECT_WRITES is 1 << 6 \
             (include/uapi/linux/fuse.h); any other value advertises a \
             different capability"
        );
        assert_eq!(
            regular_open_reply_flags(),
            FOPEN_PARALLEL_DIRECT_WRITES,
            "regular files must advertise exactly parallel direct writes \
             (no FOPEN_DIRECT_IO — the page-cache path stays enabled)"
        );
        assert_eq!(
            regular_open_reply_flags() & 1,
            0,
            "FOPEN_DIRECT_IO must stay reserved for the virtual \
             .stats/.config inodes"
        );
    }

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
