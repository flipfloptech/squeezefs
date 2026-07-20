//! Superblock v3 and the sector-0 version gate (PR K6a; design §4.1
//! "Superblock v3", §5.1, §6.1, resolved OQ 1).
//!
//! Sector 0 keeps the historical v2 wire prefix — `magic: [u8; 8]` at
//! `[0..8)` and `version: u32` at `[8..12)` — so any binary (old or new)
//! can classify any SqueezeFS volume: pre-v3 binaries read a v3 volume's
//! version as 3 and refuse loud ("upgrade squeezefs"); this binary reads a
//! legacy v2 volume's version as ≤ 2 and refuses loud (v2 support removed
//! — reformat required, [`VolumeFormat::V2Legacy`]).
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
//! The checksum covers the whole sector (not just the struct bytes, the
//! retired v2 convention) so a torn superblock write is detected no matter which bytes
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

use super::alloc_ext::{bitmap_region_len, compaction_reserve_extents};
use super::checkpoint::ROOT_LEDGER_LEN;
use super::journal::{JOURNAL_PAGE_LEN, MAX_ENTRY_LEN};
use super::node::{record_value_cap, NodeLayout, NODE_PAGE};
use super::KvError;
use std::path::Path;

/// The metadata-volume magic at sector-0 offset 0 — shared with the
/// retired v2 format by design (the version field discriminates), so a
/// legacy volume still classifies as "SqueezeFS, unsupported version"
/// rather than "foreign".
pub const MAGIC_VALUE: &[u8; 8] = b"METALV01";

/// The 4 KiB metadata sector: the superblock's durable-coverage unit and
/// the journal ring's page size.
pub const SECTOR_SIZE: usize = 4096;

/// Byte offsets of the sector-0 wire layout (module docs). `magic` and
/// `version` sit at the v2 offsets by design — the downgrade gate.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_NODE_SIZE: usize = 12;
const OFF_FEAT_INCOMPAT: usize = 16;
const OFF_FEAT_RO: usize = 24;
const OFF_ROOT_LEDGER: usize = 32;
const OFF_JOURNAL: usize = 48;
const OFF_ALLOC_BITMAP: usize = 64;
const OFF_HEAP: usize = 80;
const OFF_UUID: usize = 96;
const OFF_HASH_SEED: usize = 112;
const OFF_CHECKSUM: usize = 120;

/// Format version this module writes and mounts.
pub const SUPERBLOCK_V3_VERSION: u32 = 3;

/// Whole-sector superblock image length.
pub const SUPERBLOCK_V3_LEN: usize = SECTOR_SIZE;

/// `features_incompat` bit 0: the KV v3 node layer (§4.1). Set on every
/// v3 volume.
pub const FEATURE_INCOMPAT_KV_V3: u64 = 1 << 0;

/// `features_incompat` bit 1: the root ledger carries the node-seq mint
/// watermark (Finding A, 2026-07-13 — mounts must reseed the mint
/// counter above every persisted frame stamp). Set on every volume
/// formatted since; pre-watermark v3 volumes refuse loud (their ledger
/// slots also no longer decode) — forward-only, reformat required.
/// `format --force` delivers that reformat: the format preflight
/// degrades every refused superblock class to its plain force gate
/// (`kv::builder::format_preflight` — refused classes cannot host live
/// current clients by construction), so this refusal never blocks the
/// remedy it demands.
pub const FEATURE_INCOMPAT_NODE_SEQ_WATERMARK: u64 = 1 << 1;

/// `features_incompat` bit 3: the volume set has a **lifecycle history**
/// (design-volume-lifecycle KD-14, §7): a non-legacy volume record, a
/// non-identity slot map, or an active drain exists. Set durably at the
/// FIRST lifecycle commit — **before** the durable record it gates
/// (bit-before-durable-record ordering) — and never on untouched sets,
/// so legacy volumes stay bit-identical. Old binaries refuse loud via
/// the [`FEATURES_INCOMPAT_KNOWN`] gate. Bit 2 is reserved for
/// `KV_GUEST_SLOTS` (PR VL5a) and deliberately not defined here.
pub const FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE: u64 = 1 << 3;

