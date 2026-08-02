use crate::cache::{TieredCache, BUFFER_POOL};
use crate::dlm::DlmClient;

pub const MAX_INLINE_SIZE: usize = 4096;

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::{
    publish_phase_record, read_fill_phase_record, read_serve_phase_record, PublishPhase,
    ReadFillPhase, ReadServePhase, METRICS,
};
use crate::meta_backend::Metadata;
use crate::stripe_locks::StripeLocks;
use log::debug;
use std::sync::atomic::Ordering;

use std::time::Duration;
use uuid::Uuid;

/// Per-inode serialization for striped layout (`block_map`/size) mutation and
/// the `fetch_metadata` backend refill.
///
/// Concurrent striped writers (the kernel flushes a large file's dirty pages in
/// parallel) each COW their blocks to fresh keys and must merge them into the
/// file's block map. Two races corrupt data without this lock:
///  * **Lost update:** a non-atomic read-merge-save over a stale snapshot drops
///    the other writers' just-committed entries — a dropped entry reverts to a
///    freed key whose physical block is then reallocated, so the block reads as
///    zeros or, worse, as another block's data.
///  * **Stale refill:** `fetch_metadata`'s TTL refill reads a backend snapshot
///    and inserts it into `metadata_cache`; interleaved with a writer's commit
///    it can clobber the fresh entry with the stale map.
///
/// Held only for the short read→merge→save; block *data* I/O stays concurrent
/// (COW) outside the lock.
static INODE_META_LOCKS: once_cell::sync::Lazy<StripeLocks<tokio::sync::Mutex<()>, 4096>> =
    once_cell::sync::Lazy::new(StripeLocks::new);

/// Census-wrapped `INODE_META_LOCKS` acquisition (the D1.b named-holder
/// surface, VL10): every acquisition site routes here so a contended wait
/// on the block-map merge domain shows up in the watchdog's lock-wait
/// census with its inode named. Fast path = one `try_lock`.
async fn meta_lock_acquire(ino: u64) -> tokio::sync::MutexGuard<'static, ()> {
    crate::fuse_client::census_meta_lock_acquire(INODE_META_LOCKS.get_inode_lock(ino), ino).await
}

/// Per-inode (striped, collision-tolerant) LAYOUT-PRUNE EPOCH — a latch-free
/// monotonic counter bumped under `INODE_META_LOCKS` by every block-map
/// PRUNING merge (`TruncateFrom`, `RemoveBlocks`). The delayed-merge flush
/// paths (writeback worker / batch active-block flush) capture content OUTSIDE
/// the inode write guard and merge it later; without the epoch, a truncate or
/// punch that lands between their capture and their merge is silently undone —
/// the merge re-inserts the pruned block with pre-prune content (the
/// generic/075.2 stale-data resurrection race). Those paths capture the epoch
/// at content-capture time and merge via
/// [`DataRouter::merge_block_mappings_if_epoch`], which refuses (returns
/// `None`) when the epoch moved; the caller frees its orphaned upload and
/// retries against the post-prune state. Shard collisions only ever cause a
/// spurious retry, never a missed invalidation.
static LAYOUT_PRUNE_EPOCHS: once_cell::sync::Lazy<StripeLocks<std::sync::atomic::AtomicU64, 4096>> =
    once_cell::sync::Lazy::new(StripeLocks::new);

/// Current layout-prune epoch for `ino` (Acquire).
pub fn layout_prune_epoch(ino: u64) -> u64 {
    LAYOUT_PRUNE_EPOCHS
        .get_inode_lock(ino)
        .load(Ordering::Acquire)
}

/// W2 rider-record interval merge: fold `[start, start+data)` into the
/// ascending, pairwise-disjoint `extents` list, NEWEST WINS on overlap —
/// the record twin of `ExtentOverlay::merge`.
pub(crate) fn merge_extent_run(extents: &mut Vec<(u32, Vec<u8>)>, start: u32, data: &[u8]) {
    let end = start + data.len() as u32;
    let lo = extents.partition_point(|&(s, ref d)| (s + d.len() as u32) < start);
    let mut hi = lo;
    while hi < extents.len() && extents[hi].0 <= end {
        hi += 1;
    }
    if lo == hi {
        extents.insert(lo, (start, data.to_vec()));
        return;
    }
    let new_start = extents[lo].0.min(start);
    let new_end = (extents[hi - 1].0 + extents[hi - 1].1.len() as u32).max(end);
    let mut merged = vec![0u8; (new_end - new_start) as usize];
    for (s, d) in extents.drain(lo..hi) {
        let off = (s - new_start) as usize;
        merged[off..off + d.len()].copy_from_slice(&d);
    }
    let off = (start - new_start) as usize;
    merged[off..off + data.len()].copy_from_slice(data);
    extents.insert(lo, (new_start, merged));
}

pub fn parse_inode_from_path(path: &str) -> u64 {
    if path.starts_with("inode_") {
        path.strip_prefix("inode_")
            .unwrap()
            .parse::<u64>()
            .unwrap_or(0)
    } else {
        0
    }
}

/// PR K8 (design-cow-kv-metadata §5.3): headroom subtracted from a volume's
/// per-ino xattr value cap to derive the inline block-map ceiling
/// (`LAYOUT_INLINE_MAX = xattr_value_cap(ino) - LAYOUT_INLINE_HEADROOM`). It
/// covers the KV record framing (xattr name + envelope) so the persisted
/// `"layout"` value stays under the node layer's own record cap, with slack
/// for the non-block-map layout fields. 4 KiB at every knob setting.
const LAYOUT_INLINE_HEADROOM: usize = 4096;

/// The persisted `"layout"` xattr value — lives in [`crate::layout_wire`]
/// since the write-commit-economy campaign (the KV fold layer folds
/// layout delta records onto it); re-exported here so every historical
/// `routing::LayoutMetadata` path keeps working.
pub use crate::layout_wire::LayoutMetadata;

#[derive(Clone, Debug)]
pub struct CachedMetadata {
    /// Layout class (`inline`/`staged`/`striped`) — [`CompactString`]
    /// (op-economy campaign): every value inlines (≤ 24 B), so the
    /// per-op moka-get clone of this struct stops allocating (`String`
    /// here was a convicted warm-path allocation site).
    ///
    /// [`CompactString`]: compact_str::CompactString
    pub file_type: compact_str::CompactString,
    pub size: u64,
    /// `Arc<str>` (op-economy): clone = refcount bump, never a heap copy
    /// — this struct is handed out BY VALUE on every read.
    pub block_map_id: Option<std::sync::Arc<str>>,
    /// `Arc<str>` — see `block_map_id`.
    pub block_prefix: Option<std::sync::Arc<str>>,
    /// `Arc<str>` — see `block_map_id`.
    pub file_id: Option<std::sync::Arc<str>>,
    pub cached_at: std::time::Instant,
    /// Inline payload held zero-copy: `Bytes` clones are O(1) refcount bumps, so
    /// hot-path `metadata_cache` gets / `meta.clone()` don't deep-copy the file.
    pub data_key: Option<bytes::Bytes>,
    /// Striped block map, shared zero-copy (item A): `CachedMetadata` is
    /// handed out BY VALUE on every read (moka get + `fetch_metadata`), so
    /// the map rides an `Arc` — clone = refcount bump, not a per-op deep
    /// copy of every key String (measured 24% of daemon CPU on the cold
    /// rand-4k row as `HashMap::clone` + drop, plus the allocator traffic
    /// serving them). Writers publish copy-on-write: mutate through
    /// [`std::sync::Arc::make_mut`] (or build a fresh map) so held snapshots
    /// keep observing exactly the map they were taken with — pinned by
    /// `test_block_map_snapshot_independent_of_*`.
    pub block_map: Option<std::sync::Arc<std::collections::HashMap<u32, String>>>,
    /// When true, layout/size live only in RAM (+ staging mmap); must persist on fsync/release.
    pub layout_dirty: bool,
    /// Write-commit-economy (2026-07-30): the caller half of the layout
    /// delta eligibility ladder — how many delta records the persisted
    /// base currently folds under (0 = fresh inline bincode base, delta-
    /// eligible; [`LAYOUT_DELTA_CHAIN_INELIGIBLE`] = the persisted base
    /// is JSON/indirect/unknown, full-`Put` only). Set by
    /// `fetch_metadata_from_backend` (what the backend actually holds)
    /// and by every save's republish; the DEFAULT is ineligible so any
    /// synthesized entry conservatively re-bases with a full save.
    pub layout_delta_chain: u32,
    /// Rewrite-publish-drain Lever A (2026-08-01): the fencing era this
    /// entry's layout state is coherent with — stamped by
    /// `fetch_metadata_from_backend` (backend-true at fetch) and by every
    /// save's republish (just persisted). The coalesced publish pass may
    /// use a CLEAN cached entry as its RMW base ONLY while this equals
    /// the ino's CURRENT fencing token (every layout mutation republishes
    /// the cache under `INODE_META_LOCKS`, so a same-era clean entry is
    /// coherent by construction; a foreign era — lease lost/reacquired,
    /// possible cross-client mutation — refetches, the pre-campaign
    /// posture). `0` = unknown provenance, never serve as base.
    pub layout_base_token: u64,
}

/// [`CachedMetadata::layout_delta_chain`] sentinel: the persisted base
/// cannot fold a delta (JSON/indirect/unknown provenance).
pub const LAYOUT_DELTA_CHAIN_INELIGIBLE: u32 = u32::MAX;

/// The `fetch_metadata` serve gate, extracted (P2 per-op economy): a
/// DIRTY layout is the local authority (never re-validated — see the
/// `fetch_metadata` doc comment for the aged-fsx loss class), a clean
/// entry serves within its 1 s freshness horizon. Also gates the read
/// handler's snapshot hint in
/// [`DataRouter::read_file_range_zero_copy_with_meta`].
pub fn metadata_entry_fresh_or_dirty(entry: &CachedMetadata) -> bool {
    entry.layout_dirty || entry.cached_at.elapsed() < std::time::Duration::from_secs(1)
}

impl Default for CachedMetadata {
    fn default() -> Self {
        Self {
            file_type: "inline".into(),
            size: 0,
            block_map_id: None,
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
            block_map: None,
            layout_dirty: false,
            layout_delta_chain: LAYOUT_DELTA_CHAIN_INELIGIBLE,
            layout_base_token: 0,
        }
    }
}

/// The mutation shapes of [`DataRouter::merge_block_mappings`] (§5.3 "One
/// merge discipline") — inserts, removals, AND size-only snapshot-saves —
/// so truncate/fallocate share the serialization domain instead of racing
/// it.
pub enum BlockMapOp<'a> {
    /// Insert/overwrite entries: write-through, flush paths, routing
    /// striped merge (and the volume-lifecycle movers, PR VL4).
    /// `(block_idx, new_block_key)` pairs.
    ///
    /// `Merge(&[])` is the DEGENERATE, size-only case: no entries change,
    /// but the primitive still re-reads the CURRENT meta under
    /// `INODE_META_LOCKS` and saves size/map from that — which is exactly
    /// what makes the stale-snapshot whole-meta saves (truncate-grow,
    /// fallocate-extend) safe: they can no longer rewrite the block map
    /// "without mutating it".
    Merge(&'a [(u32, String)]),
    /// The VL4 mover publish (design-volume-lifecycle §5.1.5/§5.4):
    /// `(block_idx, expected_current, new_block_key)` — each entry merges
    /// ONLY if the map's current value for `block_idx` equals
    /// `expected_current`; anything else (a foreground write replaced the
    /// mapping, a truncate pruned it) is SKIPPED under the same
    /// `INODE_META_LOCKS` critical section — the FIND-M11-A supersession
    /// law applied to movers: stale ⇒ contractual no-op, never a clobber.
    /// Callers distinguish merged from skipped entries by the returned
    /// displaced list (a merged entry displaces exactly its
    /// `expected_current`).
    MergeExpected(&'a [(u32, String, String)]),
    /// Remove every block whose start offset ≥ `new_size`
    /// (truncate-shrink): the old `retain`-and-save re-expressed as a
    /// removal set on the same primitive; removed keys come back as the
    /// free list.
    TruncateFrom { new_size: u64 },
    /// Remove a specific set of whole block indices (hole punch): each
    /// removed index becomes a hole (read returns zeros) and its displaced
    /// key comes back as the free list. The logical size is untouched (a
    /// punch keeps the file size) — the `min_size` floor holds it in place.
    RemoveBlocks(&'a [u32]),
}

/// Layout-field policy for [`DataRouter::merge_block_mappings`] — an
/// EXPLICIT parameter, because the converted writers disagree today and a
/// silent "extracted-body default" would change the flush paths'
/// side-effects.
/// One queued block-publish op on an ino's publish conveyor (lever 1,
/// write-commit-economy 2026-07-30): the `Merge`-arm parameters of
/// [`DataRouter::merge_block_mappings`] plus the submitter's result
/// channel (per-op displaced keys / per-op error — same surface as the
/// direct call).
pub(crate) struct QueuedPublish {
    entries: Vec<(u32, String)>,
    min_size: u64,
    flip: LayoutFlip,
    fencing_token: u64,
    done: tokio::sync::oneshot::Sender<Result<Vec<String>>>,
    /// Publish decomposition (2026-08-01): the op's enqueue instant —
    /// `publish_phase_ns` queue_wait records at pass drain, total at
    /// terminal fan-out.
    enqueued_at: std::time::Instant,
}

/// `SQUEEZEFS_PUBLISH_COALESCE_MAX` cell: max block-publish ops drained
/// per conveyor pass. `<= 1` disables coalescing (the A/B lever — the
/// pre-campaign serialized per-op path). Env read once; runtime-settable
/// via [`set_publish_coalesce_override`] (tests/acceptance).
fn publish_coalesce_cell() -> &'static std::sync::atomic::AtomicI64 {
    static CELL: std::sync::OnceLock<std::sync::atomic::AtomicI64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_PUBLISH_COALESCE_MAX")
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|v| *v >= 0)
            .unwrap_or(64);
        std::sync::atomic::AtomicI64::new(v)
    })
}

/// Current publish-coalesce cap (see `publish_coalesce_cell`).
pub fn publish_coalesce_max() -> usize {
    publish_coalesce_cell().load(std::sync::atomic::Ordering::Relaxed) as usize
}

/// Set the coalesce cap override (`None` restores the env/default) —
/// the `set_depth_override` pattern.
pub fn set_publish_coalesce_override(v: Option<usize>) {
    let default = std::env::var("SQUEEZEFS_PUBLISH_COALESCE_MAX")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|x| *x >= 0)
        .unwrap_or(64);
    publish_coalesce_cell().store(
        v.map(|n| n as i64).unwrap_or(default),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN` cell: how many layout delta
/// records may fold on one persisted base before the next publish
/// re-bases with a full `Put` (bounds cold-read fold chains and the
/// journal-replay working set per key). `0` disables layout deltas
/// entirely (the lever-2 A/B lever). Default 64.
fn layout_delta_chain_cell() -> &'static std::sync::atomic::AtomicI64 {
    static CELL: std::sync::OnceLock<std::sync::atomic::AtomicI64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN")
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|v| *v >= 0)
            .unwrap_or(64);
        std::sync::atomic::AtomicI64::new(v)
    })
}

/// Current layout-delta chain cap (see `layout_delta_chain_cell`).
pub fn layout_delta_max_chain() -> u32 {
    layout_delta_chain_cell().load(std::sync::atomic::Ordering::Relaxed) as u32
}

/// Test seam (write-commit-economy lever 1; the
/// [`TEST_TIER_PUBLISH_DELAY_MS`] precedent): artificial delay, in
/// milliseconds, injected at the head of every publish-conveyor pass —
/// reproduces the field's ms-scale commit latency on µs-commit
/// sandboxes so the batching contract is deterministic. One relaxed
/// load per pass; zero-cost when unset.
pub static TEST_PUBLISH_PASS_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Set the chain-cap override (`None` restores the env/default).
pub fn set_layout_delta_chain_override(v: Option<u32>) {
    let default = std::env::var("SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|x| *x >= 0)
        .unwrap_or(64);
    layout_delta_chain_cell().store(
        v.map(|n| n as i64).unwrap_or(default),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX` cell (rewrite-publish-drain
/// Lever B, 2026-08-01): max delta-class layout saves aggregated into
/// ONE multi-ino commit by the per-volume layout-merge conveyor.
/// `0` (default) = derive from the volume's ring-admission batch cap
/// (`SQUEEZEFS_META_COMMIT_BATCH_TXS` — no new constant); `1` = the
/// pre-campaign per-save commit path verbatim (the A/B lever).
fn publish_commit_group_cell() -> &'static std::sync::atomic::AtomicI64 {
    static CELL: std::sync::OnceLock<std::sync::atomic::AtomicI64> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let v = std::env::var("SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX")
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|v| *v >= 0)
            .unwrap_or(0);
        std::sync::atomic::AtomicI64::new(v)
    })
}

/// Current publish-commit aggregation cap: `None` = derive from the
/// volume's batch cap, `Some(1)` = per-save commits (A/B), `Some(n)` =
/// explicit.
pub fn publish_commit_group_max() -> Option<usize> {
    match publish_commit_group_cell().load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        n => Some(n as usize),
    }
}

/// Set the aggregation-cap override (`None` restores the env/default) —
/// tests/acceptance.
pub fn set_publish_commit_group_override(v: Option<usize>) {
    let default = std::env::var("SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|x| *x >= 0)
        .unwrap_or(0);
    publish_commit_group_cell().store(
        v.map(|n| n as i64).unwrap_or(default),
        std::sync::atomic::Ordering::Relaxed,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutFlip {
    /// Flush-path merges (`flush_single_active_block`, `flush_due_…`,
    /// `upload_active_block_bytes`, write-through): force
    /// `file_type = "striped"` but PRESERVE `file_id` / `data_key` —
    /// today's exact field writes. Behavior-preserving by construction.
    ToStripedKeepStagedIdentity,
    /// Layout transitions (routing striped merge): `file_type = "striped"`
    /// AND clear `file_id` / `data_key`. Staged-identity release
    /// bookkeeping (`release_superseded_staged` — ring-entry/budget
    /// release) stays with the CALLER: the primitive never releases staged
    /// identity itself, so a clear is never paired with zero or two
    /// releases.
    ToStripedClearStagedIdentity,
    /// Truncate / fallocate / defrag mutations: leave `file_type`,
    /// `file_id`, `data_key` untouched (truncate's inline/staged handling
    /// stays in its caller — the primitive only owns the striped map +
    /// size).
    KeepLayout,
}

pub struct StorageBackend {
    pub device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
}

/// One `volume_states` row (design-volume-lifecycle §10): the per-volume
/// gauge object the stats inode and the admin `volume-list` verb publish.
#[derive(Clone, Debug)]
pub struct VolumeStateRow {
    pub id: String,
    pub backing_dev: String,
    /// Durable record state (`active`/`disabled`; VL4 adds
    /// draining/retired).
    pub state: String,
    /// Live health (the fail-stop override + device probe), distinct
    /// from the durable state lattice.
    pub healthy: bool,
    pub capacity_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

/// §5.9 balance-penalty slope (design-volume-lifecycle, KD-16): `k =
/// 1000` — 10 pp of fill above the set mean costs 100 health points.
pub const BALANCE_PENALTY_K: f64 = 1000.0;
/// §5.9 penalty cap: 300 of 1000, so balance NEVER outvotes health — a
/// genuinely degraded backend still loses to a merely-full one, and
/// failover semantics stay untouched (the health gates are absolute and
/// precede scoring).
pub const BALANCE_PENALTY_CAP: u32 = 300;

/// The §5.9 balance penalty: `clamp(k × (fill_ratio − set_mean_fill),
/// 0, 300)`. At or below the set mean the penalty is zero.
pub fn balance_penalty(fill_ratio: f64, set_mean_fill: f64) -> u32 {
    let raw = BALANCE_PENALTY_K * (fill_ratio - set_mean_fill);
    if raw.is_nan() || raw <= 0.0 {
        0
    } else {
        raw.min(f64::from(BALANCE_PENALTY_CAP)).round() as u32
    }
}

/// §5.9: `health_effective = device/health score − balance_penalty`,
/// saturating at zero. The KD-16 invariant is structural: the result
/// never drops more than [`BALANCE_PENALTY_CAP`] below `device_health`.
pub fn health_effective(device_health: u32, fill_ratio: f64, set_mean_fill: f64) -> u32 {
    device_health.saturating_sub(balance_penalty(fill_ratio, set_mean_fill))
}

/// One backend row of the [`PlacementTable`] snapshot (design-
/// volume-lifecycle §5.9/§10): the per-backend placement gauges plus the
/// Arcs a pick hands to the write path.
pub struct PlacementRow {
    pub id: String,
    /// `health_effective` at refresh time (`backend_placement_weight`).
    pub weight: u32,
    /// `used/capacity` from the allocator census (`backend_fill_ratio`).
    pub fill_ratio: f64,
    /// §5.4 placement eligibility at refresh time: healthy AND durable
    /// state `active`. Ineligible rows carry weight 0 and never join the
    /// band; they stay in the snapshot so the stats surface shows them.
    pub eligible: bool,
    /// Cumulative `backend_placement_picks` — Arc-shared with the router
    /// so the gauge survives table swaps.
    pub picks: std::sync::Arc<std::sync::atomic::AtomicU64>,
    allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
}

/// The §5.9 ArcSwap'd placement snapshot: an immutable per-backend row
/// vec plus the 90 %-of-max `health_effective` band and an atomic
/// round-robin cursor. Refreshed by the health worker on its cadence and
/// immediately on registration / volume-state / health-override /
/// retire transitions (plus the mover's retire path through those same
/// hooks) — NEVER on the per-write path: a pick is one ArcSwap load +
/// O(#backends) scan, zero locks, zero syscalls (the P1-11 pattern).
pub struct PlacementTable {
    pub rows: Vec<PlacementRow>,
    /// Indices into `rows`: eligible backends within 90 % of the max
    /// `health_effective`.
    band: Vec<usize>,
    /// Round-robin cursor within the band (reset on refresh — spread,
    /// not fairness bookkeeping).
    rr: std::sync::atomic::AtomicUsize,
    /// Set-level max−min fill ratio over eligible rows — the
    /// `backend_fill_spread` gauge, THE G-VL-8 instrument.
    pub fill_spread: f64,
}

impl PlacementTable {
    fn empty() -> Self {
        Self {
            rows: Vec::new(),
            band: Vec::new(),
            rr: std::sync::atomic::AtomicUsize::new(0),
            fill_spread: 0.0,
        }
    }

    /// The in-band backend ids (test/diagnostic surface).
    pub fn band_ids(&self) -> Vec<String> {
        self.band.iter().map(|&i| self.rows[i].id.clone()).collect()
    }

    /// The §5.9 hot-path pick: atomic round-robin within the band,
    /// skipping candidates the `healthy` gate refuses (the
    /// `unhealthy_backends` fail-stop mark stays authoritative and
    /// INSTANT even against a stale snapshot). Takes ONLY the snapshot —
    /// structurally no router, no filesystem, no syscalls.
    pub fn pick<F: Fn(&str) -> bool>(
        &self,
        healthy: F,
    ) -> Option<(
        String,
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        let n = self.band.len();
        if n == 0 {
            return None;
        }
        let start = self.rr.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            let row = &self.rows[self.band[start.wrapping_add(i) % n]];
            if healthy(&row.id) {
                row.picks.fetch_add(1, Ordering::Relaxed);
                return Some((row.id.clone(), row.allocator.clone(), row.device.clone()));
            }
        }
        None
    }
}

#[derive(Clone)]
pub struct BackendRouter {
    pub default_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub default_device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub backends: std::sync::Arc<
        dashmap::DashMap<String, std::sync::Arc<StorageBackend>, ahash::RandomState>,
    >,
    pub unhealthy_backends: std::sync::Arc<dashmap::DashMap<String, bool, ahash::RandomState>>,
    pub block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Read-tier purge hook, run inside every TERMINAL `free_block`
    /// (`begin_free` → purge → punch → `finish_free`). Closes the
    /// straggler-publish poison (the generic/074 fstest.3 stale-fill): a
    /// validated fill's detached publish can land AFTER the displacement
    /// purge yet PASS its incarnation after-check, because the key's
    /// incarnation retires only here — so the free itself must sweep the
    /// read tiers. Any publish that lands after this purge necessarily
    /// runs its after-check after the retire and undoes itself; any
    /// publish before it is removed here. Wired by `DataRouter::new`
    /// (`OnceCell`: the tiers outlive the router; bare routers in tests
    /// simply have no tiers to purge).
    read_tier_purge: once_cell::sync::OnceCell<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
    /// The durable data-volume records this router was registered from
    /// (KD-5; empty on bare routers). Lock-free snapshot swap: readers
    /// (stats, admin verbs) load; the mount wiring and the online
    /// `volume add-data` path store.
    volume_records: std::sync::Arc<arc_swap::ArcSwap<Vec<crate::DataVolumeRecord>>>,
    /// §5.9: the balance-aware write-placement snapshot (see
    /// [`PlacementTable`]). Hot-path reads are one ArcSwap load.
    placement_table: std::sync::Arc<arc_swap::ArcSwap<PlacementTable>>,
    /// Per-backend cumulative pick counters (`backend_placement_picks`)
    /// — kept outside the table so the gauges survive table swaps.
    placement_picks: std::sync::Arc<
        dashmap::DashMap<String, std::sync::Arc<std::sync::atomic::AtomicU64>, ahash::RandomState>,
    >,
    /// The background block-reclaim queue (async block-reclaim — the
    /// overwrite-throughput fix): terminal frees enqueue their device
    /// reclaim here instead of issuing it on the write path; the queue
    /// owns each entry's `finish_free`. Shared across router clones; the
    /// allocators' ENOSPC pressure valves drain it synchronously.
    reclaim: std::sync::Arc<crate::block_reclaim::ReclaimQueue>,
    /// Idea 4 — the discard-elision debt drainer (design-rewrite-program
    /// §3): elided terminal frees register their target here; the
    /// drainer owns the idle/pressure venues and the trim core
    /// (`trim_elided` is the fstrim/defrag face).
    debt: std::sync::Arc<crate::block_reclaim::DebtDrainer>,
}

#[cold]
#[inline(never)]
fn err_invalid_offset() -> crate::error::SqueezefsError {
    crate::error::SqueezefsError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Invalid block offset",
    ))
}

#[cold]
#[inline(never)]
fn err_backend_not_found(be_id: &str) -> crate::error::SqueezefsError {
    crate::error::SqueezefsError::InvalidOperation(format!(
        "Storage backend '{}' not found/offline",
        be_id
    ))
}

/// Best-effort device size probe (`fs::metadata`, then a seek-to-end for
/// device nodes, then a 100 GiB default). Refresh-cadence / `.config`
/// surface only — the per-write path never calls this (§5.9: the
/// per-write metadata/seek pair is retired).
fn probe_device_size_bytes(device_path: &str) -> u64 {
    let mut dev_size = 100 * 1024 * 1024 * 1024;
    if let Ok(metadata) = std::fs::metadata(device_path) {
        let len = metadata.len();
        if len > 0 {
            dev_size = len;
        } else if let Ok(mut file) = std::fs::File::open(device_path) {
            use std::io::Seek;
            if let Ok(len) = file.seek(std::io::SeekFrom::End(0)) {
                if len > 0 {
                    dev_size = len;
                }
            }
        }
    }
    dev_size
}

/// PR VL6b (design-volume-lifecycle §5.6a): the **quarantined-mapping
/// marker**. `fsck --repair --apply` replaces a mapping whose block failed
/// content verification (C7 scrub failure / unverifiable C2 lost) with
/// `damaged:<original-mapping>` — the read path serves an explicit **EIO**
/// (never fabricated zeros), the fsck census treats the reference as
/// quarantined (counted for refcount coherence, skipped by the scrub and
/// the lost checks), and the physical block stays in place for forensics
/// until the marker itself is removed (unlink / truncate / a displacing
/// overwrite frees the BASE key via `clean_block_key`'s prefix strip).
pub const DAMAGED_MAPPING_PREFIX: &str = "damaged:";

/// `true` ⇔ `mapping_str` is a §5.6a quarantined mapping.
/// How a TERMINAL block free reclaims its device range
/// (`.benchmarks/2026-07-27-shim-write-amplification.md`; contract pinned
/// in `tests/block_free_reclaim_tests.rs`).
///
/// The field conviction: `fallocate(PUNCH_HOLE)` on a RAW BLOCK DEVICE is
/// `blkdev_issue_zeroout` — a full block of Write-Zeroes WRITE bandwidth
/// per freed block, so every steady-state overwrite/delete stream paid
/// ~+1.0× device write amplification (field 1.85× at 98 % util; rig
/// 2.000× exactly, kernel and shim paths alike). Deallocate is what a
/// free means on a namespace: `BLKDISCARD` (NVMe DSM Deallocate — a
/// range command, no data payload, not write-bandwidth-accounted).
/// Regular-file backings keep `PUNCH_HOLE`: sparse-backing space reclaim
/// is host-FS metadata there (the original ENOSPC motivation), not I/O.
///
/// Correctness does not depend on freed ranges reading zeros: unmapped
/// blocks serve zeros from hole semantics (`hole_read_zeros_tests`), and
/// reused offsets are guarded by write-before-publish + the incarnation
/// seqlock (`reused_key_stale_fill_tests`) — so an unsupported/refused
/// discard is SKIPPED and counted, never degraded into a zeroing write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeReclaimOp {
    /// Regular file backing: `fallocate(PUNCH_HOLE | KEEP_SIZE)`.
    FilePunch,
    /// Block device backing: `ioctl(BLKDISCARD)`.
    BdevDiscard,
    /// Anything else: reclaim nothing (and never zero-write).
    Skip,
}

/// Classify a terminal free's reclaim op from the backing path's
/// `st_mode` (`std::os::unix::fs::MetadataExt::mode()`).
pub fn free_reclaim_op(mode: u32) -> FreeReclaimOp {
    match mode & libc::S_IFMT {
        libc::S_IFREG => FreeReclaimOp::FilePunch,
        libc::S_IFBLK => FreeReclaimOp::BdevDiscard,
        _ => FreeReclaimOp::Skip,
    }
}

pub fn is_damaged_mapping(mapping_str: &str) -> bool {
    mapping_str.starts_with(DAMAGED_MAPPING_PREFIX)
}

/// Strip a stored block-map value (`proto://offset:extra` or `offset:extra`)
/// down to the free-able key (`proto://offset` / `offset`) that
/// [`BackendRouter::free_blocks`] expects. The map stores per-block extra
/// (packed length / crypto framing) after the offset; the allocator only keys
/// on the offset. A `damaged:` quarantine marker strips to its BASE key so
/// frees/purges/recovery resolve the physical block it preserves.
/// `pub` since PR VL7: the defrag census tooling and its tests'
/// independent recomputation parse mappings through the ONE cleaner.
pub fn clean_block_key(bk: &str) -> String {
    let bk = bk.strip_prefix(DAMAGED_MAPPING_PREFIX).unwrap_or(bk);
    if let Some(pos) = bk.find("://") {
        let proto = &bk[..pos];
        let rest = &bk[pos + 3..];
        let offset = rest.split(':').next().unwrap_or(rest);
        format!("{}://{}", proto, offset)
    } else {
        bk.split(':').next().unwrap_or(bk).to_string()
    }
}

/// Byte length of the zeros served for a range read of a staged file whose
/// payload was lost by a crash (the D0 degrade contract): the requested
/// range clamped to the inode's size — identical bounds to a hole read.
fn lost_staged_range_len(meta_size: u64, offset: u64, size: u32) -> usize {
    (offset + size as u64).min(meta_size).saturating_sub(offset) as usize
}

/// On-disk header of the INDIRECT block map — the spill target for layout
/// maps whose serialized size exceeds the per-volume inline record cap
/// (§5.3): 8-byte magic + LE u32 version, then a bincode
/// `Vec<(u32, String)>` payload of `(block_idx, block_key)` entries carrying
/// the SAME backend-true key strings the inline map persists
/// ([`BackendRouter::persist_block_key`] output, verbatim — bare offset on
/// the default slot, `name://offset` elsewhere). The header exists so the
/// NEXT encoding change is a version bump, not a format break.
const INDIRECT_MAP_MAGIC: [u8; 8] = *b"SQFSIMAP";
const INDIRECT_MAP_VERSION: u32 = 1;
const INDIRECT_MAP_HEADER_LEN: usize = 12;

/// Serialize a block map for its indirect spill block (see
/// [`INDIRECT_MAP_MAGIC`]). Entries are sorted by block index for
/// deterministic on-disk bytes. The retired pre-versioned shape serialized
/// `Vec<(u32, u64)>` bare offsets, which lost the owning backend: on
/// multi-volume mounts every over-spill file's non-first-volume blocks were
/// read/freed from the wrong device after rehydrate (the 8380049 residual).
fn encode_indirect_block_map(
    block_map: &std::collections::HashMap<u32, String>,
) -> Result<Vec<u8>> {
    let mut entries: Vec<(u32, &str)> = block_map
        .iter()
        .map(|(&b, key)| (b, key.as_str()))
        .collect();
    entries.sort_unstable_by_key(|&(b, _)| b);
    let payload = bincode::serialize(&entries).map_err(|e| {
        SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Failed to serialize indirect block map: {:?}", e),
        ))
    })?;
    let mut out = Vec::with_capacity(INDIRECT_MAP_HEADER_LEN + payload.len());
    out.extend_from_slice(&INDIRECT_MAP_MAGIC);
    out.extend_from_slice(&INDIRECT_MAP_VERSION.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode an indirect block-map blob (see [`INDIRECT_MAP_MAGIC`]). Trailing
/// padding past the bincode payload is ignored (blobs are written
/// 4 KiB-aligned and read back whole-block). Anything without the versioned
/// header — notably the retired pre-versioned bare-offset `Vec<(u32, u64)>`
/// shape — fails LOUD with [`SqueezefsError::IndirectMapFormat`]: silently
/// rehydrating bare keys is exactly the wrong-device corruption this format
/// bump retired, and there is no fleet to stay compatible with (always
/// forward).
pub(crate) fn decode_indirect_block_map(raw: &[u8]) -> Result<Vec<(u32, String)>> {
    if raw.len() < INDIRECT_MAP_HEADER_LEN || raw[..8] != INDIRECT_MAP_MAGIC {
        return Err(SqueezefsError::IndirectMapFormat {
            detail: format!(
                "missing {} header; first bytes {:02x?}",
                String::from_utf8_lossy(&INDIRECT_MAP_MAGIC),
                &raw[..raw.len().min(16)]
            ),
        });
    }
    let version = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    if version != INDIRECT_MAP_VERSION {
        return Err(SqueezefsError::IndirectMapFormat {
            detail: format!(
                "unsupported version {version} (this build reads version {INDIRECT_MAP_VERSION})"
            ),
        });
    }
    bincode::deserialize::<Vec<(u32, String)>>(&raw[INDIRECT_MAP_HEADER_LEN..]).map_err(|e| {
        SqueezefsError::IndirectMapFormat {
            detail: format!("undecodable version-{version} payload: {e:?}"),
        }
    })
}

impl BackendRouter {
    pub fn new(
        default_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        default_device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let reclaim = crate::block_reclaim::ReclaimQueue::from_env();
        let debt = crate::block_reclaim::DebtDrainer::new(reclaim.clone());
        Self::wire_space_pressure_valve(&default_allocator, &reclaim);
        let router = Self {
            default_allocator,
            default_device,
            backends: std::sync::Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new())),
            unhealthy_backends: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            block_size,
            read_tier_purge: once_cell::sync::OnceCell::new(),
            volume_records: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new())),
            placement_table: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                PlacementTable::empty(),
            )),
            placement_picks: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            reclaim,
            debt,
        };
        // Seed the table so bare routers place without waiting for a
        // worker tick (construction-time probe, never per-write).
        router.refresh_placement_table();
        router
    }

    /// Wire an allocator's ENOSPC pressure valve to this router's reclaim
    /// queue (tests/async_block_reclaim_tests.rs contract 2): an
    /// allocation that would refuse for space force-drains the queued
    /// reclaims first — a full volume can never be wedged by
    /// lazily-queued space. The counter lives HERE so it means exactly
    /// "valve drains that reclaimed queued space" (0 except under real
    /// space pressure); explicit drains (`reclaim_drain`, unmount) never
    /// bump it, and neither does a pass that found the queue EMPTY —
    /// contract 8 (field ledger inversion, 2026-07-27): counting no-op
    /// passes turned a genuinely-full store into an unbounded
    /// `sync_drains` climb (one per failed allocation attempt, forever)
    /// that read as "the reclaimer is not keeping up" when there was
    /// nothing to reclaim at all.
    fn wire_space_pressure_valve(
        allocator: &std::sync::Arc<crate::block_allocator::BlockAllocator>,
        reclaim: &std::sync::Arc<crate::block_reclaim::ReclaimQueue>,
    ) {
        let rq = reclaim.clone();
        let rq_pending = reclaim.clone();
        allocator.set_space_pressure_valve(
            std::sync::Arc::new(move || {
                let rq = rq.clone();
                Box::pin(async move {
                    // Contract 6: a fenced daemon must not drain — the
                    // queued entries' finish_free belongs to the
                    // successor writer's recovery now; allocation just
                    // fails StorageFull (this daemon is dead until
                    // remount anyway).
                    if rq.fence_halted() {
                        return;
                    }
                    // Contract 2b (probe-up campaign): the drain runs
                    // OFF the caller's executor thread — only the
                    // allocating task awaits (see
                    // ReclaimQueue::drain_off_thread). Supply-wait in
                    // allocate_block makes this the ESCALATION path,
                    // not the steady state.
                    if rq.drain_off_thread().await > 0 {
                        METRICS
                            .block_free_reclaim_sync_drains
                            .fetch_add(1, Ordering::Relaxed);
                    }
                })
            }),
            std::sync::Arc::new(move || rq_pending.pending()),
        );
    }

    /// Wire the writer-guard fence probe into the reclaim queue
    /// (contract 6, `tests/async_block_reclaim_tests.rs`): called by
    /// `DataRouter::set_meta_backend` with a probe over the mount's meta
    /// set. Once any volume latches the D0 fail-stop `failed` state, the
    /// queue ceases all device reclaims permanently.
    pub fn set_reclaim_fence_signal(&self, sig: std::sync::Arc<dyn Fn() -> bool + Send + Sync>) {
        self.reclaim.set_fence_signal(sig);
    }

    /// Inject the reclaim manners' foreground device-activity signal
    /// (contracts 12–13, `tests/async_block_reclaim_tests.rs`).
    /// Production needs no wiring — the queue defaults to the METRICS
    /// device-plane sum.
    pub fn set_reclaim_foreground_signal(
        &self,
        sig: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
    ) {
        self.reclaim.set_foreground_signal(sig);
    }

    /// Drain the background reclaim queue to empty (blocking work runs on
    /// the blocking pool): unmount teardown and tests. Conservation face:
    /// after this returns, every previously-enqueued range has been
    /// reclaimed-or-consciously-skipped and `finish_free`d.
    pub async fn reclaim_drain(&self) {
        let q = self.reclaim.clone();
        if tokio::task::spawn_blocking(move || {
            let _ = q.drain_sync();
        })
        .await
        .is_err()
        {
            log::error!("reclaim_drain blocking task panicked");
        }
    }

    /// Wire the terminal-free read-tier purge (see the field doc). Called
    /// once by `DataRouter::new`; later calls are no-ops.
    pub fn set_read_tier_purge(&self, purge: std::sync::Arc<dyn Fn(&str) + Send + Sync>) {
        let _ = self.read_tier_purge.set(purge);
    }

    /// Publish the durable volume-record snapshot (mount wiring; the
    /// online add path swaps in the post-commit set).
    pub fn set_volume_records(&self, records: Vec<crate::DataVolumeRecord>) {
        self.volume_records.store(std::sync::Arc::new(records));
        self.refresh_placement_table();
    }

    /// The durable volume-record snapshot (empty on bare routers).
    pub fn volume_records(&self) -> std::sync::Arc<Vec<crate::DataVolumeRecord>> {
        self.volume_records.load_full()
    }

    /// The current [`PlacementTable`] snapshot (stats surface + tests).
    pub fn placement_snapshot(&self) -> std::sync::Arc<PlacementTable> {
        self.placement_table.load_full()
    }

    /// Rebuild and publish the §5.9 placement snapshot from the live
    /// census. Runs on the health-worker cadence and immediately on
    /// registration / volume-state / health-override / retire
    /// transitions — never on the per-write path (this is the ONLY
    /// syscall site of placement, and only for allocators without a
    /// capacity bound).
    pub fn refresh_placement_table(&self) {
        // Candidate policy unchanged: the `backend_0` default slot is a
        // candidate ONLY on bare routers (no named registrations).
        let mut cands: Vec<(
            String,
            std::sync::Arc<crate::block_allocator::BlockAllocator>,
            std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        )> = Vec::new();
        if self.backends.is_empty() {
            cands.push((
                "backend_0".to_string(),
                self.default_allocator.clone(),
                self.default_device.clone(),
            ));
        }
        for entry in self.backends.iter() {
            cands.push((
                entry.key().clone(),
                entry.value().block_allocator.clone(),
                entry.value().device.clone(),
            ));
        }

        // Census: fill = used/capacity (the same allocator numbers §5.2
        // uses); unbounded allocators fall back to a device-size probe —
        // at refresh cadence, not per write.
        struct Census {
            id: String,
            allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
            device: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
            eligible: bool,
            capacity: u64,
            used: u64,
        }
        let census: Vec<Census> = cands
            .into_iter()
            .map(|(id, allocator, device)| {
                let eligible = self.placement_eligible(&id);
                let mut capacity = allocator.capacity_bytes();
                if capacity == 0 {
                    capacity = probe_device_size_bytes(&device.device_path);
                }
                let used = allocator
                    .get_used_blocks()
                    .saturating_mul(allocator.chunk_size());
                Census {
                    id,
                    allocator,
                    device,
                    eligible,
                    capacity,
                    used,
                }
            })
            .collect();

        // set_mean = Σused/Σcapacity over the placing (eligible) set.
        let (sum_used, sum_cap) = census
            .iter()
            .filter(|c| c.eligible && c.capacity > 0)
            .fold((0u64, 0u64), |(u, c), m| {
                (u.saturating_add(m.used), c.saturating_add(m.capacity))
            });
        let set_mean = if sum_cap > 0 {
            sum_used as f64 / sum_cap as f64
        } else {
            0.0
        };

        let rows: Vec<PlacementRow> = census
            .into_iter()
            .map(|c| {
                let fill_ratio = if c.capacity > 0 {
                    (c.used as f64 / c.capacity as f64).min(1.0)
                } else {
                    0.0
                };
                // The device/health score: the free-fraction × 1000 the
                // write path has always ranked on, census-sourced.
                let device_health = ((1.0 - fill_ratio).max(0.0) * 1000.0) as u32;
                let weight = if c.eligible {
                    health_effective(device_health, fill_ratio, set_mean)
                } else {
                    0
                };
                let picks = self
                    .placement_picks
                    .entry(c.id.clone())
                    .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)))
                    .clone();
                PlacementRow {
                    id: c.id,
                    weight,
                    fill_ratio,
                    eligible: c.eligible,
                    picks,
                    allocator: c.allocator,
                    device: c.device,
                }
            })
            .collect();

        // The 90 %-of-max band over eligible rows (a zero max keeps every
        // eligible row in the band — a full-but-healthy set still places,
        // exactly like the retired per-write sort did).
        let max_weight = rows
            .iter()
            .filter(|r| r.eligible)
            .map(|r| r.weight)
            .max()
            .unwrap_or(0);
        let cutoff = (max_weight * 9) / 10;
        let band: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.eligible && r.weight >= cutoff)
            .map(|(i, _)| i)
            .collect();

        let (min_fill, max_fill) = rows
            .iter()
            .filter(|r| r.eligible)
            .fold((f64::MAX, f64::MIN), |(lo, hi), r| {
                (lo.min(r.fill_ratio), hi.max(r.fill_ratio))
            });
        let fill_spread = if max_fill > min_fill {
            max_fill - min_fill
        } else {
            0.0
        };

        self.placement_table
            .store(std::sync::Arc::new(PlacementTable {
                rows,
                band,
                rr: std::sync::atomic::AtomicUsize::new(0),
                fill_spread,
            }));
        METRICS
            .placement_table_refreshes
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Build a [`StorageBackend`] for one durable volume record — THE
    /// shared construction path for mount-time registration and the
    /// online `volume add-data` (design-volume-lifecycle §5.3 step 3):
    /// device handle, per-volume allocator, capacity bound. When
    /// `backing_dev` IS the router's default device the default-slot
    /// Arcs are reused, which is what keeps `persist_block_key`'s
    /// first-volume bare-key invariant true (unprefixed on-disk keys stay
    /// byte-identical).
    ///
    /// Building does NOT enable placement: the backend joins routing only
    /// at [`Self::publish_backend`] — the §5.3 step-4 durability order
    /// ("write-path selection of the new backend is enabled last").
    pub async fn build_backend(
        &self,
        id: &str,
        backing_dev: &str,
        meta_client: std::sync::Arc<crate::dlm::MetaClient>,
    ) -> Result<std::sync::Arc<StorageBackend>> {
        if backing_dev == self.default_device.device_path {
            return Ok(std::sync::Arc::new(StorageBackend {
                device: self.default_device.clone(),
                block_allocator: self.default_allocator.clone(),
            }));
        }
        let device = std::sync::Arc::new(crate::nvme_dev::NvmeBlockDev::new(backing_dev));
        let allocator = std::sync::Arc::new(
            crate::block_allocator::BlockAllocator::new(meta_client, id).await?,
        );
        Self::wire_space_pressure_valve(&allocator, &self.reclaim);
        match crate::nvme_dev::device_capacity_bytes(backing_dev) {
            Ok(cap) => allocator.set_capacity_bytes(cap),
            Err(e) => {
                log::warn!("could not size data volume {backing_dev}: {e}; allocator unbounded")
            }
        }
        Ok(std::sync::Arc::new(StorageBackend {
            device,
            block_allocator: allocator,
        }))
    }

    /// Insert a built backend into the routing set under its durable id:
    /// write placement, key resolution, and health-worker coverage start
    /// here. Duplicate ids and the reserved `backend_0` alias refuse.
    pub fn publish_backend(&self, id: &str, backend: std::sync::Arc<StorageBackend>) -> Result<()> {
        if id == "backend_0" {
            return Err(SqueezefsError::InvalidOperation(
                "'backend_0' is the reserved legacy key-resolution alias, not a volume id"
                    .to_string(),
            ));
        }
        match self.backends.entry(id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(SqueezefsError::InvalidOperation(
                format!("data volume '{id}' is already registered"),
            )),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(backend);
                self.refresh_placement_table();
                Ok(())
            }
        }
    }

    /// Register one durable volume record: [`Self::build_backend`] +
    /// [`Self::publish_backend`] + the record's durable `disabled` state
    /// applied as a health override. The mount loop and test fixtures
    /// call this per record — one registration path (§5.3 step 3).
    pub async fn register_backend(
        &self,
        record: &crate::DataVolumeRecord,
        meta_client: std::sync::Arc<crate::dlm::MetaClient>,
    ) -> Result<std::sync::Arc<StorageBackend>> {
        let backend = self
            .build_backend(&record.id, &record.backing_dev, meta_client)
            .await?;
        self.publish_backend(&record.id, backend.clone())?;
        if record.state == crate::VOL_STATE_DISABLED {
            self.unhealthy_backends.insert(record.id.clone(), true);
            self.refresh_placement_table();
        }
        Ok(backend)
    }

    /// The fail-stop health override (`config data-volume
    /// enable/disable`), re-homed from the retired `/dev/shm` runtime
    /// config: `disabled` routes NEW placements away; blocks already on
    /// the volume read `EIO` until re-enabled — it is NOT an evacuation
    /// (that is `volume remove-data`, PR VL4). Unknown ids refuse — and
    /// the reserved `backend_0` alias can never be poisoned (the phantom
    /// stale-status foot-gun, retired for good).
    pub fn set_health_override(&self, id: &str, disabled: bool) -> Result<()> {
        if !self.backends.contains_key(id) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "unknown data volume '{id}' (see `squeezefs volume list`; 'backend_0' is a \
                 reserved key-resolution alias, not a volume)"
            )));
        }
        if disabled {
            self.unhealthy_backends.insert(id.to_string(), true);
        } else {
            self.unhealthy_backends.remove(id);
        }
        self.refresh_placement_table();
        Ok(())
    }

    /// Durable-state lookup for a volume id from the record snapshot.
    /// `None` = no record (bare routers / `--data-lv` overrides), which
    /// callers treat as `active`.
    pub fn volume_state(&self, id: &str) -> Option<String> {
        self.volume_records
            .load()
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.state.clone())
    }

    /// The state of the volume that OWNS a parsed block key's backend id,
    /// resolving the reserved `backend_0` default-slot alias to the record
    /// whose registered Arcs ARE the default slot (the first volume).
    pub fn volume_state_for_key_backend(&self, be_id: &str) -> Option<String> {
        if be_id != "backend_0" {
            return self.volume_state(be_id);
        }
        for rec in self.volume_records.load().iter() {
            if let Some(be) = self.backends.get(&rec.id) {
                if std::sync::Arc::ptr_eq(&be.device, &self.default_device)
                    && std::sync::Arc::ptr_eq(&be.block_allocator, &self.default_allocator)
                {
                    return Some(rec.state.clone());
                }
            }
        }
        None
    }

    /// §5.4: write-placement eligibility — healthy AND durable state
    /// `active`. `Draining`/`Retired`/`disabled` volumes take no NEW
    /// placements (draining still serves reads/refcounts normally; the
    /// health gates stay the separate fail-stop override).
    pub(crate) fn placement_eligible(&self, be_id: &str) -> bool {
        if !self.is_backend_healthy(be_id) {
            return false;
        }
        match self.volume_state(be_id) {
            None => true, // recordless registration (bare routers/tests)
            Some(state) => state == crate::VOL_STATE_ACTIVE,
        }
    }

    /// Flip one volume record's durable-state SNAPSHOT (the runtime half
    /// of the §5.4 state machine — callers commit the durable record
    /// through the meta backend and then publish here). Refuses unknown
    /// ids and states outside the lattice.
    pub fn set_volume_state(&self, id: &str, state: &str) -> Result<()> {
        if ![
            crate::VOL_STATE_ACTIVE,
            crate::VOL_STATE_DISABLED,
            crate::VOL_STATE_DRAINING,
            crate::VOL_STATE_RETIRED,
        ]
        .contains(&state)
        {
            return Err(SqueezefsError::InvalidOperation(format!(
                "unknown volume state '{state}' (active|disabled|draining|retired)"
            )));
        }
        let records = self.volume_records.load_full();
        let mut updated = (*records).clone();
        let rec = updated.iter_mut().find(|r| r.id == id).ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "unknown data volume '{id}' (see `squeezefs volume list`)"
            ))
        })?;
        rec.state = state.to_string();
        self.volume_records.store(std::sync::Arc::new(updated));
        self.refresh_placement_table();
        Ok(())
    }

    /// §5.4 retire: deregister the runtime backend — reads of a straggler
    /// key now fail loud (`err_backend_not_found`). The record itself is
    /// kept forever (KD-5); callers flip it to `retired` first.
    pub fn retire_backend(&self, id: &str) -> Result<()> {
        if self.backends.remove(id).is_none() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "unknown data volume '{id}' — nothing to retire"
            )));
        }
        self.unhealthy_backends.remove(id);
        self.refresh_placement_table();
        Ok(())
    }

    /// The `volume_states` gauge rows (design-volume-lifecycle §10):
    /// durable records joined with live allocator accounting. Volumes
    /// registered without records (bare routers, `--data-lv` overrides)
    /// synthesize `active` rows so the table always covers the routing
    /// set.
    pub fn volume_states(&self) -> Vec<VolumeStateRow> {
        let records = self.volume_records.load_full();
        let mut rows = Vec::new();
        let mut covered = std::collections::HashSet::new();
        let row_for = |id: &str, backing_dev: &str, state: &str| -> VolumeStateRow {
            let (capacity, used) = self
                .backends
                .get(id)
                .map(|be| {
                    let alloc = &be.value().block_allocator;
                    (
                        alloc.capacity_bytes(),
                        alloc.get_used_blocks().saturating_mul(alloc.chunk_size()),
                    )
                })
                .unwrap_or((0, 0));
            VolumeStateRow {
                id: id.to_string(),
                backing_dev: backing_dev.to_string(),
                state: state.to_string(),
                healthy: self.is_backend_healthy(id),
                capacity_bytes: capacity,
                used_bytes: used,
                free_bytes: capacity.saturating_sub(used),
            }
        };
        for rec in records.iter() {
            covered.insert(rec.id.clone());
            rows.push(row_for(&rec.id, &rec.backing_dev, &rec.state));
        }
        for entry in self.backends.iter() {
            if !covered.contains(entry.key()) {
                let dev_path = entry.value().device.device_path.clone();
                rows.push(row_for(entry.key(), &dev_path, crate::VOL_STATE_ACTIVE));
            }
        }
        rows
    }

    /// Bytes currently allocated on the striped block backends, summed
    /// over every distinct allocator (the default allocator is usually
    /// also registered in `backends` under its volume name — dedup by
    /// allocator identity so it counts once). Served entirely from the
    /// allocators' maintained in-RAM state (monotonic high-water atomic
    /// minus the recycled-free set) — no metadata transactions, no device
    /// I/O — so it is safe on the statfs hot path.
    pub fn allocated_bytes(&self) -> u64 {
        let mut seen: Vec<*const crate::block_allocator::BlockAllocator> = Vec::new();
        let mut sum = 0u64;
        let default_ptr = std::sync::Arc::as_ptr(&self.default_allocator);
        seen.push(default_ptr);
        sum += self
            .default_allocator
            .get_used_blocks()
            .saturating_mul(self.default_allocator.chunk_size());
        for entry in self.backends.iter() {
            let alloc = &entry.value().block_allocator;
            let ptr = std::sync::Arc::as_ptr(alloc);
            if !seen.contains(&ptr) {
                seen.push(ptr);
                sum += alloc.get_used_blocks().saturating_mul(alloc.chunk_size());
            }
        }
        sum
    }

    /// Per-op backend health gate: the explicit `unhealthy_backends` mark
    /// (the real health state machine) is authoritative and instant; the
    /// device-node liveness probe behind it is TTL-cached on the device
    /// (L3 statx residual — a bare `Path::exists()` here was 0.76
    /// statx/op on the rand-4k charter workload; the I/O path fails loud
    /// on a vanished node inside the TTL window).
    pub fn is_backend_healthy(&self, be_id: &str) -> bool {
        if self.unhealthy_backends.contains_key(be_id) {
            false
        } else if be_id == "backend_0" {
            self.default_device.node_exists_cached()
        } else if let Some(be) = self.backends.get(be_id) {
            be.device.node_exists_cached()
        } else {
            false
        }
    }

    /// The `.config` data-volume health score (free-fraction × 1000 with
    /// a device-size probe). Diagnostic surface ONLY — write placement
    /// reads the §5.9 [`PlacementTable`] weights; this fn is never on the
    /// per-write path.
    pub fn get_backend_health(&self, be_id: &str) -> u32 {
        if !self.is_backend_healthy(be_id) {
            return 0;
        }

        let (allocator, device_path) = if be_id == "backend_0" {
            (
                self.default_allocator.clone(),
                self.default_device.device_path.clone(),
            )
        } else if let Some(be) = self.backends.get(be_id) {
            (be.block_allocator.clone(), be.device.device_path.clone())
        } else {
            return 0;
        };

        let dev_size = probe_device_size_bytes(&device_path);
        let total_blocks = (dev_size / (4 * 1024 * 1024)).max(1);
        let used_blocks = allocator.get_used_blocks();
        let free_blocks = total_blocks.saturating_sub(used_blocks);
        let free_factor = free_blocks as f64 / total_blocks as f64;

        let perf_factor = 1.0;

        let score = (free_factor * 1000.0 * perf_factor) as u32;
        score.min(1000)
    }

    /// §5.9 write placement: one ArcSwap load + an atomic round-robin
    /// pick inside the 90 %-of-max `health_effective` band — zero locks,
    /// zero syscalls (the retired implementation paid a per-write DashMap
    /// scan + sort + `fs::metadata`+seek pair). The candidate policy is
    /// unchanged (the `backend_0` default slot only on bare routers —
    /// see [`Self::refresh_placement_table`]); the `unhealthy_backends`
    /// fail-stop mark stays an absolute, instant gate via the pick's
    /// health closure. An empty/expired band rebuilds ONCE (recoveries
    /// don't wait a worker tick) and then fails with the same loud "no
    /// healthy backends" error as ever.
    /// The snapshot-currency check the pick path runs (RAM-only, no
    /// syscalls): a table built for a different candidate population —
    /// direct registrations without the publish hook (test fixtures),
    /// or the bare-router → named-volume transition where the phantom
    /// `backend_0` row must vanish — forces a rebuild before serving.
    fn placement_table_current(&self, table: &PlacementTable) -> bool {
        let named = self.backends.len();
        if named == 0 {
            table.rows.len() == 1 && table.rows[0].id == "backend_0"
        } else {
            table.rows.len() == named && !table.rows.iter().any(|r| r.id == "backend_0")
        }
    }

    pub fn get_active_backend(
        &self,
    ) -> Result<(
        String,
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        let table = self.placement_table.load();
        if self.placement_table_current(&table) {
            if let Some(sel) = table.pick(|id| self.is_backend_healthy(id)) {
                return Ok(sel);
            }
        }
        // Degenerate path (never taken by healthy steady state): the
        // candidate set changed under the table, the band is empty, or
        // every banded candidate is gated unhealthy — rebuild once so
        // registrations/recoveries serve immediately.
        self.refresh_placement_table();
        if let Some(sel) = self
            .placement_table
            .load()
            .pick(|id| self.is_backend_healthy(id))
        {
            return Ok(sel);
        }
        Err(crate::error::SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "No healthy storage backends available for write",
        )))
    }

    /// The VL4 mover's destination pick (design-volume-lifecycle
    /// §5.4/§5.7): the LOWEST-fill placement-eligible backend, excluding
    /// `exclude` (the move source). Choosing the emptiest survivor both
    /// spreads a drain and drives the rebalance objective toward the set
    /// mean.
    pub fn pick_fill_destination(
        &self,
        exclude: &str,
    ) -> Result<(
        String,
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        let mut best: Option<(
            String,
            std::sync::Arc<crate::block_allocator::BlockAllocator>,
            std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
            f64,
        )> = None;
        for entry in self.backends.iter() {
            let be_id = entry.key();
            if be_id == exclude || !self.placement_eligible(be_id) {
                continue;
            }
            let alloc = &entry.value().block_allocator;
            let capacity = alloc.capacity_bytes();
            let used = alloc.get_used_blocks().saturating_mul(alloc.chunk_size());
            let fill = if capacity > 0 {
                used as f64 / capacity as f64
            } else {
                // Unbounded allocators (offline tools) sort by used bytes.
                used as f64
            };
            if best.as_ref().is_none_or(|(_, _, _, f)| fill < *f) {
                best = Some((
                    be_id.clone(),
                    alloc.clone(),
                    entry.value().device.clone(),
                    fill,
                ));
            }
        }
        match best {
            Some((id, alloc, dev, _)) => Ok((id, alloc, dev)),
            None => Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!("no placement-eligible destination backend (excluding '{exclude}')"),
            ))),
        }
    }

    pub fn get_backend(
        &self,
        be_id: &str,
    ) -> Result<(
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        if !self.is_backend_healthy(be_id) {
            return Err(err_backend_not_found(be_id));
        }
        if be_id == "backend_0" {
            Ok((self.default_allocator.clone(), self.default_device.clone()))
        } else if let Some(be) = self.backends.get(be_id) {
            Ok((be.block_allocator.clone(), be.device.clone()))
        } else {
            Err(err_backend_not_found(be_id))
        }
    }

    pub fn parse_block_key(&self, block_key: &str) -> Result<(String, u64)> {
        let parts: Vec<&str> = block_key.split("://").collect();
        let (be_id, offset_str) = if parts.len() > 1 {
            (parts[0], parts[1])
        } else {
            ("backend_0", block_key)
        };
        let offset = offset_str
            .parse::<u64>()
            .map_err(|_| err_invalid_offset())?;
        Ok((be_id.to_string(), offset))
    }

    /// The key string to PERSIST for a block just written at `offset` on the
    /// backend `get_active_backend`/`get_backend` selected as `be_id`.
    ///
    /// Invariant: a persisted key must resolve — through
    /// [`Self::parse_block_key`], now and after remount — to the same
    /// device/allocator pair the bytes were written on. An unprefixed key
    /// resolves to the DEFAULT slot (the `backend_0` legacy alias), so it is
    /// only correct when the selected backend IS the default slot: the
    /// bare-router `backend_0` id itself, or a named registration of the
    /// first volume (main.rs registers the first volume's device/allocator
    /// Arcs both as the default slot and under the real name). Those cases
    /// keep today's on-disk naming — single-volume volumes stay byte-
    /// identical with their historical unprefixed keys. Every other named
    /// backend gets an explicit `name://offset` key; persisting a bare key
    /// for those was the multi-volume wrong-device read/free bug.
    pub fn persist_block_key(&self, be_id: &str, offset: u64) -> String {
        if be_id == "backend_0" {
            return offset.to_string();
        }
        if let Some(be) = self.backends.get(be_id) {
            if std::sync::Arc::ptr_eq(&be.device, &self.default_device)
                && std::sync::Arc::ptr_eq(&be.block_allocator, &self.default_allocator)
            {
                return offset.to_string();
            }
        }
        format!("{}://{}", be_id, offset)
    }

    pub fn parse_block_offset(&self, block_key: &str) -> Result<u64> {
        let (_, offset) = self.parse_block_key(block_key)?;
        Ok(offset)
    }

    pub async fn read_block(&self, block_key: &str, size: usize) -> Result<bytes::Bytes> {
        self.read_block_with_dest(block_key, size, None).await
    }

    /// R3 (§5.6) ranged device leg: read `len` bytes at `rel_start` WITHIN
    /// the block behind `block_key` — `read_block_with_dest` at
    /// `offset + rel_start` (the uring worker already takes arbitrary
    /// offsets). The device fd is O_DIRECT, so callers pass a window
    /// rounded outward to the conservative 4096-byte LBA (approved OQ #1:
    /// no per-device probe until `ranged_read_unaligned_bounces` shows
    /// real 512-native waste) and, on the zero-copy leg, a 4 KiB-aligned
    /// registered dest. Raw bytes only — no decode, no cache publish, no
    /// single-flight: the caller (`get_block_range_for_index`) owns the
    /// full fill discipline.
    pub async fn read_block_range(
        &self,
        block_key: &str,
        rel_start: u64,
        len: usize,
        dest_addr: Option<u64>,
    ) -> Result<bytes::Bytes> {
        debug_assert_eq!(
            rel_start % 4096,
            0,
            "ranged window start must be LBA-aligned"
        );
        debug_assert_eq!(len % 4096, 0, "ranged window length must be LBA-aligned");
        debug_assert!(
            dest_addr.is_none_or(|d| d % 4096 == 0),
            "ranged O_DIRECT dest must be 4 KiB-aligned"
        );
        let (be_id, offset) = self.parse_block_key(block_key)?;

        if !self.is_backend_healthy(&be_id) {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("Storage volume '{}' is disabled/offline", be_id),
            )));
        }

        if be_id == "backend_0" {
            self.default_device
                .read_block_with_dest(offset + rel_start, len, dest_addr)
                .await
        } else if let Some(be) = self.backends.get(&be_id) {
            be.device
                .read_block_with_dest(offset + rel_start, len, dest_addr)
                .await
        } else {
            Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "Storage backend '{}' not found",
                be_id
            )))
        }
    }

    pub async fn read_block_with_dest(
        &self,
        block_key: &str,
        size: usize,
        dest_addr: Option<u64>,
    ) -> Result<bytes::Bytes> {
        let (be_id, offset) = self.parse_block_key(block_key)?;

        if !self.is_backend_healthy(&be_id) {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("Storage volume '{}' is disabled/offline", be_id),
            )));
        }

        if be_id == "backend_0" {
            self.default_device
                .read_block_with_dest(offset, size, dest_addr)
                .await
        } else if let Some(be) = self.backends.get(&be_id) {
            be.device
                .read_block_with_dest(offset, size, dest_addr)
                .await
        } else {
            Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "Storage backend '{}' not found",
                be_id
            )))
        }
    }

    /// Take one reference on the block behind `block_key`. `false` = the
    /// reference was NOT taken (freed/untracked offset, unknown backend, or
    /// unparsable key) — the caller must re-resolve, never proceed unpinned.
    #[must_use]
    pub fn increment_refcount(&self, block_key: &str) -> bool {
        // Decoration-tolerant (`bk:off:len` size-carrying mappings — see
        // `parse_block_mapping`): the refcount belongs to the BASE block.
        let cleaned = clean_block_key(block_key);
        let block_key: &str = &cleaned;
        if let Ok((be_id, offset)) = self.parse_block_key(block_key) {
            if be_id == "backend_0" {
                self.default_allocator.increment_refcount(offset)
            } else if let Some(be) = self.backends.get(&be_id) {
                be.block_allocator.increment_refcount(offset)
            } else {
                false
            }
        } else {
            false
        }
    }

    /// The allocator refcount behind a (possibly decorated) block key —
    /// `None` = the offset is not allocator-tracked (freed, or a mapping
    /// class the recovery walk does not account). The VL4 mover uses it
    /// to split "freed concurrently" from "untracked" on a refused pin.
    pub(crate) fn block_refcount(&self, block_key: &str) -> Option<u32> {
        let (alloc, offset) = self.allocator_for_key(block_key)?;
        alloc.refcount(offset)
    }

    /// [`Self::increment_refcount`] with the §5.1 **validate-after-pin**
    /// step (design-random-small-writes; the clone side of the clone/patch
    /// fence): pin, `fence(SeqCst)`, snapshot the incarnation word.
    /// `PinnedUnstable` = the reference WAS taken but a patch may be
    /// mid-flight — the caller must unpin, refetch the authoritative map,
    /// and retry (the existing bounded refused-pin loop, extended).
    #[must_use]
    pub fn pin_block_validated(&self, block_key: &str) -> crate::block_allocator::PinOutcome {
        use crate::block_allocator::PinOutcome;
        // Decoration-tolerant, like `increment_refcount`: pin + word both
        // belong to the BASE block.
        let cleaned = clean_block_key(block_key);
        if let Ok((be_id, offset)) = self.parse_block_key(&cleaned) {
            if be_id == "backend_0" {
                self.default_allocator.pin_block_validated(offset)
            } else if let Some(be) = self.backends.get(&be_id) {
                be.block_allocator.pin_block_validated(offset)
            } else {
                PinOutcome::Refused
            }
        } else {
            PinOutcome::Refused
        }
    }

    /// The allocator that owns a block key's offset (see incarnation seqlock in
    /// [`crate::block_allocator::BlockAllocator`]).
    fn allocator_for_key(
        &self,
        block_key: &str,
    ) -> Option<(std::sync::Arc<crate::block_allocator::BlockAllocator>, u64)> {
        // Decoration-tolerant (FIND-RW2-A, fixed in RW4): size-carrying
        // `bk:off:len` mappings track their BASE offset's incarnation —
        // the decorated window's content dies exactly when the base block
        // is freed/reallocated. (Same rule `free_block` already applies.)
        let cleaned = clean_block_key(block_key);
        let (be_id, offset) = self.parse_block_key(&cleaned).ok()?;
        if be_id == "backend_0" {
            Some((self.default_allocator.clone(), offset))
        } else {
            self.backends
                .get(&be_id)
                .map(|be| (be.block_allocator.clone(), offset))
        }
    }

    /// Owner's durable device write for this block-key incarnation completed;
    /// validated cache fills may now publish bytes for it.
    pub fn publish_block(&self, block_key: &str) {
        if let Some((alloc, offset)) = self.allocator_for_key(block_key) {
            alloc.publish_block(offset);
        }
    }

    /// Incarnation snapshot for a validated cache fill (None = unstable, do not
    /// publish what you read).
    pub fn fill_incarnation(&self, block_key: &str) -> Option<u64> {
        self.allocator_for_key(block_key)
            .and_then(|(alloc, offset)| alloc.fill_incarnation(offset))
    }

    /// True if the incarnation is unchanged since the pre-read snapshot.
    pub fn fill_incarnation_still(&self, block_key: &str, before: u64) -> bool {
        match self.allocator_for_key(block_key) {
            Some((alloc, offset)) => alloc.fill_incarnation_still(offset, before),
            None => false,
        }
    }

    /// Whether `block_key` names an allocator-managed offset at all. Legacy
    /// `block_prefix`-style keys (`…/part_N`) and keys of unknown backends
    /// are NOT allocator offsets — they can never be freed + reallocated
    /// under the same key string, so incarnation validation is vacuous for
    /// them (a fill of such a key is always serve-valid; it is still never
    /// cache-published, preserving the historical publish gate).
    pub(crate) fn key_incarnation_tracked(&self, block_key: &str) -> bool {
        self.allocator_for_key(block_key).is_some()
    }

    /// Free one reference on a block key. The device-range reclaim
    /// (punch on file backings, `BLKDISCARD` on namespaces — see
    /// [`free_reclaim_op`]) is destructive device I/O and runs ONLY on
    /// the terminal release (a non-terminal free must never destroy a
    /// clone's still-referenced bytes), and runs in the `begin_free` →
    /// reclaim → `finish_free` window — the offset is not reallocatable
    /// until after the reclaim, so it can never race a new owner's DMA
    /// at the reused offset (the acked-write lost-update class surfaced
    /// by PR 6's pinned striped concurrency test).
    ///
    /// Since the async block-reclaim fix the reclaim + `finish_free` are
    /// QUEUED to `crate::block_reclaim` (the whole window moves to the
    /// background worker wholesale — on NVMe-oF the synchronous discard
    /// was a ~235 µs fabric round-trip per displaced block, ~2 GB/s of
    /// overwrite throughput). `begin_free` and the read-tier purge stay
    /// on this path: free ACCOUNTING is synchronous; only space RETURN is
    /// deferred.
    pub async fn free_block(&self, block_key: &str) -> Result<()> {
        // Decoration-tolerant: size-carrying mappings (`bk:off:len` — see
        // `parse_block_mapping`) free their BASE block; a raw parse of the
        // decorated string would err and silently leak the block.
        let cleaned = clean_block_key(block_key);
        let (be_id, offset) = self.parse_block_key(&cleaned)?;

        let (allocator, device_path) = if be_id == "backend_0" {
            (
                self.default_allocator.clone(),
                self.default_device.device_path.clone(),
            )
        } else if let Some(be) = self.backends.get(&be_id) {
            (be.block_allocator.clone(), be.device.device_path.clone())
        } else {
            return Ok(());
        };

        log::debug!("router free_block: key={block_key} offset={offset}");
        if allocator.begin_free(offset) {
            // Terminal release: `begin_free` has retired the incarnation, so
            // sweep the read tiers HERE — after the retire, before the
            // offset becomes reallocatable. A straggler validated-fill
            // publish (detached put delayed past the displacement purge —
            // the generic/074 fstest.3 stale-fill) either landed before
            // this purge (removed now) or lands after it, in which case its
            // own incarnation after-check runs after the retire and undoes
            // it. Either way the key's next owner can never tier-hit the
            // dead incarnation's bytes.
            if let Some(purge) = self.read_tier_purge.get() {
                purge(block_key);
            }
            let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
            if crate::block_reclaim::elide_reclaim_for(&device_path) {
                // Idea 4 — discard elision (design-rewrite-program §3):
                // bdev-class terminal frees skip the reclaim queue
                // entirely — debt record, then finish_free (the
                // sanctioned zero-destructive-work window collapse:
                // reuse is guarded by write-before-publish + the
                // incarnation seqlock; correctness never depended on
                // deallocation). ZERO device commands on this path —
                // the trim venues own the space return.
                allocator.record_elided_debt(offset, block_size);
                allocator.finish_free(offset);
                crate::fuse_client::METRICS
                    .block_free_reclaim_elided
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.debt.record(&allocator, &device_path);
                return Ok(());
            }
            // Queue the reclaim + finish_free; the in-flight registration
            // shields the begin_free-limbo offset from fsck's C2/C3/C6
            // adjudication while the queue owns it.
            let inflight = allocator.inflight_register(offset);
            self.reclaim
                .enqueue(crate::block_reclaim::ReclaimEntry {
                    allocator,
                    inflight,
                    device_path,
                    offset,
                    size: block_size,
                })
                .await;
        }
        Ok(())
    }

    /// Outstanding elided-discard debt bytes across this router's
    /// allocators (the `block_free_elided_debt_bytes` gauge's per-router
    /// face — tests / stats).
    pub fn elided_debt_bytes(&self) -> u64 {
        let mut sum = self.default_allocator.elided_debt_bytes_local();
        for be in self.backends.iter() {
            // The default slot aliases the first registered backend on
            // real mounts; avoid double-charging the same allocator.
            if !std::sync::Arc::ptr_eq(&be.value().block_allocator, &self.default_allocator) {
                sum += be.value().block_allocator.elided_debt_bytes_local();
            }
        }
        sum
    }

    /// The fstrim/defrag venue (Idea 4, KD-4.5): drain the elided-discard
    /// debt now — `full` trims the WHOLE free list (the free list is the
    /// durable truth; debt is only the incremental tracker, so a full
    /// trim restores substrate hygiene even after debt was lost to a
    /// crash/unmount). Returns `(blocks, bytes)` reclaimed. A fenced
    /// daemon issues nothing (the reclaimer's fence-halt law).
    pub async fn trim_elided(&self, full: bool) -> (u64, u64) {
        if self.reclaim.fence_halted() {
            log::error!(
                "trim_elided refused: writer guard fenced / volume fail-stopped — a \
                 fenced holder issues NO destructive device commands (successor \
                 recovery owns the accounting)"
            );
            return (0, 0);
        }
        // Targets: every backend when trimming the full free list; the
        // registered debt targets otherwise.
        let targets: Vec<(String, std::sync::Arc<crate::block_allocator::BlockAllocator>)> =
            if full {
                let mut t = vec![(
                    self.default_device.device_path.clone(),
                    self.default_allocator.clone(),
                )];
                for be in self.backends.iter() {
                    if !std::sync::Arc::ptr_eq(
                        &be.value().block_allocator,
                        &self.default_allocator,
                    ) {
                        t.push((
                            be.value().device.device_path.clone(),
                            be.value().block_allocator.clone(),
                        ));
                    }
                }
                t
            } else {
                self.debt.targets_snapshot()
            };
        let mut blocks = 0u64;
        let mut bytes = 0u64;
        for (device_path, allocator) in targets {
            let res = tokio::task::spawn_blocking(move || {
                let mut b = 0u64;
                let mut by = 0u64;
                loop {
                    // `full` walks the whole free list in one call; debt
                    // drains loop until the tracker empties (every pass
                    // consumes entries — stale claims included — so the
                    // gauge is strictly decreasing and the loop
                    // terminates).
                    let outstanding = allocator.elided_debt_bytes_local();
                    let (db, dby) =
                        crate::block_reclaim::drain_debt_sync(&allocator, &device_path, 64, full);
                    b += db;
                    by += dby;
                    if full || outstanding == 0 {
                        break;
                    }
                }
                (b, by)
            })
            .await;
            match res {
                Ok((b, by)) => {
                    blocks += b;
                    bytes += by;
                }
                Err(e) => log::error!("trim_elided blocking task panicked: {e:?}"),
            }
        }
        (blocks, bytes)
    }

    pub async fn free_blocks(&self, block_keys: &[&str]) -> Result<()> {
        for &block_key in block_keys {
            let _ = self.free_block(block_key).await;
        }
        Ok(())
    }

    pub fn start_health_check_worker(
        self: &std::sync::Arc<Self>,
        _redis_url: String,
        _fs_name: String,
    ) {
        // `Weak`, deliberately (the checkpoint-task sentinel discipline —
        // AGENTS "no leaked tasks"): a strong Arc here kept the router —
        // and this 5 s probe loop — alive FOREVER after every caller
        // dropped it. In-process that meant every dropped fixture/mount
        // left an immortal worker probing its dead device each tick.
        // Upgrading per tick lets the worker exit with its router.
        let weak = std::sync::Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            // Per-backend probe hysteresis: only FAILURE_THRESHOLD consecutive
            // hard failures mark a backend unhealthy (a starved probe under
            // saturation is inconclusive, never a flip — see crate::health).
            let mut states: std::collections::HashMap<String, crate::health::HealthState> =
                std::collections::HashMap::new();
            loop {
                interval.tick().await;
                let Some(router) = weak.upgrade() else {
                    return; // router dropped: exit, leak nothing
                };

                // Probe the default slot under the legacy `backend_0` name
                // only on bare routers: on real mounts the first volume's
                // device IS the default slot and is probed under its real
                // name — a phantom probe would double-count it and leak the
                // reserved alias into health state and logs.
                let mut outcomes: Vec<(String, crate::health::Probe)> = Vec::new();
                if router.backends.is_empty() {
                    outcomes.push((
                        "backend_0".to_string(),
                        perform_device_health_check(&router.default_device).await,
                    ));
                }
                for entry in router.backends.iter() {
                    outcomes.push((
                        entry.key().clone(),
                        perform_device_health_check(&entry.value().device).await,
                    ));
                }

                for (be_id, probe) in outcomes {
                    let state = states.entry(be_id.clone()).or_default();
                    match state.observe(probe) {
                        crate::health::Transition::WentUnhealthy => {
                            log::error!(
                                "Backend health check: backend '{}' is UNHEALTHY ({} consecutive probe failures)!",
                                be_id,
                                crate::health::HealthState::FAILURE_THRESHOLD
                            );
                            router.unhealthy_backends.insert(be_id, true);
                        }
                        crate::health::Transition::Recovered => {
                            log::info!("Backend health check: backend '{}' has recovered.", be_id);
                            router.unhealthy_backends.remove(&be_id);
                        }
                        crate::health::Transition::None => {}
                    }
                }

                // §5.9: republish the placement snapshot every tick — the
                // refresh cadence for fill drift, and the pickup point
                // for probe-driven health transitions. Failover IS the
                // republish: every write picks from this table
                // (`get_active_backend`), so an unhealthy/Draining volume
                // drops out of placement here — there is no sticky
                // active-backend pointer to repoint (deleted with the
                // KD-16 PlacementTable landing; it had no readers left).
                router.refresh_placement_table();
            }
        });
    }
}

/// One liveness probe of a data device — the health worker's per-tick
/// unit (public for the probe-attribution contract test: probe reads are
/// control-plane diagnostics, never `get_obj` data-read counts).
pub async fn perform_device_health_check(
    dev: &crate::nvme_dev::NvmeBlockDev,
) -> crate::health::Probe {
    use crate::health::Probe;
    if !std::path::Path::new(&dev.device_path).exists() {
        return Probe::Failed;
    }
    // The probe shares the device's I/O lanes with real traffic: a timeout
    // means "busy", not "dead" — report it as inconclusive so saturation can
    // never flip a healthy backend offline (hysteresis in crate::health).
    // `probe_read_block`, deliberately: a liveness probe is control-plane
    // and must never count in `get_obj` (the data churn detector).
    match tokio::time::timeout(Duration::from_secs(2), dev.probe_read_block(0, 4096)).await {
        Ok(Ok(_)) => Probe::Ok,
        Ok(Err(e)) => {
            log::warn!(
                "Device health check failed for path {}: {:?}",
                dev.device_path,
                e
            );
            Probe::Failed
        }
        Err(_) => {
            log::debug!(
                "Device health check timed out for path {} (busy, inconclusive)",
                dev.device_path
            );
            Probe::Inconclusive
        }
    }
}

pub struct DataRouterInner {
    pub dlm: DlmClient,
    pub meta_backend:
        once_cell::sync::OnceCell<std::sync::Arc<crate::meta_backend::RoutedMetaBackend>>,
    pub cache: TieredCache,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub backend_router: std::sync::Arc<BackendRouter>,
    pub block_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Hot layout/size cache, keyed by **ino** (PR M4 D1.c: the FUSE layer
    /// and every internal path derive the old `inode_{ino}` string from a
    /// `u64` they already hold — the per-op key alloc + string hash bought
    /// nothing; string forms remain only where backend keys genuinely need
    /// `fs_key!`-class shapes).
    pub metadata_cache: moka::sync::Cache<u64, CachedMetadata, ahash::RandomState>,
    /// Single-flight registry. `scc::HashMap`, NOT `HashIndex` (a measured
    /// deviation from the design doc's "container stays" note): HashIndex
    /// defers value drops through epoch reclamation, and since R1a the
    /// value's broadcast ring owns the cohort's multi-MiB `FillResult` —
    /// under a cold stream, thousands of dead flights' deferred rings
    /// retained gigabytes of dead payloads (the PR 4 row-2 cage kill).
    /// HashMap removal drops the Sender (and its ring) synchronously.
    pub(crate) inflight_block_reads:
        std::sync::Arc<scc::HashMap<String, tokio::sync::broadcast::Sender<Option<FillResult>>>>,
    /// R1b/R2 (§5.3/§5.5): per-file K=4 offset-lane classifier + pipeline
    /// state (PR 5 merged the legacy prefetch cursor into these lanes).
    /// `pub` so the pipeline suite can simulate silent moka eviction (the
    /// lane-leak self-repair phase).
    pub stream_lanes: moka::sync::Cache<String, std::sync::Arc<StreamLanes>>,
    /// §5.5 window-cap OVERRIDE (`SQUEEZEFS_READ_PREFETCH_WINDOW`;
    /// explicit value wins verbatim, railed ≤ 4096; 0 disables the
    /// pipeline outright). `None` = the budget-derived default
    /// (`derived_prefetch_window_cap` — no fixed depth, the house
    /// no-constants law).
    pub(crate) prefetch_window_override: Option<u32>,
    /// §5.5 contention scaling: prefetch's share of the hot-tier budget
    /// (`SQUEEZEFS_READ_PREFETCH_SHARE_PCT`, default 50).
    pub(crate) prefetch_share_pct: u64,
    /// §5.5 `active_streams` two-epoch activity gauge (leak-proof by
    /// construction: increment-only per epoch, aged by the roll — lanes
    /// die silently inside moka, so a dec path would leak upward).
    pub(crate) stream_gauge: StreamActivityGauge,
    /// The cold-stream read lane (2026-08-01 campaign,
    /// `src/read_lane.rs`): the arm/disarm lever
    /// (`SQUEEZEFS_READ_LANE=0` = off, the A0 attribution control), the
    /// BDP fetch estimates deriving the per-stream depth, and the
    /// aggregate in-flight gauge (R5 `read_lane_inflight`). Engages
    /// only in the R2 zero-resident-share regime (`pipeline_touch`);
    /// its fills land in `cache.read_lane_hold`, never a tier.
    pub(crate) read_lane: crate::read_lane::ReadLaneGovernor,
    /// R1b ghost table — second-touch admission memory for >256 KiB fills.
    pub(crate) ghost: std::sync::Arc<GhostTable>,
    /// Hybrid-I/O escalation cooldown — bounds ranged re-admission churn
    /// on working sets beyond the tiers (see the type doc).
    pub(crate) escalation_cooldown: std::sync::Arc<EscalationCooldown>,
    /// R1b disk-tier admission mode (env-resolved once; hot-budget-0
    /// auto-degrades SecondTouch to Always).
    pub tier_admission: TierAdmission,
    /// R3 (§5.6) ranged-read dispatch bound: requests ≤ this many bytes
    /// on passthrough, non-streaming, cache-missed striped reads fetch
    /// only their 4 KiB-aligned window (`SQUEEZEFS_READ_RANGED_THRESHOLD`,
    /// default 262144; 0 = kill switch).
    pub(crate) ranged_threshold: u64,
    /// Hybrid I/O diagnostic escape (user directive 2026-07-15;
    /// `-o direct_device_true` / `SQUEEZEFS_DIRECT_DEVICE_TRUE=1`): when
    /// set, O_DIRECT READ requests are strictly device-true — no tier
    /// serve, no admission (ghost/hot/read_lru/NVMe), no pipeline
    /// classification — the `.benchmarks` amplification-methodology
    /// ruler. Buffered traffic is unaffected. AtomicBool (one relaxed
    /// load per striped read) because the mount option is parsed after
    /// router construction (`start_mount`).
    pub(crate) direct_device_true: std::sync::atomic::AtomicBool,
    pub crypto:
        std::sync::Arc<once_cell::sync::OnceCell<crate::crypto_compress::CryptoCompressState>>,
    pub prefetcher: std::sync::Arc<IoUringPrefetcher>,
    /// Write-commit-economy (2026-07-30, lever 1): per-ino **publish
    /// conveyors** — block-publish merges enqueue here and a
    /// leader-elected detached pass drains batches, applying the whole
    /// batch under ONE `INODE_META_LOCKS` section and persisting it as
    /// ONE commit (the M7 conveyor pattern one level up; the core is
    /// the same loom-modeled `ConveyorCore`). Entries are removed when
    /// their conveyor idles (the pass's last unlead), so the map stays
    /// bounded by concurrently-writing inos.
    pub(crate) publish_conveyors: std::sync::Arc<
        scc::HashMap<
            u64,
            std::sync::Arc<crate::meta_backend::kv::conveyor_core::ConveyorCore<QueuedPublish>>,
        >,
    >,
}

#[derive(Clone)]
pub struct DataRouter {
    inner: std::sync::Arc<DataRouterInner>,
}

impl std::ops::Deref for DataRouter {
    type Target = DataRouterInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// One cold-block fill, shared by its single-flight cohort (R1a,
/// docs/design-read-path.md §5.2). `Bytes` clone = refcount bump; waiters
/// never copy, never refetch.
#[derive(Clone)]
pub(crate) struct FillResult {
    pub bytes: bytes::Bytes,
    /// The primary's serve-validity verdict (incarnation stable across the
    /// device read and the publish window). Waiters apply exactly the same
    /// downstream rule as the primary: get_block_for_index rechecks the
    /// binding; `false` forces re-resolve (unchanged semantics).
    pub serve_valid: bool,
}

/// Test seam (§5.2, the `FAIL_NEXT_WRITES` / `SIMULATE_CORRUPTION` shim
/// precedent): artificial delay, in milliseconds, injected inside the
/// awaited ≥ 64 KiB tier-publish closure — one relaxed load per publish,
/// zero-cost when unset; no `#[cfg(test)]` fork of the production path.
/// Lets the churn suite hold a single-flight cohort open long enough to
/// prove waiters are served from the carried result, not the tier.
pub static TEST_TIER_PUBLISH_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Test seam (same contract as [`TEST_TIER_PUBLISH_DELAY_MS`]): artificial
/// delay, in milliseconds, injected between a binding-validated fetch's
/// bytes-in-hand point and its binding recheck
/// ([`DataRouter::get_block_for_index`]). Lets the FIND-RW5-A churn suite
/// make every fetch provably straddle a concurrent write-through
/// displacement — one relaxed load per fetch, zero-cost when unset; no
/// `#[cfg(test)]` fork of the production path.
pub static TEST_BINDING_RECHECK_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// R1b disk-tier admission mode (docs/design-read-path.md §5.3), resolved
/// once per router from `SQUEEZEFS_READ_TIER_ADMISSION`
/// (`always|second-touch|never`, default `second-touch`; unrecognized
/// values refuse loud). A zero hot-tier budget auto-degrades `SecondTouch`
/// to `Always`: with no RAM landing zone, skipping the publish would turn
/// every sub-read of a streamed block into a device refetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierAdmission {
    Always,
    SecondTouch,
    Never,
}

/// R1b ghost table (§5.3): a fixed 2¹⁶-slot direct-mapped array of
/// `AtomicU32` tags (~256 KiB, allocated once per router). Slot =
/// `xxh3(block_key)` low 16 bits; tag = hash high bits ⊕ the fill-count
/// epoch (bumped every 2¹⁵ recorded misses). A key "ghost-hits" when its
/// slot holds its CURRENT-or-PREVIOUS-epoch tag — the window slides one
/// epoch at a time instead of globally invalidating at each bump.
///
/// Concurrency: single-word `Relaxed` loads/stores, deliberately
/// racy-tolerant — a lost update is a missed admission *hint* (a delayed
/// publish), never a correctness event; no cross-word invariant ⇒ no loom
/// model required (the design's stated mandate for this type). Collision
/// model: direct-mapped single-tag — an interleaved cold scan can
/// overwrite a warm key's record before its second touch; the cost is a
/// third-touch admission, absorbed meanwhile by the hot tier (R-1).
pub(crate) struct GhostTable {
    slots: Box<[std::sync::atomic::AtomicU32]>,
    epoch: std::sync::atomic::AtomicU32,
    misses: std::sync::atomic::AtomicU32,
}

impl GhostTable {
    const SLOTS: usize = 1 << 16;
    const EPOCH_MISSES: u32 = 1 << 15;

    fn new() -> Self {
        let mut v = Vec::with_capacity(Self::SLOTS);
        v.resize_with(Self::SLOTS, || std::sync::atomic::AtomicU32::new(0));
        Self {
            slots: v.into_boxed_slice(),
            epoch: std::sync::atomic::AtomicU32::new(1),
            misses: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn tag(hash: u64, epoch: u32) -> u32 {
        // 0 is the empty sentinel; fold the high hash bits with the epoch
        // and never emit 0.
        let t = ((hash >> 16) as u32) ^ epoch.wrapping_mul(0x9E37_79B9);
        if t == 0 {
            1
        } else {
            t
        }
    }

    /// True iff the key was recorded within the current-or-previous epoch;
    /// records the key (current epoch) either way.
    pub(crate) fn check_and_record(&self, block_key: &str) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let hash = xxhash_rust::xxh3::xxh3_64(block_key.as_bytes());
        let slot = (hash as usize) & (Self::SLOTS - 1);
        let epoch = self.epoch.load(Relaxed);
        let cur = Self::tag(hash, epoch);
        let prev = Self::tag(hash, epoch.wrapping_sub(1));
        let seen = self.slots[slot].load(Relaxed);
        let hit = seen == cur || seen == prev;
        self.slots[slot].store(cur, Relaxed);
        if !hit {
            // Epoch roll every 2¹⁵ recorded misses (racy-tolerant: a
            // double-roll under contention just narrows the window once).
            if self.misses.fetch_add(1, Relaxed) + 1 >= Self::EPOCH_MISSES {
                self.misses.store(0, Relaxed);
                self.epoch.fetch_add(1, Relaxed);
            }
        }
        hit
    }
}

/// Hybrid-I/O escalation cooldown (user directive 2026-07-15): a fixed
/// 2¹⁶-slot direct-mapped array of `AtomicU32` tags recording block keys
/// whose ranged second touch was recently ESCALATED to a whole-block
/// admission. Same shape as [`GhostTable`] but with WALL-CLOCK two-epoch
/// sliding windows (32 s epochs ⇒ a key re-escalates at most every
/// ~32–64 s): the ghost proves *reuse*, the cooldown bounds *churn*. On a
/// working set that FITS the tiers, an admitted block stays resident, its
/// touches never reach the ranged dispatch again, and the cooldown entry
/// ages out unused — convergence then zero overhead. On a set far BEYOND
/// the tiers, admitted blocks are evicted and re-touched while their
/// ghost entries are still hot; without this bound every such touch would
/// re-fetch + re-publish 4 MiB (the read-path program's R-5 spiral class
/// in tier form — the 16.9 GiB tax resurrected as churn). With it,
/// re-admission is bounded to ≈ `keys/32s` and the workload degrades to
/// the device-true ranged path between windows. Deliberately
/// racy-tolerant single-word `Relaxed` atomics (a lost update = one extra
/// or one delayed escalation, never a correctness event); no cross-word
/// invariant ⇒ no loom model required.
pub(crate) struct EscalationCooldown {
    slots: Box<[std::sync::atomic::AtomicU32]>,
}

impl EscalationCooldown {
    const SLOTS: usize = 1 << 16;
    const EPOCH_SECS: u64 = 32;

    fn new() -> Self {
        let mut v = Vec::with_capacity(Self::SLOTS);
        v.resize_with(Self::SLOTS, || std::sync::atomic::AtomicU32::new(0));
        Self {
            slots: v.into_boxed_slice(),
        }
    }

    fn epoch_now() -> u32 {
        (StreamLanes::now_ms() / (Self::EPOCH_SECS * 1000)) as u32
    }

    /// True iff `block_key` was escalation-recorded within the current or
    /// previous wall-clock epoch (⇒ the caller must NOT escalate again).
    pub(crate) fn recently_escalated(&self, block_key: &str) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let hash = xxhash_rust::xxh3::xxh3_64(block_key.as_bytes());
        let slot = (hash as usize) & (Self::SLOTS - 1);
        let epoch = Self::epoch_now();
        let seen = self.slots[slot].load(Relaxed);
        seen == GhostTable::tag(hash, epoch) || seen == GhostTable::tag(hash, epoch.wrapping_sub(1))
    }

    /// Record an escalation of `block_key` (current epoch).
    pub(crate) fn record(&self, block_key: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        let hash = xxhash_rust::xxh3::xxh3_64(block_key.as_bytes());
        let slot = (hash as usize) & (Self::SLOTS - 1);
        self.slots[slot].store(GhostTable::tag(hash, Self::epoch_now()), Relaxed);
    }
}

/// Scan-resistant admission governor (2026-07-26 finding: cold-dominated
/// random 4k over a working set ≫ budget collapsed the DEFAULT hybrid
/// posture ~20× — ghost escalations fetched whole 4 MiB blocks that
/// evicted before reuse, so admission bandwidth was pure waste competing
/// with foreground reads for device queue slots; the per-key escalation
/// cooldown alone under-bounds it because 2¹⁶-slot collisions ping-pong
/// records on large key populations — measured 7× over the per-key bound
/// on the 24 GiB rig set).
///
/// Two coupled mechanisms, both windowed over TWO 2-second epochs (the
/// house sliding-window pattern — ghost table / stream gauge):
///
/// 1. **Waste signal** (the `prefetch_evicted_unconsumed` sibling for
///    admissions): every hot-tier eviction of a *protected* (i.e.
///    ghost-admitted) entry reports its payback — `served_bytes` accrued
///    by real reader serves since insert. Shortfall against the entry's
///    own length is windowed **waste**: the admission did not pay back
///    its whole-block fetch. A never-served victim additionally counts
///    `read_admission_evicted_unhit` (the headline tripwire).
/// 2. **Clamp**: when windowed waste ≥ half of windowed admitted bytes
///    (noise floor: two blocks), escalations are admitted only while
///    windowed admitted bytes stay ≤ `fill_pct` % (default 5,
///    `SQUEEZEFS_READ_ADMISSION_FILL_PCT`) of windowed *foreground*
///    ranged device bytes — admission waste is bounded to a few percent
///    of the device bandwidth the workload is already paying. Denials
///    count `read_admission_governor_denials`; the denied read proceeds
///    as a device-true ranged window read (always correctness-safe), and
///    deliberately records **no** escalation cooldown, so genuinely hot
///    keys retry and win the trickle (skew convergence).
///
/// Fitting working sets produce no evictions ⇒ no waste ⇒ the governor
/// never clamps and warm-up/steady state are byte-identical to the
/// pre-governor hybrid policy. Uniform beyond-budget churn keeps the
/// waste ratio ≈ 100 % ⇒ the clamp is stable at the trickle. Skewed
/// workloads: hot admissions pay back (clock `referenced` re-arming keeps
/// them resident), tail admissions keep the clamp engaged — the hot
/// subset converges to RAM while the tail stays device-true.
///
/// Concurrency: single-word `Relaxed` atomics, racy-tolerant by the same
/// argument as [`GhostTable`] (a lost update is one extra or one denied
/// escalation, never a correctness event); no cross-word invariant ⇒ no
/// loom model required.
pub struct AdmissionGovernor {
    fill_pct: u64,
    epoch: std::sync::atomic::AtomicU64,
    /// Clamped-mode fill budget for the CURRENT epoch, in bytes — granted
    /// at each roll as `fill_pct` % of the PREVIOUS epoch's foreground
    /// bytes (leftovers discarded). A single word so the reservation is
    /// one `checked_sub` CAS: the earlier two-window spend arithmetic had
    /// a roll-boundary race (a stale `prev` snapshot across the shift let
    /// each boundary re-admit up to a herd of blocks — measured on the
    /// rig as exactly 2× the configured budget) and an alternating
    /// fill-to-cap/starve oscillation. Tokens have neither: budget is
    /// minted once per epoch, spent by CAS, never re-derived.
    tokens: std::sync::atomic::AtomicU64,
    /// Windowed bytes of ADMITTED (protected) victims evicted — the waste
    /// ratio's denominator. Deliberately eviction-side: an admission
    /// burst must not be able to dilute the ratio and unclamp itself (the
    /// check-then-add herd's second face — measured on the rig as the
    /// clamp flapping open at ~6× the fill budget).
    evicted_cur: std::sync::atomic::AtomicU64,
    evicted_prev: std::sync::atomic::AtomicU64,
    wasted_cur: std::sync::atomic::AtomicU64,
    wasted_prev: std::sync::atomic::AtomicU64,
    foreground_cur: std::sync::atomic::AtomicU64,
    foreground_prev: std::sync::atomic::AtomicU64,
    clamped: std::sync::atomic::AtomicBool,
}

/// Test seam (the `TEST_TIER_PUBLISH_DELAY_MS` precedent): overrides the
/// governor's wall-clock epoch length in milliseconds (0 = the production
/// 2000 ms). The token grant and waste windows are wall-clock mechanisms;
/// contract tests shrink the epoch so convergence is observable in-process
/// instead of sleeping out multi-second windows. One relaxed load per
/// roll, zero-cost when unset.
pub static TEST_ADMISSION_EPOCH_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

impl AdmissionGovernor {
    const EPOCH_MS: u64 = 2_000;

    pub fn new(fill_pct: u64) -> Self {
        Self {
            fill_pct: fill_pct.min(100),
            epoch: std::sync::atomic::AtomicU64::new(0),
            tokens: std::sync::atomic::AtomicU64::new(0),
            evicted_cur: std::sync::atomic::AtomicU64::new(0),
            evicted_prev: std::sync::atomic::AtomicU64::new(0),
            wasted_cur: std::sync::atomic::AtomicU64::new(0),
            wasted_prev: std::sync::atomic::AtomicU64::new(0),
            foreground_cur: std::sync::atomic::AtomicU64::new(0),
            foreground_prev: std::sync::atomic::AtomicU64::new(0),
            clamped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn epoch_ms() -> u64 {
        let t = TEST_ADMISSION_EPOCH_MS.load(std::sync::atomic::Ordering::Relaxed);
        if t == 0 {
            Self::EPOCH_MS
        } else {
            t
        }
    }

    /// Slide the two-epoch window and mint the epoch's token grant
    /// (op-driven; the CAS winner shifts).
    fn roll(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let now = StreamLanes::now_ms() / Self::epoch_ms();
        let seen = self.epoch.load(Relaxed);
        if seen != now
            && self
                .epoch
                .compare_exchange(seen, now, Relaxed, Relaxed)
                .is_ok()
        {
            let adjacent = now == seen.wrapping_add(1);
            for (cur, prev) in [
                (&self.evicted_cur, &self.evicted_prev),
                (&self.wasted_cur, &self.wasted_prev),
            ] {
                let c = cur.swap(0, Relaxed);
                prev.store(if adjacent { c } else { 0 }, Relaxed);
            }
            // Mint this epoch's fill budget from the foreground the
            // workload ACTUALLY paid last epoch; leftovers die with the
            // epoch (no carryover bursts). Reservations racing this store
            // cost at most one block of slack per roll — racy-tolerant.
            let f = self.foreground_cur.swap(0, Relaxed);
            self.foreground_prev
                .store(if adjacent { f } else { 0 }, Relaxed);
            self.tokens
                .store(f.saturating_mul(self.fill_pct) / 100, Relaxed);
        }
    }

    fn window(cur: &std::sync::atomic::AtomicU64, prev: &std::sync::atomic::AtomicU64) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        cur.load(Relaxed).saturating_add(prev.load(Relaxed))
    }

    /// Foreground ranged device bytes — the clamp's denominator. Fed at
    /// the ranged device-read site (the workload's own device spend);
    /// escalation fetches deliberately do NOT feed it (no self-funding).
    pub fn note_foreground(&self, bytes: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.roll();
        self.foreground_cur.fetch_add(bytes, Relaxed);
    }

    /// Eviction report from the governed (hot-block) tier: protected
    /// victims carry their payback credit; the shortfall is windowed
    /// waste. Probation victims (stream residue) are not admissions and
    /// never count.
    pub fn on_eviction(&self, len: u64, class: &crate::tiering::memory::EvictClass) {
        use std::sync::atomic::Ordering::Relaxed;
        let crate::tiering::memory::EvictClass::Protected {
            served_bytes,
            stream_admitted,
        } = class
        else {
            return;
        };
        self.roll();
        self.evicted_cur.fetch_add(len, Relaxed);
        let waste = len.saturating_sub(*served_bytes);
        if waste == 0 {
            return;
        }
        self.wasted_cur.fetch_add(waste, Relaxed);
        METRICS
            .read_admission_wasted_bytes
            .fetch_add(waste, Relaxed);
        // Stream-admitted victims report full shortfall by construction
        // (their within-pass consumption credits nothing — the honest
        // basis that lets the clamp SEE beyond-budget stream admissions
        // as the waste they are) but are exempt from the unhit tripwire:
        // that counter keeps meaning admitted-and-NEVER-touched, the ops
        // signal the 2026-07-26 campaign defined.
        if *served_bytes == 0 && !stream_admitted {
            METRICS.read_admission_evicted_unhit.fetch_add(1, Relaxed);
        }
    }

    /// The windowed clamp verdict — ONE definition for the authoritative
    /// escalation site and the prelude peek. Eviction-side ratio: of the
    /// admitted bytes the tier gave BACK this window, did at least half
    /// pay for themselves? Admissions themselves are deliberately not in
    /// the denominator — a burst must not dilute the ratio and unclamp
    /// itself. Rolls the window and refreshes the `clamped` gauge.
    fn clamp_engaged(&self, block_bytes: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.roll();
        let evicted = Self::window(&self.evicted_cur, &self.evicted_prev);
        let wasted = Self::window(&self.wasted_cur, &self.wasted_prev);
        let clamped = wasted >= 2 * block_bytes.max(1) && wasted.saturating_mul(2) >= evicted;
        self.clamped.store(clamped, Relaxed);
        clamped
    }

    /// NON-RESERVING admission peek (DIALED P1.5 — the IPC direct-drive
    /// prelude's "governor token check"): the clamp verdict + a plain
    /// token-availability load, WITHOUT the reservation CAS. `false` = a
    /// governor DENIAL (accounted; the caller direct-drives the miss as
    /// a device-window read and records no cooldown, exactly the
    /// authoritative site's denial semantics). `true` = grant-shaped —
    /// the caller routes the op to the handler path, whose
    /// [`Self::allow_escalation`] remains the ONLY reservation site (the
    /// herd-safety argument is preserved: peeks can over-ADMIT into the
    /// handler near a token boundary — bounded per epoch — but can never
    /// over-SPEND the grant; the losing racers degrade to handler-side
    /// ranged window reads, never to over-admission).
    pub fn escalation_would_admit(&self, block_bytes: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if self.clamp_engaged(block_bytes) && self.tokens.load(Relaxed) < block_bytes {
            METRICS
                .read_admission_governor_denials
                .fetch_add(1, Relaxed);
            return false;
        }
        true
    }

    /// The one token RESERVATION, not check-then-add: under real mount
    /// concurrency (256-deep O_DIRECT) a herd of simultaneous attempts
    /// all passed a plain check before any spend landed — measured on
    /// the rig as ~6× the configured budget (93 admits/s where 5 % of
    /// foreground allowed 15). The single-word token CAS makes the check
    /// and the spend one atomic step with no cross-word snapshot to
    /// race. Shared by the ranged escalation site and the stream-fill
    /// admission site — one grant economy, two instruments.
    fn reserve_tokens(&self, block_bytes: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.tokens
            .fetch_update(Relaxed, Relaxed, |t| t.checked_sub(block_bytes))
            .is_ok()
    }

    /// The escalation-site decision: admit (accounting the block) or deny
    /// (counted; the caller falls back to the device-true ranged window
    /// read and records no cooldown).
    pub fn allow_escalation(&self, block_bytes: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if self.clamp_engaged(block_bytes) && !self.reserve_tokens(block_bytes) {
            METRICS
                .read_admission_governor_denials
                .fetch_add(1, Relaxed);
            return false;
        }
        true
    }

    /// The STREAM-fill admission decision (the transient stream window,
    /// 2026-07-29): may a classified stream's ghost-hit re-fill admit
    /// protected (+ publish), or must it ride the transient window
    /// (probation, publish skipped, waste-ledger-invisible)? Same clamp +
    /// token reservation as the ranged site — under an engaged clamp the
    /// grant trickle is bounded to `fill_pct` % of the stream's own
    /// foreground device spend (streaming fills feed `note_foreground`,
    /// so the governor's documented bounded-waste law holds on this
    /// shape too) — but refusals count `read_admission_stream_transients`
    /// and NEVER `read_admission_governor_denials`: a held-transient fill
    /// still serves its reader from hot probation (nothing degraded),
    /// while a ranged denial names a read that stayed device-true. Two
    /// meanings, two instruments.
    pub fn allow_stream_admission(&self, block_bytes: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if self.clamp_engaged(block_bytes) && !self.reserve_tokens(block_bytes) {
            METRICS
                .read_admission_stream_transients
                .fetch_add(1, Relaxed);
            return false;
        }
        true
    }

    /// Stats gauge: the clamp verdict of the most recent escalation
    /// attempt (windowed state is op-driven; this is a snapshot, not a
    /// recomputation).
    pub fn clamped(&self) -> bool {
        self.clamped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// One classifier offset lane (§5.3) grown into the R2 pipeline owner
/// (§5.5): racy-tolerant atomics throughout — moka races and lost updates
/// cost a later classification or a slightly mis-sized window, never
/// wrongness, and never a lock on the read path.
pub struct StreamLane {
    next_expected_offset: std::sync::atomic::AtomicU64,
    run_reads: std::sync::atomic::AtomicU32,
    last_seen_ms: std::sync::atomic::AtomicU64,
    classified: std::sync::atomic::AtomicBool,
    /// §5.5 pipeline plan: the next block index to ISSUE (exclusive upper
    /// edge of the issued span). Reset with the lane.
    next_prefetch_block: std::sync::atomic::AtomicU32,
    /// Adaptive window: starts at 2, ×2 on foreground-wait, halved (AIMD)
    /// on consumer-detected evicted-unconsumed, capped by the env knob.
    window: std::sync::atomic::AtomicU32,
    /// Issued-but-not-completed fills.
    inflight: std::sync::atomic::AtomicU32,
    /// Read-lane (2026-08-01) issued-but-not-completed lane fetches —
    /// deliberately separate from `inflight`: the two issue regimes are
    /// disjoint (the lane runs exactly where R2's resident share is 0)
    /// but the share flips with `active_streams`, and conflated
    /// accounting across a flip would wedge whichever regime resumes.
    rl_inflight: std::sync::atomic::AtomicU32,
    /// Completed fills not yet foreground-consumed — the resident-
    /// unconsumed bound (§5.5 mechanism i).
    unconsumed: std::sync::atomic::AtomicU32,
    /// First block index NOT yet consumed by the foreground (the consume
    /// cursor): consumption is counted per BLOCK the stream advances
    /// past, never per request — four 1 MiB sub-reads of one 4 MiB block
    /// are ONE consumption (the per-request draft drained `unconsumed` 4x
    /// too fast, over-issued into the budget, and the AIMD quiescence arm
    /// then stalled healthy lanes — measured live on the row-2 shape).
    consumed_edge: std::sync::atomic::AtomicU32,
    /// First block index of the ISSUED span: the evicted-unconsumed
    /// detector probes residency only for blocks in
    /// `[issued_base, next_prefetch_block)` — blocks the pipeline actually
    /// fetched. Without this base the plan-REBASE span (blocks the reader
    /// skipped past, never issued) counted as pipeline coverage, and every
    /// foreground fetch inside it fired the detector: measured live on the
    /// row-2 shape as `evicted_unconsumed` ≈ 1 700 false positives whose
    /// AIMD/quiescence response froze healthy lanes at issued ≈ 33.
    issued_base: std::sync::atomic::AtomicU32,
    /// Abandonment fence: bumped on lane reset; tasks check it at
    /// admission and before landing their fill accounting.
    generation: std::sync::atomic::AtomicU64,
    /// `active_streams` gauge stamp: this lane counted itself in this
    /// epoch (one relaxed compare per issue-path touch).
    last_counted_epoch: std::sync::atomic::AtomicU64,
    /// AIMD quiescence arm, PROGRESS-clocked: after a consumer-detected
    /// evicted-unconsumed fill, issue pauses until the CONSUME EDGE
    /// reaches this block index — the suppression span doubles with each
    /// consecutive detection (`detect_streak`) and a clean consume resets
    /// it. Self-clocked by the stream: a healthy lane's transient
    /// detection costs milliseconds of lookahead; a genuinely starved
    /// lane re-detects on resume, the span grows geometrically, and the
    /// fetch ratio converges to ~1.0x. (A 2 s WALL-clock arm tried first
    /// froze healthy lanes wholesale — measured on the bench row-2 shape
    /// as issue starvation: 283 of 4 096 blocks pipelined, row 2 at
    /// 0.64x row 1.)
    suppress_until_edge: std::sync::atomic::AtomicU32,
    /// Consecutive evicted-unconsumed detections without an intervening
    /// clean consume — exponent for the suppression span (capped).
    detect_streak: std::sync::atomic::AtomicU32,
}

/// §5.5 `active_streams` — a two-epoch activity gauge (the same sliding
/// pattern as the ghost table), leak-proof by construction: classified
/// lanes increment the CURRENT epoch's counter at most once per epoch
/// (per-lane `last_counted_epoch` stamp); `active = max(cur, prev)`; on
/// epoch roll `prev ← cur, cur ← 0`. Deliberately NO decrement path:
/// lanes live inside per-path moka entries that evict silently, so an
/// inc/dec counter would only leak upward. Epoch length = the 2 s
/// staleness constant. Single-word Relaxed atomics, racy-tolerant (a
/// double roll narrows one window; a lost increment undercounts one
/// epoch) — no loom model required.
pub(crate) struct StreamActivityGauge {
    epoch: std::sync::atomic::AtomicU64,
    cur: std::sync::atomic::AtomicU32,
    prev: std::sync::atomic::AtomicU32,
}

impl StreamActivityGauge {
    fn new() -> Self {
        Self {
            epoch: std::sync::atomic::AtomicU64::new(0),
            cur: std::sync::atomic::AtomicU32::new(0),
            prev: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn now_epoch() -> u64 {
        StreamLanes::now_ms() / 2_000
    }

    /// Count `lane` for the current epoch (idempotent per epoch per lane)
    /// and return the current `active_streams` estimate (≥ 1).
    fn touch(&self, lane: &StreamLane) -> u32 {
        use std::sync::atomic::Ordering::Relaxed;
        let now = Self::now_epoch();
        let seen = self.epoch.load(Relaxed);
        if seen != now
            && self
                .epoch
                .compare_exchange(seen, now, Relaxed, Relaxed)
                .is_ok()
        {
            // Roll: one full missed epoch (or more) ages everything out.
            let cur = self.cur.swap(0, Relaxed);
            self.prev
                .store(if now == seen + 1 { cur } else { 0 }, Relaxed);
        }
        if lane.last_counted_epoch.swap(now, Relaxed) != now {
            self.cur.fetch_add(1, Relaxed);
        }
        let active = std::cmp::max(self.cur.load(Relaxed), self.prev.load(Relaxed)).max(1);
        crate::fuse_client::METRICS
            .prefetch_active_streams
            .store(active as u64, Relaxed);
        active
    }
}

/// §5.5 window-economy decisions (read-saturation campaign, 2026-07-29),
/// extracted pure so `tests/read_prefetch_window_tests.rs` pins their
/// tables — `pipeline_touch` routes through these three functions.
///
/// The default window cap: the prefetch share of the hot budget, in
/// blocks (no fixed depth — the house no-constants law; an explicit
/// `SQUEEZEFS_READ_PREFETCH_WINDOW` wins verbatim). Rails: floor 4 (below
/// it the AIMD start of 2 cannot even double once), cap 4096 (bounds a
/// pathological budget/block ratio).
pub fn derived_prefetch_window_cap(share_pct: u64, hot_budget: u64, block_size: u64) -> u32 {
    if block_size == 0 {
        // Unconfigured geometry (pre-mount router): no basis to derive —
        // the floor, defensively.
        return 4;
    }
    ((share_pct.min(100) * hot_budget / 100) / block_size).clamp(4, 4096) as u32
}

/// One §5.5 issue-admission decision: may this lane issue one more
/// prefetch fetch? `resident_share` is the lane's landed-fill budget in
/// blocks (`share% × hot_budget / block / active_streams`). The two
/// bounds are split deliberately (the campaign's core change):
///
/// - **Landed-unconsumed ≤ resident share** — completed fills occupy the
///   hot tier until their consumer arrives; the budget must see them.
///   A share of 0 (the lane cannot retain even ONE block) never
///   speculates: every fill would be evicted-before-consume by
///   construction (the §5.5 no-floor rationale, preserved verbatim).
/// - **In-flight + unconsumed ≤ min(window, cap)** — the AIMD plan
///   bound. In-flight fetch bytes are transient DMA buffers charged to
///   R5 via `prefetch_inflight_bytes`, NOT hot-tier residents; charging
///   them against the resident share (the pre-campaign formula) made
///   the 16-stream default shape a structurally depth-1 pipeline that
///   could never overlap fetch latency with consumption.
pub fn prefetch_issue_admits(
    window: u32,
    cap: u32,
    resident_share: u64,
    in_flight: u32,
    unconsumed: u32,
) -> bool {
    if resident_share == 0 {
        return false;
    }
    u64::from(unconsumed) < resident_share
        && u64::from(in_flight) + u64::from(unconsumed) < u64::from(window.min(cap))
}

/// One §5.5 window-growth decision (the AIMD up-edge): foreground-wait
/// (the reader caught an in-flight fetch — depth, unconditionally) or a
/// clean plan overrun (the reader passed the whole issued plan — the
/// silent-consumption regime's shallowness signal: warm ring serves
/// never wait on the single flight, so an il stream's only depth
/// evidence is the front miss). Overrun growth is refused while the
/// lane carries an evicted-unconsumed streak — growing into
/// demonstrated starvation feeds the R-5 spiral the AIMD collapse is
/// fighting. All growth is mem-budget-Green-gated (§5.7).
pub fn prefetch_window_grows(
    will_wait: bool,
    overran: bool,
    detect_streak: u32,
    green: bool,
) -> bool {
    green && (will_wait || (overran && detect_streak == 0))
}

/// K = 4 offset lanes per file, so concurrent sequential readers of one
/// file do not mutually reset each other. More than K concurrent readers
/// degrade the excess to the random class — a later pipeline start,
/// never wrongness.
pub struct StreamLanes {
    lanes: [StreamLane; 4],
    /// Read-lane round-4 (2026-08-01): the FILE-level issue owner —
    /// exactly ONE lane cursor per file drives the read-lane ahead
    /// pipeline at a time. Under qd reorder the pre-classification
    /// claim path routinely mints 2+ classified lanes for ONE reader
    /// (measured: ~2 classify events per file-pass), and sibling
    /// cursors each issuing `[end+1, end+depth]` doubled the ahead
    /// spend into FIFO churn (r3C1: 190k of 349k deposits evicted
    /// unconsumed, read_amp 1.27). Ownership is sticky while fresh
    /// (< 2 s) and rotates on staleness — a real second reader takes
    /// over within one staleness window. Racy-tolerant: a lost CAS
    /// costs one skipped top-up.
    issue_owner_idx: std::sync::atomic::AtomicUsize,
    issue_owner_ms: std::sync::atomic::AtomicU64,
    /// Consecutive reads of this file that matched NO lane — the
    /// random-dominated signature. A spurious classification (random
    /// traffic occasionally lands 4 contiguous offsets) would otherwise
    /// veto the ranged path file-wide for its whole 2 s freshness window,
    /// steering thousands of random misses into ungoverned whole-block
    /// fetches (the scan-resistance cold-row residual, measured 20–40 GiB
    /// per 20 s on the rig). At [`Self::FOREIGN_DECLASSIFY_RUN`] the
    /// classifications clear; any lane match resets the run. Honest cost:
    /// a REAL small-request stream interleaved with a ≥ 16:1 random flood
    /// on the same file declassifies too and rides ranged window reads
    /// until the flood subsides — under such a flood its whole-block
    /// locality was churning anyway, and re-classification is 4 requests.
    foreign_since_match: std::sync::atomic::AtomicU32,
}

/// §5.6 zero-copy ranged destination, one definition: a 4 KiB-aligned
/// pointer into the registered uring payload region
/// (`get_payload_buffer`), offered ONLY on the zero-copy leg
/// (window == request). The callee DMAs the full served length at offset
/// 0 and zeroes `served..requested` (the reused-payload replay rule); the
/// bounce leg never sees it.
pub struct RangedDest {
    pub ptr: *mut u8,
    pub cap: usize,
}

// SAFETY: `ptr` addresses registered uring payload memory (or a test's
// private aligned allocation) whose access is exclusive to this request
// for the lease's lifetime — the §5.4 payload-lease protocol serializes
// kernel/worker access, and the ranged serve is the request's only
// writer. Sending the pointer across the executor's threads moves that
// exclusive access, never shares it.
unsafe impl Send for RangedDest {}

/// A block fill's provenance at the validated fill site
/// (`get_cached_or_fetch_block_traced`) — the transient stream window's
/// dispatch input (2026-07-29):
///
/// * `Demand` — an ordinary foreground fill (random/unclassified files,
///   GDS warmers, ranged-escalation whole-block fetches). Today's
///   admission semantics verbatim.
/// * `DemandStream` — a foreground fill for a file holding a FRESH §5.3
///   streaming classification: its device fetch feeds the governor's
///   foreground basis, and its ghost hit admits only through
///   `allow_stream_admission`.
/// * `Prefetch` — a §5.5 pipeline fill (streaming by definition): same
///   stream admission arbitration, but never feeds the foreground basis
///   (no self-funding), and its non-admitted hot put carries the
///   one-lap clock grace.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FillClass {
    Demand,
    DemandStream,
    Prefetch,
}

impl FillClass {
    fn streaming(self) -> bool {
        matches!(self, FillClass::DemandStream | FillClass::Prefetch)
    }
}

/// The classifier verdict handed back to the read path: which lane (if
/// any) this request rides, and whether that lane is classified streaming.
pub(crate) struct LaneRef<'a> {
    pub(crate) lane: &'a StreamLane,
    pub(crate) streaming: bool,
}

/// Per-request read classifier hint (hybrid I/O, user directive
/// 2026-07-15; the §5.3 `ReadClassHint` the design named): the FUSE read
/// handler hands the routing layer the request's O_DIRECT bit
/// (`fuse_read_in.flags`). Under the DEFAULT hybrid policy the bit only
/// labels observability (`read_odirect_tier_serves` /
/// `read_odirect_ghost_admits`) — O_DIRECT and buffered ride the same
/// serve/admission machinery. Combined with the mount-scoped
/// `direct_device_true` escape it selects the strictly device-true
/// diagnostic path. Internal readers (copy_file_range, RMW seeds) pass
/// `default()`.
#[derive(Clone, Copy, Default)]
pub struct ReadClassHint {
    /// The request rode an O_DIRECT file description.
    pub odirect: bool,
    /// The serve dest is a client-visible session ARENA window (E-IL2 —
    /// read-copy-count 2026-08-02), not a registered uring ent payload.
    /// NT-store serve copies are EXEMPTED for arena dests: the consumer
    /// is the client's `slab_read` within ~one op, and the counted field
    /// bracket showed NT there LOSES (−2.8 % rd-il) while ring-ent dests
    /// WIN (+11 % rd-kern) — the destination's next reader distance is
    /// the whole story, measured per the fastest-wins ruling.
    pub dest_arena: bool,
    /// The request already fed the stream lanes at the IPC sink
    /// (read-saturation campaign, 2026-07-29): ring reads observe into
    /// the §5.3 classifier ONCE, at [`DataRouter::ring_read_lane_touch`]
    /// — a ring op that then demotes to the handler must not observe
    /// again (a second observation of an already-advanced lane looks
    /// like foreign traffic, and 16 of them destroy the very
    /// classification that routed the op here — the §5.3 declassify
    /// rule). The handler's `pipeline_touch` sites skip when set.
    pub lane_pre_fed: bool,
}

/// The dest-arm serve copy (read-copy-count 2026-08-02): NT-policied for
/// registered uring ent payload dests (the counted +11 % EXA-cold-read
/// win — `SQUEEZEFS_NT_READ_SERVE`, default on, floor 256 KiB), CACHED
/// for client-visible arena dests (`dest_arena` — the counted −2.8 % il
/// negative: the client's `slab_read` consumes those lines within ~one
/// op). Returns `true` iff the NT body ran — callers feed
/// `nt_read_serve_bytes` from it.
///
/// # Safety
/// `dst`/`src` valid for `len` bytes, non-overlapping.
#[inline]
pub(crate) unsafe fn serve_copy_to_dest(
    dst: *mut u8,
    src: *const u8,
    len: usize,
    dest_arena: bool,
) -> bool {
    if dest_arena {
        std::ptr::copy_nonoverlapping(src, dst, len);
        false
    } else {
        crate::nt_copy::read_serve_copy_raw(dst, src, len)
    }
}

impl StreamLanes {
    fn new() -> Self {
        let mk = || StreamLane {
            next_expected_offset: std::sync::atomic::AtomicU64::new(u64::MAX),
            run_reads: std::sync::atomic::AtomicU32::new(0),
            last_seen_ms: std::sync::atomic::AtomicU64::new(0),
            classified: std::sync::atomic::AtomicBool::new(false),
            next_prefetch_block: std::sync::atomic::AtomicU32::new(0),
            window: std::sync::atomic::AtomicU32::new(2),
            inflight: std::sync::atomic::AtomicU32::new(0),
            rl_inflight: std::sync::atomic::AtomicU32::new(0),
            unconsumed: std::sync::atomic::AtomicU32::new(0),
            consumed_edge: std::sync::atomic::AtomicU32::new(0),
            issued_base: std::sync::atomic::AtomicU32::new(0),
            generation: std::sync::atomic::AtomicU64::new(0),
            last_counted_epoch: std::sync::atomic::AtomicU64::new(u64::MAX),
            suppress_until_edge: std::sync::atomic::AtomicU32::new(0),
            detect_streak: std::sync::atomic::AtomicU32::new(0),
        };
        Self {
            lanes: [mk(), mk(), mk(), mk()],
            foreign_since_match: std::sync::atomic::AtomicU32::new(0),
            issue_owner_idx: std::sync::atomic::AtomicUsize::new(usize::MAX),
            issue_owner_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Foreign-run length that clears every lane's classification (see
    /// `foreign_since_match`).
    const FOREIGN_DECLASSIFY_RUN: u32 = 16;

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// §5.6 dispatch probe: does any lane hold a FRESH streaming
    /// classification (within the 2 s staleness constant)? Streams keep
    /// whole-block fetches (1.0× amplification + the pipeline); a stale
    /// classified lane must not deny a now-random file the ranged path.
    pub(crate) fn any_streaming_fresh(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let now = Self::now_ms();
        self.lanes.iter().any(|l| {
            l.classified.load(Relaxed) && now.saturating_sub(l.last_seen_ms.load(Relaxed)) < 2_000
        })
    }

    /// Classification unit (§5.3, one definition): `run_reads ≥ 4`
    /// contiguous requests — each starting where the previous ended —
    /// classifies the lane Streaming. A non-matching offset claims the
    /// stalest idle lane (resetting its run AND abandoning its pipeline:
    /// generation bump, plan cleared, window collapsed — §5.5
    /// cancellation), else the read is Random.
    ///
    /// `claim`: whether a non-matching offset may run the foreign-count
    /// + stalest-lane-claim machinery (read-saturation campaign): warm
    /// ring serves pass `false` — they may CONTINUE a lane a miss
    /// started (contiguity match above), but a fully-warm stream has
    /// nothing to prefetch and the per-op claim path was the measured
    /// warm-row tax (−6..−16 % on the 1M-IOPS rand-4k rows vs the
    /// pipeline kill switch). Misses and every kernel-path request keep
    /// `true` (kernel semantics unchanged).
    pub(crate) fn observe(&self, offset: u64, len: u64, claim: bool) -> Option<LaneRef<'_>> {
        use std::sync::atomic::Ordering::Relaxed;
        let now = Self::now_ms();
        // Lane match: continue the run.
        for lane in &self.lanes {
            if lane.next_expected_offset.load(Relaxed) == offset {
                self.foreign_since_match.store(0, Relaxed);
                lane.next_expected_offset.store(offset + len, Relaxed);
                let run = lane.run_reads.fetch_add(1, Relaxed) + 1;
                lane.last_seen_ms.store(now, Relaxed);
                if run >= 4 {
                    if !lane.classified.swap(true, Relaxed) {
                        crate::fuse_client::METRICS
                            .read_streams_classified
                            .fetch_add(1, Relaxed);
                    }
                    return Some(LaneRef {
                        lane,
                        streaming: true,
                    });
                }
                return Some(LaneRef {
                    lane,
                    streaming: false,
                });
            }
        }
        // CLASSIFIED-MEMBERSHIP tolerance (read-lane campaign round 3,
        // 2026-08-01 — the field's qd-reorder wedge): once a lane is
        // classified, an out-of-order sibling of the same stream must
        // keep MEMBERSHIP even though it breaks exact contiguity — a
        // libaio qd8 completion swap otherwise wedges the lane forever
        // (`next_expected_offset` names a request that already arrived;
        // measured: 0.7 % of a qd8 stream's arrivals matched, 1,233
        // declassify/reclassify churn events per 70 s row, the lane
        // starved at ~3 % engagement). The window is
        // REQUEST-LENGTH-scaled — `±64 × len`, capped at 16 blocks'
        // worth of bytes via the caller's own len (a qd-64 reorder span
        // of the stream's own requests) — so small-request random
        // traffic stays foreign (4 KiB ⇒ ±256 KiB: the 2026-07-26
        // mid-row shape declassifies exactly as before; simulated 0 %
        // member across the random harm shapes, 88–100 % across the
        // seq reorder shapes). RUN-BUILDING stays exact-contiguity —
        // the pinned random-vs-stream discriminator is untouched;
        // membership applies only to lanes that already earned
        // classification, while they are fresh. `next_expected_offset`
        // advances max-forward (racy-tolerant: a lost update is a
        // slightly stale edge inside the tolerance).
        for lane in &self.lanes {
            if lane.classified.load(Relaxed)
                && now.saturating_sub(lane.last_seen_ms.load(Relaxed)) < 2_000
            {
                let exp = lane.next_expected_offset.load(Relaxed);
                if exp == u64::MAX {
                    continue;
                }
                let tol = len.saturating_mul(64);
                if offset.saturating_add(tol) >= exp && offset <= exp.saturating_add(tol) {
                    self.foreign_since_match.store(0, Relaxed);
                    if offset + len > exp {
                        lane.next_expected_offset.store(offset + len, Relaxed);
                    }
                    lane.last_seen_ms.store(now, Relaxed);
                    return Some(LaneRef {
                        lane,
                        streaming: true,
                    });
                }
            }
        }
        if !claim {
            return None;
        }
        // No lane matched: a foreign read. A long-enough foreign run
        // means random traffic dominates this file — clear every lane's
        // classification (and its run credit: fresh evidence required)
        // so a spurious stream cannot hold the ranged-path veto for its
        // whole freshness window. Racy-tolerant like the lanes
        // themselves: a lost update delays the declassify by one read.
        if self.foreign_since_match.fetch_add(1, Relaxed) + 1 >= Self::FOREIGN_DECLASSIFY_RUN {
            self.foreign_since_match.store(0, Relaxed);
            for lane in &self.lanes {
                if lane.classified.swap(false, Relaxed) {
                    lane.run_reads.store(0, Relaxed);
                }
            }
        }
        // Claim the stalest lane (2 s staleness — the existing constant).
        let mut stalest = 0usize;
        let mut stalest_ms = u64::MAX;
        for (i, lane) in self.lanes.iter().enumerate() {
            let seen = lane.last_seen_ms.load(Relaxed);
            if seen < stalest_ms {
                stalest_ms = seen;
                stalest = i;
            }
        }
        if now.saturating_sub(stalest_ms) >= 2_000 {
            let lane = &self.lanes[stalest];
            // Abandon the previous stream on this lane (§5.5): tasks in
            // flight land as wasted; the plan restarts from scratch.
            lane.generation.fetch_add(1, Relaxed);
            lane.next_expected_offset.store(offset + len, Relaxed);
            lane.run_reads.store(1, Relaxed);
            lane.classified.store(false, Relaxed);
            lane.next_prefetch_block.store(0, Relaxed);
            lane.window.store(2, Relaxed);
            lane.unconsumed.store(0, Relaxed);
            // `inflight`/`rl_inflight` deliberately NOT reset: in-flight
            // tasks decrement their own counters at settle (a reset here
            // would underflow them); the generation bump makes their
            // completions land as wasted.
            lane.consumed_edge.store(0, Relaxed);
            lane.issued_base.store(0, Relaxed);
            lane.suppress_until_edge.store(0, Relaxed);
            lane.detect_streak.store(0, Relaxed);
            lane.last_seen_ms.store(now, Relaxed);
            return Some(LaneRef {
                lane,
                streaming: false,
            });
        }
        None
    }
}

/// Single-flight registry guard. Three-case drop semantics (§5.2, exact):
/// on the SUCCESS path the primary has already broadcast `Some(FillResult)`
/// and marked the guard `completed` — the drop is CLOSE-ONLY (no second
/// value; a late subscriber that raced between the send and the drop sees
/// `Err(Closed)`/`Err(Lagged)` and falls into the cache re-check loop). On
/// the FAILURE path (fetch error return) and on FUTURE-DROP mid-fetch
/// (caller cancelled), the un-`completed` guard sends `None` before
/// closing, so live waiters fail fast into the re-check loop (one becomes
/// the new primary) instead of waiting out a 50 ms slice.
struct InflightBlockReadGuard {
    key: String,
    inflight_block_reads:
        std::sync::Arc<scc::HashMap<String, tokio::sync::broadcast::Sender<Option<FillResult>>>>,
    tx: tokio::sync::broadcast::Sender<Option<FillResult>>,
    /// Set by the primary after a successful `send(Some(..))` — flips the
    /// drop from `None`-then-close to close-only.
    completed: std::cell::Cell<bool>,
}

impl Drop for InflightBlockReadGuard {
    fn drop(&mut self) {
        self.inflight_block_reads
            .remove_if_sync(&self.key, |current| current.same_channel(&self.tx));
        if !self.completed.get() {
            let _ = self.tx.send(None);
        }
    }
}

/// Process-default block size: `SQUEEZEFS_DEFAULT_BLOCK_SIZE` or 4 MiB.
///
/// This is the construction-time default [`DataRouter::new`] seeds
/// `block_size` with (a mount may raise it later via
/// [`DataRouter::set_block_size`] at FUSE init). The staging shard plan
/// ([`crate::cache::nvme`]) sizes its same-key replace headroom against
/// this value: staged entries are block-size-class by construction, so the
/// plan and the router must agree on the default.
pub fn default_block_size() -> u64 {
    std::env::var("SQUEEZEFS_DEFAULT_BLOCK_SIZE")
        .ok()
        .and_then(|val| val.parse::<u64>().ok())
        .unwrap_or(4 * 1024 * 1024)
}

/// RW1 mapping-form sanity classifier (docs/design-random-small-writes.md
/// §5.1 predicate 1 / review Issue 19 — the polarity tripwire, permanent).
///
/// Classifies a striped `block_map` entry's FORM for the attribution rig's
/// pinned sanity line:
///
/// * `"undecorated-2part"` — `persist_block_key`'s bare offset strings
///   (`offset` / `be://offset`), which `DataRouter::parse_block_mapping`
///   returns with `exact == false`: the whole-block form every ordinary
///   striped block carries — the W1-eligible population.
/// * `"decorated-3part"` — `bk:off:len` (promoted staged / spill / clip
///   publishes; `exact == true`, `len` load-bearing): the W1-INELIGIBLE
///   population (`patch_ineligible_decorated` in RW2).
///
/// Diagnostic only: RW2's `is_whole_block_mapping()` will be the single
/// eligibility predicate source; this classifier exists so the rig PRINTS
/// the fixture's actual form and the Issue-19 class of polarity inversion
/// (a predicate keyed on `exact == true` would patch NOTHING, the loss
/// dataset included) is caught at rig time, forever.
pub fn block_mapping_form(mapping_str: &str) -> &'static str {
    // The base key may itself contain `://` (non-default backends); the
    // decoration is parsed strictly AFTER that prefix — the same rule as
    // `parse_block_mapping`.
    let rest = match mapping_str.find("://") {
        Some(pos) => &mapping_str[pos + 3..],
        None => mapping_str,
    };
    match rest.split(':').count() {
        1 => "undecorated-2part",
        3 => "decorated-3part",
        _ => "unknown",
    }
}

/// **The single W1 predicate-1 source** (design-random-small-writes §5.1,
/// review Issue 19): `true` ⇔ `mapping_str` is the **undecorated 2-part
/// whole-block form** — the shape `persist_block_key` emits for every
/// ordinary striped block (bare `offset` / `be://offset` strings), which
/// `parse_block_mapping` returns as `(bk, 0, block_size, exact == false)`.
///
/// Polarity note (normative): eligibility must **NOT** be keyed on the
/// `exact` flag — `exact == true` marks precisely the decorated 3-part
/// `bk:off:len` form (promoted staged files, the *ineligible* population);
/// a predicate demanding it would patch nothing, the loss dataset
/// included. Call sites use THIS helper, never raw flag polarity; the
/// decorated form counts `patch_ineligible_decorated` and holes (unmapped)
/// count `patch_ineligible_unmapped`.
pub fn is_whole_block_mapping(mapping_str: &str) -> bool {
    // The base key may itself contain `://` (non-default backends); the
    // decoration is parsed strictly AFTER that prefix — the same rule as
    // `parse_block_mapping` / `block_mapping_form`.
    let rest = match mapping_str.find("://") {
        Some(pos) => &mapping_str[pos + 3..],
        None => mapping_str,
    };
    !rest.is_empty() && !rest.contains(':')
}

impl DataRouter {
    pub fn set_meta_backend(
        &self,
        meta_backend: std::sync::Arc<crate::meta_backend::RoutedMetaBackend>,
    ) {
        // Contract 6 (tests/async_block_reclaim_tests.rs): the reclaim
        // queue observes the SAME per-volume `failed` latch the
        // journal-barrier fail-stop escalation sets (what
        // `disabled_volumes` mirrors) — a fenced holder must never keep
        // issuing destructive discards from the blocking pool. Weak: the
        // probe must not extend the meta set's lifetime through the
        // router (a dropped backend reads as not-fenced, which is the
        // process-teardown shape).
        let weak = std::sync::Arc::downgrade(&meta_backend);
        self.backend_router
            .set_reclaim_fence_signal(std::sync::Arc::new(move || {
                weak.upgrade()
                    .map(|mb| mb.volumes.iter().any(|v| v.is_failed()))
                    .unwrap_or(false)
            }));
        let _ = self.inner.meta_backend.set(meta_backend);
    }

    /// Decode a staged/promoted block mapping. Two forms:
    ///
    /// * **Size-carrying** `bk:rel_off:packed_len` (what promotion / spill /
    ///   truncate-clip publish): `packed_len` is the EXACT stored transform
    ///   image length — `exact == true`. Without it a passthrough
    ///   (no-compression) image is unrecoverable from a whole-block read:
    ///   the trailing device bytes of a recycled block are indistinguishable
    ///   from payload (framed transforms self-delimit; passthrough is
    ///   byte-identity), which is how a promoted staged file's RMW seed
    ///   ballooned to `block_size` carrying a prior tenant's stale bytes
    ///   (`tests/staged_truncate_stale_tests.rs` churn test).
    /// * **Bare legacy** `bk`: pre-fix volumes — the caller must read the
    ///   whole block window and bound what it consumes (`exact == false`).
    ///
    /// The base key may itself contain `://` (non-default backends), so the
    /// decoration is parsed strictly AFTER that prefix.
    fn parse_block_mapping(&self, mapping_str: &str) -> Result<(u64, u64, usize, bool)> {
        // §5.6a quarantined mapping: fsck repair replaced this block with an
        // explicit damaged marker — reads are EIO by contract (never
        // fabricated zeros, never a stale-bytes serve). Every striped read
        // resolves its mapping through here, so this is the single choke
        // point.
        if is_damaged_mapping(mapping_str) {
            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )));
        }
        let default_size = self.block_size.load(Ordering::Acquire) as usize;
        let (prefix, rest) = match mapping_str.find("://") {
            Some(pos) => mapping_str.split_at(pos + 3),
            None => ("", mapping_str),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 3 {
            let bk = self
                .backend_router
                .parse_block_offset(&format!("{prefix}{}", parts[0]))?;
            let off = parts[1].parse::<u64>().unwrap_or(0);
            match parts[2].parse::<usize>() {
                Ok(sz) => Ok((bk, off, sz, true)),
                Err(_) => Ok((bk, off, default_size, false)),
            }
        } else {
            let bk = self.backend_router.parse_block_offset(mapping_str)?;
            Ok((bk, 0, default_size, false))
        }
    }

    pub(crate) async fn fetch_metadata_from_backend(
        &self,
        ino: u64,
    ) -> Result<Option<CachedMetadata>> {
        if let Some(backend) = self.inner.meta_backend.get() {
            let xattr_res = backend.getxattr(ino, "layout").await?;
            if let Some(bytes) = xattr_res {
                let layout_opt = if bytes.starts_with(b"{") {
                    serde_json::from_slice::<LayoutMetadata>(&bytes).ok()
                } else {
                    bincode::deserialize::<LayoutMetadata>(&bytes).ok()
                };
                if let Some(layout) = layout_opt {
                    // Write-commit-economy: the fetched value IS the
                    // persisted base — an inline bincode layout is
                    // delta-eligible (chain restarts at 0); JSON-era and
                    // indirect bases are full-Put only.
                    let base_is_json = bytes.starts_with(b"{");
                    let base_is_indirect = layout
                        .block_map_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("indirect:"));
                    let layout_delta_chain = if base_is_json || base_is_indirect {
                        LAYOUT_DELTA_CHAIN_INELIGIBLE
                    } else {
                        0
                    };
                    let mut block_map = layout.block_map.clone();
                    if let Some(ref map_id) = layout.block_map_id {
                        if map_id.starts_with("indirect:") {
                            let block_key = map_id.strip_prefix("indirect:").unwrap();
                            let block_size =
                                self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
                            let raw_bytes = self
                                .backend_router
                                .read_block(block_key, block_size)
                                .await?;
                            // Rewrite-publish-drain ledger (2026-08-01):
                            // every indirect map rehydrate is a whole-
                            // block device READ on the meta-fetch path —
                            // publish-pass base fetches pay it per pass
                            // on spilled maps.
                            METRICS
                                .layout_indirect_map_reads
                                .fetch_add(1, Ordering::Relaxed);
                            METRICS
                                .layout_indirect_map_read_bytes
                                .fetch_add(raw_bytes.len() as u64, Ordering::Relaxed);
                            // Rehydrate the persisted key strings VERBATIM:
                            // they are backend-true (`persist_block_key`
                            // output) and must resolve to the same device
                            // the bytes were written on.
                            let entries = decode_indirect_block_map(&raw_bytes).map_err(|e| {
                                log::error!(
                                    "ino {}: indirect block map at '{}' is unreadable: {}",
                                    ino,
                                    block_key,
                                    e
                                );
                                e
                            })?;
                            let mut map = std::collections::HashMap::with_capacity(entries.len());
                            for (b, key) in entries {
                                map.insert(b, key);
                            }
                            block_map = Some(map);
                        }
                    }
                    return Ok(Some(CachedMetadata {
                        file_type: layout.file_type.into(),
                        size: layout.size,
                        block_map_id: layout.block_map_id.map(Into::into),
                        block_prefix: layout.block_prefix.map(Into::into),
                        file_id: layout.file_id.map(Into::into),
                        cached_at: std::time::Instant::now(),
                        data_key: layout.data_key.map(bytes::Bytes::from),
                        block_map: block_map.map(std::sync::Arc::new),
                        layout_dirty: false,
                        layout_delta_chain,
                        // Lever A: backend-true at fetch — coherent with
                        // the CURRENT era by definition.
                        layout_base_token: self.inner.dlm.get_fencing_token_ino(ino),
                    }));
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn save_metadata_to_backend(
        &self,
        ino: u64,
        m: &CachedMetadata,
        fencing_token: u64,
    ) -> Result<()> {
        self.save_metadata_to_backend_ext(ino, m, fencing_token, None)
            .await
    }

    /// [`Self::save_metadata_to_backend`] with the write-commit-economy
    /// publish face: `publish_entries = Some(batch)` marks this save as
    /// a pure block-map-insert publish, eligible to persist as an
    /// O(batch) **layout delta record** instead of the re-serialized
    /// whole layout — when the caller half of the eligibility ladder
    /// holds (inline persisted base of known bincode provenance, chain
    /// under the cap, no indirect spill). Everything else about the
    /// save (fencing, indirect handling, RAM republish coherence) is
    /// identical.
    async fn save_metadata_to_backend_ext(
        &self,
        ino: u64,
        m: &CachedMetadata,
        fencing_token: u64,
        publish_entries: Option<&[(u32, String)]>,
    ) -> Result<()> {
        let backend = self.inner.meta_backend.get().ok_or_else(|| {
            SqueezefsError::InvalidOperation("Metadata backend not initialized".to_string())
        })?;

        // Fencing check
        let current_fencing = self.inner.dlm.get_fencing_token_ino(ino);
        if fencing_token < current_fencing {
            return Err(SqueezefsError::FencingTokenExpired {
                token: fencing_token,
                expected: current_fencing,
            });
        }

        // Publish decomposition (2026-08-01): per-save spans record on
        // publish-class saves only (the pipeline hot path under test) —
        // plain layout persists stay unrecorded.
        let is_publish = publish_entries.is_some();
        let t_encode = std::time::Instant::now();
        let mut old_indirect_to_free = None;
        // The layout as it would be persisted INLINE (block map retained under
        // the inline sentinel). The indirect branch below overwrites the map /
        // id only if the serialized value spills past the per-volume cap.
        let mut layout = LayoutMetadata {
            file_type: m.file_type.to_string(),
            size: m.size,
            block_map_id: m.block_map.as_ref().map(|_| format!("block_map_{}", ino)),
            block_prefix: m.block_prefix.as_deref().map(str::to_string),
            file_id: m.file_id.as_deref().map(str::to_string),
            data_key: m.data_key.as_ref().map(|b| b.to_vec()),
            block_map: m.block_map.as_deref().cloned(),
        };

        // §5.3: spill the inline block map to an indirect block only when the
        // serialized layout value would exceed the target volume's per-ino
        // record cap (`LAYOUT_INLINE_MAX = xattr_value_cap(ino) - 4 KiB framing
        // headroom`) — not a fixed entry count. At the 64 KiB default cap a
        // file whose block map serializes within ~60 KiB (≈ 6 GiB at 4 MiB
        // blocks) keeps an inline map. Beyond the cap the indirect mechanism
        // is used unchanged.
        let inline_bytes = bincode::serialize(&layout).map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize binary layout: {:?}", e),
            ))
        })?;
        let needs_indirect = m.block_map.is_some()
            && inline_bytes.len()
                > backend
                    .xattr_value_cap(ino)
                    .saturating_sub(LAYOUT_INLINE_HEADROOM);
        if is_publish {
            publish_phase_record(PublishPhase::SaveEncode, t_encode);
        }

        // PR VL6a: a freshly allocated indirect blob is registered
        // in-flight until this function's `set_layout_and_size` publishes
        // the layout naming it (the guard drops at function end).
        let t_blob = std::time::Instant::now();
        let mut _blob_inflight: Option<crate::block_allocator::InflightAllocGuard> = None;
        let bytes = if needs_indirect {
            let bm = m.block_map.as_ref().unwrap();
            // Backend-true spill (versioned v1 blob): the SAME key strings
            // the inline map persists go to disk verbatim. Reducing them to
            // bare offsets — the retired pre-versioned shape — lost the
            // owning volume, so every rehydrated non-first-volume block was
            // read/freed from the wrong device.
            let mut serialized_map = encode_indirect_block_map(bm)?;
            // The blob lives in ONE allocator block and the fetch path reads
            // exactly one block back. Overflow must fail LOUD here: writing
            // past the block would silently corrupt the neighboring
            // allocation.
            let map_block_size =
                self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
            if serialized_map.len() > map_block_size {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "indirect block map for ino {} ({} entries, {} B serialized) exceeds \
                         one {} B block",
                        ino,
                        bm.len(),
                        serialized_map.len(),
                        map_block_size
                    ),
                )));
            }
            let aligned_len = (serialized_map.len() + 4095) & !4095;
            serialized_map.resize(aligned_len, 0);

            // Allocate or reuse indirect block offset
            let mut reuse_info = None;
            if let Some(ref map_id) = m.block_map_id {
                if map_id.starts_with("indirect:") {
                    let old_block_key = map_id.strip_prefix("indirect:").unwrap();
                    if let Ok((be_id, off)) = self.backend_router.parse_block_key(old_block_key) {
                        reuse_info = Some((be_id, off));
                    }
                }
            }

            // VL4 (§5.4): an indirect blob may only be rewritten IN PLACE
            // on a placement-eligible volume. A blob living on a
            // Draining/Retired volume relocates to a fresh allocation (and
            // the old blob block is freed after the layout commit) — this
            // is how the mover's empty-merge "blob relocation" tasks and
            // every ordinary merge on a draining set migrate the map block
            // itself off the victim.
            let reuse_info = reuse_info.filter(|(be, _)| {
                self.backend_router
                    .volume_state_for_key_backend(be)
                    .is_none_or(|state| state == crate::VOL_STATE_ACTIVE)
            });
            let (be_id, offset, nvme_writer) = if let Some((be, off)) = reuse_info {
                let (_, dev) = self.backend_router.get_backend(&be)?;
                (be, off, dev)
            } else {
                if let Some(ref map_id) = m.block_map_id {
                    if let Some(old_block_key) = map_id.strip_prefix("indirect:") {
                        old_indirect_to_free = Some(old_block_key.to_string());
                    }
                }
                let (be, block_allocator, dev) = self.backend_router.get_active_backend()?;
                let off = block_allocator.allocate_block().await?;
                _blob_inflight = Some(block_allocator.inflight_register(off));
                (be, off, dev)
            };

            let block_key = self.backend_router.persist_block_key(&be_id, offset);
            let data_bytes = bytes::Bytes::from(serialized_map);
            let blob_len = data_bytes.len() as u64;
            nvme_writer.write_block(offset, data_bytes).await?;

            layout.block_map = None;
            layout.block_map_id = Some(format!("indirect:{}", block_key));

            let out = bincode::serialize(&layout).map_err(|e| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Failed to serialize binary layout: {:?}", e),
                ))
            })?;
            if is_publish {
                publish_phase_record(PublishPhase::BlobWrite, t_blob);
                METRICS
                    .publish_indirect_blob_bytes
                    .fetch_add(blob_len, Ordering::Relaxed);
            }
            out
        } else {
            // Collapsing back to (or staying) inline: free any old indirect
            // block; the inline layout value is already serialized above.
            if let Some(ref map_id) = m.block_map_id {
                if map_id.starts_with("indirect:") {
                    let old_block_key = map_id.strip_prefix("indirect:").unwrap();
                    old_indirect_to_free = Some(old_block_key.to_string());
                }
            }
            inline_bytes
        };

        // Write-commit-economy lever 2: a merge-class publish persists
        // as an O(batch) delta when the whole eligibility ladder holds —
        // the backend's half (live non-JSON base + incompat ratchet)
        // decides the rest and returns what it staged.
        let max_chain = layout_delta_max_chain();
        let delta_eligible = publish_entries.is_some()
            && !needs_indirect
            && max_chain > 0
            && m.layout_delta_chain < max_chain
            && !m
                .block_map_id
                .as_deref()
                .is_some_and(|id| id.starts_with("indirect:"));
        let t_commit = std::time::Instant::now();
        let delta_used = if delta_eligible {
            let delta = crate::layout_wire::LayoutDelta::from_final_state(
                &layout.file_type,
                m.size,
                layout.block_map_id.as_deref(),
                layout.block_prefix.as_deref(),
                layout.file_id.as_deref(),
                layout.data_key.as_deref(),
                publish_entries
                    .expect("delta_eligible requires entries")
                    .to_vec(),
            );
            // The full layout moves as `Bytes` (Lever B: the aggregated
            // conveyor parks it as the always-correct fallback — a move,
            // never a per-save copy).
            backend
                .merge_layout_and_size(ino, &delta, bytes::Bytes::from(bytes), m.size)
                .await?
        } else {
            backend.set_layout_and_size(ino, &bytes, m.size).await?;
            false
        };
        if is_publish {
            publish_phase_record(PublishPhase::MetaCommit, t_commit);
            // The full-save decision ledger (2026-08-01): why a
            // publish-class save fell off the O(batch) delta path —
            // rewrite-tax attribution. `delta_used` (not the caller-half
            // eligibility) is the arbiter so backend-half refusals land
            // in the ledger too.
            if !delta_used {
                let ctr = if needs_indirect {
                    &METRICS.publish_full_save_indirect
                } else if max_chain > 0
                    && m.layout_delta_chain != LAYOUT_DELTA_CHAIN_INELIGIBLE
                    && m.layout_delta_chain >= max_chain
                {
                    &METRICS.publish_full_save_chain_cap
                } else {
                    &METRICS.publish_full_save_other
                };
                ctr.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Keep hot cache coherent without a remove+refetch on the next write.
        let mut cached = m.clone();
        // Chain accounting (the caller half of the eligibility ladder):
        // a delta save deepens the chain; a full inline save re-bases it
        // (eligible at 0); an indirect save is full-Put-only until the
        // map collapses back inline.
        cached.layout_delta_chain = if delta_used {
            m.layout_delta_chain.saturating_add(1)
        } else if needs_indirect {
            LAYOUT_DELTA_CHAIN_INELIGIBLE
        } else {
            0
        };
        // The blob pointer must follow the SAVED layout (2026-07-27
        // write-pipeline campaign conviction, red in
        // tests/indirect_map_backend_keys_tests.rs): the indirect branch
        // above allocates/relocates the blob and names it ONLY in `layout`
        // — republishing the RAM entry with the caller's stale
        // `block_map_id` made the NEXT dirty-RAM-based merge see a
        // non-indirect id, allocate a fresh blob, and free nothing (one
        // leaked blob incarnation per merge; ~130 orphans per 700-block
        // spill burst measured). Sequential inline uploads masked it —
        // the pipeline's size-bump/merge interleaving exposed it.
        cached.block_map_id = layout.block_map_id.clone().map(Into::into);
        cached.cached_at = std::time::Instant::now();
        // Lever A (2026-08-01): the republished entry IS the just-
        // persisted state — coherent with the save's fencing era.
        cached.layout_base_token = fencing_token;
        self.metadata_cache.insert(ino, cached);

        if let Some(ref old_key) = old_indirect_to_free {
            let _ = self.backend_router.free_block(old_key).await;
        }

        Ok(())
    }

    pub fn new(
        dlm: DlmClient,
        cache: TieredCache,
        block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    ) -> Self {
        let block_size =
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(default_block_size()));
        let backend_router = std::sync::Arc::new(BackendRouter::new(
            block_allocator.clone(),
            nvme_writer.clone(),
            block_size.clone(),
        ));
        cache.set_backend_router(backend_router.clone());

        let fs_name = crate::fs_prefix();
        let redis_url = dlm.redis_url().to_string();
        backend_router.start_health_check_worker(redis_url, fs_name.to_string());

        // R1b admission mode (§5.3): env-resolved once; unrecognized values
        // refuse loud at mount (forward-only — no silent fallback). A zero
        // hot-tier budget auto-degrades SecondTouch to Always: without the
        // RAM landing zone, skipping publishes would refetch every sub-read.
        //
        // DEFAULT = `second-touch` (the §5.3 policy) as of PR 5: the R-5
        // evict-before-consume spiral that forced PR 4's temporary `always`
        // default is closed by this PR's per-lane resident-unconsumed
        // accounting + contention-scaled windows + AIMD collapse (pinned in
        // tests/read_prefetch_pipeline_tests.rs phase C/D — the spiral
        // shape stays bounded < 2x unique fetches by construction).
        let tier_admission = match std::env::var("SQUEEZEFS_READ_TIER_ADMISSION")
            .as_deref()
            .unwrap_or("second-touch")
        {
            "always" => TierAdmission::Always,
            "second-touch" => TierAdmission::SecondTouch,
            "never" => TierAdmission::Never,
            other => panic!(
                "SQUEEZEFS_READ_TIER_ADMISSION must be one of \
                 always|second-touch|never (got {other:?})"
            ),
        };
        let tier_admission =
            if tier_admission == TierAdmission::SecondTouch && cache.hot_block.max_bytes() == 0 {
                log::warn!(
                    "hot-block tier budget is 0: read-tier admission auto-degrades \
                 second-touch -> always (no RAM landing zone for skipped fills)"
                );
                TierAdmission::Always
            } else {
                tier_admission
            };

        // §5.5 pipeline knobs (env-resolved once; unrecognized values
        // refuse loud, forward-only).
        let prefetch_window_override = match std::env::var("SQUEEZEFS_READ_PREFETCH_WINDOW") {
            Ok(v) => Some(
                v.trim()
                    .parse::<u32>()
                    .unwrap_or_else(|e| {
                        panic!("SQUEEZEFS_READ_PREFETCH_WINDOW must be an integer: {e}")
                    })
                    .min(4096),
            ),
            Err(_) => None,
        };
        let prefetch_share_pct = match std::env::var("SQUEEZEFS_READ_PREFETCH_SHARE_PCT") {
            Ok(v) => v.trim().parse::<u64>().unwrap_or_else(|e| {
                panic!("SQUEEZEFS_READ_PREFETCH_SHARE_PCT must be an integer percent: {e}")
            }),
            Err(_) => 50,
        }
        .clamp(1, 100);
        // §5.6 ranged-read threshold (0 disables — the kill switch).
        let ranged_threshold = match std::env::var("SQUEEZEFS_READ_RANGED_THRESHOLD") {
            Ok(v) => v.trim().parse::<u64>().unwrap_or_else(|e| {
                panic!("SQUEEZEFS_READ_RANGED_THRESHOLD must be an integer byte count: {e}")
            }),
            Err(_) => 262_144,
        };
        // Hybrid I/O diagnostic escape (env half; `-o direct_device_true`
        // sets it post-construction from `start_mount`). "1"/"true" arms.
        let direct_device_true = std::env::var("SQUEEZEFS_DIRECT_DEVICE_TRUE")
            .map(|v| {
                let v = v.trim();
                v == "1" || v.eq_ignore_ascii_case("true")
            })
            .unwrap_or(false);
        if direct_device_true {
            log::info!(
                "Hybrid I/O escape armed (SQUEEZEFS_DIRECT_DEVICE_TRUE): O_DIRECT reads \
                 bypass the read tiers — no serve, no admission (device-true diagnostic)"
            );
        }

        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let total_memory = sys.total_memory();
        let metadata_capacity = std::cmp::max(10_000, total_memory / 200_000);

        let router = Self {
            inner: std::sync::Arc::new(DataRouterInner {
                dlm,
                meta_backend: once_cell::sync::OnceCell::new(),
                cache,
                block_allocator,
                nvme_writer,
                backend_router,
                block_size,
                // time_to_IDLE, not time_to_live: a `layout_dirty` entry is
                // the ONLY authority for its layout until the persist cadence
                // cleans it (see fetch_metadata) — expiry keyed on last WRITE
                // let an actively-READ dirty entry idle out and strand acked
                // payload behind a stale backend refill. Access-refreshed
                // expiry keeps any observed entry resident; staleness is
                // governed by `cached_at` (the 1 s freshness horizon), not by
                // residency.
                metadata_cache: moka::sync::Cache::builder()
                    .max_capacity(metadata_capacity)
                    .time_to_idle(std::time::Duration::from_secs(300))
                    .build_with_hasher(ahash::RandomState::new()),
                inflight_block_reads: std::sync::Arc::new(scc::HashMap::new()),
                stream_lanes: moka::sync::Cache::builder()
                    .max_capacity(100000)
                    .time_to_live(std::time::Duration::from_secs(30))
                    .build(),
                ghost: std::sync::Arc::new(GhostTable::new()),
                escalation_cooldown: std::sync::Arc::new(EscalationCooldown::new()),
                tier_admission,
                ranged_threshold,
                direct_device_true: std::sync::atomic::AtomicBool::new(direct_device_true),
                prefetch_window_override,
                prefetch_share_pct,
                read_lane: crate::read_lane::ReadLaneGovernor::from_env(),
                stream_gauge: StreamActivityGauge::new(),
                crypto: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
                prefetcher: std::sync::Arc::new(IoUringPrefetcher::new()),
                publish_conveyors: std::sync::Arc::new(scc::HashMap::new()),
            }),
        };
        // Merge-worker promotion commits layout through the router (weak:
        // the router owns the cache, never the reverse).
        router
            .cache
            .nvme
            .set_data_router(std::sync::Arc::downgrade(&router.inner));
        // Terminal-free read-tier purge (the generic/074 fstest.3
        // stale-fill fix — see BackendRouter::read_tier_purge). The
        // closure owns tier handles (Arc'd inners), not the router: no
        // cycle.
        {
            let cache = router.cache.clone();
            router
                .backend_router
                .set_read_tier_purge(std::sync::Arc::new(move |block_key: &str| {
                    // ALL FOUR block-key tiers (R4 §5.4) — the unified purge.
                    cache.purge_block_key(block_key);
                }));
        }
        router
    }

    /// Rehydrate a `DataRouter` from its inner Arc (merge-worker hook).
    pub(crate) fn from_inner(inner: std::sync::Arc<DataRouterInner>) -> Self {
        Self { inner }
    }

    pub fn set_crypto(&self, crypto: crate::crypto_compress::CryptoCompressState) {
        // §5.7 CRYPTO_SCRATCH_POOL: size the transform scratch off the
        // CONFIGURED block size (FUSE init calls `set_block_size` before
        // installing crypto). Passthrough states skip pool allocation; the
        // clone below shares the same pool (Arc-held).
        crypto
            .init_scratch_pool(self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize);
        let _ = self.crypto.set(crypto.clone());
        let _ = self.cache.nvme.crypto.set(crypto);
    }

    pub fn get_crypto(&self) -> &crate::crypto_compress::CryptoCompressState {
        static DEFAULT_CRYPTO: once_cell::sync::Lazy<crate::crypto_compress::CryptoCompressState> =
            once_cell::sync::Lazy::new(|| {
                crate::crypto_compress::CryptoCompressState::new(
                    "none".to_string(),
                    "none".to_string(),
                    None,
                )
            });
        self.crypto.get().unwrap_or(&*DEFAULT_CRYPTO)
    }

    /// Arm/disarm the hybrid-I/O diagnostic escape (`-o direct_device_true`
    /// parses in `start_mount`, after construction; the env half resolves
    /// in `new`). See the field doc for the contract.
    pub fn set_direct_device_true(&self, on: bool) {
        self.direct_device_true
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the device-true diagnostic escape is armed (mount-scoped).
    pub fn direct_device_true(&self) -> bool {
        self.direct_device_true
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the cold-stream read lane is armed (the
    /// `read_lane_armed` stats field; `SQUEEZEFS_READ_LANE=0` = off).
    pub fn read_lane_enabled(&self) -> bool {
        self.read_lane.enabled()
    }

    /// Aggregate in-flight lane-fetch bytes (the `read_lane_inflight_bytes`
    /// stats field and the R5 `read_lane_inflight` component source).
    pub fn read_lane_inflight_bytes(&self) -> u64 {
        self.read_lane.inflight_bytes()
    }

    pub async fn read_nvme_block(&self, block_key: &str) -> Result<bytes::Bytes> {
        // FIND-RW2-A (fixed in RW4): decorated `bk:off:len` mappings —
        // the promoted-staged form a striped block map can legitimately
        // carry — used to reach `parse_block_key` verbatim and fail
        // `Invalid block offset`, breaking every deferred-seed
        // materialize (and fold seed) over such blocks. Decode the
        // decoration here, at the single device-fetch funnel: read the
        // LBA-aligned window covering the EXACT stored image and slice it
        // (`read_promoted_staged_block`'s discipline), so passthrough
        // tails of recycled tenants are never served as payload.
        let (_base, off, sz, exact) = self.parse_block_mapping(block_key)?;
        if exact {
            // Window covering [off, off+sz), LBA-rounded (device reads are
            // O_DIRECT-aligned); every real publish uses off == 0.
            let read_len = (off as usize + sz).div_ceil(4096) * 4096;
            let cleaned = clean_block_key(block_key);
            let raw = self.backend_router.read_block(&cleaned, read_len).await?;
            let start = (off as usize).min(raw.len());
            let end = (off as usize + sz).min(raw.len());
            return Ok(raw.slice(start..end));
        }
        self.backend_router
            .read_block(block_key, self.device_block_window())
            .await
    }

    /// Device window for an UNDECORATED (whole-block) read. Passthrough
    /// volumes read exactly `block_size` (byte-identity — §5.6 ranged
    /// reads and the zero-copy raw-DMA leg depend on it). Transformed
    /// volumes read the FIND-RW4-A worst-case stored image — frame +
    /// AEAD envelope + a full raw-escape payload — rounded up to the
    /// 4 KiB LBA and clamped to the allocator chunk (the write-path
    /// guard bounds every stored image by the chunk, and the mount
    /// geometry gate guarantees the worst case fits it). Pre-fix the
    /// window was `block_size` exactly, which is what made every
    /// incompressible block unreadable ("malformed transform frame").
    pub(crate) fn device_block_window(&self) -> usize {
        let bs = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
        let crypto = self.get_crypto();
        if crypto.is_passthrough() {
            return bs;
        }
        (crypto.max_stored_image_len(bs).div_ceil(4096) * 4096)
            .min(crate::block_allocator::CHUNK_SIZE as usize)
    }

    pub fn set_block_size(&self, block_size: u64) {
        self.block_size
            .store(block_size, std::sync::atomic::Ordering::Relaxed);
    }

    async fn fetch_block_from_remote(&self, block_key: &str) -> Result<bytes::Bytes> {
        // read_fill_phase_ns: `fetch_dma` = the whole device-fetch await
        // (worker channel + device queue + service + oneshot wake — the
        // dev_queue/dev_service sub-spans record at the NvmeBlockDev
        // funnel); `decode` = the transform leg (passthrough ≈ 0).
        let t_dma = std::time::Instant::now();
        let raw = if let Some(dht) = self.cache.nvme.dht_node.get() {
            let client = crate::p2p::P2pClient::new();
            if let Ok(data) = client.download_block_from_peer(dht, block_key).await {
                bytes::Bytes::from(data)
            } else {
                self.read_nvme_block(block_key).await?
            }
        } else {
            self.read_nvme_block(block_key).await?
        };
        read_fill_phase_record(ReadFillPhase::FetchDma, t_dma);
        // Copy ledger: device DMA into the pooled fill intermediate — the
        // nvme-tcp RX-copy pricing denominator (raw device bytes; decode
        // below is a no-op on passthrough volumes).
        METRICS
            .read_fill_dma_bytes
            .fetch_add(raw.len() as u64, Ordering::Relaxed);

        let t_dec = std::time::Instant::now();
        let decompressed = self.get_crypto().process_read_async(raw).await?;
        read_fill_phase_record(ReadFillPhase::Decode, t_dec);
        Ok(decompressed)
    }

    /// Fetch a block by key without binding validation. Only for callers
    /// that cannot serve wrong-block bytes to anyone: cache warmers
    /// (prefetch) and single-key uses where the key is not resolved from a
    /// block map snapshot. Data-serving striped paths must go through
    /// [`Self::get_block_for_index`] instead (reused-key stale-fill family).
    pub async fn get_cached_or_fetch_block(
        &self,
        block_key: &str,
    ) -> Result<crate::cache::pool::ReadBlockValue> {
        Ok(self
            .get_cached_or_fetch_block_traced(block_key, FillClass::Demand)
            .await?
            .0)
    }

    /// [`Self::get_cached_or_fetch_block`] plus the fill's INCARNATION
    /// VALIDITY: `true` when the returned bytes provably belong to the key's
    /// current incarnation — a cache-tier hit (entries always hold current-
    /// incarnation bytes: validated fills publish-then-revalidate-then-undo,
    /// owners put fresh bytes or purge, displacement purges both tiers), an
    /// untracked legacy key (never freed/reallocated), or a device fill whose
    /// incarnation seqlock was stable before the read and unchanged after.
    /// `false` means the key transitioned (allocate/publish/free) during the
    /// device read: the bytes may be a dead incarnation's and the caller must
    /// re-resolve — serving them for a resolved block index is exactly the
    /// reused-key stale-fill corruption.
    ///
    /// `class`: the fill's provenance (see [`FillClass`]). Prefetch fills
    /// keep FULL ghost semantics (record + hit): with the pipeline
    /// fetching every block of every classified pass, a ghost BYPASS
    /// (tried first) meant a genuinely re-read stream never admitted to
    /// the disk tier — warm re-reads stayed device-bound forever
    /// (measured: 9.2 GiB/s vs the 16.6 lineage; R-1's mitigation stack
    /// broken). Same-pass fake heat is prevented MECHANICALLY instead:
    /// the one-lap clock grace + the progress-clocked quiescence keep one
    /// pass ≈ one miss per key (`prefetch_evicted_unconsumed` ≈ 0), so a
    /// second recorded miss is real cross-pass re-read heat regardless of
    /// which agent fetched.
    ///
    /// What the class changes (the transient stream window, 2026-07-29):
    /// a STREAM-class fill's ghost hit admits protected + publish only
    /// through [`AdmissionGovernor::allow_stream_admission`] — held
    /// transient (probation, publish skipped) under the clamp, with
    /// `DemandStream` fills funding the trickle via `note_foreground`
    /// (their whole-block fetch IS the workload's own device spend;
    /// `Prefetch` never self-funds — the ranged site's rule). Granted
    /// stream admissions enter marked (`put_protected_stream`) so their
    /// within-pass consumption never dilutes the waste ledger. A
    /// non-admitted `Prefetch` fill's hot put carries the one-lap grace
    /// (`put_probationary_referenced`) — clock parity with the consumed
    /// residue it races (never stickiness).
    async fn get_cached_or_fetch_block_traced(
        &self,
        block_key: &str,
        class: FillClass,
    ) -> Result<(crate::cache::pool::ReadBlockValue, bool)> {
        // §5.6a quarantined mapping: EIO before any tier probe (the damaged
        // key itself is never cache-published, but the contract is a loud
        // refusal at the first resolution point, not a miss-then-fetch).
        if is_damaged_mapping(block_key) {
            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )));
        }
        // Single-flight block fetch. Waiters must not hang if they miss the
        // completion broadcast (subscribe-after-send race under multi-thread
        // large sequential reads + prefetch). Always re-check caches and use a
        // bounded wait so FUSE cannot wedge permanently (also blocks .config).
        const WAIT_SLICE: Duration = Duration::from_millis(50);
        const MAX_WAIT: Duration = Duration::from_secs(60);
        // read_serve_phase_ns `sf_wait`: entry → served WITHOUT becoming
        // the primary, for any caller that subscribed to an in-flight
        // fill at least once (the deep-qd cohort-wait term). Primaries
        // record the fill chain instead (`read_fill_phase_ns`).
        let sf_t0 = std::time::Instant::now();
        let mut sf_waited = false;
        let deadline = std::time::Instant::now() + MAX_WAIT;

        loop {
            // R4 hot-block tier first (§5.4 probe order: overlay → hot →
            // read_lru → NVMe → device): `Bytes` refcount hit, zero copy.
            // Hot entries hold current-incarnation bytes by the same
            // argument as tier entries (device-validated fills + the
            // unified purge on every free), so a hit is serve-valid here;
            // block-serving callers additionally recheck the binding
            // exactly as for NVMe-tier hits.
            if let Some(cached_block) = self.cache.hot_block.get_no_promote(block_key) {
                // No-promote (R1b liveness, measured): this loop serves a
                // fill's own sub-read/waiter consumption — promotion here
                // manufactured protected victims out of one-pass streams,
                // whose dehydrations parked multi-MiB payloads in the
                // eviction channel (the PR 4 row-2 cage kill's third
                // head). Keep-worthiness evidence is ghost admission and
                // block-level re-access, never self-consumption.
                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                METRICS.hot_block_hits.fetch_add(1, Ordering::Relaxed);
                if sf_waited {
                    read_serve_phase_record(ReadServePhase::SfWait, sf_t0);
                }
                return Ok((
                    crate::cache::pool::ReadBlockValue::Bytes(cached_block),
                    true,
                ));
            }

            if let Some(cached_block) = self.cache.read_lru.get(block_key) {
                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                if sf_waited {
                    read_serve_phase_record(ReadServePhase::SfWait, sf_t0);
                }
                return Ok((
                    crate::cache::pool::ReadBlockValue::Bytes(cached_block),
                    true,
                ));
            }

            if let Some(cached_block) = self.cache.nvme.read_cached_block(block_key) {
                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                let bytes = bytes::Bytes::from(cached_block);
                // NO RAM re-promote: this NVMe→RAM copy would be a cache
                // publish under a possibly-reused key, and the incarnation
                // word cannot prove ENTRY provenance — a tier entry from a
                // key's dying incarnation (undo/purge still in flight, see
                // the fill below) would validate against the NEW owner's
                // stable word and stick its bytes in the RAM LRU until
                // remount (all-zero block reads; surfaced by PR 6 routing
                // aligned striped writes through the no-LRU-put
                // write-through path). The RAM LRU is filled only by
                // device-validated fills (below) and legitimate owners;
                // NVMe-tier hits stay NVMe-tier hits — the bytes still
                // serve this caller.
                if sf_waited {
                    read_serve_phase_record(ReadServePhase::SfWait, sf_t0);
                }
                return Ok((crate::cache::pool::ReadBlockValue::Bytes(bytes), true));
            }

            // Read-lane hold (2026-08-01): the anti-refetch serve for
            // cohort stragglers — a completed fill whose hot-probation
            // copy lost the clock race is still findable here until its
            // consumption coverage completes. Zero credit (this loop
            // cannot know the caller's slice; the single-block arm's
            // fast path and the primary-slice sites own the coverage
            // accounting). Hold entries hold current-incarnation bytes
            // by the hot-tier argument (validated deposits + unified
            // purge), and block-serving callers recheck the binding.
            if self.read_lane.enabled() {
                if let Some((held, ledger_visible)) = self
                    .cache
                    .read_lane_hold
                    .serve_with_provenance(block_key, 0)
                {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    METRICS.read_lane_serves.fetch_add(1, Ordering::Relaxed);
                    // Ledger visibility (the 2026-07-31 fix): a
                    // demand-deposited entry served here — every tier
                    // probe above missed — is the pre-lane refetch in
                    // serve form and carries the R1b ceremony with it
                    // (ghost touch, second-touch publish, hot
                    // re-landing). Lane deposits stay invisible.
                    if ledger_visible {
                        self.hold_serve_admission(block_key, &held, class).await;
                    }
                    if sf_waited {
                        read_serve_phase_record(ReadServePhase::SfWait, sf_t0);
                    }
                    return Ok((crate::cache::pool::ReadBlockValue::Bytes(held), true));
                }
            }

            if std::time::Instant::now() >= deadline {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("Timed out fetching block {}", block_key),
                )));
            }

            if let Some(entry) = self.inflight_block_reads.get_sync(block_key) {
                let tx = entry.get().clone();
                drop(entry);
                let mut rx = tx.subscribe();
                sf_waited = true;
                // Completion may have raced between get_sync and subscribe — recheck.
                if let Some(cached_block) = self
                    .cache
                    .hot_block
                    .get_no_promote(block_key)
                    .or_else(|| self.cache.read_lru.get(block_key))
                {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                    read_serve_phase_record(ReadServePhase::SfWait, sf_t0);
                    return Ok((
                        crate::cache::pool::ReadBlockValue::Bytes(cached_block),
                        true,
                    ));
                }
                if self.inflight_block_reads.get_sync(block_key).is_none() {
                    // Primary finished; loop to re-read caches.
                    continue;
                }
                match tokio::time::timeout(WAIT_SLICE, rx.recv()).await {
                    Ok(Ok(Some(res))) => {
                        // R1a (§5.2): served from the cohort's carried fill —
                        // no tier probe stands between a waiter and its
                        // bytes, so PR 4's publish-skipping classes cannot
                        // reintroduce the refetch churn. Same downstream
                        // rule as the primary: serve_valid=false makes the
                        // caller re-resolve the binding.
                        METRICS
                            .singleflight_waiter_result_serves
                            .fetch_add(1, Ordering::Relaxed);
                        read_serve_phase_record(ReadServePhase::SfWait, sf_t0);
                        return Ok((
                            crate::cache::pool::ReadBlockValue::Bytes(res.bytes),
                            res.serve_valid,
                        ));
                    }
                    Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
                        // Primary failed/cancelled (None), lagged, closed,
                        // or slice timeout — recheck caches; one waiter
                        // becomes the new primary.
                        continue;
                    }
                }
            }

            // Try to become the primary fetcher. Capacity 4 (design R-2):
            // exactly one terminal value is ever sent, so any capacity ≥ 1
            // suffices — the small bound caps per-receiver retained `Bytes`
            // clones (the old 64 was sized for repeated `()` wakeups that
            // no longer exist).
            let (tx, _rx) = tokio::sync::broadcast::channel(4);
            match self
                .inflight_block_reads
                .insert_sync(block_key.to_string(), tx.clone())
            {
                Ok(_) => {
                    METRICS.cache_misses.fetch_add(1, Ordering::Relaxed);
                    // read_fill_phase_ns `fill_total`: primary claim →
                    // fill complete — what a whole cohort waits on.
                    let fill_t0 = std::time::Instant::now();
                    let guard = InflightBlockReadGuard {
                        key: block_key.to_string(),
                        inflight_block_reads: self.inflight_block_reads.clone(),
                        tx,
                        completed: std::cell::Cell::new(false),
                    };

                    // Validated fill (block-key incarnation seqlock): block keys
                    // are offset strings, so a freed+reallocated offset reuses
                    // the SAME key string. A fill that raced an owner's
                    // COW-write/free can hold pre-write or hole-punched bytes
                    // (zeros); publishing them poisons the shared caches for the
                    // key's next owner until remount. Snapshot the incarnation
                    // before the device read and publish only if it is stable
                    // and unchanged after — otherwise hand the bytes to the
                    // caller uncached (transient, never sticky).
                    let incarnation = self.backend_router.fill_incarnation(block_key);
                    let downloaded = match self.fetch_block_from_remote(block_key).await {
                        Ok(b) => b,
                        Err(e) => {
                            // Guard drop still notifies waiters so they can retry/fail.
                            return Err(e);
                        }
                    };
                    let downloaded_bytes = downloaded;
                    // Untracked keys (legacy `…/part_N`, unknown backends) are
                    // never freed/reallocated: the fill is serve-valid by
                    // construction, though still never cache-published.
                    let mut serve_valid = !self.backend_router.key_incarnation_tracked(block_key);
                    let publishable = incarnation.filter(|&before| {
                        self.backend_router
                            .fill_incarnation_still(block_key, before)
                    });
                    if let Some(before) = publishable {
                        // read_fill_phase_ns `admission`: the ghost/
                        // governor decision + the awaited tier publish
                        // when admitted (skip ≈ 0 — the R1b posture).
                        let adm_t0 = std::time::Instant::now();
                        // R1b admission (§5.3): the disk-tier publish
                        // decision for the > 256 KiB population. FIRST
                        // touch skips (the 16.5 GiB-per-16 GiB tax kill —
                        // the fill still lands hot-tier probation below);
                        // a SECOND miss within the two-epoch ghost window
                        // publishes, so re-read heat converges to the
                        // tier. ≤ 256 KiB fills keep today's behavior
                        // verbatim (small-config population; their publish
                        // cost is noise). Skipping is always
                        // correctness-safe: absence ⇒ the next reader goes
                        // to the device; every RETAINED publish keeps the
                        // full validated-fill discipline untouched.
                        // Foreground basis (the transient stream window):
                        // a DemandStream fill's whole-block fetch is the
                        // workload's own device spend — under an engaged
                        // clamp it mints the admission trickle, so the
                        // bounded-waste law (waste ≤ fill_pct % of what
                        // the workload already pays) holds on the
                        // streaming shape exactly as it does for ranged.
                        // Prefetch never self-funds (the ranged site's
                        // no-self-funding rule).
                        if class == FillClass::DemandStream {
                            self.cache
                                .admission_governor
                                .note_foreground(downloaded_bytes.len() as u64);
                        }
                        let ghost_admit = if downloaded_bytes.len() <= 256 * 1024 {
                            true
                        } else {
                            match self.tier_admission {
                                TierAdmission::Always => true,
                                TierAdmission::Never => false,
                                // Speculative fills INCLUDED (see fn doc):
                                // a second recorded miss is real cross-pass
                                // heat; same-pass double-misses are
                                // prevented mechanically, not filtered.
                                TierAdmission::SecondTouch => {
                                    let hit = self.ghost.check_and_record(block_key);
                                    if hit {
                                        METRICS
                                            .read_tier_admission_ghost_hits
                                            .fetch_add(1, Ordering::Relaxed);
                                    }
                                    // The transient stream window
                                    // (2026-07-29): a classified stream's
                                    // ghost hit is not admission-by-right —
                                    // the governor arbitrates. Denied ⇒
                                    // probation + publish skipped (the
                                    // fill still serves its reader; the
                                    // ledger never sees the victim).
                                    hit && (!class.streaming()
                                        || self
                                            .cache
                                            .admission_governor
                                            .allow_stream_admission(downloaded_bytes.len() as u64))
                                }
                            }
                        };
                        if !ghost_admit {
                            METRICS
                                .read_fill_publishes_skipped
                                .fetch_add(1, Ordering::Relaxed);
                        } else if downloaded_bytes.len() >= 64 * 1024 {
                            METRICS.read_tier_admissions.fetch_add(1, Ordering::Relaxed);
                        }
                        if !ghost_admit {
                            // no disk publish
                        } else if downloaded_bytes.len() < 64 * 1024 {
                            let _ = self
                                .cache
                                .nvme
                                .cache_read_block(block_key, downloaded_bytes.clone());
                        } else {
                            let nvme_clone = self.cache.nvme.clone();
                            let backend_router = self.backend_router.clone();
                            let bk_clone = block_key.to_string();
                            let dl_clone = downloaded_bytes.clone();
                            // AWAITED publish — the fill is tier-visible
                            // BEFORE the single-flight guard drops (the
                            // read-tier refetch-churn fix): a detached
                            // publish could run arbitrarily late behind the
                            // blocking-pool backlog, and since >256 KiB
                            // blocks never enter the RAM LRU, the next
                            // sub-block read of this block missed every
                            // tier, found no in-flight entry, and refetched
                            // the whole block from the device — queueing
                            // yet another publish (the elbencho row-2 6.3×
                            // get_obj multiplier; see
                            // tests/read_tier_refetch_churn_tests.rs).
                            // Waiters woken by this guard's drop now
                            // re-check the tier and HIT instead of becoming
                            // fresh primaries. Still on the blocking pool:
                            // the put takes the tier-shard parking_lot
                            // write lock and moves megabytes (never on an
                            // async worker); awaiting the JoinHandle parks
                            // only this task, which holds no locks here.
                            // Incarnation discipline unchanged: re-check
                            // before AND after the put — the residual
                            // exposure stays a put→check instruction window
                            // that a whole free→allocate→DMA→publish cycle
                            // cannot fit inside. A JoinError (panic/
                            // shutdown) just loses the publish: the next
                            // read refetches — the pre-fix behavior, never
                            // a correctness loss.
                            let _ = tokio::task::spawn_blocking(move || {
                                // §5.2 test seam: one relaxed load per
                                // publish, zero-cost when unset — lets the
                                // churn suite hold a cohort open to prove
                                // waiter serves are publish-independent.
                                let delay_ms = TEST_TIER_PUBLISH_DELAY_MS
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                if delay_ms > 0 {
                                    std::thread::sleep(Duration::from_millis(delay_ms));
                                }
                                if !backend_router.fill_incarnation_still(&bk_clone, before) {
                                    return;
                                }
                                let _ = nvme_clone.cache_read_block(&bk_clone, dl_clone);
                                if !backend_router.fill_incarnation_still(&bk_clone, before) {
                                    nvme_clone.remove_cached_read_block(&bk_clone);
                                }
                            })
                            .await;
                        }
                        read_fill_phase_record(ReadFillPhase::Admission, adm_t0);
                        // read_fill_phase_ns `deposit`: the cache landing
                        // — hold deposit + RAM-LRU/hot put + the seqlock
                        // completion check below.
                        let dep_t0 = std::time::Instant::now();
                        // Read-lane hold deposit (2026-08-01): every
                        // > 256 KiB demand primary parks its completed
                        // fill in the ledger-invisible hold — the
                        // deep-qd cohort stability fix (a same-block
                        // straggler that lost this flight's window AND
                        // the hot clock race serves from here instead
                        // of refetching 4 MiB — the qd32 read_amp-1.41
                        // face). A `Bytes` refcount clone; retired by
                        // consumption coverage; purged with the tiers
                        // on movement (the undo below). Red pauses new
                        // deposits (R5 posture). `Prefetch`-class fills
                        // are EXCLUDED: R2's regime keeps its
                        // hot-probation landing and its
                        // evicted-unconsumed spiral detector verbatim
                        // (pinned by read_prefetch_pipeline_tests) —
                        // the lane's own fetches deposit in
                        // `lane_fetch_block`, the regimes stay
                        // disjoint.
                        if self.read_lane.enabled()
                            && class != FillClass::Prefetch
                            && downloaded_bytes.len() > crate::read_lane::READ_LANE_MIN_FILL_BYTES
                            && crate::mem_budget::level() != crate::mem_budget::Level::Red
                        {
                            // DEMAND provenance: serves of this entry
                            // carry the R1b ledger (the 2026-07-31
                            // ledger-visibility fix) — a hold serve
                            // after this fill's hot copy is evicted is
                            // the pre-lane refetch in serve form.
                            self.cache.read_lane_hold.insert_demand(
                                block_key,
                                downloaded_bytes.clone(),
                                self.read_lane_hold_budget(),
                            );
                        }
                        // Avoid flooding RAM LRU with full 4 MiB blocks under
                        // multi-GB sequential reads. Small blocks still cache.
                        if downloaded_bytes.len() <= 256 * 1024 {
                            self.cache.read_lru.put(block_key, downloaded_bytes.clone());
                        } else {
                            // R4 (§5.4): the > 256 KiB population finally
                            // gets a RAM tier — a `Bytes` refcount clone,
                            // never a copy. Device-validated fills ONLY:
                            // this put sits inside the publishable window
                            // and the undo below removes it on incarnation
                            // movement. NVMe-tier hits are never
                            // re-promoted here (same provenance argument
                            // as the read_lru no-repromote rule). Class
                            // (R1b): a ghost-admitted (re-read) fill
                            // enters PROTECTED — proven warmth; first-touch
                            // fills enter probation (first in eviction
                            // line; any read promotes in place, sticky).
                            METRICS.hot_block_misses.fetch_add(1, Ordering::Relaxed);
                            if ghost_admit && self.tier_admission == TierAdmission::SecondTouch {
                                if class.streaming() {
                                    // Governor-granted stream admission:
                                    // protected, MARKED — its within-pass
                                    // consumption credits no payback (see
                                    // put_protected_stream), so the clamp
                                    // keeps seeing beyond-budget stream
                                    // admissions honestly.
                                    self.cache
                                        .hot_block
                                        .put_protected_stream(block_key, downloaded_bytes.clone());
                                } else {
                                    self.cache
                                        .hot_block
                                        .put(block_key, downloaded_bytes.clone());
                                }
                            } else if class.streaming() {
                                // §5.5 pipeline fill: probation class WITH
                                // the one-lap clock grace — parity with
                                // consumed residue whose serves re-arm
                                // `referenced`; without it the clock evicts
                                // the pipeline's future to keep the
                                // stream's past (595 refetches on the
                                // row-2 shape, measured). The SAME
                                // argument covers a transient-window
                                // DemandStream fill (2026-07-29): its
                                // requester's remaining sub-reads consume
                                // it IMMEDIATELY, and without the grace it
                                // loses the clock race mid-consumption to
                                // sibling streams (measured on the
                                // sustained bracket: kern seq-1M −9 %,
                                // fetch ratio 1.19× vs the protected
                                // baseline's 1.07×).
                                self.cache.hot_block.put_probationary_referenced(
                                    block_key,
                                    downloaded_bytes.clone(),
                                );
                            } else {
                                self.cache
                                    .hot_block
                                    .put_probationary(block_key, downloaded_bytes.clone());
                            }
                        }
                        // Seqlock completion (publish-then-revalidate): the
                        // pre-publish check alone is check-then-act — this
                        // task can be preempted between it and the puts, and
                        // a put landing after a new owner's
                        // allocate→DMA→publish→purge sequence would stick
                        // the dead incarnation's bytes under the reused key
                        // (all-zero block reads until remount; surfaced by
                        // PR 6 routing aligned writes through the no-put
                        // write-through path). Undo on any movement: a
                        // poisoned entry is at worst transient — removed by
                        // the very task that published it — never sticky.
                        // The same final check is the fill's serve validity:
                        // word stable before the read and unchanged through
                        // the puts ⇒ the bytes are the key's current
                        // incarnation.
                        serve_valid = self
                            .backend_router
                            .fill_incarnation_still(block_key, before);
                        if !serve_valid {
                            // Undo across every tier this fill (or a racing
                            // sibling) could have published to — the
                            // unified purge (R4 §5.4).
                            self.cache.purge_block_key(block_key);
                        }
                        read_fill_phase_record(ReadFillPhase::Deposit, dep_t0);
                    }
                    // R1a (§5.2): hand the cohort its fill — after the
                    // publishes and the final still-check, so waiters
                    // receive exactly the primary's serve-validity verdict.
                    // `Bytes` clone = refcount bump. Then flip the guard to
                    // close-only: the success drop must never send a second
                    // value (a late subscriber that raced the send sees
                    // Closed/Lagged and is served by the cache re-check).
                    let _ = guard.tx.send(Some(FillResult {
                        bytes: downloaded_bytes.clone(),
                        serve_valid,
                    }));
                    guard.completed.set(true);
                    read_fill_phase_record(ReadFillPhase::FillTotal, fill_t0);
                    return Ok((
                        crate::cache::pool::ReadBlockValue::Bytes(downloaded_bytes),
                        serve_valid,
                    ));
                }
                Err(_) => {
                    // Lost the race to insert — loop and wait on the winner.
                    continue;
                }
            }
        }
    }

    /// R2 pipeline driver (§5.5), called at the top of every striped read:
    /// classifies the request through the file's K=4 offset lanes, does
    /// the lane's consume bookkeeping, records foreground-waits (window ×2
    /// growth), and tops the pipeline up to the contention-scaled
    /// effective window — every fetch through the result-carrying
    /// single-flight (dedupe with the foreground), fills landing hot-tier
    /// probation. Latch-free throughout (moka get + relaxed atomics +
    /// admitted spawns); racy-tolerant by design.
    ///
    /// `will_wait_inflight`: the caller observed this request's block key
    /// already in the single-flight registry — the reader caught the
    /// pipeline (§5.5's growth trigger: pipeline too shallow).
    ///
    /// `first_key`: the resolved block key of `start_block` (None = hole/
    /// unresolved). Feeds the CONSUME-TIME evicted-unconsumed detector:
    /// when the reader advances onto a block the pipeline ISSUED
    /// (`issued_base ≤ start_block < next_prefetch_block`), that fill must
    /// still be findable — hot tier, RAM LRU, NVMe read tier, or the
    /// single-flight registry (still in flight). Absent everywhere ⇒ the
    /// fill was evicted before its consumer arrived (§5.5's R-5 spiral
    /// signal): count `prefetch_evicted_unconsumed`, halve the window
    /// (AIMD), and arm the quiescence gate. The probe runs at most once
    /// per block (gated on the consume edge advancing), and only against
    /// the issued span — a serve-exit detector tried first counted the
    /// reader's own plan-rebase spans as evictions and froze healthy lanes
    /// (measured: issued ≈ 33, false `evicted_unconsumed` ≈ 1 700 on the
    /// clean row-2 shape).
    /// §5.5 pipeline kill switch: an explicit
    /// `SQUEEZEFS_READ_PREFETCH_WINDOW=0`.
    pub(crate) fn prefetch_disabled(&self) -> bool {
        self.prefetch_window_override == Some(0)
    }

    /// The effective §5.5 window cap: the explicit override verbatim,
    /// else the budget-derived default (`derived_prefetch_window_cap` —
    /// resolved per touch because the mount's block size lands after
    /// router construction).
    pub(crate) fn prefetch_window_cap(&self) -> u32 {
        match self.prefetch_window_override {
            Some(v) => v,
            None => derived_prefetch_window_cap(
                self.prefetch_share_pct,
                self.cache.hot_block.max_bytes(),
                self.block_size.load(Ordering::Relaxed),
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pipeline_touch(
        &self,
        file_path: &str,
        meta: &CachedMetadata,
        block_size: u64,
        offset: u64,
        len: u64,
        start_block: u32,
        end_block: u32,
        will_wait_inflight: bool,
        first_key: Option<&str>,
        lane_claim: bool,
    ) {
        if self.prefetch_disabled() || meta.file_type != "striped" {
            return;
        }
        // Borrowed-key get first: the warm ring path calls this per op
        // (op-economy: allocation-free), so the owned-key `get_with`
        // runs only on the entry-create miss (once per file) — and only
        // for claiming callers: a warm serve on a never-missed file has
        // no lanes to continue and must not mint any.
        let lanes = match self.stream_lanes.get(file_path) {
            Some(l) => l,
            None if lane_claim => self.stream_lanes.get_with(file_path.to_string(), || {
                std::sync::Arc::new(StreamLanes::new())
            }),
            None => return,
        };
        let (lane_idx, streaming) = {
            let Some(lane_ref) = lanes.observe(offset, len, lane_claim) else {
                return;
            };
            let idx = lanes
                .lanes
                .iter()
                .position(|l| std::ptr::eq(l, lane_ref.lane))
                .unwrap_or(0);
            (idx, lane_ref.streaming)
        };
        let lane = &lanes.lanes[lane_idx];
        use std::sync::atomic::Ordering::Relaxed;
        let generation = lane.generation.load(Relaxed);

        // Consume bookkeeping — per BLOCK ADVANCED, not per request: the
        // stream consuming block b means every pipeline-issued block in
        // [consumed_edge, end_block] is now behind the reader. (A
        // per-request draft drained `unconsumed` once per sub-read — 4x
        // too fast on the row-2 shape — over-issuing into the budget.)
        let issued_edge = lane.next_prefetch_block.load(Relaxed);
        let issued_base = lane.issued_base.load(Relaxed);
        let ce = lane.consumed_edge.load(Relaxed);
        if end_block >= ce {
            lane.consumed_edge.store(end_block + 1, Relaxed);
            if issued_edge > ce {
                let drain_from = ce.max(issued_base);
                let drain_to = end_block.min(issued_edge - 1);
                if drain_to >= drain_from {
                    for _ in 0..=(drain_to - drain_from) {
                        let _ = lane
                            .unconsumed
                            .fetch_update(Relaxed, Relaxed, |v| v.checked_sub(1));
                    }
                }
            }

            // Consume-time evicted-unconsumed detection (see doc comment).
            if start_block >= issued_base && start_block < issued_edge {
                if let Some(k) = first_key {
                    let resident = self.cache.hot_block.get_no_promote(k).is_some()
                        || self.cache.read_lru.get_no_promote(k).is_some()
                        || self.cache.nvme.has_cached_read_block(k)
                        // Read-lane hold residency (2026-08-01): a
                        // lane-held fill is findable — without this arm
                        // every lane-covered consume would fire the
                        // evicted-unconsumed detector and quiesce
                        // healthy lanes.
                        || (self.read_lane.enabled() && self.cache.read_lane_hold.contains(k))
                        || self.inflight_block_reads.read_sync(k, |_, _| ()).is_some();
                    if !resident {
                        METRICS.prefetch_evicted_unconsumed.fetch_add(1, Relaxed);
                        let _ = lane
                            .window
                            .fetch_update(Relaxed, Relaxed, |w| Some((w / 2).max(2)));
                        // Progress-clocked quiescence: pause issue until
                        // the consumer advances 2^(streak+1) blocks past
                        // this one (cap 64). Geometric growth is what
                        // separates a transient budget wobble (one short
                        // gap, streak resets on the next clean consume)
                        // from sustained starvation (spans double until
                        // the pipeline is effectively quiescent and the
                        // fetch ratio converges to ~1.0x).
                        let streak = lane.detect_streak.fetch_add(1, Relaxed).min(5);
                        let span = 2u32 << streak;
                        lane.suppress_until_edge
                            .store(end_block.saturating_add(1 + span), Relaxed);
                    } else {
                        // Clean consume of a pipelined block: starvation
                        // (if any) has cleared — re-arm fast response.
                        lane.detect_streak.store(0, Relaxed);
                    }
                }
            }
        }

        if !streaming {
            return;
        }

        // R5 advisory-at-admission (§5.7 — one relaxed load): Red stops
        // issue outright (the shed callback clears the plans; foreground
        // serves keep working); Yellow freezes window GROWTH but keeps
        // the pipeline alive at its current depth.
        let mem_level = crate::mem_budget::level();
        if mem_level == crate::mem_budget::Level::Red {
            return;
        }

        // Window growth (the AIMD up-edge, one pure decision —
        // `prefetch_window_grows`): foreground-wait, or a CLEAN plan
        // overrun. The overrun trigger is the read-saturation campaign's
        // addition: on the il path warm serves are silent consumption
        // (they never wait on the single flight), so the only depth
        // evidence a healthy ring stream produces is the reader passing
        // the whole issued plan — pre-campaign that shape re-based the
        // plan without ever growing it, pinning il pipelines at the
        // start window. `issued_edge`/`issued_base` were loaded above
        // (consume bookkeeping): `edge > base` = a plan existed;
        // `end_block ≥ edge` = the reader passed it.
        let overran = issued_edge > issued_base && end_block >= issued_edge;
        if prefetch_window_grows(
            will_wait_inflight,
            overran,
            lane.detect_streak.load(Relaxed),
            mem_level == crate::mem_budget::Level::Green,
        ) {
            METRICS.prefetch_foreground_waits.fetch_add(1, Relaxed);
            let cap = self.prefetch_window_cap();
            let _ = lane
                .window
                .fetch_update(Relaxed, Relaxed, |w| Some((w.saturating_mul(2)).min(cap)));
        }

        // Contention-scaled RESIDENT share (§5.5 mechanism ii). NO floor:
        // the doc's formula truncating to 0 is a signal, not an edge case —
        // when the per-lane share of the hot budget cannot retain even ONE
        // block, every speculative fill is guaranteed evicted-before-
        // consume, so the only non-wasteful window is empty (a .max(1)
        // floor here kept 4 contention-phase lanes thrashing a 2-block
        // budget: 104 fetches for 48 unique — measured). Since the
        // read-saturation campaign the share bounds LANDED-unconsumed
        // fills only; in-flight depth rides the AIMD window (see
        // `prefetch_issue_admits` — the split is the campaign's core
        // change: in-flight DMA bytes are R5-charged transients, not
        // hot-tier residents, and charging them here made the 16-stream
        // default shape a structurally depth-1 pipeline).
        let active = self.stream_gauge.touch(lane) as u64;
        let hot_budget = self.cache.hot_block.max_bytes();
        let resident_share =
            self.prefetch_share_pct * hot_budget / 100 / block_size.max(1) / active;
        let cap = self.prefetch_window_cap();
        let window = lane.window.load(Relaxed);
        // The plan bound (the gauge's meaning is unchanged: how deep the
        // pipeline may currently run).
        let plan_bound = u64::from(window.min(cap));
        let hwm = METRICS.prefetch_window_hwm.load(Relaxed);
        if plan_bound > hwm {
            METRICS.prefetch_window_hwm.store(plan_bound, Relaxed);
        }
        if resident_share == 0 {
            // R2 declines: the per-lane share of the hot budget cannot
            // retain even ONE landed block, so hot-landing speculation
            // is guaranteed evicted-before-consume (the §5.5 no-floor
            // rationale). THE READ LANE (2026-08-01 campaign) engages
            // exactly here — the field's 0.49×-of-raw plateau regime
            // (`.benchmarks/2026-07-31-fio-gap-accounting.md` §6.2):
            // pipelined whole-block fetches landing in the
            // ledger-invisible hold instead of the hot tier, so the
            // stream's next blocks are in flight while the current one
            // serves. Depth derives at runtime (BDP); the governor's
            // scan-resistance verdict stands untouched.
            self.read_lane_top_up(
                file_path, meta, block_size, end_block, &lanes, lane_idx, generation, active,
                mem_level,
            );
            return;
        }

        // (Re)base the plan when it is uninitialized or fell behind the
        // reader: issue restarts past this request. The skipped span was
        // never issued, so the covered window empties with the base.
        if lane.next_prefetch_block.load(Relaxed) <= end_block {
            lane.next_prefetch_block.store(end_block + 1, Relaxed);
            lane.issued_base.store(end_block + 1, Relaxed);
        }

        // Top up: landed-unconsumed bounded by the resident share,
        // in-flight + unconsumed bounded by the AIMD window (§5.5
        // mechanism i — issue stops; the spiral cannot start), and
        // gated by the progress-clocked quiescence arm (sustained
        // starvation pauses issue instead of re-feeding the evict cycle).
        if lane.consumed_edge.load(Relaxed) < lane.suppress_until_edge.load(Relaxed) {
            return;
        }
        let total_blocks = meta.size.div_ceil(block_size) as u32;
        loop {
            let in_flight = lane.inflight.load(Relaxed);
            let unconsumed = lane.unconsumed.load(Relaxed);
            if !prefetch_issue_admits(window, cap, resident_share, in_flight, unconsumed) {
                break;
            }
            let next = lane.next_prefetch_block.load(Relaxed);
            if next >= total_blocks {
                break;
            }
            if lane
                .next_prefetch_block
                .compare_exchange(next, next + 1, Relaxed, Relaxed)
                .is_err()
            {
                continue;
            }
            lane.inflight.fetch_add(1, Relaxed);
            METRICS.prefetch_issued.fetch_add(1, Relaxed);
            METRICS
                .prefetch_inflight_bytes
                .fetch_add(block_size, Relaxed);
            if !self.spawn_prefetch_task(
                file_path.to_string(),
                meta.clone(),
                next,
                block_size,
                lanes.clone(),
                lane_idx,
                generation,
            ) {
                // Admission shed the task un-run (P1-5: foreground always
                // wins): roll the issue-side accounting back HERE — the
                // task's own settle path never executes, and a leaked
                // `inflight` never drains (the lane would stall at
                // `in_flight + unconsumed >= effective` forever). The
                // block was not fetched: it stays foreground-served;
                // `prefetch_wasted` balances the task ledger so
                // `issued == completed + wasted` still converges.
                lane.inflight.fetch_sub(1, Relaxed);
                METRICS
                    .prefetch_inflight_bytes
                    .fetch_sub(block_size, Relaxed);
                METRICS.prefetch_wasted.fetch_add(1, Relaxed);
                break;
            }
        }
    }

    /// Ring-side stream feed (read-saturation campaign, 2026-07-29): the
    /// §5.3 classifier + §5.5 pipeline driver for IPC ring reads. The
    /// kernel lane feeds the lanes from the read handler; ring ops never
    /// reach it on the warm path (sync fast-path serves are silent
    /// consumption — without this feed the pipeline wedges at its
    /// window and, worse, a sequential il stream NEVER classifies, so
    /// every 4k miss pays one ranged fabric round trip via direct-drive
    /// forever: the field's burst-then-collapse signature, reproduced
    /// as `read_streams_classified = 0` / `prefetch_issued = 0` on the
    /// baseline rig rows).
    ///
    /// One call per ring read, at the sink, BEFORE the miss ladder runs
    /// — the 4th contiguous op's classification must veto direct-drive
    /// for the 5th (`ranged_eligible`'s streaming veto). Ops that then
    /// demote to the handler carry `ReadClassHint::lane_pre_fed` so the
    /// handler's `pipeline_touch` sites skip them (double observation
    /// declassifies — §5.3's 16-foreign rule).
    ///
    /// Latch-free, and allocation-free on the warm path (moka gets +
    /// lane atomics; the lanes entry allocates once per file). Callable
    /// from the foreign IPC service threads: the top-up's spawn rides
    /// the runtime-agnostic [`crate::bg_admit::spawn_bg`].
    /// `meta`: the caller's already-fetched metadata entry when it has
    /// one (the §5.5.1 probe's — a second per-op `metadata_cache.get`
    /// re-inflated the op-economy alloc pin via moka read-buffer
    /// housekeeping); `None` looks it up (rare paths only).
    pub fn ring_read_lane_touch(
        &self,
        meta: Option<&CachedMetadata>,
        ino: u64,
        offset: u64,
        len: u32,
        missed: bool,
    ) {
        if len == 0 || self.prefetch_disabled() {
            return;
        }
        let looked_up;
        let meta = match meta {
            Some(m) => m,
            None => {
                let Some(m) = self.metadata_cache.get(&ino) else {
                    return;
                };
                looked_up = m;
                &looked_up
            }
        };
        if meta.file_type != "striped" {
            return;
        }
        let block_size = self.block_size.load(Ordering::Relaxed);
        if block_size == 0 {
            return;
        }
        let end_offset = offset + u64::from(len);
        let start_block = (offset / block_size) as u32;
        let end_block = ((end_offset - 1) / block_size) as u32;
        let file_path = crate::keys::inode_path_stack(ino);
        // The evicted-unconsumed detector + the foreground-wait growth
        // trigger ride MISSES only (a warm serve is by definition
        // resident); warm serves keep the prelude lean — consume
        // bookkeeping + top-up need no key resolution.
        let (first_key, will_wait) = if missed {
            let k = meta
                .block_map
                .as_ref()
                .and_then(|m| m.get(&start_block))
                .map(String::as_str);
            let ww = k.is_some_and(|k| self.inflight_block_reads.read_sync(k, |_, _| ()).is_some());
            (k, ww)
        } else {
            (None, false)
        };
        self.pipeline_touch(
            &file_path,
            meta,
            block_size,
            offset,
            u64::from(len),
            start_block,
            end_block,
            will_wait,
            first_key,
            // Warm serves continue lanes, never claim them (the
            // warm-row tax — see `StreamLanes::observe`).
            missed,
        );
    }

    /// One §5.5 pipeline fetch: through the single-flight (dedupes with
    /// the foreground — R1a's guarantee), fills landing hot-tier probation
    /// via the admission table's prefetch row (ghost-bypassed —
    /// speculative). The GDS local-cache arm and the tier-resident
    /// MADV_WILLNEED arm of the legacy prefetcher are preserved verbatim.
    /// Stale-generation completions land as `prefetch_wasted` (their
    /// probationary put is refcount-cheap and first in eviction line —
    /// §5.5 deliberately skips io_uring cancel plumbing).
    ///
    /// Returns `spawn_bg`'s admission verdict: `false` = the task was
    /// shed un-run and the CALLER must roll back its issue accounting.
    #[allow(clippy::too_many_arguments)]
    fn spawn_prefetch_task(
        &self,
        file_path: String,
        meta: CachedMetadata,
        block: u32,
        block_size: u64,
        lanes: std::sync::Arc<StreamLanes>,
        lane_idx: usize,
        generation: u64,
    ) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let router = self.clone();
        crate::bg_admit::spawn_bg(async move {
            let lane = &lanes.lanes[lane_idx];
            let settle = |completed: bool| {
                lane.inflight.fetch_sub(1, Relaxed);
                METRICS
                    .prefetch_inflight_bytes
                    .fetch_sub(block_size, Relaxed);
                let live = lane.generation.load(Relaxed) == generation;
                if completed && live {
                    METRICS.prefetch_completed.fetch_add(1, Relaxed);
                    // Resident-unconsumed counts only fills the reader is
                    // still BEHIND: a slow fill completing after the
                    // consume edge passed its block has no drain left
                    // (the edge only moves forward) — counting it would
                    // occupy window budget forever and stall the lane.
                    if block >= lane.consumed_edge.load(Relaxed) {
                        lane.unconsumed.fetch_add(1, Relaxed);
                    }
                } else {
                    METRICS.prefetch_wasted.fetch_add(1, Relaxed);
                }
            };
            if lane.generation.load(Relaxed) != generation {
                settle(false);
                return;
            }
            let key = match router
                .load_striped_block_keys(&file_path, &meta, block, block)
                .await
            {
                Ok(mut keys) => match keys.pop().and_then(|(_, k)| k) {
                    Some(k) => k,
                    None => {
                        // Hole in the current map: nothing to warm.
                        settle(true);
                        return;
                    }
                },
                Err(err) => {
                    debug!("Prefetch: failed to resolve block key: {err:?}");
                    settle(false);
                    return;
                }
            };

            if router.cache.gds.is_available() {
                if let Some(local_path) = router.cache.gds.get_gds_path(&key) {
                    if !local_path.exists() {
                        debug!("Prefetch (GDS): scheduling download for block {key}");
                        if let Ok(downloaded) = router.fetch_block_from_remote(&key).await {
                            // P2-8: path-based cache write via the process
                            // io_uring file worker.
                            if let Err(e) =
                                crate::uring_fs::write_all(&local_path, downloaded.clone()).await
                            {
                                debug!("Prefetch (GDS): failed to write block {key}: {e:?}");
                            }
                        }
                        settle(true);
                        return;
                    }
                }
            }

            if let Some(guard) =
                router
                    .cache
                    .nvme
                    .get_cached_read_block_range_zero_copy(&key, 0, u32::MAX)
            {
                debug!("Prefetch (io_uring): page prefetch for block {key}");
                let addr = guard.as_ptr() as u64;
                let len = guard.len();
                router.prefetcher.prefetch(addr, len);
                settle(true);
                return;
            }

            // Speculative: full ghost semantics + the graced probation put
            // — see the get_cached_or_fetch_block_traced doc for why the
            // ghost bypass was rejected (warm-re-read convergence).
            match router
                .get_cached_or_fetch_block_traced(&key, FillClass::Prefetch)
                .await
            {
                Ok(_) => settle(true),
                Err(err) => {
                    debug!("Prefetch: failed to fetch block {key}: {err:?}");
                    settle(false);
                }
            }
        })
    }

    /// The hold's live insert-time byte budget (read-lane campaign):
    /// the issue path's cached consume-window derivation when the lane
    /// has run, else the floor-shaped derivation over the live stream
    /// gauge — see [`crate::read_lane::hold_budget_bytes`].
    pub(crate) fn read_lane_hold_budget(&self) -> u64 {
        let block_size = self.block_size.load(Ordering::Relaxed);
        let streams = METRICS
            .prefetch_active_streams
            .load(Ordering::Relaxed)
            .min(u64::from(u32::MAX)) as u32;
        // Fallback consume-window depth: the derived §5.5 window cap —
        // the cohort span a deep-qd reorder window can spread a
        // block's sub-reads across (no constants: share% × hot budget
        // / block, railed [4, 4096]).
        let window_depth = derived_prefetch_window_cap(
            self.prefetch_share_pct,
            self.cache.hot_block.max_bytes(),
            block_size,
        );
        self.read_lane
            .hold_budget()
            .max(crate::read_lane::hold_budget_bytes(
                crate::read_lane::effective_mem_budget(),
                block_size,
                streams,
                window_depth,
            ))
    }

    /// The R1b admission ceremony for a LEDGER-VISIBLE hold serve (the
    /// 2026-07-31 ledger-visibility fix): both hold serve sites fire
    /// only after every tier probe missed, so a serve of a
    /// demand-deposited entry stands in for exactly the device refetch
    /// the pre-lane read would have paid — and must touch the ledger
    /// the way that refetch did, or re-read heat never converges to
    /// the disk tier and the dehydration gate never sees a protected
    /// victim (the read_tier_admission_tests regression). This mirrors
    /// the primary-fill admission arm verbatim for the > 256 KiB
    /// population (hold entries are all > 256 KiB by the deposit
    /// gates): ghost check-and-record, second-touch publish (awaited,
    /// the refetch-churn discipline), governor arbitration for
    /// streaming classes, and the protected/probation hot landing —
    /// under the same incarnation seqlock discipline
    /// (publish-then-revalidate + the unified-purge undo). Lane-fetch
    /// deposits never reach this (scan-resistance stands — pinned by
    /// read_lane_tests contract 2).
    ///
    /// Two deliberate differences from the fill-path arm:
    ///   * no `note_foreground` — the serve pays ZERO device bytes,
    ///     and the governor's bounded-waste law prices admission
    ///     tokens against real device spend (the escalation-fetch
    ///     no-self-funding rule);
    ///   * no hold re-deposit / read_lru arm — the entry is already
    ///     held and always above the small-fill boundary.
    ///
    /// Cost posture: this runs only on ledger-visible hold serves —
    /// the shape that pre-lane paid a whole device fetch plus this
    /// exact ceremony — and the re-landed hot copy serves the block's
    /// remaining sub-reads, so per re-miss it runs at most ~once
    /// (concurrent same-key serves may duplicate it — the GhostTable's
    /// racy-tolerant class; puts and validated publishes are
    /// idempotent).
    async fn hold_serve_admission(&self, block_key: &str, held: &bytes::Bytes, class: FillClass) {
        // Mirror the fill path's publishable window: an unstable or
        // untracked incarnation skips the whole arm (no ledger touch,
        // no publish, no hot landing) — exactly as the primary fill
        // skips its admission arm outside the window.
        let incarnation = self.backend_router.fill_incarnation(block_key);
        let Some(before) =
            incarnation.filter(|&b| self.backend_router.fill_incarnation_still(block_key, b))
        else {
            return;
        };
        let ghost_admit = match self.tier_admission {
            TierAdmission::Always => true,
            TierAdmission::Never => false,
            TierAdmission::SecondTouch => {
                let hit = self.ghost.check_and_record(block_key);
                if hit {
                    METRICS
                        .read_tier_admission_ghost_hits
                        .fetch_add(1, Ordering::Relaxed);
                }
                // The transient stream window (2026-07-29): a
                // streaming-class re-miss stays governor-arbitrated —
                // a hold serve must not become a clamp bypass.
                hit && (!class.streaming()
                    || self
                        .cache
                        .admission_governor
                        .allow_stream_admission(held.len() as u64))
            }
        };
        if !ghost_admit {
            METRICS
                .read_fill_publishes_skipped
                .fetch_add(1, Ordering::Relaxed);
        } else {
            METRICS.read_tier_admissions.fetch_add(1, Ordering::Relaxed);
            // AWAITED publish, same rationale as the fill path (the
            // read-tier refetch-churn fix): the tier copy is visible
            // before this serve returns, so the admission-suite
            // contract "the SECOND miss publishes" is observable at
            // the serve boundary. Blocking pool for the same reason as
            // the fill path's put (tier-shard lock + megabytes moved).
            let nvme_clone = self.cache.nvme.clone();
            let backend_router = self.backend_router.clone();
            let bk_clone = block_key.to_string();
            let held_clone = held.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if !backend_router.fill_incarnation_still(&bk_clone, before) {
                    return;
                }
                let _ = nvme_clone.cache_read_block(&bk_clone, held_clone);
                if !backend_router.fill_incarnation_still(&bk_clone, before) {
                    nvme_clone.remove_cached_read_block(&bk_clone);
                }
            })
            .await;
        }
        // Hot landing — the pre-lane refetch's warmth propagation
        // (class ladder identical to the fill path's > 256 KiB arm).
        METRICS.hot_block_misses.fetch_add(1, Ordering::Relaxed);
        if ghost_admit && self.tier_admission == TierAdmission::SecondTouch {
            if class.streaming() {
                self.cache
                    .hot_block
                    .put_protected_stream(block_key, held.clone());
            } else {
                self.cache.hot_block.put(block_key, held.clone());
            }
        } else if class.streaming() {
            self.cache
                .hot_block
                .put_probationary_referenced(block_key, held.clone());
        } else {
            self.cache
                .hot_block
                .put_probationary(block_key, held.clone());
        }
        // Seqlock completion (publish-then-revalidate): undo across
        // every tier on movement — the unified purge, exactly the fill
        // path's rule.
        if !self
            .backend_router
            .fill_incarnation_still(block_key, before)
        {
            self.cache.purge_block_key(block_key);
        }
    }

    /// The read-lane issue path (2026-08-01 campaign — the §5.5
    /// zero-resident-share regime's pipeline): top the lane up to the
    /// BDP-derived per-stream depth with whole-block fetches through
    /// [`Self::lane_fetch_block`] — single-flight-deduped with the
    /// foreground, landing in the ledger-invisible hold, never a tier.
    /// Shares the R2 plan cursor (`next_prefetch_block`/`issued_base` —
    /// rebase-past-the-reader semantics verbatim) and the
    /// progress-clocked quiescence arm; carries its OWN in-flight
    /// bound (`rl_inflight`) and the aggregate R5 cap.
    #[allow(clippy::too_many_arguments)]
    fn read_lane_top_up(
        &self,
        file_path: &str,
        meta: &CachedMetadata,
        block_size: u64,
        end_block: u32,
        lanes: &std::sync::Arc<StreamLanes>,
        lane_idx: usize,
        generation: u64,
        active_streams: u64,
        mem_level: crate::mem_budget::Level,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        // The R1b size boundary, mirrored: ≤ 256 KiB blocks keep
        // today's behavior verbatim (they have a RAM tier already and
        // their share never truncates to 0 on real budgets).
        if !self.read_lane.enabled()
            || block_size <= crate::read_lane::READ_LANE_MIN_FILL_BYTES as u64
        {
            return;
        }
        // File-level single issuer (round 4 — see the StreamLanes field
        // doc): sibling classified lanes minted by reorder claims must
        // not each drive an ahead pipeline.
        {
            use std::sync::atomic::Ordering::Relaxed as R;
            let now = StreamLanes::now_ms();
            let owner = lanes.issue_owner_idx.load(R);
            let owner_ms = lanes.issue_owner_ms.load(R);
            if owner != lane_idx && owner != usize::MAX && now.saturating_sub(owner_ms) < 2_000 {
                return;
            }
            lanes.issue_owner_idx.store(lane_idx, R);
            lanes.issue_owner_ms.store(now, R);
        }
        let budget_cap =
            crate::read_lane::effective_mem_budget() / crate::read_lane::READ_LANE_BUDGET_DIVISOR;
        let lane = &lanes.lanes[lane_idx];
        // Ahead depth: the explicit pin only (default 0 — the campaign
        // brackets falsified ahead speculation on demand-concurrent
        // venues; see read_lane_depth_blocks). The hold keeps working
        // regardless: demand deposits + cohort serves are the measured
        // win.
        let streams = active_streams.min(u64::from(u32::MAX)) as u32;
        let depth = self.read_lane.depth_blocks(
            block_size,
            streams,
            mem_level == crate::mem_budget::Level::Red,
            budget_cap,
        );
        METRICS
            .read_lane_depth_target
            .store(u64::from(depth), Relaxed);
        if depth == 0 {
            return;
        }
        // Cache the consume-window hold budget for the deposit sites
        // (streams × depth are known only here).
        self.read_lane
            .set_hold_budget(crate::read_lane::hold_budget_bytes(
                crate::read_lane::effective_mem_budget(),
                block_size,
                streams,
                depth,
            ));
        // (Re)base the plan when it is uninitialized or fell behind the
        // reader (the R2 rule verbatim: the skipped span was never
        // issued, so it empties with the base).
        if lane.next_prefetch_block.load(Relaxed) <= end_block {
            lane.next_prefetch_block.store(end_block + 1, Relaxed);
            lane.issued_base.store(end_block + 1, Relaxed);
        }
        // Progress-clocked quiescence (§5.5): sustained starvation —
        // here, hold trims racing the consumer — pauses issue instead
        // of re-feeding the evict cycle.
        if lane.consumed_edge.load(Relaxed) < lane.suppress_until_edge.load(Relaxed) {
            return;
        }
        let total_blocks = meta.size.div_ceil(block_size) as u32;
        // The reader-tied horizon: issue only inside
        // (end_block, end_block + 1 + depth] — a skip-settled task (the
        // demand front already owns the flight) must not let the cursor
        // sprint to EOF fetching bytes the reader is minutes away from
        // (round-2 field lesson: an unbounded cursor turned the hold
        // into a 23.6 GiB FIFO with ~50 % of deposits evicted
        // unconsumed).
        let horizon = end_block.saturating_add(1).saturating_add(depth);
        loop {
            let in_flight = lane.rl_inflight.load(Relaxed);
            if !crate::read_lane::lane_issue_admits(
                in_flight,
                depth,
                self.read_lane.inflight_bytes(),
                block_size,
                budget_cap,
            ) {
                break;
            }
            let next = lane.next_prefetch_block.load(Relaxed);
            if next >= total_blocks || next > horizon {
                break;
            }
            if lane
                .next_prefetch_block
                .compare_exchange(next, next + 1, Relaxed, Relaxed)
                .is_err()
            {
                continue;
            }
            lane.rl_inflight.fetch_add(1, Relaxed);
            self.read_lane.add_inflight(block_size);
            if !self.spawn_read_lane_task(
                file_path.to_string(),
                meta.clone(),
                next,
                block_size,
                lanes.clone(),
                lane_idx,
                generation,
            ) {
                // Admission shed the task un-run (foreground always
                // wins): roll the issue accounting back HERE — the
                // task's settle path never executes.
                lane.rl_inflight.fetch_sub(1, Relaxed);
                self.read_lane.sub_inflight(block_size);
                METRICS.read_lane_wasted.fetch_add(1, Relaxed);
                break;
            }
        }
    }

    /// One read-lane fetch task: resolve the block's key, skip if the
    /// fill is already resident anywhere (hold/hot/LRU/tier/flight),
    /// else run the ledger-invisible fetch. Same admission
    /// (`spawn_bg`), generation fencing, and shed-rollback contract as
    /// [`Self::spawn_prefetch_task`]. Returns `spawn_bg`'s verdict:
    /// `false` = shed un-run, the CALLER rolls back.
    #[allow(clippy::too_many_arguments)]
    fn spawn_read_lane_task(
        &self,
        file_path: String,
        meta: CachedMetadata,
        block: u32,
        block_size: u64,
        lanes: std::sync::Arc<StreamLanes>,
        lane_idx: usize,
        generation: u64,
    ) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let router = self.clone();
        crate::bg_admit::spawn_bg(async move {
            let lane = &lanes.lanes[lane_idx];
            let settle = |completed: bool| {
                lane.rl_inflight.fetch_sub(1, Relaxed);
                router.read_lane.sub_inflight(block_size);
                if !completed || lane.generation.load(Relaxed) != generation {
                    METRICS.read_lane_wasted.fetch_add(1, Relaxed);
                }
            };
            if lane.generation.load(Relaxed) != generation {
                settle(false);
                return;
            }
            let key = match router
                .load_striped_block_keys(&file_path, &meta, block, block)
                .await
            {
                Ok(mut keys) => match keys.pop().and_then(|(_, k)| k) {
                    Some(k) => k,
                    None => {
                        // Hole in the current map: nothing to fetch.
                        settle(true);
                        return;
                    }
                },
                Err(err) => {
                    debug!("read-lane: failed to resolve block key: {err:?}");
                    settle(false);
                    return;
                }
            };
            // Already resident or in flight: the reader will find it.
            if router.cache.read_lane_hold.contains(&key)
                || router.cache.hot_block.get_no_promote(&key).is_some()
                || router.cache.read_lru.get_no_promote(&key).is_some()
                || router.cache.nvme.has_cached_read_block(&key)
                || router
                    .inflight_block_reads
                    .read_sync(&key, |_, _| ())
                    .is_some()
            {
                settle(true);
                return;
            }
            match router.lane_fetch_block(&key).await {
                Ok(()) => settle(true),
                Err(err) => {
                    debug!("read-lane: failed to fetch block {key}: {err:?}");
                    settle(false);
                }
            }
        })
    }

    /// The LEDGER-INVISIBLE whole-block fetch (read-lane campaign): a
    /// single-flight-registered device fetch that deposits its
    /// validated fill in the hold and hands the cohort its
    /// `FillResult` — and does NOTHING else. No ghost recording, no
    /// admission governor arbitration or ledger movement, no hot/LRU/
    /// NVMe-tier publication: the 2026-07-26 scan-resistance verdict
    /// stands, the lane adds fetch concurrency, never tier residency.
    ///
    /// Correctness is the validated-fill discipline verbatim:
    /// incarnation snapshot before the device read, deposit only while
    /// stable, still-check after, unified purge on movement (the hold
    /// is a `purge_block_key` arm). Waiters receive the same
    /// serve-validity verdict as every primary; block-serving callers
    /// recheck the binding downstream.
    async fn lane_fetch_block(&self, block_key: &str) -> Result<()> {
        // §5.6a quarantined mapping: same loud-refusal contract as the
        // cached fetch path.
        if is_damaged_mapping(block_key) {
            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )));
        }
        let (tx, _rx) = tokio::sync::broadcast::channel(4);
        if self
            .inflight_block_reads
            .insert_sync(block_key.to_string(), tx.clone())
            .is_err()
        {
            // Another fetcher owns the flight — the reader dedupes onto
            // it; nothing for the lane to do.
            return Ok(());
        }
        METRICS.cache_misses.fetch_add(1, Ordering::Relaxed);
        let guard = InflightBlockReadGuard {
            key: block_key.to_string(),
            inflight_block_reads: self.inflight_block_reads.clone(),
            tx,
            completed: std::cell::Cell::new(false),
        };
        let incarnation = self.backend_router.fill_incarnation(block_key);
        let downloaded = self.fetch_block_from_remote(block_key).await?;
        // Guard drop broadcasts None on the error path above (waiters
        // fail fast into the re-check loop — the §5.2 contract).
        let mut serve_valid = !self.backend_router.key_incarnation_tracked(block_key);
        if let Some(before) =
            incarnation.filter(|&b| self.backend_router.fill_incarnation_still(block_key, b))
        {
            // Deposit inside the publishable window; Red pauses new
            // deposits (the R5 posture — in-flight/hold converge).
            if crate::mem_budget::level() != crate::mem_budget::Level::Red {
                self.cache.read_lane_hold.insert(
                    block_key,
                    downloaded.clone(),
                    self.read_lane_hold_budget(),
                );
            }
            // Publish-then-revalidate (the seqlock completion rule):
            // movement purges the deposit — never sticky.
            serve_valid = self
                .backend_router
                .fill_incarnation_still(block_key, before);
            if !serve_valid {
                self.cache.purge_block_key(block_key);
            }
        }
        METRICS.read_lane_fetches.fetch_add(1, Ordering::Relaxed);
        METRICS
            .read_lane_fetch_bytes
            .fetch_add(downloaded.len() as u64, Ordering::Relaxed);
        let _ = guard.tx.send(Some(FillResult {
            bytes: downloaded,
            serve_valid,
        }));
        guard.completed.set(true);
        Ok(())
    }

    /// The CURRENT block-index→key binding of `(file_path, b)`, as fresh as
    /// the last completed block-map merge. The RAM `metadata_cache` entry is
    /// consulted WITHOUT the TTL gate: every merge/save republishes the entry
    /// under `INODE_META_LOCKS` (`save_metadata_to_backend`) strictly BEFORE
    /// its caller frees the displaced keys, so any entry present here is at
    /// least as fresh as every merge whose displaced key could have been
    /// reallocated by the time this runs. On a miss, `fetch_metadata`'s
    /// refill reads the backend under the same lock (serialized ≥ merges).
    async fn current_block_binding(&self, file_path: &str, b: u32) -> Result<Option<String>> {
        let ino = parse_inode_from_path(file_path);
        let meta = match self.metadata_cache.get(&ino) {
            Some(m) => m,
            None => self.fetch_metadata(file_path).await?,
        };
        if meta.block_map.is_none() && meta.block_map_id.is_none() && meta.block_prefix.is_none() {
            // The file no longer carries a striped layout (concurrent
            // truncate/delete/layout flip): every striped binding is gone.
            return Ok(None);
        }
        let mut keys = self.load_striped_block_keys(file_path, &meta, b, b).await?;
        Ok(keys.pop().and_then(|(_, k)| k))
    }

    /// Hybrid-I/O diagnostic fetch (the `direct_device_true` escape): one
    /// whole-block DEVICE fetch carrying the validated-fill incarnation
    /// discipline with ZERO cache interaction — no tier/hot/read_lru
    /// probes or puts, no single-flight registration (N concurrent
    /// diagnostic readers = N device reads, by contract: the measurement
    /// must see the device, not each other), no ghost recording, and no
    /// cache_hits/cache_misses accounting (a diagnostic must not perturb
    /// the live policy's signals). Decode (`process_read`) still applies —
    /// transform volumes serve plaintext. The returned bool is the same
    /// serve-validity verdict as `get_cached_or_fetch_block_traced`:
    /// incarnation stable before the read and unchanged after.
    async fn fetch_block_device_true(
        &self,
        block_key: &str,
    ) -> Result<(crate::cache::pool::ReadBlockValue, bool)> {
        // §5.6a quarantined mapping: same EIO contract as the cached path.
        if is_damaged_mapping(block_key) {
            return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                libc::EIO,
            )));
        }
        let tracked = self.backend_router.key_incarnation_tracked(block_key);
        let before = self.backend_router.fill_incarnation(block_key);
        let bytes = self.fetch_block_from_remote(block_key).await?;
        let serve_valid = !tracked
            || before.is_some_and(|bf| self.backend_router.fill_incarnation_still(block_key, bf));
        Ok((
            crate::cache::pool::ReadBlockValue::Bytes(bytes),
            serve_valid,
        ))
    }

    /// BINDING-VALIDATED striped block serve — the reused-key stale-fill fix
    /// (the `8e3995e` follow-up). Block keys are device-offset strings; the
    /// incarnation seqlock validates KEY↔CONTENT for cache publishes but
    /// cannot protect a reader whose BLOCK-INDEX→KEY resolution went stale:
    /// after displace→free→reallocate the key legitimately holds the NEW
    /// owner's bytes, a fill of them validates perfectly, and serving them
    /// for the resolved index returns another block's content (the rare
    /// generic/075.2 soak corruption — stale data exactly one block over at
    /// the same intra-block offset; zeros when the key was freed but not yet
    /// reused).
    ///
    /// Serve rule: bytes obtained for key K are returned for block `b` only
    /// when (a) the fetch was incarnation-valid (tier hit, untracked key, or
    /// seqlock stable-and-unchanged across the device read) AND (b) the
    /// CURRENT map still binds `b → K` once the bytes are in hand. If K were
    /// rebound to `b` after its observed incarnation died, it must have been
    /// freed (retire, gen+1) and republished (gen+1) in between — (a) would
    /// have failed — so (a) ∧ (b) proves the serve is the block's current
    /// content, linearized at the recheck. On any movement: re-resolve the
    /// binding and retry (`Ok(None)` = the block is a hole in the current
    /// map — the caller serves zeros). Latch-free: the recheck is a moka get
    /// + scc reads on the hot path.
    ///
    /// `resolved_key` is the caller's (possibly stale) map resolution;
    /// `None` short-circuits to a hole.
    ///
    /// `device_true` (hybrid-I/O escape): the fetch bypasses every cache
    /// tier and the single-flight (`fetch_block_device_true`) while
    /// keeping this exact binding proof. Default-path callers pass
    /// `false`.
    ///
    /// `escalate_contended` (VL8 item 7; FIND-RW5-A extension): after two
    /// losses of EITHER face — MOVEMENT (the binding changed — a COW
    /// rewrite displaced the key) or CONTENTION (the binding is unchanged
    /// but the key's incarnation word keeps moving — a patch storm, or
    /// allocator free-list reuse cycling the same offset through other
    /// custody's alloc→DMA→publish) — the fetch serializes under the
    /// block's `BLOCK_FLUSH_LOCKS` stripe AND goes DEVICE-DIRECT
    /// (`fetch_block_device_true`): a cohort fill inherited from the
    /// shared single-flight was produced outside the stripe and
    /// re-imports the invalidity the stripe excludes (the generic/464
    /// rebind-exhaustion EIO, .benchmarks/2026-07-21-wedge-and-074-
    /// fixes.md). All faces additionally get a bounded exponential
    /// backoff tail past the fast attempts. MUST be `false` at call
    /// sites that may already hold this block's stripe (seed fetches
    /// under a held block guard; write-side RMW seeds) — the stripe is
    /// not reentrant.
    /// Lock-order: acquires (3) only, after any caller-held (1) — legal
    /// under the P1-9 order; nothing below takes (1)/(3)/(4).
    pub async fn get_block_for_index(
        &self,
        file_path: &str,
        b: u32,
        resolved_key: Option<&str>,
        device_true: bool,
        escalate_contended: bool,
    ) -> Result<Option<crate::cache::pool::ReadBlockValue>> {
        // Each retry re-resolves against the freshest map. Exhaustion
        // fails loud rather than serving unproven bytes — but only after
        // the FIND-RW5-A liveness ladder (the generic/464 dominant EIO
        // face: `fill_valid=false` on an UNCHANGED binding, 8 fast losses
        // → user EIO; diagnostic tape /tmp/rw5a_diag/):
        //
        //  1. fast attempts (unchanged), then a bounded exponential
        //     backoff tail — allocator free-list offset reuse cycles one
        //     device offset through alloc→DMA→publish under OTHER
        //     writers' custody, so a loser needs the storm to pause, not
        //     more instant retries;
        //  2. escalated attempts (the VL8 item-7 stripe hold, extended to
        //     BOTH loss faces) fetch DEVICE-DIRECT: the cohort fill of
        //     the shared single-flight was produced OUTSIDE the stripe by
        //     a non-escalated reader, so inheriting it re-imports exactly
        //     the invalidity the stripe was taken to exclude. The direct
        //     fetch (the `fetch_block_device_true` primitive) carries the
        //     full incarnation discipline, publishes nothing, and runs
        //     serialized — writers of this block that hold the stripe are
        //     excluded for its whole window.
        const MAX_REBINDS: usize = 24;
        const BACKOFF_AFTER: usize = 4;
        const CONTENDED_BEFORE_ESCALATE: usize = 2;
        // Fill provenance (the transient stream window, 2026-07-29):
        // computed ONCE per call — a file holding a fresh §5.3 streaming
        // classification fills as DemandStream (its ghost hits are
        // governor-arbitrated and its device fetches fund the trickle).
        // One moka get per FILL, never per warm op.
        let fill_class = if !device_true
            && self
                .stream_lanes
                .get(file_path)
                .is_some_and(|lanes| lanes.any_streaming_fresh())
        {
            FillClass::DemandStream
        } else {
            FillClass::Demand
        };
        // Never-invisible hole revalidation (fstests generic/795): a None
        // binding handed in from the caller's ENTRY-TIME map snapshot may
        // be stale-absent — a concurrent write-through / writeback publish
        // (which updates the RAM metadata_cache strictly BEFORE retiring
        // the overlay / staged sibling the caller already probed) can land
        // inside the caller's read window, leaving the acked bytes
        // invisible to both the snapshot and the retired tiers. Re-resolve
        // against the freshest map before any hole verdict; only a
        // fresh-map absence serves zeros. (Rebind-loop absences below are
        // fresh by construction — they come from current_block_binding.)
        let mut key: Option<String> = match resolved_key {
            Some(k) => Some(k.to_string()),
            None => self.current_block_binding(file_path, b).await?,
        };
        let mut losses = 0usize;
        for attempt in 0..MAX_REBINDS {
            let Some(cur_key) = key else {
                return Ok(None);
            };
            if attempt >= BACKOFF_AFTER {
                let shift = (attempt - BACKOFF_AFTER).min(3) as u32;
                tokio::time::sleep(Duration::from_millis(1u64 << shift)).await;
            }
            // VL8 item 7 escalation (FIND-RW5-A extension: EITHER loss
            // face): hold the block's stripe across one DIRECT fetch so no
            // stripe-holding writer can overlap it and no cohort fill can
            // stand in for it.
            let escalated = escalate_contended && losses >= CONTENDED_BEFORE_ESCALATE;
            let _contention_guard = if escalated {
                let ino = parse_inode_from_path(file_path);
                Some(
                    crate::fuse_client::block_lock_acquire(
                        ino,
                        b,
                        crate::fuse_client::BlockLockSite::ReadEscalate,
                    )
                    .await,
                )
            } else {
                None
            };
            let fetched = if device_true || escalated {
                self.fetch_block_device_true(&cur_key).await
            } else {
                self.get_cached_or_fetch_block_traced(&cur_key, fill_class)
                    .await
            };
            let (val, incarnation_valid) = match fetched {
                Ok(x) => x,
                Err(e) => {
                    // A fetch/DECODE failure on a NON-current binding is a
                    // stale-binding LOSS, not corruption: a displaced+freed
                    // offset is legally reused mid-read, and on transformed
                    // volumes the dead incarnation's bytes fail frame/AEAD
                    // decode (the reads_mid_fold LZ4 EINVAL — the
                    // never-invisible fold widened readers into the
                    // displace/free/reuse window). Rebind exactly like a
                    // wrong-bytes fill; propagate only when the CURRENT map
                    // still binds this key (a real I/O/corruption error).
                    let current = self.current_block_binding(file_path, b).await?;
                    if current.as_deref() == Some(cur_key.as_str()) {
                        return Err(e);
                    }
                    losses += 1;
                    METRICS
                        .stale_binding_rebinds
                        .fetch_add(1, Ordering::Relaxed);
                    debug!(
                        "stale-binding rebind (fetch error): file={} block={} key={} \
                         current={:?} err={e}",
                        file_path, b, cur_key, current
                    );
                    key = current;
                    continue;
                }
            };
            // Recheck the binding only AFTER the bytes are in hand: the
            // proof needs (movement between snapshot and serve) ⇒ (word
            // changed), which only holds when the recheck follows the read.
            let recheck_delay = TEST_BINDING_RECHECK_DELAY_MS.load(Ordering::Relaxed);
            if recheck_delay > 0 {
                tokio::time::sleep(Duration::from_millis(recheck_delay)).await;
            }
            let bind_t0 = std::time::Instant::now();
            let current = self.current_block_binding(file_path, b).await?;
            read_serve_phase_record(ReadServePhase::BindingCheck, bind_t0);
            if incarnation_valid && current.as_deref() == Some(cur_key.as_str()) {
                return Ok(Some(val));
            }
            losses += 1;
            METRICS
                .stale_binding_rebinds
                .fetch_add(1, Ordering::Relaxed);
            debug!(
                "stale-binding rebind: file={} block={} resolved_key={} current={:?} fill_valid={} escalated={}",
                file_path, b, cur_key, current, incarnation_valid, escalated
            );
            key = current;
        }
        Err(SqueezefsError::Io(std::io::Error::other(format!(
            "block {b} of {file_path} did not settle after {MAX_REBINDS} binding rebinds"
        ))))
    }

    /// §5.6 dispatch rule (fetch granularity policy): sub-block ranged
    /// device reads fire only for passthrough volumes (decode needs the
    /// whole physical block otherwise — the check is static per mount),
    /// requests at or under the threshold (`SQUEEZEFS_READ_RANGED_THRESHOLD`,
    /// 0 = kill switch), NON-streaming files (streams want whole blocks:
    /// 1.0× amplification + the pipeline; the freshness-gated classifier
    /// probe costs one moka get), and blocks larger than the threshold
    /// (small-block volumes keep the whole-block path — their fetch is
    /// already request-sized and stays RAM-LRU-cacheable). Callers invoke
    /// this strictly AFTER the overlay/hot/tier probes missed.
    pub(crate) fn ranged_eligible(&self, file_path: &str, request_len: u64) -> bool {
        self.ranged_threshold != 0
            && request_len <= self.ranged_threshold
            && self.block_size.load(Ordering::Relaxed) > self.ranged_threshold
            && self.get_crypto().is_passthrough()
            && !self
                .stream_lanes
                .get(file_path)
                .is_some_and(|lanes| lanes.any_streaming_fresh())
    }

    /// The hybrid second-touch escalation CANDIDACY prefix — ONE
    /// definition for the handler's ranged dispatch and the DIALED P1.5
    /// IPC direct-drive prelude: records the ghost TOUCH (always — skew
    /// evidence must accumulate regardless of the verdict), then gates on
    /// the ghost hit, mem-budget Red (escalations pause, heat recording
    /// continues — the §5.7 publish-pause mirror), and the per-key
    /// escalation cooldown. `true` = the touch is escalation-shaped and
    /// the caller consults the governor (`allow_escalation` on the
    /// authoritative handler site, `escalation_would_admit` on the
    /// prelude peek). Latch-free single-word atomics throughout —
    /// prelude-callable from a foreign service thread.
    pub(crate) fn ranged_escalation_candidate(&self, block_key: &str) -> bool {
        let ghost_hit = self.ghost.check_and_record(block_key);
        ghost_hit
            && crate::mem_budget::level() != crate::mem_budget::Level::Red
            && !self.escalation_cooldown.recently_escalated(block_key)
    }

    /// BINDING-VALIDATED ranged striped serve (R3, §5.6). Identical proof
    /// obligation to [`Self::get_block_for_index`]: bytes for key K serve
    /// block `b` only if (a) the fill was incarnation-valid — snapshot
    /// before the device read, unchanged after (the raw-dest leg's own
    /// discipline) — AND (b) the CURRENT map still binds b → K once the
    /// bytes are in hand. On movement: re-resolve and retry
    /// (`MAX_REBINDS`), falling back to the whole-block validated loop on
    /// exhaustion pressure. `Ok(None)` = the block is a hole in the
    /// current map (the caller serves zeros).
    ///
    /// NEVER PUBLISHED as a partial: a partial payload must not exist
    /// under a whole-block tier key (tier/hot entries are whole-block by
    /// contract — a short entry would serve truncated bytes to a larger
    /// read). Ranged fills serve their caller only.
    ///
    /// GHOST-EVIDENCE ADMISSION (hybrid I/O, user directive 2026-07-15 —
    /// supersedes the record-only stance): under `second-touch` the
    /// dispatch consults the ghost table. A FIRST touch stays a
    /// device-true window read (record, no admit — streaming/pollution
    /// protection unchanged); a SECOND touch within the two-epoch window
    /// ESCALATES to one whole-block fetch through the single-flight,
    /// whose own fill-site ghost check admits it (protected hot put +
    /// today's validated NVMe publish) — rand-4k re-read heat converges
    /// to RAM instead of staying device-bound forever. The escalation is
    /// an ADMISSION and rides the mem-budget authority: Red pauses it
    /// (heat recording continues, mirroring the §5.7 publish-pause
    /// semantics at the whole-block fill site). `always`/`never` keep
    /// their verbatim escape-hatch semantics (no ghost interaction, no
    /// escalation). The `direct_device_true` escape skips ALL of it —
    /// no record, no escalation, pure window reads.
    ///
    /// Device windows are rounded outward to the conservative 4096-byte
    /// LBA (approved OQ #1). `dest` is offered only on the zero-copy leg
    /// (window == request, 4 KiB-aligned): the served value is then
    /// backed by the dest region itself. Unaligned edges take the bounce
    /// leg (window DMA into a pooled aligned buffer, request slice out —
    /// bounded ≤ request + 8 KiB); window padding is never served.
    ///
    /// NOT single-flighted by design (§5.6): deduping 4 KiB fetches under
    /// a 4 MiB block key would serialize independent sub-reads for no
    /// byte savings. (The escalated whole-block fetch IS single-flighted —
    /// concurrent second-touchers of one block dedupe to one device fetch.)
    pub async fn get_block_range_for_index(
        &self,
        file_path: &str,
        b: u32,
        rel_range: std::ops::Range<u64>,
        resolved_key: Option<&str>,
        dest: Option<RangedDest>,
        hint: ReadClassHint,
    ) -> Result<Option<crate::cache::pool::ReadBlockValue>> {
        const MAX_REBINDS: usize = 8;
        const LBA: u64 = 4096;
        let device_true = self.direct_device_true() && hint.odirect;
        // Hybrid second-touch escalation (see doc comment). Checked ONCE
        // per request against the resolved key — the check itself records
        // the touch (the primitive's old success-site record moved here),
        // so first touches keep exactly one ghost interaction per read.
        if !device_true && self.tier_admission == TierAdmission::SecondTouch {
            if let Some(k) = resolved_key {
                if self.ranged_escalation_candidate(k)
                    // Scan-resistance governor (2026-07-26): an escalation
                    // is a whole-block admission FETCH — under the waste
                    // clamp it must fit the fill budget or the read stays
                    // a device-true ranged window read. Denials record NO
                    // cooldown, so hot keys retry and win the trickle.
                    && self
                        .cache
                        .admission_governor
                        .allow_escalation(self.block_size.load(Ordering::Relaxed))
                {
                    // Cooldown (churn bound, see EscalationCooldown): a
                    // key escalates at most once per ~32–64 s window —
                    // fitting working sets converge once and stop
                    // touching this dispatch; beyond-tier sets degrade to
                    // the device-true ranged path between windows instead
                    // of re-fetching + re-publishing 4 MiB per eviction
                    // (the R-5 spiral class in tier form).
                    self.escalation_cooldown.record(k);
                    METRICS
                        .ranged_read_ghost_escalations
                        .fetch_add(1, Ordering::Relaxed);
                    if hint.odirect {
                        METRICS
                            .read_odirect_ghost_admits
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    let whole = self
                        .get_block_for_index(file_path, b, resolved_key, false, true)
                        .await?;
                    return Ok(whole
                        .map(|val| Self::slice_whole_for_ranged(val, &rel_range, dest.as_ref())));
                }
            }
        }
        let block_size = self.block_size.load(Ordering::Relaxed);
        let req_len = (rel_range.end - rel_range.start) as usize;
        debug_assert!(req_len > 0, "empty ranged request");
        debug_assert!(
            rel_range.end <= block_size,
            "ranged request escapes its block"
        );
        debug_assert_eq!(
            block_size % LBA,
            0,
            "ranged dispatch requires LBA-multiple block sizes"
        );
        let aligned_start = rel_range.start & !(LBA - 1);
        let aligned_end = std::cmp::min(rel_range.end.div_ceil(LBA) * LBA, block_size);
        let window = (aligned_end - aligned_start) as usize;
        let bounced = window != req_len;
        // The zero-copy leg's contract: window == request and the dest is
        // 4 KiB-aligned registered memory — callers offer `dest` only
        // then. A dest on a bounced shape is a caller bug.
        debug_assert!(
            dest.is_none() || !bounced,
            "RangedDest offered off the zero-copy leg"
        );

        let mut key: Option<String> = resolved_key.map(str::to_string);
        for attempt in 0..MAX_REBINDS {
            let Some(cur_key) = key else {
                return Ok(None);
            };
            // FIND-RW5-A backoff tail (same rationale as the whole-block
            // loop): the ranged fast phase keeps its exhaustion fallback
            // to the escalating whole-block loop, but a short backoff
            // absorbs the free-list-reuse incarnation storms first.
            if attempt >= 4 {
                let shift = (attempt - 4).min(3) as u32;
                tokio::time::sleep(std::time::Duration::from_millis(1u64 << shift)).await;
            }
            let tracked = self.backend_router.key_incarnation_tracked(&cur_key);
            let before = self.backend_router.fill_incarnation(&cur_key);

            METRICS.ranged_reads.fetch_add(1, Ordering::Relaxed);
            METRICS
                .ranged_read_bytes
                .fetch_add(window as u64, Ordering::Relaxed);
            // Scan-resistance governor: ranged window bytes are the
            // workload's own device spend — the clamp's denominator.
            // (Escalation fetches ride the whole-block path and never
            // self-fund.)
            self.cache.admission_governor.note_foreground(window as u64);
            if bounced {
                METRICS
                    .ranged_read_unaligned_bounces
                    .fetch_add(1, Ordering::Relaxed);
            }
            let window_bytes = match &dest {
                Some(d) => {
                    // DMA straight into the registered payload dest; the
                    // value below is constructed only after validation.
                    let b = self
                        .backend_router
                        .read_block_range(&cur_key, aligned_start, window, Some(d.ptr as u64))
                        .await?;
                    // Copy ledger: device DMA straight into the dest.
                    METRICS
                        .read_dest_dma_bytes
                        .fetch_add(window as u64, Ordering::Relaxed);
                    b
                }
                None => {
                    let b = self
                        .backend_router
                        .read_block_range(&cur_key, aligned_start, window, None)
                        .await?;
                    // Copy ledger: window DMA into the pooled bounce.
                    METRICS
                        .read_fill_dma_bytes
                        .fetch_add(window as u64, Ordering::Relaxed);
                    b
                }
            };

            // Fill discipline, then serve rule — bytes in hand FIRST.
            let incarnation_ok = !tracked
                || before
                    .is_some_and(|bf| self.backend_router.fill_incarnation_still(&cur_key, bf));
            let current = self.current_block_binding(file_path, b).await?;
            if incarnation_ok && current.as_deref() == Some(cur_key.as_str()) {
                // Heat capture moved to the dispatch check above (one
                // ghost interaction per read; the device-true escape
                // records nothing by contract).
                let value = match &dest {
                    Some(d) => {
                        // Served == window == request; the dest region is
                        // fully DMA-covered — nothing to zero (the
                        // reused-payload replay rule is satisfied by full
                        // coverage).
                        crate::cache::pool::ReadBlockValue::Bytes(bytes::Bytes::from_owner(
                            crate::cache::pool::UringBufOwner {
                                ptr: d.ptr,
                                len: req_len,
                            },
                        ))
                    }
                    None => {
                        let from = (rel_range.start - aligned_start) as usize;
                        crate::cache::pool::ReadBlockValue::Bytes(
                            window_bytes.slice(from..from + req_len),
                        )
                    }
                };
                return Ok(Some(value));
            }
            METRICS
                .stale_binding_rebinds
                .fetch_add(1, Ordering::Relaxed);
            METRICS.ranged_read_rebinds.fetch_add(1, Ordering::Relaxed);
            debug!(
                "stale-binding rebind (ranged read): file={} block={} key={} current={:?} fill_valid={}",
                file_path, b, cur_key, current, incarnation_ok
            );
            key = current;
        }

        // Exhaustion pressure (§5.6): fall back to the whole-block
        // validated loop — single-flighted, decode-correct, and immune to
        // per-window churn — and slice the request out. Under the
        // device-true escape the fallback keeps the escape's contract
        // (cache-free, publish-free fetch). With a dest the slice is
        // copied in and the tail zeroed (reused-payload replay rule at
        // the copy site).
        let whole = self
            .get_block_for_index(file_path, b, key.as_deref(), device_true, true)
            .await?;
        Ok(whole.map(|val| Self::slice_whole_for_ranged(val, &rel_range, dest.as_ref())))
    }

    /// Serve a ranged request out of a whole-block value (the escalation
    /// and exhaustion-fallback tail of [`Self::get_block_range_for_index`]).
    /// With a dest the slice is copied in and the tail zeroed — the
    /// reused-payload replay rule at the copy site; short blocks (EOF
    /// tails, holes-after-truncate) zero-fill the remainder.
    fn slice_whole_for_ranged(
        val: crate::cache::pool::ReadBlockValue,
        rel_range: &std::ops::Range<u64>,
        dest: Option<&RangedDest>,
    ) -> crate::cache::pool::ReadBlockValue {
        let req_len = (rel_range.end - rel_range.start) as usize;
        let start = std::cmp::min(rel_range.start as usize, val.len());
        let end = std::cmp::min(rel_range.end as usize, val.len());
        match dest {
            Some(d) => {
                let len = end - start;
                unsafe {
                    std::ptr::copy_nonoverlapping(val[start..end].as_ptr(), d.ptr, len);
                    if len < req_len {
                        std::ptr::write_bytes(d.ptr.add(len), 0, req_len - len);
                    }
                }
                // Copy ledger: serve copy into the dest (escalation /
                // exhaustion tail of the ranged dispatch).
                crate::fuse_client::METRICS
                    .read_copy_dest_bytes
                    .fetch_add(len as u64, Ordering::Relaxed);
                crate::cache::pool::ReadBlockValue::Bytes(bytes::Bytes::from_owner(
                    crate::cache::pool::UringBufOwner {
                        ptr: d.ptr,
                        len: req_len,
                    },
                ))
            }
            None => {
                // Zero-copy slice for `Bytes`-backed whole values (the
                // fill-loop population — E-IL1's sibling); pooled reprs
                // bounce, counted.
                if end - start == req_len {
                    if let crate::cache::pool::ReadBlockValue::Bytes(b) = &val {
                        return crate::cache::pool::ReadBlockValue::Bytes(b.slice(start..end));
                    }
                }
                let mut out = vec![0u8; req_len];
                out[..end - start].copy_from_slice(&val[start..end]);
                crate::fuse_client::METRICS
                    .read_copy_bounce_bytes
                    .fetch_add((end - start) as u64, Ordering::Relaxed);
                crate::cache::pool::ReadBlockValue::Bytes(bytes::Bytes::from(out))
            }
        }
    }

    pub async fn fetch_metadata(&self, file_path: &str) -> Result<CachedMetadata> {
        // A DIRTY layout is the LOCAL AUTHORITY, never re-validated from the
        // backend: a staged/inline write's layout+size live only in RAM (+
        // the staging ring) until the fsync/flush/promotion cadence persists
        // them (`layout_dirty`), so the TTL refill below would replace the
        // only pointer to acked payload with a stale backend snapshot — or
        // with the default empty entry when the backend never saw a layout.
        // On an aged daemon (merge queue saturated, persists deferred for
        // seconds) any >1 s op gap then made acked staged bytes read back as
        // ZEROS and turned the follow-up fsync into a hard failure (the
        // aged-fsx loss class, tests/staged_dirty_layout_refill_tests.rs).
        // Dirty entries are cleaned only under INODE_META_LOCKS by the paths
        // that persist them (persist_dirty_layout_if_needed, promotion,
        // spill), which insert the fresh CLEAN entry themselves; a held DLM
        // lease means no remote writer can legitimately outrun a dirty
        // entry, and lease-loss staleness is governed by fencing at persist
        // time — not by serving a snapshot that predates acked writes.
        let fresh_or_dirty = metadata_entry_fresh_or_dirty;
        let ino = parse_inode_from_path(file_path);
        if let Some(entry) = self.metadata_cache.get(&ino) {
            if fresh_or_dirty(&entry) {
                // moka's get already returned an owned clone — hand it out
                // directly (the old `entry.clone()` re-cloned every field
                // per op on the hot read path).
                return Ok(entry);
            }
        }

        // Refill under the per-inode metadata lock so a stale backend snapshot
        // can never clobber a concurrent writer's fresh cache entry (see
        // INODE_META_LOCKS). Double-check after acquiring: a writer or racing
        // filler may have refreshed (or dirtied) the entry while we waited.
        let _meta_guard = meta_lock_acquire(ino).await;
        if let Some(entry) = self.metadata_cache.get(&ino) {
            if fresh_or_dirty(&entry) {
                return Ok(entry);
            }
        }
        if let Some(m) = self.fetch_metadata_from_backend(ino).await? {
            self.metadata_cache.insert(ino, m.clone());
            return Ok(m);
        }

        // If not found, return a default inline metadata (e.g. newly created file)
        let m = CachedMetadata {
            file_type: "inline".into(),
            size: 0,
            block_map_id: None,
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
            block_map: None,
            layout_dirty: false,
            layout_delta_chain: LAYOUT_DELTA_CHAIN_INELIGIBLE,
            // Synthesized (backend has no layout): never a coherent
            // publish base.
            layout_base_token: 0,
        };
        self.metadata_cache.insert(ino, m.clone());
        Ok(m)
    }

    /// Grow a non-striped layout's logical size to at least `target_size`
    /// (never shrinks; no-op when already large enough) — the
    /// fallocate/ZERO_RANGE extend commit. Runs under `INODE_META_LOCKS`
    /// against the FRESHEST entry (RAM cache, then backend), the same
    /// discipline as the promote/spill/truncate commits: an unlocked save of
    /// a pre-promotion snapshot erased a just-published `block_map[0]` right
    /// after the ring entry was released, stranding the payload (the aged
    /// zeros-LOSS / EIO-wedge leg).
    pub async fn grow_layout_size(
        &self,
        ino: u64,
        target_size: u64,
        fencing_token: u64,
    ) -> Result<()> {
        let _meta_guard = meta_lock_acquire(ino).await;
        // NOTE: `fetch_metadata` would retake this lock.
        let current = match self.metadata_cache.get(&ino) {
            Some(m) => Some(m),
            None => self.fetch_metadata_from_backend(ino).await?,
        };
        let Some(current) = current else {
            return Ok(());
        };
        if current.size >= target_size || current.file_type == "striped" {
            return Ok(());
        }
        let mut updated = current;
        updated.size = target_size;
        // The save persists the whole layout — including a dirty RAM-only
        // one — so the entry is clean afterwards.
        updated.layout_dirty = false;
        updated.cached_at = std::time::Instant::now();
        self.save_metadata_to_backend(ino, &updated, fencing_token)
            .await?;
        self.metadata_cache.insert(ino, updated);
        Ok(())
    }

    /// Persist layout/size if the hot cache marked it dirty (writeback path).
    ///
    /// Runs under the per-inode metadata lock: a concurrent staged-promotion
    /// commit mutates the same RAM entry + backend layout, and an unlocked
    /// persist could clobber the promoted mapping with a pre-promotion
    /// snapshot (stranding the staged data once its ring entry is released).
    pub async fn persist_dirty_layout_if_needed(
        &self,
        file_path: &str,
        fencing_token: u64,
    ) -> Result<()> {
        let ino = parse_inode_from_path(file_path);
        let _meta_guard = meta_lock_acquire(ino).await;
        let Some(meta) = self.metadata_cache.get(&ino) else {
            return Ok(());
        };
        if !meta.layout_dirty {
            return Ok(());
        }
        let mut clean = meta.clone();
        clean.layout_dirty = false;
        clean.cached_at = std::time::Instant::now();
        self.save_metadata_to_backend(ino, &clean, fencing_token)
            .await?;
        self.metadata_cache.insert(ino, clean);
        Ok(())
    }

    /// Promote a resident staged file to a durable backend block and release
    /// its staging-ring entry + budget (merge-worker path, capacity pressure).
    ///
    /// Returns `Ok(true)` when the entry was promoted and released. Any
    /// identity/generation mismatch is a benign skip (`Ok(false)`): the entry
    /// either no longer exists or a racing re-stage/layout-transition now
    /// owns it. Ordering:
    ///
    /// 1. Block data I/O first (io_uring, no locks held).
    /// 2. Layout commit (backend + RAM cache together) under the per-inode
    ///    metadata lock, with staged identity + stage-generation re-checked
    ///    under that lock.
    /// 3. Ring entry + budget release only if the stage generation is still
    ///    the one we promoted (`remove_staged_if_generation`).
    pub(crate) async fn promote_staged_file(
        &self,
        file_path: &str,
        file_id: &str,
        fencing_token: u64,
    ) -> Result<bool> {
        let nvme = &self.cache.nvme;
        let Some(gen) = nvme.staged_generation(file_id).await else {
            return Ok(false);
        };
        let Some(raw) = nvme.read_staged(file_id) else {
            // Counted but not resident (should not happen): reconcile so the
            // budget cannot leak. Blocking-pool hop: shard WRITE lock
            // (shard-lock invariant rule 2).
            let _ = nvme
                .remove_staged_if_generation_async(file_id.to_string(), gen)
                .await;
            return Ok(false);
        };

        // W2 rider fence: a file with a LIVE extent record never promotes —
        // the record's extents are NEWER than the ring image, and composing
        // them here would put the promotion's commit in a multi-writer race
        // with every rider mutation site. The record-bearing state is
        // transient by construction (whole-image writes fold-first; fsync/
        // teardown/threshold folds drain records), so deferral converges;
        // under hard ring pressure the write path's spill leg still drains
        // (records fold into the spilled image — never wedged custody).
        let promote_ino = parse_inode_from_path(file_path);
        let rider_key = crate::keys::active_block_ext(promote_ino, 0).to_string();
        if self.cache.nvme.has_staged_extent_record(&rider_key) {
            return Ok(false);
        }

        let raw_len = raw.len();
        let processed = self
            .get_crypto()
            .process_write_async(bytes::Bytes::from(raw))
            .await?;
        let (be_id, allocator, writer) = self.backend_router.get_active_backend()?;
        if processed.len() as u64 > allocator.chunk_size() {
            // FIND-RW4-A: unreachable post-fix (the store-raw escape bounds
            // every stored image by `max_stored_image_len`, and the mount
            // geometry gate guarantees that fits the chunk). Staying
            // resident is the never-wrong degrade for this opportunistic
            // path — but it must be LOUD, because a permanently
            // unpromotable file means the geometry invariant broke.
            log::error!(
                "staged promotion of {file_path}: stored image ({} B) exceeds the {} B \
                 allocator chunk — FIND-RW4-A geometry invariant violated; file stays \
                 ring-resident (readable, never promoted)",
                processed.len(),
                allocator.chunk_size()
            );
            return Ok(false);
        }
        let offset = allocator.allocate_block().await?;
        // PR VL6a: live owner registration for the allocate→commit window
        // (drops at function end, after the layout commit below).
        let _inflight = allocator.inflight_register(offset);
        // Size-carrying mapping (`bk:0:packed_len`): without the exact
        // stored-image length, a passthrough transform cannot strip the
        // whole-block read's recycled-tenant tail (see
        // `parse_block_mapping`).
        let block_key = format!(
            "{}:0:{}",
            self.backend_router.persist_block_key(&be_id, offset),
            processed.len()
        );
        if let Err(e) = writer.write_block(offset, processed).await {
            let _ = allocator.free_block(offset).await;
            return Err(e);
        }
        allocator.publish_block(offset);

        let ino = parse_inode_from_path(file_path);
        let commit = async {
            let _meta_guard = meta_lock_acquire(ino).await;
            // Authoritative meta: RAM cache first (post-write truth), then
            // backend. NOTE: `fetch_metadata` would retake this lock.
            let current = match self.metadata_cache.get(&ino) {
                Some(m) => Some(m),
                None => self.fetch_metadata_from_backend(ino).await?,
            };
            let Some(current) = current else {
                return Ok::<bool, SqueezefsError>(false);
            };
            if current.file_type != "staged"
                || current.file_id.as_deref() != Some(file_id)
                || nvme.staged_generation(file_id).await != Some(gen)
            {
                return Ok(false);
            }
            let mut updated = current.clone();
            let mut block_map = updated.block_map.take().unwrap_or_default();
            // CoW publish: a held reader snapshot keeps its map (pinned by
            // test_block_map_snapshot_independent_of_*).
            let displaced = std::sync::Arc::make_mut(&mut block_map).insert(0, block_key.clone());
            updated.block_map = Some(block_map);
            // The promoted image IS acked file content: a `current` snapshot
            // whose size persist lags the blob (the RMW that staged this
            // image reaches its commit lock AFTER this promotion) must not
            // persist a size SMALLER than the image — a later TTL refill
            // would resurrect the stale pair {old size, new image} and roll
            // an acked extend back to an implicit-zero tail (the leg-5
            // fail3 tape: promote saved size=306494 with a 425111-byte
            // image).
            updated.size = updated.size.max(raw_len as u64);
            updated.layout_dirty = false;
            updated.cached_at = std::time::Instant::now();
            self.save_metadata_to_backend(ino, &updated, fencing_token)
                .await?;
            self.metadata_cache.insert(ino, updated);
            if let Some(prev) = displaced {
                if prev != block_key {
                    // Re-promotion over an older durable copy: purge + free it.
                    self.cache.purge_block_key(&prev);
                    let _ = self.backend_router.free_block(&prev).await;
                }
            }
            // Ring-entry release INSIDE the commit's lock section — the
            // DESTRUCTIVE step of the promotion lifecycle is serialized with
            // every layout commit (leg 5 of the zeros-LOSS family): released
            // after the lock, it landed inside an RMW's stage→commit-lock
            // window, so the RMW's commit saw its just-staged entry gone yet
            // published "ring is authoritative, map=None" and freed this
            // promotion's mapping — the file's sole surviving copy (the
            // offset was reallocated and punched under two other inodes;
            // the next durable clip read 100 % zeros). Under the lock, the
            // staged-arm commit's residency check is exact: removal cannot
            // interleave with it. Still generation-gated: a re-stage that
            // bumped the generation after our blob read keeps its (newer)
            // ring entry — this promotion's mapping is then the one the
            // RMW's commit releases as superseded. Blocking-pool hop: shard
            // WRITE lock (invariant rule 2); bounded (index remove + page
            // reclaim), the same class of work truncate already holds this
            // lock across.
            let _ = nvme
                .remove_staged_if_generation_async(file_id.to_string(), gen)
                .await;
            Ok(true)
        }
        .await;

        // TEMP-PROBE (leg5-v2 rail a; stripped before commit)
        match commit {
            Ok(true) => Ok(true),
            Ok(false) => {
                let _ = allocator.free_block(offset).await;
                Ok(false)
            }
            Err(e) => {
                let _ = allocator.free_block(offset).await;
                Err(e)
            }
        }
    }

    /// Resolve the durable mapping (`block_map[0]`) of a staged file whose
    /// ring entry is gone. The caller's `meta` snapshot can predate a
    /// concurrent promotion/spill commit, so fall back to the freshest cached
    /// entry and finally the authoritative backend before declaring the
    /// payload unreachable.
    /// Loud marker for a staged file whose payload is GONE — no ring entry
    /// (crash-torn → discarded by segment recovery, or lost before it ever
    /// hit the segment) and no promoted mapping. Per the D0 staging degrade
    /// contract the read path serves size-consistent zeros; this records
    /// the loss once per read in the log and the stats surface
    /// (`staged_payload_lost_reads`).
    fn note_lost_staged_payload(&self, file_path: &str, file_id: &str) {
        crate::fuse_client::METRICS
            .staged_payload_lost_reads
            .fetch_add(1, Ordering::Relaxed);
        log::warn!(
            "staged payload for {file_path} (file id {file_id}) is gone from local staging \
             and was never promoted — a crash discarded acked-unfsynced data; serving \
             size-consistent zeros (D0 degrade contract)"
        );
    }

    async fn staged_block_mapping(&self, file_path: &str, meta: &CachedMetadata) -> Option<String> {
        let map0 = |m: &CachedMetadata| m.block_map.as_ref().and_then(|bm| bm.get(&0).cloned());
        if let Some(mapping) = map0(meta) {
            return Some(mapping);
        }
        let ino = parse_inode_from_path(file_path);
        if let Some(mapping) = self.metadata_cache.get(&ino).as_ref().and_then(map0) {
            return Some(mapping);
        }
        self.fetch_metadata_from_backend(ino)
            .await
            .ok()
            .flatten()
            .as_ref()
            .and_then(map0)
    }

    /// The freshest observable layout identity of `file_path`: the RAM
    /// metadata cache (every transition publishes here synchronously,
    /// including RAM-only dirty layouts the backend has not seen), falling
    /// back to the authoritative backend. Deliberately does NOT insert the
    /// backend snapshot into the cache — an unlocked insert here could
    /// clobber a concurrent writer's fresher RAM entry (`fetch_metadata`
    /// owns the locked refill).
    async fn freshest_layout_identity(&self, file_path: &str) -> Option<CachedMetadata> {
        let ino = parse_inode_from_path(file_path);
        if let Some(m) = self.metadata_cache.get(&ino) {
            return Some(m);
        }
        self.fetch_metadata_from_backend(ino).await.ok().flatten()
    }

    /// Fetch + decode the promoted/spilled durable copy of a staged file
    /// (`block_map[0]`). The caller owns binding revalidation: these bytes
    /// may belong to a freed-and-reused block if the identity moved while
    /// the read was in flight.
    async fn read_promoted_staged_block(&self, block_key: &str) -> Result<bytes::Bytes> {
        // Size-carrying (`bk:0:packed_len`) mappings decode to EXACTLY the
        // promoted payload: read the 4 KiB-aligned window covering the image
        // (device reads are LBA-aligned) and slice the exact image before
        // the transform. A bare legacy key has lost the packed length: the
        // whole-block read's tail is device garbage that a passthrough
        // (no-compression) `process_read` cannot strip — callers must bound
        // what they consume (the read path clamps to `meta.size`).
        let (offset_u64, off, sz, exact) = self.parse_block_mapping(block_key)?;
        let read_len = if exact { sz.div_ceil(4096) * 4096 } else { sz };
        let packed_bytes = self
            .nvme_writer
            .read_block(offset_u64 + off, read_len)
            .await?;
        let packed_bytes = if exact && packed_bytes.len() > sz {
            packed_bytes.slice(0..sz)
        } else {
            packed_bytes
        };
        self.get_crypto().process_read_async(packed_bytes).await
    }

    /// Release the artifacts of a superseded staged layout *after* the new
    /// layout is published (backend as applicable + RAM cache): the staging
    /// ring entry (returns its budget) and any promoted/spilled durable
    /// copies the new layout no longer references. Callers hold the
    /// per-inode metadata lock; `old_map` is the LAST PUBLISHED layout's
    /// map, captured UNDER that lock immediately before the caller's commit
    /// (`metadata_cache.get` at the top of the locked section).
    ///
    /// NEVER pass the op-entry `meta` snapshot: any mapping present there
    /// but absent from the under-lock state was already displaced by an
    /// intermediate commit (promotion re-publish, spill, truncate clip) —
    /// and every displacing commit frees what it displaced, so freeing the
    /// stale key again is a DOUBLE FREE. `free_block` punches the device
    /// extent, so when the offset had already been reallocated the second
    /// free zeroed a LIVE block under its new owner — scattered durable
    /// zero runs for acked data (the aged zeros-LOSS residual: one device
    /// offset cycling as `block_map[0]` across consecutive promotions on
    /// the failure tape). A promotion that lands between the caller's
    /// snapshot and its lock cannot leak: its published mapping IS the
    /// under-lock state this frees.
    async fn release_superseded_staged(
        &self,
        old_ring_id: Option<&str>,
        old_map: Option<&std::collections::HashMap<u32, String>>,
        keep_block_key: Option<&str>,
    ) {
        if let Some(fid) = old_ring_id {
            // Blocking-pool hop: shard WRITE lock (invariant rule 2).
            let _ = self.cache.nvme.remove_staged_async(fid.to_string()).await;
        }
        let mut freed = std::collections::HashSet::new();
        for map in old_map.into_iter() {
            for bk in map.values() {
                if keep_block_key == Some(bk.as_str()) || !freed.insert(bk.clone()) {
                    continue;
                }
                self.cache.purge_block_key(bk);
                let _ = self.backend_router.free_block(bk).await;
            }
        }
    }

    /// The ONLY way to mutate a striped block map (zero-copy write-path
    /// design §5.3 "One merge discipline"). Serializes under
    /// `INODE_META_LOCKS.get_inode_lock(ino)`; fetches the CURRENT meta
    /// (authoritative backend, falling back to the freshest RAM entry and
    /// finally a default for never-persisted layouts); applies `op`; bumps
    /// size to at least `min_size` (or truncates to `new_size` exactly for
    /// [`BlockMapOp::TruncateFrom`]); applies `layout_flip`; saves with
    /// fencing revalidation (which also republishes the RAM cache entry
    /// coherently); returns the keys actually displaced/removed from the
    /// current map — the caller frees them AFTER this returns (never a
    /// start-of-call snapshot key) — having already purged them from every
    /// RAM/NVMe read tier.
    ///
    /// Lock order: callers may hold `active_inode_locks` (1) and/or
    /// `BLOCK_FLUSH_LOCKS` (3); this primitive MUST NOT acquire either —
    /// `INODE_META_LOCKS` sits strictly after them (P1-9 extended order,
    /// see `stripe_locks.rs`). NOTE: never call `fetch_metadata` from under
    /// this lock — it retakes it on refill and self-deadlocks.
    pub async fn merge_block_mappings(
        &self,
        ino: u64,
        op: BlockMapOp<'_>,
        min_size: u64,
        layout_flip: LayoutFlip,
        fencing_token: u64,
    ) -> Result<Vec<String>> {
        self.merge_block_mappings_if_epoch(ino, op, min_size, layout_flip, fencing_token, None)
            .await
            .map(|d| d.expect("unconditional merge cannot be epoch-refused"))
    }

    /// [`Self::merge_block_mappings`] with delayed-merge revalidation: when
    /// `expected_epoch` is `Some(e)` and the inode's layout-prune epoch no
    /// longer equals `e` (a truncate/punch pruned the map after the caller
    /// captured its content), the merge is REFUSED under the same
    /// `INODE_META_LOCKS` critical section — nothing is applied or saved and
    /// `Ok(None)` is returned. The caller must discard/free its
    /// now-unreachable upload and re-capture against the post-prune state.
    pub async fn merge_block_mappings_if_epoch(
        &self,
        ino: u64,
        op: BlockMapOp<'_>,
        min_size: u64,
        layout_flip: LayoutFlip,
        fencing_token: u64,
        expected_epoch: Option<u64>,
    ) -> Result<Option<Vec<String>>> {
        let _map_guard = meta_lock_acquire(ino).await;

        let epoch_word = LAYOUT_PRUNE_EPOCHS.get_inode_lock(ino);
        if let Some(expected) = expected_epoch {
            if epoch_word.load(Ordering::Acquire) != expected {
                return Ok(None);
            }
        }
        // Pruning ops invalidate every in-flight delayed merge: bump under
        // the meta lock, BEFORE the save, so a delayed merge serialized after
        // this critical section observes the new epoch and refuses.
        if matches!(
            op,
            BlockMapOp::TruncateFrom { .. } | BlockMapOp::RemoveBlocks(_)
        ) {
            epoch_word.fetch_add(1, Ordering::Release);
        }

        // RMW base — the dirty-authority rule (FIND-RW5-A face 5): a DIRTY
        // RAM entry is the LOCAL AUTHORITY (staged-family commits defer
        // their backend persist), so basing this RMW on the backend
        // resurrected the PRE-DIRTY map: keys an earlier commit had already
        // displaced-and-freed reappeared as "displaced" here and were freed
        // AGAIN (the double-free double-owner mint — run17 forensics), and
        // the dirty lineage's own keys were silently clobbered from the
        // saved layout (the zeros-LOSS face). Clean/absent entries keep the
        // backend base (freshest authoritative).
        let mut current = match self.metadata_cache.get(&ino) {
            Some(m) if m.layout_dirty => m,
            cached => match self.fetch_metadata_from_backend(ino).await? {
                Some(m) => m,
                // Never-persisted layout: the freshest RAM entry (post-write
                // truth) beats an empty default.
                None => cached.unwrap_or_default(),
            },
        };

        let forensics_base = if current.layout_dirty {
            "dirty-ram"
        } else {
            "backend/cached"
        };
        let forensics_op: Option<String> = if std::env::var("SQUEEZEFS_FREE_FORENSICS").is_ok() {
            Some(match &op {
                BlockMapOp::Merge(e) => format!("Merge({e:?})"),
                BlockMapOp::MergeExpected(e) => format!("MergeExpected({e:?})"),
                BlockMapOp::TruncateFrom { new_size } => format!("TruncateFrom({new_size})"),
                BlockMapOp::RemoveBlocks(idxs) => format!("RemoveBlocks({idxs:?})"),
            })
        } else {
            None
        };

        // CoW publish (item A): take the Arc, mutate a uniquely-owned copy
        // via `make_mut` — held reader snapshots keep the exact map they
        // were taken with (test_block_map_snapshot_independent_of_*).
        let mut block_map_arc = current.block_map.take().unwrap_or_default();
        let block_map = std::sync::Arc::make_mut(&mut block_map_arc);
        let mut displaced: Vec<String> = Vec::new();
        let purge = |bk: &str| {
            // Purge every cache tier for a displaced/removed key: its offset
            // will be reallocated under the SAME key string once freed, and
            // a stale tier hit would serve the dead incarnation's bytes.
            self.cache.purge_block_key(bk);
        };
        match op {
            BlockMapOp::Merge(entries) => {
                for (b, new_key) in entries {
                    if let Some(prev) = block_map.insert(*b, new_key.clone()) {
                        if prev != *new_key {
                            purge(&prev);
                            displaced.push(prev);
                        }
                    }
                }
                // Size floor: never below the caller's bound nor the freshest
                // RAM size (writes publish size to the RAM cache ahead of the
                // deferred layout commit — a merge must not regress it).
                current.size = std::cmp::max(current.size, min_size);
                if let Some(cached) = self.metadata_cache.get(&ino) {
                    if cached.size > current.size {
                        current.size = cached.size;
                    }
                }
            }
            BlockMapOp::MergeExpected(entries) => {
                for (b, expected, new_key) in entries {
                    match block_map.get(b) {
                        // Merge only where the mover's captured mapping is
                        // still current — the supersession law: a mismatch
                        // (foreground replace) or absence (truncate/punch
                        // prune) skips silently; re-plan revisits.
                        Some(cur) if cur == expected && cur != new_key => {
                            let prev = block_map
                                .insert(*b, new_key.clone())
                                .expect("get() just observed the entry");
                            purge(&prev);
                            displaced.push(prev);
                        }
                        _ => {}
                    }
                }
                // Same size discipline as Merge.
                current.size = std::cmp::max(current.size, min_size);
                if let Some(cached) = self.metadata_cache.get(&ino) {
                    if cached.size > current.size {
                        current.size = cached.size;
                    }
                }
            }
            BlockMapOp::TruncateFrom { new_size } => {
                let block_size = self.block_size.load(Ordering::Relaxed);
                block_map.retain(|&b, bk| {
                    if (b as u64) * block_size >= new_size {
                        purge(bk);
                        displaced.push(bk.clone());
                        false
                    } else {
                        true
                    }
                });
                current.size = new_size;
            }
            BlockMapOp::RemoveBlocks(idxs) => {
                for &b in idxs {
                    if let Some(bk) = block_map.remove(&b) {
                        purge(&bk);
                        displaced.push(bk);
                    }
                }
                // A punch never grows or shrinks the file: hold the size at the
                // caller's floor / freshest RAM size (same discipline as Merge).
                current.size = std::cmp::max(current.size, min_size);
                if let Some(cached) = self.metadata_cache.get(&ino) {
                    if cached.size > current.size {
                        current.size = cached.size;
                    }
                }
            }
        }
        // FIND-RW5-A merge forensics (env-gated, diagnostic-only): one line
        // per merge naming the base authority, the op, what displaced, and
        // the post-merge bindings — enough to reconstruct whether a later
        // read's stale binding came from a lost map update or a wrongful
        // free of a still-current key.
        if let Some(op_name) = forensics_op {
            let mut map_now: Vec<(u32, String)> =
                block_map.iter().map(|(b, k)| (*b, k.clone())).collect();
            map_now.sort_unstable_by_key(|(b, _)| *b);
            log::warn!(
                "MERGE FORENSICS ino={ino} base={forensics_base} op={op_name} \
                 token={fencing_token} displaced={displaced:?} map_now={map_now:?}"
            );
        }
        current.block_map = Some(block_map_arc);

        match layout_flip {
            LayoutFlip::ToStripedKeepStagedIdentity => {
                current.file_type = "striped".into();
            }
            LayoutFlip::ToStripedClearStagedIdentity => {
                current.file_type = "striped".into();
                current.file_id = None;
                current.data_key = None;
            }
            LayoutFlip::KeepLayout => {}
        }

        // Fencing revalidation happens inside; the save also republishes the
        // RAM metadata_cache entry, keeping RAM + backend coherent under the
        // same guard.
        self.save_metadata_to_backend(ino, &current, fencing_token)
            .await?;
        Ok(Some(displaced))
    }

    /// Write-commit-economy lever 1 (2026-07-30): the **coalescing**
    /// face of [`Self::merge_block_mappings`] for the block-publish hot
    /// path (unconditional `Merge` shapes only). Ops enqueue on the
    /// ino's publish conveyor; a leader-elected detached pass drains
    /// whatever accumulated (no timers — the batch is what queued while
    /// the previous commit was in flight, the jbd2/M7 shape), applies
    /// the WHOLE batch under one `INODE_META_LOCKS` section with the
    /// exact per-op semantics of the direct primitive (dirty-authority
    /// RMW base, per-op fencing, displaced-key purge, size floors,
    /// flips), and persists it as ONE commit — an O(batch) layout delta
    /// where the eligibility ladder allows.
    ///
    /// Durability contract: the returned future resolves only after the
    /// op's batch COMMITTED — there is no parked window an fsync could
    /// race (fsync's own flush merges ride the same conveyor and their
    /// completion precedes its barrier, exactly as with the direct
    /// path). Blocks awaiting their batch are precisely as crash-exposed
    /// as blocks awaiting the direct path's serialized merge queue.
    pub async fn merge_block_mappings_coalesced(
        &self,
        ino: u64,
        entries: Vec<(u32, String)>,
        min_size: u64,
        flip: LayoutFlip,
        fencing_token: u64,
    ) -> Result<Vec<String>> {
        if publish_coalesce_max() <= 1 {
            // A/B lever (`SQUEEZEFS_PUBLISH_COALESCE_MAX=1`): the
            // pre-campaign serialized per-op path, verbatim.
            return self
                .merge_block_mappings(
                    ino,
                    BlockMapOp::Merge(&entries),
                    min_size,
                    flip,
                    fencing_token,
                )
                .await;
        }
        let conveyor = match self.publish_conveyors.read_sync(&ino, |_, c| c.clone()) {
            Some(c) => c,
            None => match self.publish_conveyors.entry_sync(ino) {
                scc::hash_map::Entry::Occupied(occ) => occ.get().clone(),
                scc::hash_map::Entry::Vacant(vac) => {
                    let fresh = std::sync::Arc::new(
                        crate::meta_backend::kv::conveyor_core::ConveyorCore::new(),
                    );
                    vac.insert_entry(fresh.clone());
                    fresh
                }
            },
        };
        let (done, rx) = tokio::sync::oneshot::channel();
        let blocks = entries.len() as u64;
        // Enqueue-then-elect with no await between (the conveyor_core
        // no-lost-wakeup protocol): a cancelled submitter can never
        // strand its entry without a responsible leader.
        conveyor.enqueue(
            QueuedPublish {
                entries,
                min_size,
                flip,
                fencing_token,
                done,
                enqueued_at: std::time::Instant::now(),
            },
            blocks,
        );
        if conveyor.try_lead() {
            // Detached (the M7 cancellation-safety law): no
            // client-visible cancellation can drop a batch mid-commit.
            let router = self.clone();
            let conveyor = conveyor.clone();
            tokio::spawn(async move {
                router.publish_pass_task(ino, conveyor).await;
            });
        }
        match rx.await {
            Ok(out) => out,
            Err(_) => Err(SqueezefsError::Io(std::io::Error::other(
                "publish conveyor pass dropped its result channel (pass panic — \
                 publish failed loud; custody stays with the caller's never-lossy ladder)",
            ))),
        }
    }

    /// The detached publish pass (lever 1): drain → batch-apply → one
    /// save → fan out, until the queue idles; then release leadership
    /// (release-then-recheck) and retire the conveyor's map entry when
    /// it is provably idle. The drop guard contains a panicking pass:
    /// queued ops fail LOUD (their callers run the never-lossy ladder)
    /// and leadership is released so later publishes elect fresh passes
    /// — never a wedged conveyor.
    async fn publish_pass_task(
        &self,
        ino: u64,
        conveyor: std::sync::Arc<
            crate::meta_backend::kv::conveyor_core::ConveyorCore<QueuedPublish>,
        >,
    ) {
        struct PassGuard {
            conveyor:
                std::sync::Arc<crate::meta_backend::kv::conveyor_core::ConveyorCore<QueuedPublish>>,
            clean: bool,
        }
        impl Drop for PassGuard {
            fn drop(&mut self) {
                if self.clean {
                    return;
                }
                // Panic containment (the M7 pass-guard shape): fail out
                // everything queued behind the dead pass, then release
                // leadership so later publishes are never stranded.
                loop {
                    for q in self.conveyor.drain(usize::MAX, u64::MAX) {
                        let _ = q.done.send(Err(SqueezefsError::Io(std::io::Error::other(
                            "publish conveyor pass panicked — publish failed loud",
                        ))));
                    }
                    if !self.conveyor.unlead_and_recheck() {
                        break;
                    }
                }
            }
        }
        let mut guard = PassGuard {
            conveyor: conveyor.clone(),
            clean: false,
        };
        let cap = publish_coalesce_max();
        loop {
            let batch = conveyor.drain(cap, u64::MAX);
            if batch.is_empty() {
                if !conveyor.unlead_and_recheck() {
                    break;
                }
                continue;
            }
            self.publish_pass(ino, batch).await;
        }
        guard.clean = true;
        drop(guard);
        // Idle retirement: drop the map entry when this conveyor is
        // still installed and provably empty. A racing submitter that
        // already cloned the Arc keeps working on it (it elects its own
        // leader on that handle); a later submitter mints a fresh entry
        // — two conveyors for one ino only ever serialize on
        // `INODE_META_LOCKS`, never diverge.
        self.publish_conveyors.remove_if_sync(&ino, |c| {
            std::sync::Arc::ptr_eq(c, &conveyor) && c.pending() == 0
        });
    }

    /// One publish batch: the exact per-op semantics of
    /// [`Self::merge_block_mappings`]'s `Merge` arm, applied N times
    /// under ONE `INODE_META_LOCKS` section, persisted with ONE save.
    async fn publish_pass(&self, ino: u64, batch: Vec<QueuedPublish>) {
        // Publish decomposition (2026-08-01): queue_wait per drained op,
        // recorded BEFORE the test-delay seam (the seam models commit
        // latency, not queue residence).
        for op in &batch {
            publish_phase_record(PublishPhase::QueueWait, op.enqueued_at);
        }
        // Test seam (the `TEST_TIER_PUBLISH_DELAY_MS` pattern — one
        // relaxed load, zero-cost when unset, no `#[cfg(test)]` fork):
        // an artificial pass delay reproduces the field's ms-scale
        // commit latency on µs-commit sandboxes, so the batching
        // contract (`tests/publish_coalesce_tests.rs`) is deterministic.
        let delay = TEST_PUBLISH_PASS_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let t_lock = std::time::Instant::now();
        let _map_guard = meta_lock_acquire(ino).await;
        publish_phase_record(PublishPhase::LockWait, t_lock);

        // Fan an unclonable error out to every waiter, preserving the
        // fencing classification (`pipeline_disposition` keys on it).
        fn dup_err(e: &SqueezefsError) -> SqueezefsError {
            match e {
                SqueezefsError::FencingTokenExpired { token, expected } => {
                    SqueezefsError::FencingTokenExpired {
                        token: *token,
                        expected: *expected,
                    }
                }
                other => SqueezefsError::Io(std::io::Error::other(format!(
                    "coalesced publish failed: {other}"
                ))),
            }
        }

        // RMW base — the dirty-authority rule, verbatim from the direct
        // primitive (FIND-RW5-A face 5), plus the Lever A coherent-RAM
        // arm (2026-08-01): a CLEAN cached entry whose `layout_base_token`
        // matches the ino's CURRENT fencing era serves as the base —
        // every layout mutation republishes the cache under
        // `INODE_META_LOCKS`, so a same-era clean entry is coherent by
        // construction (the pre-campaign clean⇒refetch paid a backend
        // getxattr per pass that folded the ino's whole unrebased delta
        // chain and cloned the map — 6.3 M fold applies per 60 s field
        // rewrite row — and its chain-accounting reset kept the on-disk
        // chain from EVER re-basing). A foreign-era or unknown entry
        // refetches exactly as before: the lease-loss stale-map hazard
        // stays closed (`tests/publish_drain_economy_tests.rs`).
        let current_fencing = self.inner.dlm.get_fencing_token_ino(ino);
        let t_base = std::time::Instant::now();
        let mut current = match self.metadata_cache.get(&ino) {
            Some(m) if m.layout_dirty => {
                METRICS
                    .publish_base_dirty_serves
                    .fetch_add(1, Ordering::Relaxed);
                m
            }
            Some(m) if m.layout_base_token != 0 && m.layout_base_token == current_fencing => {
                METRICS
                    .publish_base_ram_serves
                    .fetch_add(1, Ordering::Relaxed);
                m
            }
            cached => {
                METRICS.publish_base_fetches.fetch_add(1, Ordering::Relaxed);
                match self.fetch_metadata_from_backend(ino).await {
                    Ok(Some(m)) => m,
                    Ok(None) => cached.unwrap_or_default(),
                    Err(e) => {
                        publish_phase_record(PublishPhase::BaseFetch, t_base);
                        for op in batch {
                            publish_phase_record(PublishPhase::Total, op.enqueued_at);
                            let _ = op.done.send(Err(dup_err(&e)));
                        }
                        return;
                    }
                }
            }
        };
        publish_phase_record(PublishPhase::BaseFetch, t_base);

        // CoW publish (item A): held reader snapshots keep their map.
        let t_apply = std::time::Instant::now();
        let mut block_map_arc = current.block_map.take().unwrap_or_default();
        let block_map = std::sync::Arc::make_mut(&mut block_map_arc);
        /// One op's terminal-fan-out bookkeeping (sender + per-op payload
        /// + the enqueue instant for the `total` publish phase).
        struct Outcome<T> {
            done: tokio::sync::oneshot::Sender<Result<Vec<String>>>,
            payload: T,
            enqueued_at: std::time::Instant,
        }
        let mut applied: Vec<Outcome<Vec<String>>> = Vec::new();
        let mut fenced: Vec<Outcome<u64>> = Vec::new();
        let mut batch_entries: Vec<(u32, String)> = Vec::new();
        let mut save_token = 0u64;
        for op in batch {
            // Per-op fencing: a stale op fails ALONE (the supersession
            // law); the batch's fresh members proceed.
            if op.fencing_token < current_fencing {
                fenced.push(Outcome {
                    done: op.done,
                    payload: op.fencing_token,
                    enqueued_at: op.enqueued_at,
                });
                continue;
            }
            let mut displaced: Vec<String> = Vec::new();
            for (b, new_key) in &op.entries {
                if let Some(prev) = block_map.insert(*b, new_key.clone()) {
                    if prev != *new_key {
                        // Purge every tier for a displaced key — its
                        // offset will be reallocated under the SAME key
                        // string once freed (the primitive's rule).
                        self.cache.purge_block_key(&prev);
                        displaced.push(prev);
                    }
                }
            }
            // Size floor: never below the op's bound (the freshest RAM
            // floor is applied once, below).
            current.size = std::cmp::max(current.size, op.min_size);
            match op.flip {
                LayoutFlip::ToStripedKeepStagedIdentity => {
                    current.file_type = "striped".into();
                }
                LayoutFlip::ToStripedClearStagedIdentity => {
                    current.file_type = "striped".into();
                    current.file_id = None;
                    current.data_key = None;
                }
                LayoutFlip::KeepLayout => {}
            }
            save_token = save_token.max(op.fencing_token);
            METRICS
                .layout_publish_batched_blocks
                .fetch_add(op.entries.len() as u64, Ordering::Relaxed);
            batch_entries.extend(op.entries.iter().cloned());
            applied.push(Outcome {
                done: op.done,
                payload: displaced,
                enqueued_at: op.enqueued_at,
            });
        }
        // The freshest RAM size floor (writes publish size to the RAM
        // cache ahead of the deferred layout commit — a merge must not
        // regress it). Once per batch ≡ once per op serially.
        if let Some(cached) = self.metadata_cache.get(&ino) {
            if cached.size > current.size {
                current.size = cached.size;
            }
        }
        current.block_map = Some(block_map_arc);
        publish_phase_record(PublishPhase::Apply, t_apply);
        for o in fenced {
            publish_phase_record(PublishPhase::Total, o.enqueued_at);
            let _ = o.done.send(Err(SqueezefsError::FencingTokenExpired {
                token: o.payload,
                expected: current_fencing,
            }));
        }
        if applied.is_empty() {
            return;
        }
        METRICS
            .layout_publish_batches
            .fetch_add(1, Ordering::Relaxed);
        match self
            .save_metadata_to_backend_ext(ino, &current, save_token, Some(&batch_entries))
            .await
        {
            Ok(()) => {
                for o in applied {
                    publish_phase_record(PublishPhase::Total, o.enqueued_at);
                    let _ = o.done.send(Ok(o.payload));
                }
            }
            Err(e) => {
                for o in applied {
                    publish_phase_record(PublishPhase::Total, o.enqueued_at);
                    let _ = o.done.send(Err(dup_err(&e)));
                }
            }
        }
    }

    /// Bump the RAM metadata entry's size floor (write handler's
    /// `expected_new_size` publish).
    ///
    /// Runs under `INODE_META_LOCKS`: this is a get→mutate→insert on the same
    /// entry every block-map merge republishes, and an unserialized insert
    /// whose get (or refill await) completed just before a concurrent merge
    /// RESURRECTS THE PRE-MERGE MAP with a fresh `cached_at` — every read for
    /// the next TTL second then resolves block→key bindings whose keys are
    /// displaced, freed, and up for reallocation (the reused-key stale-fill
    /// family's widest window). The refill leg inlines `fetch_metadata`'s
    /// locked body (that call would retake this lock).
    pub async fn update_metadata_cache_size(&self, file_path: &str, size: u64) {
        let ino = parse_inode_from_path(file_path);
        let _meta_guard = meta_lock_acquire(ino).await;
        let entry = match self.metadata_cache.get(&ino) {
            Some(entry) => Some(entry),
            None => match self.fetch_metadata_from_backend(ino).await {
                Ok(Some(m)) => {
                    self.metadata_cache.insert(ino, m.clone());
                    Some(m)
                }
                Ok(None) | Err(_) => None,
            },
        };
        if let Some(mut entry) = entry {
            if size > entry.size {
                entry.size = size;
                // The grown size is ACKED-ONLY-HERE state: the striped
                // write path defers its durable size persist to the
                // flush/fsync cadence, so this RAM floor is the sole
                // carrier of the write's true end until then. Mark the
                // entry DIRTY — the local-authority rule (see
                // `fetch_metadata`'s aged-fsx note): a clean entry's TTL
                // refill (or any evict-and-refetch) would replace the
                // floor with the lagging durable size and reads would
                // clamp acked bytes away (the one-shot write-through
                // tail-byte loss, tests/write_through_tests.rs::
                // test_one_shot_full_block_write_through). Persisted and
                // cleaned by `persist_dirty_layout_if_needed` on the
                // fsync/release cadence like every dirty layout.
                entry.layout_dirty = true;
                entry.cached_at = std::time::Instant::now();
                self.metadata_cache.insert(ino, entry);
            }
        }
    }

    /// L4-8 (§5.5.1 "the 1 M+ engine"): the SYNC serve mirror for the IPC
    /// read fast path — exactly the sync-servable striped legs of
    /// [`Self::read_file_range_zero_copy`] (the staging mmap ring, the
    /// R4 hot-block tier, then — il hold-probe campaign 2026-08-03 —
    /// the read-lane hold as the fourth leg, in the handler ladder's
    /// order), no awaits, no blocking locks. `None` = any shape needing
    /// async work (W2 extent overlays, multi-block, cold blocks,
    /// non-striped layouts) — the caller demotes to the handoff, which
    /// runs the full handler; parity is by construction because these
    /// legs are pointwise copies of the handler's own (same keys, same
    /// clamps, same currency rules). A ledger-visible hold serve
    /// dispatches its R1b ceremony to the handler lanes (see the leg-2b
    /// comment); the serve itself completes on the service thread.
    ///
    /// Caller contract: `offset + read_len` already clamped to the
    /// authoritative size (the fast path's guarded size check), and the
    /// per-inode read guard held (mutators excluded — the same currency
    /// the handler's serve legs rely on).
    /// Sync tier serve **into the caller's sink** (the §5.5.1 warm fast
    /// path; op-economy campaign 2026-07-28): payload bytes land directly
    /// in the ring op's arena window — the former intermediate
    /// `Bytes::copy_from_slice` bounce (one alloc + one memcpy per warm
    /// op) is deleted. Returns the served byte count (short serves are
    /// the handler's own semantics); `None` = not sync-servable, demote.
    pub fn try_read_range_sync(
        &self,
        file_path: &str,
        meta: &CachedMetadata,
        offset: u64,
        read_len: usize,
        out: &dyn crate::PayloadSink,
    ) -> Option<usize> {
        if read_len == 0 {
            return None;
        }
        // The STAGED layout (whole-file ring blob keyed by file_id) —
        // mirror of the handler's staged serve: entry hit needs no
        // further validation (same-key re-stages replace atomically),
        // short blobs zero-pad (truncate-up hole tails), a W2 rider
        // overlay demotes to the handler's compose leg.
        if meta.file_type == "staged" {
            let file_id = meta.file_id.as_ref()?;
            if self.has_staged_extent_runs(file_path, 0) {
                return None;
            }
            let guard = self.cache.nvme.read_staged_zero_copy(file_id)?;
            let start = (offset as usize).min(guard.len);
            let end = (offset as usize + read_len).min(guard.len);
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            out.write_at(0, &guard[start..end]);
            if end - start < read_len {
                // Truncate-up hole tail: zero-pad to the full length,
                // exactly as the handler's staged serve would.
                out.zero_at(end - start, read_len - (end - start));
            }
            return Some(read_len);
        }
        if meta.file_type != "striped" {
            return None;
        }
        let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
        let end_offset = offset + read_len as u64;
        let start_block = (offset / block_size) as u32;
        let end_block = ((end_offset - 1) / block_size) as u32;
        if start_block != end_block {
            return None;
        }
        let b_start = start_block as u64 * block_size;
        let rel_s = (offset - b_start) as usize;
        let rel_e = rel_s + read_len;

        // W2 overlay present ⇒ the compose leg (async) owns this read
        // ("overlay never invisible" — one latch-free probe otherwise).
        if self.has_staged_extent_runs(file_path, start_block) {
            return None;
        }

        // Leg 1: the staging mmap ring — same key, same clamps as the
        // handler's "check active block staging first" leg (short serves
        // included: a shorter staged image returns short, exactly as the
        // handler would).
        let cache_key = crate::keys::StackKey::format(format_args!(
            "active_block:{file_path}:block_{start_block}"
        ));
        let heap_key;
        let cache_key: &str = match &cache_key {
            Some(k) => k,
            None => {
                // Pathological path length (never an `inode_N` form):
                // heap fallback — a truncated key would serve wrong data.
                heap_key = crate::keys::active_block_for_path(file_path, start_block);
                &heap_key
            }
        };
        if let Some(guard) = self.cache.nvme.read_staged_zero_copy(cache_key) {
            let start = rel_s.min(guard.len);
            let end = rel_e.min(guard.len);
            out.write_at(0, &guard[start..end]);
            return Some(end - start);
        }

        // Leg 2: the R4 hot tier. Skipped wholesale under the
        // device-true diagnostic posture: per-op O_DIRECT-ness is not
        // visible on the ring, and quietly tier-serving there would turn
        // the device-true il row into a warm row (measurement fraud).
        if self.direct_device_true() {
            return None;
        }
        let part_key;
        let b_key: &str = if let Some(map) = &meta.block_map {
            map.get(&start_block)?
        } else if meta.block_map_id.is_some() {
            // Map-id without an inline map (anomalous — see
            // `load_striped_block_keys`): this sync leg cannot re-resolve
            // from the backend, so DEMOTE to the async handler (which can),
            // never fabricate a miss/hole.
            return None;
        } else {
            let prefix = meta.block_prefix.as_ref()?;
            match crate::keys::StackKey::format(format_args!("{prefix}/part_{start_block}")) {
                Some(k) => {
                    part_key = k;
                    &part_key
                }
                None => return None, // pathological prefix length: demote
            }
        };
        // `get_serving`: real reader serve — credit the user bytes as
        // admission payback (the governor's earned-vs-waste basis).
        if let Some(hot) = self.cache.hot_block.get_serving(b_key, read_len as u64) {
            let start = rel_s.min(hot.len());
            let end = rel_e.min(hot.len());
            METRICS.hot_block_hits.fetch_add(1, Ordering::Relaxed);
            out.write_at(0, &hot[start..end]);
            // Read-lane coverage credit (il hold-probe campaign,
            // 2026-08-03 — the handler hot arm's rule mirrored): this
            // consumption path retires the hold's copy too (no-op when
            // the key is not held), so `hold_evicted_unconsumed` keeps
            // meaning starvation on il rows.
            if self.read_lane.enabled() {
                self.cache
                    .read_lane_hold
                    .credit(b_key, (end - start) as u64);
            }
            return Some(end - start);
        }

        // Leg 2b: the read-lane hold — the sync fast path's fourth leg
        // (il hold-probe campaign, 2026-08-03; charter
        // `.benchmarks/2026-08-02-il-anomalies.md` §2: the kernel path
        // serves ~294 k ops/row from hold/hot at ~µs while the il
        // DIALED-P1.5 prelude direct-drove those misses device-true —
        // the whole −8 %/+110 µs cold-rand-4k deficit). Probe order
        // mirrors the handler ladder (hot strictly first, so warm hot
        // entries keep funding the governor's payback basis); the probe
        // itself is `serve_with_provenance` — lock-free scc, callable
        // from the foreign service thread. Binding currency is leg 3's
        // own argument, structural here: the caller holds this inode's
        // read guard and `b_key` came from the CURRENT map. A miss
        // falls through unchanged (leg 3 → demote/direct-drive —
        // fallback-is-correctness).
        if self.read_lane.enabled() {
            if let Some((held, ledger_visible)) =
                self.cache.read_lane_hold.serve_with_provenance(b_key, 0)
            {
                let start = rel_s.min(held.len());
                let end = rel_e.min(held.len());
                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                METRICS.read_lane_serves.fetch_add(1, Ordering::Relaxed);
                METRICS
                    .read_lane_serve_bytes
                    .fetch_add((end - start) as u64, Ordering::Relaxed);
                METRICS
                    .ipc_hold_probe_serves
                    .fetch_add(1, Ordering::Relaxed);
                out.write_at(0, &held[start..end]);
                self.cache
                    .read_lane_hold
                    .credit(b_key, (end - start) as u64);
                // R1b ledger visibility (the `3cd528b` law's THIRD serve
                // site): a demand-deposited entry served after the hot
                // probe missed is the pre-lane device refetch in serve
                // form — it carries the admission ceremony (ghost touch
                // → second-touch publish → protected hot re-landing,
                // `hold_serve_admission`) with it. The ceremony's legs
                // await (tier publish rides spawn_blocking), so it
                // cannot run on this foreign service thread: dispatch
                // it to the fuse3 per-core handler lanes — `tpc_spawn`,
                // the 2026-07-26 handoff-economy venue (NEVER
                // `Handle::spawn` from a foreign thread: that lands on
                // the global inject queue, the measured ~130 µs/op
                // term). One serve ⇒ one dispatched ceremony
                // (exactly-once; concurrent same-key serves may
                // duplicate it — the GhostTable's racy-tolerant class,
                // identical to the kernel serve sites). Lane-fetch
                // deposits stay ledger-invisible end to end (the
                // scan-resistance verdict).
                if ledger_visible {
                    let class = if self
                        .stream_lanes
                        .get(file_path)
                        .is_some_and(|l| l.any_streaming_fresh())
                    {
                        FillClass::DemandStream
                    } else {
                        FillClass::Demand
                    };
                    let router = self.clone();
                    let bk = b_key.to_string();
                    let held_for_ceremony = held.clone();
                    fuse3::raw::tpc_spawn(async move {
                        router
                            .hold_serve_admission(&bk, &held_for_ceremony, class)
                            .await;
                    });
                }
                return Some(end - start);
            }
            // The probe ran (lane armed, binding resolved, hot missed)
            // and found nothing — the engagement pair's miss half.
            METRICS
                .ipc_hold_probe_misses
                .fetch_add(1, Ordering::Relaxed);
        }

        // Leg 3: the NVMe read-cache shard (sync mmap — the handler's
        // "tier fast path with binding recheck" leg). Binding currency:
        // the handler rechecks via `current_block_binding().await`, whose
        // source under a warm metadata cache is exactly the CURRENT map
        // `b_key` was just derived from — and this daemon's writers
        // update that map synchronously under the inode lock the caller
        // holds. Same currency class, no await.
        let guard = self.cache.nvme.get_cached_read_block_range_zero_copy(
            b_key,
            rel_s as u64,
            read_len as u32,
        )?;
        METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
        out.write_at(0, &guard);
        Some(guard.len)
    }

    pub async fn load_striped_block_keys(
        &self,
        file_path: &str,
        meta: &CachedMetadata,
        start_block: u32,
        end_block: u32,
    ) -> Result<Vec<(u32, Option<String>)>> {
        let mut block_keys = Vec::new();

        if let Some(block_map) = &meta.block_map {
            for b in start_block..=end_block {
                let key_opt = block_map.get(&b).cloned();
                block_keys.push((b, key_opt));
            }
        } else if meta.block_map_id.is_some() {
            // A map-id WITHOUT an inline map: `fetch_metadata_from_backend`
            // always inline-resolves (indirect maps rehydrate into
            // `block_map`), so this shape only arises from an anomalous RAM
            // entry. The old arm consulted a moka `block_map_cache` whose
            // populate sites died with the Redis removal (23ed315) — every
            // miss silently resolved the block as a HOLE, a fabricate-zeros
            // engine (the generic/795 gate found it). Re-resolve
            // AUTHORITATIVELY from the backend instead; if the backend
            // cannot produce a map either, fail LOUD — never fabricate.
            let ino = parse_inode_from_path(file_path);
            log::warn!(
                "load_striped_block_keys: ino {ino} carries block_map_id without an \
                 inline map (anomalous entry) — re-resolving from the backend"
            );
            match self.fetch_metadata_from_backend(ino).await? {
                Some(fresh) if fresh.block_map.is_some() => {
                    let map = fresh.block_map.as_ref().expect("checked is_some");
                    for b in start_block..=end_block {
                        block_keys.push((b, map.get(&b).cloned()));
                    }
                }
                _ => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "striped ino {ino}: block map unresolvable (id present, no \
                         inline map, backend re-fetch empty) — refusing to serve \
                         fabricated holes"
                    )));
                }
            }
        } else if let Some(block_prefix) = &meta.block_prefix {
            for b in start_block..=end_block {
                block_keys.push((b, Some(format!("{}/part_{}", block_prefix, b))));
            }
        } else {
            return Err(SqueezefsError::InvalidOperation(
                "Missing block_map and block_prefix for striped file".to_string(),
            ));
        }

        block_keys.sort_by_key(|(block_idx, _)| *block_idx);
        Ok(block_keys)
    }

    /// Write file data using progressive data layout routing with offset support (POSIX random-access RMW).
    /// Write path with phased meta connections (P1-10): Redis/Garnet work uses
    /// short-lived connections; durable NVMe / staging I/O never holds a pooled
    /// meta connection across the await.
    pub async fn write_file(
        &self,
        file_path: &str,
        offset: u64,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        METRICS.meta_updates.fetch_add(1, Ordering::Relaxed);

        let ino = parse_inode_from_path(file_path);
        let meta_key = crate::keys::metadata_for_path(file_path);

        let current_fencing = self.dlm.get_fencing_token_ino(ino);
        if fencing_token < current_fencing {
            return Err(crate::error::SqueezefsError::FencingTokenExpired {
                token: fencing_token,
                expected: current_fencing,
            });
        }

        let meta = self.fetch_metadata(file_path).await?;
        if meta.file_type == "striped" {
            self.write_striped(file_path, &meta_key, offset, data, fencing_token)
                .await?;
            crate::fuse_client::METRICS
                .layout_striped_writes
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let end_offset = (offset as usize) + data.len();
        let _staged_block_guard = if meta.file_type == "staged"
            || (meta.file_type == "inline"
                && end_offset > crate::fuse_client::MAX_INLINE_SIZE as usize)
        {
            // RW1: the staged_write lock site (the FIND-VS-B staged-layout
            // sibling shape) — same lock, rig-attributed.
            Some(
                crate::fuse_client::block_lock_acquire(
                    ino,
                    0,
                    crate::fuse_client::BlockLockSite::StagedWrite,
                )
                .await,
            )
        } else {
            None
        };

        // Re-resolve the layout identity UNDER the guard (RW4): the
        // snapshot above was taken before the lock, so a write/spill/
        // promotion that committed while we waited leaves it naming a DEAD
        // identity — the RMW seed below would then resolve through the
        // "identity flipped" degrade arm and codify a short/zeros base
        // over live acked bytes (surfaced by the W2 rider's fsync-driven
        // fold writes racing a same-ino writer; the window predates W2).
        let meta = if _staged_block_guard.is_some() {
            let fresh = self.fetch_metadata(file_path).await?;
            if fresh.file_type == "striped" {
                // The layout flipped striped while we waited: this write
                // belongs to the striped path now.
                drop(_staged_block_guard);
                self.write_striped(file_path, &meta_key, offset, data, fencing_token)
                    .await?;
                crate::fuse_client::METRICS
                    .layout_striped_writes
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            fresh
        } else {
            meta
        };

        // W2 staged-layout rider (design-random-small-writes §5.2): a
        // SUB-IMAGE overwrite of a ring-resident staged file rides a
        // 4 KiB-class extent record instead of the whole-image RMW
        // (seed + same-key re-stage — the FIND-VS-B storm shape). Bounded
        // win: ≤ 25 % of the image, non-extending, under the fold
        // thresholds; everything else (and a refused record put) takes
        // today's whole-image path, which FOLDS any existing record first.
        if meta.file_type == "staged" && !data.is_empty() {
            if let Some(fid) = meta.file_id.as_deref() {
                if let Some(img_len) = self.cache.nvme.staged_len(fid) {
                    let small = (data.len() as u64) * 4 <= img_len.max(1);
                    // Bound by the staged IMAGE the record rides on —
                    // never by `meta.size` (fstests generic/075.3, VL10
                    // release gate): a truncate-UP inflates the size past
                    // the image (even past the 4 MiB allocator chunk), and
                    // a size-bounded admission then parks extents the fold
                    // composes into an image the FIND-RW4-A guard rightly
                    // refuses to store — fsync wedges EIO forever
                    // (`domapwrite: msync: EIO` under fsx). Beyond-image
                    // writes take the whole-image path below, which grows
                    // and promotes correctly.
                    let non_extending = offset + data.len() as u64 <= img_len.min(meta.size);
                    if small && non_extending {
                        // Record mutations serialize under the HELD block-0
                        // guard (every rider site holds it; truncate holds
                        // the inode write guard); promotions never touch
                        // records (they DEFER on record-bearing files), so
                        // no meta-lock section is needed here.
                        let ext_key = crate::keys::active_block_ext(ino, 0).to_string();
                        let mut extents: Vec<(u32, Vec<u8>)> =
                            match self.cache.nvme.read_extent_record(&ext_key) {
                                Some(Ok(rec)) => rec.extents,
                                Some(Err(_)) => Vec::new(), // recovery disposes loudly
                                None => Vec::new(),
                            };
                        let payload: u64 = extents.iter().map(|(_, d)| d.len() as u64).sum();
                        let fm = crate::fuse_client::fold_max_extents();
                        let fb = crate::fuse_client::fold_max_bytes();
                        let under_thresholds = (fm == 0 || (extents.len() as u64) < fm)
                            && (fb == 0 || payload + data.len() as u64 <= fb);
                        if under_thresholds {
                            merge_extent_run(&mut extents, offset as u32, &data);
                            let record = crate::cache::nvme::ExtentRecord {
                                version: crate::cache::nvme::EXTENT_RECORD_VERSION,
                                fencing_token,
                                block_idx: 0,
                                // The complement owes the staged image /
                                // promoted block — a fold must seed (safe
                                // direction even across a layout flip).
                                base_deferred: true,
                                extents,
                            };
                            // Fence racing promotions FIRST: a commit that
                            // validates its generation after this put must
                            // observe the bump and abort (never retire a
                            // record it did not compose).
                            self.cache.nvme.bump_staged_generation(fid).await;
                            let nvme = self.cache.nvme.clone();
                            let ek = ext_key.clone();
                            let admitted = tokio::task::spawn_blocking(move || {
                                nvme.put_extent_record(&ek, &record)
                            })
                            .await
                            .map_err(|e| {
                                SqueezefsError::Io(std::io::Error::other(e.to_string()))
                            })?;
                            if admitted {
                                self.cache.write_lru.remove(file_path);
                                self.cache.read_lru.remove(file_path);
                                crate::fuse_client::METRICS
                                    .staged_rider_extent_writes
                                    .fetch_add(1, Ordering::Relaxed);
                                crate::fuse_client::METRICS
                                    .extent_spill_bytes
                                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                                return Ok(());
                            }
                            // Ring refused (never-lossy backpressure): the
                            // whole-image path below owns the write.
                        }
                    }
                }
            }
        }

        let stripe_threshold = if self.cache.nvme.staging_dirs().is_empty() {
            MAX_INLINE_SIZE
        } else {
            self.block_size.load(Ordering::Acquire) as usize
        };

        // Full overwrite of empty / new file (common small-file create+write path):
        // skip loading prior payload and avoid an extra Vec assemble when offset==0.
        let full_overwrite_empty = offset == 0
            && meta.size == 0
            && meta.data_key.as_ref().map(|d| d.is_empty()).unwrap_or(true)
            && meta.file_id.is_none()
            && meta
                .block_map
                .as_ref()
                .map(|m| m.is_empty())
                .unwrap_or(true);

        // RMW base. The whole-file RAM snapshot (write_lru/read_lru keyed by
        // file_path) is trusted ONLY for inline files, where it is coherent
        // (small, fully rewritten each write). For a STAGED file it can be a
        // stale snapshot left by a prior `read_file` (copy_file_range source
        // read) or an inline-era write — RMW-ing from it would re-stage stale
        // bytes and revert a just-punched/written range (the generic/616
        // residual). Staged reads its authoritative base from the staging ring
        // (or the promoted durable block), which tracks every mutation.
        // Follow-up C (the staged-RMW allocation flood, dhat-attributed):
        // the whole-image RMW seed lives in a recycled `BUFFER_POOL`
        // backing, never a fresh ~image-sized `Vec` per sub-block write —
        // at fsx storm rates the old path churned ~21 GB of 4 MiB-class
        // heap per 30 k ops, and its jemalloc retention was the
        // aged-daemon cage-kill class. Transient by construction on the
        // hot (staged) shape: the ring stays authoritative and the staged
        // arm removes both LRU entries, so the buffer recycles at drop /
        // `into_bytes` release. The inline arm re-materializes an
        // exact-size copy before RETAINING (see below) so a ≤ 4 KiB
        // inline payload never pins a pooled 4 MiB backing.
        let mut existing_data = BUFFER_POOL.alloc();
        if full_overwrite_empty {
            // empty seed
        } else if meta.file_type == "inline" {
            let src: Option<bytes::Bytes> = self
                .cache
                .write_lru
                .get(file_path)
                .or_else(|| self.cache.read_lru.get(file_path))
                .or_else(|| meta.data_key.clone());
            if let Some(src) = src {
                existing_data.resize(src.len(), 0);
                existing_data.copy_from_slice(&src);
            }
        } else {
            match meta.file_type.as_str() {
                "staged" => {
                    if let Some(ref file_id) = meta.file_id {
                        if self
                            .cache
                            .nvme
                            .read_staged_into(file_id, &mut existing_data)
                        {
                            crate::fuse_client::METRICS
                                .staged_rmw_pooled_seeds
                                .fetch_add(1, Ordering::Relaxed);
                        } else {
                            // Ring miss under the inode write lock +
                            // BLOCK_FLUSH_LOCKS: the only racer that can move
                            // this identity is the merge-worker promotion,
                            // which publishes `block_map[0]` (RAM cache +
                            // backend) strictly BEFORE removing the ring
                            // entry — so resolve the promoted mapping through
                            // ALL sources (snapshot, cache, backend), not the
                            // possibly-pre-promotion snapshot alone. A miss
                            // everywhere = genuine crash loss (zeros base per
                            // the D0 degrade contract).
                            // Binding-revalidated fetch (the promoted-mapping
                            // ABA, WRITE-side — the read path learned this in
                            // the 074/127/616 family): the mapping resolved
                            // here is freed the moment ANY layout commit
                            // supersedes it (an RMW commit releasing it as
                            // superseded, a re-promotion displacing it, a
                            // truncate clip), and the allocator hands the
                            // offset straight to the next promotion of the
                            // SAME file — the leg-5 residual tape shows one
                            // offset ping-ponging promote→free→realloc at
                            // storm rate. An unvalidated read then seeds the
                            // whole-image rebuild from punched zeros /
                            // another incarnation's bytes and CODIFIES them.
                            // Serve rule: only a fetch whose binding is still
                            // the CURRENT identity after the read completed
                            // may seed; on movement re-resolve and retry —
                            // bounded, then loud (never a silent zeros seed
                            // for acked data).
                            let mut attempts = 0u32;
                            let mut mapping_opt = self.staged_block_mapping(file_path, &meta).await;
                            while let Some(mapping_str) = mapping_opt.take() {
                                attempts += 1;
                                if attempts > 64 {
                                    return Err(SqueezefsError::InvalidOperation(format!(
                                        "staged RMW seed of {file_path} kept moving after \
                                         {attempts} re-resolves (offset {offset})"
                                    )));
                                }
                                let fetched = self.read_promoted_staged_block(&mapping_str).await;
                                let fresh = self.freshest_layout_identity(file_path).await;
                                let still_bound = fresh.as_ref().is_some_and(|f| {
                                    f.file_type == "staged"
                                        && f.file_id == meta.file_id
                                        && f.block_map
                                            .as_ref()
                                            .and_then(|bm| bm.get(&0))
                                            .is_some_and(|cur| *cur == mapping_str)
                                });
                                match fetched {
                                    Ok(mut plain) if still_bound => {
                                        let (_, _, _, exact) =
                                            self.parse_block_mapping(&mapping_str)?;
                                        if !exact && plain.len() as u64 > meta.size {
                                            // Bare legacy mapping: the whole-
                                            // block read's tail is another
                                            // tenant's device garbage a
                                            // passthrough transform cannot
                                            // strip. Bound by `meta.size` —
                                            // safe on THIS leg only (every
                                            // durable-mapping publish persists
                                            // the size in the same commit).
                                            plain = plain.slice(0..meta.size as usize);
                                        }
                                        existing_data.resize(plain.len(), 0);
                                        existing_data.copy_from_slice(&plain);
                                        break;
                                    }
                                    Err(e) if still_bound => return Err(e),
                                    _ => {
                                        // Binding moved (or the fetch hit the
                                        // freed window): re-resolve. A ring
                                        // entry re-appearing means a newer
                                        // re-stage owns the truth — the outer
                                        // ring-hit path can't be re-entered
                                        // here, but its content supersedes
                                        // this write's base only through the
                                        // fresh mapping/identity, so keep
                                        // resolving the freshest mapping.
                                        crate::fuse_client::METRICS
                                            .staged_identity_retries
                                            .fetch_add(1, Ordering::Relaxed);
                                        if let Some(f) = fresh {
                                            if f.file_id == meta.file_id && f.file_type == "staged"
                                            {
                                                if let Some(id) = f.file_id.as_deref() {
                                                    if self
                                                        .cache
                                                        .nvme
                                                        .read_staged_into(id, &mut existing_data)
                                                    {
                                                        crate::fuse_client::METRICS
                                                            .staged_rmw_pooled_seeds
                                                            .fetch_add(1, Ordering::Relaxed);
                                                        break;
                                                    }
                                                }
                                                mapping_opt =
                                                    self.staged_block_mapping(file_path, &f).await;
                                                continue;
                                            }
                                        }
                                        // Identity flipped entirely: nothing
                                        // durable to seed from this shape.
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        };

        // W2 rider FOLD-FIRST (§5.2): any whole-image path (staged RMW,
        // extending write, layout transition, inline shrink-rewrite) folds
        // the staged extent record into its RMW base before proceeding —
        // the commit arms below retire the record after publishing. The
        // record's runs are NEWER than the base image; the incoming write
        // (newest of all) overlays afterwards in the payload assembly.
        let rider_ext_key = crate::keys::active_block_ext(ino, 0).to_string();
        let mut fold_rider = false;
        if self.cache.nvme.has_staged_extent_record(&rider_ext_key) {
            if let Some(Ok(rec)) = self.cache.nvme.read_extent_record(&rider_ext_key) {
                let max_end = rec
                    .extents
                    .iter()
                    .map(|(s0, d)| *s0 as usize + d.len())
                    .max()
                    .unwrap_or(0);
                if existing_data.len() < max_end {
                    existing_data.resize(max_end, 0);
                }
                for (s0, d) in &rec.extents {
                    existing_data[*s0 as usize..*s0 as usize + d.len()].copy_from_slice(d);
                }
                fold_rider = true;
            }
            // Torn/future records never compose here; the recovery sweep
            // owns their loud disposition.
        }

        // NOTE deliberately NO `meta.size` clamp on the seed: the physical
        // blob/durable image is authoritative when the cached logical size
        // lags LOW (hot-entry eviction + deferred layout persist — the
        // pinned `truncate_down_stale_size` contract). Stale-LONG images
        // (the aged-fsx resurrection) are killed at the source instead:
        // `truncate_layout` clips every physical tier in-place/durably, and
        // orders the ring patch (msync) before the KV size commit, so a
        // committed truncate is always physically effective
        // (`tests/staged_truncate_stale_tests.rs`).

        // Logical size after this write. Folding `meta.size` in preserves a
        // truncate-up / fallocate-extend hole (logical size beyond the
        // physical payload): without it a small write regressed the file to
        // its patched payload length, silently shrinking e.g. a 100 GiB
        // truncate-up to 4 KiB (tests/sparse_write_bounded_tests.rs).
        let existing_len = existing_data.len();
        let new_size = existing_len.max(end_offset).max(meta.size as usize);

        if new_size > stripe_threshold {
            // Transition layout → striped, SPARSELY (the generic/285 OOM fix):
            // file-content coverage is O(map), never O(logical size). Only the
            // data-bearing blocks — those intersecting the existing payload
            // [0, existing_len) or the new write [offset, end_offset) — are
            // assembled and written; every other index stays UNMAPPED (a hole
            // that reads zeros). The old path materialized the whole
            // [0, end_offset) span in RAM and wrote every zero-filled hole
            // block durably, which for seek_sanity_test's 8 TiB far write
            // meant an ~8 TiB Vec zero-fill (~108 GB RSS → daemon OOM) and
            // O(filesize) device writes for 64 KiB of data.
            //
            // Block data I/O runs unlocked; only the layout commit is
            // serialized against concurrent staged promotion (see
            // INODE_META_LOCKS).
            let block_size = self.block_size.load(Ordering::Acquire) as usize;
            let mut block_idxs = std::collections::BTreeSet::new();
            for b in 0..existing_len.div_ceil(block_size) {
                block_idxs.insert(b as u32);
            }
            if !data.is_empty() {
                let first = offset as usize / block_size;
                let last = (end_offset - 1) / block_size;
                for b in first..=last {
                    block_idxs.insert(b as u32);
                }
            }

            // Per-block assembly, bounded by the write size (+1 block of
            // existing payload): zeros base, existing bytes under, new data
            // over. Blocks fully covered by `data` are zero-copy slices.
            let data_start = offset as usize;
            let mut chunks: Vec<(u32, bytes::Bytes)> = Vec::with_capacity(block_idxs.len());
            for b in block_idxs {
                let start = b as usize * block_size;
                let chunk_len = block_size.min(new_size - start);
                let chunk = if data_start <= start && end_offset >= start + chunk_len {
                    // Entire chunk comes from the new write: slice, no copy
                    // (the common aligned full-block case of a large write).
                    data.slice(start - data_start..start - data_start + chunk_len)
                } else {
                    let mut buf = vec![0u8; chunk_len];
                    if existing_len > start {
                        let e_end = existing_len.min(start + chunk_len);
                        buf[..e_end - start].copy_from_slice(&existing_data[start..e_end]);
                    }
                    if data_start < start + chunk_len && end_offset > start {
                        let s = data_start.max(start);
                        let e = end_offset.min(start + chunk_len);
                        buf[s - start..e - start]
                            .copy_from_slice(&data[s - data_start..e - data_start]);
                    }
                    bytes::Bytes::from(buf)
                };
                chunks.push((b, chunk));
            }

            // The in-flight guards hold the fsck registry entries until
            // the layout commit below published the mappings (VL6a).
            let (block_mappings, _inflight_guards) =
                self.durable_write_sparse_blocks(chunks).await?;

            let mut block_map = std::collections::HashMap::new();
            for (idx, key) in block_mappings {
                block_map.insert(idx, key);
            }

            {
                let _meta_guard = meta_lock_acquire(ino).await;
                let fresh = self.metadata_cache.get(&ino);
                let mut updated_meta = meta.clone();
                updated_meta.file_type = "striped".into();
                updated_meta.size = new_size as u64;
                updated_meta.block_map = Some(std::sync::Arc::new(block_map));
                updated_meta.file_id = None;
                self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                    .await?;

                self.cache.write_lru.remove(file_path);
                self.cache.read_lru.remove(file_path);

                self.metadata_cache.insert(ino, updated_meta);
                // The staged form is superseded: release its ring entry
                // (budget) and any promoted/spilled durable copy.
                self.release_superseded_staged(
                    meta.file_id.as_deref(),
                    fresh.as_ref().and_then(|f| f.block_map.as_deref()),
                    None,
                )
                .await;
            }
            if fold_rider {
                self.retire_rider_record(ino).await;
            }
            crate::fuse_client::METRICS
                .layout_striped_writes
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // Small-layout patch/assemble. `end_offset <= new_size <=
        // stripe_threshold <= block_size` past the promotion branch above, so
        // this materialization is bounded by one block. `new_size` may still
        // exceed the payload length (a truncate-up hole tail): inline/staged
        // layouts carry that as an implicit-zero tail the read/RMW paths
        // already honor (see truncate_layout).
        let payload_bytes = if full_overwrite_empty && offset == 0 {
            data.clone()
        } else {
            if existing_data.len() < end_offset {
                existing_data.resize(end_offset, 0);
            }
            existing_data[offset as usize..end_offset].copy_from_slice(&data);
            if new_size <= MAX_INLINE_SIZE {
                // The inline arm RETAINS its payload (meta.data_key + both
                // RAM LRUs): re-materialize exact-size so a tiny inline
                // file never pins the pooled 4 MiB backing (the flood in
                // retention clothes). ≤ MAX_INLINE bytes — trivial.
                let exact = bytes::Bytes::copy_from_slice(&existing_data);
                drop(existing_data); // recycle the pooled backing now
                exact
            } else {
                // Staged/spill shapes are transient consumers (staging
                // copies into the mmap ring; the staged arm removes both
                // LRU entries): the pooled backing recycles when the last
                // `Bytes` handle drops.
                existing_data.into_bytes()
            }
        };

        if new_size <= MAX_INLINE_SIZE {
            // Layout: inline — RAM only until fsync/release (writeback).
            crate::fuse_client::METRICS
                .layout_inline_writes
                .fetch_add(1, Ordering::Relaxed);
            let shared_data = payload_bytes;

            let _meta_guard = meta_lock_acquire(ino).await;
            let fresh = self.metadata_cache.get(&ino);
            let mut updated_meta = meta.clone();
            updated_meta.file_type = "inline".into();
            updated_meta.size = new_size as u64;
            // Zero-copy store: `shared_data` is `Bytes`; clone is a refcount bump,
            // not a payload copy (was `shared_data.to_vec()` = full memcpy per write).
            updated_meta.data_key = Some(shared_data.clone());
            updated_meta.file_id = None;
            updated_meta.block_map = None;
            updated_meta.layout_dirty = true;
            updated_meta.cached_at = std::time::Instant::now();

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
            self.metadata_cache.insert(ino, updated_meta);
            // A truncated-then-rewritten staged/spilled file leaves a ring
            // entry and/or a durable copy behind: release them.
            self.release_superseded_staged(
                meta.file_id.as_deref(),
                fresh.as_ref().and_then(|f| f.block_map.as_deref()),
                None,
            )
            .await;
            if fold_rider {
                self.retire_rider_record(ino).await;
            }
        } else if !self.cache.nvme.staging_dirs().is_empty()
            && (new_size as u64) <= self.block_size.load(Ordering::Acquire)
        {
            // Layout: staged — mmap stage + RAM meta; MetaLV layout deferred to fsync.
            crate::fuse_client::METRICS
                .layout_staged_writes
                .fetch_add(1, Ordering::Relaxed);
            let new_file_id = meta
                .file_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string().into());

            let stage_res = self
                .cache
                .nvme
                .stage_write(
                    file_path,
                    &new_file_id,
                    payload_bytes.clone(),
                    fencing_token,
                )
                .await;

            let shared_data = payload_bytes;

            match stage_res {
                Ok(_) => {
                    let _meta_guard = meta_lock_acquire(ino).await;
                    let fresh = self.metadata_cache.get(&ino);
                    // Ring-residency check (leg 5 of the zeros-LOSS family).
                    // A promotion enqueued by THIS stage's high-water
                    // crossing can consume the just-staged image before we
                    // reach this lock: it commits (its generation check
                    // passes against this stage's generation), publishes
                    // `block_map[0]` (the `fresh` read above), and releases
                    // the ring entry. Publishing "ring is authoritative,
                    // map=None" and freeing fresh's mapping would then free
                    // the file's SOLE copy. The check is EXACT, not a
                    // TOCTOU: the promotion's ring release runs INSIDE its
                    // commit's `INODE_META_LOCKS` section (see
                    // `promote_staged_file`), so while we hold the lock no
                    // removal can interleave — present means present until
                    // we release.
                    let ring_resident = self.cache.nvme.staged_len(&new_file_id).is_some();
                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".into();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(new_file_id);
                    updated_meta.data_key = None;
                    if ring_resident {
                        updated_meta.block_map = None;
                        updated_meta.layout_dirty = true;
                    } else {
                        // A promotion consumed exactly our staged bytes:
                        // ADOPT its published mapping. DIRTY unconditionally
                        // — this entry is the only holder of the consistent
                        // {size, mapping} pair (the promotion may have
                        // persisted a pre-RMW size), and only dirty entries
                        // are refill-immune under the dirty-authority rule.
                        updated_meta.block_map = fresh.as_ref().and_then(|f| f.block_map.clone());
                        updated_meta.layout_dirty = true;
                    }
                    updated_meta.cached_at = std::time::Instant::now();
                    self.metadata_cache.insert(ino, updated_meta);
                    // Drop any stale whole-file RAM snapshot: the staging ring
                    // entry is now authoritative for this file, but a prior
                    // `read_file` (e.g. a copy_file_range source read) or an
                    // inline-era write may have cached the whole file under
                    // `file_path`. Reads prefer that cache (read_file_range*),
                    // so leaving it would serve pre-write bytes — including
                    // reading a just-punched/zeroed range as its stale prior
                    // contents (generic/616). The inline and spill branches
                    // already refresh these tiers; the staged branch must too.
                    self.cache.write_lru.remove(file_path);
                    self.cache.read_lru.remove(file_path);
                    // The fresh stage supersedes any promoted/spilled durable
                    // copy of older content — release it ONLY when the ring
                    // entry actually survives to be authoritative.
                    if ring_resident {
                        self.release_superseded_staged(
                            None,
                            fresh.as_ref().and_then(|f| f.block_map.as_deref()),
                            None,
                        )
                        .await;
                    }
                    if fold_rider {
                        self.retire_rider_record(ino).await;
                    }
                }
                Err(SqueezefsError::Io(ref e)) if e.kind() == std::io::ErrorKind::StorageFull => {
                    // FIND-RW5-A: the never-lossy escalation, counted.
                    crate::fuse_client::METRICS
                        .staged_spill_escalations
                        .fetch_add(1, Ordering::Relaxed);
                    // Spill is the designed degraded mode under sustained
                    // pressure and can fire thousands of times in a burst:
                    // one line per second + a suppressed count keeps the
                    // signal without drowning the log.
                    {
                        static LAST_SPILL_WARN: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        static SUPPRESSED: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let last = LAST_SPILL_WARN.load(Ordering::Relaxed);
                        if now != last
                            && LAST_SPILL_WARN
                                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                                .is_ok()
                        {
                            let suppressed = SUPPRESSED.swap(0, Ordering::Relaxed);
                            log::warn!(
                                "NVMe write staging cache full: direct synchronous backend block write for {} ({} similar spills suppressed)",
                                file_path, suppressed
                            );
                        } else {
                            SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let processed_data = self
                        .get_crypto()
                        .process_write_async(shared_data.clone())
                        .await?;

                    let (be_id, block_allocator, nvme_writer) =
                        self.backend_router.get_active_backend()?;
                    crate::block_allocator::ensure_stored_block_image_fits(
                        processed_data.len(),
                        block_allocator.chunk_size(),
                        "staged spill",
                    )?;
                    let be_offset = block_allocator.allocate_block().await?;
                    // PR VL6a: in-flight until `save_metadata_to_backend`
                    // below publishes the spill layout (scope-held).
                    let _inflight = block_allocator.inflight_register(be_offset);
                    // Size-carrying mapping (`bk:0:packed_len` — see
                    // `parse_block_mapping`).
                    let stored_block_key = format!(
                        "{}:0:{}",
                        self.backend_router.persist_block_key(&be_id, be_offset),
                        processed_data.len()
                    );

                    nvme_writer.write_block(be_offset, processed_data).await?;
                    block_allocator.publish_block(be_offset);

                    let mut block_map = std::collections::HashMap::new();
                    block_map.insert(0, stored_block_key.clone());

                    // Spill takes a *fresh* file_id: the stale ring entry
                    // under the old id must never shadow this newer durable
                    // payload on reads, and an in-flight promotion of the old
                    // id must never pass its identity check and clobber this
                    // layout with pre-spill content.
                    let spill_file_id = Uuid::new_v4().to_string();

                    let _meta_guard = meta_lock_acquire(ino).await;
                    let fresh = self.metadata_cache.get(&ino);
                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".into();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(spill_file_id.into());
                    updated_meta.data_key = None;
                    updated_meta.block_map = Some(std::sync::Arc::new(block_map));
                    // Durable backend write already happened — commit layout now.
                    updated_meta.layout_dirty = false;
                    self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                        .await?;
                    self.metadata_cache.insert(ino, updated_meta);
                    // Release the superseded stale ring entry (returns its
                    // budget) and any older durable copy it had.
                    self.release_superseded_staged(
                        meta.file_id.as_deref(),
                        fresh.as_ref().and_then(|f| f.block_map.as_deref()),
                        Some(&stored_block_key),
                    )
                    .await;
                    if fold_rider {
                        self.retire_rider_record(ino).await;
                    }
                }
                Err(e) => return Err(e),
            }

            self.cache.write_lru.put(file_path, shared_data.clone());
            self.cache.read_lru.put(file_path, shared_data);
        }
        // No further arm: past the promotion branch `new_size <=
        // stripe_threshold`, which is MAX_INLINE_SIZE on cache-less volumes
        // (inline arm always matches) and block_size when staging dirs exist
        // (staged arm always matches).

        Ok(())
    }

    /// Persist a SPARSE set of `(block_idx, chunk)` stripe blocks on the
    /// active backend, returning their `(block_idx, block_key)` mappings.
    /// Indices absent from `chunks` stay unmapped — holes that read zeros —
    /// which is what keeps layout promotion O(map) instead of O(logical size)
    /// (the generic/285 OOM fix).
    ///
    /// Every block I/O is **awaited**. On any allocate/crypto/write failure, all
    /// blocks allocated in this call are freed and the error is returned so the
    /// caller can leave the prior layout (inline/staged) untouched.
    /// PR VL6a: the returned [`InflightAllocGuard`]s register every
    /// fresh block as live-owner in-flight — the caller MUST hold them
    /// until its layout commit published the mappings (drop-on-error is
    /// exactly right: the error paths free the blocks).
    async fn durable_write_sparse_blocks(
        &self,
        chunks: Vec<(u32, bytes::Bytes)>,
    ) -> Result<(
        Vec<(u32, String)>,
        Vec<crate::block_allocator::InflightAllocGuard>,
    )> {
        let mut block_mappings: Vec<(u32, String)> = Vec::new();
        let mut allocated_keys: Vec<String> = Vec::new();
        let mut inflight_guards: Vec<crate::block_allocator::InflightAllocGuard> = Vec::new();

        for (block_idx, chunk) in chunks {
            let (be_id, block_allocator, nvme_writer) =
                match self.backend_router.get_active_backend() {
                    Ok(res) => res,
                    Err(e) => {
                        for k in &allocated_keys {
                            let _ = self.backend_router.free_block(k).await;
                        }
                        return Err(e);
                    }
                };

            let offset = match block_allocator.allocate_block().await {
                Ok(o) => o,
                Err(e) => {
                    for k in &allocated_keys {
                        let _ = self.backend_router.free_block(k).await;
                    }
                    return Err(e);
                }
            };
            let stored_block_key = self.backend_router.persist_block_key(&be_id, offset);
            allocated_keys.push(stored_block_key.clone());
            inflight_guards.push(block_allocator.inflight_register(offset));

            let processed = match self.get_crypto().process_write_async(chunk.clone()).await {
                Ok(p) => p,
                Err(e) => {
                    for k in &allocated_keys {
                        let _ = self.backend_router.free_block(k).await;
                    }
                    return Err(e);
                }
            };
            if let Err(e) = crate::block_allocator::ensure_stored_block_image_fits(
                processed.len(),
                block_allocator.chunk_size(),
                "sparse stripe write",
            ) {
                for k in &allocated_keys {
                    let _ = self.backend_router.free_block(k).await;
                }
                return Err(e);
            }

            if let Err(e) = nvme_writer.write_block(offset, processed).await {
                for k in &allocated_keys {
                    let _ = self.backend_router.free_block(k).await;
                }
                return Err(e);
            }

            // Cache plaintext block for subsequent reads (key = block key, not
            // file path). The fresh put covers the RAM tier; purge the NVMe
            // read tier like the no-put owners do (`upload_full_block`) — a
            // validated fill of this key's dying incarnation may have
            // published there before our allocate.
            self.cache.purge_block_key(&stored_block_key);
            self.cache.read_lru.put(&stored_block_key, chunk);
            block_allocator.publish_block(offset);

            block_mappings.push((block_idx, stored_block_key));
        }

        Ok((block_mappings, inflight_guards))
    }

    /// Register a completed stripe layout in MetaLV after durable block writes.
    /// Block-map, refcounts, and sizes are updated atomically using the WAL-redo transaction scope.

    /// Striped RMW. P1-10: meta connections are phased — open for block-map
    /// reads, **dropped** before durable block I/O, re-acquired only for the
    /// atomic map/size commit and refcount cleanup (frees run after redis work).
    async fn write_striped(
        &self,
        file_path: &str,
        _meta_key: &str,
        offset: u64,
        data: bytes::Bytes,
        _fencing_token: u64,
    ) -> Result<()> {
        let ino = parse_inode_from_path(file_path);
        let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
        let end_pos = offset + data.len() as u64;
        let start_block = (offset / block_size) as u32;
        let end_block = if data.is_empty() {
            start_block
        } else {
            ((end_pos - 1) / block_size) as u32
        };

        if data.is_empty() {
            return Ok(());
        }

        let meta = self.fetch_metadata(file_path).await?;
        let existing_size = meta.size;
        let block_map = meta.block_map.clone().unwrap_or_default();

        let mut old_block_keys = Vec::new();
        for b in start_block..=end_block {
            old_block_keys.push(block_map.get(&b).cloned());
        }

        // Spawn tasks to modify affected blocks concurrently. The old
        // main-thread cache pre-resolve is gone: a pre-resolved buffer is a
        // binding snapshot that ages while the task waits to run — exactly
        // the reused-key stale-fill window — so every RMW seed now resolves
        // through the binding-validated fetch inside its task.
        use futures::stream::{FuturesUnordered, StreamExt};
        let tasks = FuturesUnordered::new();
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(
            crate::bg_admit::striped_block_concurrency(),
        ));

        let mut old_keys_iter = old_block_keys.into_iter();
        for b in start_block..=end_block {
            let old_block_key = old_keys_iter.next().unwrap();

            let block_start_file_offset = b as u64 * block_size;
            let block_end_file_offset = block_start_file_offset + block_size;

            let overlap_start = std::cmp::max(block_start_file_offset, offset);
            let overlap_end = std::cmp::min(block_end_file_offset, end_pos);

            let rel_start = (overlap_start - block_start_file_offset) as usize;
            let rel_end = (overlap_end - block_start_file_offset) as usize;

            let data_slice = unsafe {
                let sub = data.get_unchecked(
                    (overlap_start - offset) as usize..(overlap_end - offset) as usize,
                );
                data.slice_ref(sub)
            };

            let router_clone = self.clone();
            let crypto = self.get_crypto().clone();
            let read_lru = self.cache.read_lru.clone();
            let file_path_clone = file_path.to_string();

            let needs_existing = {
                let existing_block_end = std::cmp::min(existing_size, block_end_file_offset);
                existing_block_end > block_start_file_offset
                    && (overlap_start > block_start_file_offset || overlap_end < existing_block_end)
            };

            // §5.6 (PR 6) full-coverage slice reuse (kills audit #12 here):
            // when the overlap covers the whole block there is no existing
            // data to RMW-seed (`needs_existing` is provably false) — the
            // payload slice IS the block, so it flows to crypto/DMA and into
            // the read LRU directly instead of being copied into a
            // `PooledBuf` first. Lease-safe by construction: the §5.4
            // severance boundary guarantees no transport lease ever reaches
            // `DataRouter::write_file`, so retaining `data_slice` retains a
            // private copy. The RMW-seed copy below (partial coverage,
            // audit #13) is untouched by design.
            let full_coverage = rel_start == 0 && rel_end == block_size as usize;

            let sem_clone = sem.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = sem_clone.acquire().await.map_err(|e| {
                    SqueezefsError::Io(std::io::Error::other(format!(
                        "Semaphore acquire error: {:?}",
                        e
                    )))
                })?;

                let block_bytes = if full_coverage {
                    data_slice
                } else {
                    let mut block_data = if needs_existing {
                        // Binding-validated RMW seed (reused-key stale-fill
                        // family): the resolved key may have been displaced,
                        // freed and reallocated to ANOTHER block by the time
                        // this task runs — seeding from it would merge user
                        // data over a foreign block's bytes and upload the
                        // result (persistent corruption). A hole rebind
                        // seeds zeros.
                        match router_clone
                            .get_block_for_index(
                                &file_path_clone,
                                b,
                                old_block_key.as_deref(),
                                false,
                                // Write-side RMW seed: no contention
                                // escalation (VL8 item 7) — write-vs-patch
                                // of one block already serialize on its
                                // stripe; escalating from a write task
                                // risks a same-stripe self-wait.
                                false,
                            )
                            .await?
                        {
                            Some(crate::cache::pool::ReadBlockValue::Pooled(p)) => p,
                            Some(crate::cache::pool::ReadBlockValue::Bytes(b)) => {
                                let mut pooled = BUFFER_POOL.alloc();
                                pooled.resize(b.len(), 0);
                                pooled.copy_from_slice(&b);
                                pooled
                            }
                            None => {
                                let mut pooled = BUFFER_POOL.alloc();
                                pooled.resize(rel_end, 0);
                                pooled
                            }
                        }
                    } else {
                        let mut pooled = BUFFER_POOL.alloc();
                        pooled.resize(rel_end, 0);
                        pooled
                    };

                    if block_data.len() < rel_end {
                        block_data.resize(rel_end, 0);
                    }

                    unsafe {
                        block_data
                            .get_unchecked_mut(rel_start..rel_end)
                            .copy_from_slice(&data_slice);
                    }

                    block_data.into_bytes()
                };

                let (be_id, block_allocator, nvme_writer) =
                    router_clone.backend_router.get_active_backend()?;
                let offset = block_allocator.allocate_block().await?;
                // PR VL6a: live-owner registration rides the task result
                // back to the caller, which holds it across the merge.
                let inflight = block_allocator.inflight_register(offset);
                let stored_new_block_key = router_clone
                    .backend_router
                    .persist_block_key(&be_id, offset);

                let processed_block = crypto.process_write_async(block_bytes.clone()).await?;
                if let Err(e) = crate::block_allocator::ensure_stored_block_image_fits(
                    processed_block.len(),
                    block_allocator.chunk_size(),
                    "striped RMW block write",
                ) {
                    let _ = block_allocator.free_block(offset).await;
                    return Err(e);
                }
                nvme_writer.write_block(offset, processed_block).await?;

                // Cache + publish only after the device write: a racing
                // validated fill for this key must either see the durable bytes
                // or fail its incarnation check — never observe (and cache) the
                // pre-write contents of a reused offset. The fresh put covers
                // the RAM tier; the NVMe read tier must be PURGED like the
                // no-put owners do (`upload_full_block`) — a validated fill of
                // the key's dying incarnation may have published there before
                // our allocate, and a put-owner that only overwrites RAM
                // leaves that entry to serve dead bytes once the RAM entry
                // evicts.
                router_clone.cache.purge_block_key(&stored_new_block_key);
                read_lru.put(&stored_new_block_key, block_bytes);
                block_allocator.publish_block(offset);

                Ok::<_, SqueezefsError>((b, stored_new_block_key, inflight))
            }));
        }

        let mut results = Vec::new();
        // Held across the merge below (VL6a live-owner window); dropped
        // with the function — after the publish — or on the error path
        // where the blocks are freed.
        let mut _inflight_guards: Vec<crate::block_allocator::InflightAllocGuard> = Vec::new();
        let mut first_err: Option<SqueezefsError> = None;
        let mut tasks_stream = tasks;
        while let Some(task_res) = tasks_stream.next().await {
            match task_res.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Block write task panicked: {:?}",
                    e
                )))
            }) {
                Ok(Ok((b, key, guard))) => {
                    results.push((b, key));
                    _inflight_guards.push(guard);
                }
                Ok(Err(e)) | Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            for (_b, new_key) in &results {
                let _ = self.backend_router.free_block(new_key).await;
            }
            return Err(e);
        }

        // Atomic per-inode layout merge through the shared primitive (§5.3
        // one merge discipline): read→merge→save serialized under
        // INODE_META_LOCKS against every other striped-map writer, merging
        // into the *current* map — never into our start-of-call snapshot,
        // which would drop concurrent writers' entries and revert blocks to
        // freed keys. The block data I/O above ran concurrently (COW to
        // fresh keys); displaced-from-current keys are freed only after the
        // new map is published (durable + cached), so no reader can resolve
        // a block to a key we are freeing.
        let displaced_keys = self
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&results),
                end_pos,
                LayoutFlip::ToStripedClearStagedIdentity,
                _fencing_token,
            )
            .await?;
        for bk in displaced_keys {
            let _ = self.backend_router.free_block(&bk).await;
        }

        // Drop any whole-file RAM snapshot: patching a shared whole-file buffer
        // under concurrent writers is itself a lost-update hazard. Reads
        // re-resolve through the now-consistent block map.
        self.cache.write_lru.remove(file_path);
        self.cache.read_lru.remove(file_path);
        Ok(())
    }

    pub async fn read_file_range_zero_copy(
        &self,
        file_path: &str,
        offset: u64,
        size: u32,
        dest_addr: Option<u64>,
        hint: ReadClassHint,
    ) -> Result<(
        bytes::Bytes,
        Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    )> {
        self.read_file_range_zero_copy_with_meta(file_path, offset, size, dest_addr, hint, None)
            .await
    }

    /// [`Self::read_file_range_zero_copy`] with an optional in-hand RAM
    /// metadata snapshot (P2 per-op economy): the FUSE read handler just
    /// probed the cache for size coherency — its snapshot serves the
    /// first resolve attempt instead of a second moka get + clone. The
    /// hint is trusted only under [`metadata_entry_fresh_or_dirty`] (the
    /// exact `fetch_metadata` gate — a >1 s-stale clean entry still
    /// re-validates from the backend), and every re-resolve inside the
    /// loop goes through `fetch_metadata` as before.
    pub async fn read_file_range_zero_copy_with_meta(
        &self,
        file_path: &str,
        offset: u64,
        size: u32,
        dest_addr: Option<u64>,
        hint: ReadClassHint,
        meta_hint: Option<CachedMetadata>,
    ) -> Result<(
        bytes::Bytes,
        Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    )> {
        // OQ-5 (lost-wakeup wedge, 2026-07-30): the warm all-RAM serve legs
        // below (fresh/dirty moka meta + staging-ring mmap / hot-block RAM
        // tier) can complete with ZERO tokio coop-budget leaves — a caller
        // task looping warm reads then never ends its poll, and any task it
        // wakes mid-loop (e.g. the M6 times-drain via a DLM stripe guard
        // drop) is scheduled into this worker's UNSTEALABLE LIFO slot and
        // starves forever (tokio's documented LIFO footgun, #4323/#4941;
        // the stealable-LIFO change #7431 was reverted upstream in 1.52.2
        // for perf). One budget unit per read op bounds every such loop to
        // one budget window (≤128) before the task yields; on foreign
        // threads without a runtime context this is a no-op (unconstrained
        // budget), so the IPC sync-lane serve is untouched. Pinned by
        // `warm_read_loop_yields_to_peer_tasks_on_one_worker` and the
        // OQ-5 storm-squeeze acceptance (.benchmarks/2026-07-30-oq5-*).
        tokio::task::coop::consume_budget().await;

        // Fetch metadata first. The whole-file RAM snapshot (read_lru /
        // write_lru keyed by file_path) is deliberately NOT consulted on the
        // read data path: it is a whole-file copy that goes stale under
        // sub-file mutations (write / punch / truncate) of staged & striped
        // files — a punched or truncated range would otherwise read its prior
        // bytes instead of zeros (the generic/616 residual). Each layout below
        // serves from its authoritative, mutation-tracking tier instead: the
        // inline `data_key`, the staging ring (staged), or the per-block read
        // caches / hole map (striped).
        // read_serve_phase_ns `meta_resolve` (≈ 0 with a fresh handler
        // hint; the backend round trip when the hint is stale/absent).
        let meta_t0 = std::time::Instant::now();
        let mut meta = match meta_hint {
            Some(h) if metadata_entry_fresh_or_dirty(&h) => h,
            _ => self.fetch_metadata(file_path).await?,
        };
        read_serve_phase_record(ReadServePhase::MetaResolve, meta_t0);

        // POSIX full-length below-EOF contract (the generic/617 short-read
        // family): every leg below returns EXACTLY
        // `min(size, logical_size - offset)` bytes for an in-bounds read.
        // Physical layouts legitimately under-cover the logical size (a
        // staged blob / inline data_key left short by truncate-up, an
        // extending far write, or a striped hole) — the uncovered remainder
        // is an implicit-zero hole that must be FILLED here, bounded by the
        // request length. The page cache masked short replies for buffered
        // reads; O_DIRECT (fsx -Z) hands them to userspace as short reads.
        //
        // Staged-identity revalidation (the fstests 074/127/616 transient-
        // zeros family): reads run UNLOCKED, so `meta` can be superseded
        // mid-read by a re-stage / promotion / spill / layout flip. Every
        // such transition publishes its NEW identity (RAM metadata cache +
        // backend as applicable) strictly BEFORE releasing the old one's
        // artifacts, so a read that loses its identity re-resolves the
        // freshest one and re-dispatches — bounded — instead of concluding
        // the payload is gone. The zeros-degrade leg is reserved for a
        // STABLE lost identity (crash recovery discarded the payload):
        // ring entry gone AND no promoted mapping AND no in-flight stage
        // (budget ledger empty) AND the identity unchanged on re-resolve.
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            if attempts > 64 {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "read_file_range_zero_copy: staged identity of {file_path} kept moving \
                     after {attempts} re-resolves (offset {offset}, size {size})"
                )));
            }
            match meta.file_type.as_str() {
                "inline" => {
                    if offset >= meta.size {
                        return Ok((bytes::Bytes::new(), None));
                    }
                    let want = (std::cmp::min(offset + size as u64, meta.size) - offset) as usize;
                    let dlen = meta.data_key.as_ref().map(|d| d.len()).unwrap_or(0);
                    let start = std::cmp::min(offset as usize, dlen);
                    let end = std::cmp::min(offset as usize + want, dlen);
                    let mut out = vec![0u8; want];
                    if start < end {
                        out[..end - start].copy_from_slice(
                            &meta.data_key.as_ref().expect("dlen > 0")[start..end],
                        );
                    }
                    return Ok((bytes::Bytes::from(out), None));
                }
                "staged" => {
                    let file_id = meta.file_id.clone().ok_or_else(|| {
                        SqueezefsError::InvalidOperation(
                            "Missing file_id for staged file".to_string(),
                        )
                    })?;

                    if let Some(guard) = self.cache.nvme.read_staged_zero_copy(&file_id) {
                        // Ring entries always hold their identity's CURRENT
                        // payload: a same-key re-stage replaces the entry
                        // atomically for readers (see NvmeShard::reserve_and_write
                        // phase 2), so a hit needs no further validation.
                        METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                        if offset >= meta.size {
                            return Ok((bytes::Bytes::new(), None));
                        }
                        // Full below-EOF length; the blob may be shorter than the
                        // logical size (truncate-up hole tail) — pad with zeros.
                        let want =
                            (std::cmp::min(offset + size as u64, meta.size) - offset) as usize;
                        let start = std::cmp::min(offset as usize, guard.len);
                        let end = std::cmp::min(offset as usize + want, guard.len);
                        let phys = end - start;
                        // W2 staged-layout rider (§5.2): overlay the
                        // record's runs (NEWER than the ring image) — one
                        // latch-free probe when absent.
                        let rider_runs = self.staged_extent_runs_in(
                            file_path,
                            0,
                            offset as usize,
                            offset as usize + want,
                        );
                        if !rider_runs.is_empty() {
                            let mut out = vec![0u8; want];
                            out[..phys].copy_from_slice(&guard[start..end]);
                            for (abs, d) in rider_runs {
                                let lo = abs - offset as usize;
                                out[lo..lo + d.len()].copy_from_slice(&d);
                            }
                            let (data, backing) = if let Some(dest) = dest_addr {
                                let dest_ptr = dest as *mut u8;
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        out.as_ptr(),
                                        dest_ptr,
                                        out.len(),
                                    );
                                    let d = bytes::Bytes::from_owner(
                                        crate::cache::pool::UringBufOwner {
                                            ptr: dest_ptr,
                                            len: out.len(),
                                        },
                                    );
                                    (d, None)
                                }
                            } else {
                                (bytes::Bytes::from(out), None)
                            };
                            return Ok((data, backing));
                        }
                        let (data, backing) = if let Some(dest) = dest_addr {
                            let dest_ptr = dest as *mut u8;
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    guard[start..end].as_ptr(),
                                    dest_ptr,
                                    phys,
                                );
                                if want > phys {
                                    std::ptr::write_bytes(dest_ptr.add(phys), 0, want - phys);
                                }
                                let d =
                                    bytes::Bytes::from_owner(crate::cache::pool::UringBufOwner {
                                        ptr: dest_ptr,
                                        len: want,
                                    });
                                (d, None)
                            }
                        } else if phys == want {
                            let mut sliced_guard = guard;
                            sliced_guard.offset += start;
                            sliced_guard.len = phys;
                            let d = bytes::Bytes::copy_from_slice(&sliced_guard);
                            (d, None)
                        } else {
                            let mut out = vec![0u8; want];
                            out[..phys].copy_from_slice(&guard[start..end]);
                            (bytes::Bytes::from(out), None)
                        };
                        return Ok((data, backing));
                    }

                    // Ring miss. A LIVE staged identity is reachable through
                    // exactly one of: a promoted/spilled durable mapping
                    // (published before the ring entry was released), an
                    // in-flight stage (budget ledger still counts the id), or a
                    // NEWER identity already published in the metadata cache
                    // (layout flip / spill re-id). Resolve in that order;
                    // degrade to zeros ONLY for a stable lost identity.
                    if let Some(bk) = self.staged_block_mapping(file_path, &meta).await {
                        if offset >= meta.size {
                            return Ok((bytes::Bytes::new(), None));
                        }
                        let fetched = self.read_promoted_staged_block(&bk).await;
                        // Binding revalidation (the promoted-mapping ABA — same
                        // serve rule as the striped binding-validated fetch):
                        // the durable copy is freed the moment a newer identity
                        // publishes (`release_superseded_staged` runs strictly
                        // AFTER the publish), so if the CURRENT identity still
                        // binds (staged, this file_id, block_map[0] == bk) after
                        // the fetch completed, the fetched bytes are the live
                        // incarnation. On movement: re-resolve and retry — a
                        // fetch error is surfaced only for a stable binding
                        // (real I/O error, not freed-and-reused bytes).
                        let fresh = self.freshest_layout_identity(file_path).await;
                        let still_bound = fresh.as_ref().is_some_and(|f| {
                            f.file_type == "staged"
                                && f.file_id.as_deref() == Some(&file_id[..])
                                && f.block_map
                                    .as_ref()
                                    .and_then(|bm| bm.get(&0))
                                    .is_some_and(|cur| *cur == bk)
                        });
                        match fetched {
                            Ok(decompressed) if still_bound => {
                                // Full below-EOF length: the promoted block may
                                // be shorter than the logical size — its tail
                                // (and any in-bounds offset past the physical
                                // end) is an implicit-zero hole.
                                let want = (std::cmp::min(offset + size as u64, meta.size) - offset)
                                    as usize;
                                let dlen = decompressed.len();
                                let start = std::cmp::min(offset as usize, dlen);
                                let end = std::cmp::min(offset as usize + want, dlen);
                                let mut out = vec![0u8; want];
                                if start < end {
                                    out[..end - start].copy_from_slice(&decompressed[start..end]);
                                }
                                // W2 rider: the record's runs are newer
                                // than the promoted image (the crash
                                // window between a promotion's publish
                                // and its record retire).
                                for (abs, d) in self.staged_extent_runs_in(
                                    file_path,
                                    0,
                                    offset as usize,
                                    offset as usize + want,
                                ) {
                                    let lo = abs - offset as usize;
                                    out[lo..lo + d.len()].copy_from_slice(&d);
                                }
                                return Ok((bytes::Bytes::from(out), None));
                            }
                            Err(e) if still_bound => return Err(e),
                            _ => {
                                METRICS
                                    .staged_identity_retries
                                    .fetch_add(1, Ordering::Relaxed);
                                if let Some(f) = fresh {
                                    meta = f;
                                }
                                continue;
                            }
                        }
                    }

                    // No mapping either: re-resolve the identity.
                    if let Some(fresh) = self.freshest_layout_identity(file_path).await {
                        if fresh.file_type != "staged"
                            || fresh.file_id.as_deref() != Some(&file_id[..])
                        {
                            // The identity MOVED (layout flip, spill re-id,
                            // re-created file): re-dispatch against it.
                            METRICS
                                .staged_identity_retries
                                .fetch_add(1, Ordering::Relaxed);
                            meta = fresh;
                            continue;
                        }
                    }
                    if self.cache.nvme.staged_generation(&file_id).await.is_some() {
                        // Same identity, budget-counted but not ring-resident: a
                        // stage/promotion of this id is IN FLIGHT. Bounded
                        // backoff, then re-resolve — never zeros for live data.
                        METRICS
                            .staged_identity_retries
                            .fetch_add(1, Ordering::Relaxed);
                        if attempts <= 4 {
                            tokio::task::yield_now().await;
                        } else {
                            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                        }
                        continue;
                    }

                    // STABLE lost identity: ring entry gone (crash-torn →
                    // discarded by segment recovery, or never flushed before a
                    // kill), no promoted mapping, no in-flight stage, and the
                    // identity unchanged on re-resolve. D0 degrade contract —
                    // acked-unfsynced staged data MAY be lost by a crash but
                    // must never error: serve size-consistent zeros (the file
                    // exists at meta.size with no backing bytes — hole
                    // semantics), loudly.
                    let len = lost_staged_range_len(meta.size, offset, size);
                    self.note_lost_staged_payload(file_path, &file_id);
                    return Ok((bytes::Bytes::from(vec![0u8; len]), None));
                }
                "striped" => {
                    let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
                    let file_size = meta.size;

                    if offset >= file_size {
                        return Ok((bytes::Bytes::new(), None));
                    }

                    let end_offset = std::cmp::min(offset + size as u64, file_size);
                    if offset >= end_offset {
                        return Ok((bytes::Bytes::new(), None));
                    }

                    let start_block = (offset / block_size) as u32;
                    let end_block = ((end_offset - 1) / block_size) as u32;

                    // 1. Single block read optimization: check cache and staging zero-copy
                    if start_block == end_block {
                        let b_idx = start_block;
                        let b_start_offset = b_idx as u64 * block_size;
                        let slice_start = offset - b_start_offset;
                        let slice_len = (end_offset - offset) as u32;
                        let cache_key =
                            crate::keys::active_block_for_path(file_path, b_idx).to_string();

                        // W2 (§5.2 "overlay never invisible"): a staged
                        // extent record's runs are NEWER than every base
                        // (staged image / tiers / device / hole). When one
                        // exists (one latch-free probe otherwise — R6),
                        // take the compose path: base bytes + runs overlay.
                        {
                            let rel_s = slice_start as usize;
                            let rel_e = (slice_start + slice_len as u64) as usize;
                            let ext_runs =
                                self.staged_extent_runs_in(file_path, b_idx, rel_s, rel_e);
                            if !ext_runs.is_empty() {
                                // Fully-covered requests never touch the
                                // base (the item-B "ACKed bytes serve from
                                // the overlay" law, record form).
                                let mut cursor = rel_s;
                                for (abs, d) in &ext_runs {
                                    if *abs > cursor {
                                        break;
                                    }
                                    cursor = cursor.max(abs + d.len());
                                }
                                let fully_covered = cursor >= rel_e;
                                let mut out = vec![0u8; rel_e - rel_s];
                                if fully_covered {
                                    // runs overlay below fills everything
                                } else if let Some(img) = self.cache.nvme.read_staged(&cache_key) {
                                    let start = rel_s.min(img.len());
                                    let end = rel_e.min(img.len());
                                    out[..end - start].copy_from_slice(&img[start..end]);
                                } else {
                                    let block_keys = self
                                        .load_striped_block_keys(
                                            file_path,
                                            &meta,
                                            start_block,
                                            end_block,
                                        )
                                        .await?;
                                    let bk = block_keys.first().and_then(|(_, k)| k.as_deref());
                                    if bk.is_some() {
                                        if let Some(base) = self
                                            .get_block_for_index(file_path, b_idx, bk, false, true)
                                            .await?
                                        {
                                            let start = rel_s.min(base.len());
                                            let end = rel_e.min(base.len());
                                            out[..end - start].copy_from_slice(&base[start..end]);
                                        }
                                    }
                                }
                                for (abs, d) in ext_runs {
                                    out[abs - rel_s..abs - rel_s + d.len()].copy_from_slice(&d);
                                }
                                let (data, backing) = if let Some(dest) = dest_addr {
                                    let dest_ptr = dest as *mut u8;
                                    unsafe {
                                        std::ptr::copy_nonoverlapping(
                                            out.as_ptr(),
                                            dest_ptr,
                                            out.len(),
                                        );
                                        let d = bytes::Bytes::from_owner(
                                            crate::cache::pool::UringBufOwner {
                                                ptr: dest_ptr,
                                                len: out.len(),
                                            },
                                        );
                                        (d, None)
                                    }
                                } else {
                                    (bytes::Bytes::from(out), None)
                                };
                                return Ok((data, backing));
                            }
                        }

                        // Check active block staging first
                        if let Some(guard) = self.cache.nvme.read_staged_zero_copy(&cache_key) {
                            let start = std::cmp::min(slice_start as usize, guard.len);
                            let end =
                                std::cmp::min((slice_start + slice_len as u64) as usize, guard.len);
                            let len = end - start;
                            let (data, backing) = if let Some(dest) = dest_addr {
                                let dest_ptr = dest as *mut u8;
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        guard[start..end].as_ptr(),
                                        dest_ptr,
                                        len,
                                    );
                                    let d = bytes::Bytes::from_owner(
                                        crate::cache::pool::UringBufOwner { ptr: dest_ptr, len },
                                    );
                                    (d, None)
                                }
                            } else {
                                let mut sliced_guard = guard;
                                sliced_guard.offset += start;
                                sliced_guard.len = len;
                                let d = bytes::Bytes::copy_from_slice(&sliced_guard);
                                (d, None)
                            };
                            return Ok((data, backing));
                        }

                        // Check the RAM tiers, then the NVMe read block cache
                        // (read_serve_phase_ns: `key_resolve` = the map
                        // resolve; `classify_probe` runs from here to the
                        // serve-or-fetch decision).
                        let key_t0 = std::time::Instant::now();
                        let block_keys = self
                            .load_striped_block_keys(file_path, &meta, start_block, end_block)
                            .await?;
                        read_serve_phase_record(ReadServePhase::KeyResolve, key_t0);
                        let probe_t0 = std::time::Instant::now();
                        // Hybrid-I/O diagnostic escape (user directive
                        // 2026-07-15): device-true O_DIRECT requests skip
                        // the classifier/pipeline (prefetch fills are
                        // publishes an escape-mode reader would never
                        // consume — 2x amplification otherwise), skip
                        // every tier serve probe, and dispatch straight
                        // to a validated device read below. Overlay
                        // probes above stay — RYW correctness is never
                        // diagnostic-optional.
                        let device_true = self.direct_device_true() && hint.odirect;
                        if device_true {
                            METRICS
                                .read_device_true_reads
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        // §5.5 pipeline driver — once per request, before
                        // the serve probes: consume bookkeeping, growth on
                        // foreground-wait (key already in the single-
                        // flight = the reader caught the pipeline), and
                        // the windowed top-up. Ring-originated requests
                        // fed the lanes at the sink (`lane_pre_fed`) —
                        // observing them again would declassify (§5.3).
                        if !device_true && !hint.lane_pre_fed {
                            let first_key = block_keys.first().and_then(|(_, k)| k.as_deref());
                            let will_wait = first_key.is_some_and(|k| {
                                self.inflight_block_reads.read_sync(k, |_, _| ()).is_some()
                            });
                            self.pipeline_touch(
                                file_path,
                                &meta,
                                block_size,
                                offset,
                                (end_offset - offset).max(1),
                                start_block,
                                end_block,
                                will_wait,
                                first_key,
                                true,
                            );
                        }
                        if let Some((_, b_key_opt)) = block_keys.first() {
                            // R4 hot-block fast path (§5.4): a hot hit takes
                            // the SAME binding recheck as the NVMe-tier hit
                            // below — hot entries hold current-incarnation
                            // bytes (device-validated fills + unified purge
                            // on free), so binding currency alone validates
                            // the serve. `Bytes` refcount hit; the reply
                            // slice is the only copy.
                            if let Some(b_key) = b_key_opt.as_ref().filter(|_| !device_true) {
                                // `get_serving`: real reader serve — credit
                                // the user bytes as admission payback (the
                                // governor's earned-vs-waste basis).
                                if let Some(hot) =
                                    self.cache.hot_block.get_serving(b_key, slice_len as u64)
                                {
                                    read_serve_phase_record(
                                        ReadServePhase::ClassifyProbe,
                                        probe_t0,
                                    );
                                    let start = std::cmp::min(slice_start as usize, hot.len());
                                    let end = std::cmp::min(
                                        (slice_start + slice_len as u64) as usize,
                                        hot.len(),
                                    );
                                    let slice_t0 = std::time::Instant::now();
                                    let data = if let Some(dest) = dest_addr {
                                        let len = end - start;
                                        let dest_ptr = dest as *mut u8;
                                        // Copy ledger: the lawful serve
                                        // copy into the zero-copy dest
                                        // (NT lever site — read serves
                                        // default cached, see nt_copy).
                                        // SAFETY: dest is registered
                                        // payload / validated arena
                                        // memory ≥ len; source is the
                                        // hot entry's private bytes.
                                        if unsafe {
                                            serve_copy_to_dest(
                                                dest_ptr,
                                                hot[start..end].as_ptr(),
                                                len,
                                                hint.dest_arena,
                                            )
                                        } {
                                            METRICS
                                                .nt_read_serve_bytes
                                                .fetch_add(len as u64, Ordering::Relaxed);
                                        }
                                        METRICS
                                            .read_copy_dest_bytes
                                            .fetch_add(len as u64, Ordering::Relaxed);
                                        bytes::Bytes::from_owner(
                                            crate::cache::pool::UringBufOwner {
                                                ptr: dest_ptr,
                                                len,
                                            },
                                        )
                                    } else {
                                        hot.slice(start..end)
                                    };
                                    read_serve_phase_record(ReadServePhase::SliceOut, slice_t0);
                                    let bind_t0 = std::time::Instant::now();
                                    let still_bound = self
                                        .current_block_binding(file_path, start_block)
                                        .await?
                                        .as_deref()
                                        == Some(b_key.as_str());
                                    read_serve_phase_record(ReadServePhase::BindingCheck, bind_t0);
                                    if still_bound {
                                        METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                                        METRICS.hot_block_hits.fetch_add(1, Ordering::Relaxed);
                                        if hint.odirect {
                                            // Hybrid I/O: O_DIRECT serves
                                            // from OUR tiers by directive
                                            // (kernel page cache stays
                                            // bypassed kernel-side).
                                            METRICS
                                                .read_odirect_tier_serves
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        // Read-lane coverage credit: this
                                        // consumption path retires the
                                        // hold's copy too (no-op when the
                                        // key is not held).
                                        if self.read_lane.enabled() {
                                            self.cache
                                                .read_lane_hold
                                                .credit(b_key, (end - start) as u64);
                                        }
                                        return Ok((data, None));
                                    }
                                    METRICS
                                        .stale_binding_rebinds
                                        .fetch_add(1, Ordering::Relaxed);
                                    debug!(
                                        "stale-binding rebind (hot-tier hit): file={} block={} key={}",
                                        file_path, start_block, b_key
                                    );
                                }
                            }
                            // Read-lane hold fast path (2026-08-01): the
                            // deep-qd cohort stability serve — a completed
                            // fill evicted from hot probation before its
                            // stragglers arrived is still here until its
                            // coverage completes. SAME proof obligation as
                            // the hot/tier hits: hold entries carry
                            // current-incarnation bytes (validated
                            // deposits + unified purge), binding currency
                            // validates the serve; coverage is credited
                            // only on a proven serve. Probed strictly
                            // AFTER the hot tier so warm hot entries keep
                            // funding the governor's payback basis.
                            if self.read_lane.enabled() {
                                if let Some(b_key) = b_key_opt.as_ref().filter(|_| !device_true) {
                                    if let Some((held, ledger_visible)) =
                                        self.cache.read_lane_hold.serve_with_provenance(b_key, 0)
                                    {
                                        read_serve_phase_record(
                                            ReadServePhase::ClassifyProbe,
                                            probe_t0,
                                        );
                                        let start = std::cmp::min(slice_start as usize, held.len());
                                        let end = std::cmp::min(
                                            (slice_start + slice_len as u64) as usize,
                                            held.len(),
                                        );
                                        let slice_t0 = std::time::Instant::now();
                                        let data = if let Some(dest) = dest_addr {
                                            let len = end - start;
                                            let dest_ptr = dest as *mut u8;
                                            // Copy ledger + NT lever
                                            // (see the hot arm).
                                            // SAFETY: as the hot arm.
                                            if unsafe {
                                                serve_copy_to_dest(
                                                    dest_ptr,
                                                    held[start..end].as_ptr(),
                                                    len,
                                                    hint.dest_arena,
                                                )
                                            } {
                                                METRICS
                                                    .nt_read_serve_bytes
                                                    .fetch_add(len as u64, Ordering::Relaxed);
                                            }
                                            METRICS
                                                .read_copy_dest_bytes
                                                .fetch_add(len as u64, Ordering::Relaxed);
                                            bytes::Bytes::from_owner(
                                                crate::cache::pool::UringBufOwner {
                                                    ptr: dest_ptr,
                                                    len,
                                                },
                                            )
                                        } else {
                                            held.slice(start..end)
                                        };
                                        read_serve_phase_record(ReadServePhase::SliceOut, slice_t0);
                                        let bind_t0 = std::time::Instant::now();
                                        let still_bound = self
                                            .current_block_binding(file_path, start_block)
                                            .await?
                                            .as_deref()
                                            == Some(b_key.as_str());
                                        read_serve_phase_record(
                                            ReadServePhase::BindingCheck,
                                            bind_t0,
                                        );
                                        if still_bound {
                                            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                                            METRICS
                                                .read_lane_serves
                                                .fetch_add(1, Ordering::Relaxed);
                                            METRICS
                                                .read_lane_serve_bytes
                                                .fetch_add((end - start) as u64, Ordering::Relaxed);
                                            if hint.odirect {
                                                METRICS
                                                    .read_odirect_tier_serves
                                                    .fetch_add(1, Ordering::Relaxed);
                                            }
                                            self.cache
                                                .read_lane_hold
                                                .credit(b_key, (end - start) as u64);
                                            // Ledger visibility (the
                                            // 2026-07-31 fix): a demand-
                                            // deposited entry served after
                                            // the hot/tier probes missed is
                                            // the pre-lane refetch in serve
                                            // form — run the R1b ceremony
                                            // with the same class the
                                            // refetch would have filled
                                            // under (one lanes probe, the
                                            // fill_class rule; only on the
                                            // visible arm — lane deposits
                                            // stay invisible).
                                            if ledger_visible {
                                                let class = if self
                                                    .stream_lanes
                                                    .get(file_path)
                                                    .is_some_and(|l| l.any_streaming_fresh())
                                                {
                                                    FillClass::DemandStream
                                                } else {
                                                    FillClass::Demand
                                                };
                                                self.hold_serve_admission(b_key, &held, class)
                                                    .await;
                                            }
                                            return Ok((data, None));
                                        }
                                        METRICS
                                            .stale_binding_rebinds
                                            .fetch_add(1, Ordering::Relaxed);
                                        debug!(
                                            "stale-binding rebind (read-lane hold hit): file={} block={} key={}",
                                            file_path, start_block, b_key
                                        );
                                    }
                                }
                            }
                            // Tier fast path with binding recheck (reused-key
                            // stale-fill family): a zero-copy NVMe read-cache hit
                            // is copied out first — the mmap shard guard never
                            // lives across an await — then served only if the
                            // CURRENT map still binds this block to the key (tier
                            // entries always hold their key's current-incarnation
                            // bytes, so binding currency alone validates the
                            // serve). On movement the validated loop below
                            // re-resolves and overwrites the dest.
                            if let Some(b_key) = b_key_opt.as_ref().filter(|_| !device_true) {
                                if let Some(guard) =
                                    self.cache.nvme.get_cached_read_block_range_zero_copy(
                                        b_key,
                                        slice_start,
                                        slice_len,
                                    )
                                {
                                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
                                    read_serve_phase_record(
                                        ReadServePhase::ClassifyProbe,
                                        probe_t0,
                                    );
                                    let len = guard.len();
                                    let slice_t0 = std::time::Instant::now();
                                    let data = if let Some(dest) = dest_addr {
                                        let dest_ptr = dest as *mut u8;
                                        // Copy ledger + NT lever (see
                                        // the hot arm; source here is
                                        // the tier mmap under guard).
                                        // SAFETY: as the hot arm.
                                        unsafe {
                                            if serve_copy_to_dest(
                                                dest_ptr,
                                                guard.as_ptr(),
                                                len,
                                                hint.dest_arena,
                                            ) {
                                                METRICS
                                                    .nt_read_serve_bytes
                                                    .fetch_add(len as u64, Ordering::Relaxed);
                                            }
                                            METRICS
                                                .read_copy_dest_bytes
                                                .fetch_add(len as u64, Ordering::Relaxed);
                                            bytes::Bytes::from_owner(
                                                crate::cache::pool::UringBufOwner {
                                                    ptr: dest_ptr,
                                                    len,
                                                },
                                            )
                                        }
                                    } else {
                                        // Copy ledger: the mmap guard
                                        // cannot live across an await —
                                        // this bounce is structural for
                                        // None-dest tier serves.
                                        METRICS
                                            .read_copy_bounce_bytes
                                            .fetch_add(len as u64, Ordering::Relaxed);
                                        bytes::Bytes::copy_from_slice(&guard)
                                    };
                                    drop(guard);
                                    read_serve_phase_record(ReadServePhase::SliceOut, slice_t0);
                                    let bind_t0 = std::time::Instant::now();
                                    let still_bound = self
                                        .current_block_binding(file_path, start_block)
                                        .await?
                                        .as_deref()
                                        == Some(b_key.as_str());
                                    read_serve_phase_record(ReadServePhase::BindingCheck, bind_t0);
                                    if still_bound {
                                        if hint.odirect {
                                            METRICS
                                                .read_odirect_tier_serves
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        // Read-lane coverage credit (see
                                        // the hot fast path).
                                        if self.read_lane.enabled() {
                                            self.cache.read_lane_hold.credit(b_key, len as u64);
                                        }
                                        return Ok((data, None));
                                    }
                                    METRICS
                                        .stale_binding_rebinds
                                        .fetch_add(1, Ordering::Relaxed);
                                    debug!(
                                    "stale-binding rebind (single-block tier hit): file={} block={} key={}",
                                    file_path, start_block, b_key
                                );
                                }
                            }

                            // R3 ranged dispatch (§5.6) — strictly after the
                            // overlay/hot/tier probes missed: passthrough,
                            // small, non-streaming requests fetch only their
                            // 4 KiB-aligned window. First touches are never
                            // published nor single-flighted; a second touch
                            // within the ghost window escalates inside the
                            // primitive (hybrid I/O). Hole ⇒ zeros, same as
                            // the whole-block arm below.
                            //
                            // Device-true escape: EVERY request size takes
                            // this window path on passthrough volumes (the
                            // eligibility policy is for the default mode) —
                            // exactly the requested bytes, device-true,
                            // publish-free; the primitive skips the ghost.
                            if b_key_opt.is_some()
                                && ((device_true && self.get_crypto().is_passthrough())
                                    || (!device_true
                                        && self.ranged_eligible(file_path, slice_len as u64)))
                            {
                                let rel = slice_start..slice_start + slice_len as u64;
                                let aligned = slice_start % 4096 == 0
                                    && (slice_len as u64) % 4096 == 0
                                    && rel.end <= block_size;
                                let rdest = match dest_addr {
                                    // Offer the dest only on the zero-copy
                                    // leg (window == request) AND when the
                                    // dest pointer itself is 4 KiB-aligned
                                    // (O_DIRECT DMA contract): the kernel
                                    // payload arena is aligned by
                                    // construction, but arena-dest il
                                    // serves (E-IL2) hand arbitrary window
                                    // pointers — unaligned dests take the
                                    // bounce leg.
                                    Some(d) if aligned && d % 4096 == 0 => Some(RangedDest {
                                        ptr: d as *mut u8,
                                        cap: slice_len as usize,
                                    }),
                                    _ => None,
                                };
                                read_serve_phase_record(ReadServePhase::ClassifyProbe, probe_t0);
                                let served = self
                                    .get_block_range_for_index(
                                        file_path,
                                        start_block,
                                        rel,
                                        b_key_opt.as_deref(),
                                        rdest,
                                        hint,
                                    )
                                    .await?;
                                match served {
                                    Some(val) => {
                                        let data = match dest_addr {
                                            Some(dest) if aligned => {
                                                // Value already backs the dest
                                                // region (zero-copy leg).
                                                let _ = dest;
                                                match val {
                                                    crate::cache::pool::ReadBlockValue::Bytes(
                                                        b,
                                                    ) => b,
                                                    other => bytes::Bytes::copy_from_slice(&other),
                                                }
                                            }
                                            Some(dest) => {
                                                // Bounce with a payload dest:
                                                // copy the request in; full
                                                // coverage (val.len() ==
                                                // slice_len), nothing to zero.
                                                let len = val.len();
                                                let dest_ptr = dest as *mut u8;
                                                unsafe {
                                                    std::ptr::copy_nonoverlapping(
                                                        val.as_ptr(),
                                                        dest_ptr,
                                                        len,
                                                    );
                                                    if len < slice_len as usize {
                                                        std::ptr::write_bytes(
                                                            dest_ptr.add(len),
                                                            0,
                                                            slice_len as usize - len,
                                                        );
                                                    }
                                                }
                                                // Copy ledger: serve copy
                                                // into the dest.
                                                METRICS
                                                    .read_copy_dest_bytes
                                                    .fetch_add(len as u64, Ordering::Relaxed);
                                                bytes::Bytes::from_owner(
                                                    crate::cache::pool::UringBufOwner {
                                                        ptr: dest_ptr,
                                                        len: slice_len as usize,
                                                    },
                                                )
                                            }
                                            None => match val {
                                                crate::cache::pool::ReadBlockValue::Bytes(b) => b,
                                                other => {
                                                    // Copy ledger: pooled
                                                    // value bounce (rare —
                                                    // ranged values are
                                                    // Bytes-backed).
                                                    METRICS.read_copy_bounce_bytes.fetch_add(
                                                        other.len() as u64,
                                                        Ordering::Relaxed,
                                                    );
                                                    bytes::Bytes::copy_from_slice(&other)
                                                }
                                            },
                                        };
                                        return Ok((data, None));
                                    }
                                    None => {
                                        // Hole in the current map: zeros.
                                        let len = slice_len as usize;
                                        let data = if let Some(dest) = dest_addr {
                                            let dest_ptr = dest as *mut u8;
                                            unsafe {
                                                std::ptr::write_bytes(dest_ptr, 0, len);
                                                bytes::Bytes::from_owner(
                                                    crate::cache::pool::UringBufOwner {
                                                        ptr: dest_ptr,
                                                        len,
                                                    },
                                                )
                                            }
                                        } else {
                                            bytes::Bytes::from(vec![0u8; len])
                                        };
                                        return Ok((data, None));
                                    }
                                }
                            }

                            // Validated resolve: raw full-block DMA into the uring
                            // payload dest when possible (revalidated afterwards),
                            // else the binding-validated fetch loop. `None` = the
                            // block is a hole in the CURRENT map.
                            // (read_serve_phase_ns: every probe missed —
                            // the cold leg; `block_fetch` spans each fetch
                            // await below, `slice_out` the reply copies.)
                            read_serve_phase_record(ReadServePhase::ClassifyProbe, probe_t0);
                            let downloaded: Option<crate::cache::pool::ReadBlockValue> =
                                match b_key_opt {
                                    Some(b_key) => {
                                        let fetch_t0 = std::time::Instant::now();
                                        let mut resolved = None;
                                        if let Some(dest) = dest_addr {
                                            // §5.6 sibling-leg hygiene: this raw
                                            // leg DMAs DEVICE bytes into the
                                            // payload dest without process_read
                                            // — on a transform (compressed/
                                            // encrypted) volume those are
                                            // ciphertext/frame bytes, and its
                                            // revalidation proves identity, not
                                            // transform correctness. Transform
                                            // configs fall through to the
                                            // validated whole-block loop, which
                                            // decodes.
                                            if slice_start == 0
                                                && slice_len as u64 == block_size
                                                && self.get_crypto().is_passthrough()
                                                // O_DIRECT DMA contract: the
                                                // kernel payload arena is
                                                // 4 KiB-aligned by
                                                // construction; arena-dest
                                                // il serves (E-IL2) must
                                                // prove it (unaligned dests
                                                // ride the validated loop's
                                                // memcpy slice instead).
                                                && dest % 4096 == 0
                                            {
                                                // Zero-copy device→payload DMA. The raw read
                                                // bypasses the single-flight fill, so it
                                                // carries the fill discipline itself:
                                                // incarnation snapshot before, still-check
                                                // after, then the binding recheck. On any
                                                // movement the validated loop below
                                                // overwrites the dest.
                                                let tracked = self
                                                    .backend_router
                                                    .key_incarnation_tracked(b_key);
                                                let before =
                                                    self.backend_router.fill_incarnation(b_key);
                                                self.backend_router
                                                    .read_block_with_dest(
                                                        b_key,
                                                        block_size as usize,
                                                        Some(dest),
                                                    )
                                                    .await?;
                                                // Copy ledger: device DMA
                                                // straight into the dest —
                                                // zero daemon copies.
                                                METRICS
                                                    .read_dest_dma_bytes
                                                    .fetch_add(block_size, Ordering::Relaxed);
                                                let incarnation_ok = !tracked
                                                    || before.is_some_and(|bf| {
                                                        self.backend_router
                                                            .fill_incarnation_still(b_key, bf)
                                                    });
                                                if incarnation_ok
                                                    && self
                                                        .current_block_binding(
                                                            file_path,
                                                            start_block,
                                                        )
                                                        .await?
                                                        .as_deref()
                                                        == Some(b_key.as_str())
                                                {
                                                    let len = block_size as usize;
                                                    let dest_ptr = dest as *mut u8;
                                                    let b = bytes::Bytes::from_owner(
                                                        crate::cache::pool::UringBufOwner {
                                                            ptr: dest_ptr,
                                                            len,
                                                        },
                                                    );
                                                    resolved = Some(
                                                        crate::cache::pool::ReadBlockValue::Bytes(
                                                            b,
                                                        ),
                                                    );
                                                } else {
                                                    METRICS
                                                        .stale_binding_rebinds
                                                        .fetch_add(1, Ordering::Relaxed);
                                                    debug!(
                                                    "stale-binding rebind (raw dest read): file={} block={} key={}",
                                                    file_path, start_block, b_key
                                                );
                                                }
                                            }
                                        }
                                        match resolved {
                                            Some(r) => {
                                                read_serve_phase_record(
                                                    ReadServePhase::BlockFetch,
                                                    fetch_t0,
                                                );
                                                Some(r)
                                            }
                                            None => {
                                                // Device-true on a transform
                                                // volume reaches here (decode
                                                // requires the whole block):
                                                // the fetch stays cache-free
                                                // and publish-free.
                                                let val = self
                                                    .get_block_for_index(
                                                        file_path,
                                                        start_block,
                                                        Some(b_key),
                                                        device_true,
                                                        true,
                                                    )
                                                    .await?;
                                                read_serve_phase_record(
                                                    ReadServePhase::BlockFetch,
                                                    fetch_t0,
                                                );
                                                let slice_t0 = std::time::Instant::now();
                                                match (val, dest_addr) {
                                                    (Some(val), Some(dest)) => {
                                                        let start = std::cmp::min(
                                                            slice_start as usize,
                                                            val.len(),
                                                        );
                                                        let end = std::cmp::min(
                                                            (slice_start + slice_len as u64)
                                                                as usize,
                                                            val.len(),
                                                        );
                                                        let len = end - start;
                                                        let dest_ptr = dest as *mut u8;
                                                        unsafe {
                                                            // Copy ledger +
                                                            // NT lever (see
                                                            // the hot arm) —
                                                            // the EXA cold
                                                            // slice_out.
                                                            if serve_copy_to_dest(
                                                                dest_ptr,
                                                                val[start..end].as_ptr(),
                                                                len,
                                                                hint.dest_arena,
                                                            ) {
                                                                METRICS
                                                                    .nt_read_serve_bytes
                                                                    .fetch_add(
                                                                        len as u64,
                                                                        Ordering::Relaxed,
                                                                    );
                                                            }
                                                            METRICS.read_copy_dest_bytes.fetch_add(
                                                                len as u64,
                                                                Ordering::Relaxed,
                                                            );
                                                            // Unwritten remainder of the reused
                                                            // uring dest region must never replay
                                                            // a previous reply's bytes.
                                                            if len < slice_len as usize {
                                                                std::ptr::write_bytes(
                                                                    dest_ptr.add(len),
                                                                    0,
                                                                    slice_len as usize - len,
                                                                );
                                                            }
                                                        }
                                                        read_serve_phase_record(
                                                            ReadServePhase::SliceOut,
                                                            slice_t0,
                                                        );
                                                        let b = bytes::Bytes::from_owner(
                                                            crate::cache::pool::UringBufOwner {
                                                                ptr: dest_ptr,
                                                                len,
                                                            },
                                                        );
                                                        Some(crate::cache::pool::ReadBlockValue::Bytes(b))
                                                    }
                                                    (Some(val), None) => Some(val),
                                                    (None, _) => None,
                                                }
                                            }
                                        }
                                    }
                                    // Stale-absent binding (generic/795):
                                    // the entry-time map snapshot may miss
                                    // a binding a concurrent write-through/
                                    // writeback published mid-read — after
                                    // it retired the overlay/sibling this
                                    // read already probed. The fetch loop
                                    // re-resolves freshest-first; only a
                                    // fresh-map absence is a hole.
                                    None => {
                                        let fetch_t0 = std::time::Instant::now();
                                        let val = self
                                            .get_block_for_index(
                                                file_path,
                                                start_block,
                                                None,
                                                device_true,
                                                true,
                                            )
                                            .await?;
                                        read_serve_phase_record(
                                            ReadServePhase::BlockFetch,
                                            fetch_t0,
                                        );
                                        let slice_t0 = std::time::Instant::now();
                                        match (val, dest_addr) {
                                            (Some(val), Some(dest)) => {
                                                let start =
                                                    std::cmp::min(slice_start as usize, val.len());
                                                let end = std::cmp::min(
                                                    (slice_start + slice_len as u64) as usize,
                                                    val.len(),
                                                );
                                                let len = end - start;
                                                let dest_ptr = dest as *mut u8;
                                                unsafe {
                                                    // Copy ledger + NT lever
                                                    // (see the hot arm).
                                                    if serve_copy_to_dest(
                                                        dest_ptr,
                                                        val[start..end].as_ptr(),
                                                        len,
                                                        hint.dest_arena,
                                                    ) {
                                                        METRICS.nt_read_serve_bytes.fetch_add(
                                                            len as u64,
                                                            Ordering::Relaxed,
                                                        );
                                                    }
                                                    METRICS
                                                        .read_copy_dest_bytes
                                                        .fetch_add(len as u64, Ordering::Relaxed);
                                                    if len < slice_len as usize {
                                                        std::ptr::write_bytes(
                                                            dest_ptr.add(len),
                                                            0,
                                                            slice_len as usize - len,
                                                        );
                                                    }
                                                }
                                                read_serve_phase_record(
                                                    ReadServePhase::SliceOut,
                                                    slice_t0,
                                                );
                                                let b = bytes::Bytes::from_owner(
                                                    crate::cache::pool::UringBufOwner {
                                                        ptr: dest_ptr,
                                                        len,
                                                    },
                                                );
                                                Some(crate::cache::pool::ReadBlockValue::Bytes(b))
                                            }
                                            (Some(val), None) => Some(val),
                                            (None, _) => None,
                                        }
                                    }
                                };

                            match downloaded {
                                Some(downloaded) => {
                                    // Read-lane coverage credit
                                    // (2026-08-01): the primary's own
                                    // slice is consumption too — without
                                    // it a streamed block would sit one
                                    // sub-read short of retirement
                                    // forever (no-op when not held).
                                    if !device_true && self.read_lane.enabled() {
                                        if let Some(bk) = b_key_opt.as_deref() {
                                            let served = std::cmp::min(
                                                slice_len as u64,
                                                (downloaded.len() as u64)
                                                    .saturating_sub(slice_start),
                                            );
                                            self.cache.read_lane_hold.credit(bk, served);
                                        }
                                    }
                                    if let Some(dest) = dest_addr {
                                        let len = (end_offset - offset) as usize;
                                        let data = bytes::Bytes::from_owner(
                                            crate::cache::pool::UringBufOwner {
                                                ptr: dest as *mut u8,
                                                len,
                                            },
                                        );
                                        return Ok((data, Some(std::sync::Arc::new(downloaded))));
                                    } else {
                                        let slice_t0 = std::time::Instant::now();
                                        let start =
                                            std::cmp::min(slice_start as usize, downloaded.len());
                                        let end = std::cmp::min(
                                            (slice_start + slice_len as u64) as usize,
                                            downloaded.len(),
                                        );
                                        // E-IL1 (read-copy-count 2026-08-02):
                                        // `Bytes`-backed fills serve a
                                        // REFCOUNT slice — the former
                                        // `copy_from_slice` here was a
                                        // 1 MiB-class alloc + full CPU
                                        // pass per cold il read (the None-
                                        // dest transport shape). Retention
                                        // is unchanged: the backing was
                                        // already returned alongside.
                                        let data = match &downloaded {
                                            crate::cache::pool::ReadBlockValue::Bytes(b) => {
                                                b.slice(start..end)
                                            }
                                            other => {
                                                // Copy ledger: pooled-repr
                                                // bounce (PooledBuf cannot
                                                // share refcounts).
                                                METRICS.read_copy_bounce_bytes.fetch_add(
                                                    (end - start) as u64,
                                                    Ordering::Relaxed,
                                                );
                                                bytes::Bytes::copy_from_slice(&other[start..end])
                                            }
                                        };
                                        read_serve_phase_record(ReadServePhase::SliceOut, slice_t0);
                                        return Ok((data, Some(std::sync::Arc::new(downloaded))));
                                    }
                                }
                                None => {
                                    // Hole (initial resolution or rebound to a
                                    // punched/truncated index): zero-filled slice.
                                    let len = slice_len as usize;
                                    let data = if let Some(dest) = dest_addr {
                                        let dest_ptr = dest as *mut u8;
                                        unsafe {
                                            std::ptr::write_bytes(dest_ptr, 0, len);
                                            bytes::Bytes::from_owner(
                                                crate::cache::pool::UringBufOwner {
                                                    ptr: dest_ptr,
                                                    len,
                                                },
                                            )
                                        }
                                    } else {
                                        let mut hole_pooled = BUFFER_POOL.alloc();
                                        hole_pooled.resize(len, 0);
                                        bytes::Bytes::copy_from_slice(&hole_pooled)
                                    };
                                    return Ok((data, None));
                                }
                            }
                        }
                    }

                    // 2. Multi-block or cache miss: load and assemble using pooled buffer
                    // (read_serve_phase_ns: the common phases record here
                    // too; per-block `block_fetch` spans record in the
                    // parallel tasks below — the single-block leg above is
                    // the fully-tiled decomposition venue.)
                    let key_t0 = std::time::Instant::now();
                    let block_keys = self
                        .load_striped_block_keys(file_path, &meta, start_block, end_block)
                        .await?;
                    read_serve_phase_record(ReadServePhase::KeyResolve, key_t0);
                    // Hybrid-I/O diagnostic escape: same contract as the
                    // single-block arm (no classifier/pipeline, no tier
                    // serves, no admission — validated device reads only).
                    let device_true = self.direct_device_true() && hint.odirect;
                    if device_true {
                        METRICS
                            .read_device_true_reads
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    // §5.5: multi-block reads advance the pipeline past
                    // end_block (consume span + top-up). The consume-time
                    // detector probes the first block's key like the
                    // single-block arm. Ring-originated requests fed the
                    // lanes at the sink (`lane_pre_fed`, §5.3).
                    if !device_true && !hint.lane_pre_fed {
                        self.pipeline_touch(
                            file_path,
                            &meta,
                            block_size,
                            offset,
                            (end_offset - offset).max(1),
                            start_block,
                            end_block,
                            false,
                            block_keys.first().and_then(|(_, k)| k.as_deref()),
                            true,
                        );
                    }

                    let final_len = (end_offset - offset) as usize;
                    let (raw_ptr, final_buf_opt) = if let Some(dest) = dest_addr {
                        (dest as usize, None)
                    } else {
                        let mut final_buf = BUFFER_POOL.alloc();
                        final_buf.resize(final_len, 0);
                        let ptr = final_buf.as_mut_ptr() as usize;
                        (ptr, Some(final_buf))
                    };

                    // Spawn concurrent tasks to download block data in parallel.
                    // Acquire the admission permit *inside* each task so the coordinator
                    // never holds N permits while spawning (can deadlock the semaphore
                    // when block_count > permit pool under nested multi-block reads).
                    let mut futures = Vec::new();
                    // Copy ledger classification for the per-block assembly
                    // copies below: writes land either in the zero-copy
                    // dest (dest_addr) or the pooled final_buf (bounce).
                    let assembly_into_dest = final_buf_opt.is_none();
                    for (b_idx, b_key_opt) in block_keys {
                        let router = self.clone();
                        let b_start_offset = b_idx as u64 * block_size;
                        let b_end_offset = b_start_offset + block_size;
                        let slice_start = std::cmp::max(offset, b_start_offset);
                        let slice_end = std::cmp::min(end_offset, b_end_offset);
                        let dest_start = (slice_start - offset) as usize;
                        let copy_len = (slice_end - slice_start) as usize;

                        let rel_start = (slice_start - b_start_offset) as usize;
                        let file_path_clone = file_path.to_string();
                        let sem = crate::bg_admit::STRIPED_IO_SEM.clone();

                        futures.push(tokio::spawn(async move {
                            let _permit = sem.acquire_owned().await.map_err(|_| {
                                SqueezefsError::InvalidOperation(
                                    "striped read admission closed".to_string(),
                                )
                            })?;
                            // Every byte of this block's dest region [dest_start,
                            // dest_start + copy_len) MUST be written: with a uring
                            // payload dest (`dest_addr`), the buffer is REUSED
                            // across requests, so a region left unwritten — a hole
                            // block, or the tail past a short tier copy — replays
                            // the previous reply's bytes to the kernel (transient
                            // stale-read corruption; the on-disk file is fine).
                            // Each arm reports how many bytes it wrote; the
                            // remainder is zeroed (holes read zeros).
                            let cache_key =
                                crate::keys::active_block_for_path(&file_path_clone, b_idx)
                                    .to_string();
                            let written: usize = if let Some(active_data) =
                                router.cache.nvme.read_staged(&cache_key)
                            {
                                let start = std::cmp::min(rel_start, active_data.len());
                                let end = std::cmp::min(rel_start + copy_len, active_data.len());
                                let actual_copy = end - start;
                                if actual_copy > 0 {
                                    unsafe {
                                        let dest = (raw_ptr + dest_start) as *mut u8;
                                        std::ptr::copy_nonoverlapping(
                                            active_data[start..end].as_ptr(),
                                            dest,
                                            actual_copy,
                                        );
                                    }
                                }
                                actual_copy
                            } else if b_key_opt.is_some()
                                && ((device_true && router.get_crypto().is_passthrough())
                                    || (!device_true
                                        && router
                                            .ranged_eligible(&file_path_clone, copy_len as u64)))
                            {
                                // R3 (§5.6), multi-block per-block leg: this
                                // block's slice is small/passthrough/non-
                                // streaming — fetch only its window (bounce
                                // shape: the assembled dest region is not
                                // guaranteed 4 KiB-aligned per block). Same
                                // fill discipline inside the primitive;
                                // Ok(None) = hole ⇒ the zero-fill below.
                                // Device-true escape: every per-block slice
                                // takes the window path on passthrough
                                // volumes (single-block arm contract).
                                match router
                                    .get_block_range_for_index(
                                        &file_path_clone,
                                        b_idx,
                                        rel_start as u64..(rel_start + copy_len) as u64,
                                        b_key_opt.as_deref(),
                                        None,
                                        hint,
                                    )
                                    .await?
                                {
                                    Some(ranged) => {
                                        let actual_copy = std::cmp::min(ranged.len(), copy_len);
                                        if actual_copy > 0 {
                                            unsafe {
                                                let dest = (raw_ptr + dest_start) as *mut u8;
                                                std::ptr::copy_nonoverlapping(
                                                    ranged.as_ptr(),
                                                    dest,
                                                    actual_copy,
                                                );
                                            }
                                        }
                                        actual_copy
                                    }
                                    None => 0,
                                }
                            } else if let Some(downloaded) = {
                                let fetch_t0 = std::time::Instant::now();
                                let val = router
                                    .get_block_for_index(
                                        &file_path_clone,
                                        b_idx,
                                        b_key_opt.as_deref(),
                                        device_true,
                                        true,
                                    )
                                    .await?;
                                read_serve_phase_record(ReadServePhase::BlockFetch, fetch_t0);
                                val
                            } {
                                // Binding-validated serve (reused-key stale-fill
                                // family): the RAM-LRU fast path lives inside the
                                // primitive; a stale b→key resolution re-resolves
                                // instead of copying another block's bytes into
                                // this block's dest region.
                                let start = std::cmp::min(rel_start, downloaded.len());
                                let end = std::cmp::min(rel_start + copy_len, downloaded.len());
                                let actual_copy = end - start;
                                if actual_copy > 0 {
                                    unsafe {
                                        let dest = (raw_ptr + dest_start) as *mut u8;
                                        std::ptr::copy_nonoverlapping(
                                            downloaded[start..end].as_ptr(),
                                            dest,
                                            actual_copy,
                                        );
                                    }
                                }
                                actual_copy
                            } else {
                                // Hole block (initial resolution or rebound to a
                                // punched/truncated index): zero the whole region.
                                0
                            };
                            // Copy ledger: one serve copy per assembled
                            // block slice, classified by destination.
                            if written > 0 {
                                let ctr = if assembly_into_dest {
                                    &METRICS.read_copy_dest_bytes
                                } else {
                                    &METRICS.read_copy_bounce_bytes
                                };
                                ctr.fetch_add(written as u64, Ordering::Relaxed);
                            }
                            if written < copy_len {
                                unsafe {
                                    std::ptr::write_bytes(
                                        (raw_ptr + dest_start + written) as *mut u8,
                                        0,
                                        copy_len - written,
                                    );
                                }
                            }
                            // W2 (§5.2): overlay the block's staged extent
                            // record — its runs are NEWER than any base
                            // (device / tier / staged image / hole zeros).
                            for (s, d) in router.staged_extent_runs_in(
                                &file_path_clone,
                                b_idx,
                                rel_start,
                                rel_start + copy_len,
                            ) {
                                unsafe {
                                    let dst = (raw_ptr + dest_start + (s - rel_start)) as *mut u8;
                                    std::ptr::copy_nonoverlapping(d.as_ptr(), dst, d.len());
                                }
                            }
                            Ok::<(), SqueezefsError>(())
                        }));
                    }

                    let results = futures::future::try_join_all(futures).await.map_err(|e| {
                        SqueezefsError::Io(std::io::Error::other(format!(
                            "Parallel block download task panicked: {:?}",
                            e
                        )))
                    })?;

                    for res in results {
                        res?;
                    }

                    if let Some(final_buf) = final_buf_opt {
                        // Copy ledger: the assembled pooled buffer is
                        // handed out as a zero-copy `Bytes` view (E-IL1's
                        // multi-block sibling — the former full-length
                        // `copy_from_slice` bounce is gone).
                        debug_assert_eq!(final_buf.len(), final_len);
                        let data = final_buf.into_bytes();
                        return Ok((data, None));
                    } else {
                        let data = bytes::Bytes::from_owner(crate::cache::pool::UringBufOwner {
                            ptr: dest_addr.unwrap() as *mut u8,
                            len: final_len,
                        });
                        return Ok((data, None));
                    }
                }
                _ => {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "Unknown file type: {}",
                        meta.file_type
                    )))
                }
            }
        }
    }

    /// W2 rider FOLD (fsync / teardown / recovery drains): compose the
    /// record's extents into the staged image DIRECTLY under the block-0
    /// guard — seed from the ring blob (or the validated promoted copy),
    /// apply the extents, same-key re-stage, retire the record. One
    /// whole-image RMW per k rider writes — the rider's amortization form.
    /// Never-lossy: a refused/failed re-stage leaves image + record
    /// untouched and propagates (fsync is the error surface). Ok(true) =
    /// a record existed and was drained.
    pub(crate) async fn fold_rider_record(&self, ino: u64, fencing_token: u64) -> Result<bool> {
        let key = crate::keys::active_block_ext(ino, 0).to_string();
        if !self.cache.nvme.has_staged_extent_record(&key) {
            return Ok(false);
        }
        let file_path = crate::keys::inode_path(ino);
        let _guard = crate::fuse_client::block_lock_acquire(
            ino,
            0,
            crate::fuse_client::BlockLockSite::StagedWrite,
        )
        .await;
        // Re-probe + resolve the identity UNDER the guard.
        let rec = match self.cache.nvme.read_extent_record(&key) {
            Some(Ok(rec)) => rec,
            Some(Err(_)) => return Ok(false), // recovery owns the disposition
            None => return Ok(false),         // drained by a racing write's fold-first
        };
        let meta = self.fetch_metadata(&file_path).await?;
        if meta.file_type == "striped" {
            // The striped fold owns block-0 records of striped layouts.
            return Ok(false);
        }
        if rec.extents.is_empty() || meta.file_type != "staged" {
            // Degenerate record, or a non-staged layout (inline flip):
            // every layout commit retires its record AFTER publishing an
            // image that already contains the extents (fold-first), so a
            // surviving record here is a crash-residue stale duplicate —
            // retire it.
            self.retire_rider_record(ino).await;
            return Ok(true);
        }
        let Some(fid) = meta.file_id.clone() else {
            return Ok(false);
        };
        // Base image: the ring blob, else the validated promoted copy.
        let mut img: Vec<u8> = {
            let mut buf = BUFFER_POOL.alloc();
            if self.cache.nvme.read_staged_into(&fid, &mut buf) {
                buf.to_vec()
            } else if let Some(bk) = self.staged_block_mapping(&file_path, &meta).await {
                let fetched = self.read_promoted_staged_block(&bk).await?;
                let fresh = self.freshest_layout_identity(&file_path).await;
                let still_bound = fresh.as_ref().is_some_and(|f| {
                    f.file_type == "staged"
                        && f.file_id == meta.file_id
                        && f.block_map
                            .as_ref()
                            .and_then(|bm| bm.get(&0))
                            .is_some_and(|cur| *cur == bk)
                });
                if !still_bound {
                    // Identity moving under the fold: leave everything in
                    // place — the next drain re-resolves.
                    return Ok(false);
                }
                let mut v = fetched.to_vec();
                if v.len() as u64 > meta.size {
                    v.truncate(meta.size as usize);
                }
                v
            } else {
                // Payload lost (D0 crash degrade): the record extents are
                // the only surviving custody — fold them over zeros.
                vec![0u8; meta.size.min(u32::MAX as u64) as usize]
            }
        };
        let max_end = rec
            .extents
            .iter()
            .map(|(s0, d)| *s0 as usize + d.len())
            .max()
            .unwrap_or(0);
        if img.len() < max_end {
            img.resize(max_end, 0);
        }
        for (s0, d) in &rec.extents {
            img[*s0 as usize..*s0 as usize + d.len()].copy_from_slice(d);
        }
        // Same-key crash-safe replace (bumps the stage generation — a
        // racing promotion's commit aborts). A refused replace (the ring
        // cannot place the crash-safe second copy, or the budget is
        // oversubscribed) takes the FIND-RW5-A durable-spill escalation:
        // the composed image goes straight to a backend block and the
        // layout commits, exactly like the staged-replace arm — the fold
        // must NEVER surface StorageFull to its driver (fsync/writeback/
        // recovery), and custody transfers only after the durable commit.
        match self
            .cache
            .nvme
            .stage_write(
                &file_path,
                &fid,
                bytes::Bytes::from(img.clone()),
                fencing_token,
            )
            .await
        {
            Ok(_) => {}
            Err(SqueezefsError::Io(ref e)) if e.kind() == std::io::ErrorKind::StorageFull => {
                crate::fuse_client::METRICS
                    .staged_spill_escalations
                    .fetch_add(1, Ordering::Relaxed);
                let img_len = img.len() as u64;
                let processed = self
                    .get_crypto()
                    .process_write_async(bytes::Bytes::from(img))
                    .await?;
                let (be_id, block_allocator, nvme_writer) =
                    self.backend_router.get_active_backend()?;
                crate::block_allocator::ensure_stored_block_image_fits(
                    processed.len(),
                    block_allocator.chunk_size(),
                    "rider-fold spill",
                )?;
                let be_offset = block_allocator.allocate_block().await?;
                // PR VL6a: in-flight until the layout commit below.
                let _inflight = block_allocator.inflight_register(be_offset);
                let stored_block_key = format!(
                    "{}:0:{}",
                    self.backend_router.persist_block_key(&be_id, be_offset),
                    processed.len()
                );
                if let Err(e) = nvme_writer.write_block(be_offset, processed).await {
                    let _ = block_allocator.free_block(be_offset).await;
                    return Err(e);
                }
                block_allocator.publish_block(be_offset);

                // Spill takes a FRESH file_id (the spill identity
                // discipline): the stale ring entry must never shadow this
                // durable image, and an in-flight promotion of the old id
                // must fail its generation check.
                let spill_file_id = Uuid::new_v4().to_string();
                let _meta_guard = meta_lock_acquire(ino).await;
                let fresh = self.metadata_cache.get(&ino);
                let still_ours = fresh
                    .as_ref()
                    .map(|f| f.file_type == "staged" && f.file_id.as_deref() == Some(&fid))
                    .unwrap_or(false);
                if !still_ours {
                    // Identity moved under the fold (promotion/re-stage
                    // committed meanwhile): that commit folded-first, so
                    // the record is stale-duplicate custody — free our
                    // orphan upload and let the next drain re-resolve.
                    let _ = block_allocator.free_block(be_offset).await;
                    return Ok(false);
                }
                let mut block_map = std::collections::HashMap::new();
                block_map.insert(0, stored_block_key.clone());
                let mut updated_meta = meta.clone();
                updated_meta.file_type = "staged".into();
                updated_meta.size = updated_meta.size.max(img_len);
                updated_meta.file_id = Some(spill_file_id.into());
                updated_meta.data_key = None;
                updated_meta.block_map = Some(std::sync::Arc::new(block_map));
                updated_meta.layout_dirty = false;
                updated_meta.cached_at = std::time::Instant::now();
                // FIND-M11-A: the merge presents the ino's CURRENT
                // generation, read at the last responsible moment.
                let merge_token = self.dlm.get_fencing_token_ino(ino);
                self.save_metadata_to_backend(ino, &updated_meta, merge_token)
                    .await?;
                self.metadata_cache.insert(ino, updated_meta);
                self.cache.write_lru.remove(&file_path);
                self.cache.read_lru.remove(&file_path);
                // Release the superseded ring entry + any older durable copy.
                self.release_superseded_staged(
                    Some(&fid),
                    fresh.as_ref().and_then(|f| f.block_map.as_deref()),
                    Some(&stored_block_key),
                )
                .await;
            }
            Err(e) => return Err(e),
        }
        self.retire_rider_record(ino).await;
        Ok(true)
    }

    /// Clip the ino's rider record to `[0, new_size)`: drop extents fully
    /// beyond, truncate the straddler, re-put (same-key crash-safe
    /// replace) or remove when empty. Blocking-pool hop (shard WRITE
    /// lock).
    async fn clip_rider_record(&self, ino: u64, new_size: u64) {
        let key = crate::keys::active_block_ext(ino, 0).to_string();
        if !self.cache.nvme.has_staged_extent_record(&key) {
            return;
        }
        // Record mutations serialize behind the truncate's held inode
        // WRITE guard (rider writers hold the inode read guard);
        // promotions defer on record-bearing files.
        let Some(Ok(rec)) = self.cache.nvme.read_extent_record(&key) else {
            return; // torn/future: recovery owns the disposition
        };
        let mut clipped: Vec<(u32, Vec<u8>)> = Vec::with_capacity(rec.extents.len());
        for (s0, mut d) in rec.extents {
            if (s0 as u64) >= new_size {
                continue;
            }
            let keep = (new_size - s0 as u64).min(d.len() as u64) as usize;
            d.truncate(keep);
            if !d.is_empty() {
                clipped.push((s0, d));
            }
        }
        let nvme = self.cache.nvme.clone();
        let record = crate::cache::nvme::ExtentRecord {
            version: crate::cache::nvme::EXTENT_RECORD_VERSION,
            fencing_token: rec.fencing_token,
            block_idx: 0,
            base_deferred: rec.base_deferred,
            extents: clipped,
        };
        let _ = tokio::task::spawn_blocking(move || {
            if record.extents.is_empty() {
                nvme.remove_active_block(&key);
            } else if !nvme.rewrite_extent_record_in_place(&key, &record) {
                // The in-place patch failed (entry vanished / foreign
                // shape): the record must NOT survive the truncate with
                // beyond-EOF extents — resurrection bait (the
                // staged-truncate-stale family). Discard: the sub-size
                // extents it carried are un-fsynced custody the truncate
                // barrier legally supersedes, and the base image (already
                // clipped in place) stays authoritative.
                nvme.remove_active_block(&key);
            }
        })
        .await;
    }

    /// Retire the ino's rider record after a whole-image commit that
    /// folded it (`fold_rider` in `write_file_opts`): blocking-pool hop
    /// (shard WRITE lock), counted `staged_rider_folds`.
    async fn retire_rider_record(&self, ino: u64) {
        let key = crate::keys::active_block_ext(ino, 0).to_string();
        let nvme = self.cache.nvme.clone();
        let _ = tokio::task::spawn_blocking(move || nvme.remove_active_block(&key)).await;
        crate::fuse_client::METRICS
            .staged_rider_folds
            .fetch_add(1, Ordering::Relaxed);
    }

    /// W2 (§5.2) read-side record probe: the extents of block `b`'s staged
    /// `active_block_ext:` record intersected with `[start, end)` (offsets
    /// within the block), as `(block_offset, payload_copy)`. One latch-free
    /// occupancy probe when no record exists (R6: the empty-map cost is
    /// today's miss); bad records never compose (the recovery sweep and
    /// the checkout absorb dispose of them loudly).
    /// Zero-alloc W2 overlay **existence** gate for the sync serve legs
    /// (op-economy campaign): does ANY staged extent record exist for
    /// `(file_path, b)`? Conservative on purpose — presence demotes to
    /// the async handler's compose leg regardless of range overlap
    /// ("overlay never invisible"; a record-bearing block is not a warm
    /// clean block, so the demotion costs nothing on the warm path).
    /// One latch-free occupancy-index probe, stack-formatted key.
    pub(crate) fn has_staged_extent_runs(&self, file_path: &str, b: u32) -> bool {
        match crate::keys::StackKey::format(format_args!("active_block_ext:{file_path}:block_{b}"))
        {
            Some(key) => self.cache.nvme.has_staged_extent_record(&key),
            None => {
                // Pathological path length: heap key, same probe.
                let key = crate::keys::active_block_ext_for_path(file_path, b);
                self.cache.nvme.has_staged_extent_record(&key)
            }
        }
    }

    pub(crate) fn staged_extent_runs_in(
        &self,
        file_path: &str,
        b: u32,
        start: usize,
        end: usize,
    ) -> Vec<(usize, Vec<u8>)> {
        let key = crate::keys::active_block_ext_for_path(file_path, b);
        if !self.cache.nvme.has_staged_extent_record(&key) {
            return Vec::new();
        }
        match self.cache.nvme.read_extent_record(&key) {
            Some(Ok(rec)) => rec
                .extents
                .iter()
                .filter(|&&(s, ref d)| (s as usize) < end && s as usize + d.len() > start)
                .map(|&(s, ref d)| {
                    let lo = start.max(s as usize);
                    let hi = end.min(s as usize + d.len());
                    (lo, d[lo - s as usize..hi - s as usize].to_vec())
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Retrieve the file size from metadata.
    pub async fn get_file_size(&self, file_path: &str) -> Result<u64> {
        let meta = self.fetch_metadata(file_path).await?;
        Ok(meta.size)
    }

    pub fn cache(&self) -> &TieredCache {
        &self.cache
    }

    /// Clone a file metadata-only. If it's inline, copy the inline data.
    /// If it's staged, copy the staging folder/files and mapping.
    /// If it's striped, copy the block map and increment all block reference counts.
    pub async fn clone_file(
        &self,
        src: &str,
        dest: &str,
        src_token: Option<u64>,
        dest_token: Option<u64>,
    ) -> Result<()> {
        let _src_ino = parse_inode_from_path(src);
        let dest_ino = parse_inode_from_path(dest);

        let _src_lease;
        let _resolved_src_token = if let Some(t) = src_token {
            t
        } else {
            _src_lease = self
                .dlm
                .acquire_lock(src, None, std::time::Duration::from_secs(5))
                .await?;
            _src_lease.fencing_token()
        };

        let _dest_lease;
        let resolved_dest_token = if let Some(t) = dest_token {
            t
        } else {
            _dest_lease = self
                .dlm
                .acquire_lock(dest, None, std::time::Duration::from_secs(5))
                .await?;
            _dest_lease.fencing_token()
        };

        let meta = self.fetch_metadata(src).await?;

        let mut updated_meta = meta.clone();
        if meta.file_type == "staged" {
            let file_id = meta.file_id.as_ref().ok_or_else(|| {
                SqueezefsError::InvalidOperation("Missing file_id for staged file".to_string())
            })?;
            let new_file_id = Uuid::new_v4().to_string();
            if let Some(mut data) = self.cache.nvme.read_staged(file_id) {
                // W2: the clone's image must carry the source's rider
                // record extents (newer than the ring image).
                let src_ino = parse_inode_from_path(src);
                for (s0, d) in
                    self.staged_extent_runs_in(&crate::keys::inode_path(src_ino), 0, 0, usize::MAX)
                {
                    if data.len() < s0 + d.len() {
                        data.resize(s0 + d.len(), 0);
                    }
                    data[s0..s0 + d.len()].copy_from_slice(&d);
                }
                // FIND-RW5-A: a refused destination stage takes the
                // durable-spill escalation (backend block + `block_map[0]`
                // mapping) — a clone must never surface StorageFull because
                // the staging ring is full of OTHER files' live custody.
                match self
                    .cache
                    .nvme
                    .stage_write(
                        dest,
                        &new_file_id,
                        bytes::Bytes::from(data.clone()),
                        resolved_dest_token,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(SqueezefsError::Io(ref e))
                        if e.kind() == std::io::ErrorKind::StorageFull =>
                    {
                        crate::fuse_client::METRICS
                            .staged_spill_escalations
                            .fetch_add(1, Ordering::Relaxed);
                        let processed = self
                            .get_crypto()
                            .process_write_async(bytes::Bytes::from(data))
                            .await?;
                        let (be_id, block_allocator, nvme_writer) =
                            self.backend_router.get_active_backend()?;
                        crate::block_allocator::ensure_stored_block_image_fits(
                            processed.len(),
                            block_allocator.chunk_size(),
                            "staged-clone spill",
                        )?;
                        let be_offset = block_allocator.allocate_block().await?;
                        // PR VL6a: in-flight until the caller's dest-layout
                        // commit below publishes the mapping (scope-held —
                        // this fn commits `updated_meta` before returning).
                        let _inflight = block_allocator.inflight_register(be_offset);
                        let stored_block_key = format!(
                            "{}:0:{}",
                            self.backend_router.persist_block_key(&be_id, be_offset),
                            processed.len()
                        );
                        if let Err(e) = nvme_writer.write_block(be_offset, processed).await {
                            let _ = block_allocator.free_block(be_offset).await;
                            return Err(e);
                        }
                        block_allocator.publish_block(be_offset);
                        let mut block_map = std::collections::HashMap::new();
                        block_map.insert(0, stored_block_key);
                        updated_meta.block_map = Some(std::sync::Arc::new(block_map));
                    }
                    Err(e) => return Err(e),
                }
            }
            updated_meta.file_id = Some(new_file_id.into());
        } else if meta.file_type == "striped" {
            // All-or-nothing pin of every source block, VALIDATED (§5.1
            // clone/patch fence, design-random-small-writes). Two ways a
            // pin round fails:
            //
            // * **Refused** — the snapshot map is stale (a block was
            //   freed/displaced since the fetch). Never proceed with an
            //   unpinned block: the clone would alias a reallocatable
            //   offset that reads as foreign bytes after reuse.
            // * **PinnedUnstable** — the reference WAS taken, but the
            //   validate-after-pin (pin-CAS → fence(SeqCst) → incarnation
            //   snapshot) observed the block's word unstable: a W1 patch
            //   may be mid-DMA on it. A completed clone must never
            //   reference a block that mutates afterward, so unpin and
            //   retry against the refetched authoritative map (a retry
            //   that lands post-`publish_block` legitimately snapshots the
            //   patched content — old-or-new is clone-vs-write semantics;
            //   mutate-AFTER-completion is the corruption the fence kills).
            //
            // Both arms share the bounded retry: `attempt >= 3` is the
            // accepted bounded EBUSY-class loud refusal (review Issue 16) —
            // a point-in-time clone of a hot-mutating file has no progress
            // guarantee worth a new lock-order edge.
            let src_ino = parse_inode_from_path(src);
            // generic/795 (the whole-file-clone face): a below-size block
            // that is unbound in the map but has a staged `active_block:`
            // sibling is ACKED CUSTODY the clone cannot carry by refcount —
            // cloning around it mints a full-size layout whose block reads
            // zeros. The cfr handler drains this station before calling;
            // any other caller must too. Refuse loud, never corrupt.
            {
                let bs = self.block_size.load(Ordering::Relaxed);
                let blocks = meta.size.div_ceil(bs) as u32;
                for b in 0..blocks {
                    // Bound or unbound alike: a staged sibling / extent
                    // record IS the block's newest acked custody — the
                    // durable image (if any) is stale, and a refcount clone
                    // of it would durably serve the pre-park bytes (zeros
                    // for a gap-baked image).
                    let key = crate::keys::active_block(src_ino, b as u64).to_string();
                    let ext = crate::keys::active_block_ext(src_ino, b as u64).to_string();
                    if self.cache.nvme.has_staged_active_block(&key)
                        || self.cache.nvme.has_staged_extent_record(&ext)
                    {
                        return Err(SqueezefsError::InvalidOperation(format!(
                            "clone source {src} block {b} holds undrained staged                              custody (acked bytes not yet merged into the map) —                              drain/flush the source before cloning"
                        )));
                    }
                }
            }
            let mut current = meta.clone();
            let mut attempt = 0usize;
            loop {
                let map = current.block_map.clone().unwrap_or_default();
                let mut pinned: Vec<&String> = Vec::with_capacity(map.len());
                let mut retry = None;
                for bk in map.values() {
                    match self.backend_router.pin_block_validated(bk) {
                        crate::block_allocator::PinOutcome::Pinned => pinned.push(bk),
                        crate::block_allocator::PinOutcome::PinnedUnstable => {
                            // The unvalidated pin is undone with the rest.
                            pinned.push(bk);
                            retry = Some((bk.clone(), "unstable under a racing in-place patch"));
                            break;
                        }
                        crate::block_allocator::PinOutcome::Refused => {
                            retry = Some((bk.clone(), "freed concurrently"));
                            break;
                        }
                    }
                }
                match retry {
                    None => {
                        updated_meta = current;
                        break;
                    }
                    Some((bad, why)) => {
                        // Undo the partial pins (free_block = one decrement).
                        for bk in pinned {
                            let _ = self.backend_router.free_block(bk).await;
                        }
                        attempt += 1;
                        if attempt >= 3 {
                            return Err(SqueezefsError::InvalidOperation(format!(
                                "clone source {src} block {bad} {why} \
                                 (map still contended after {attempt} attempts); aborting \
                                 to avoid an unpinned/unvalidated clone"
                            )));
                        }
                        let _meta_guard = meta_lock_acquire(src_ino).await;
                        match self.fetch_metadata_from_backend(src_ino).await? {
                            Some(fresh) if fresh.file_type == "striped" => current = fresh,
                            _ => {
                                return Err(SqueezefsError::InvalidOperation(format!(
                                    "clone source {src} changed layout mid-clone; retry the clone"
                                )))
                            }
                        }
                    }
                }
            }
        }

        self.save_metadata_to_backend(dest_ino, &updated_meta, resolved_dest_token)
            .await?;

        let mut cached_opt = self.cache.write_lru.get(src);
        if cached_opt.is_none() {
            cached_opt = self.cache.read_lru.get(src);
        }
        if let Some(cached) = cached_opt {
            self.cache.write_lru.put(dest, cached);
        }

        self.metadata_cache.insert(dest_ino, updated_meta);
        Ok(())
    }

    /// Truncate a file's layout metadata, reclaiming blocks that fall beyond the new size.
    pub async fn truncate_layout(&self, ino: u64, new_size: u64, fencing_token: u64) -> Result<()> {
        let file_path = crate::keys::inode_path(ino);
        let mut meta = self.fetch_metadata(&file_path).await?;

        // `truncate_layout` is only ever called from the SETATTR(size) handler,
        // whose `new_size` is the kernel-authoritative new file length: after
        // it, the file is EXACTLY `new_size` bytes and NOTHING beyond may
        // survive on any tier.
        //
        // Do NOT gate the removal of data beyond `new_size` on a grow-vs-shrink
        // test against `meta.size`. That size is laggable: the hot layout-cache
        // entry can be evicted and refilled from the backend, whose layout-size
        // persist trails the last writes (staged/dirty layouts persist only on
        // fsync; the striped map lags in a narrower window). When the observed
        // `meta.size` is smaller than the file's true physical extent, a real
        // shrink was misclassified as a grow, the "remove beyond new_size" step
        // was SKIPPED, and the stale bytes in `[new_size, physical)` resurfaced
        // as a non-zero read the moment the file was re-extended over them — the
        // durable `generic/616` hole-vs-writeback corruption.
        //
        // The removal operations below are idempotent no-ops when there is
        // nothing beyond `new_size` (a genuine grow), so running them
        // unconditionally is both correct and cheap on the grow path.

        if meta.file_type == "striped" {
            // Removal-RMW through the shared merge primitive (§5.3 one merge
            // discipline): read→prune blocks whose start is >= new_size→save,
            // serialized under INODE_META_LOCKS against every other striped-map
            // writer so a concurrent write-through's entries are never dropped.
            // For a genuine grow this removes no blocks and just publishes
            // `size = new_size`. `TruncateFrom` sets the size authoritatively.
            let removed = self
                .merge_block_mappings(
                    ino,
                    BlockMapOp::TruncateFrom { new_size },
                    new_size,
                    LayoutFlip::KeepLayout,
                    fencing_token,
                )
                .await?;
            let blocks_to_free: Vec<String> =
                removed.iter().map(|bk| clean_block_key(bk)).collect();
            if !blocks_to_free.is_empty() {
                let free_refs: Vec<&str> = blocks_to_free.iter().map(|s| s.as_str()).collect();
                let _ = self.backend_router.free_blocks(&free_refs).await;
            }
            // The bytes past new_size are gone; drop the whole-file RAM
            // snapshot so a later re-extend reads zeros for the hole rather
            // than slicing stale bytes from a cached full-length copy.
            self.cache.write_lru.remove(&file_path);
            self.cache.read_lru.remove(&file_path);
            return Ok(());
        }

        // inline / staged: authoritatively drop everything beyond new_size.
        //
        // Staged files carry the payload on up to TWO physical tiers — the
        // staging-ring blob and/or a promoted/spilled durable whole image
        // (`block_map[0]`) — and BOTH must be clipped, because a later
        // truncate-up re-raises `meta.size` over whatever physical tail
        // survives: every consumer clamps to `meta.size`, so surviving
        // pre-truncate bytes become servable content (the aged-fsx stale
        // resurrection, `tests/staged_truncate_stale_tests.rs`). The old
        // shrink re-staged the clipped image through `stage_write`, whose
        // same-key replace needs a FRESH segment extent — under a
        // full/fragmented ring it REFUSED and the error was swallowed; a
        // promoted blob (ring miss) skipped the shrink entirely.
        //
        // Clip target: `new_size`, gated on the ACTUAL physical length —
        // never the laggable `meta.size`. The cached logical size can lag
        // LOW (hot-entry eviction + deferred layout persist), and the
        // pinned contract (`tests/hole_read_zeros_tests.rs`
        // truncate_down_stale_size) is that a lagging size must never
        // destroy real payload below the truncate point: the blob/durable
        // image is authoritative for `[0, new_size)`. The crash-window
        // guard is ORDERING, not a size comparison: the ring header patch
        // is msync'd before the KV size commit (see
        // `NvmeShard::shrink_staged_value`), so "clipped logical size
        // committed but stale-long blob survived" cannot arise.
        let pre_size = meta.size;

        if meta.file_type == "inline" {
            meta.size = new_size;
            if let Some(ref mut data) = meta.data_key {
                // `Bytes::truncate` is shrink-only (no-op when already shorter),
                // so this is safe on the grow path too.
                if data.len() as u64 > new_size {
                    data.truncate(new_size as usize);
                }
            }
            self.save_metadata_to_backend(ino, &meta, fencing_token)
                .await?;
            self.metadata_cache.insert(ino, meta);
            self.cache.write_lru.remove(&file_path);
            self.cache.read_lru.remove(&file_path);
            return Ok(());
        }

        // Staged. Phase 1 (unlocked): clip the ring blob IN PLACE. An 8-byte
        // header patch of the live extent needs no segment placement, so
        // ring pressure can never refuse it (the flaw that let stale blobs
        // survive). The patch bumps the stage generation under the ledger
        // entry lock, so an in-flight promotion that read the pre-clip image
        // fails its commit-time generation check instead of publishing a
        // stale-long durable copy.
        let snapshot_file_id = meta.file_id.clone();
        if let Some(ref file_id) = snapshot_file_id {
            if let Some(blob_len) = self.cache.nvme.staged_len(file_id) {
                if blob_len > new_size && self.cache.nvme.shrink_staged(file_id, new_size).await {
                    crate::fuse_client::METRICS
                        .staged_truncate_inplace_shrinks
                        .fetch_add(1, Ordering::Relaxed);
                }
                // A `false` shrink means the entry vanished between the peek
                // and the patch (a promotion won the race and REMOVED it
                // after committing its mapping) — the durable leg below
                // resolves that mapping through all sources and clips it.
            }
        }
        // W2 rider record: clip extents past the truncate point or they
        // resurface through the next fold / re-extend (the
        // staged-truncate-stale family applied to the record kind). The
        // caller holds the inode WRITE guard, excluding rider writers.
        if new_size < pre_size {
            self.clip_rider_record(ino, new_size).await;
        }

        // Phase 2 (unlocked, data I/O before the meta flip — P0 layout
        // atomicity): on a genuine shrink, a promoted/spilled durable whole
        // image longer than new_size must be clip-rewritten. Failure is
        // LOUD (`?`): a truncate that cannot prove the durable tail is gone
        // must fail the SETATTR, never silently leave resurrection bait.
        // (`new_size == 0` needs no clip: the commit's prune drops
        // `block_map[0]` entirely — block start 0 >= 0.)
        let mut clipped_bk: Option<(String, String)> = None; // (old, new)
                                                             // PR VL6a: live-owner guard for the clip block's
                                                             // allocate→commit window (drops at function end).
        let mut _clip_inflight: Option<crate::block_allocator::InflightAllocGuard> = None;
        if new_size < pre_size && new_size > 0 {
            if let Some(old_bk) = self.staged_block_mapping(&file_path, &meta).await {
                // The mapping can be displaced under our feet by a racing
                // re-promotion (merge worker — not FUSE-serialized) freeing
                // `old_bk`: a failed read re-resolves the freshest binding
                // once and retries; an error on a STABLE binding is real.
                // Binding-revalidated fetch (the promoted-mapping ABA, same
                // serve rule as the read path and the RMW seed): a mapping
                // can be displaced-and-freed while our read is in flight and
                // the offset instantly re-tenanted by the next promotion —
                // an error-only revalidation misses the poisoned SUCCESS
                // (freed+rewritten bytes read back fine). Only a fetch whose
                // binding still holds after the read may be clipped; on
                // movement re-resolve and retry, bounded, then loud.
                let mut old_bk = old_bk;
                let mut img = None;
                let mut attempts = 0u32;
                loop {
                    attempts += 1;
                    if attempts > 64 {
                        return Err(SqueezefsError::InvalidOperation(format!(
                            "truncate durable clip of {file_path} kept moving after \
                             {attempts} re-resolves (new_size {new_size})"
                        )));
                    }
                    let fetched = self.read_promoted_staged_block(&old_bk).await;
                    let fresh = self.freshest_layout_identity(&file_path).await;
                    let fresh_bk = match fresh {
                        Some(ref f) if f.file_type == "staged" && f.file_id == snapshot_file_id => {
                            f.block_map.as_ref().and_then(|bm| bm.get(&0).cloned())
                        }
                        _ => None,
                    };
                    match (fetched, fresh_bk) {
                        (Ok(i), Some(ref bk)) if *bk == old_bk => {
                            img = Some(i);
                            break;
                        }
                        (Err(e), Some(ref bk)) if *bk == old_bk => return Err(e),
                        (_, Some(bk)) => {
                            crate::fuse_client::METRICS
                                .staged_identity_retries
                                .fetch_add(1, Ordering::Relaxed);
                            old_bk = bk;
                        }
                        (_, None) => break,
                    }
                }
                if let Some(img) = img {
                    if img.len() as u64 > new_size {
                        let clipped = img.slice(0..new_size as usize);
                        let processed = self.get_crypto().process_write_async(clipped).await?;
                        let (be_id, allocator, writer) =
                            self.backend_router.get_active_backend()?;
                        crate::block_allocator::ensure_stored_block_image_fits(
                            processed.len(),
                            allocator.chunk_size(),
                            "staged truncate durable clip",
                        )?;
                        let offset = allocator.allocate_block().await?;
                        _clip_inflight = Some(allocator.inflight_register(offset));
                        // Size-carrying mapping (`bk:0:packed_len` — see
                        // `parse_block_mapping`).
                        let new_bk = format!(
                            "{}:0:{}",
                            self.backend_router.persist_block_key(&be_id, offset),
                            processed.len()
                        );
                        if let Err(e) = writer.write_block(offset, processed).await {
                            let _ = allocator.free_block(offset).await;
                            return Err(e);
                        }
                        allocator.publish_block(offset);
                        crate::fuse_client::METRICS
                            .staged_truncate_durable_clips
                            .fetch_add(1, Ordering::Relaxed);
                        clipped_bk = Some((old_bk, new_bk));
                    }
                }
            }
        }

        // Phase 3: commit under the per-inode metadata lock against the
        // FRESHEST meta (the promote-commit discipline — a promotion/spill
        // that committed since our snapshot must not be clobbered with a
        // pre-commit block_map).
        let commit = async {
            let _meta_guard = meta_lock_acquire(ino).await;
            // NOTE: `fetch_metadata` would retake this lock.
            let current = match self.metadata_cache.get(&ino) {
                Some(m) => Some(m),
                None => self.fetch_metadata_from_backend(ino).await?,
            };
            let mut updated = current.unwrap_or(meta);
            let mut blocks_to_free: Vec<String> = Vec::new();
            // Published = our clipped block took `block_map[0]`. A racing
            // promotion that re-published the mapping since our snapshot
            // wins (its image was read post-clip, hence ≤ clip ≤ new_size —
            // never stale-long); our clip block is then discarded below.
            let mut published = false;
            if updated.file_type == "staged" && updated.file_id == snapshot_file_id {
                if let Some((ref old_bk, ref new_bk)) = clipped_bk {
                    if let Some(ref mut bm) = updated.block_map {
                        // CoW publish (item A): mutate a uniquely-owned copy.
                        if bm.get(&0) == Some(old_bk) {
                            std::sync::Arc::make_mut(bm).insert(0, new_bk.clone());
                            blocks_to_free.push(old_bk.clone());
                            published = true;
                        }
                    }
                }
                // Prune whole blocks at/after new_size (drops `block_map[0]`
                // itself on truncate-to-zero).
                if let Some(ref mut bm) = updated.block_map {
                    let block_size = self.block_size.load(Ordering::Relaxed);
                    std::sync::Arc::make_mut(bm).retain(|&b, bk| {
                        let block_start = b as u64 * block_size;
                        if block_start >= new_size {
                            blocks_to_free.push(bk.clone());
                            false
                        } else {
                            true
                        }
                    });
                }
            }
            updated.size = new_size;
            updated.cached_at = std::time::Instant::now();
            self.save_metadata_to_backend(ino, &updated, fencing_token)
                .await?;
            self.metadata_cache.insert(ino, updated);

            // The truncated tail is gone: drop any whole-file RAM snapshot so
            // a later re-extend reads zeros instead of a stale copy.
            self.cache.write_lru.remove(&file_path);
            self.cache.read_lru.remove(&file_path);

            // Displaced/pruned durable copies die only AFTER the publish
            // (release-superseded order): purge read tiers, then free.
            for bk in &blocks_to_free {
                self.cache.purge_block_key(bk);
            }
            if !blocks_to_free.is_empty() {
                let cleaned: Vec<String> = blocks_to_free
                    .iter()
                    .map(|bk| clean_block_key(bk))
                    .collect();
                let refs: Vec<&str> = cleaned.iter().map(|s| s.as_str()).collect();
                let _ = self.backend_router.free_blocks(&refs).await;
            }
            Ok::<bool, SqueezefsError>(published)
        }
        .await;

        match commit {
            Ok(published) => {
                if !published {
                    // The clipped block never took the mapping (racing
                    // promotion won, or the mapping was pruned): free it.
                    if let Some((_, new_bk)) = clipped_bk {
                        let _ = self.backend_router.free_block(&new_bk).await;
                    }
                }
                Ok(())
            }
            Err(e) => {
                if let Some((_, new_bk)) = clipped_bk {
                    let _ = self.backend_router.free_block(&new_bk).await;
                }
                Err(e)
            }
        }
    }

    /// Hole-punch a set of WHOLE striped block indices: remove them from the
    /// block map (a subsequent read of an unmapped index returns zeros — see
    /// `read_file_range*`) and free their offsets, keeping the logical size
    /// (`min_size`) unchanged. The removed offsets are freed only AFTER the new
    /// map is published (`merge_block_mappings` purges every read tier for the
    /// displaced keys first), and `free_blocks` runs the destructive device
    /// punch inside the allocator's `begin_free → punch → finish_free` window,
    /// so a reused offset can never be read back through the stale map (the
    /// incarnation-seqlock guarantee). The caller must first drop any in-RAM
    /// active-block buffer / staged copy for these indices (those tiers are
    /// owned by the FUSE layer). Partial-edge blocks are NOT handled here — the
    /// caller RMW-zeros those through the write path.
    pub async fn punch_striped_blocks(
        &self,
        ino: u64,
        block_idxs: &[u32],
        min_size: u64,
        fencing_token: u64,
    ) -> Result<()> {
        if block_idxs.is_empty() {
            return Ok(());
        }
        let removed = self
            .merge_block_mappings(
                ino,
                BlockMapOp::RemoveBlocks(block_idxs),
                min_size,
                LayoutFlip::KeepLayout,
                fencing_token,
            )
            .await?;
        let blocks_to_free: Vec<String> = removed.iter().map(|bk| clean_block_key(bk)).collect();
        if !blocks_to_free.is_empty() {
            let free_refs: Vec<&str> = blocks_to_free.iter().map(|s| s.as_str()).collect();
            let _ = self.backend_router.free_blocks(&free_refs).await;
        }
        // Drop the whole-file RAM snapshot: the punched indices are holes now.
        let file_path = crate::keys::inode_path(ino);
        self.cache.write_lru.remove(&file_path);
        self.cache.read_lru.remove(&file_path);
        Ok(())
    }

    /// Safely delete all underlying storage files/blocks associated with the file.
    pub async fn delete_file(
        &self,
        file_path: &str,
        _con: &mut crate::dlm::MetaConnection,
    ) -> Result<()> {
        let meta = self.fetch_metadata(file_path).await?;

        let mut blocks_to_free: Vec<String> = Vec::new();

        if let Some(ref block_map) = meta.block_map {
            for bk in block_map.values() {
                self.cache.purge_block_key(bk);
                blocks_to_free.push(clean_block_key(bk));
            }
        }

        if meta.file_type == "staged" {
            if let Some(ref file_id) = meta.file_id {
                // Blocking-pool hop: shard WRITE lock (invariant rule 2).
                let _ = self
                    .cache
                    .nvme
                    .remove_staged_async(file_id.to_string())
                    .await;
            }
        }

        if let Some(ref map_id) = meta.block_map_id {
            if map_id.starts_with("indirect:") {
                let block_key = map_id.strip_prefix("indirect:").unwrap();
                blocks_to_free.push(block_key.to_string());
            }
        }

        if !blocks_to_free.is_empty() {
            let free_refs: Vec<&str> = blocks_to_free.iter().map(|s| s.as_str()).collect();
            let _ = self.backend_router.free_blocks(&free_refs).await;
        }

        // No per-corpse `removexattr("layout")` transaction here: the sole
        // caller is inode reclaim, whose batched `destroy_inodes` kills the
        // whole xattr block inside its own commit (one transaction per batch
        // instead of one per corpse — the extra commit's sector guards
        // collided with foreground unlinks under delete storms).

        // Staged-overlay teardown is O(PRESENT + map), never O(logical
        // size): the pre-fix sweep enumerated EVERY logical block
        // (`0..=max_block`) to build the key list — a 16 TiB sparse
        // corpse meant a 4M-entry HashSet + 8M key Strings on the
        // reclaim worker, minutes of post-unmount daemon linger (the
        // fstests generic/294/306/452/529/530 "previous daemon still
        // running after 60s" family; pinned in
        // tests/sparse_write_bounded_tests.rs::delete_of_huge_sparse_file_is_omap).
        // The occupancy index lists exactly what is staged for this path
        // (both `active_block:` and `active_block_ext:` families, the
        // canonical `…:block_{b}` forms); the block map adds the mapped
        // indices' canonical keys for defense in depth.
        // One blocking-pool hop for the whole sweep: each removal is a
        // staging shard WRITE lock (shard-lock invariant rule 2).
        let mut keys: Vec<String> = self
            .cache
            .nvme
            .staged_keys_with_prefix(&format!("active_block:{file_path}:"));
        keys.extend(
            self.cache
                .nvme
                .staged_keys_with_prefix(&format!("active_block_ext:{file_path}:")),
        );
        if let Some(ref block_map) = meta.block_map {
            for &b in block_map.keys() {
                keys.push(crate::keys::active_block_for_path(file_path, b).to_string());
                keys.push(crate::keys::active_block_ext_for_path(file_path, b).to_string());
            }
        }
        keys.sort_unstable();
        keys.dedup();
        let _ = self.cache.nvme.remove_active_blocks_async(keys).await;

        self.cache.write_lru.remove(file_path);
        self.cache.read_lru.remove(file_path);
        self.metadata_cache
            .invalidate(&parse_inode_from_path(file_path));
        Ok(())
    }

    /// Resolve a logical filesystem path (e.g., "/dir1/file.txt") to its FUSE inode number.
    pub async fn resolve_path_to_inode(&self, path: &str) -> Result<u64> {
        let mut current_ino = 1u64; // Root inode

        if let Some(backend) = self.inner.meta_backend.get() {
            for part in path.split('/') {
                if part.is_empty() || part == "." {
                    continue;
                }
                let inode = backend.lookup(current_ino, part).await?;
                current_ino = inode.ino;
            }
        }

        Ok(current_ino)
    }

    /// Clone a path to another path metadata-only.
    pub async fn clone_path(&self, src_path: &str, dest_path: &str) -> Result<()> {
        let src_ino = self.resolve_path_to_inode(src_path).await?;

        let dest_p = std::path::Path::new(dest_path);
        let parent_str = dest_p.parent().and_then(|p| p.to_str()).unwrap_or("");
        let file_name = dest_p.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            SqueezefsError::InvalidOperation("Invalid destination filename".to_string())
        })?;

        let parent_ino = self.resolve_path_to_inode(parent_str).await?;

        if let Some(backend) = self.inner.meta_backend.get() {
            let src_inode = backend.getattr(src_ino).await?;
            let dest_inode = backend
                .create_with_rdev(
                    parent_ino,
                    file_name,
                    src_inode.mode,
                    src_inode.uid,
                    src_inode.gid,
                    src_inode.rdev,
                )
                .await?;
            let _ = backend
                .setattr(
                    dest_inode.ino,
                    Some(src_inode.mode),
                    None,
                    None,
                    Some(src_inode.size),
                    None,
                    None,
                    None,
                )
                .await?;

            self.clone_file(
                crate::keys::inode_path(src_ino).as_str(),
                crate::keys::inode_path(dest_inode.ino).as_str(),
                None,
                None,
            )
            .await?;
        }

        Ok(())
    }
}

pub struct IoUringPrefetcher {
    #[cfg(target_os = "linux")]
    tx: tokio::sync::mpsc::Sender<(u64, usize)>,
    pub prefetch_count: std::sync::atomic::AtomicUsize,
}

impl IoUringPrefetcher {
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(u64, usize)>(8192);
            std::thread::spawn(move || {
                use io_uring::{opcode, IoUring};
                let mut ring = match IoUring::new(512) {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("Failed to initialize prefetcher io_uring: {:?}", e);
                        return;
                    }
                };
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    while let Some((addr, len)) = rx.recv().await {
                        let prefetch_e = opcode::Madvise::new(
                            addr as *mut std::ffi::c_void,
                            len as i64,
                            libc::MADV_WILLNEED,
                        )
                        .build()
                        .user_data(0x99);
                        unsafe {
                            if let Err(e) = ring.submission().push(&prefetch_e) {
                                log::debug!("Prefetcher: Failed to push to io_uring: {:?}", e);
                                continue;
                            }
                        }
                        if let Err(e) = ring.submit() {
                            log::debug!("Prefetcher: io_uring submit failed: {:?}", e);
                        }
                        // Reap completions
                        let mut cq = ring.completion();
                        cq.sync();
                        for _ in cq {}
                    }
                });
            });
            Self {
                tx,
                prefetch_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                prefetch_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    pub fn prefetch(&self, _addr: u64, _len: usize) {
        self.prefetch_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(target_os = "linux")]
        {
            let _ = self.tx.try_send((_addr, _len));
        }
    }
}

impl Default for IoUringPrefetcher {
    fn default() -> Self {
        Self::new()
    }
}
