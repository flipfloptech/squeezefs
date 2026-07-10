//! Offline v2 → v3 metadata converter — `squeezefs migrate` (PR K9; design
//! §6.2, normative).
//!
//! **Mechanism** (in-place, crash-safe, idempotent — §6.2):
//!
//! 1. **Preflight** — classify sector 0. A v3 volume is a clean idempotent
//!    no-op (a re-run after a completed flip); a blank volume has nothing to
//!    migrate; a v2 volume proceeds. Live client registrations refuse the
//!    migration (the [`crate::meta_backend::MetaLvBackend::format_preflight`]
//!    live-client policy, reused with `force` so an *already-formatted* v2
//!    volume is not itself a refusal — migrating one is the whole point).
//! 2. **Read the v2 namespace** through the v2 read path: every magic-valid
//!    inode (plus the root), every directory's dentries, and every
//!    non-quarantined inode's xattrs (incl. `"layout"` and `"system.symlink"`).
//!    Quarantined inos [1024, 1152) migrate with `flags2`
//!    [`FLAGS2_QUARANTINE_CONTENT_LOST`] set and their (presumed-corrupt) xattr
//!    blocks left unread — preserving
//!    today's degrade (empty regular reads; symlink-target reads → EIO).
//! 3. **Build** a v3 image into the free tail (`build_start` from the highest
//!    used ino) via the K6a builder engine
//!    ([`super::builder::build_migrated_image`]) — no live v2 byte is touched
//!    and the superblock is *not* flipped.
//! 4. **Verify** the round-trip digest (source records vs the read-back v3
//!    image, [`super::builder::digest_walk`]); a mismatch aborts *before* any
//!    flip.
//! 5. **Flip** — a single checksummed sector-0 write behind an `fdatasync`
//!    (`--dry-run` stops at step 4). Everything below `build_start` — v2
//!    tables, the dead journal region, the whole xattr reservation — is now
//!    free v3 heap extents.
//!
//! Crash-safety: until step 5 sector 0 still holds the v2 superblock and every
//! live v2 byte is intact, so a crash before the flip makes a re-run a clean
//! restart (the build region is re-zeroed and rebuilt); a crash after the flip
//! leaves a v3 volume that a re-run recognizes as done (idempotent). The build
//! region's 32 KiB header sectors are re-zeroed on every run so a v2 remount
//! between a torn migrate and its re-run degrades cleanly on any ino whose
//! xattr block now overlaps v3 build garbage.

use super::builder::{build_migrated_image, digest_record_set, digest_walk, MigrateImageInput};
use super::checkpoint::read_newest_ledger;
use super::node::NodeLayout;
use super::node_cache::{NodeCache, NodeCacheConfig};
use super::record::{
    dentry_key, dentry_name_hash54, inode_key, xattr_key, xattr_name_hash56, DentryValue,
    InodeValue, Record, XattrValue, FLAGS2_QUARANTINE_CONTENT_LOST, TREE_DENTRIES, TREE_INODES,
    TREE_XATTRS,
};
use super::superblock::{classify_volume, SuperblockV3, VolumeFormat};
use super::tree::{KvTree, RootPtr};
use super::KvError;
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::storage::{MetaLvStorage, JOURNAL_REGION_SIZE, JOURNAL_REGION_START};
use crate::meta_backend::xattr::{is_quarantined, XATTR_BLOCK_SIZE, XATTR_BLOCK_START};
use crate::meta_backend::{MetaLvBackend, Metadata};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// The default v3 node size a migration produces (§5.1 default; migrate takes
/// no `--meta-node-kib` — new v3 volumes pick node size, converted ones use the
/// shipped default).
pub const MIGRATE_NODE_SIZE: usize = 256 * 1024;

/// Node-cache budget for the read-back digest verification pass (read-only,
/// clean nodes evict freely — a modest budget suffices).
const VERIFY_CACHE_BUDGET: u64 = 128 * 1024 * 1024;
/// Writeback-delta knob for the verify cache (never written — read path only).
const VERIFY_WRITEBACK_DELTA: usize = 4096;

