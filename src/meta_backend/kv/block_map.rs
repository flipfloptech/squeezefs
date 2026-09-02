//! **The block-map tree's codecs** — PB-class file support, PR 1 of the
//! `feat/kvmap-*` ladder (`docs/design-kvmap-block-map-tree.md`, Rev 1).
//!
//! ## The problem the tree closes (design §0/§1)
//!
//! A striped file's block map lives inline in its `layout` xattr until
//! the encoded map exceeds the xattr cap (~6–8 GiB of file at 4 MiB
//! blocks), then spills to a single CoW blob block — a hard ceiling of
//! `block_size²/30` (~545 GiB at 4 MiB blocks), O(file) publish cost,
//! delta-ineligible heads, the finding-23/24/33/35c/41 blob-lifecycle
//! fragility class, and whole-map RAM rehydration per open. The verdict:
//! one record per mapping in a first-class KV tree
//! ([`super::record::TREE_BLOCK_MAP`] = 7, incompat bit 16), riding the
//! existing conveyor/journal/checkpoint/node-cache machinery verbatim —
//! the shape [`super::block_refs`] already proved at this cardinality,
//! keyed the other way.
//!
//! ## Format (design §2)
//!
//! ```text
//! key (12 B, big-endian composites — memcmp order == logical order):
//!   [0..8)    owner_ino:   u64   the owning inode (GLOBAL ino)
//!   [8..12)   block_index: u32   the ino's block-map index; u32::MAX is
//!                                RESERVED (§6 A5: the refs MAP_BLOB
//!                                sentinel claims it) and refused at
//!                                encode AND decode
//!
//! value (versioned, little-endian — no ordering requirement):
//!   [0]       version: u8       [`BLOCK_MAP_VALUE_VERSION`]
//!   [1]       kind:    u8       POINT | STRING | RUN | POINT2 | RUN2
//!   POINT  ⇒  [2..10) vol_tag: u64 ‖ [10..18) offset: u64
//!   STRING ⇒  [2..]   decorated block key bytes, VERBATIM
//!   RUN    ⇒  [2..10) vol_tag ‖ [10..18) start_offset ‖ [18..22) len: u32
//!   POINT2 ⇒  POINT ‖ [18..26) incarnation: u64 (verbatim stamp)
//!   RUN2   ⇒  RUN   ‖ [22..30) start_incarnation: u64
//! ```
//!
//! POINT is the binary fast form: `vol_tag` is the durable `vol-{16 hex}`
//! data-volume identity decoded verbatim (KD-5 — [`super::block_refs::
//! volume_tag`], never a path or an ordinal) plus the device offset.
//! Decorated keys (`damaged:` markers, `:extra` trailers) ride STRING
//! untouched. RUN/RUN2 (PR 6a, design §12) collapse straight sequential
//! spans — key at `start_index`, one-block offset stride, capped at
//! [`RUN_LEN_MAX`] (A6: the exact-miss read is a bounded forward
//! take-last, and this cap IS its bound). POINT2/RUN2 carry the spec
//! §6.2 item-6 incarnation stamp VERBATIM (Rev 1.4 #1: a decode that
//! re-attached the CURRENT stamp would let a freed-and-reissued offset
//! pass the staleness refusal). Any unknown kind/version refuses loud —
//! guessing would silently resolve a block to the wrong device bytes,
//! the exact failure the tree exists to prevent.
//!
//! ## The head sentinel (design §2/§3, amendment A2)
//!
//! A crossed ino's layout head carries `block_map_id = "kvmap:1"` with
//! `block_map: None` — ~100 B regardless of file size. The A2
//! truncate/unlink design flips size first and parks a durable per-ino
//! sweep cursor IN the head (`kvmap:1;sweep:K`) for the background
//! job-fabric sweep, so PR 1's codec carries the cursor from day one
//! even though the sweep itself lands later. An unknown `kvmap:N` major
//! refuses loud (always-forward), and the grammar is EXACT — the parser
//! accepts nothing its encoder cannot produce.
//!
//! ## What PR 1 does NOT do
//!
//! Nothing here changes any existing path: no production site stamps
//! bit 16 or stages a [`BlockMapOp`] (the crossing/spill switch is PR 2,
//! runs are PR 6, walkers are PR 4). The staging/lookup surface exists
//! dark, exercised by `tests/kvmap_tree_tests.rs` and `meta_lv_bench`.

