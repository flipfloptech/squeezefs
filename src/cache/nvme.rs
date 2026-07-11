use crate::error::{Result, SqueezefsError};
use bytes::Bytes;
use log::{error, info};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use xxhash_rust::xxh3::xxh3_64;

/// Marker file binding a staging dir to the filesystem generation
/// (`meta_backend::volume_set_generation`) it was populated by. Lives at
/// the root of every staging dir handed to [`NvmeStaging::new`].
pub const STAGING_GENERATION_MARKER: &str = ".squeezefs_generation";

/// Marker header line: versions the marker format itself, so a future
/// identity-scheme change can re-stamp instead of misparsing.
const STAGING_GENERATION_HEADER: &str = "squeezefs-staging-generation-v1";

/// Full marker file image for `fs_generation`.
fn generation_marker_content(fs_generation: &str) -> Vec<u8> {
    format!("{STAGING_GENERATION_HEADER}\n{fs_generation}\n").into_bytes()
}

/// Remove every regular file directly inside `dir` (segment files; the dir
/// itself and any nested dirs/symlinks stay). Follows `dir` when it is the
/// mount layout's `cache_segment -> ../cache_segment` symlink — stale
/// offset-keyed read-cache blocks are exactly as poisonous as stale staging
/// segments. Returns `(files_removed, bytes_removed)`; a missing dir is
/// zero work. Directory enumeration/unlink are std::fs by design — the
/// uring-fs exclusion list ("directory create/remove metadata").
fn wipe_segment_files(dir: &std::path::Path) -> std::io::Result<(usize, u64)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            bytes += meta.len();
            fs::remove_file(entry.path())?;
            files += 1;
        }
    }
    Ok((files, bytes))
}

/// Remove stale GDS read-cache materializations (`*.gds_cache`) at the
/// staging-dir root — same offset/object-keyed poisoning class as the
/// segment files.
fn wipe_gds_cache_files(dir: &std::path::Path) -> std::io::Result<(usize, u64)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e),
    };
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        let is_gds = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.ends_with(".gds_cache"));
        if !is_gds {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            bytes += meta.len();
            fs::remove_file(entry.path())?;
            files += 1;
        }
    }
    Ok((files, bytes))
}

/// Bind `dir` to `fs_generation` BEFORE any segment is mapped, recovered,
/// or budget-seeded (the reformat-over-stale-staging fix):
///
/// - marker matches ⇒ same generation, keep everything (warm restart);
/// - marker missing/unreadable/mismatched with NO segment data ⇒ stamp;
/// - marker missing/unreadable/mismatched WITH segment data ⇒ the content
///   belongs to a DEAD filesystem generation (or predates generation
///   stamping): discard it — wipe segment files in `cache_segment/` and
///   `staging_segment/` plus `*.gds_cache` materializations — with ONE
///   loud log line, bump `staging_generation_discards`, then stamp.
///
/// Marker I/O is io_uring (`crate::uring_fs`); the stamp is fdatasync'd so
/// a fresh generation is never adopted volatile.
async fn bind_staging_generation(dir: &std::path::Path, fs_generation: &str) -> Result<()> {
    let marker_path = dir.join(STAGING_GENERATION_MARKER);
    let expected = generation_marker_content(fs_generation);

    let found = crate::uring_fs::read_all(&marker_path).await.ok();
    if found.as_deref() == Some(expected.as_slice()) {
        return Ok(());
    }

    let cache_dir = dir.join("cache_segment");
    let staging_dir = dir.join("staging_segment");
    let has_data = dir_has_segment_data(&cache_dir) || dir_has_segment_data(&staging_dir);
    if has_data {
        let (cf, cb) = wipe_segment_files(&cache_dir)?;
        let (sf, sb) = wipe_segment_files(&staging_dir)?;
        let (gf, gb) = wipe_gds_cache_files(dir)?;
        crate::fuse_client::METRICS
            .staging_generation_discards
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let found_desc = match &found {
            Some(bytes) => format!(
                "marker {:?}",
                String::from_utf8_lossy(bytes).replace('\n', "\\n")
            ),
            None => "no readable generation marker".to_string(),
        };
        let msg = format!(
            "STAGING GENERATION MISMATCH at {}: discarding staging content from a dead \
             filesystem generation ({found_desc}, mounted generation \"{fs_generation}\") — \
             removed {sf} staging segment file(s) ({sb} bytes), {cf} read-cache segment \
             file(s) ({cb} bytes), {gf} gds cache file(s) ({gb} bytes); staged writes and \
             active blocks stamped by the old generation are gone by design (reformat \
             discards data)",
            dir.display(),
        );
        // Loud at DEFAULT verbosity: the daemon's env_logger default filter
        // is ERROR, so a warn alone is invisible on a stock mount. Console
        // line (stderr → daemon log / terminal) + the structured record.
        eprintln!("{msg}");
        log::warn!("{msg}");
    }

    crate::uring_fs::write_all(&marker_path, expected).await?;
    crate::uring_fs::fdatasync(&marker_path).await?;
    Ok(())
}

pub fn dir_has_segment_data(path: &std::path::Path) -> bool {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        if metadata.is_file() && metadata.len() > 0 {
            return true;
        }
    }

    false
}

static SAFEGUARD_CACHE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static LAST_CHECK_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn check_disk_free_safeguard(path: &std::path::Path) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let last = LAST_CHECK_TIME.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 1 {
        return SAFEGUARD_CACHE.load(std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let abs_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Ok(c_path) = CString::new(abs_path.as_os_str().as_bytes()) {
            unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 && stat.f_blocks > 0 {
                    let free_fraction = stat.f_bavail as f64 / stat.f_blocks as f64;
                    let free_bytes = stat.f_bavail as u64 * stat.f_frsize as u64;
                    let safe = !(free_bytes < 100 * 1024 * 1024
                        || (free_fraction < 0.01 && free_bytes < 1024 * 1024 * 1024));
                    SAFEGUARD_CACHE.store(safe, std::sync::atomic::Ordering::Relaxed);
                    LAST_CHECK_TIME.store(now, std::sync::atomic::Ordering::Relaxed);
                    return safe;
                }
            }
        }
    }
    true
}