/// `squeezefs migrate` options (§6.2 / §5.1).
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// `--grow <bytes>`: new **total** device size (file-backed `set_len`) when
    /// the free tail cannot hold the v3 image. `None` = never grow (refuse with
    /// the exact deficit instead).
    pub grow_to_bytes: Option<u64>,
    /// `--dry-run`: build + digest-diff against the source, no superblock flip.
    pub dry_run: bool,
    /// v3 node size (bytes). Always [`MIGRATE_NODE_SIZE`] from the CLI; tests
    /// override it for small sandboxes.
    pub node_size: usize,
    /// `--meta-journal-mb` override (bytes); `None` = the OQ 1 clamp.
    pub journal_len_override: Option<u64>,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            grow_to_bytes: None,
            dry_run: false,
            node_size: MIGRATE_NODE_SIZE,
            journal_len_override: None,
        }
    }
}

/// Outcome of one [`migrate_volume`] call (CLI output + test assertions).
#[derive(Debug, Clone)]
pub struct MigrateReport {
    /// The volume already carried a v3 superblock: a clean idempotent no-op
    /// (a re-run after a completed flip). No bytes were written.
    pub already_v3: bool,
    /// `--dry-run`: the image was built + verified but not flipped.
    pub dry_run: bool,
    /// Whether sector 0 was flipped to v3 (false for dry-run / no-op).
    pub flipped: bool,
    /// Device length used for the build (post-`--grow`).
    pub device_len: u64,
    /// `Some(new_len)` if `--grow` extended the device.
    pub grew_to: Option<u64>,
    /// v3 node size of the produced image.
    pub node_size: usize,
    /// Highest magic-valid v2 ino (the §6.2 build-region base).
    pub max_used_ino: u64,
    /// Tail build-region base offset (`align_up(72 MiB + (max+1)·32 KiB, ns)`).
    pub build_start: u64,
    pub inodes_migrated: u64,
    pub dentries_migrated: u64,
    pub xattrs_migrated: u64,
    /// Quarantined inos [1024, 1152) carried with the content-lost flag.
    pub quarantined_inodes: u64,
    /// Digest of the assembled source records (§4.10 walk shape).
    pub source_digest: u64,
    /// Digest of the read-back v3 image; equals `source_digest` on success.
    pub built_digest: u64,
    /// Btree nodes written into the tail.
    pub nodes_written: u64,
    /// §4.8 ino watermark recorded in the v3 ledger.
    pub next_ino: u64,
    /// Free v3 extents whose physical bytes cover the v2 dead journal region
    /// [104 MiB, 108 MiB) — reclaimed (§6.2 accounting).
    pub reclaimed_journal_extents: u64,
    /// Free v3 extents whose physical bytes cover the v2 xattr reservation
    /// below `build_start` (the used-ino blocks + inter-block slack) —
    /// reclaimed (§6.2 accounting).
    pub reclaimed_reservation_extents: u64,
    /// Free v3 heap extents after the build.
    pub free_extents_after: u64,
    /// Total v3 heap extents.
    pub total_extents: u64,
}

/// The §6.2 build-region base: `align_up(72 MiB + (max_used_ino + 1)·32 KiB,
/// node_size)`. Everything at or above it is unused xattr reservation, so no
/// live v2 byte is written by the tail build.
pub fn build_start_for(max_used_ino: u64, node_size: u64) -> u64 {
    let raw = XATTR_BLOCK_START + (max_used_ino + 1) * XATTR_BLOCK_SIZE as u64;
    raw.div_ceil(node_size) * node_size
}

/// Device length in bytes (files and block devices alike).
fn device_len(path: &Path) -> Result<u64> {
    use std::io::Seek;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(SqueezefsError::Io)?;
    f.seek(std::io::SeekFrom::End(0))
        .map_err(SqueezefsError::Io)
}