/// Incompat feature bits this binary understands. Any other set bit
/// refuses the mount naming the bit (§6.1).
pub const FEATURES_INCOMPAT_KNOWN: u64 = FEATURE_INCOMPAT_KV_V3
    | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
    | FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE;

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
        let layout = NodeLayout::new(node_size)?;
        let node_size = layout.node_size() as u64;

        let root_ledger = ExtentRef {
            start: SECTOR_SIZE as u64,
            len: ROOT_LEDGER_LEN,
        };

        let journal_len = match journal_len_override {
            Some(len) => {
                // Ring floor: the reserve carve-out (max(256 KiB, ring/64))
                // plus one max-size entry must always be admissible, or a
                // legal transaction could never commit (§4.4 pt 5).
                let floor = 256 * 1024 + MAX_ENTRY_LEN;
                if len % JOURNAL_PAGE_LEN != 0 || len < floor {
                    return Err(KvError::Corrupt(format!(
                        "journal ring override {len} bytes is invalid: must be a 4 KiB \
                         multiple of at least {floor} bytes (reserve + one max entry)"
                    )));
                }
                len
            }
            None => journal_ring_len(volume_len),
        };
        let journal = ExtentRef {
            start: root_ledger.end(),
            len: journal_len,
        };

        // Bitmap region sized for the extent-count upper bound (the heap
        // cannot exceed volume/node_size extents); the actual count is
        // recomputed below once the heap start is fixed. Over-reserving a
        // page pair or two is deliberate — it breaks the mutual
        // dependency between bitmap size and heap size.
        let upper_extents = volume_len / node_size;
        let alloc_bitmap = ExtentRef {
            start: journal.end(),
            len: bitmap_region_len(upper_extents),
        };

        let heap_start = alloc_bitmap.end().div_ceil(node_size) * node_size;
        let total_extents = volume_len.saturating_sub(heap_start) / node_size;
        // A usable volume must hold the §4.7 compaction reserve plus room
        // for the three tree roots and growth.
        let min_extents = compaction_reserve_extents(total_extents) + 4;
        if total_extents < min_extents {
            return Err(KvError::Corrupt(format!(
                "metadata volume too small for format v3: {volume_len} bytes leaves \
                 {total_extents} heap extents of {node_size} bytes after the fixed structures \
                 (superblock + ledger + {journal_len}-byte journal + bitmap end at \
                 {heap_start}); at least {min_extents} extents are required — grow the volume \
                 or pass a smaller --meta-journal-mb / --meta-node-kib"
            )));
        }
        let heap = ExtentRef {
            start: heap_start,
            len: total_extents * node_size,
        };

        Ok(Self {
            node_size: node_size as u32,
            features_incompat: FEATURE_INCOMPAT_KV_V3 | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
            features_ro: 0,
            root_ledger,
            journal,
            alloc_bitmap,
            heap,
            uuid,
            hash_seed,
        })
    }

    /// Heap extent count (`heap.len / node_size`).
    pub fn total_extents(&self) -> u64 {
        self.heap.len / u64::from(self.node_size)
    }

    /// Journal ring page count (`journal.len / 4096`).
    pub fn journal_pages(&self) -> u64 {
        self.journal.len / JOURNAL_PAGE_LEN
    }

    /// Incompat feature bits set on disk that this binary does not
    /// understand (nonzero ⇒ the mount was refused by
    /// [`classify_sector0`]; kept for error surfaces and tests).
    pub fn unknown_incompat(&self) -> u64 {
        self.features_incompat & !FEATURES_INCOMPAT_KNOWN
    }

    /// Read-only feature bits set on disk that this binary does not
    /// understand (nonzero ⇒ mount read-only once K6b has a write path
    /// to withhold; K6a's read side logs it).
    pub fn unknown_ro(&self) -> u64 {
        self.features_ro & !FEATURES_RO_KNOWN
    }

    /// Encode into a checksummed whole-sector image.
    pub fn encode_sector(&self) -> Result<Vec<u8>, KvError> {
        self.validate_geometry()?;
        let mut img = vec![0u8; SUPERBLOCK_V3_LEN];
        img[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(MAGIC_VALUE);
        img[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&SUPERBLOCK_V3_VERSION.to_le_bytes());
        img[OFF_NODE_SIZE..OFF_NODE_SIZE + 4].copy_from_slice(&self.node_size.to_le_bytes());
        img[OFF_FEAT_INCOMPAT..OFF_FEAT_INCOMPAT + 8]
            .copy_from_slice(&self.features_incompat.to_le_bytes());
        img[OFF_FEAT_RO..OFF_FEAT_RO + 8].copy_from_slice(&self.features_ro.to_le_bytes());
        for (off, ext) in [
            (OFF_ROOT_LEDGER, &self.root_ledger),
            (OFF_JOURNAL, &self.journal),
            (OFF_ALLOC_BITMAP, &self.alloc_bitmap),
            (OFF_HEAP, &self.heap),
        ] {
            img[off..off + 8].copy_from_slice(&ext.start.to_le_bytes());
            img[off + 8..off + 16].copy_from_slice(&ext.len.to_le_bytes());
        }
        img[OFF_UUID..OFF_UUID + 16].copy_from_slice(&self.uuid);
        img[OFF_HASH_SEED..OFF_HASH_SEED + 8].copy_from_slice(&self.hash_seed.to_le_bytes());
        let sum = sector_checksum(&img);
        img[OFF_CHECKSUM..OFF_CHECKSUM + 8].copy_from_slice(&sum.to_le_bytes());
        Ok(img)
    }

    /// Decode + verify a sector-0 image already known to carry version 3:
    /// magic, checksum over the whole sector, bounds-checked geometry
    /// (§9: every length validated before use), feature gate
    /// (unknown incompat bits refuse loud, naming the bits). Torn or
    /// tampered superblocks fail loud — the §4.10 torn-SB crash case.
    pub fn decode_sector(buf: &[u8]) -> Result<Self, KvError> {
        Self::decode_sector_with_known(buf, FEATURES_INCOMPAT_KNOWN)
    }

    /// [`Self::decode_sector`] against an explicit known-incompat mask.
    /// The production gate passes [`FEATURES_INCOMPAT_KNOWN`]; tests pass
    /// historical masks to prove the forward-only refusal an OLD binary
    /// would issue for newer bits (KD-14's pinned old-mask check — the
    /// real gate code path, not a synthetic "some bit set" assertion).
    pub fn decode_sector_with_known(buf: &[u8], known_incompat: u64) -> Result<Self, KvError> {
        if buf.len() != SUPERBLOCK_V3_LEN {
            return Err(KvError::Corrupt(format!(
                "v3 superblock sector must be {SUPERBLOCK_V3_LEN} bytes, got {}",
                buf.len()
            )));
        }
        if &buf[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC_VALUE {
            return Err(KvError::Corrupt(
                "bad v3 superblock magic (corrupted or foreign volume)".to_string(),
            ));
        }
        let version = u32::from_le_bytes(buf[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
        if version != SUPERBLOCK_V3_VERSION {
            return Err(KvError::Corrupt(format!(
                "v3 decoder handed superblock version {version} (classification bug)"
            )));
        }
        let stored = u64::from_le_bytes(buf[OFF_CHECKSUM..OFF_CHECKSUM + 8].try_into().unwrap());
        let computed = sector_checksum(buf);
        if stored != computed {
            // Not the bare ChecksumMismatch variant: its display names
            // bsets, and a torn superblock deserves its own words.
            return Err(KvError::Corrupt(format!(
                "superblock checksum mismatch: stored {stored:#018x}, computed {computed:#018x} \
                 — corrupted superblock"
            )));
        }
        let ext_at = |off: usize| ExtentRef {
            start: u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()),
            len: u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap()),
        };
        let sb = Self {
            node_size: u32::from_le_bytes(
                buf[OFF_NODE_SIZE..OFF_NODE_SIZE + 4].try_into().unwrap(),
            ),
            features_incompat: u64::from_le_bytes(
                buf[OFF_FEAT_INCOMPAT..OFF_FEAT_INCOMPAT + 8]
                    .try_into()
                    .unwrap(),
            ),
            features_ro: u64::from_le_bytes(buf[OFF_FEAT_RO..OFF_FEAT_RO + 8].try_into().unwrap()),
            root_ledger: ext_at(OFF_ROOT_LEDGER),
            journal: ext_at(OFF_JOURNAL),
            alloc_bitmap: ext_at(OFF_ALLOC_BITMAP),
            heap: ext_at(OFF_HEAP),
            uuid: buf[OFF_UUID..OFF_UUID + 16].try_into().unwrap(),
            hash_seed: u64::from_le_bytes(
                buf[OFF_HASH_SEED..OFF_HASH_SEED + 8].try_into().unwrap(),
            ),
        };
        // Feature gate (§6.1): the KV_V3 bit must be present; unknown
        // incompat bits refuse the mount naming the bits. Unknown ro bits
        // pass — their read-only semantics belong to callers with a write
        // path (§4.11).
        if sb.features_incompat & FEATURE_INCOMPAT_KV_V3 == 0 {
            return Err(KvError::Corrupt(
                "v3 superblock without the KV_V3 incompat bit (corrupt feature field)".to_string(),
            ));
        }
        if sb.features_incompat & FEATURE_INCOMPAT_NODE_SEQ_WATERMARK == 0 {
            return Err(KvError::Corrupt(
                "pre-watermark v3 volume: formatted before the node-seq mint watermark \
                 (Finding A) and no longer supported; reformat required"
                    .to_string(),
            ));
        }
        let unknown = sb.features_incompat & !known_incompat;
        if unknown != 0 {
            let bits: Vec<String> = (0..64)
                .filter(|b| unknown & (1u64 << b) != 0)
                .map(|b| format!("bit {b}"))
                .collect();
            return Err(KvError::Corrupt(format!(
                "unknown incompatible feature bits on v3 superblock: {} \
                 ({unknown:#x}) — upgrade squeezefs to mount this volume",
                bits.join(", ")
            )));
        }
        sb.validate_geometry()?;
        Ok(sb)
    }

    /// §9 bounds discipline: every geometry field validated before any
    /// caller dereferences it. The ledger | journal | bitmap triple must
    /// ascend without overlap and above sector 0; lengths must match their
    /// consumers' fixed shapes; and the heap must be whole aligned extents
    /// covering the bitmap's bit range.
    ///
    /// Two legal placements of that fixed triple relative to the heap
    /// (design §4.1 "no hardcoded offsets except sector 0 — placed wherever
    /// free space exists"):
    ///
    /// * **fresh format** ([`SuperblockV3::plan`]) — `SB | ledger | journal
    ///   | bitmap | heap`, the triple entirely *below* `heap.start`;
    /// * **in-heap triple** (historically produced by the retired v2→v3
    ///   in-place migrator; still a legal v3 geometry — the on-disk format
    ///   is unchanged by the migrator's removal) — the heap spans the whole
    ///   device and the fixed triple lives in *reserved extents inside it*
    ///   (the bitmap marks those extents allocated so the allocator never
    ///   hands them out; that is the image producer's contract, not
    ///   something this structural check can see). Then the triple is
    ///   entirely within `[heap.start, heap.end)` and
    ///   `heap.start ≤ ledger.start`.
    ///
    /// Both are accepted; anything else is corruption.
    fn validate_geometry(&self) -> Result<(), KvError> {
        let layout = NodeLayout::new(self.node_size as usize)?;
        let node_size = layout.node_size() as u64;
        let corrupt = |msg: String| Err(KvError::Corrupt(format!("v3 superblock geometry: {msg}")));

        if self.root_ledger.start < SECTOR_SIZE as u64 {
            return corrupt(format!(
                "root ledger at {} overlaps sector 0",
                self.root_ledger.start
            ));
        }
        if self.root_ledger.len != ROOT_LEDGER_LEN {
            return corrupt(format!(
                "root ledger length {} != the fixed {ROOT_LEDGER_LEN}",
                self.root_ledger.len
            ));
        }
        if self.journal.start < self.root_ledger.end()
            || self.journal.len == 0
            || self.journal.len % JOURNAL_PAGE_LEN != 0
        {
            return corrupt(format!(
                "journal [{}, +{}) must follow the ledger in whole 4 KiB pages",
                self.journal.start, self.journal.len
            ));
        }
        if self.alloc_bitmap.start < self.journal.end() {
            return corrupt(format!(
                "alloc bitmap at {} overlaps the journal",
                self.alloc_bitmap.start
            ));
        }
        if self.heap.start % NODE_PAGE as u64 != 0
            || self.heap.len < node_size
            || self.heap.len % node_size != 0
        {
            return corrupt(format!(
                "heap [{}, +{}) must be whole {node_size}-byte extents on a {NODE_PAGE}-byte \
                 boundary",
                self.heap.start, self.heap.len
            ));
        }
        // The fixed ledger/journal/bitmap triple (already validated ascending
        // & non-overlapping above) is placed against the heap in one of the
        // two legal ways documented on this fn: entirely below it (fresh
        // format) or entirely within it on reserved extents (migrated volume,
        // §6.2). Anything else is corruption.
        let triple_below_heap = self.alloc_bitmap.end() <= self.heap.start;
        let triple_within_heap =
            self.heap.start <= self.root_ledger.start && self.alloc_bitmap.end() <= self.heap.end();
        if !(triple_below_heap || triple_within_heap) {
            return corrupt(format!(
                "fixed structures (ledger/journal/bitmap ending at {}) must sit either entirely \
                 below heap.start {} (fresh format) or entirely within the heap [{}, {}) on \
                 reserved extents (migrated volume) — got neither",
                self.alloc_bitmap.end(),
                self.heap.start,
                self.heap.start,
                self.heap.end()
            ));
        }
        if self.alloc_bitmap.len < bitmap_region_len(self.total_extents()) {
            return corrupt(format!(
                "alloc bitmap length {} cannot cover {} extents",
                self.alloc_bitmap.len,
                self.total_extents()
            ));
        }
        // The heap must clear the §4.7 compaction reserve with headroom —
        // the same floor `plan` enforces. Without this, a crafted (still
        // checksummed) superblock could hand the allocator a reserve ≥
        // total and turn a mount into a construction assert.
        let total = self.total_extents();
        let min_extents = compaction_reserve_extents(total) + 4;
        if total < min_extents {
            return corrupt(format!(
                "heap of {total} extents cannot hold the compaction reserve \
                 (≥ {min_extents} required)"
            ));
        }
        Ok(())
    }
}

/// xxh3 over the whole sector image with the checksum field zeroed.
fn sector_checksum(img: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&img[..OFF_CHECKSUM]);
    h.update(&[0u8; 8]);
    h.update(&img[OFF_CHECKSUM + 8..]);
    h.digest()
}

