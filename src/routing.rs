use crate::cache::{TieredCache, BUFFER_POOL};
use crate::dlm::DlmClient;

pub const MAX_INLINE_SIZE: usize = 4096;

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
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

#[derive(serde::Serialize, serde::Deserialize, Clone, Default, Debug)]
pub struct LayoutMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub data_key: Option<Vec<u8>>,
    pub block_map: Option<std::collections::HashMap<u32, String>>,
}

#[derive(Clone, Debug)]
pub struct CachedMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
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
}

impl Default for CachedMetadata {
    fn default() -> Self {
        Self {
            file_type: "inline".to_string(),
            size: 0,
            block_map_id: None,
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
            block_map: None,
            layout_dirty: false,
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
    /// P1-11: lock-free active backend id (hot path read).
    pub active_write_backend: std::sync::Arc<arc_swap::ArcSwap<String>>,
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
        let router = Self {
            default_allocator,
            default_device,
            backends: std::sync::Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new())),
            unhealthy_backends: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            active_write_backend: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                "backend_0".to_string(),
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
        };
        // Seed the table so bare routers place without waiting for a
        // worker tick (construction-time probe, never per-write).
        router.refresh_placement_table();
        router
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

    fn punch_hole_sync(device_path: &str, offset: u64, size: u64) {
        #[cfg(target_os = "linux")]
        {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(device_path) {
                use std::os::unix::io::AsRawFd;
                let fd = file.as_raw_fd();
                unsafe {
                    let _ = libc::fallocate(
                        fd,
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        offset as libc::off_t,
                        size as libc::off_t,
                    );
                }
            }
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

    /// Free one reference on a block key. The hole punch is destructive
    /// device I/O and runs ONLY on the terminal release (a non-terminal
    /// free must never zero a clone's still-referenced bytes), and runs in
    /// the `begin_free` → punch → `finish_free` window — the offset is not
    /// reallocatable until after the punch, so the punch can never race a
    /// new owner's DMA at the reused offset (the acked-write lost-update
    /// class surfaced by PR 6's pinned striped concurrency test).
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
            Self::punch_hole_sync(&device_path, offset, block_size);
            allocator.finish_free(offset);
        }
        Ok(())
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
        let router = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            // Per-backend probe hysteresis: only FAILURE_THRESHOLD consecutive
            // hard failures mark a backend unhealthy (a starved probe under
            // saturation is inconclusive, never a flip — see crate::health).
            let mut states: std::collections::HashMap<String, crate::health::HealthState> =
                std::collections::HashMap::new();
            loop {
                interval.tick().await;

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
                // for probe-driven health transitions.
                router.refresh_placement_table();

                // 3. Trigger failover if currently active write backend is
                // unhealthy — or no longer placement-eligible (VL4: the
                // sticky pointer is repointed off a Draining volume as a
                // side effect, §5.4).
                let active_be = (*router.active_write_backend.load_full()).clone();
                if !router.placement_eligible(&active_be) {
                    log::warn!(
                        "Active write backend '{}' is unhealthy or not placement-eligible! \
                         Initiating failover...",
                        active_be
                    );
                    // Fail over to an eligible NAMED volume; the `backend_0`
                    // default slot is a candidate only on bare routers (same
                    // policy as `get_active_backend`).
                    let mut fallback_be = None;
                    for entry in router.backends.iter() {
                        let be_id = entry.key();
                        if router.placement_eligible(be_id) {
                            fallback_be = Some(be_id.clone());
                            break;
                        }
                    }
                    if fallback_be.is_none()
                        && router.backends.is_empty()
                        && router.is_backend_healthy("backend_0")
                    {
                        fallback_be = Some("backend_0".to_string());
                    }

                    if let Some(healthy_be) = fallback_be {
                        log::info!(
                            "Failover: Switching active write backend from '{}' to '{}'",
                            active_be,
                            healthy_be
                        );
                        router
                            .active_write_backend
                            .store(std::sync::Arc::new(healthy_be.clone()));
                        // Switch is completed in-memory.
                    } else {
                        log::error!("Failover failed: No healthy storage backends available!");
                    }
                }
            }
        });
    }
}