use super::KvError;

/// Key length: `owner_ino | block_index`.
pub const BLOCK_MAP_KEY_LEN: usize = 8 + 4;

/// Current value version. Anything else refuses loud (forward-only): an
/// unknown version means a newer binary wrote mappings this one cannot
/// interpret, and guessing would resolve blocks to wrong device bytes.
pub const BLOCK_MAP_VALUE_VERSION: u8 = 1;

/// `kind`: binary `(vol_tag, offset)` mapping — the fast common form.
pub const BLOCK_MAP_KIND_POINT: u8 = 1;

/// `kind`: decorated block key stored verbatim as bytes (`damaged:`
/// markers etc. — design §2).
pub const BLOCK_MAP_KIND_STRING: u8 = 2;

/// `kind`: a RUN record (PR 6a, design §2/§12) — `vol_tag ‖ start_offset
/// ‖ len`, key at `start_index`, covering indices
/// `[start_index, start_index + len)` at one-block offset stride (the
/// stride itself is the owning data volume's block size, resolved by the
/// router — never stored, so the 22-B value stays fixed). Read law: an
/// exact-match record at N supersedes a covering run (design §2).
pub const BLOCK_MAP_KIND_RUN: u8 = 3;

/// `kind`: POINT2 (PR 6a, design §12) — a POINT carrying the block key's
/// **incarnation stamp** (spec §6.2 item 6) VERBATIM: `vol_tag ‖ offset ‖
/// incarnation`. The stamp is part of the key's identity and is decoded
/// back verbatim — never re-attached from the live map, which would let
/// a record naming a freed-and-reissued offset pass the staleness
/// refusal the stamp exists to fire (Rev 1.4 #1).
pub const BLOCK_MAP_KIND_POINT_STAMPED: u8 = 4;

/// `kind`: RUN2 (PR 6a, design §12) — a RUN whose covered keys carry
/// incarnation stamps: `vol_tag ‖ start_offset ‖ len ‖ start_incarnation`,
/// legal iff the covered stamps are LITERALLY consecutive composed words
/// (`stamp(i) = start_incarnation + i` — consecutive-mint lane_seqs prove
/// stride-1; the emitter verifies every actual stamp, so the decode
/// arithmetic reproduces exactly what was minted).
pub const BLOCK_MAP_KIND_RUN_STAMPED: u8 = 5;

/// Encoded POINT value length: `version | kind | vol_tag | offset`.
pub const BLOCK_MAP_POINT_VALUE_LEN: usize = 1 + 1 + 8 + 8;

/// Encoded RUN value length: `version | kind | vol_tag | start_offset |
/// len`.
pub const BLOCK_MAP_RUN_VALUE_LEN: usize = 1 + 1 + 8 + 8 + 4;

/// Encoded POINT2 value length: POINT + the u64 incarnation stamp.
pub const BLOCK_MAP_POINT_STAMPED_VALUE_LEN: usize = BLOCK_MAP_POINT_VALUE_LEN + 8;

/// Encoded RUN2 value length: RUN + the u64 start incarnation.
pub const BLOCK_MAP_RUN_STAMPED_VALUE_LEN: usize = BLOCK_MAP_RUN_VALUE_LEN + 8;

/// The longest span one run record may cover (design §6 **A6**): the tree
/// has no floor/predecessor primitive, so the exact-miss read law is one
/// bounded forward `range([ino‖N−RUN_LEN_MAX+1, ino‖N])` take-last — this
/// constant IS that bound, so it is a format law, never a knob (a wider
/// run would be invisible to every reader's floor probe).
pub const RUN_LEN_MAX: u32 = 4096;

/// The head-sentinel prefix (`block_map_id = "kvmap:…"`).
pub const KVMAP_HEAD_PREFIX: &str = "kvmap:";

