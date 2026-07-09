//! `KvMetaBackend` — the v3 volume backend, **read side** (PR K6a; design
//! §4.5, §5.2; the §4.4 commit pipeline and checkpoint scheduling are
//! PR K6b's).
//!
//! ## Mount sequence (§3 "mount O(active set)", §4.x)
//!
//! 1. **Superblock**: sector 0 via the K6a version gate — torn/foreign/
//!    unknown-incompat superblocks fail loud (§4.10 durable-coverage
//!    units).
//! 2. **Root ledger**: newest slot whose checksum verifies; a torn newest
//!    slot falls back to its predecessor (K3). **No valid slot on a
//!    v3-superblocked volume is loud** — format always writes one, so
//!    all-slots-invalid is real corruption (§4.1's loud list), the policy
//!    K3's `read_newest_ledger` deferred to this mount wiring.
//! 3. **Allocator bitmap**: newest-valid A/B page per pair + journal
//!    delta replay (K4).
//! 4. **Journal replay — read-only, into the K5 cache**: the K3 scan
//!    recovers entries ≥ `journal_tail_seq`; tree records apply to the
//!    RAM-authoritative node cache **with their original seqs** (per-key
//!    LWW by seq — replay reproduces RAM, the K1 fold theorem). Nothing
//!    is written back; dirty deltas stay in RAM for K6b's checkpoint
//!    task, exactly like live commits will.
//! 5. **Trees**: roots pinned via `KvTree::open`; `next_ino` recovered as
//!    `max(ledger.next_ino, max replayed ino + 1)` (§4.8).
//!
//! Reads (`lookup` / `getattr` / `readdir` / `getxattr` / `listxattr`)
//! serve from the K5 latch-free snapshots + THE K1 fold. Mutating
//! `Metadata` ops do not exist here yet — `KvMetaBackend` implements the
//! trait in K6b; K6a routes reads through
//! [`crate::meta_backend::VolumeBackend`]'s static dispatch.
//!
//! ## Readdir offset contract (§5.1, backend half)
//!
//! v3 honors `offset`/`max` (v2 ignores them): offsets 0/1/2 resume from
//! the directory start (the FUSE layer owns synthetic `.`/`..` emission —
//! PR K7); an offset `c > 2` resumes at entries whose dentry-key suffix is
//! **strictly greater** than `c − 3`. Cookies are the key itself, so they
//! are stable across concurrent inserts/removals.

use super::alloc_ext::{compaction_reserve_extents, ExtentAllocator};
use super::checkpoint::{read_newest_ledger, LedgerRecord};
use super::journal::{checkpoint_reserve_bytes, JournalRing};
use super::node::{key_successor, NodeLayout};
use super::node_cache::{NodeCache, NodeCacheConfig, DEFAULT_WRITEBACK_DELTA_BYTES};
use super::record::{
    decode_inode_key, decode_readdir_cookie, dentry_key, dentry_name_hash54, inode_key, xattr_key,
    xattr_name_hash56, DentryValue, InodeValue, ReaddirPos, XattrValue, HASH54_MAX, HASH56_MAX,
    TREE_ALLOC_RESERVED, TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
};
use super::superblock::{classify_volume, SuperblockV3, VolumeFormat};
use super::tree::{KvTree, RootPtr};
use super::KvError;
use crate::error::Result;
use crate::meta_backend::atomicity::META_VOLUME_ATOMICITY_COW;
use crate::meta_backend::{DirEntry, Ino, Inode, Metadata};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// `SQUEEZEFS_META_NODE_CACHE_MB` (§5.1; default 512 — §4.5).
pub const NODE_CACHE_MB_ENV: &str = "SQUEEZEFS_META_NODE_CACHE_MB";

/// Pending-free FIFO capacity handed to the K4 allocator at mount. K6b's
/// checkpoint cadence keeps the live count far below it; the replay-window
/// contract (`ExtentAllocator::load`) fails loud if a recovered window
/// exceeds it.
pub const PENDING_FREE_CAP: usize = 65_536;

/// Range-scan page size for chained reads (readdir/listxattr/probes).
const SCAN_PAGE: usize = 512;