/// Binary header for staged / active-block payloads on local NVMe staging.
/// Shared with crash recovery (`recovery::recover_staging`) — must stay stable.
#[derive(Clone, Debug)]
pub struct StagedMetadata {
    pub fencing_token: u64,
    pub original_size: u64,
    pub file_path: String,
}

impl StagedMetadata {
    pub fn serialize(&self) -> Vec<u8> {
        let path_bytes = self.file_path.as_bytes();
        let mut buf = Vec::with_capacity(20 + path_bytes.len());
        buf.extend_from_slice(&self.fencing_token.to_be_bytes());
        buf.extend_from_slice(&self.original_size.to_be_bytes());
        buf.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(path_bytes);
        buf
    }

    pub fn deserialize(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 20 {
            return None;
        }
        let fencing_token = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
        let original_size = u64::from_be_bytes(bytes[8..16].try_into().ok()?);
        let path_len = u32::from_be_bytes(bytes[16..20].try_into().ok()?) as usize;
        if bytes.len() < 20 + path_len {
            return None;
        }
        let file_path = String::from_utf8(bytes[20..20 + path_len].to_vec()).ok()?;
        Some(Self {
            fencing_token,
            original_size,
            file_path,
        })
    }
}

/// Parse a packed staging blob: `[meta_len:u64][meta bytes][payload…]`.
/// Active blocks pad the header to 4 KiB; ordinary staged files do not.
///
/// Returns `None` on malformed / hostile lengths (no panics on overflow — P2-13).
pub fn parse_staged_blob(bytes: &[u8], is_active_block: bool) -> Option<(StagedMetadata, Vec<u8>)> {
    if bytes.len() < 8 {
        return None;
    }
    let meta_len = usize::try_from(u64::from_be_bytes(bytes[0..8].try_into().ok()?)).ok()?;
    let meta_end = 8usize.checked_add(meta_len)?;
    if bytes.len() < meta_end {
        return None;
    }
    let meta = StagedMetadata::deserialize(&bytes[8..meta_end])?;
    let data_start = if is_active_block { 4096usize } else { meta_end };
    let payload_len = usize::try_from(meta.original_size).ok()?;
    let data_end = data_start.checked_add(payload_len)?;
    if bytes.len() < data_end {
        return None;
    }
    Some((meta, bytes[data_start..data_end].to_vec()))
}

/// §5.5 write-only, guard-backed DMA source over one staged payload
/// (zero-copy write-path design, PR 3).
///
/// Wraps the staging shard's read guard so a flush can DMA the payload
/// straight off the staging mmap (`bytes::Bytes::from_owner`, no copy).
/// Active-block values are packed at a 4 KiB boundary with whole-block
/// length, so the source qualifies for `nvme_dev::write_block`'s zero-copy
/// `WriteData::Aligned` branch by construction (PR 2 contract).
///
/// Two §5.5 rules are enforced structurally:
///
/// - **Consumed by value, deliberately non-`Clone`**: the only sink is
///   [`write_block_from_staging`], which guarantees the guard is dead when
///   it returns — retention past the DMA (and therefore holding the shard
///   read lock into a same-shard write-lock op such as
///   `NvmeStaging::remove_active_block`, a parking_lot read→write
///   self-deadlock) is a compile error, not a review item.
///
/// ```compile_fail
/// # fn keep_a_copy(source: squeezefs::cache::nvme::StagedDmaSource) {
/// // §5.5: the source is deliberately non-Clone — a second guard-backed
/// // handle could outlive the DMA and reach a cache.
/// let _second: squeezefs::cache::nvme::StagedDmaSource = source.clone();
/// # }
/// ```
///
/// ```compile_fail
/// # async fn retain_past_dma(
/// #     crypto: &squeezefs::crypto_compress::CryptoCompressState,
/// #     dev: &squeezefs::nvme_dev::NvmeBlockDev,
/// #     source: squeezefs::cache::nvme::StagedDmaSource,
/// # ) {
/// squeezefs::cache::nvme::write_block_from_staging(crypto, dev, 0, source)
///     .await
///     .unwrap();
/// // §5.5: retention past the DMA is a compile error (moved value).
/// let _still_alive = source.len();
/// # }
/// ```
///
/// - **A guard-backed `Bytes` never enters any cache**: an LRU entry has
///   unbounded lifetime and would hold the shard read-locked until
///   eviction. Cache-bound consumers (the not-yet-striped promotion put)
///   must use [`StagedDmaSource::detached_copy`] — a bounded real copy on a
///   cold path.
pub struct StagedDmaSource {
    guard: crate::tiering::nvme::NvmeCacheReadGuard,
}

impl StagedDmaSource {
    /// Payload length (the staged entry's `original_size`).
    pub fn len(&self) -> usize {
        self.guard.len
    }

    pub fn is_empty(&self) -> bool {
        self.guard.len == 0
    }

    /// Bounded REAL copy, detached from the guard — the only sanctioned way
    /// for staged bytes to outlive the source (§5.5: read-LRU promotion
    /// seeds; never the guard-backed memory itself).
    pub fn detached_copy(&self) -> Bytes {
        Bytes::copy_from_slice(&self.guard)
    }

    /// Convert into the guard-backed `Bytes` for the DMA submit. Private to
    /// this module on purpose: only [`write_block_from_staging`] may
    /// materialize a clonable guard-backed handle, and it provably drops it
    /// before returning.
    fn into_bytes(self) -> Bytes {
        Bytes::from_owner(self)
    }
}

impl AsRef<[u8]> for StagedDmaSource {
    fn as_ref(&self) -> &[u8] {
        &self.guard
    }
}

