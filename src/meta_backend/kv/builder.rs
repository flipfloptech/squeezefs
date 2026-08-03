//! Offline bulk v3 image builder (PR K6a; design §5.2 `builder.rs`, §8
//! "gate-volume producer").
//!
//! Produces **complete, valid, checkpointed** v3 volume images from an
//! in-memory description (dirs / files / hard links / xattrs — layouts are
//! the `"layout"` xattr, §5.3): superblock, one root-ledger record naming
//! the built tree roots, a zeroed journal ring (nothing to replay), A/B
//! bitmap pages covering exactly the claimed extents, and bottom-up-packed
//! btree nodes written through the K2 node layer (`crate::uring_fs`,
//! io_uring-only).
//!
//! ## Determinism
//!
//! Identical descriptions (including `hash_seed` and `uuid` — the CLI
//! passes random ones; tests pass fixed ones) build **byte-identical
//! images**: inos are assigned monotonically per §4.8, record seqs are 0
//! (checkpoint-covered by construction: `journal_tail_seq` = 0 and every
//! bset horizon = 0), node seqs count up in a fixed tree-id-then-level
//! order, extents are claimed lowest-first (the K4 core's hint scan),
//! collision `coll_seq`s are assigned in sorted-name order within a
//! `(parent, hash)` group, and builder timestamps default to 0 unless
//! set. Determinism is what lets the §8 mount-time gates measure real
//! images.
//!
//! ## Fresh-volume hygiene
//!
//! `build` zeroes `[0, heap.start)` — superblock, ledger, ring, bitmap —
//! before writing (the §9 v3 quick-format rule): a stale ledger record or
//! checksummed ring page from a previous filesystem must never survive
//! into a fresh volume's mount-time recovery. Heap extents need no wipe:
//! unreferenced extents are unreachable through the FS API (§9), and a
//! reused extent's ghost bset frames carry foreign `node_seq` stamps the
//! K2 loader refuses as not-same-incarnation. The superblock is stamped
//! **last**, after an `fdatasync` barrier over everything it references —
//! the §6.2 single-sector flip discipline applied to format.

use super::alloc_ext::{compaction_reserve_extents, ExtentAllocator};
use super::backend::KvMetaBackend;
use super::checkpoint::{write_ledger_slot, LedgerRecord, TreeRoot};
use super::node::{key_successor, write_node, NodeLayout, NodeWriteParams, NODE_PAGE};
use super::record::{
    dentry_key, dentry_name_hash54, inode_key, xattr_key, xattr_name_hash56, DentryValue,
    InodeValue, Record, XattrValue, TREE_BLOCK_REFS, TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
};
use super::superblock::{write_superblock_v3, SuperblockV3};
use super::tree::{encode_interior_value, KvTree, KEY_SPACE_MAX};
use super::KvError;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;

/// The root directory's fixed ino (§4.8: ino 0 reserved, 1 = root,
/// `next_ino` starts above the built population).
pub const ROOT_INO: u64 = 1;

/// The root-ino xattr recording the volume-set format configuration; the
/// mount bootstrap reads it back off the first volume (`main.rs`).
pub const FORMAT_CONFIG_XATTR: &str = "user.squeezefs.format_config";

/// Pending-free FIFO capacity for builder-internal allocators (never
/// exercised — the builder only claims).
const BUILDER_PENDING_CAP: usize = 64;

/// Wipe/zero chunk size.
const ZERO_CHUNK: usize = 1024 * 1024;

/// Format-time identity + geometry knobs for one built image.
#[derive(Debug, Clone)]
pub struct BuilderConfig {
    /// Node size in bytes (validated against
    /// [`super::node::NodeLayout`]).
    pub node_size: usize,
    /// `--meta-journal-mb` override; `None` = the resolved OQ 1 clamp
    /// (`clamp(volume/64, 8 MiB, 32 MiB)` — §4.1).
    pub journal_len_override: Option<u64>,
    /// §4.2 seeded-hash key. Random in production ([`BuilderConfig::new`]);
    /// fixed in tests for deterministic images.
    pub hash_seed: u64,
    pub uuid: [u8; 16],
}

impl BuilderConfig {
    /// Production defaults: random `hash_seed`/`uuid` (the §9
    /// hash-flooding posture requires an unpredictable seed).
    pub fn new(node_size: usize) -> Self {
        Self {
            node_size,
            journal_len_override: None,
            hash_seed: rand::random::<u64>(),
            uuid: *uuid::Uuid::new_v4().as_bytes(),
        }
    }
}

/// One described inode: the packed record value (times default 0 — the
/// determinism contract) plus nothing else; names live in the dentry map.
#[derive(Debug, Clone)]
struct InodeSpec {
    value: InodeValue,
}