/// The whole v2 namespace, ready to encode into v3 records.
struct V2Namespace {
    /// `ino → packed value` (quarantined inos carry the content-lost flag).
    inodes: BTreeMap<u64, InodeValue>,
    /// `parent → (name → (child_ino, file_type_byte))`, name-sorted.
    dentries: BTreeMap<u64, BTreeMap<Vec<u8>, (u64, u8)>>,
    /// `(ino, name) → value`, name-sorted per ino.
    xattrs: BTreeMap<(u64, Vec<u8>), Vec<u8>>,
    max_used_ino: u64,
    quarantined: u64,
}

fn value_from_disk(di: &crate::meta_backend::inode::DiskInode, quarantined: bool) -> InodeValue {
    InodeValue {
        mode: di.mode,
        uid: di.uid,
        gid: di.gid,
        nlink: di.nlink,
        flags: di.flags,
        flags2: if quarantined {
            FLAGS2_QUARANTINE_CONTENT_LOST
        } else {
            0
        },
        size: di.size,
        atime: di.atime,
        mtime: di.mtime,
        ctime: di.ctime,
    }
}

/// `d_type`-style byte for a mode's `S_IFMT` bits (mirrors the builder).
fn dt_of(mode: u32) -> u8 {
    ((mode & libc::S_IFMT) >> 12) as u8
}

/// Read the whole v2 namespace through the v2 read path.
async fn read_v2_namespace(backend: &MetaLvBackend) -> Result<V2Namespace> {
    let storage = &backend.storage;

    // Inodes: the root (the inode scan skips the reserved low range) plus every
    // magic-valid slot. `max_used_ino` comes from the scan (§6.2), which sees
    // even unreachable/leaked inos so no live xattr block is ever overwritten.
    let mut inodes: BTreeMap<u64, InodeValue> = BTreeMap::new();
    let mut max_used_ino: u64 = 1;
    let mut quarantined: u64 = 0;

    let root_di = crate::meta_backend::inode::read_inode(storage, 1).await?;
    inodes.insert(1, value_from_disk(&root_di, false));

    storage
        .scan_used_inodes(|ino, di| {
            max_used_ino = max_used_ino.max(ino);
            let q = is_quarantined(ino);
            if q {
                quarantined += 1;
            }
            inodes.insert(ino, value_from_disk(di, q));
        })
        .await?;

    // Dentries: every directory's entries; file_type derived from the child's
    // mode (robust against a stale dentry `file_type`).
    let mut dentries: BTreeMap<u64, BTreeMap<Vec<u8>, (u64, u8)>> = BTreeMap::new();
    let dir_inos: Vec<u64> = inodes
        .iter()
        .filter(|(_, v)| (v.mode & libc::S_IFMT) == libc::S_IFDIR)
        .map(|(ino, _)| *ino)
        .collect();
    for dir in dir_inos {
        let entries = backend.readdir(dir, 0, usize::MAX).await?;
        let map = dentries.entry(dir).or_default();
        for e in entries {
            let child_mode = inodes.get(&e.ino).map(|v| v.mode).unwrap_or(e.file_type);
            map.insert(e.name.into_bytes(), (e.ino, dt_of(child_mode)));
        }
    }

    // Xattrs: every non-quarantined ino (quarantined blocks are presumed
    // corrupt and left unread — their content is lost by contract, §6.2).
    // Symlinks store their target as raw bytes outside the listable xattr
    // block, so v2 `listxattr(symlink)` is empty — read `system.symlink`
    // explicitly (v3 keeps it in the xattr tree like any value, §5.3).
    let mut xattrs: BTreeMap<(u64, Vec<u8>), Vec<u8>> = BTreeMap::new();
    let all_inos: Vec<u64> = inodes.keys().copied().collect();
    for ino in all_inos {
        if is_quarantined(ino) {
            continue;
        }
        if (inodes[&ino].mode & libc::S_IFMT) == libc::S_IFLNK {
            if let Some(target) = backend.getxattr(ino, "system.symlink").await? {
                xattrs.insert((ino, b"system.symlink".to_vec()), target);
            }
            continue;
        }
        for name in backend.listxattr(ino).await? {
            if let Some(value) = backend.getxattr(ino, &name).await? {
                xattrs.insert((ino, name.into_bytes()), value);
            }
        }
    }

    Ok(V2Namespace {
        inodes,
        dentries,
        xattrs,
        max_used_ino,
        quarantined,
    })
}