/// The head-sentinel major version this binary writes and accepts.
pub const KVMAP_HEAD_MAJOR: u32 = 1;

/// Build a mapping key (big-endian composite, §4.2 key discipline).
/// Index `u32::MAX` is RESERVED (design §6 A5 — the block_refs MAP_BLOB
/// sentinel claims it, and the §5 ceiling is enforced as an explicit
/// EFBIG at `(2³² − 1) × block_size`): a clean refusal, never a panic.
pub fn block_map_key(owner_ino: u64, block_index: u32) -> Result<[u8; BLOCK_MAP_KEY_LEN], KvError> {
    if block_index == u32::MAX {
        return Err(KvError::Corrupt(format!(
            "block-map index {block_index} is reserved (design A5: the MAP_BLOB \
             sentinel); the file ceiling is (2^32 − 1) blocks"
        )));
    }
    let mut key = [0u8; BLOCK_MAP_KEY_LEN];
    key[0..8].copy_from_slice(&owner_ino.to_be_bytes());
    key[8..12].copy_from_slice(&block_index.to_be_bytes());
    Ok(key)
}

/// Decode a mapping key; length-checked (§9 bounds rule). A key CARRYING
/// the reserved index cannot have been legitimately encoded, so it is
/// corruption, not a mapping.
pub fn decode_block_map_key(key: &[u8]) -> Result<(u64, u32), KvError> {
    if key.len() != BLOCK_MAP_KEY_LEN {
        return Err(KvError::Corrupt(format!(
            "block-map key must be {BLOCK_MAP_KEY_LEN} bytes, got {}",
            key.len()
        )));
    }
    let owner_ino = u64::from_be_bytes(key[0..8].try_into().expect("length checked"));
    let block_index = u32::from_be_bytes(key[8..12].try_into().expect("length checked"));
    if block_index == u32::MAX {
        return Err(KvError::Corrupt(format!(
            "block-map key for ino {owner_ino} carries the reserved index u32::MAX \
             (design A5) — no encoder produces it"
        )));
    }
    Ok((owner_ino, block_index))
}

/// Inclusive-start / inclusive-end key bounds covering `owner_ino`'s
/// mappings from `from_index` upward — the [`super::tree::KvTree::range`]
/// window shape. The end bound uses the RESERVED index `u32::MAX` raw: no
/// record can carry it ([`block_map_key`] refuses), so the bound is exact
/// — every legal index of this ino is inside, nothing of `ino + 1` is.
pub fn index_range_from(
    owner_ino: u64,
    from_index: u32,
) -> ([u8; BLOCK_MAP_KEY_LEN], [u8; BLOCK_MAP_KEY_LEN]) {
    let mut lo = [0u8; BLOCK_MAP_KEY_LEN];
    lo[0..8].copy_from_slice(&owner_ino.to_be_bytes());
    lo[8..12].copy_from_slice(&from_index.to_be_bytes());
    let mut hi = [0u8; BLOCK_MAP_KEY_LEN];
    hi[0..8].copy_from_slice(&owner_ino.to_be_bytes());
    hi[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    (lo, hi)
}

/// One decoded mapping value (design §2 v1 kinds + the PR 6a run/stamped
/// forms, design §12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapEntry {
    /// Binary `(vol_tag, offset)` — `vol_tag` per
    /// [`super::block_refs::volume_tag`] (KD-5).
    Point { vol_tag: u64, offset: u64 },
    /// A decorated block key, verbatim bytes.
    String(Vec<u8>),
    /// A RUN (design §2/§12): `len` consecutive indices from the record's
    /// key, offsets advancing one data-volume block per index. Exact
    /// records supersede a covering run (the §2 read law).
    Run {
        vol_tag: u64,
        start_offset: u64,
        len: u32,
    },
    /// POINT2 (design §12): a point whose block key carries an
    /// incarnation stamp, stored VERBATIM (never re-attached at decode).
    PointStamped {
        vol_tag: u64,
        offset: u64,
        incarnation: u64,
    },
    /// RUN2 (design §12): a run of stamped keys whose stamps are
    /// literally consecutive (`stamp(i) = start_incarnation + i`).
    RunStamped {
        vol_tag: u64,
        start_offset: u64,
        len: u32,
        start_incarnation: u64,
    },
}