/// Process (crypto/compress) and DMA one staged payload to `offset` on the
/// block backend, consuming the write-only `source` by value (§5.5).
///
/// Normative sequencing, encoded here once so every flush caller inherits
/// it:
///
/// - **Passthrough** (default): the DMA reads straight off the staging mmap
///   — the guard-backed `Bytes` moves into `write_block`, which keeps it
///   alive through completion *and* the sampled `--write-verification`
///   read-back, then drops it before returning. (The uring worker's
///   keep-alive clone is released on its own thread at CQE handling,
///   independent of any shard lock, so it can only delay — never deadlock —
///   a subsequent shard writer.)
/// - **Non-passthrough**: `process_write_async` consumes the guard-backed
///   `Bytes` and returns a fresh transform buffer — the guard is dead
///   *before* the DMA is submitted.
///
/// Either way the staging-shard read guard is provably dead when this
/// returns: callers may then merge metadata and take same-shard write locks
/// (`remove_active_block`) without self-deadlock, and no guard-backed
/// `Bytes` can leak into a cache. The shard read lock is therefore held
/// across exactly one transform-or-DMA (plus the sampled read-back verify
/// on `--write-verification` mounts) — the §5.5 hold bound.
pub async fn write_block_from_staging(
    crypto: &crate::crypto_compress::CryptoCompressState,
    writer: &crate::nvme_dev::NvmeBlockDev,
    offset: u64,
    source: StagedDmaSource,
) -> Result<()> {
    let processed = crypto.process_write_async(source.into_bytes()).await?;
    writer.write_block(offset, processed).await
}