/// Encode the namespace into the three key-ascending v3 record vectors, with
/// the builder's deterministic `coll_seq` assignment (sorted-name order within
/// a `(parent/ino, hash)` group) so a migrated volume is byte-identical to a
/// fresh build of the same content.
fn assemble_records(
    ns: &V2Namespace,
    hash_seed: u64,
) -> Result<(Vec<Record>, Vec<Record>, Vec<Record>)> {
    let inode_records: Vec<Record> = ns
        .inodes
        .iter()
        .map(|(ino, v)| Record::put(inode_key(*ino).to_vec(), 0, v.encode()))
        .collect();

    let mut dentry_sorted: BTreeMap<Vec<u8>, Record> = BTreeMap::new();
    for (parent, dir) in &ns.dentries {
        let mut groups: BTreeMap<u64, u16> = BTreeMap::new();
        for (name, (child, dt)) in dir {
            let hash = dentry_name_hash54(name, hash_seed);
            let coll = groups.entry(hash).or_insert(0);
            if *coll > u16::from(u8::MAX) {
                return Err(KvError::DentryChainOverflow.into());
            }
            let key = dentry_key(*parent, hash, *coll as u8).to_vec();
            *coll += 1;
            let value = DentryValue {
                child_ino: *child,
                file_type: *dt,
                name: name.clone(),
            }
            .encode()
            .map_err(SqueezefsError::from)?;
            dentry_sorted.insert(key, Record::put(Vec::new(), 0, value));
        }
    }
    let dentry_records: Vec<Record> = dentry_sorted
        .into_iter()
        .map(|(key, mut rec)| {
            rec.key = key;
            rec
        })
        .collect();

    let mut xattr_sorted: BTreeMap<Vec<u8>, Record> = BTreeMap::new();
    let mut xgroups: BTreeMap<(u64, u64), u16> = BTreeMap::new();
    for ((ino, name), value) in &ns.xattrs {
        let hash = xattr_name_hash56(name, hash_seed);
        let coll = xgroups.entry((*ino, hash)).or_insert(0);
        if *coll > u16::from(u8::MAX) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "xattr hash-collision chain full on ino {ino} (coll_seq 0..=255 occupied)"
            )));
        }
        let key = xattr_key(*ino, hash, *coll as u8).to_vec();
        *coll += 1;
        let enc = XattrValue {
            name: name.clone(),
            value: value.clone(),
        }
        .encode()
        .map_err(SqueezefsError::from)?;
        xattr_sorted.insert(key, Record::put(Vec::new(), 0, enc));
    }
    let xattr_records: Vec<Record> = xattr_sorted
        .into_iter()
        .map(|(key, mut rec)| {
            rec.key = key;
            rec
        })
        .collect();

    Ok((inode_records, dentry_records, xattr_records))
}

/// Read the just-built (un-flipped) v3 image back through the tree read path
/// and digest it (§4.10) — the round-trip proof the flip rides behind.
async fn digest_built_image(path: &Path, sb: &SuperblockV3) -> Result<u64> {
    let ledger = read_newest_ledger(path, sb.root_ledger.start)
        .await?
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "migrate verification: no valid ledger record in the freshly built image".into(),
            )
        })?;
    let layout = NodeLayout::new(sb.node_size as usize).map_err(SqueezefsError::from)?;
    let cache = NodeCache::new(NodeCacheConfig {
        path: path.to_path_buf(),
        layout,
        heap_base: sb.heap.start,
        budget_bytes: VERIFY_CACHE_BUDGET,
        writeback_delta_bytes: VERIFY_WRITEBACK_DELTA,
    });
    let seq = Arc::new(AtomicU64::new(ledger.seq));
    let mut trees: Vec<KvTree> = Vec::with_capacity(3);
    for tree_id in [TREE_INODES, TREE_DENTRIES, TREE_XATTRS] {
        let root = ledger
            .tree_roots
            .iter()
            .find(|r| r.tree_id == tree_id)
            .ok_or_else(|| {
                SqueezefsError::InvalidOperation(format!(
                    "migrate verification: built ledger names no root for tree {tree_id}"
                ))
            })?;
        let tree = KvTree::open(
            cache.clone(),
            tree_id,
            RootPtr {
                addr: root.node_addr,
                seq: root.node_seq,
            },
            seq.clone(),
        )
        .await?;
        trees.push(tree);
    }
    let refs: Vec<&KvTree> = trees.iter().collect();
    digest_walk(&refs).await.map_err(SqueezefsError::from)
}