/// The in-memory volume description. Population methods reject duplicate
/// names, missing/non-directory parents, oversized names (> 255 B) and
/// values (> the §4.2 per-volume cap) with typed [`KvError`]s — a builder
/// input error must surface before any byte is written.
pub struct ImageBuilder {
    cfg: BuilderConfig,
    layout: NodeLayout,
    inodes: BTreeMap<u64, InodeSpec>,
    /// Per-directory `name → (child_ino, file_type)`; BTreeMap keeps
    /// name-sorted iteration (the coll_seq determinism rule).
    dentries: BTreeMap<u64, BTreeMap<Vec<u8>, (u64, u8)>>,
    /// `(ino, name) → value`, name-sorted per ino.
    xattrs: BTreeMap<(u64, Vec<u8>), Vec<u8>>,
    next_ino: u64,
    /// PR VL5a: the §5.5.1a membership stamp for a `--meta-slots`
    /// format. `Some` makes the built image slot-mapped: the bootstrap
    /// ledger record carries the stamp and the superblock carries
    /// `KV_GUEST_SLOTS` (bit 2). `None` (the default) builds the legacy
    /// byte-identical image.
    membership_stamp: Option<super::checkpoint::MembershipStamp>,
}

impl ImageBuilder {
    /// A description holding only the root directory (ino 1,
    /// `S_IFDIR | 0o755`, uid/gid 0, times 0).
    pub fn new(cfg: BuilderConfig) -> Result<Self, KvError> {
        let layout = NodeLayout::new(cfg.node_size)?;
        let mut inodes = BTreeMap::new();
        inodes.insert(
            ROOT_INO,
            InodeSpec {
                value: InodeValue {
                    mode: libc::S_IFDIR | 0o755,
                    nlink: 1,
                    ..Default::default()
                },
            },
        );
        let mut dentries = BTreeMap::new();
        dentries.insert(ROOT_INO, BTreeMap::new());
        Ok(Self {
            cfg,
            layout,
            inodes,
            dentries,
            xattrs: BTreeMap::new(),
            next_ino: ROOT_INO + 1,
            membership_stamp: None,
        })
    }

    /// Make the built image a slot-mapped set member (PR VL5a): the
    /// bootstrap ledger record carries `stamp` and the superblock
    /// carries `KV_GUEST_SLOTS`. The stamp participates in the
    /// determinism contract like every other input (fixed stamp ⇒ fixed
    /// image).
    pub fn set_membership_stamp(&mut self, stamp: super::checkpoint::MembershipStamp) {
        self.membership_stamp = Some(stamp);
    }

    fn check_name(name: &str) -> Result<(), KvError> {
        if name.is_empty() {
            return Err(KvError::Corrupt("empty entry name".to_string()));
        }
        if name.len() > 255 {
            return Err(KvError::NameTooLong { len: name.len() });
        }
        Ok(())
    }