#[derive(Clone)]
pub struct NvmeStaging {
    staging_dirs: Vec<PathBuf>,
    max_write_bytes: u64,
    max_read_bytes: u64,
    pub block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
    pub nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
    pub backend_router:
        std::sync::Arc<once_cell::sync::OnceCell<std::sync::Arc<crate::routing::BackendRouter>>>,
    redis_client: std::sync::Arc<crate::dlm::MetaClient>,
    /// Bounded merge-queue sender (P1-1). Full → StorageFull / backpressure.
    write_tx: mpsc::Sender<PendingStagedWrite>,
    pub p2p_addr: std::sync::Arc<std::sync::OnceLock<String>>,
    pub current_staged_write_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Budget ledger: staged `file_id` → (bytes counted in
    /// `current_staged_write_bytes`, stage generation). Every add to the
    /// gauge records its cost here; every ring-entry removal credits the
    /// gauge through here — the gauge can never ratchet. The generation
    /// lets the merge worker skip crediting an entry that a racing
    /// re-stage has already replaced (the newer stage owns the budget).
    staged_ledger: std::sync::Arc<scc::HashMap<String, (u64, u64)>>,
    /// Router hook for the merge worker: promotion must commit layout through
    /// `DataRouter` (RAM metadata cache + backend coherently, under the
    /// per-inode metadata lock). Weak — the router owns this cache.
    data_router:
        std::sync::Arc<std::sync::OnceLock<std::sync::Weak<crate::routing::DataRouterInner>>>,
    pub space_freed_notify: std::sync::Arc<tokio::sync::Notify>,
    pub staged_writes_in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub staged_drained_notify: std::sync::Arc<tokio::sync::Notify>,

    // Hypertier NVMe cache instances
    pub read_nvme_cache: std::sync::Arc<crate::tiering::nvme::NvmeCache>,
    pub staging_nvme_cache: std::sync::Arc<crate::tiering::nvme::NvmeCache>,
    pub dht_node: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<crate::tiering::dht::DhtNode>>>,
    pub crypto: std::sync::Arc<std::sync::OnceLock<crate::crypto_compress::CryptoCompressState>>,
}

#[derive(Debug, Clone)]
pub struct PendingStagedWrite {
    pub file_path: String,
    pub file_id: String,
    pub fencing_token: u64,
    pub padded_size: u64,
}

impl NvmeStaging {
    /// `fs_generation` binds every staging dir to the mounted filesystem
    /// generation (`meta_backend::volume_set_generation`): the generation
    /// gate runs FIRST, so segment recovery, index rebuild, and
    /// budget/ledger seeding below only ever see same-generation content.
    /// `None` is for offline tooling with no metadata volume set (`clone`):
    /// existing staging is adopted untouched and never stamped.
    pub async fn new(
        staging_dirs: Vec<PathBuf>,
        max_write_bytes: u64,
        max_read_bytes: u64,
        block_allocator: std::sync::Arc<crate::block_allocator::BlockAllocator>,
        nvme_writer: std::sync::Arc<crate::nvme_dev::NvmeBlockDev>,
        redis_client: std::sync::Arc<crate::dlm::MetaClient>,
        fs_generation: Option<&str>,
    ) -> Result<Self> {
        // Cache-less filesystem (format declared NO --disk-cache-paths):
        // the ring objects still exist (anonymous memory shards) so every
        // hot-path type stays non-optional, but nothing is ever admitted —
        // `stage_write` fails loud, `put_active_block` refuses, and
        // `cache_read_block` is a no-op (see those methods). Clamp the
        // never-used anonymous maps to one tiny shard instead of letting
        // the disk-sized budgets reserve gigabytes of address space.
        let (max_write_bytes, max_read_bytes) = if staging_dirs.is_empty() {
            (1024 * 1024, 1024 * 1024)
        } else {
            (max_write_bytes, max_read_bytes)
        };

        // Generation gate (reformat-over-stale-staging fix): validate the
        // marker and discard dead-generation content BEFORE any segment is
        // scanned, mapped, or recovered.
        if let Some(fs_generation) = fs_generation {
            for dir in &staging_dirs {
                fs::create_dir_all(dir)?;
                bind_staging_generation(dir, fs_generation).await?;
            }
        }

        // Initialize directories for segments.
        //
        // Read-cache segments come up COLD on purpose: the read cache is
        // keyed by bare block keys whose offsets are freed and REUSED
        // across sessions, and there is no cross-session incarnation store
        // to validate a recovered entry against (the fill-time seqlock
        // only guards live fills). A resurrected entry under a reused key
        // serves the PREVIOUS incarnation's bytes — the crash-recovery
        // twin of the reused-key stale-fill family. A cache is
        // reconstructible; stale-poisonable state is not worth recovering:
        // wipe it, never index it. The STAGING segments are the opposite —
        // sole copy of dirty data under identity-stable keys (uuid
        // file_ids, inode-keyed overlays) — and are recovered below.
        let mut read_cache_dirs = Vec::new();
        let mut staging_segment_dirs = Vec::new();
        let mut staging_segment_dirs_have_data = Vec::new();
        for dir in &staging_dirs {
            let rc_dir = dir.join("cache_segment");
            let ss_dir = dir.join("staging_segment");
            let ss_has_data = dir_has_segment_data(&ss_dir);
            let (rc_files, rc_bytes) = wipe_segment_files(&rc_dir)?;
            if rc_files > 0 {
                log::info!(
                    "staging init: discarded {rc_files} read-cache segment file(s) \
                     ({rc_bytes} B) from a previous session — read cache starts cold \
                     (block-key offsets are not incarnation-stable across mounts)"
                );
            }
            fs::create_dir_all(&rc_dir)?;
            fs::create_dir_all(&ss_dir)?;
            read_cache_dirs.push(rc_dir);
            staging_segment_dirs.push(ss_dir);
            staging_segment_dirs_have_data.push(ss_has_data);
        }

        // Process parallelism, not the (possibly core-pinned) constructor
        // thread's mask — the Hang-1 sizing poison (see `crate::cpu`).
        let cores = crate::cpu::process_parallelism();
        let default_shards = std::cmp::max(cores.next_power_of_two(), 16);

        let (read_shards, actual_max_read_bytes) = if max_read_bytes < 10 * 1024 * 1024 {
            (1, max_read_bytes)
        } else {
            #[cfg(test)]
            {
                let mut shards = default_shards;
                while shards > 16 && max_read_bytes / (shards as u64) < 4 * 1024 * 1024 {
                    shards /= 2;
                }
                (shards, max_read_bytes)
            }
            #[cfg(not(test))]
            {
                let min_required = (default_shards * 4 * 1024 * 1024) as u64;
                (default_shards, std::cmp::max(max_read_bytes, min_required))
            }
        };

        let read_cache_dirs_refs: Vec<&std::path::Path> =
            read_cache_dirs.iter().map(|p| p.as_path()).collect();
        let read_capacities = if staging_dirs.is_empty() {
            vec![actual_max_read_bytes as usize]
        } else {
            let read_cap = actual_max_read_bytes as usize / staging_dirs.len();
            vec![read_cap; staging_dirs.len()]
        };
        let read_nvme_cache = Arc::new(crate::tiering::nvme::NvmeCache::new(
            &read_cache_dirs_refs,
            &read_capacities,
            read_shards,
        )?);

        let (write_shards, actual_max_write_bytes) = if max_write_bytes < 10 * 1024 * 1024 {
            (1, max_write_bytes)
        } else {
            #[cfg(test)]
            {
                let mut shards = default_shards;
                while shards > 16 && max_write_bytes / (shards as u64) < 4 * 1024 * 1024 {
                    shards /= 2;
                }
                (shards, max_write_bytes)
            }
            #[cfg(not(test))]
            {
                let min_required = (default_shards * 4 * 1024 * 1024) as u64;
                (default_shards, std::cmp::max(max_write_bytes, min_required))
            }
        };

        let staging_dirs_refs: Vec<&std::path::Path> =
            staging_segment_dirs.iter().map(|p| p.as_path()).collect();
        let write_capacities = if staging_dirs.is_empty() {
            vec![actual_max_write_bytes as usize]
        } else {
            let write_cap = actual_max_write_bytes as usize / staging_dirs.len();
            vec![write_cap; staging_dirs.len()]
        };
        let staging_nvme_cache = Arc::new(crate::tiering::nvme::NvmeCache::new(
            &staging_dirs_refs,
            &write_capacities,
            write_shards,
        )?);

        // Recover the STAGING index only when segment data existed before
        // this startup (read-cache segments were wiped above — see the
        // init comment: never recover a cache under non-incarnation-stable
        // keys).
        if staging_segment_dirs_have_data
            .iter()
            .any(|has_data| *has_data)
        {
            staging_nvme_cache.recover_index();
        }

        // Seed the staged-write budget from recovered *staged* entries only.
        // Orphan active blocks (crash leftovers; uploaded+removed by later
        // flushes or unlink) must not consume the staged budget, or a mount
        // over a dirty segment starts with the admission gate already pinned.
        let staged_ledger: std::sync::Arc<scc::HashMap<String, (u64, u64)>> =
            std::sync::Arc::new(scc::HashMap::new());
        let mut initial_write_bytes = 0u64;
        for key in staging_nvme_cache.list_keys() {
            if key.starts_with(b"active_block:") {
                continue;
            }
            let Ok(file_id) = std::str::from_utf8(&key) else {
                continue;
            };
            if let Some(guard) = staging_nvme_cache.get(&key) {
                let cost = guard.len as u64;
                initial_write_bytes += cost;
                let _ = staged_ledger.insert_sync(file_id.to_string(), (cost, 0));
            }
        }

        // P1-1: bound the merge worker queue to avoid unbounded RAM growth under write storms.
        const STAGING_MERGE_QUEUE_CAP: usize = 1024;
        let (write_tx, write_rx) = mpsc::channel::<PendingStagedWrite>(STAGING_MERGE_QUEUE_CAP);

        let backend_router = std::sync::Arc::new(once_cell::sync::OnceCell::new());

        let staging = Self {
            staging_dirs: staging_dirs.clone(),
            max_write_bytes: actual_max_write_bytes,
            max_read_bytes: actual_max_read_bytes,
            block_allocator: block_allocator.clone(),
            nvme_writer: nvme_writer.clone(),
            backend_router,
            redis_client: redis_client.clone(),
            write_tx,
            p2p_addr: std::sync::Arc::new(std::sync::OnceLock::new()),
            current_staged_write_bytes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                initial_write_bytes,
            )),
            staged_ledger,
            data_router: std::sync::Arc::new(std::sync::OnceLock::new()),
            space_freed_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            staged_writes_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                staging_nvme_cache.list_keys().len(),
            )),
            staged_drained_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            read_nvme_cache,
            staging_nvme_cache,
            dht_node: std::sync::Arc::new(std::sync::OnceLock::new()),
            crypto: std::sync::Arc::new(std::sync::OnceLock::new()),
        };

        // Spawn background merge worker
        staging.start_merge_worker(write_rx);

        Ok(staging)
    }

    pub fn set_backend_router(&self, router: std::sync::Arc<crate::routing::BackendRouter>) {
        let _ = self.backend_router.set(router);
    }

    /// Late-bind the owning `DataRouter` (weak) for merge-worker promotion.
    pub(crate) fn set_data_router(&self, router: std::sync::Weak<crate::routing::DataRouterInner>) {
        let _ = self.data_router.set(router);
    }

    pub fn redis_client(&self) -> &std::sync::Arc<crate::dlm::MetaClient> {
        &self.redis_client
    }

    pub fn get_staged_path(&self, _file_id: &str) -> PathBuf {
        self.staging_dirs.first().cloned().unwrap_or_default()
    }

    /// Stage a write locally into staging_nvme_cache using zero-copy memory-mapped segments.
    ///
    /// `data` is owned (`Bytes`, refcounted — no payload copy) because the
    /// ring write takes the staging shard WRITE lock, which must run on the
    /// blocking pool (shard-lock invariant rule 2, `tiering::nvme::NvmeShard`):
    /// a shard writer legitimately waits for §5.5 read guards held across
    /// awaits, so acquiring it on an async executor thread can park the very
    /// thread that must poll the guard holder (the Hang-1 wedge shape).
    pub async fn stage_write(
        &self,
        file_path: &str,
        file_id: &str,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        let meta = StagedMetadata {
            fencing_token,
            original_size: data.len() as u64,
            file_path: file_path.to_string(),
        };
        let meta_bytes = meta.serialize();
        let meta_len = meta_bytes.len() as u64;
        let unpadded_len = 8 + meta_bytes.len() + data.len();
        let padded_size = unpadded_len as u64;

        let target_dir = self.staging_dirs.first().ok_or_else(|| {
            SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "no staging directories declared at format (cache-less filesystem)",
            ))
        })?;

        if !check_disk_free_safeguard(target_dir) {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "Local NVMe staging disk free space safeguard triggered (< 1% or < 100MB free)"
                    .to_string(),
            )));
        }

        // Re-staging the same file_id replaces its ring entry: the gate must
        // charge the *delta*, not the sum of every intermediate payload.
        let prior_cost = self
            .staged_ledger
            .read_sync(file_id, |_, (cost, _)| *cost)
            .unwrap_or(0);

        let over_cap =
            |cur: u64| cur.saturating_sub(prior_cost) + padded_size > self.max_write_bytes;

        let mut total_staged_bytes = self
            .current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed);

        if over_cap(total_staged_bytes) {
            // Capacity pressure: promote resident staged entries to durable
            // backend blocks so the pool drains, then wait (bounded) for the
            // merge worker to credit freed space. Never a fixed futile stall.
            self.kick_promotion(file_id, 64);
            let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
            loop {
                let notified = self.space_freed_notify.notified();
                tokio::pin!(notified);
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    break;
                }
                total_staged_bytes = self
                    .current_staged_write_bytes
                    .load(std::sync::atomic::Ordering::Relaxed);
                if !over_cap(total_staged_bytes) {
                    break;
                }
                self.kick_promotion(file_id, 64);
            }
            total_staged_bytes = self
                .current_staged_write_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
        }

        if over_cap(total_staged_bytes) {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "Local NVMe staging cache capacity exceeded: current {} bytes, writing {} bytes, max capacity {} bytes",
                    total_staged_bytes, padded_size, self.max_write_bytes
                )
            )));
        }

        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());

        // Ring write + ledger charge under this file_id's ledger entry lock:
        // a concurrent promotion/unlink of the same id either completes fully
        // before (and sees the old generation) or after (and sees the bumped
        // generation) — it can never remove the ring entry we just wrote.
        // spawn_blocking: the ring write is a staging shard WRITE lock
        // (shard-lock invariant rule 2 — never park an async executor on it).
        let (admitted, replaced_cost, is_new) = {
            let ledger = self.staged_ledger.clone();
            let staging = self.staging_nvme_cache.clone();
            let file_id_owned = file_id.to_string();
            let key_clone = key_bytes.clone();
            let data_clone = data.clone();
            tokio::task::spawn_blocking(move || {
                let mut entry = ledger.entry_sync(file_id_owned).or_insert((0, 0));
                let is_new = staging.get(&key_clone).is_none();
                // Memory-mapped copy directly (lock-free, zero disk syscall wait).
                // The segment never destroys live entries: refusal here is loud
                // backpressure and the caller escalates to a durable spill.
                let admitted = staging.reserve_and_write(
                    key_clone.clone(),
                    meta_len,
                    &meta_bytes,
                    &data_clone,
                    None,
                );
                if admitted {
                    let (cost, gen) = *entry.get();
                    *entry.get_mut() = (padded_size, gen.wrapping_add(1));
                    (true, cost, is_new)
                } else {
                    // Drop a placeholder created for this refused stage.
                    if entry.get().0 == 0 {
                        let _ = entry.remove();
                    }
                    (false, 0, false)
                }
            })
            .await
            .map_err(|e| SqueezefsError::Io(std::io::Error::other(e.to_string())))?
        };
        if !admitted {
            return Err(SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                format!(
                    "Local NVMe staging segment cannot admit {} bytes without destroying live staged entries",
                    padded_size
                ),
            )));
        }
        self.current_staged_write_bytes
            .fetch_add(padded_size, std::sync::atomic::Ordering::Relaxed);
        if replaced_cost > 0 {
            Self::sub_saturating(&self.current_staged_write_bytes, replaced_cost);
            self.space_freed_notify.notify_waiters();
        }

        if is_new {
            self.staged_writes_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        log::debug!(
            "NVMe Staging: Staged write for file {} (ID: {}) size = {} bytes",
            file_path,
            file_id,
            data.len()
        );

        // Keep the hot path enqueue-free (promoting every small stage to the
        // backend competed with create/fsync), but arm the drain *before* the
        // pool hard-fills: past the high-water mark, ask the merge worker to
        // promote this entry in the background.
        let high_water = self.max_write_bytes - self.max_write_bytes / 4;
        if self
            .current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
            > high_water
        {
            self.try_enqueue_staged_merge(file_path, file_id, fencing_token, padded_size);
        }
        Ok(())
    }

    /// Saturating subtract on the staged-budget gauge. Protocol core:
    /// [`crate::gauge_core`] (loom-model-checked).
    fn sub_saturating(gauge: &std::sync::atomic::AtomicU64, amount: u64) {
        crate::gauge_core::sub_saturating(gauge, amount);
    }

    /// Ask the merge worker to promote up to `max_items` resident staged
    /// entries (excluding `exclude_file_id`, whose newest payload is the one
    /// being staged right now). Best-effort: a full queue means promotion is
    /// already in flight.
    fn kick_promotion(&self, exclude_file_id: &str, max_items: usize) {
        let mut pending: Vec<(String, u64)> = Vec::new();
        self.staged_ledger.iter_sync(|file_id, (cost, _)| {
            if file_id != exclude_file_id {
                pending.push((file_id.clone(), *cost));
            }
            pending.len() < max_items
        });
        for (file_id, cost) in pending {
            let Some(meta) = self.staged_meta_of(&file_id) else {
                continue;
            };
            let item = PendingStagedWrite {
                file_path: meta.file_path,
                file_id,
                fencing_token: meta.fencing_token,
                padded_size: cost,
            };
            if self.write_tx.try_send(item).is_err() {
                break;
            }
        }
    }

    /// Read the staged header for `file_id` from the ring (no payload copy).
    fn staged_meta_of(&self, file_id: &str) -> Option<StagedMetadata> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() < 8 {
            return None;
        }
        let meta_len = u64::from_be_bytes(bytes[0..8].try_into().ok()?) as usize;
        if bytes.len() < 8 + meta_len {
            return None;
        }
        StagedMetadata::deserialize(&bytes[8..8 + meta_len])
    }

    /// Current stage generation of `file_id`, if it is budget-counted.
    pub fn staged_generation(&self, file_id: &str) -> Option<u64> {
        self.staged_ledger.read_sync(file_id, |_, (_, gen)| *gen)
    }

    /// Remove `file_id`'s ring entry and return its budget **iff** its stage
    /// generation still equals `gen`. A racing re-stage bumps the generation
    /// first (under the same ledger entry lock), so its fresh ring entry and
    /// budget are never destroyed by a promotion that raced it.
    pub fn remove_staged_if_generation(&self, file_id: &str, gen: u64) -> bool {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let removed_cost = {
            let scc::hash_map::Entry::Occupied(entry) =
                self.staged_ledger.entry_sync(file_id.to_string())
            else {
                return false;
            };
            let (cost, cur_gen) = *entry.get();
            if cur_gen != gen {
                return false;
            }
            if self.staging_nvme_cache.remove(&key_bytes).is_some() {
                let prev = self
                    .staged_writes_in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                if prev == 1 {
                    self.staged_drained_notify.notify_waiters();
                }
            }
            let _ = entry.remove();
            cost
        };
        Self::sub_saturating(&self.current_staged_write_bytes, removed_cost);
        self.space_freed_notify.notify_waiters();
        true
    }

    /// Ask the background merge worker to promote a staged file_id (best-effort).
    pub fn try_enqueue_staged_merge(
        &self,
        file_path: &str,
        file_id: &str,
        fencing_token: u64,
        padded_size: u64,
    ) {
        let pending = PendingStagedWrite {
            file_path: file_path.to_string(),
            file_id: file_id.to_string(),
            fencing_token,
            padded_size,
        };
        let _ = self.write_tx.try_send(pending);
    }

    /// Put a packed active block write to staging_nvme_cache.
    ///
    /// Returns `false` when the segment cannot admit the block without
    /// destroying live entries; the caller must keep the data (RAM buffer)
    /// or upload it durably — never drop it.
    #[must_use]
    pub fn put_active_block(&self, key: &str, data: &[u8], fencing_token: u64) -> bool {
        // Cache-less filesystem: refuse admission. Every caller already
        // implements the never-lossy `admitted == false` path (keep the
        // RAM buffer / escalate to a durable upload), which IS the
        // cache-less design: RAM tiers + direct block I/O only.
        if self.staging_dirs.is_empty() {
            return false;
        }
        let meta = StagedMetadata {
            fencing_token,
            original_size: data.len() as u64,
            file_path: key.to_string(),
        };
        let meta_bytes = meta.serialize();
        let meta_len = meta_bytes.len() as u64;

        let key_bytes = Bytes::copy_from_slice(key.as_bytes());

        let is_new = self.staging_nvme_cache.get(&key_bytes).is_none();
        let admitted = self.staging_nvme_cache.reserve_and_write(
            key_bytes,
            meta_len,
            &meta_bytes,
            data,
            Some(4096),
        );
        if admitted && is_new {
            self.staged_writes_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        admitted
    }

    /// Remove a packed active block write from staging_nvme_cache.
    pub fn remove_active_block(&self, key: &str) -> Option<Vec<u8>> {
        let val = self.read_staged(key);
        let key_bytes = Bytes::copy_from_slice(key.as_bytes());
        if self.staging_nvme_cache.remove(&key_bytes).is_some() {
            let prev = self
                .staged_writes_in_flight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if prev == 1 {
                self.staged_drained_notify.notify_waiters();
            }
        }
        // Return the budget of a counted staged entry (unlink, spill purge,
        // layout transition). Active-block keys are never in the ledger.
        if let Some((_, (cost, _))) = self.staged_ledger.remove_sync(key) {
            Self::sub_saturating(&self.current_staged_write_bytes, cost);
            self.space_freed_notify.notify_waiters();
        }
        val
    }

    /// Remove a staged write from staging_nvme_cache.
    pub fn remove_staged(&self, file_id: &str) -> Option<Vec<u8>> {
        self.remove_active_block(file_id)
    }

    // ---- blocking-pool variants (shard-lock invariant rule 2) ----
    //
    // The staging shard WRITE lock legitimately waits for §5.5 read guards
    // held across awaits (`tiering::nvme::NvmeShard` doc), so acquiring it
    // on an async executor thread can park the very thread that must poll
    // the guard holder — the Hang-1 total-daemon wedge. Async contexts use
    // these wrappers; the sync originals remain for blocking contexts
    // (existing `spawn_blocking` closures, the merge worker, tests).

    /// [`Self::remove_active_block`] on the blocking pool, for async callers.
    pub async fn remove_active_block_async(&self, key: String) -> Result<Option<Vec<u8>>> {
        let nvme = self.clone();
        tokio::task::spawn_blocking(move || nvme.remove_active_block(&key))
            .await
            .map_err(|e| SqueezefsError::Io(std::io::Error::other(e.to_string())))
    }

    /// Remove many active blocks in ONE blocking-pool hop (delete/reclaim
    /// paths sweep every block index of an inode).
    pub async fn remove_active_blocks_async(&self, keys: Vec<String>) -> Result<()> {
        let nvme = self.clone();
        tokio::task::spawn_blocking(move || {
            for key in keys {
                nvme.remove_active_block(&key);
            }
        })
        .await
        .map_err(|e| SqueezefsError::Io(std::io::Error::other(e.to_string())))
    }

    /// [`Self::remove_staged`] on the blocking pool, for async callers.
    pub async fn remove_staged_async(&self, file_id: String) -> Result<Option<Vec<u8>>> {
        self.remove_active_block_async(file_id).await
    }

    /// [`Self::remove_staged_if_generation`] on the blocking pool, for async callers.
    pub async fn remove_staged_if_generation_async(
        &self,
        file_id: String,
        gen: u64,
    ) -> Result<bool> {
        let nvme = self.clone();
        tokio::task::spawn_blocking(move || nvme.remove_staged_if_generation(&file_id, gen))
            .await
            .map_err(|e| SqueezefsError::Io(std::io::Error::other(e.to_string())))
    }

    /// [`Self::put_active_block`] on the blocking pool, for async callers.
    /// `data` is `Bytes` (refcounted) — no payload copy.
    pub async fn put_active_block_async(
        &self,
        key: String,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<bool> {
        let nvme = self.clone();
        tokio::task::spawn_blocking(move || nvme.put_active_block(&key, &data, fencing_token))
            .await
            .map_err(|e| SqueezefsError::Io(std::io::Error::other(e.to_string())))
    }

    /// Read staged data directly from staging_nvme_cache memory-mapped segments.
    pub fn read_staged(&self, file_id: &str) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    let data_start = if file_id.starts_with("active_block:") {
                        4096
                    } else {
                        8 + meta_len
                    };
                    let data_end = data_start + meta.original_size as usize;
                    if bytes.len() >= data_end {
                        return Some(bytes[data_start..data_end].to_vec());
                    }
                }
            }
        }
        None
    }

    /// Read staged fencing token directly from staging_nvme_cache memory-mapped segments without copying data.
    pub fn get_staged_fencing_token(&self, file_id: &str) -> Option<u64> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let guard = self.staging_nvme_cache.get(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    return Some(meta.fencing_token);
                }
            }
        }
        None
    }

    /// Take the §5.5 write-only DMA source for a staged entry: a guard-backed,
    /// zero-copy view of the payload for [`write_block_from_staging`].
    /// Holding it pins the entry's shard against writers/evictors, so take it
    /// immediately before the upload and let the helper consume it.
    pub fn staged_dma_source(&self, key: &str) -> Option<StagedDmaSource> {
        self.read_staged_zero_copy(key)
            .map(|guard| StagedDmaSource { guard })
    }

    /// Read staged data zero-copy directly from staging_nvme_cache memory-mapped segments.
    pub fn read_staged_zero_copy(
        &self,
        file_id: &str,
    ) -> Option<crate::tiering::nvme::NvmeCacheReadGuard> {
        let key_bytes = Bytes::copy_from_slice(file_id.as_bytes());
        let mut guard = self.staging_nvme_cache.get_static(&key_bytes)?;
        let bytes = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
        if bytes.len() >= 8 {
            let meta_len = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8])) as usize;
            if bytes.len() >= 8 + meta_len {
                if let Some(meta) = StagedMetadata::deserialize(&bytes[8..8 + meta_len]) {
                    let data_start = if file_id.starts_with("active_block:") {
                        4096
                    } else {
                        8 + meta_len
                    };
                    let data_end = data_start + meta.original_size as usize;
                    if bytes.len() >= data_end {
                        guard.offset += data_start;
                        guard.len = meta.original_size as usize;
                        return Some(guard);
                    }
                }
            }
        }
        None
    }

    fn start_merge_worker(&self, mut write_rx: mpsc::Receiver<PendingStagedWrite>) {
        let data_router = self.data_router.clone();
        let gauge = self.current_staged_write_bytes.clone();
        let high_water = self.max_write_bytes - self.max_write_bytes / 4;

        tokio::spawn(async move {
            let mut batch: Vec<PendingStagedWrite> = Vec::new();
            let mut current_bytes = 0u64;
            let max_batch_bytes = 4 * 1024 * 1024;
            let flush_timeout = Duration::from_millis(500);

            loop {
                let sleep = time::sleep(flush_timeout);
                tokio::pin!(sleep);

                tokio::select! {
                    res = write_rx.recv() => {
                        match res {
                            Some(pending) => {
                                current_bytes += pending.padded_size;
                                batch.push(pending);

                                // Promote immediately under capacity pressure
                                // (writers may be gate-blocked on freed space);
                                // otherwise batch up to amortize.
                                if current_bytes >= max_batch_bytes
                                    || gauge.load(std::sync::atomic::Ordering::Relaxed) > high_water
                                {
                                    Self::promote_batch(&data_router, &mut batch).await;
                                    current_bytes = 0;
                                }
                            }
                            None => {
                                if !batch.is_empty() {
                                    info!("NVMe Staging: Channel closed. Promoting remaining {} staged writes.", batch.len());
                                    Self::promote_batch(&data_router, &mut batch).await;
                                }
                                break;
                            }
                        }
                    }
                    _ = &mut sleep => {
                        if !batch.is_empty() {
                            Self::promote_batch(&data_router, &mut batch).await;
                            current_bytes = 0;
                        }
                    }
                }
            }
        });
    }

    /// Promote every distinct pending staged file to a durable backend block
    /// through the owning router (coherent RAM + backend layout commit).
    /// Conservative on any miss: the entry stays resident and budget-counted.
    async fn promote_batch(
        data_router: &std::sync::OnceLock<std::sync::Weak<crate::routing::DataRouterInner>>,
        batch: &mut Vec<PendingStagedWrite>,
    ) {
        let Some(router) = data_router.get().and_then(std::sync::Weak::upgrade) else {
            // Router not wired (shutdown or partially constructed cache):
            // drop the notices, entries stay resident + counted.
            batch.clear();
            return;
        };
        let router = crate::routing::DataRouter::from_inner(router);

        let mut seen = std::collections::HashSet::new();
        for item in batch.drain(..) {
            if !seen.insert(item.file_id.clone()) {
                continue;
            }
            crate::coz_progress!("nvme_staged_promotion");
            match router
                .promote_staged_file(&item.file_path, &item.file_id, item.fencing_token)
                .await
            {
                Ok(true) => info!(
                    "NVMe Staging: promoted staged file {} (ID: {}) to durable block",
                    item.file_path, item.file_id
                ),
                Ok(false) => {}
                Err(e) => error!(
                    "NVMe Staging: promotion failed for {} (ID: {}): {:?} — entry stays resident",
                    item.file_path, item.file_id, e
                ),
            }
        }
    }

    pub fn staging_dirs(&self) -> &[PathBuf] {
        &self.staging_dirs
    }

    /// Cache a block of read data on local NVMe using read_nvme_cache.
    pub fn cache_read_block(&self, block_key: &str, data: Bytes) -> Result<()> {
        // Cache-less filesystem: no NVMe read-cache tier — dehydration of
        // RAM-LRU evictions is a clean no-op (the block stays readable
        // from the backend; only the local cache tier is absent).
        if self.staging_dirs.is_empty() {
            return Ok(());
        }
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let val_bytes = data;
        self.read_nvme_cache
            .put(key_bytes.clone(), val_bytes.clone());

        if let Some(dht) = self.dht_node.get() {
            let dht_clone = dht.clone();
            let key_hash = xxh3_64(block_key.as_bytes());
            // P1-5: best-effort peer publish under global admission.
            crate::bg_admit::spawn_bg(async move {
                let owners = dht_clone.find_closest_peers(key_hash, 3);
                if let Some(primary_owner) = owners.first() {
                    if primary_owner != dht_clone.peer_addr() {
                        let _ = dht_clone
                            .store_remote_value(primary_owner, key_bytes, val_bytes)
                            .await;
                    }
                }
            });
        }

        Ok(())
    }

    /// INCARNATION-VALIDATED read-cache publish — the only legal route for
    /// non-owner publishes (RAM-LRU dehydration, p2p peer stores). Their
    /// payloads can be arbitrarily stale (an evicted entry parked in the
    /// dehydration channel, a peer store in flight), so an unconditional
    /// `cache_read_block` could stick a DEAD incarnation's bytes under a
    /// freed-and-reallocated key — served for the key's next owner (the
    /// generic/074 fstest.3 stale-fill family). Same discipline as the
    /// routing validated fill: snapshot the key's incarnation, publish only
    /// while it is stable, and undo if it moved across the put. Untracked
    /// legacy keys (never freed/reallocated) are vacuously valid.
    ///
    /// Returns whether the entry is (still) published.
    pub fn cache_read_block_validated(
        &self,
        block_key: &str,
        data: Bytes,
        backend_router: &crate::routing::BackendRouter,
    ) -> bool {
        if !backend_router.key_incarnation_tracked(block_key) {
            return self.cache_read_block(block_key, data).is_ok();
        }
        let Some(before) = backend_router.fill_incarnation(block_key) else {
            // Unstable (mid-write or retired): never publish.
            return false;
        };
        if self.cache_read_block(block_key, data).is_err() {
            return false;
        }
        if !backend_router.fill_incarnation_still(block_key, before) {
            self.remove_cached_read_block(block_key);
            return false;
        }
        true
    }

    /// [`Self::cache_read_block_validated`] against this staging's own wired
    /// router (dehydration worker / p2p store call sites). Unwired routers
    /// (bare tooling) refuse the publish — a cache entry is never worth an
    /// unvalidated stick.
    pub fn cache_read_block_validated_self(&self, block_key: &str, data: Bytes) -> bool {
        match self.backend_router.get() {
            Some(router) => self.cache_read_block_validated(block_key, data, router),
            None => false,
        }
    }

    pub fn current_staged_write_bytes(&self) -> u64 {
        self.current_staged_write_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn current_read_cache_bytes(&self) -> u64 {
        self.read_nvme_cache.current_bytes() as u64
    }

    pub fn read_cached_block(&self, block_key: &str) -> Option<Vec<u8>> {
        self.get_cached_read_block(block_key)
    }

    /// Drop a read-cache entry for a block key. Called when the physical block
    /// behind the key is freed: block keys are offset strings, so the next
    /// allocation of that offset reuses the same key string and must never be
    /// served this incarnation's bytes.
    pub fn remove_cached_read_block(&self, block_key: &str) {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let _ = self.read_nvme_cache.remove(&key_bytes);
    }

    pub fn get_cached_read_block(&self, block_key: &str) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let guard = self.read_nvme_cache.get(&key_bytes)?;
        Some(guard.guard.mmap[guard.offset..guard.offset + guard.len].to_vec())
    }

    pub fn get_cached_read_block_range(
        &self,
        block_key: &str,
        offset: u64,
        size: u32,
    ) -> Option<Vec<u8>> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let guard = self.read_nvme_cache.get(&key_bytes)?;
        let start = guard.offset + offset as usize;
        if start >= guard.offset + guard.len {
            return Some(Vec::new());
        }
        let read_len = std::cmp::min(size as usize, guard.len - offset as usize);
        let end = start + read_len;
        Some(guard.guard.mmap[start..end].to_vec())
    }

    pub fn get_cached_read_block_range_zero_copy(
        &self,
        block_key: &str,
        offset: u64,
        size: u32,
    ) -> Option<crate::tiering::nvme::NvmeCacheReadGuard> {
        let key_bytes = Bytes::copy_from_slice(block_key.as_bytes());
        let mut guard = self.read_nvme_cache.get_static(&key_bytes)?;
        let start = guard.offset + offset as usize;
        if start >= guard.offset + guard.len {
            guard.offset += guard.len;
            guard.len = 0;
            return Some(guard);
        }
        let read_len = std::cmp::min(size as usize, guard.len - offset as usize);
        guard.offset = start;
        guard.len = read_len;
        Some(guard)
    }

    pub fn list_staged_files(&self) -> Vec<String> {
        self.staging_nvme_cache
            .list_keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    pub fn list_cached_blocks(&self) -> Vec<String> {
        self.read_nvme_cache
            .list_keys()
            .into_iter()
            .filter_map(|k| String::from_utf8(k.to_vec()).ok())
            .collect()
    }

    pub fn max_write_bytes(&self) -> u64 {
        self.max_write_bytes
    }

    pub fn max_read_bytes(&self) -> u64 {
        self.max_read_bytes
    }
}