/// Count heap extents whose physical bytes fall in `[lo, hi)` that are **free**
/// (reclaim accounting, §6.2). `lo`/`hi` are clamped to the heap.
fn free_extents_in_range(
    alloc: &super::alloc_ext::ExtentAllocator,
    sb: &SuperblockV3,
    lo: u64,
    hi: u64,
) -> u64 {
    let ns = u64::from(sb.node_size);
    let first = lo / ns;
    let last = hi.div_ceil(ns).min(sb.total_extents());
    let mut free = 0;
    for extent in first..last {
        if !alloc.is_allocated(extent) {
            free += 1;
        }
    }
    free
}

/// Convert a v2 metadata volume to v3 in place (design §6.2). Idempotent and
/// crash-safe; `--dry-run` builds + verifies without flipping.
pub async fn migrate_volume(path: &Path, opts: &MigrateOptions) -> Result<MigrateReport> {
    // 1. Preflight: classify sector 0.
    match classify_volume(path).await.map_err(SqueezefsError::from)? {
        VolumeFormat::V3(sb) => {
            // Already migrated — a clean idempotent no-op (§6.2). No writes.
            return Ok(MigrateReport {
                already_v3: true,
                dry_run: opts.dry_run,
                flipped: false,
                device_len: device_len(path)?,
                grew_to: None,
                node_size: sb.node_size as usize,
                max_used_ino: 0,
                build_start: sb.root_ledger.start,
                inodes_migrated: 0,
                dentries_migrated: 0,
                xattrs_migrated: 0,
                quarantined_inodes: 0,
                source_digest: 0,
                built_digest: 0,
                nodes_written: 0,
                next_ino: 0,
                reclaimed_journal_extents: 0,
                reclaimed_reservation_extents: 0,
                free_extents_after: 0,
                total_extents: sb.total_extents(),
            });
        }
        VolumeFormat::Blank => {
            return Err(SqueezefsError::InvalidOperation(format!(
                "{}: not formatted (zeroed superblock) — nothing to migrate",
                path.display()
            )));
        }
        VolumeFormat::V2(_) => {}
    }

    // 2. Open the v2 source + live-client refusal (format_preflight policy,
    // reused with force=true so an already-formatted v2 is not itself refused —
    // migrating one is the point; only *live* mounts refuse).
    let storage = MetaLvStorage::open(path, 128 * 1024 * 1024)?;
    storage.validate_superblock().await?;
    MetaLvBackend::format_preflight(&storage, true).await?;
    let backend = MetaLvBackend::new(storage);

    // 3. Read the whole v2 namespace.
    let ns = read_v2_namespace(&backend).await?;
    let inodes_migrated = ns.inodes.len() as u64;
    let dentries_migrated: u64 = ns.dentries.values().map(|d| d.len() as u64).sum();
    let xattrs_migrated = ns.xattrs.len() as u64;
    let quarantined_inodes = ns.quarantined;
    let max_used_ino = ns.max_used_ino;

    // 4. Geometry + a fresh random seed/uuid (v2 carries neither). Both
    // digests use this seed, so they stay comparable regardless of its value.
    NodeLayout::new(opts.node_size).map_err(SqueezefsError::from)?; // validate node size early
    let node_size = opts.node_size as u64;
    let hash_seed = rand::random::<u64>();
    let uuid = *uuid::Uuid::new_v4().as_bytes();
    let build_start = build_start_for(max_used_ino, node_size);

    // 5. Assemble the v3 records once; the source digest and the space estimate
    // both derive from them.
    let (inode_records, dentry_records, xattr_records) = assemble_records(&ns, hash_seed)?;
    let source_digest = digest_record_set(&[
        (TREE_INODES, &inode_records),
        (TREE_DENTRIES, &dentry_records),
        (TREE_XATTRS, &xattr_records),
    ]);
    let rec_bytes: usize = [&inode_records, &dentry_records, &xattr_records]
        .into_iter()
        .flat_map(|v| v.iter())
        .map(|r| r.record_ref().encoded_len())
        .sum();
    let node_extents = estimate_node_extents(rec_bytes, node_size);

    // 6. Space: refuse loud (or --grow) if the tail cannot hold the image. The
    // journal clamp and bitmap size depend on the device length, so resolve a
    // stable suggestion by a short fixpoint before deciding.
    let mut dev_len = device_len(path)?;
    let mut grew_to: Option<u64> = None;
    let need_at = |cand: u64| {
        min_device_len(
            node_extents,
            node_size,
            opts.journal_len_override,
            build_start,
            cand,
        )
    };
    if dev_len < need_at(dev_len) {
        let mut suggest = need_at(dev_len);
        for _ in 0..4 {
            let next = need_at(suggest);
            if next <= suggest {
                break;
            }
            suggest = next;
        }
        let meta = std::fs::metadata(path).map_err(SqueezefsError::Io)?;
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_block_device() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "{}: the free tail is too small to migrate in place ({} bytes short); the volume \
                 is a block device — grow it externally or migrate to a larger device (§6.2). \
                 Need a device of at least {suggest} bytes.",
                path.display(),
                suggest.saturating_sub(dev_len)
            )));
        }
        match opts.grow_to_bytes {
            Some(grow) if grow >= suggest => {
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .map_err(SqueezefsError::Io)?;
                f.set_len(grow).map_err(SqueezefsError::Io)?;
                dev_len = grow;
                grew_to = Some(grow);
            }
            Some(grow) => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "{}: --grow {grow} is still too small; migrating in place needs a device of \
                     at least {suggest} bytes ({} more than the current {dev_len}).",
                    path.display(),
                    suggest.saturating_sub(dev_len)
                )));
            }
            None => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "{}: the free tail is too small to migrate in place ({} bytes short). Pass \
                     --grow {suggest} to extend the volume to at least that many bytes, or \
                     migrate to a larger device (§6.2).",
                    path.display(),
                    suggest.saturating_sub(dev_len)
                )));
            }
        }
    }

    // 7. Plan the migrate geometry (whole-device heap, structures reserved in
    // the tail).
    let sb = SuperblockV3::plan_migrate(
        dev_len,
        build_start,
        opts.node_size,
        opts.journal_len_override,
        uuid,
        hash_seed,
    )
    .map_err(SqueezefsError::from)?;

    let next_ino = max_used_ino + 1;
    let input = MigrateImageInput {
        inode_records,
        dentry_records,
        xattr_records,
        next_ino,
    };

    // 8. Build into the tail (never touches a live v2 byte; no flip).
    let built = build_migrated_image(path, dev_len, &sb, &input)
        .await
        .map_err(SqueezefsError::from)?;

    // 9. Verify the round-trip digest before any flip.
    let built_digest = digest_built_image(path, &sb).await?;
    if built_digest != source_digest {
        return Err(SqueezefsError::InvalidOperation(format!(
            "{}: migrate verification failed — source digest {source_digest:#018x} != built \
             image digest {built_digest:#018x}; the v2 superblock is untouched, nothing was \
             flipped (re-run to retry)",
            path.display()
        )));
    }

    // 10. Reclaim accounting from the built allocator state (the low region is
    // free; only extent 0, the reserved triple, and the built nodes are held).
    let alloc = load_built_allocator(path, &sb).await?;
    let reclaimed_journal_extents = free_extents_in_range(
        &alloc,
        &sb,
        JOURNAL_REGION_START,
        JOURNAL_REGION_START + JOURNAL_REGION_SIZE,
    );
    let reclaimed_reservation_extents =
        free_extents_in_range(&alloc, &sb, XATTR_BLOCK_START, build_start);
    let free_extents_after = alloc.free_extents();
    let total_extents = sb.total_extents();

    // 11. Flip (unless dry-run): one checksummed sector-0 write behind a
    // barrier, then make the flip itself durable.
    let flipped = if opts.dry_run {
        false
    } else {
        super::superblock::write_superblock_v3(path, &sb)
            .await
            .map_err(SqueezefsError::from)?;
        crate::uring_fs::fdatasync(path.to_path_buf()).await?;
        true
    };

    Ok(MigrateReport {
        already_v3: false,
        dry_run: opts.dry_run,
        flipped,
        device_len: dev_len,
        grew_to,
        node_size: opts.node_size,
        max_used_ino,
        build_start,
        inodes_migrated,
        dentries_migrated,
        xattrs_migrated,
        quarantined_inodes,
        source_digest,
        built_digest,
        nodes_written: built.nodes_written,
        next_ino,
        reclaimed_journal_extents,
        reclaimed_reservation_extents,
        free_extents_after,
        total_extents,
    })
}

