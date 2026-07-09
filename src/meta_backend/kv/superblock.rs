//! Superblock v3 and the dual-format version gate (PR K6a; design §4.1
//! "Superblock v3", §5.1, §6.1, resolved OQ 1).
//!
//! Sector 0 keeps the v2 wire prefix — `magic: [u8; 8]` at `[0..8)` and
//! `version: u32` at `[8..12)` — so every pre-v3 binary reads a v3 volume's
//! version as 3 and refuses loud ("upgrade squeezefs",
//! `storage.rs:validate_superblock`): the downgrade story the WAL design
//! bought is exactly what makes the format bump safe (§4.1).
//!
//! ## Sector layout (little-endian, one 4 KiB sector)
//!
//! ```text
//! [0..8)     magic            "METALV01" (unchanged — version discriminates)
//! [8..12)    version: u32     3
//! [12..16)   node_size: u32   bytes; default 262144, format-time knob (§5.1)
//! [16..24)   features_incompat: u64   bit 0 = KV_V3; unknown ⇒ refuse mount
//! [24..32)   features_ro: u64         unknown ⇒ mount read-only (§4.11)
//! [32..48)   root_ledger  { start: u64, len: u64 }
//! [48..64)   journal      { start: u64, len: u64 }
//! [64..80)   alloc_bitmap { start: u64, len: u64 }
//! [80..96)   heap         { start: u64, len: u64 }
//! [96..112)  uuid: [u8; 16]
//! [112..120) hash_seed: u64   random at format; keys the §4.2 name hashes
//! [120..128) checksum: u64    xxh3_64 over the WHOLE sector, field zeroed
//! [128..4096) zero padding    covered by the checksum
//! ```
//!
//! The checksum covers the whole sector (not just the struct bytes, the v2
//! convention) so a torn superblock write is detected no matter which bytes
//! the tear scrambled — the SB is a **durable-coverage unit** (§4.1): unlike
//! journal contents, a bad superblock fails the mount loud (§4.10 torn-SB
//! crash case).
//!
//! ## Tree-roots bootstrap
//!
//! The superblock deliberately carries **no tree roots** — those live in the
//! root ledger (§4.1) so checkpoints never rewrite sector 0. "Bootstrap" is
//! the `root_ledger` pointer: mount = SB → newest valid ledger record →
//! per-tree roots. The [`crate::meta_backend::kv::builder`] writes the
//! initial ledger record naming the fresh (empty or bulk-built) tree roots.

use super::KvError;
use crate::meta_backend::storage::{Superblock, SECTOR_SIZE};
use std::path::Path;

/// Format version this module writes and mounts.
pub const SUPERBLOCK_V3_VERSION: u32 = 3;

/// Whole-sector superblock image length.
pub const SUPERBLOCK_V3_LEN: usize = SECTOR_SIZE;

/// `features_incompat` bit 0: the KV v3 node layer (§4.1). Set on every
/// v3 volume.
pub const FEATURE_INCOMPAT_KV_V3: u64 = 1 << 0;

/// Incompat feature bits this binary understands. Any other set bit
/// refuses the mount naming the bit (§6.1).
pub const FEATURES_INCOMPAT_KNOWN: u64 = FEATURE_INCOMPAT_KV_V3;

/// Read-only feature bits this binary understands (none yet — §4.11
/// reserves the mechanism for snapshots). Unknown bits mount read-only.
pub const FEATURES_RO_KNOWN: u64 = 0;

/// Resolved OQ 1 journal-ring clamp floor: 8 MiB.
pub const JOURNAL_RING_MIN: u64 = 8 * 1024 * 1024;

/// Resolved OQ 1 journal-ring clamp ceiling: 32 MiB.
pub const JOURNAL_RING_MAX: u64 = 32 * 1024 * 1024;

/// One on-disk extent `[start, start + len)` named by the superblock
/// (§4.1: "no hardcoded offsets except sector 0").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentRef {
    pub start: u64,
    pub len: u64,
}

impl ExtentRef {
    /// Exclusive end offset.
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// The v3 superblock (§4.1). `magic`, `version`, and `checksum` are wire
/// artifacts owned by [`SuperblockV3::encode_sector`] /
/// [`SuperblockV3::decode_sector`]; the struct carries the format-time
/// decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperblockV3 {
    /// Node/extent size in bytes (validated: 4 KiB multiple in
    /// [64 KiB, 1 MiB] — [`super::node::NodeLayout`]).
    pub node_size: u32,
    /// §4.1 feature bits; bit 0 ([`FEATURE_INCOMPAT_KV_V3`]) always set.
    pub features_incompat: u64,
    /// §4.11 read-only feature bits.
    pub features_ro: u64,
    /// The 32 × 4 KiB root-ledger slot array (§4.1).
    pub root_ledger: ExtentRef,
    /// The journal ring (sized by the resolved OQ 1 clamp at format).
    pub journal: ExtentRef,
    /// A/B allocator bitmap page pairs (§4.7).
    pub alloc_bitmap: ExtentRef,
    /// The node heap; `heap.len / node_size` extents.
    pub heap: ExtentRef,
    pub uuid: [u8; 16],
    /// Per-volume secret seed keying the §4.2 dentry/xattr name hashes.
    pub hash_seed: u64,
}