    /// Bind `name` under `parent` to `(child, file_type)`, with the
    /// duplicate/parent checks shared by every population method.
    fn bind(&mut self, parent: u64, name: &str, child: u64, dt: u8) -> Result<(), KvError> {
        Self::check_name(name)?;
        let parent_spec = self.inodes.get(&parent).ok_or_else(|| {
            KvError::Corrupt(format!(
                "parent ino {parent} does not exist in the description"
            ))
        })?;
        if parent_spec.value.mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(KvError::Corrupt(format!(
                "parent ino {parent} is not a directory"
            )));
        }
        let dir = self.dentries.entry(parent).or_default();
        if dir.contains_key(name.as_bytes()) {
            return Err(KvError::Corrupt(format!(
                "duplicate name {name:?} under ino {parent}"
            )));
        }
        dir.insert(name.as_bytes().to_vec(), (child, dt));
        Ok(())
    }

    fn add_inode(&mut self, value: InodeValue) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, InodeSpec { value });
        ino
    }

    /// Add a directory under `parent`; returns its ino.
    pub fn add_dir(
        &mut self,
        parent: u64,
        name: &str,
        perm: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64, KvError> {
        let mode = libc::S_IFDIR | (perm & 0o7777);
        // Validate against the parent before allocating the ino, so a
        // refused add leaves the description untouched.
        Self::check_name(name)?;
        let ino = self.next_ino; // provisional — bound below
        self.bind(parent, name, ino, dt_of(mode))?;
        let ino = self.add_inode(InodeValue {
            mode,
            uid,
            gid,
            nlink: 1,
            ..Default::default()
        });
        self.dentries.entry(ino).or_default();
        Ok(ino)
    }

    /// Add a regular file under `parent`; returns its ino.
    pub fn add_file(
        &mut self,
        parent: u64,
        name: &str,
        perm: u32,
        uid: u32,
        gid: u32,
        size: u64,
    ) -> Result<u64, KvError> {
        let mode = libc::S_IFREG | (perm & 0o7777);
        Self::check_name(name)?;
        let ino = self.next_ino; // provisional — bound below
        self.bind(parent, name, ino, dt_of(mode))?;
        let ino = self.add_inode(InodeValue {
            mode,
            uid,
            gid,
            nlink: 1,
            size,
            ..Default::default()
        });
        Ok(ino)
    }

    /// Add a hard link: one more name for an existing non-directory ino
    /// (`nlink` maintained).
    pub fn add_link(&mut self, ino: u64, parent: u64, name: &str) -> Result<(), KvError> {
        let target = self
            .inodes
            .get(&ino)
            .ok_or_else(|| KvError::Corrupt(format!("link target ino {ino} does not exist")))?;
        let mode = target.value.mode;
        if mode & libc::S_IFMT == libc::S_IFDIR {
            return Err(KvError::Corrupt(format!(
                "hard links to directories are not allowed (ino {ino})"
            )));
        }
        self.bind(parent, name, ino, dt_of(mode))?;
        let spec = self.inodes.get_mut(&ino).expect("checked above");
        spec.value.nlink += 1;
        Ok(())
    }

    /// Set (or replace) one xattr on `ino`. The `"layout"` xattr — the
    /// data path's `LayoutMetadata` bytes (§5.3) — travels through here
    /// like any other value.
    pub fn set_xattr(&mut self, ino: u64, name: &str, value: &[u8]) -> Result<(), KvError> {
        Self::check_name(name)?;
        if !self.inodes.contains_key(&ino) {
            return Err(KvError::Corrupt(format!(
                "xattr target ino {ino} does not exist"
            )));
        }
        // The record value is the encoded XattrValue; enforce the §4.2
        // per-volume cap at description time so the error carries the
        // caller's context, not a mid-build failure.
        let encoded_len = 1 + name.len() + value.len();
        let cap = self.layout.record_value_cap();
        if encoded_len > cap {
            return Err(KvError::ValueTooLarge {
                len: encoded_len,
                cap,
            });
        }
        self.xattrs
            .insert((ino, name.as_bytes().to_vec()), value.to_vec());
        Ok(())
    }

    /// Override an ino's timestamps (ns). Builder defaults are 0 — see
    /// the determinism contract in the module docs.
    pub fn set_times(
        &mut self,
        ino: u64,
        atime: u64,
        mtime: u64,
        ctime: u64,
    ) -> Result<(), KvError> {
        let spec = self
            .inodes
            .get_mut(&ino)
            .ok_or_else(|| KvError::Corrupt(format!("ino {ino} does not exist")))?;
        spec.value.atime = atime;
        spec.value.mtime = mtime;
        spec.value.ctime = ctime;
        Ok(())
    }

    /// Override an inode's owner (the `format_v3` root-stamping surface;
    /// deterministic images keep the 0:0 default).
    pub fn set_owner(&mut self, ino: u64, uid: u32, gid: u32) -> Result<(), KvError> {
        let spec = self
            .inodes
            .get_mut(&ino)
            .ok_or_else(|| KvError::Corrupt(format!("ino {ino} does not exist")))?;
        spec.value.uid = uid;
        spec.value.gid = gid;
        Ok(())
    }

    /// Inodes described so far (root included).
    pub fn inode_count(&self) -> u64 {
        self.inodes.len() as u64
    }

    // -----------------------------------------------------------------
    // Record assembly (key-sorted, seq 0, checkpoint-covered).
    // -----------------------------------------------------------------

    fn inode_records(&self) -> Vec<Record> {
        // BTreeMap iteration is ino order == big-endian key order.
        self.inodes
            .iter()
            .map(|(ino, spec)| Record::put(inode_key(*ino).to_vec(), 0, spec.value.encode()))
            .collect()
    }

    fn dentry_records(&self) -> Result<Vec<Record>, KvError> {
        let mut out: BTreeMap<Vec<u8>, Record> = BTreeMap::new();
        for (parent, dir) in &self.dentries {
            // Group same-hash names; name-sorted outer iteration makes
            // coll_seq assignment deterministic (module docs).
            let mut groups: BTreeMap<u64, u16> = BTreeMap::new();
            for (name, (child, dt)) in dir {
                let hash = dentry_name_hash54(name, self.cfg.hash_seed);
                let coll = groups.entry(hash).or_insert(0);
                if *coll > u16::from(u8::MAX) {
                    super::META_KV_DENTRY_COLLISION_OVERFLOWS.fetch_add(1, Ordering::Relaxed);
                    return Err(KvError::DentryChainOverflow);
                }
                let key = dentry_key(*parent, hash, *coll as u8).to_vec();
                *coll += 1;
                let value = DentryValue {
                    child_ino: *child,
                    file_type: *dt,
                    name: name.clone(),
                }
                .encode()?;
                out.insert(key, Record::put(Vec::new(), 0, value));
            }
        }
        // Keys were assembled out of memcmp order (parent-major but
        // hash-shuffled within a directory); the BTreeMap re-sorts.
        Ok(out
            .into_iter()
            .map(|(key, mut rec)| {
                rec.key = key;
                rec
            })
            .collect())
    }

    fn xattr_records(&self) -> Result<Vec<Record>, KvError> {
        let mut out: BTreeMap<Vec<u8>, Record> = BTreeMap::new();
        let mut groups: BTreeMap<(u64, u64), u16> = BTreeMap::new();
        for ((ino, name), value) in &self.xattrs {
            let hash = xattr_name_hash56(name, self.cfg.hash_seed);
            let coll = groups.entry((*ino, hash)).or_insert(0);
            if *coll > u16::from(u8::MAX) {
                return Err(KvError::Corrupt(format!(
                    "xattr hash-collision chain full on ino {ino} (coll_seq 0..=255 occupied)"
                )));
            }
            let key = xattr_key(*ino, hash, *coll as u8).to_vec();
            *coll += 1;
            let enc = XattrValue {
                name: name.clone(),
                value: value.clone(),
            }
            .encode()?;
            out.insert(key, Record::put(Vec::new(), 0, enc));
        }
        Ok(out
            .into_iter()
            .map(|(key, mut rec)| {
                rec.key = key;
                rec
            })
            .collect())
    }

    // -----------------------------------------------------------------
    // Image assembly.
    // -----------------------------------------------------------------

    /// Build the image into `path` (a file or block device of
    /// `volume_len` usable bytes): plan geometry, zero the fixed
    /// structures, pack leaves bottom-up per tree, write interior levels,
    /// persist the bitmap, write the bootstrap ledger record, and stamp
    /// the superblock **last** behind an `fdatasync` barrier (nothing
    /// references a half-built image — the §6.2 flip discipline applied
    /// to format).
    pub async fn build(&self, path: &Path, volume_len: u64) -> Result<BuiltImage, KvError> {
        let mut sb = SuperblockV3::plan(
            volume_len,
            self.cfg.node_size,
            self.cfg.journal_len_override,
            self.cfg.uuid,
            self.cfg.hash_seed,
        )?;
        if self.membership_stamp.is_some() {
            // Every stamped member carries the stamp bits (2/4) beside
            // the format-time dynamic-routing bit 6 (the superblock plan
            // default). The §5.5.1a bit-before-first-stamp invariant is
            // subsumed by format's flip discipline: sector 0 was zeroed
            // above the stamped ledger write, and the bit-carrying
            // superblock is stamped LAST behind the barrier — no crash
            // prefix leaves a stamped volume mountable by ANY binary
            // without the bits.
            sb.features_incompat |= super::superblock::FEATURE_INCOMPAT_KV_GUEST_SLOTS
                | super::superblock::FEATURE_INCOMPAT_KV_SLOT_MIGRATION;
        }

        // **Test seam** (`SQUEEZEFS_TEST_STAMP_BLOCK_REFS=1`): stamp
        // incompat bit 8 at format so a suite can exercise the DURABLE
        // block-accounting path end to end. Production formats never carry
        // it (ruling D9 — `SuperblockV3::plan` omits it, and the reason is a
        // safety property: see the constant's doc), so this is the only way
        // to point the existing write-path suites at the ledger and let the
        // §6.2-item-1 oracle grade the wiring — which is exactly how the
        // remaining drift was found and closed.
        //
        // Read through the ONE env-knob convention (ENG-10 — registered in
        // `env_knobs::KNOBS`, so a malformed value refuses at startup rather
        // than silently mis-stamping), never set in production (the
        // `SQUEEZEFS_TEST_POWER_CUT_DEVS` / `SQUEEZEFS_TEST_WRITE_STALL_MS`
        // precedent). It only ever ADDS the bit, so a volume it creates is
        // indistinguishable from one the Phase-8 window stamped.
        if crate::env_knobs::bool_knob("SQUEEZEFS_TEST_STAMP_BLOCK_REFS", false) {
            sb.features_incompat |= super::superblock::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS;
        }

        // **Test seam** (`SQUEEZEFS_TEST_STAMP_WRITER_SCOPE=1`): stamp
        // incompat bit 10 (§6.2 items 8/10 — writer-scoped staging) at
        // format, the same posture and for the same reason as the block-refs
        // seam above: production formats never carry it (ruling D9), so this
        // is the only way to point the staging/extent/recovery suites at the
        // scoped key + node-scoped stamp path end to end.
        if crate::env_knobs::bool_knob("SQUEEZEFS_TEST_STAMP_WRITER_SCOPE", false) {
            sb.features_incompat |= super::superblock::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING;
        }

        // §9 quick-format hygiene: zero SB + ledger + ring + bitmap.
        zero_range(path, 0, sb.heap.start).await?;

        let total_extents = sb.total_extents();
        let alloc = ExtentAllocator::format(
            total_extents,
            compaction_reserve_extents(total_extents),
            BUILDER_PENDING_CAP,
        );

        let mut writer = TreeWriter {
            path,
            layout: &self.layout,
            heap_base: sb.heap.start,
            alloc: &alloc,
            // Generation-namespaced (uuid-derived) so heap extents reused
            // across a quick reformat never chain the dead generation's
            // tail bsets — see [`node_seq_base`].
            next_node_seq: node_seq_base(self.cfg.uuid),
            nodes_written: 0,
        };
        // Spec §6.2 item 1 (incompat bit 8): the FORMAT-TIME half of the
        // durable block-reference tree — an EMPTY root, one node, so a
        // bit-8 image needs no structural mutation at first mount.
        //
        // Keyed on the decoded on-disk bit, and per ruling D9 nothing sets
        // that bit at format today (`SuperblockV3::plan` deliberately
        // omits it), so the branch is inert on every image this binary
        // writes and the MOUNT-side mint is what runs. It stays because it
        // is the correct format-time behavior for the bit — the Phase-8
        // reformat window and the eventual `plan` re-add both land on it —
        // and because a builder that silently produced a bit-8 image with
        // no root would be a mount refusal, not a degradation.
        let block_refs =
            sb.features_incompat & super::superblock::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS != 0;
        let mut planned: Vec<(u8, Vec<Record>)> = vec![
            (TREE_INODES, self.inode_records()),
            (TREE_DENTRIES, self.dentry_records()?),
            (TREE_XATTRS, self.xattr_records()?),
        ];
        if block_refs {
            planned.push((TREE_BLOCK_REFS, Vec::new()));
        }
        let mut tree_roots = Vec::with_capacity(planned.len());
        for (tree_id, records) in planned {
            let (addr, seq) = writer.write_tree(tree_id, records).await?;
            tree_roots.push(TreeRoot {
                tree_id,
                node_addr: addr,
                node_seq: seq,
            });
        }
        let nodes_written = writer.nodes_written;
        let node_seq_watermark = writer.next_node_seq;

        // Persist the allocator's claimed-extent bitmap (A slots,
        // generation 1 — the ledger names it).
        alloc
            .write_dirty_pages(path, sb.alloc_bitmap.start, 1)
            .await?;

        // The bootstrap checkpoint record: fresh ring (tail 0), the §4.8
        // watermark, generation 1 bitmap.
        let ledger = LedgerRecord {
            seq: 1,
            tree_roots,
            journal_tail_seq: 0,
            next_ino: self.next_ino,
            alloc_bitmap_generation: 1,
            node_seq_watermark,
            membership_stamp: self.membership_stamp.clone(),
            // Ruling D9: format never stamps the partitioned-append bit,
            // so a fresh volume's bootstrap record is the pre-partition
            // (suffix-less) form — byte-identical to the shipped image.
            append_partition: None,
        };
        write_ledger_slot(path, sb.root_ledger.start, &ledger).await?;

        // Barrier, then the single-sector commit point, then make IT
        // durable too.
        crate::uring_fs::fdatasync(path.to_path_buf()).await?;
        write_superblock_v3(path, &sb).await?;
        crate::uring_fs::fdatasync(path.to_path_buf()).await?;

        Ok(BuiltImage {
            superblock: sb,
            ledger_seq: ledger.seq,
            next_ino: self.next_ino,
            nodes_written,
            extents_allocated: nodes_written,
        })
    }
}