impl MapEntry {
    /// [`Self::encode`]'s length without the allocation — the
    /// `publish_map_record_bytes` accounting (design §5).
    pub fn encoded_len(&self) -> usize {
        match self {
            MapEntry::Point { .. } => BLOCK_MAP_POINT_VALUE_LEN,
            MapEntry::String(bytes) => 2 + bytes.len(),
            MapEntry::Run { .. } => BLOCK_MAP_RUN_VALUE_LEN,
            MapEntry::PointStamped { .. } => BLOCK_MAP_POINT_STAMPED_VALUE_LEN,
            MapEntry::RunStamped { .. } => BLOCK_MAP_RUN_STAMPED_VALUE_LEN,
        }
    }

    /// Indices this record covers from its key: 1 for point/string forms,
    /// the run length otherwise — the span arithmetic every coverage
    /// check (floor probe, diff, C11) shares.
    pub fn run_len(&self) -> u32 {
        match self {
            MapEntry::Run { len, .. } | MapEntry::RunStamped { len, .. } => *len,
            _ => 1,
        }
    }

    /// Encode as `version | kind | payload` (values are little-endian —
    /// no ordering requirement, the §4.2 convention).
    pub fn encode(&self) -> Vec<u8> {
        match self {
            MapEntry::Point { vol_tag, offset } => {
                let mut out = Vec::with_capacity(BLOCK_MAP_POINT_VALUE_LEN);
                out.push(BLOCK_MAP_VALUE_VERSION);
                out.push(BLOCK_MAP_KIND_POINT);
                out.extend_from_slice(&vol_tag.to_le_bytes());
                out.extend_from_slice(&offset.to_le_bytes());
                out
            }
            MapEntry::String(bytes) => {
                let mut out = Vec::with_capacity(2 + bytes.len());
                out.push(BLOCK_MAP_VALUE_VERSION);
                out.push(BLOCK_MAP_KIND_STRING);
                out.extend_from_slice(bytes);
                out
            }
            MapEntry::Run {
                vol_tag,
                start_offset,
                len,
            } => {
                let mut out = Vec::with_capacity(BLOCK_MAP_RUN_VALUE_LEN);
                out.push(BLOCK_MAP_VALUE_VERSION);
                out.push(BLOCK_MAP_KIND_RUN);
                out.extend_from_slice(&vol_tag.to_le_bytes());
                out.extend_from_slice(&start_offset.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
                out
            }
            MapEntry::PointStamped {
                vol_tag,
                offset,
                incarnation,
            } => {
                let mut out = Vec::with_capacity(BLOCK_MAP_POINT_STAMPED_VALUE_LEN);
                out.push(BLOCK_MAP_VALUE_VERSION);
                out.push(BLOCK_MAP_KIND_POINT_STAMPED);
                out.extend_from_slice(&vol_tag.to_le_bytes());
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&incarnation.to_le_bytes());
                out
            }
            MapEntry::RunStamped {
                vol_tag,
                start_offset,
                len,
                start_incarnation,
            } => {
                let mut out = Vec::with_capacity(BLOCK_MAP_RUN_STAMPED_VALUE_LEN);
                out.push(BLOCK_MAP_VALUE_VERSION);
                out.push(BLOCK_MAP_KIND_RUN_STAMPED);
                out.extend_from_slice(&vol_tag.to_le_bytes());
                out.extend_from_slice(&start_offset.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&start_incarnation.to_le_bytes());
                out
            }
        }
    }
}

