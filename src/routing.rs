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
    pub block_map: Option<std::collections::HashMap<u32, String>>,
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
    /// Insert/overwrite entries: write-through, flush paths, defrag
    /// `BlockMove`, routing striped merge. `(block_idx, new_block_key)`
    /// pairs.
    ///
    /// `Merge(&[])` is the DEGENERATE, size-only case: no entries change,
    /// but the primitive still re-reads the CURRENT meta under
    /// `INODE_META_LOCKS` and saves size/map from that — which is exactly
    /// what makes the stale-snapshot whole-meta saves (truncate-grow,
    /// fallocate-extend) safe: they can no longer rewrite the block map
    /// "without mutating it".
    Merge(&'a [(u32, String)]),
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

/// Strip a stored block-map value (`proto://offset:extra` or `offset:extra`)
/// down to the free-able key (`proto://offset` / `offset`) that
/// [`BackendRouter::free_blocks`] expects. The map stores per-block extra
/// (packed length / crypto framing) after the offset; the allocator only keys
/// on the offset.
pub(crate) fn clean_block_key(bk: &str) -> String {
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
        Self {
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
        }
    }

    /// Wire the terminal-free read-tier purge (see the field doc). Called
    /// once by `DataRouter::new`; later calls are no-ops.
    pub fn set_read_tier_purge(&self, purge: std::sync::Arc<dyn Fn(&str) + Send + Sync>) {
        let _ = self.read_tier_purge.set(purge);
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

    pub fn is_backend_healthy(&self, be_id: &str) -> bool {
        if self.unhealthy_backends.contains_key(be_id) {
            false
        } else if be_id == "backend_0" {
            std::path::Path::new(&self.default_device.device_path).exists()
        } else if let Some(be) = self.backends.get(be_id) {
            std::path::Path::new(&be.device.device_path).exists()
        } else {
            false
        }
    }

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

        // Query real device capacity
        let mut dev_size = 100 * 1024 * 1024 * 1024;
        if let Ok(metadata) = std::fs::metadata(&device_path) {
            let len = metadata.len();
            if len > 0 {
                dev_size = len;
            } else if let Ok(mut file) = std::fs::File::open(&device_path) {
                use std::io::Seek;
                if let Ok(len) = file.seek(std::io::SeekFrom::End(0)) {
                    if len > 0 {
                        dev_size = len;
                    }
                }
            }
        }

        let total_blocks = (dev_size / (4 * 1024 * 1024)).max(1);
        let used_blocks = allocator.get_used_blocks();
        let free_blocks = total_blocks.saturating_sub(used_blocks);
        let free_factor = free_blocks as f64 / total_blocks as f64;

        let perf_factor = 1.0;

        let score = (free_factor * 1000.0 * perf_factor) as u32;
        score.min(1000)
    }

    pub fn get_active_backend(
        &self,
    ) -> Result<(
        String,
        std::sync::Arc<crate::block_allocator::BlockAllocator>,
        std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    )> {
        let mut healthy_backends = Vec::new();

        // The `backend_0` default slot is a placement candidate ONLY when no
        // named volume is registered (bare routers: offline tools, tests).
        // On real mounts every volume — including the first, whose
        // device/allocator ARE the default slot — is registered under its
        // real name, and the phantom must not appear in write placement,
        // health scoring, or the data-volume table. `backend_0` remains a
        // pure key-resolution ALIAS of the default slot for legacy
        // unprefixed / `backend_0://` block keys (see `parse_block_key`,
        // `get_backend`, `free_block`).
        if self.backends.is_empty() && self.is_backend_healthy("backend_0") {
            let health = self.get_backend_health("backend_0");
            healthy_backends.push((
                "backend_0".to_string(),
                self.default_allocator.clone(),
                self.default_device.clone(),
                health,
            ));
        }

        for entry in self.backends.iter() {
            let be_id = entry.key();
            if self.is_backend_healthy(be_id) {
                let health = self.get_backend_health(be_id);
                healthy_backends.push((
                    be_id.clone(),
                    entry.value().block_allocator.clone(),
                    entry.value().device.clone(),
                    health,
                ));
            }
        }

        if healthy_backends.is_empty() {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "No healthy storage backends available for write",
            )));
        }

        // Sort by health descending
        healthy_backends.sort_by(|a, b| b.3.cmp(&a.3));

        let max_health = healthy_backends[0].3;
        // Filter candidates within 90% of max health
        let candidates: Vec<_> = healthy_backends
            .into_iter()
            .filter(|b| b.3 >= (max_health * 9) / 10)
            .collect();

        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let idx = COUNTER.fetch_add(1, Ordering::Relaxed) % candidates.len();
        let selected = &candidates[idx];

        Ok((selected.0.clone(), selected.1.clone(), selected.2.clone()))
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
        let (be_id, offset) = self.parse_block_key(block_key).ok()?;
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

                // 3. Trigger failover if currently active write backend is unhealthy
                let active_be = (*router.active_write_backend.load_full()).clone();
                if !router.is_backend_healthy(&active_be) {
                    log::warn!(
                        "Active write backend '{}' is unhealthy! Initiating failover...",
                        active_be
                    );
                    // Fail over to a healthy NAMED volume; the `backend_0`
                    // default slot is a candidate only on bare routers (same
                    // policy as `get_active_backend`).
                    let mut fallback_be = None;
                    for entry in router.backends.iter() {
                        let be_id = entry.key();
                        if router.is_backend_healthy(be_id) {
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
    pub metadata_cache: moka::sync::Cache<String, CachedMetadata>,
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
    /// R1b disk-tier admission mode (env-resolved once; hot-budget-0
    /// auto-degrades SecondTouch to Always).
    pub tier_admission: TierAdmission,
    /// R3 (§5.6) ranged-read dispatch bound: requests ≤ this many bytes
    /// on passthrough, non-streaming, cache-missed striped reads fetch
    /// only their 4 KiB-aligned window (`SQUEEZEFS_READ_RANGED_THRESHOLD`,
    /// default 262144; 0 = kill switch).
    pub(crate) ranged_threshold: u64,
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
                        block_map,
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
        let file_path = crate::keys::inode_path(ino);
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
            block_map: m.block_map.clone(),
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

            let (be_id, offset, nvme_writer) = if let Some((be, off)) = reuse_info {
                let (_, dev) = self.backend_router.get_backend(&be)?;
                (be, off, dev)
            } else {
                let (be, block_allocator, dev) = self.backend_router.get_active_backend()?;
                let off = block_allocator.allocate_block().await?;
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
        self.metadata_cache.insert(file_path.to_string(), cached);

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
        let default_block_size = std::env::var("SQUEEZEFS_DEFAULT_BLOCK_SIZE")
            .ok()
            .and_then(|val| val.parse::<u64>().ok())
            .unwrap_or(4 * 1024 * 1024);
        let block_size = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(default_block_size));
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
                tier_admission,
                ranged_threshold,
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

    pub async fn read_nvme_block(&self, block_key: &str) -> Result<bytes::Bytes> {
        let size = self.block_size.load(std::sync::atomic::Ordering::Relaxed) as usize;
        self.backend_router.read_block(block_key, size).await
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
        let meta = match self.metadata_cache.get(file_path) {
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
    pub async fn get_block_for_index(
        &self,
        file_path: &str,
        b: u32,
        resolved_key: Option<&str>,
    ) -> Result<Option<crate::cache::pool::ReadBlockValue>> {
        // Each retry re-resolves against the freshest map, so consecutive
        // failures require back-to-back whole COW-rewrite cycles of this one
        // block landing inside single fetches — churn far past any real
        // workload. Exhaustion fails loud rather than serving unproven bytes.
        const MAX_REBINDS: usize = 8;
        let mut key: Option<String> = resolved_key.map(str::to_string);
        for _ in 0..MAX_REBINDS {
            let Some(cur_key) = key else {
                return Ok(None);
            };
            let (val, incarnation_valid) = self
                .get_cached_or_fetch_block_traced(&cur_key, false)
                .await?;
            // Recheck the binding only AFTER the bytes are in hand: the
            // proof needs (movement between snapshot and serve) ⇒ (word
            // changed), which only holds when the recheck follows the read.
            let current = self.current_block_binding(file_path, b).await?;
            if incarnation_valid && current.as_deref() == Some(cur_key.as_str()) {
                return Ok(Some(val));
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
    /// NEVER PUBLISHED: a partial payload must not exist under a
    /// whole-block tier key (tier/hot entries are whole-block by contract
    /// — a short entry would serve truncated bytes to a larger read).
    /// Ranged fills serve their caller only; re-read heat is RECORDED in
    /// the ghost table (never consulted for ranged dispatch — the pinned
    /// N-disjoint-reads amplification contract forbids self-escalation),
    /// so a subsequent whole-block fetch ghost-admits per §5.3 and
    /// genuinely hot ranges converge to cached whole blocks.
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
    /// byte savings.
    pub async fn get_block_range_for_index(
        &self,
        file_path: &str,
        b: u32,
        rel_range: std::ops::Range<u64>,
        resolved_key: Option<&str>,
        dest: Option<RangedDest>,
    ) -> Result<Option<crate::cache::pool::ReadBlockValue>> {
        const MAX_REBINDS: usize = 8;
        const LBA: u64 = 4096;
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
                // §5.6 heat capture: record-only (see doc comment). Only
                // meaningful under second-touch; always/never keep their
                // verbatim escape-hatch semantics.
                if self.tier_admission == TierAdmission::SecondTouch {
                    let _ = self.ghost.check_and_record(&cur_key);
                }
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
        // per-window churn — and slice the request out. With a dest the
        // slice is copied in and the tail zeroed (reused-payload replay
        // rule at the copy site).
        let whole = self
            .get_block_for_index(file_path, b, key.as_deref())
            .await?;
        Ok(whole.map(|val| {
            let start = std::cmp::min(rel_range.start as usize, val.len());
            let end = std::cmp::min(rel_range.end as usize, val.len());
            match &dest {
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
        }))
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
        if let Some(entry) = self.metadata_cache.get(file_path) {
            if fresh_or_dirty(&entry) {
                return Ok(entry.clone());
            }
        }

        let ino = parse_inode_from_path(file_path);

        // Refill under the per-inode metadata lock so a stale backend snapshot
        // can never clobber a concurrent writer's fresh cache entry (see
        // INODE_META_LOCKS). Double-check after acquiring: a writer or racing
        // filler may have refreshed (or dirtied) the entry while we waited.
        let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
        if let Some(entry) = self.metadata_cache.get(file_path) {
            if fresh_or_dirty(&entry) {
                return Ok(entry.clone());
            }
        }
        if let Some(m) = self.fetch_metadata_from_backend(ino).await? {
            self.metadata_cache.insert(file_path.to_string(), m.clone());
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
        self.metadata_cache.insert(file_path.to_string(), m.clone());
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
        let file_path = crate::keys::inode_path(ino);
        let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
        // NOTE: `fetch_metadata` would retake this lock.
        let current = match self.metadata_cache.get(&file_path) {
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
        self.metadata_cache.insert(file_path, updated);
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
        let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
        let Some(meta) = self.metadata_cache.get(file_path) else {
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
        self.metadata_cache.insert(file_path.to_string(), clean);
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
        let Some(gen) = nvme.staged_generation(file_id) else {
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

        let processed = self
            .get_crypto()
            .process_write_async(bytes::Bytes::from(raw))
            .await?;
        let (be_id, allocator, writer) = self.backend_router.get_active_backend()?;
        if processed.len() as u64 > allocator.chunk_size() {
            // Incompressible expansion past the block size: stays resident.
            return Ok(false);
        }
        let offset = allocator.allocate_block().await?;
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
            let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
            // Authoritative meta: RAM cache first (post-write truth), then
            // backend. NOTE: `fetch_metadata` would retake this lock.
            let current = match self.metadata_cache.get(file_path) {
                Some(m) => Some(m),
                None => self.fetch_metadata_from_backend(ino).await?,
            };
            let Some(current) = current else {
                return Ok::<bool, SqueezefsError>(false);
            };
            if current.file_type != "staged"
                || current.file_id.as_deref() != Some(file_id)
                || nvme.staged_generation(file_id) != Some(gen)
            {
                return Ok(false);
            }
            let mut updated = current.clone();
            let mut block_map = updated.block_map.take().unwrap_or_default();
            let displaced = block_map.insert(0, block_key.clone());
            updated.block_map = Some(block_map);
            updated.layout_dirty = false;
            updated.cached_at = std::time::Instant::now();
            self.save_metadata_to_backend(ino, &updated, fencing_token)
                .await?;
            self.metadata_cache.insert(file_path.to_string(), updated);
            if let Some(prev) = displaced {
                if prev != block_key {
                    // Re-promotion over an older durable copy: purge + free it.
                    self.cache.purge_block_key(&prev);
                    let _ = self.backend_router.free_block(&prev).await;
                }
            }
            Ok(true)
        }
        .await;

        match commit {
            Ok(true) => {
                // Blocking-pool hop: shard WRITE lock (invariant rule 2).
                let _ = nvme
                    .remove_staged_if_generation_async(file_id.to_string(), gen)
                    .await;
                Ok(true)
            }
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
        if let Some(mapping) = self.metadata_cache.get(file_path).as_ref().and_then(map0) {
            return Some(mapping);
        }
        let ino = parse_inode_from_path(file_path);
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
        if let Some(m) = self.metadata_cache.get(file_path) {
            return Some(m);
        }
        let ino = parse_inode_from_path(file_path);
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
    /// per-inode metadata lock; `old_maps` are the pre-publish snapshots
    /// (write-entry meta + last cached meta) so a promotion that landed
    /// between them cannot leak its block.
    async fn release_superseded_staged(
        &self,
        old_ring_id: Option<&str>,
        old_maps: [Option<&std::collections::HashMap<u32, String>>; 2],
        keep_block_key: Option<&str>,
    ) {
        if let Some(fid) = old_ring_id {
            // Blocking-pool hop: shard WRITE lock (invariant rule 2).
            let _ = self.cache.nvme.remove_staged_async(fid.to_string()).await;
        }
        let mut freed = std::collections::HashSet::new();
        for map in old_maps.into_iter().flatten() {
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
        let file_path = crate::keys::inode_path(ino);
        let _map_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;

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
            None => self.metadata_cache.get(&file_path).unwrap_or_default(),
        };

        let mut block_map = current.block_map.take().unwrap_or_default();
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
                if let Some(cached) = self.metadata_cache.get(&file_path) {
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
                if let Some(cached) = self.metadata_cache.get(&file_path) {
                    if cached.size > current.size {
                        current.size = cached.size;
                    }
                }
            }
        }
        current.block_map = Some(block_map);

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
        let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
        let entry = match self.metadata_cache.get(file_path) {
            Some(entry) => Some(entry),
            None => match self.fetch_metadata_from_backend(ino).await {
                Ok(Some(m)) => {
                    self.metadata_cache.insert(file_path.to_string(), m.clone());
                    Some(m)
                }
                Ok(None) | Err(_) => None,
            },
        };
        if let Some(mut entry) = entry {
            if size > entry.size {
                entry.size = size;
                entry.cached_at = std::time::Instant::now();
                self.metadata_cache.insert(file_path.to_string(), entry);
            }
        }
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
            let block_lock = crate::fuse_client::BLOCK_FLUSH_LOCKS.get_lock(ino, 0);
            Some(block_lock.lock().await)
        } else {
            None
        };

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
                            let mapping_opt = self.staged_block_mapping(file_path, &meta).await;
                            if let Some(mapping_str) = mapping_opt {
                                let mut plain =
                                    self.read_promoted_staged_block(&mapping_str).await?;
                                let (_, _, _, exact) = self.parse_block_mapping(&mapping_str)?;
                                if !exact && plain.len() as u64 > meta.size {
                                    // Bare legacy mapping: the whole-block
                                    // read's tail is another tenant's device
                                    // garbage a passthrough transform cannot
                                    // strip. Bound by `meta.size` — safe on
                                    // THIS leg only, because every event
                                    // that publishes/clips a durable staged
                                    // mapping (promotion, spill, truncate
                                    // clip) persists the size in the same
                                    // commit, so a ring-miss image never
                                    // legitimately exceeds it (unlike the
                                    // ring blob, whose size may lag — the
                                    // pinned truncate_down_stale_size
                                    // contract).
                                    plain = plain.slice(0..meta.size as usize);
                                }
                                existing_data.resize(plain.len(), 0);
                                existing_data.copy_from_slice(&plain);
                            }
                        }
                    }
                }
                _ => {}
            }
        };

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

            let block_mappings = self.durable_write_sparse_blocks(chunks).await?;

            let mut block_map = std::collections::HashMap::new();
            for (idx, key) in block_mappings {
                block_map.insert(idx, key);
            }

            {
                let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
                let fresh = self.metadata_cache.get(file_path);
                let mut updated_meta = meta.clone();
                updated_meta.file_type = "striped".to_string();
                updated_meta.size = new_size as u64;
                updated_meta.block_map = Some(block_map);
                updated_meta.file_id = None;
                self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                    .await?;

                self.cache.write_lru.remove(file_path);
                self.cache.read_lru.remove(file_path);

                self.metadata_cache
                    .insert(file_path.to_string(), updated_meta);
                // The staged form is superseded: release its ring entry
                // (budget) and any promoted/spilled durable copy.
                self.release_superseded_staged(
                    meta.file_id.as_deref(),
                    [
                        meta.block_map.as_ref(),
                        fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                    ],
                    None,
                )
                .await;
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

            let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
            let fresh = self.metadata_cache.get(file_path);
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
            self.metadata_cache
                .insert(file_path.to_string(), updated_meta);
            // A truncated-then-rewritten staged/spilled file leaves a ring
            // entry and/or a durable copy behind: release them.
            self.release_superseded_staged(
                meta.file_id.as_deref(),
                [
                    meta.block_map.as_ref(),
                    fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                ],
                None,
            )
            .await;
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
                    let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
                    let fresh = self.metadata_cache.get(file_path);
                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".to_string();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(new_file_id);
                    updated_meta.data_key = None;
                    updated_meta.block_map = None;
                    updated_meta.layout_dirty = true;
                    updated_meta.cached_at = std::time::Instant::now();
                    self.metadata_cache
                        .insert(file_path.to_string(), updated_meta);
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
                    // copy of older content. The ring entry itself is the
                    // fresh data (replaced in-place by stage_write) — keep it.
                    self.release_superseded_staged(
                        None,
                        [
                            meta.block_map.as_ref(),
                            fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                        ],
                        None,
                    )
                    .await;
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
                    let be_offset = block_allocator.allocate_block().await?;
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

                    let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
                    let fresh = self.metadata_cache.get(file_path);
                    let mut updated_meta = meta.clone();
                    updated_meta.file_type = "staged".to_string();
                    updated_meta.size = new_size as u64;
                    updated_meta.file_id = Some(spill_file_id);
                    updated_meta.data_key = None;
                    updated_meta.block_map = Some(block_map);
                    // Durable backend write already happened — commit layout now.
                    updated_meta.layout_dirty = false;
                    self.save_metadata_to_backend(ino, &updated_meta, fencing_token)
                        .await?;
                    self.metadata_cache
                        .insert(file_path.to_string(), updated_meta);
                    // Release the superseded stale ring entry (returns its
                    // budget) and any older durable copy it had.
                    self.release_superseded_staged(
                        meta.file_id.as_deref(),
                        [
                            meta.block_map.as_ref(),
                            fresh.as_ref().and_then(|f| f.block_map.as_ref()),
                        ],
                        Some(&stored_block_key),
                    )
                    .await;
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
    async fn durable_write_sparse_blocks(
        &self,
        chunks: Vec<(u32, bytes::Bytes)>,
    ) -> Result<Vec<(u32, String)>> {
        let mut block_mappings: Vec<(u32, String)> = Vec::new();
        let mut allocated_keys: Vec<String> = Vec::new();

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

            let processed = match self.get_crypto().process_write_async(chunk.clone()).await {
                Ok(p) => p,
                Err(e) => {
                    for k in &allocated_keys {
                        let _ = self.backend_router.free_block(k).await;
                    }
                    return Err(e);
                }
            };

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

        Ok(block_mappings)
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
                            .get_block_for_index(&file_path_clone, b, old_block_key.as_deref())
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
                let stored_new_block_key = router_clone
                    .backend_router
                    .persist_block_key(&be_id, offset);

                let processed_block = crypto.process_write_async(block_bytes.clone()).await?;
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

                Ok::<_, SqueezefsError>((b, stored_new_block_key))
            }));
        }

        let mut results = Vec::new();
        let mut first_err: Option<SqueezefsError> = None;
        let mut tasks_stream = tasks;
        while let Some(task_res) = tasks_stream.next().await {
            match task_res.map_err(|e| {
                SqueezefsError::Io(std::io::Error::other(format!(
                    "Block write task panicked: {:?}",
                    e
                )))
            }) {
                Ok(Ok(res)) => results.push(res),
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
                    if self.cache.nvme.staged_generation(&file_id).is_some() {
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
                        // §5.5 pipeline driver — once per request, before
                        // the serve probes: consume bookkeeping, growth on
                        // foreground-wait (key already in the single-
                        // flight = the reader caught the pipeline), and
                        // the windowed top-up.
                        {
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
                            if let Some(ref b_key) = b_key_opt {
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
                            if let Some(ref b_key) = b_key_opt {
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
                            // 4 KiB-aligned window. Never published, never
                            // single-flighted; full fill discipline inside
                            // the primitive. Hole ⇒ zeros, same as the
                            // whole-block arm below.
                            if b_key_opt.is_some()
                                && self.ranged_eligible(file_path, slice_len as u64)
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
                                                let val = self
                                                    .get_block_for_index(
                                                        file_path,
                                                        start_block,
                                                        Some(b_key),
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
                    // §5.5: multi-block reads advance the pipeline past
                    // end_block (consume span + top-up). The consume-time
                    // detector probes the first block's key like the
                    // single-block arm.
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
                                && router.ranged_eligible(&file_path_clone, copy_len as u64)
                            {
                                // R3 (§5.6), multi-block per-block leg: this
                                // block's slice is small/passthrough/non-
                                // streaming — fetch only its window (bounce
                                // shape: the assembled dest region is not
                                // guaranteed 4 KiB-aligned per block). Same
                                // fill discipline inside the primitive;
                                // Ok(None) = hole ⇒ the zero-fill below.
                                match router
                                    .get_block_range_for_index(
                                        &file_path_clone,
                                        b_idx,
                                        rel_start as u64..(rel_start + copy_len) as u64,
                                        b_key_opt.as_deref(),
                                        None,
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
                                .get_block_for_index(&file_path_clone, b_idx, b_key_opt.as_deref())
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
            if let Some(data) = self.cache.nvme.read_staged(file_id) {
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
            // All-or-nothing pin of every source block. A refusal means the
            // snapshot map is stale (a block was freed/displaced since the
            // fetch): undo partial pins, re-read the authoritative map under
            // the per-inode metadata lock, and retry. Never proceed with an
            // unpinned block — the clone would alias a reallocatable offset
            // that reads as foreign bytes after reuse.
            let src_ino = parse_inode_from_path(src);
            let mut current = meta.clone();
            let mut attempt = 0usize;
            loop {
                let map = current.block_map.clone().unwrap_or_default();
                let mut pinned: Vec<&String> = Vec::with_capacity(map.len());
                let mut refused = None;
                for bk in map.values() {
                    if self.backend_router.increment_refcount(bk) {
                        pinned.push(bk);
                    } else {
                        refused = Some(bk.clone());
                        break;
                    }
                }
                match refused {
                    None => {
                        updated_meta = current;
                        break;
                    }
                    Some(bad) => {
                        // Undo the partial pins (free_block = one decrement).
                        for bk in pinned {
                            let _ = self.backend_router.free_block(bk).await;
                        }
                        attempt += 1;
                        if attempt >= 3 {
                            return Err(SqueezefsError::InvalidOperation(format!(
                                "clone source {src} block {bad} freed concurrently \
                                 (map still stale after {attempt} attempts); aborting \
                                 to avoid an unpinned clone"
                            )));
                        }
                        let _meta_guard = INODE_META_LOCKS.get_inode_lock(src_ino).lock().await;
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

        self.metadata_cache.insert(dest.to_string(), updated_meta);
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
            self.metadata_cache.insert(file_path.clone(), meta);
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

        // Phase 2 (unlocked, data I/O before the meta flip — P0 layout
        // atomicity): on a genuine shrink, a promoted/spilled durable whole
        // image longer than new_size must be clip-rewritten. Failure is
        // LOUD (`?`): a truncate that cannot prove the durable tail is gone
        // must fail the SETATTR, never silently leave resurrection bait.
        // (`new_size == 0` needs no clip: the commit's prune drops
        // `block_map[0]` entirely — block start 0 >= 0.)
        let mut clipped_bk: Option<(String, String)> = None; // (old, new)
        if new_size < pre_size && new_size > 0 {
            if let Some(old_bk) = self.staged_block_mapping(&file_path, &meta).await {
                // The mapping can be displaced under our feet by a racing
                // re-promotion (merge worker — not FUSE-serialized) freeing
                // `old_bk`: a failed read re-resolves the freshest binding
                // once and retries; an error on a STABLE binding is real.
                let mut old_bk = old_bk;
                let img = match self.read_promoted_staged_block(&old_bk).await {
                    Ok(img) => Some(img),
                    Err(e) => {
                        let fresh = self.freshest_layout_identity(&file_path).await;
                        let fresh_bk = match fresh {
                            Some(ref f) if f.file_type == "staged" => {
                                f.block_map.as_ref().and_then(|bm| bm.get(&0).cloned())
                            }
                            _ => None,
                        };
                        match fresh_bk {
                            Some(bk) if bk != old_bk => {
                                old_bk = bk;
                                Some(self.read_promoted_staged_block(&old_bk).await?)
                            }
                            Some(_) => return Err(e),
                            None => None,
                        }
                    }
                };
                if let Some(img) = img {
                    if img.len() as u64 > new_size {
                        let clipped = img.slice(0..new_size as usize);
                        let processed = self.get_crypto().process_write_async(clipped).await?;
                        let (be_id, allocator, writer) =
                            self.backend_router.get_active_backend()?;
                        let offset = allocator.allocate_block().await?;
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
            let _meta_guard = INODE_META_LOCKS.get_inode_lock(ino).lock().await;
            // NOTE: `fetch_metadata` would retake this lock.
            let current = match self.metadata_cache.get(&file_path) {
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
                        if bm.get(&0) == Some(old_bk) {
                            bm.insert(0, new_bk.clone());
                            blocks_to_free.push(old_bk.clone());
                            published = true;
                        }
                    }
                }
                // Prune whole blocks at/after new_size (drops `block_map[0]`
                // itself on truncate-to-zero).
                if let Some(ref mut bm) = updated.block_map {
                    let block_size = self.block_size.load(Ordering::Relaxed);
                    bm.retain(|&b, bk| {
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
            self.metadata_cache.insert(file_path.clone(), updated);

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
            .map(|b| crate::keys::active_block_for_path(file_path, b as u32).to_string())
            .collect();
        let _ = self.cache.nvme.remove_active_blocks_async(keys).await;

        self.cache.write_lru.remove(file_path);
        self.cache.read_lru.remove(file_path);
        self.metadata_cache.invalidate(file_path);
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