/// `d_type`-style byte for a mode's `S_IFMT` bits (`DentryValue`'s u8
/// field); the read side reconstructs the historical
/// `file_type = mode & S_IFMT` convention by shifting back.
fn dt_of(mode: u32) -> u8 {
    ((mode & libc::S_IFMT) >> 12) as u8
}

/// Node-seq base for a fresh image, namespaced by the volume's generation
/// `uuid` (random per format invocation): quick format zeroes only
/// `[0, heap.start)`, so heap extents keep the DEAD generation's appended
/// tail-bset frames — if node seqs restarted at 0 every generation (they
/// did), those frames satisfy the §4.5 `node_seq_at_write == node_seq`
/// chain check on the fresh image's identically-seqed nodes and the dead
/// tree's records resurrect. Deriving the base from the uuid makes a
/// cross-generation seq collision as improbable as a checksum collision
/// while keeping the builder's determinism contract (fixed uuid ⇒ fixed
/// image). Top bit cleared: 2^63 of monotonic headroom before wrap.
fn node_seq_base(uuid: [u8; 16]) -> u64 {
    u64::from_le_bytes(uuid[..8].try_into().expect("8-byte slice")) & (u64::MAX >> 1)
}

/// Zero `[start, start + len)` in bounded chunks via `uring_fs`.
async fn zero_range(path: &Path, start: u64, len: u64) -> Result<(), KvError> {
    let zeros = bytes::Bytes::from(vec![0u8; ZERO_CHUNK]);
    let mut off = start;
    let end = start + len;
    while off < end {
        let n = usize::try_from((end - off).min(ZERO_CHUNK as u64)).expect("chunk fits usize");
        crate::uring_fs::write_at(path, off, zeros.slice(..n)).await?;
        off += n as u64;
    }
    Ok(())
}