/// Run-length validation shared by both run kinds: the encoder emits
/// `2 ..= RUN_LEN_MAX` only (a 1-run is a point; a parser wider than its
/// encoder is a format hole, and a run past `RUN_LEN_MAX` is INVISIBLE
/// to the A6 bounded floor probe — accepting one would silently
/// unresolve every covered index).
fn validate_run_len(len: u32) -> Result<(), KvError> {
    if len < 2 || len > RUN_LEN_MAX {
        return Err(KvError::Corrupt(format!(
            "block-map run length {len} is outside 2..={RUN_LEN_MAX} (design A6/§12) — \
             no encoder produces it"
        )));
    }
    Ok(())
}

/// Decode + validate a mapping value: version-gated, kind-gated,
/// length-checked (§9). Every failure is loud corruption — silently
/// accepting an unreadable mapping would resolve a block to the wrong
/// device bytes, the exact failure this tree exists to prevent.
pub fn decode_block_map_value(value: &[u8]) -> Result<MapEntry, KvError> {
    if value.len() < 2 {
        return Err(KvError::Corrupt(format!(
            "block-map value must carry version + kind, got {} byte(s)",
            value.len()
        )));
    }
    if value[0] != BLOCK_MAP_VALUE_VERSION {
        return Err(KvError::Corrupt(format!(
            "block-map value version {} is not {BLOCK_MAP_VALUE_VERSION} (a newer \
             binary's mapping — refusing to guess where the block lives)",
            value[0]
        )));
    }
    match value[1] {
        BLOCK_MAP_KIND_POINT => {
            if value.len() != BLOCK_MAP_POINT_VALUE_LEN {
                return Err(KvError::Corrupt(format!(
                    "block-map POINT value must be {BLOCK_MAP_POINT_VALUE_LEN} bytes, \
                     got {}",
                    value.len()
                )));
            }
            Ok(MapEntry::Point {
                vol_tag: u64::from_le_bytes(value[2..10].try_into().expect("length checked")),
                offset: u64::from_le_bytes(value[10..18].try_into().expect("length checked")),
            })
        }
        BLOCK_MAP_KIND_STRING => Ok(MapEntry::String(value[2..].to_vec())),
        BLOCK_MAP_KIND_RUN => {
            if value.len() != BLOCK_MAP_RUN_VALUE_LEN {
                return Err(KvError::Corrupt(format!(
                    "block-map RUN value must be {BLOCK_MAP_RUN_VALUE_LEN} bytes, got {}",
                    value.len()
                )));
            }
            let len = u32::from_le_bytes(value[18..22].try_into().expect("length checked"));
            validate_run_len(len)?;
            Ok(MapEntry::Run {
                vol_tag: u64::from_le_bytes(value[2..10].try_into().expect("length checked")),
                start_offset: u64::from_le_bytes(value[10..18].try_into().expect("length checked")),
                len,
            })
        }
        BLOCK_MAP_KIND_POINT_STAMPED => {
            if value.len() != BLOCK_MAP_POINT_STAMPED_VALUE_LEN {
                return Err(KvError::Corrupt(format!(
                    "block-map POINT2 value must be {BLOCK_MAP_POINT_STAMPED_VALUE_LEN} \
                     bytes, got {}",
                    value.len()
                )));
            }
            let incarnation = u64::from_le_bytes(value[18..26].try_into().expect("length checked"));
            if incarnation == 0 {
                // Stamp 0 is INCARNATION_NONE — an unstamped key rides
                // POINT, so a zero here has two spellings for one mapping
                // (the record-equality diff's poison) and no encoder
                // produces it.
                return Err(KvError::Corrupt(
                    "block-map POINT2 value carries incarnation 0 (an unstamped key \
                     rides POINT — design §12); no encoder produces it"
                        .to_string(),
                ));
            }
            Ok(MapEntry::PointStamped {
                vol_tag: u64::from_le_bytes(value[2..10].try_into().expect("length checked")),
                offset: u64::from_le_bytes(value[10..18].try_into().expect("length checked")),
                incarnation,
            })
        }
        BLOCK_MAP_KIND_RUN_STAMPED => {
            if value.len() != BLOCK_MAP_RUN_STAMPED_VALUE_LEN {
                return Err(KvError::Corrupt(format!(
                    "block-map RUN2 value must be {BLOCK_MAP_RUN_STAMPED_VALUE_LEN} \
                     bytes, got {}",
                    value.len()
                )));
            }
            let len = u32::from_le_bytes(value[18..22].try_into().expect("length checked"));
            validate_run_len(len)?;
            let start_incarnation =
                u64::from_le_bytes(value[22..30].try_into().expect("length checked"));
            // Zero stamp: the POINT2 law. Overflowing stamp arithmetic:
            // the emitter verified every ACTUAL minted stamp, so a span
            // whose `start + len - 1` wraps cannot have been emitted.
            if start_incarnation == 0 || start_incarnation.checked_add(u64::from(len) - 1).is_none()
            {
                return Err(KvError::Corrupt(format!(
                    "block-map RUN2 value carries start incarnation {start_incarnation} \
                     with len {len}, which no emitter produces (design §12)"
                )));
            }
            Ok(MapEntry::RunStamped {
                vol_tag: u64::from_le_bytes(value[2..10].try_into().expect("length checked")),
                start_offset: u64::from_le_bytes(value[10..18].try_into().expect("length checked")),
                len,
                start_incarnation,
            })
        }
        other => Err(KvError::Corrupt(format!(
            "block-map value carries unknown kind {other}"
        ))),
    }
}