/// Mount-scoped replay outcome (§10: `meta_kv_replay_entries`,
/// `meta_kv_replay_dropped_torn`, `meta_kv_replay_ms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvReplayStats {
    /// Entries recovered and applied.
    pub entries: u64,
    /// Drop-and-resync events confirmed by a later entry — nonzero after
    /// a crash is working-as-designed; nonzero after a clean unmount is
    /// the corruption alert (§10).
    pub dropped_torn: u64,
    /// Wall-clock replay time.
    pub replay_ms: u64,
}

/// One mounted v3 metadata volume (read side).
pub struct KvMetaBackend {
    path: PathBuf,
    sb: SuperblockV3,
    ledger: LedgerRecord,
    /// Shared node cache behind the three trees (§4.5). Held for tree
    /// lifetime; the trees clone the `Arc`.
    inodes: KvTree,
    dentries: KvTree,
    xattrs: KvTree,
    alloc: Arc<ExtentAllocator>,
    /// §4.8 monotonic watermark, recovered at mount; K6b's create path
    /// `fetch_add`s it.
    next_ino: AtomicU64,
    replay: KvReplayStats,
}

impl std::fmt::Debug for KvMetaBackend {
    /// Summarizes instead of deriving (the node-layer precedent): a
    /// backend embeds the whole node cache.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvMetaBackend")
            .field("path", &self.path)
            .field("ledger_seq", &self.ledger.seq)
            .field("next_ino", &self.next_ino.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Resolve the node-cache budget knob (bytes).
fn node_cache_budget_bytes() -> u64 {
    std::env::var(NODE_CACHE_MB_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(super::node_cache::DEFAULT_CACHE_BUDGET_BYTES)
}

impl KvMetaBackend {
    /// Mount `path` per the module-docs sequence. Loud failures: bad/torn
    /// superblock, unknown incompat feature bits, no valid ledger slot,
    /// stale/corrupt tree roots, real device I/O errors. Journal-window
    /// tears recover and are counted, never loud (§4.1).
    ///
    /// Returns `Arc<Self>` (PR K6b): the per-volume checkpoint/writeback
    /// task holds a `Weak` back-reference to the backend, so construction
    /// and task spawn are one step.
    pub async fn open(path: &Path) -> std::result::Result<Arc<Self>, KvError> {
        Ok(Arc::new(Self::open_inner(path).await?))
    }

    async fn open_inner(path: &Path) -> std::result::Result<Self, KvError> {
        let t0 = std::time::Instant::now();

        // 1. Superblock (the version gate is the loud unit).
        let sb = match classify_volume(path).await? {
            VolumeFormat::V3(sb) => sb,
            VolumeFormat::Blank => {
                return Err(KvError::Corrupt(format!(
                    "{} is not formatted (zeroed superblock) — run `squeezefs format` first",
                    path.display()
                )))
            }
            VolumeFormat::V2(_) => {
                return Err(KvError::Corrupt(format!(
                    "{} is a format-v2 volume routed to the v3 backend (dispatch bug)",
                    path.display()
                )))
            }
        };
        if sb.unknown_ro() != 0 {
            // §4.11: read-only feature bits from a future format. K6a's
            // surface is read-only by construction; K6b's write path must
            // re-check this mask and withhold mutations.
            log::warn!(
                "meta volume {}: unknown read-only feature bits {:#x} — mounting read-only",
                path.display(),
                sb.unknown_ro()
            );
        }

        // 2. Root ledger: newest valid slot; all-slots-invalid is LOUD on
        // a v3-superblocked volume (§4.1's loud list — format always
        // writes a bootstrap record, so nothing-valid is corruption, not
        // freshness).
        let ledger = read_newest_ledger(path, sb.root_ledger.start)
            .await?
            .ok_or_else(|| {
                KvError::Corrupt(format!(
                    "{}: no valid root-ledger record in any of the 32 slots — the volume \
                     carries a v3 superblock, so this is corruption, not a fresh format",
                    path.display()
                ))
            })?;

        // 3+4a. Journal recovery: one sequential ring read, §4.1 tear
        // semantics (never loud for ring contents).
        let (_ring, recovery) = JournalRing::recover(
            path,
            sb.journal.start,
            sb.journal_pages(),
            checkpoint_reserve_bytes(sb.journal.len),
            ledger.journal_tail_seq,
        )
        .await?;

        // 4b. Allocator: newest-valid A/B pages + replayed deltas (§4.7).
        let total_extents = sb.total_extents();
        let alloc = Arc::new(
            ExtentAllocator::load(
                path,
                sb.alloc_bitmap.start,
                total_extents,
                compaction_reserve_extents(total_extents),
                PENDING_FREE_CAP,
                ledger.seq,
                &recovery.entries,
            )
            .await?,
        );

        // 5a. Node cache + trees from the ledger roots.
        let layout = NodeLayout::new(sb.node_size as usize)?;
        let cache = NodeCache::new(NodeCacheConfig {
            path: path.to_path_buf(),
            layout,
            heap_base: sb.heap.start,
            budget_bytes: node_cache_budget_bytes(),
            writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
        });
        cache.set_durable_tail(ledger.journal_tail_seq);
        let seq = Arc::new(AtomicU64::new(ledger.seq));
        let mut opened: Vec<KvTree> = Vec::with_capacity(3);
        for tree_id in [TREE_INODES, TREE_DENTRIES, TREE_XATTRS] {
            let root = ledger
                .tree_roots
                .iter()
                .find(|r| r.tree_id == tree_id)
                .ok_or_else(|| {
                    KvError::Corrupt(format!(
                        "{}: ledger record seq {} names no root for tree {tree_id}",
                        path.display(),
                        ledger.seq
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
            // Post-replay seq assignment stays above every node seq the
            // roots carry.
            seq.fetch_max(root.node_seq, Ordering::AcqRel);
            opened.push(tree);
        }
        let mut opened = opened.into_iter();
        let (inodes, dentries, xattrs) = (
            opened.next().expect("three trees"),
            opened.next().expect("three trees"),
            opened.next().expect("three trees"),
        );

        // 5b. Read-only replay into the cache: original seqs, per-key LWW
        // (§4.2 replay fold); allocator records were already consumed by
        // the K4 load; reserved-tree records are ignored (no v1 writer
        // emits them; the window never fails a mount loud).
        let mut max_replayed_ino: u64 = 0;
        for entry in &recovery.entries {
            for (tree_id, rec) in &entry.records {
                let tree = match *tree_id {
                    TREE_INODES => &inodes,
                    TREE_DENTRIES => &dentries,
                    TREE_XATTRS => &xattrs,
                    TREE_ALLOC_RESERVED => continue,
                    _ => continue,
                };
                if *tree_id == TREE_INODES {
                    if let Ok(ino) = decode_inode_key(&rec.key) {
                        max_replayed_ino = max_replayed_ino.max(ino);
                    }
                }
                tree.apply_replayed(
                    &rec.key,
                    rec.seq,
                    rec.kind,
                    Bytes::copy_from_slice(&rec.value),
                )
                .await?;
            }
        }

        // 5c. §4.8: next_ino = max(ledger watermark, replayed inos + 1).
        let next_ino = ledger.next_ino.max(max_replayed_ino + 1);

        let replay = KvReplayStats {
            entries: recovery.entries.len() as u64,
            dropped_torn: recovery.dropped_torn,
            replay_ms: t0.elapsed().as_millis() as u64,
        };
        Ok(Self {
            path: path.to_path_buf(),
            sb,
            ledger,
            inodes,
            dentries,
            xattrs,
            alloc,
            next_ino: AtomicU64::new(next_ino),
            replay,
        })
    }

    /// The mounted superblock.
    pub fn superblock(&self) -> &SuperblockV3 {
        &self.sb
    }

    /// The ledger record this mount selected (newest valid).
    pub fn mounted_ledger(&self) -> &LedgerRecord {
        &self.ledger
    }

    /// Mount replay statistics.
    pub fn replay_stats(&self) -> KvReplayStats {
        self.replay
    }

    /// The §4.8 monotonic ino watermark as recovered by this mount.
    pub fn next_ino(&self) -> u64 {
        self.next_ino.load(Ordering::Acquire)
    }

    /// Free heap extents right now (mount log / stats surface).
    pub fn free_extents(&self) -> u64 {
        self.alloc.free_extents()
    }

    /// The volume path.
    pub fn device_path(&self) -> &Path {
        &self.path
    }

    /// The three logical trees, tree-id order (inodes, dentries, xattrs)
    /// — the digest walk's input ([`super::builder::digest_walk`]).
    pub fn trees(&self) -> [&KvTree; 3] {
        [&self.inodes, &self.dentries, &self.xattrs]
    }

    /// The resolved OQ 2 contract class for every v3 volume:
    /// `meta_volume_atomicity = "cow-checksummed"` — satisfied by
    /// construction (§4.10; every unit checksummed, never-overwrite-live),
    /// independent of the physical probe reported alongside.
    pub fn atomicity_contract(&self) -> &'static str {
        META_VOLUME_ATOMICITY_COW
    }

    // -----------------------------------------------------------------
    // Read ops (the v2 `Metadata` read shapes, served by tree + fold).
    // -----------------------------------------------------------------

    /// Live-record scan over one hash chain window
    /// `(prefix, hash, 0) ..= (prefix, hash, 255)` — holes from deleted
    /// lower `coll_seq`s are naturally skipped because `range` yields only
    /// post-fold live records (§4.2).
    async fn chain_scan(
        &self,
        tree: &KvTree,
        start_key: &[u8],
        end_key: &[u8],
    ) -> std::result::Result<Vec<(Bytes, Bytes)>, KvError> {
        // A chain is ≤ 256 records by construction.
        tree.range(start_key, end_key, 256).await
    }

    /// Resolve `name` under `parent` to its dentry value, if live.
    async fn find_dentry(
        &self,
        parent: Ino,
        name: &str,
    ) -> std::result::Result<Option<DentryValue>, KvError> {
        if name.len() > 255 {
            return Ok(None); // unrepresentable ⇒ cannot exist
        }
        let hash = dentry_name_hash54(name.as_bytes(), self.sb.hash_seed);
        let start = dentry_key(parent, hash, 0);
        let end = dentry_key(parent, hash, u8::MAX);
        for (_k, v) in self.chain_scan(&self.dentries, &start, &end).await? {
            let d = DentryValue::decode(&v)?;
            if d.name == name.as_bytes() {
                return Ok(Some(d));
            }
        }
        Ok(None)
    }

    async fn read_inode_value(&self, ino: Ino) -> std::result::Result<Option<InodeValue>, KvError> {
        match self.inodes.lookup(&inode_key(ino)).await? {
            Some(v) => Ok(Some(InodeValue::decode(&v)?)),
            None => Ok(None),
        }
    }

    fn not_found(what: String) -> crate::error::SqueezefsError {
        crate::error::SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, what))
    }

    /// Resolve `name` under `parent` and return the child's attributes.
    /// Seeded-hash chain probe (§4.2): live records of the
    /// `(parent, hash54, *)` window compared by full name.
    pub async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        let dentry = self.find_dentry(parent, name).await?.ok_or_else(|| {
            Self::not_found(format!("Dentry {name} not found in parent {parent}"))
        })?;
        self.getattr(dentry.child_ino).await
    }

    /// Attributes of `ino` from the inode tree (K1 fold; Δtime deltas
    /// folded into the base record).
    pub async fn getattr(&self, ino: Ino) -> Result<Inode> {
        let v = self
            .read_inode_value(ino)
            .await?
            .ok_or_else(|| Self::not_found(format!("Inode {ino} not found")))?;
        Ok(Inode {
            ino,
            mode: v.mode,
            uid: v.uid,
            gid: v.gid,
            size: v.size,
            nlink: v.nlink,
            atime: v.atime,
            mtime: v.mtime,
            ctime: v.ctime,
            flags: v.flags,
        })
    }

    /// List `dir` per the module-docs offset contract; at most `max`
    /// entries, hash order (legal POSIX readdir order, risk R8). An ino
    /// with no dentries lists empty — the v2 contract (no existence
    /// check on the read path).
    pub async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        if max == 0 {
            return Ok(out);
        }
        // §5.1 resume rule: offsets 0/1/2 ⇒ the directory start (the
        // synthetic ./.. slots belong to the FUSE layer); c > 2 ⇒
        // strictly after the cookie's key suffix.
        let mut cursor: Vec<u8> = match decode_readdir_cookie(offset).map_err(KvError::from)? {
            ReaddirPos::Start | ReaddirPos::AfterDot | ReaddirPos::AfterDotDot => {
                dentry_key(dir, 0, 0).to_vec()
            }
            ReaddirPos::AfterEntry { hash54, coll_seq } => {
                key_successor(&dentry_key(dir, hash54, coll_seq))
            }
        };
        let end = dentry_key(dir, HASH54_MAX, u8::MAX);
        while out.len() < max {
            let want = (max - out.len()).min(SCAN_PAGE);
            let page = self.dentries.range(&cursor, &end, want).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (_k, v) in &page {
                let d = DentryValue::decode(v)?;
                out.push(DirEntry {
                    ino: d.child_ino,
                    name: String::from_utf8_lossy(&d.name).into_owned(),
                    file_type: u32::from(d.file_type) << 12,
                });
            }
        }
        Ok(out)
    }

    /// One xattr value; `Ok(None)` for absent names and inos alike (the
    /// v2 degrade contract).
    pub async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        if name.len() > 255 {
            return Ok(None);
        }
        let hash = xattr_name_hash56(name.as_bytes(), self.sb.hash_seed);
        let start = xattr_key(ino, hash, 0);
        let end = xattr_key(ino, hash, u8::MAX);
        for (_k, v) in self.chain_scan(&self.xattrs, &start, &end).await? {
            let x = XattrValue::decode(&v)?;
            if x.name == name.as_bytes() {
                return Ok(Some(x.value));
            }
        }
        Ok(None)
    }

    /// All xattr names of `ino`; empty for inos without xattrs (v2
    /// contract).
    pub async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut cursor: Vec<u8> = xattr_key(ino, 0, 0).to_vec();
        let end = xattr_key(ino, HASH56_MAX, u8::MAX);
        loop {
            let page = self.xattrs.range(&cursor, &end, SCAN_PAGE).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (_k, v) in &page {
                let x = XattrValue::decode(v)?;
                out.push(String::from_utf8_lossy(&x.name).into_owned());
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // PR K6b — the §4.4 commit pipeline + §4.6 checkpoint surface.
    // Tests-first commit: signatures only (`todo!()` bodies); the feat
    // commit fills them in.
    // -----------------------------------------------------------------

    /// The per-volume metadata lock manager (design §4.9 4a): the same
    /// `DlmLockManager` discipline the v2 backend embeds — I/D stripes
    /// acquired *before* any node lock (level 4b).
    pub fn dlm(&self) -> &crate::meta_backend::dlm::DlmLockManager {
        todo!("PR K6b: kv commit pipeline")
    }

    /// The mounted journal ring (K6a dropped it after replay; K6b stores
    /// it — the §4.4 admission/reservation core and the checkpoint task's
    /// `reusable_upto` watermark live here). Public for the crash harness,
    /// which computes physical entry offsets to arm write faults.
    pub fn journal_ring(&self) -> &super::journal::JournalRing {
        todo!("PR K6b: kv commit pipeline")
    }

    /// §4.8 monotonic ino allocation: one `fetch_add`, no reuse, no
    /// free-on-failure (a failed create burns the ino; crash-skipped
    /// ranges waste nothing that matters).
    pub fn allocate_ino(&self) -> Ino {
        todo!("PR K6b: kv commit pipeline")
    }

    /// §4.4 pt 4 escalation state: repeated journal-write failures latch
    /// the volume failed — every subsequent mutation returns `EIO` until
    /// remount (the `errors=remount-ro` analog). The routed layer mirrors
    /// this into `disabled_volumes`.
    pub fn is_failed(&self) -> bool {
        todo!("PR K6b: kv commit pipeline")
    }

    /// Ring-admission parks so far (§4.4 pt 5 `meta_kv_journal_full_stalls`
    /// — counted **before** any node lock is taken).
    pub fn journal_full_stalls(&self) -> u64 {
        todo!("PR K6b: kv commit pipeline")
    }

    /// Coalesced durability barrier for this volume (the v2
    /// `MetaLvBackend::sync_device` shape, riding the same
    /// `SyncCoalescer` group-commit discipline — §4.6 pt 4).
    pub async fn sync_device(&self) -> Result<()> {
        todo!("PR K6b: kv commit pipeline")
    }

    /// Force one full checkpoint cycle now (§4.6 pt 2): flush dirty
    /// nodes (snapshot-then-write), barrier, compute the tail, write the
    /// ledger slot, advance `reusable_upto` once durable. The background
    /// task calls this on cadence; tests and `shutdown` call it directly.
    pub async fn checkpoint_now(&self) -> std::result::Result<(), KvError> {
        todo!("PR K6b: checkpoint/writeback")
    }

    /// Clean unmount: final checkpoint (tail == head ⇒ an empty replay
    /// window on the next mount) + checkpoint-task drain (no leaked
    /// tasks — `tests/dismount_teardown_tests.rs`). Idempotent.
    pub async fn shutdown(&self) -> std::result::Result<(), KvError> {
        todo!("PR K6b: checkpoint/writeback")
    }

    /// Whether the background checkpoint task is still alive (a `Weak`
    /// probe for the teardown tests: `upgrade()` fails once the task has
    /// exited and dropped its liveness token).
    pub fn checkpoint_alive_probe(&self) -> std::sync::Weak<()> {
        todo!("PR K6b: checkpoint/writeback")
    }

    /// §4.8 batched destroy: one journal entry per batch carrying the
    /// inode `Delete`s plus each ino's enumerated xattr `Delete`s.
    pub async fn destroy_inodes(&self, inos: &[Ino]) -> Result<()> {
        let _ = inos;
        todo!("PR K6b: kv commit pipeline")
    }

    /// §5.3: layout xattr + size as ONE two-record transaction (v2's
    /// non-transactional two-write path, made atomic on v3).
    pub async fn set_layout_and_size(&self, ino: Ino, layout: &[u8], size: u64) -> Result<()> {
        let _ = (ino, layout, size);
        todo!("PR K6b: kv commit pipeline")
    }
}

/// The full mutating `Metadata` surface on v3 (PR K6b): every op the v2
/// backend serves, staged as a `KvTx` (records + read-your-own-writes
/// overlay) and committed through the §4.4 pipeline — pre-lock ring
/// admission, ascending-NodeId leaf locks with revalidate/retry, in-lock
/// reservation, out-of-lock entry write, seq-conditional rollback.
///
/// Error shapes and semantics mirror `MetaLvBackend`'s trait impl (the
/// dual-format conformance suite runs the same assertions against both).
#[async_trait::async_trait]
impl Metadata for KvMetaBackend {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        // The read side is live since K6a.
        KvMetaBackend::lookup(self, parent, name).await
    }

    async fn create(
        &self,
        _parent: Ino,
        _name: &str,
        _mode: u32,
        _uid: u32,
        _gid: u32,
    ) -> Result<Inode> {
        todo!("PR K6b: kv commit pipeline")
    }

    async fn unlink(&self, _parent: Ino, _name: &str) -> Result<Ino> {
        todo!("PR K6b: kv commit pipeline")
    }

    async fn link(&self, _ino: Ino, _new_parent: Ino, _new_name: &str) -> Result<Inode> {
        todo!("PR K6b: kv commit pipeline")
    }

    async fn rename(
        &self,
        _old_parent: Ino,
        _old_name: &str,
        _new_parent: Ino,
        _new_name: &str,
        _flags: u32,
    ) -> Result<()> {
        todo!("PR K6b: kv commit pipeline")
    }

    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        KvMetaBackend::readdir(self, dir, offset, max).await
    }

    async fn getattr(&self, ino: Ino) -> Result<Inode> {
        KvMetaBackend::getattr(self, ino).await
    }

    #[allow(clippy::too_many_arguments)] // the trait's signature
    async fn setattr(
        &self,
        _ino: Ino,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<u64>,
        _mtime: Option<u64>,
        _ctime: Option<u64>,
    ) -> Result<Inode> {
        todo!("PR K6b: kv commit pipeline")
    }

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        KvMetaBackend::getxattr(self, ino, name).await
    }

    async fn setxattr(&self, _ino: Ino, _name: &str, _value: &[u8]) -> Result<()> {
        todo!("PR K6b: kv commit pipeline")
    }

    async fn removexattr(&self, _ino: Ino, _name: &str) -> Result<()> {
        todo!("PR K6b: kv commit pipeline")
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        KvMetaBackend::listxattr(self, ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        // Single destroy == a size-1 batch: one code path (the v2 shape).
        self.destroy_inodes(std::slice::from_ref(&ino)).await
    }
}