/// Bottom-up tree writer: leaves greedily packed to ~3/4 of a node's
/// usable bytes (append headroom — the K5 split-fill convention), then
/// interior levels of `(child max_key → (addr, seq))` separators until a
/// single root remains. Extents claim lowest-first; node seqs count up in
/// write order — both deterministic.
struct TreeWriter<'a> {
    path: &'a Path,
    layout: &'a NodeLayout,
    heap_base: u64,
    alloc: &'a ExtentAllocator,
    next_node_seq: u64,
    nodes_written: u64,
}

impl TreeWriter<'_> {
    fn usable_budget(&self) -> usize {
        let usable = self.layout.node_size()
            - NODE_PAGE
            - super::node::BSET_FRAME_LEN
            - super::bset::BSET_HEADER_LEN;
        usable * 3 / 4
    }

    async fn write_tree(
        &mut self,
        tree_id: u8,
        records: Vec<Record>,
    ) -> Result<(u64, u64), KvError> {
        // Level 0.
        let mut level: u8 = 0;
        let mut nodes = self.write_level(tree_id, level, &records).await?;
        // Interior levels until one root spans the key space.
        while nodes.len() > 1 {
            level = level
                .checked_add(1)
                .ok_or_else(|| KvError::Corrupt("tree deeper than 255 levels".to_string()))?;
            let separators: Vec<Record> = nodes
                .iter()
                .map(|(addr, seq, max_key)| {
                    Record::put(max_key.clone(), 0, encode_interior_value(*addr, *seq))
                })
                .collect();
            nodes = self.write_level(tree_id, level, &separators).await?;
        }
        let (addr, seq, _) = nodes.pop().expect("write_level yields at least one node");
        Ok((addr, seq))
    }

    /// Write one whole level: `records` chunked by the byte budget, each
    /// chunk one node, sibling bounds partitioning the key space gap-free
    /// (`next.min = successor(prev.max)`; first min = `""`, last max =
    /// [`KEY_SPACE_MAX`] — the K2/K5 rule). Returns
    /// `(addr, node_seq, max_key)` per node, left to right.
    async fn write_level(
        &mut self,
        tree_id: u8,
        level: u8,
        records: &[Record],
    ) -> Result<Vec<(u64, u64, Vec<u8>)>, KvError> {
        let budget = self.usable_budget();
        let chunks: Vec<&[Record]> = if records.is_empty() {
            vec![&records[..]]
        } else {
            chunk_by_encoded_len(records, budget)
        };
        let mut out = Vec::with_capacity(chunks.len());
        let mut min_key: Vec<u8> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let max_key: Vec<u8> = if i + 1 == chunks.len() {
                KEY_SPACE_MAX.to_vec()
            } else {
                chunk
                    .last()
                    .expect("non-final chunks are non-empty")
                    .key
                    .clone()
            };
            let extent = self.alloc.claim_internal()?;
            let addr = self.heap_base + extent * self.layout.node_size() as u64;
            self.next_node_seq += 1;
            let node_seq = self.next_node_seq;
            write_node(
                self.path,
                self.layout,
                &NodeWriteParams {
                    node_addr: addr,
                    node_seq,
                    tree_id,
                    level,
                    min_key: &min_key,
                    max_key: &max_key,
                },
                chunk,
                0,
            )
            .await?;
            self.nodes_written += 1;
            min_key = key_successor(&max_key);
            out.push((addr, node_seq, max_key));
        }
        Ok(out)
    }
}