/// Load the built image's allocator (the same A/B bitmap + fresh empty ring the
/// mount path loads) for the reclaim-accounting read-back.
async fn load_built_allocator(
    path: &Path,
    sb: &SuperblockV3,
) -> Result<super::alloc_ext::ExtentAllocator> {
    let ledger = read_newest_ledger(path, sb.root_ledger.start)
        .await?
        .ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "migrate accounting: no valid ledger record in the built image".into(),
            )
        })?;
    let total_extents = sb.total_extents();
    let alloc = super::alloc_ext::ExtentAllocator::load(
        path,
        sb.alloc_bitmap.start,
        total_extents,
        super::alloc_ext::compaction_reserve_extents(total_extents),
        64,
        ledger.seq,
        &[],
    )
    .await
    .map_err(SqueezefsError::from)?;
    Ok(alloc)
}

/// Conservative estimate of the heap extents the packed btree nodes occupy:
/// leaves at the builder's ~3/4 fill, interior levels at a conservative
/// 500-child fan-in, one root floor per tree, plus a 10 % pad. Seed-independent
/// (record key/value *lengths* do not depend on the hash seed).
fn estimate_node_extents(rec_bytes: usize, node_size: u64) -> u64 {
    use super::bset::BSET_HEADER_LEN;
    use super::node::{BSET_FRAME_LEN, NODE_PAGE};
    let usable =
        (node_size as usize).saturating_sub(NODE_PAGE + BSET_FRAME_LEN + BSET_HEADER_LEN) * 3 / 4;
    let leaves = rec_bytes.div_ceil(usable.max(1)).max(3) as u64;
    let interior = leaves.div_ceil(500) + 3;
    leaves + interior + leaves / 10
}