/// The resolved OQ 1 ring default: `clamp(volume_len / 64, 8 MiB, 32 MiB)`,
/// rounded down to whole 4 KiB pages (the clamp bounds already are, so the
/// alignment can never dip below the floor).
pub fn journal_ring_len(volume_len: u64) -> u64 {
    let clamped = (volume_len / 64).clamp(JOURNAL_RING_MIN, JOURNAL_RING_MAX);
    clamped / JOURNAL_PAGE_LEN * JOURNAL_PAGE_LEN
}

/// Validate the `--meta-node-kib` knob (§5.1): allowed values
/// 64/128/256/512/1024; sub-256 KiB settings return `Ok` with a warning
/// string naming the reduced per-volume record-value cap `node_size/4`
/// (§4.2) for the CLI to print. Anything else is a typed refusal —
/// including everything below the 64 KiB floor.
pub fn validate_node_kib(kib: u32) -> Result<(usize, Option<String>), KvError> {
    if ![64, 128, 256, 512, 1024].contains(&kib) {
        return Err(KvError::Corrupt(format!(
            "--meta-node-kib {kib} is not supported: allowed values are 64|128|256|512|1024 \
             (64 KiB floor, 1 MiB ceiling — design §5.1)"
        )));
    }
    let bytes = kib as usize * 1024;
    let warning = if kib < 256 {
        Some(format!(
            "--meta-node-kib {kib} trades xattr/layout headroom for cold-read latency: the \
             per-volume record-value cap becomes {} bytes (node_size/4, design §4.2) — values \
             above it are rejected and layout maps spill to the indirect mechanism sooner",
            record_value_cap(bytes)
        ))
    } else {
        None
    };
    Ok((bytes, warning))
}