/// Greedy chunking of key-ascending records into runs of ≤ `budget`
/// encoded bytes (every run non-empty; a single record never exceeds the
/// budget — the §4.2 value cap guarantees it at every node size).
fn chunk_by_encoded_len(records: &[Record], budget: usize) -> Vec<&[Record]> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut acc = 0usize;
    for (i, r) in records.iter().enumerate() {
        let len = r.record_ref().encoded_len();
        if acc + len > budget && i > start {
            chunks.push(&records[start..i]);
            start = i;
            acc = 0;
        }
        acc += len;
    }
    chunks.push(&records[start..]);
    chunks
}

/// Summary of one built image (assertions + mount logs).
#[derive(Debug, Clone)]
pub struct BuiltImage {
    pub superblock: SuperblockV3,
    /// The bootstrap ledger record's checkpoint seq.
    pub ledger_seq: u64,
    /// `next_ino` recorded in the ledger (§4.8 watermark).
    pub next_ino: u64,
    /// Btree nodes written (all trees, all levels).
    pub nodes_written: u64,
    /// Heap extents claimed (== nodes written; one extent per node).
    pub extents_allocated: u64,
}

/// The §4.10 post-fold digest walk: xxh3 over every **live** record of
/// the given trees — `(tree_id, key, folded value)` in tree-id-then-key
/// order; tombstones and unfolded deltas excluded — so two states compare
/// by user-visible content, not physical encoding. Used by the builder
/// determinism tests and the torn-ledger fallback crash case.
///
/// The `writer_claim` record (PR M1, design-metadata-throughput §5.0) is
/// **excluded**: it is per-mount guard state, unique to every mount *by
/// design* (fresh writer id + heartbeat), not user-visible content — two
/// replays of one filesystem under different mounts must digest equal.
///
/// `writer_term` (DLM S2, spec §6.11) is excluded for the **same reason and
/// it is not optional**: the gate bumps the durable fencing era on every
/// claim and the record is never deleted, so including it makes the digest
/// change on every mount by construction — which is precisely what the
/// sentence above forbids. Leaving it in broke
/// `crash_contract_tests::test_kv_v3_torn_newest_ledger_mount_serves_predecessor`
/// (post-fold digest mismatch) and two `kv_backend` tests. **Any future
/// per-mount control record belongs on this exclusion list**; the general
/// rule is `is_pinned_control_record`-shaped state, never user content.
pub async fn digest_walk(trees: &[&KvTree]) -> Result<u64, KvError> {
    const WALK_PAGE: usize = 1024;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    for tree in trees {
        h.update(&[tree.tree_id()]);
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            let page = tree.range(&cursor, &KEY_SPACE_MAX, WALK_PAGE).await?;
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last_key);
            for (k, v) in &page {
                if tree.tree_id() == TREE_XATTRS
                    && XattrValue::decode(v)
                        .map(|x| {
                            x.name == super::backend::WRITER_CLAIM_XATTR.as_bytes()
                                // DLM S2 (incompat bit 7): the durable
                                // writer TERM is the same class as the
                                // claim — mount-guard state, not
                                // filesystem content — and it is
                                // MONOTONE PER MOUNT by design, so
                                // including it made every
                                // digest-across-remount comparison
                                // structurally false (era N vs era N+1).
                                || x.name == super::backend::WRITER_TERM_XATTR.as_bytes()
                        })
                        .unwrap_or(false)
                {
                    continue; // mount-guard state, not filesystem content
                }
                h.update(&(k.len() as u64).to_le_bytes());
                h.update(k);
                h.update(&(v.len() as u64).to_le_bytes());
                h.update(v);
            }
        }
    }
    Ok(h.digest())
}