/// The minimum device length (bytes) that lets the v3 image — reserved triple
/// (ledger|journal|bitmap), `node_extents` of nodes, and the §4.7 compaction
/// reserve, all above `build_start` — fit, when the device is `candidate` bytes
/// (the journal clamp and bitmap size scale with the device, so the caller
/// resolves a fixpoint). Result is node-size aligned, so `dev_len ≥ result`
/// implies `heap.end ≥ result` (§6.2 deficit refusal).
fn min_device_len(
    node_extents: u64,
    node_size: u64,
    journal_len_override: Option<u64>,
    build_start: u64,
    candidate: u64,
) -> u64 {
    use super::alloc_ext::{bitmap_region_len, compaction_reserve_extents};
    use super::checkpoint::ROOT_LEDGER_LEN;
    use super::superblock::journal_ring_len;
    let total_extents = (candidate / node_size).max(1);
    let journal_len = journal_len_override.unwrap_or_else(|| journal_ring_len(candidate));
    let bitmap_len = bitmap_region_len(total_extents);
    let triple = ROOT_LEDGER_LEN + journal_len + bitmap_len;
    let first_node_off = (build_start + triple).div_ceil(node_size) * node_size;
    let reserve_extents = compaction_reserve_extents(total_extents) + 4;
    first_node_off + (node_extents + reserve_extents) * node_size
}