impl SuperblockV3 {
    /// Plan a fresh volume's geometry (format time): ledger at 4096,
    /// journal ring per the resolved OQ 1 clamp (`journal_len_override`
    /// replaces the clamp — the `--meta-journal-mb` CLI contract, §5.1),
    /// bitmap sized for the heap, heap aligned to `node_size`. Errors
    /// typed and loud when the volume cannot hold the fixed structures
    /// plus at least one usable extent beyond the §4.7 compaction reserve.
    pub fn plan(
        volume_len: u64,
        node_size: usize,
        journal_len_override: Option<u64>,
        uuid: [u8; 16],
        hash_seed: u64,
    ) -> Result<Self, KvError> {
        let _ = (volume_len, node_size, journal_len_override, uuid, hash_seed);
        todo!("PR K6a implementation commit")
    }

    /// Heap extent count (`heap.len / node_size`).
    pub fn total_extents(&self) -> u64 {
        todo!("PR K6a implementation commit")
    }

    /// Journal ring page count (`journal.len / 4096`).
    pub fn journal_pages(&self) -> u64 {
        todo!("PR K6a implementation commit")
    }

    /// Incompat feature bits set on disk that this binary does not
    /// understand (nonzero ⇒ the mount was refused by
    /// [`classify_sector0`]; kept for error surfaces and tests).
    pub fn unknown_incompat(&self) -> u64 {
        todo!("PR K6a implementation commit")
    }

    /// Read-only feature bits set on disk that this binary does not
    /// understand (nonzero ⇒ mount read-only once K6b has a write path
    /// to withhold; K6a's read side logs it).
    pub fn unknown_ro(&self) -> u64 {
        todo!("PR K6a implementation commit")
    }

    /// Encode into a checksummed whole-sector image.
    pub fn encode_sector(&self) -> Result<Vec<u8>, KvError> {
        todo!("PR K6a implementation commit")
    }

    /// Decode + verify a sector-0 image already known to carry version 3:
    /// magic, checksum over the whole sector, bounds-checked geometry
    /// (§9: every length validated before use), feature gate
    /// (unknown incompat bits refuse loud, naming the bits). Torn or
    /// tampered superblocks fail loud — the §4.10 torn-SB crash case.
    pub fn decode_sector(buf: &[u8]) -> Result<Self, KvError> {
        let _ = buf;
        todo!("PR K6a implementation commit")
    }
}

/// The resolved OQ 1 ring default: `clamp(volume_len / 64, 8 MiB, 32 MiB)`,
/// rounded down to whole 4 KiB pages (the clamp bounds already are).
pub fn journal_ring_len(volume_len: u64) -> u64 {
    let _ = volume_len;
    todo!("PR K6a implementation commit")
}

/// Validate the `--meta-node-kib` knob (§5.1): allowed values
/// 64/128/256/512/1024; sub-256 KiB settings return `Ok` with a warning
/// string naming the reduced per-volume record-value cap `node_size/4`
/// (§4.2) for the CLI to print. Anything else is a typed refusal.
pub fn validate_node_kib(kib: u32) -> Result<(usize, Option<String>), KvError> {
    let _ = kib;
    todo!("PR K6a implementation commit")
}

/// Sector-0 classification for the dual-format mount dispatch (§6.1) —
/// the storage.rs pattern (blank distinguished from garbage so the
/// operator error is actionable) extended with the v3 arm.
#[derive(Debug, Clone)]
pub enum VolumeFormat {
    /// All-zero magic: never formatted. Callers decide loudness (mount:
    /// "run `squeezefs format` first"; format: proceed).
    Blank,
    /// A validated v2 superblock (magic + version ≤ 2 + checksum-iff-
    /// nonzero — byte-identical policy to `storage.rs`).
    V2(Superblock),
    /// A validated v3 superblock.
    V3(SuperblockV3),
}

/// Classify a sector-0 image (pure): [`VolumeFormat::Blank`] for zeroed
/// magic; loud errors for foreign magic, versions above 3 ("upgrade
/// squeezefs"), checksum mismatches, and v3 structural/feature-gate
/// failures. The version check precedes checksum verification on both
/// arms — an unknown version must be reported as such, never as a
/// checksum mismatch (the storage.rs discipline).
pub fn classify_sector0(sector: &[u8]) -> Result<VolumeFormat, KvError> {
    let _ = sector;
    todo!("PR K6a implementation commit")
}

/// Read sector 0 of `path` via `crate::uring_fs` (io_uring-only,
/// AGENTS.md) and [`classify_sector0`] it. Never grows or mutates the
/// volume (unlike `MetaLvStorage::open`, which `set_len`s small files —
/// a v3 volume must not be touched by v2 plumbing).
pub async fn classify_volume(path: &Path) -> Result<VolumeFormat, KvError> {
    let _ = path;
    todo!("PR K6a implementation commit")
}

/// Write `sb` to sector 0 of `path` (one checksummed whole-sector
/// `uring_fs::write_at` — the same single-sector commit-point class as
/// every v2 superblock write).
pub async fn write_superblock_v3(path: &Path, sb: &SuperblockV3) -> Result<(), KvError> {
    let _ = (path, sb);
    todo!("PR K6a implementation commit")
}