/// One staged mapping operation: bind (`Put`) or remove (`Delete`) the
/// mapping at `(owner_ino, block_index)`. Built by the PR 2 crossing /
/// publish paths, drained into the layout transaction by the meta
/// backend (`KvTx::stage_block_map`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockMapOp {
    /// Bind `entry` at the index: one `Put`.
    Put {
        owner_ino: u64,
        block_index: u32,
        entry: MapEntry,
    },
    /// Remove the mapping: one `Delete`.
    Delete { owner_ino: u64, block_index: u32 },
}

impl BlockMapOp {
    /// The record key this op targets ([`block_map_key`]'s refusal
    /// included — the reserved index cannot be staged).
    pub fn key(&self) -> Result<[u8; BLOCK_MAP_KEY_LEN], KvError> {
        match self {
            BlockMapOp::Put {
                owner_ino,
                block_index,
                ..
            }
            | BlockMapOp::Delete {
                owner_ino,
                block_index,
            } => block_map_key(*owner_ino, *block_index),
        }
    }
}

/// The parsed head sentinel: `kvmap:1[;sweep:K][;gen:N]` — the A2 sweep
/// cursor (design §3 truncate/unlink) plus, since PR 5b, the **map
/// generation** (design §11's belt): a monotone per-ino counter of
/// committed migration trains on the multi-writer plane, so a SHIPPED
/// train whose carried base generation lags the durable head refuses
/// (retried-class) instead of composing claims onto a base that moved.
/// `gen == 0` encodes ABSENT — solo volumes' heads stay byte-identical
/// to the pre-belt form (the dark-by-default posture).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvmapHead {
    pub sweep_cursor: Option<u32>,
    pub gen: u64,
}

impl KvmapHead {
    /// Encode the canonical head string (`gen` omitted at 0 — the
    /// pre-belt byte-identity law).
    pub fn encode(&self) -> String {
        let mut out = format!("{KVMAP_HEAD_PREFIX}{KVMAP_HEAD_MAJOR}");
        if let Some(k) = self.sweep_cursor {
            out.push_str(&format!(";sweep:{k}"));
        }
        if self.gen > 0 {
            out.push_str(&format!(";gen:{}", self.gen));
        }
        out
    }
}

/// Parse a canonical decimal — the exact-grammar guard: `u32::from_str`
/// alone would accept `007`/`+7`, forms [`KvmapHead::encode`] never
/// produces, and a parser wider than its encoder is a format hole.
fn parse_canonical_u32(s: &str, what: &str) -> Result<u32, KvError> {
    let v: u32 = s
        .parse()
        .map_err(|_| KvError::Corrupt(format!("kvmap head {what} {s:?} is not a u32")))?;
    if v.to_string() != s {
        return Err(KvError::Corrupt(format!(
            "kvmap head {what} {s:?} is not canonical decimal"
        )));
    }
    Ok(v)
}