/// Convenience: [`digest_walk`] over a mounted backend's three trees.
pub async fn digest_backend(backend: &KvMetaBackend) -> Result<u64, KvError> {
    digest_walk(&backend.trees()).await
}

/// `squeezefs format` options for one metadata volume (the CLI arm's
/// contract, §5.1).
#[derive(Debug, Clone)]
pub struct FormatV3Options {
    pub node_size: usize,
    /// `--meta-journal-mb` (bytes); `None` = the OQ 1 clamp.
    pub journal_len_override: Option<u64>,
    /// `--force` (the preflight contract: an already-formatted volume is
    /// refused without it; live clients refuse even with it).
    pub force: bool,
    /// `--full`: zero the whole device (forensic erasure, §9) instead of
    /// just the fixed structures.
    pub full_wipe: bool,
    /// `user.squeezefs.format_config` bytes to record on the root ino of
    /// the first volume (the mount-time bootstrap config).
    pub format_config_xattr: Option<Vec<u8>>,
}

/// No-side-effect format gate (run standalone by the CLI across ALL
/// volumes before ANY volume is wiped, so a refused multi-volume format
/// leaves everything intact; also the first step of [`format_v3`]).
///
/// Policy (pinned by `tests/format_guard_tests.rs` /
/// `tests/mount_registration_tests.rs`):
/// - blank / never-formatted volume: formatting allowed;
/// - already-formatted volume (any SqueezeFS superblock — v3 or a legacy
///   v2 one): refused without `force` — even idle — so a fat-fingered
///   format cannot silently destroy a filesystem;
/// - **live** client registrations (fresh heartbeat, read from the v3
///   xattr tree of the root ino via a task-free probe mount) refuse
///   format even WITH `force`: reformatting under an active mount is
///   never safe. Stale registrations (heartbeat older than
///   [`crate::fuse_client::CLIENT_STALE_TTL_SECS`], i.e. crashed clients)
///   never block. Legacy-v2 volumes cannot be probed (no v2 reader
///   exists) and cannot be live-mounted by this binary — `force` is the
///   gate there.
/// - a volume whose sector 0 **reads back but refuses classification**
///   (a pre-watermark v3 superblock — Finding A, unknown future incompat
///   bits, a version above 3, a torn superblock checksum, foreign magic)
///   degrades to the same plain `force` gate: without `force` the refusal
///   carries the classification reason plus the `--force` remedy; with
///   `force` the format proceeds. The live-client probe is impossible
///   there AND unnecessary **by construction**: this binary refuses to
///   mount every one of those classes, so no live *current* client can
///   exist on such a volume (a current client could never have mounted
///   it). Anything else strands the operator — the mount refusal demands
///   the very reformat the gate would be blocking (the user-hit
///   pre-watermark `format --force` regression). Device **I/O errors**
///   still propagate: "pass `--force`" would be a lie when the volume
///   cannot even be read.
///
/// The probe is **read-only**: no checkpoint task is spawned and nothing
/// is written, so preflighting a volume another process has live-mounted
/// can never corrupt it (the whole point of the check). A v3 volume whose
/// probe mount fails (corrupt) degrades to the plain `force` gate — a
/// broken volume must stay reformattable.
pub async fn format_preflight(
    path: &Path,
    force: bool,
) -> Result<(), crate::error::SqueezefsError> {
    use super::superblock::{classify_volume, VolumeFormat};
    // `refused`: the classification reason for a non-blank volume this
    // binary refuses to interpret (see the policy above) — carried into
    // the no-`force` refusal so the operator sees WHAT is on the volume
    // alongside the `--force` remedy.
    let mut refused: Option<String> = None;
    match classify_volume(path).await {
        Ok(VolumeFormat::Blank) => return Ok(()), // never formatted: nothing to protect
        Ok(VolumeFormat::V2Legacy) => {}
        Ok(VolumeFormat::V3(_)) => {
            if let Ok(be) = KvMetaBackend::open_probe(path).await {
                // PR M1 (design-metadata-throughput §5.0): the live sweep
                // covers `client:* ∪ writer_claim` — the mount guard's
                // claim record marks a live writer exactly like a
                // registration marks a live client, under the ONE
                // staleness law. `mount_registrations` is the shared
                // reader (also the `squeezefs clients`/`status` surface);
                // the preflight's predicate is heartbeat freshness alone
                // (a kill -9'd holder's records block format until the
                // TTL, unchanged — reformat-under-crash stays TTL-gated).
                let live: Vec<String> = be
                    .mount_registrations()
                    .await
                    .into_iter()
                    .filter(|r| r.heartbeat_fresh)
                    .map(|r| r.key)
                    .collect();
                if !live.is_empty() {
                    return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                        "Cannot format: metadata volume is actively mounted by clients: {:?}",
                        live
                    )));
                }
            }
        }
        // Sector 0 was READ but refused interpretation (every
        // classification refusal is `KvError::Corrupt`): unsupported or
        // unreadable prior state — pre-watermark v3, unknown incompat
        // bits, future versions, torn superblocks, foreign magic. No
        // live-client probe is possible, and none is needed: this binary
        // cannot mount such a volume, so it cannot host a live current
        // client. `--force` must be able to clobber it — the refused
        // classes' own mount errors demand exactly that reformat.
        Err(KvError::Corrupt(reason)) => refused = Some(reason),
        // A device that cannot be read at all is a real error, not a
        // guarded format: propagate loud (formatting would fail anyway).
        Err(e) => return Err(e.into()),
    }

    if !force {
        return Err(crate::error::SqueezefsError::InvalidOperation(
            match refused {
                Some(reason) => format!(
                    "Metadata volume carries prior on-disk state ({reason}); refusing to destroy \
                 it. Pass --force to reformat."
                ),
                None => {
                    "Metadata volume is already formatted as SqueezeFS; refusing to destroy it. \
                     Pass --force to reformat."
                        .to_string()
                }
            },
        ));
    }
    Ok(())
}