async fn perform_device_health_check(dev: &crate::nvme_dev::NvmeBlockDev) -> crate::health::Probe {
    use crate::health::Probe;
    if !std::path::Path::new(&dev.device_path).exists() {
        return Probe::Failed;
    }
    // The probe shares the device's I/O lanes with real traffic: a timeout
    // means "busy", not "dead" — report it as inconclusive so saturation can
    // never flip a healthy backend offline (hysteresis in crate::health).
    match tokio::time::timeout(Duration::from_secs(2), dev.read_block(0, 4096)).await {
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
    pub metadata_cache: moka::sync::Cache<u64, CachedMetadata>,
    pub block_map_cache: moka::sync::Cache<(String, u32), (Option<String>, std::time::Instant)>,
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
    /// §5.5 window cap (`SQUEEZEFS_READ_PREFETCH_WINDOW`, default 16;
    /// 0 disables the pipeline outright).
    pub(crate) prefetch_window_cap: u32,
    /// §5.5 contention scaling: prefetch's share of the hot-tier budget
    /// (`SQUEEZEFS_READ_PREFETCH_SHARE_PCT`, default 50).
    pub(crate) prefetch_share_pct: u64,
    /// §5.5 `active_streams` two-epoch activity gauge (leak-proof by
    /// construction: increment-only per epoch, aged by the roll — lanes
    /// die silently inside moka, so a dec path would leak upward).
    pub(crate) stream_gauge: StreamActivityGauge,
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
    pub stripe_write_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
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

/// K = 4 offset lanes per file, so concurrent sequential readers of one
/// file do not mutually reset each other. More than K concurrent readers
/// degrade the excess to the random class — a later pipeline start,
/// never wrongness.
pub struct StreamLanes {
    lanes: [StreamLane; 4],
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
        }
    }

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
    pub(crate) fn observe(&self, offset: u64, len: u64) -> Option<LaneRef<'_>> {
        use std::sync::atomic::Ordering::Relaxed;
        let now = Self::now_ms();
        // Lane match: continue the run.
        for lane in &self.lanes {
            if lane.next_expected_offset.load(Relaxed) == offset {
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
                        file_type: layout.file_type,
                        size: layout.size,
                        block_map_id: layout.block_map_id,
                        block_prefix: layout.block_prefix,
                        file_id: layout.file_id,
                        cached_at: std::time::Instant::now(),
                        data_key: layout.data_key.map(bytes::Bytes::from),
                        block_map: block_map.map(std::sync::Arc::new),
                        layout_dirty: false,
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

        let mut old_indirect_to_free = None;
        // The layout as it would be persisted INLINE (block map retained under
        // the inline sentinel). The indirect branch below overwrites the map /
        // id only if the serialized value spills past the per-volume cap.
        let mut layout = LayoutMetadata {
            file_type: m.file_type.clone(),
            size: m.size,
            block_map_id: m.block_map.as_ref().map(|_| format!("block_map_{}", ino)),
            block_prefix: m.block_prefix.clone(),
            file_id: m.file_id.clone(),
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

        // PR VL6a: a freshly allocated indirect blob is registered
        // in-flight until this function's `set_layout_and_size` publishes
        // the layout naming it (the guard drops at function end).
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
            nvme_writer.write_block(offset, data_bytes).await?;

            layout.block_map = None;
            layout.block_map_id = Some(format!("indirect:{}", block_key));

            bincode::serialize(&layout).map_err(|e| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Failed to serialize binary layout: {:?}", e),
                ))
            })?
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

        backend.set_layout_and_size(ino, &bytes, m.size).await?;
        // Keep hot cache coherent without a remove+refetch on the next write.
        let mut cached = m.clone();
        cached.cached_at = std::time::Instant::now();
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
        let prefetch_window_cap = match std::env::var("SQUEEZEFS_READ_PREFETCH_WINDOW") {
            Ok(v) => v.trim().parse::<u32>().unwrap_or_else(|e| {
                panic!("SQUEEZEFS_READ_PREFETCH_WINDOW must be an integer: {e}")
            }),
            Err(_) => 16,
        }
        .min(16);
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
        let block_map_capacity = std::cmp::max(50_000, total_memory / 50_000);

        // Process parallelism, not the (possibly core-pinned) constructor
        // thread's mask — the Hang-1 sizing poison collapsed this to 4.
        let stripe_permits = crate::cpu::process_parallelism() * 4;
        let stripe_write_semaphore =
            std::sync::Arc::new(tokio::sync::Semaphore::new(stripe_permits));

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
                    .build(),
                block_map_cache: moka::sync::Cache::builder()
                    .max_capacity(block_map_capacity)
                    .time_to_live(std::time::Duration::from_secs(300))
                    .build(),
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
                prefetch_window_cap,
                prefetch_share_pct,
                stream_gauge: StreamActivityGauge::new(),
                crypto: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
                prefetcher: std::sync::Arc::new(IoUringPrefetcher::new()),
                stripe_write_semaphore,
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

        let decompressed = self.get_crypto().process_read_async(raw).await?;
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
            .get_cached_or_fetch_block_traced(block_key, false)
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
    /// `speculative`: the fill is a §5.5 pipeline prefetch. It keeps FULL
    /// ghost semantics (record + hit): with the pipeline fetching every
    /// block of every classified pass, a ghost BYPASS (tried first) meant
    /// a genuinely re-read stream never admitted to the disk tier — warm
    /// re-reads stayed device-bound forever (measured: 9.2 GiB/s vs the
    /// 16.6 lineage; R-1's mitigation stack broken). Same-pass fake heat
    /// is prevented MECHANICALLY instead: the one-lap clock grace + the
    /// progress-clocked quiescence keep one pass ≈ one miss per key
    /// (`prefetch_evicted_unconsumed` ≈ 0), so a second recorded miss is
    /// real cross-pass re-read heat regardless of which agent fetched.
    /// What `speculative` DOES change: a non-admitted fill's hot put
    /// carries the one-lap grace (`put_probationary_referenced`) — clock
    /// parity with the consumed residue it races (never stickiness).
    async fn get_cached_or_fetch_block_traced(
        &self,
        block_key: &str,
        speculative: bool,
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
                return Ok((
                    crate::cache::pool::ReadBlockValue::Bytes(cached_block),
                    true,
                ));
            }

            if let Some(cached_block) = self.cache.read_lru.get(block_key) {
                METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
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
                return Ok((crate::cache::pool::ReadBlockValue::Bytes(bytes), true));
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
                // Completion may have raced between get_sync and subscribe — recheck.
                if let Some(cached_block) = self
                    .cache
                    .hot_block
                    .get_no_promote(block_key)
                    .or_else(|| self.cache.read_lru.get(block_key))
                {
                    METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
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
                                    hit
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
                                self.cache
                                    .hot_block
                                    .put(block_key, downloaded_bytes.clone());
                            } else if speculative {
                                // §5.5 pipeline fill: probation class WITH
                                // the one-lap clock grace — parity with
                                // consumed residue whose serves re-arm
                                // `referenced`; without it the clock evicts
                                // the pipeline's future to keep the
                                // stream's past (595 refetches on the
                                // row-2 shape, measured).
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
    ) {
        if self.prefetch_window_cap == 0 || meta.file_type != "striped" {
            return;
        }
        let lanes = self.stream_lanes.get_with(file_path.to_string(), || {
            std::sync::Arc::new(StreamLanes::new())
        });
        let (lane_idx, streaming) = {
            let Some(lane_ref) = lanes.observe(offset, len) else {
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

        // Foreground caught the pipeline: window ×2 (capped).
        if will_wait_inflight && mem_level == crate::mem_budget::Level::Green {
            METRICS.prefetch_foreground_waits.fetch_add(1, Relaxed);
            let _ = lane.window.fetch_update(Relaxed, Relaxed, |w| {
                Some((w.saturating_mul(2)).min(self.prefetch_window_cap))
            });
        }

        // Contention-scaled effective window (§5.5 mechanism ii). NO floor:
        // the doc's formula truncating to 0 is a signal, not an edge case —
        // when the per-lane share of the hot budget cannot retain even ONE
        // block, every speculative fill is guaranteed evicted-before-
        // consume, so the only non-wasteful window is empty (a .max(1)
        // floor here kept 4 contention-phase lanes thrashing a 2-block
        // budget: 104 fetches for 48 unique — measured).
        let active = self.stream_gauge.touch(lane) as u64;
        let hot_budget = self.cache.hot_block.max_bytes();
        let share_blocks = self.prefetch_share_pct * hot_budget / 100 / block_size.max(1) / active;
        let window = lane.window.load(Relaxed).min(self.prefetch_window_cap);
        let effective = (window as u64).min(share_blocks) as u32;
        let hwm = METRICS.prefetch_window_hwm.load(Relaxed);
        if (effective as u64) > hwm {
            METRICS.prefetch_window_hwm.store(effective as u64, Relaxed);
        }
        if effective == 0 {
            return;
        }

        // (Re)base the plan when it is uninitialized or fell behind the
        // reader: issue restarts past this request. The skipped span was
        // never issued, so the covered window empties with the base.
        if lane.next_prefetch_block.load(Relaxed) <= end_block {
            lane.next_prefetch_block.store(end_block + 1, Relaxed);
            lane.issued_base.store(end_block + 1, Relaxed);
        }

        // Top up: resident-unconsumed + in-flight bounded by the window
        // (§5.5 mechanism i — issue stops; the spiral cannot start), and
        // gated by the progress-clocked quiescence arm (sustained
        // starvation pauses issue instead of re-feeding the evict cycle).
        if lane.consumed_edge.load(Relaxed) < lane.suppress_until_edge.load(Relaxed) {
            return;
        }
        let total_blocks = meta.size.div_ceil(block_size) as u32;
        loop {
            let in_flight = lane.inflight.load(Relaxed);
            let unconsumed = lane.unconsumed.load(Relaxed);
            if in_flight.saturating_add(unconsumed) >= effective {
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
            match router.get_cached_or_fetch_block_traced(&key, true).await {
                Ok(_) => settle(true),
                Err(err) => {
                    debug!("Prefetch: failed to fetch block {key}: {err:?}");
                    settle(false);
                }
            }
        })
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
    /// `escalate_contended` (VL8 item 7): MOVEMENT (the binding changed —
    /// a COW rewrite displaced the key) and CONTENTION (the binding is
    /// unchanged but the key's incarnation word keeps moving — a W1
    /// in-place patch storm on this very block) are different failures.
    /// Movement keeps the bounded rebind ladder. Contention has no rebind
    /// to make: with `escalate_contended`, after two contended attempts
    /// ONE fetch is serialized under the block's `BLOCK_FLUSH_LOCKS`
    /// stripe — the lock every patch holds across its DMA — so the
    /// seqlock settles and the read makes guaranteed progress instead of
    /// exhausting the bound into a spurious EIO (the bimodal
    /// read-mid-patch failure). MUST be `false` at call sites that may
    /// already hold this block's stripe (seed fetches under a held block
    /// guard; write-side RMW seeds) — the stripe is not reentrant.
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
        // Each retry re-resolves against the freshest map. MOVEMENT
        // exhaustion (the binding itself kept changing) requires
        // back-to-back whole COW-rewrite cycles of this one block landing
        // inside single fetches — churn far past any real workload — and
        // fails loud rather than serving unproven bytes. CONTENTION never
        // reaches exhaustion on the escalating path (see above).
        const MAX_REBINDS: usize = 8;
        const CONTENDED_BEFORE_ESCALATE: usize = 2;
        let mut key: Option<String> = resolved_key.map(str::to_string);
        let mut contended = 0usize;
        for _ in 0..MAX_REBINDS {
            let Some(cur_key) = key else {
                return Ok(None);
            };
            // VL8 item 7 contention escalation: hold the block's stripe
            // across this one fetch so no patch can overlap it.
            let _contention_guard = if escalate_contended && contended >= CONTENDED_BEFORE_ESCALATE
            {
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
            let (val, incarnation_valid) = if device_true {
                self.fetch_block_device_true(&cur_key).await?
            } else {
                self.get_cached_or_fetch_block_traced(&cur_key, false)
                    .await?
            };
            // Recheck the binding only AFTER the bytes are in hand: the
            // proof needs (movement between snapshot and serve) ⇒ (word
            // changed), which only holds when the recheck follows the read.
            let recheck_delay = TEST_BINDING_RECHECK_DELAY_MS.load(Ordering::Relaxed);
            if recheck_delay > 0 {
                tokio::time::sleep(Duration::from_millis(recheck_delay)).await;
            }
            let current = self.current_block_binding(file_path, b).await?;
            if incarnation_valid && current.as_deref() == Some(cur_key.as_str()) {
                return Ok(Some(val));
            }
            if current.as_deref() == Some(cur_key.as_str()) {
                contended += 1;
            } else {
                contended = 0;
            }
            METRICS
                .stale_binding_rebinds
                .fetch_add(1, Ordering::Relaxed);
            debug!(
                "stale-binding rebind: file={} block={} resolved_key={} current={:?} fill_valid={}",
                file_path, b, cur_key, current, incarnation_valid
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
                let ghost_hit = self.ghost.check_and_record(k);
                if ghost_hit
                    && crate::mem_budget::level() != crate::mem_budget::Level::Red
                    && !self.escalation_cooldown.recently_escalated(k)
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
        for _ in 0..MAX_REBINDS {
            let Some(cur_key) = key else {
                return Ok(None);
            };
            let tracked = self.backend_router.key_incarnation_tracked(&cur_key);
            let before = self.backend_router.fill_incarnation(&cur_key);

            METRICS.ranged_reads.fetch_add(1, Ordering::Relaxed);
            METRICS
                .ranged_read_bytes
                .fetch_add(window as u64, Ordering::Relaxed);
            if bounced {
                METRICS
                    .ranged_read_unaligned_bounces
                    .fetch_add(1, Ordering::Relaxed);
            }
            let window_bytes = match &dest {
                Some(d) => {
                    // DMA straight into the registered payload dest; the
                    // value below is constructed only after validation.
                    self.backend_router
                        .read_block_range(&cur_key, aligned_start, window, Some(d.ptr as u64))
                        .await?
                }
                None => {
                    self.backend_router
                        .read_block_range(&cur_key, aligned_start, window, None)
                        .await?
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
                crate::cache::pool::ReadBlockValue::Bytes(bytes::Bytes::from_owner(
                    crate::cache::pool::UringBufOwner {
                        ptr: d.ptr,
                        len: req_len,
                    },
                ))
            }
            None => {
                let mut out = vec![0u8; req_len];
                out[..end - start].copy_from_slice(&val[start..end]);
                crate::cache::pool::ReadBlockValue::Bytes(bytes::Bytes::from(out))
            }
        }
    }

    pub async fn fetch_metadata(&self, file_path: &str) -> Result<CachedMetadata> {
        use std::time::Duration;
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
        let fresh_or_dirty = |entry: &CachedMetadata| {
            entry.layout_dirty || entry.cached_at.elapsed() < Duration::from_secs(1)
        };
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
            file_type: "inline".to_string(),
            size: 0,
            block_map_id: None,
            block_prefix: None,
            file_id: None,
            cached_at: std::time::Instant::now(),
            data_key: None,
            block_map: None,
            layout_dirty: false,
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

        let mut current = match self.fetch_metadata_from_backend(ino).await? {
            Some(m) => m,
            // Never-persisted layout: the freshest RAM entry (post-write
            // truth for dirty layouts) beats an empty default.
            None => self.metadata_cache.get(&ino).unwrap_or_default(),
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
        current.block_map = Some(block_map_arc);

        match layout_flip {
            LayoutFlip::ToStripedKeepStagedIdentity => {
                current.file_type = "striped".to_string();
            }
            LayoutFlip::ToStripedClearStagedIdentity => {
                current.file_type = "striped".to_string();
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
                entry.cached_at = std::time::Instant::now();
                self.metadata_cache.insert(ino, entry);
            }
        }
    }

    /// L4-8 (§5.5.1 "the 1 M+ engine"): the SYNC serve mirror for the IPC
    /// read fast path — exactly the sync-servable striped legs of
    /// [`Self::read_file_range_zero_copy`] (the staging mmap ring, then
    /// the R4 hot-block tier), no awaits, no blocking locks. `None` = any
    /// shape needing async work (W2 extent overlays, multi-block, cold
    /// blocks, non-striped layouts) — the caller demotes to the handoff,
    /// which runs the full handler; parity is by construction because
    /// these legs are pointwise copies of the handler's own (same keys,
    /// same clamps, same currency rules).
    ///
    /// Caller contract: `offset + read_len` already clamped to the
    /// authoritative size (the fast path's guarded size check), and the
    /// per-inode read guard held (mutators excluded — the same currency
    /// the handler's serve legs rely on).
    pub fn try_read_range_sync(
        &self,
        file_path: &str,
        meta: &CachedMetadata,
        offset: u64,
        read_len: usize,
    ) -> Option<bytes::Bytes> {
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
            let guard = self.cache.nvme.read_staged_zero_copy(file_id)?;
            if !self
                .staged_extent_runs_in(file_path, 0, offset as usize, offset as usize + read_len)
                .is_empty()
            {
                return None;
            }
            let start = (offset as usize).min(guard.len);
            let end = (offset as usize + read_len).min(guard.len);
            METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
            if end - start == read_len {
                return Some(bytes::Bytes::copy_from_slice(&guard[start..end]));
            }
            let mut out = vec![0u8; read_len];
            out[..end - start].copy_from_slice(&guard[start..end]);
            return Some(bytes::Bytes::from(out));
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
        if !self
            .staged_extent_runs_in(file_path, start_block, rel_s, rel_e)
            .is_empty()
        {
            return None;
        }

        // Leg 1: the staging mmap ring — same key, same clamps as the
        // handler's "check active block staging first" leg (short serves
        // included: a shorter staged image returns short, exactly as the
        // handler would).
        let cache_key = crate::keys::active_block_for_path(file_path, start_block).to_string();
        if let Some(guard) = self.cache.nvme.read_staged_zero_copy(&cache_key) {
            let start = rel_s.min(guard.len);
            let end = rel_e.min(guard.len);
            return Some(bytes::Bytes::copy_from_slice(&guard[start..end]));
        }

        // Leg 2: the R4 hot tier. Skipped wholesale under the
        // device-true diagnostic posture: per-op O_DIRECT-ness is not
        // visible on the ring, and quietly tier-serving there would turn
        // the device-true il row into a warm row (measurement fraud).
        if self.direct_device_true() {
            return None;
        }
        let b_key = if let Some(map) = &meta.block_map {
            map.get(&start_block).cloned()?
        } else if let Some(id) = &meta.block_map_id {
            self.block_map_cache
                .get(&(id.clone(), start_block))
                .and_then(|(k, _)| k)?
        } else {
            format!("{}/part_{}", meta.block_prefix.as_ref()?, start_block)
        };
        if let Some(hot) = self.cache.hot_block.get_no_promote(&b_key) {
            let start = rel_s.min(hot.len());
            let end = rel_e.min(hot.len());
            METRICS.hot_block_hits.fetch_add(1, Ordering::Relaxed);
            return Some(hot.slice(start..end));
        }

        // Leg 3: the NVMe read-cache shard (sync mmap — the handler's
        // "tier fast path with binding recheck" leg). Binding currency:
        // the handler rechecks via `current_block_binding().await`, whose
        // source under a warm metadata cache is exactly the CURRENT map
        // `b_key` was just derived from — and this daemon's writers
        // update that map synchronously under the inode lock the caller
        // holds. Same currency class, no await.
        let guard = self.cache.nvme.get_cached_read_block_range_zero_copy(
            &b_key,
            rel_s as u64,
            read_len as u32,
        )?;
        METRICS.cache_hits.fetch_add(1, Ordering::Relaxed);
        Some(bytes::Bytes::copy_from_slice(&guard))
    }

    pub async fn load_striped_block_keys(
        &self,
        _file_path: &str,
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
        } else if let Some(block_map_id) = &meta.block_map_id {
            for b in start_block..=end_block {
                let cache_key = (block_map_id.clone(), b);
                if let Some(entry) = self.block_map_cache.get(&cache_key) {
                    let (bk, _) = &entry;
                    block_keys.push((b, bk.clone()));
                } else {
                    block_keys.push((b, None));
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
                    let non_extending = offset + data.len() as u64 <= meta.size;
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
                updated_meta.file_type = "striped".to_string();
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
            updated_meta.file_type = "inline".to_string();
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
                .unwrap_or_else(|| Uuid::new_v4().to_string());

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
                    updated_meta.file_type = "staged".to_string();
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
                    updated_meta.file_type = "staged".to_string();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(spill_file_id);
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
        // Fetch metadata first. The whole-file RAM snapshot (read_lru /
        // write_lru keyed by file_path) is deliberately NOT consulted on the
        // read data path: it is a whole-file copy that goes stale under
        // sub-file mutations (write / punch / truncate) of staged & striped
        // files — a punched or truncated range would otherwise read its prior
        // bytes instead of zeros (the generic/616 residual). Each layout below
        // serves from its authoritative, mutation-tracking tier instead: the
        // inline `data_key`, the staging ring (staged), or the per-block read
        // caches / hole map (striped).
        let mut meta = self.fetch_metadata(file_path).await?;

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
                                && f.file_id.as_deref() == Some(file_id.as_str())
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
                            || fresh.file_id.as_deref() != Some(file_id.as_str())
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
                        let block_keys = self
                            .load_striped_block_keys(file_path, &meta, start_block, end_block)
                            .await?;
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
                        // the windowed top-up.
                        if !device_true {
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
                                if let Some(hot) = self.cache.hot_block.get_no_promote(b_key) {
                                    let start = std::cmp::min(slice_start as usize, hot.len());
                                    let end = std::cmp::min(
                                        (slice_start + slice_len as u64) as usize,
                                        hot.len(),
                                    );
                                    let data = if let Some(dest) = dest_addr {
                                        let len = end - start;
                                        let dest_ptr = dest as *mut u8;
                                        unsafe {
                                            std::ptr::copy_nonoverlapping(
                                                hot[start..end].as_ptr(),
                                                dest_ptr,
                                                len,
                                            );
                                        }
                                        bytes::Bytes::from_owner(
                                            crate::cache::pool::UringBufOwner {
                                                ptr: dest_ptr,
                                                len,
                                            },
                                        )
                                    } else {
                                        hot.slice(start..end)
                                    };
                                    if self
                                        .current_block_binding(file_path, start_block)
                                        .await?
                                        .as_deref()
                                        == Some(b_key.as_str())
                                    {
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
                                    let len = guard.len();
                                    let data = if let Some(dest) = dest_addr {
                                        let dest_ptr = dest as *mut u8;
                                        unsafe {
                                            std::ptr::copy_nonoverlapping(
                                                guard.as_ptr(),
                                                dest_ptr,
                                                len,
                                            );
                                            bytes::Bytes::from_owner(
                                                crate::cache::pool::UringBufOwner {
                                                    ptr: dest_ptr,
                                                    len,
                                                },
                                            )
                                        }
                                    } else {
                                        bytes::Bytes::copy_from_slice(&guard)
                                    };
                                    drop(guard);
                                    if self
                                        .current_block_binding(file_path, start_block)
                                        .await?
                                        .as_deref()
                                        == Some(b_key.as_str())
                                    {
                                        if hint.odirect {
                                            METRICS
                                                .read_odirect_tier_serves
                                                .fetch_add(1, Ordering::Relaxed);
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
                                    // The payload arena is 4 KiB-aligned by
                                    // construction; offer the dest only on
                                    // the zero-copy leg (window == request).
                                    Some(d) if aligned => Some(RangedDest {
                                        ptr: d as *mut u8,
                                        cap: slice_len as usize,
                                    }),
                                    _ => None,
                                };
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
                                                bytes::Bytes::from_owner(
                                                    crate::cache::pool::UringBufOwner {
                                                        ptr: dest_ptr,
                                                        len: slice_len as usize,
                                                    },
                                                )
                                            }
                                            None => match val {
                                                crate::cache::pool::ReadBlockValue::Bytes(b) => b,
                                                other => bytes::Bytes::copy_from_slice(&other),
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
                            let downloaded: Option<crate::cache::pool::ReadBlockValue> =
                                match b_key_opt {
                                    Some(b_key) => {
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
                                            Some(r) => Some(r),
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
                                                            std::ptr::copy_nonoverlapping(
                                                                val[start..end].as_ptr(),
                                                                dest_ptr,
                                                                len,
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
                                    None => None,
                                };

                            match downloaded {
                                Some(downloaded) => {
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
                                        let start =
                                            std::cmp::min(slice_start as usize, downloaded.len());
                                        let end = std::cmp::min(
                                            (slice_start + slice_len as u64) as usize,
                                            downloaded.len(),
                                        );
                                        let slice: &[u8] = &downloaded[start..end];
                                        let data = bytes::Bytes::copy_from_slice(slice);
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
                    let block_keys = self
                        .load_striped_block_keys(file_path, &meta, start_block, end_block)
                        .await?;
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
                    // single-block arm.
                    if !device_true {
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
                            } else if let Some(downloaded) = router
                                .get_block_for_index(
                                    &file_path_clone,
                                    b_idx,
                                    b_key_opt.as_deref(),
                                    device_true,
                                    true,
                                )
                                .await?
                            {
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
                        let data = bytes::Bytes::copy_from_slice(&final_buf[..final_len]);
                        return Ok((data, Some(std::sync::Arc::new(final_buf))));
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
        // racing promotion's commit aborts). StorageFull propagates:
        // never-lossy custody stays exactly where it was.
        self.cache
            .nvme
            .stage_write(&file_path, &fid, bytes::Bytes::from(img), fencing_token)
            .await?;
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
                self.cache
                    .nvme
                    .stage_write(
                        dest,
                        &new_file_id,
                        bytes::Bytes::from(data),
                        resolved_dest_token,
                    )
                    .await?;
            }
            updated_meta.file_id = Some(new_file_id);
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
                let _ = self.cache.nvme.remove_staged_async(file_id.clone()).await;
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

        // Targeted O(1) active block removals without full staging listing
        let block_size = self.block_size.load(std::sync::atomic::Ordering::Relaxed);
        let max_block = if block_size > 0 {
            (meta.size + block_size - 1) / block_size
        } else {
            0
        };
        let mut block_indices = std::collections::HashSet::new();
        for b in 0..=max_block {
            block_indices.insert(b);
        }
        if let Some(ref block_map) = meta.block_map {
            for &b in block_map.keys() {
                block_indices.insert(b as u64);
            }
        }
        // Canonical key form (`…:block_{b}`): the previous hand-rolled
        // `active_block:{path}:{b}` never matched a real entry, leaking
        // staged overlays past delete to shadow a reused inode's reads.
        // One blocking-pool hop for the whole sweep: each removal is a
        // staging shard WRITE lock (shard-lock invariant rule 2).
        let keys: Vec<String> = block_indices
            .into_iter()
            .flat_map(|b| {
                [
                    crate::keys::active_block_for_path(file_path, b as u32).to_string(),
                    crate::keys::active_block_ext_for_path(file_path, b as u32).to_string(),
                ]
            })
            .collect();
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
                .create(
                    parent_ino,
                    file_name,
                    src_inode.mode,
                    src_inode.uid,
                    src_inode.gid,
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
