//! Offline bulk v3 image builder (PR K6a; design §5.2 `builder.rs`, §8
//! "gate-volume producer", §6.2 "the engine `migrate` reuses").
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
//! order, extents are claimed lowest-first, collision `coll_seq`s are
//! assigned in sorted-name order within a `(parent, hash)` group, and
//! builder timestamps default to 0 unless set. Determinism is what lets
//! the §8 mount-time gates measure real images and lets K9's migrate
//! dry-run diff a digest walk before flipping the superblock.
//!
//! ## Fresh-volume hygiene
//!
//! `build` zeroes `[0, heap.start)` — superblock, ledger, ring, bitmap —
//! before writing (the §9 v3 quick-format rule): a stale ledger record or
//! checksummed ring page from a previous filesystem must never survive
//! into a fresh volume's mount-time recovery. Heap extents need no wipe:
//! unreferenced extents are unreachable through the FS API (§9), and a
//! reused extent's ghost bset frames carry foreign `node_seq` stamps the
//! K2 loader refuses as not-same-incarnation.

use super::backend::KvMetaBackend;
use super::superblock::SuperblockV3;
use super::tree::KvTree;
use super::KvError;
use std::path::Path;

/// The root directory's fixed ino (§4.8: ino 0 reserved, 1 = root,
/// `next_ino` starts above the built population).
pub const ROOT_INO: u64 = 1;

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
        let _ = node_size;
        todo!("PR K6a implementation commit")
    }
}

/// The in-memory volume description. Population methods reject duplicate
/// names, missing/non-directory parents, oversized names (> 255 B) and
/// values (> the §4.2 per-volume cap) with typed [`KvError`]s — a builder
/// input error must surface before any byte is written.
pub struct ImageBuilder {
    _private: (),
}

impl ImageBuilder {
    /// A description holding only the root directory (ino 1,
    /// `S_IFDIR | 0o755`, uid/gid 0, times 0).
    pub fn new(cfg: BuilderConfig) -> Result<Self, KvError> {
        let _ = cfg;
        todo!("PR K6a implementation commit")
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
        let _ = (parent, name, perm, uid, gid);
        todo!("PR K6a implementation commit")
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
        let _ = (parent, name, perm, uid, gid, size);
        todo!("PR K6a implementation commit")
    }

    /// Add a hard link: one more name for an existing non-directory ino
    /// (`nlink` maintained).
    pub fn add_link(&mut self, ino: u64, parent: u64, name: &str) -> Result<(), KvError> {
        let _ = (ino, parent, name);
        todo!("PR K6a implementation commit")
    }

    /// Set (or replace) one xattr on `ino`. The `"layout"` xattr — the
    /// data path's `LayoutMetadata` bytes (§5.3) — travels through here
    /// like any other value.
    pub fn set_xattr(&mut self, ino: u64, name: &str, value: &[u8]) -> Result<(), KvError> {
        let _ = (ino, name, value);
        todo!("PR K6a implementation commit")
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
        let _ = (ino, atime, mtime, ctime);
        todo!("PR K6a implementation commit")
    }

    /// Inodes described so far (root included).
    pub fn inode_count(&self) -> u64 {
        todo!("PR K6a implementation commit")
    }

    /// Build the image into `path` (a file or block device of
    /// `volume_len` usable bytes): plan geometry, zero the fixed
    /// structures, pack leaves bottom-up per tree, write interior levels,
    /// persist the bitmap, write the bootstrap ledger record, and stamp
    /// the superblock **last** (nothing references a half-built image —
    /// the §6.2 flip discipline applied to format).
    pub async fn build(&self, path: &Path, volume_len: u64) -> Result<BuiltImage, KvError> {
        let _ = (path, volume_len);
        todo!("PR K6a implementation commit")
    }
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
/// determinism tests, the torn-ledger fallback crash case, and (in K9)
/// migrate's dry-run diff.
pub async fn digest_walk(trees: &[&KvTree]) -> Result<u64, KvError> {
    let _ = trees;
    todo!("PR K6a implementation commit")
}

/// Convenience: [`digest_walk`] over a mounted backend's three trees.
pub async fn digest_backend(backend: &KvMetaBackend) -> Result<u64, KvError> {
    let _ = backend;
    todo!("PR K6a implementation commit")
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

/// The public v3 formatter (what `squeezefs format` calls from K6a on;
/// the v2 formatter is test-surface-only — §6.2, resolved OQ 4): runs the
/// same preflight policy as v2 (`format_preflight` — already-formatted
/// volumes refused without `force`, live clients refuse even with it),
/// then builds an empty (plus optional config xattr) image via
/// [`ImageBuilder`].
pub async fn format_v3(
    path: &Path,
    volume_len: u64,
    opts: &FormatV3Options,
) -> Result<BuiltImage, crate::error::SqueezefsError> {
    let _ = (path, volume_len, opts);
    todo!("PR K6a implementation commit")
}