/// The public v3 formatter (what `squeezefs format` calls): runs
/// [`format_preflight`] (already-formatted volumes refused without
/// `force`, live clients refuse even with it), grows regular files to
/// `volume_len`, optionally full-wipes, then builds an empty (plus
/// optional config xattr) image via [`ImageBuilder`]. The built image
/// is a SINGLE-MEMBER dynamic-routing set (synthesized stamp, derived
/// width — design-dynamic-meta-routing §5.1); multi-member sets format
/// each member through [`format_v3_stamped`] with one shared plan.
pub async fn format_v3(
    path: &Path,
    volume_len: u64,
    opts: &FormatV3Options,
) -> Result<BuiltImage, crate::error::SqueezefsError> {
    format_v3_inner(path, volume_len, opts, None).await
}

/// [`format_v3`] for one member of a multi-volume set (PR VL5a,
/// design-volume-lifecycle §5.5.1a): the built image carries the
/// caller's `stamp` (one shared [`crate::meta_backend::MetaSlotPlan`]
/// across the set) in its bootstrap ledger record, plus the stamp bits
/// on its superblock.
pub async fn format_v3_stamped(
    path: &Path,
    volume_len: u64,
    opts: &FormatV3Options,
    stamp: super::checkpoint::MembershipStamp,
) -> Result<BuiltImage, crate::error::SqueezefsError> {
    format_v3_inner(path, volume_len, opts, Some(stamp)).await
}

async fn format_v3_inner(
    path: &Path,
    volume_len: u64,
    opts: &FormatV3Options,
    stamp: Option<super::checkpoint::MembershipStamp>,
) -> Result<BuiltImage, crate::error::SqueezefsError> {
    format_preflight(path, opts.force).await?;

    // Regular files grow to the requested volume length (control-path
    // one-shot; ftruncate has no uring opcode).
    let meta = std::fs::metadata(path).map_err(crate::error::SqueezefsError::Io)?;
    if meta.is_file() && meta.len() < volume_len {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(crate::error::SqueezefsError::Io)?;
        f.set_len(volume_len)
            .map_err(crate::error::SqueezefsError::Io)?;
    }

    if opts.full_wipe {
        zero_range(path, 0, volume_len).await?;
    }

    // Dynamic meta routing (design-dynamic-meta-routing §5.1): EVERY
    // format is a stamped set member now — a caller-less single-volume
    // format synthesizes its own single-member plan (fresh set uuid,
    // derived width, one stride run), so growth by `volume add-meta` is
    // open to every filesystem from birth.
    let stamp = match stamp {
        Some(st) => st,
        None => {
            let mut plan = crate::meta_backend::plan_meta_slot_set(1)?;
            plan.stamps.remove(0)
        }
    };
    let mut builder = ImageBuilder::new(BuilderConfig {
        node_size: opts.node_size,
        journal_len_override: opts.journal_len_override,
        // Set members share ONE set-wide hash seed, derived from the
        // (random, per-format) set uuid — record keys must stay
        // byte-identical across hosts or a migrated slot's seeded
        // dentry/xattr hashes could never resolve on its new volume.
        // Same §9 secrecy class as the per-volume seed (both are minted
        // from fresh format-time randomness and both live plaintext in
        // sector 0).
        hash_seed: xxhash_rust::xxh3::xxh3_64(&stamp.set_uuid),
        ..BuilderConfig::new(opts.node_size)
    })?;
    // Root stamping: the root directory belongs to the INVOKING user
    // (SUDO_UID:SUDO_GID under sudo — raw getuid() is root there, which
    // made every user-mode mount EACCES on create; genuine root stays
    // root), or an unprivileged mount cannot create anything under it.
    // The BUILDER default stays 0:0 (its determinism contract); the
    // public formatter is the user-facing surface.
    let (root_uid, root_gid) = crate::config_ops::invoking_owner();
    builder.set_owner(ROOT_INO, root_uid, root_gid)?;
    if let Some(cfg) = &opts.format_config_xattr {
        builder.set_xattr(ROOT_INO, FORMAT_CONFIG_XATTR, cfg)?;
    }
    builder.set_membership_stamp(stamp);
    Ok(builder.build(path, volume_len).await?)
}