/// Sector-0 classification for the mount/format version gate (§6.1) —
/// blank distinguished from garbage so the operator error is actionable.
#[derive(Debug, Clone)]
pub enum VolumeFormat {
    /// All-zero magic: never formatted. Callers decide loudness (mount:
    /// "run `squeezefs format` first"; format: proceed).
    Blank,
    /// SqueezeFS magic with version ≤ 2: the retired fixed-geometry
    /// format. **No longer mountable or readable** — mount and generation
    /// derivation refuse loud ("reformat required"); `format` treats it
    /// as an already-formatted volume (`--force` reformats it to v3).
    V2Legacy,
    /// A validated v3 superblock.
    V3(SuperblockV3),
}

/// Classify a sector-0 image (pure): [`VolumeFormat::Blank`] for zeroed
/// magic; loud errors for foreign magic, versions above 3 ("upgrade
/// squeezefs"), checksum mismatches, and v3 structural/feature-gate
/// failures. The version check precedes checksum verification — an
/// unknown version must be reported as such, never as a checksum
/// mismatch.
pub fn classify_sector0(sector: &[u8]) -> Result<VolumeFormat, KvError> {
    if sector.len() != SUPERBLOCK_V3_LEN {
        return Err(KvError::Corrupt(format!(
            "sector-0 image must be {SUPERBLOCK_V3_LEN} bytes, got {}",
            sector.len()
        )));
    }
    let magic = &sector[OFF_MAGIC..OFF_MAGIC + 8];
    if magic == [0u8; 8] {
        return Ok(VolumeFormat::Blank);
    }
    if magic != MAGIC_VALUE {
        return Err(KvError::Corrupt(format!(
            "invalid superblock magic {magic:?} (expected {MAGIC_VALUE:?}) — corrupted or \
             foreign volume"
        )));
    }
    // The version check precedes checksum verification: a future format
    // may change checksum semantics, so an unknown version must be
    // reported as such, never as a checksum mismatch.
    let version = u32::from_le_bytes(sector[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
    match version {
        // Retired v2 format: recognized (SqueezeFS magic), never decoded —
        // there is no v2 reader left. The classification alone lets the
        // mount refuse with the precise "no longer supported" message and
        // lets `format --force` reformat the volume.
        v if v <= 2 => Ok(VolumeFormat::V2Legacy),
        SUPERBLOCK_V3_VERSION => Ok(VolumeFormat::V3(SuperblockV3::decode_sector(sector)?)),
        v => Err(KvError::Corrupt(format!(
            "unsupported metadata format version {v} (this binary supports <= \
             {SUPERBLOCK_V3_VERSION}) — upgrade squeezefs"
        ))),
    }
}

/// Read sector 0 of `path` via `crate::uring_fs` (io_uring-only,
/// AGENTS.md) and [`classify_sector0`] it. Never grows or mutates the
/// volume.
pub async fn classify_volume(path: &Path) -> Result<VolumeFormat, KvError> {
    let got = crate::uring_fs::read_at(path, 0, SUPERBLOCK_V3_LEN).await?;
    // A short read (file smaller than one sector) zero-extends: zeros
    // classify as Blank, exactly what a never-formatted stub file is.
    let sector: std::borrow::Cow<'_, [u8]> = if got.len() == SUPERBLOCK_V3_LEN {
        std::borrow::Cow::Borrowed(&got)
    } else {
        let mut full = vec![0u8; SUPERBLOCK_V3_LEN];
        full[..got.len().min(SUPERBLOCK_V3_LEN)]
            .copy_from_slice(&got[..got.len().min(SUPERBLOCK_V3_LEN)]);
        std::borrow::Cow::Owned(full)
    };
    classify_sector0(&sector).map_err(|e| match e {
        // Prefix classification failures with the volume path — these are
        // operator-facing mount/format refusals.
        KvError::Corrupt(msg) => KvError::Corrupt(format!("{}: {msg}", path.display())),
        other => other,
    })
}

/// Write `sb` to sector 0 of `path` (one checksummed whole-sector
/// `uring_fs::write_at` — the single-sector commit-point class).
pub async fn write_superblock_v3(path: &Path, sb: &SuperblockV3) -> Result<(), KvError> {
    let img = sb.encode_sector()?;
    crate::uring_fs::write_at(path, 0, img).await?;
    Ok(())
}

/// Stamp [`FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE`] on `path`'s superblock
/// — the "first non-trivial lifecycle commit" gate (KD-14). Returns
/// whether the bit was NEWLY set (`false` = already stamped, no write).
/// Refuses blank / legacy-v2 / corrupt volumes loud. Callers must invoke
/// this **before** committing the durable record the bit gates
/// (bit-before-durable-record ordering, design-volume-lifecycle §7): a
/// crash between the bit write and the record commit leaves a set old
/// binaries refuse and this binary mounts unchanged — the safe prefix.
///
/// Sector 0 is written only here and at format, never by the live
/// backend (checkpoints flip the root ledger), so the whole-sector
/// checksummed rewrite is race-free against an open volume.
pub async fn set_volume_lifecycle_bit(path: &Path) -> Result<bool, KvError> {
    match classify_volume(path).await? {
        VolumeFormat::V3(mut sb) => {
            if sb.features_incompat & FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE != 0 {
                return Ok(false);
            }
            sb.features_incompat |= FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE;
            write_superblock_v3(path, &sb).await?;
            Ok(true)
        }
        VolumeFormat::Blank => Err(KvError::Corrupt(format!(
            "{}: cannot stamp the volume-lifecycle bit on an unformatted volume — run \
             `squeezefs format` first",
            path.display()
        ))),
        VolumeFormat::V2Legacy => Err(KvError::Corrupt(format!(
            "{}: format v2 is no longer supported; reformat required",
            path.display()
        ))),
    }
}