/// [`parse_canonical_u32`] at generation width (the belt counter is
/// unbounded-monotone, never an index).
fn parse_canonical_u64(s: &str, what: &str) -> Result<u64, KvError> {
    let v: u64 = s
        .parse()
        .map_err(|_| KvError::Corrupt(format!("kvmap head {what} {s:?} is not a u64")))?;
    if v.to_string() != s {
        return Err(KvError::Corrupt(format!(
            "kvmap head {what} {s:?} is not canonical decimal"
        )));
    }
    Ok(v)
}

/// Parse a head sentinel string. Refuses loud: non-`kvmap:` strings, an
/// unknown major (always-forward — a newer grammar means a newer
/// binary's volume, and guessing a sweep state could resurrect shadowed
/// mappings), and anything outside the exact
/// `kvmap:1[;sweep:K][;gen:N]` grammar (segments in encoder order only;
/// `gen:0` refused — the encoder omits an absent generation, and a
/// parser wider than its encoder is a format hole).
pub fn parse_kvmap_head(s: &str) -> Result<KvmapHead, KvError> {
    let Some(rest) = s.strip_prefix(KVMAP_HEAD_PREFIX) else {
        return Err(KvError::Corrupt(format!(
            "not a kvmap head: {s:?} (expected the {KVMAP_HEAD_PREFIX:?} prefix)"
        )));
    };
    let mut segs = rest.split(';');
    let major = parse_canonical_u32(segs.next().unwrap_or(""), "major version")?;
    if major != KVMAP_HEAD_MAJOR {
        return Err(KvError::Corrupt(format!(
            "kvmap head major version {major} is not {KVMAP_HEAD_MAJOR} (a newer \
             binary's head — refusing to guess its sweep state)"
        )));
    }
    let mut sweep_cursor = None;
    let mut gen = 0u64;
    let mut next = segs.next();
    if let Some(seg) = next {
        if let Some(k) = seg.strip_prefix("sweep:") {
            sweep_cursor = Some(parse_canonical_u32(k, "sweep cursor")?);
            next = segs.next();
        }
    }
    if let Some(seg) = next {
        let Some(g) = seg.strip_prefix("gen:") else {
            return Err(KvError::Corrupt(format!(
                "kvmap head carries unknown segment {seg:?}"
            )));
        };
        gen = parse_canonical_u64(g, "map generation")?;
        if gen == 0 {
            return Err(KvError::Corrupt(
                "kvmap head carries gen:0, which the encoder never produces (an \
                 absent generation is omitted)"
                    .to_string(),
            ));
        }
        next = segs.next();
    }
    if let Some(seg) = next {
        return Err(KvError::Corrupt(format!(
            "kvmap head carries unknown segment {seg:?}"
        )));
    }
    Ok(KvmapHead { sweep_cursor, gen })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bound helper's raw end bound is the ONE place the reserved
    /// index appears — as a bound, never a key.
    #[test]
    fn range_end_bound_is_the_reserved_index() {
        let (lo, hi) = index_range_from(9, 4);
        assert_eq!(decode_block_map_key(&lo).unwrap(), (9, 4));
        assert!(decode_block_map_key(&hi).is_err(), "the bound is not a key");
        assert!(lo < hi);
    }

    /// Kind discriminants are pinned and distinct — the on-disk format.
    #[test]
    fn kind_discriminants_pin_the_on_disk_format() {
        assert_eq!(BLOCK_MAP_KIND_POINT, 1);
        assert_eq!(BLOCK_MAP_KIND_STRING, 2);
        assert_eq!(BLOCK_MAP_KIND_RUN, 3, "PR 6a claimed the PR-1 reservation");
        assert_eq!(BLOCK_MAP_KIND_POINT_STAMPED, 4);
        assert_eq!(BLOCK_MAP_KIND_RUN_STAMPED, 5);
        assert_eq!(RUN_LEN_MAX, 4096, "A6's floor-probe bound is a format law");
    }
}
