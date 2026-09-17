//! **Tree 0 — the control tree's `slot_state` records**
//! (docs/design-symmetric-metadata.md §5.2.2 / §5.4.2; incompat bit 17).
//!
//! Under the slot-tree forest every non-native slot tree's root has a
//! durable home outside the fixed ledger: a `slot_state:{s}` record in
//! the volume's control tree ([`super::record::TREE_CONTROL`]), whose own
//! root the ledger names. An `Unleased` record names the tree's root,
//! its ino cursor, its lease generation `g`, its extent count (the
//! affinity cap's durable input, §5.1.2) and the manager's seq at the
//! last release (`prefer: unleased-then-idle`'s ordering key, §5.1.2); a
//! `Leased` record names the LESSEE (KD-SYM-17, PR 4) — the root then
//! rides the lessee's appender page. Both are FIXED-SIZE and versioned,
//! and refuse a future version loud (forward-only format; version 3
//! since PR 4 review round 3 moved the flushed-leaf log tails OUT of the
//! `Unleased` image — bit 17 is stamped by no field volume, so no older
//! record exists outside a test tempdir).
//!
//! **The tails ride their own record, `slot_tails:{s}`** (§5.8.2 — the
//! flushed-leaf log tails the last release recorded, stamped with the
//! generation `g` they attest; consumed by PR 5's frame screen): written
//! by the RELEASE in the same control entry as the `Unleased` record and
//! left alone by the grant, so a `Leased` slot keeps the tails of its
//! previous generation readable (rule 2 of the screen reads them WHILE
//! the slot is leased at `g + 1`), the `Unleased`/`Leased` images stay
//! fixed-size (the door's parking pre-admission — review round 3, Issue
//! 24 — is exact), and a grant never rewrites a tails set. The set is
//! INLINE while it fits the volume's KV value cap and SPILLS to heap
//! extents the record names above it (design §5.2.2's `tails:
//! ExtentRef?`; [`SlotTails`]).
//!
//! Keys are memcmp-ordered: `b"slot_state:" ‖ slot: u32 BE` (and
//! `b"slot_tails:" ‖ slot: u32 BE`), so a range walk over a prefix yields
//! slots in index order — the census's shape.

use super::record::ForestSlot;
use super::tree::RootPtr;
use super::KvError;

/// Key prefix of every slot-state record in tree 0.
pub const SLOT_STATE_KEY_PREFIX: &[u8] = b"slot_state:";
/// `prefix ‖ slot: u32 BE`.
pub const SLOT_STATE_KEY_LEN: usize = SLOT_STATE_KEY_PREFIX.len() + 4;

/// Record value version (byte 0 of every image).
pub const SLOT_STATE_VERSION: u8 = 3;
const VARIANT_UNLEASED: u8 = 1;
const VARIANT_LEASED: u8 = 2;
/// `version ‖ variant ‖ root.addr ‖ root.seq ‖ cursor ‖ g ‖
/// slot_tree_extents: u32 ‖ last_written: u64 ‖ seq_floor: u64`.
/// `seq_floor` is the seq-space law's word (design §5.1.4 / §5.8.2, PR 4
/// review round 2): the departing ring's stamp frontier at the release —
/// every record of the slot carries a seq strictly below it, and the
/// next lessee's ring is raised above it at the grant.
pub const UNLEASED_LEN: usize = 1 + 1 + 8 + 8 + 8 + 4 + 4 + 8 + 8;
/// `version ‖ variant ‖ appender_id: u32 ‖ g: u32 ‖ page_addr: u64 ‖
/// root.addr ‖ root.seq ‖ cursor ‖ slot_tree_extents: u32 ‖ seq_floor:
/// u64` — the words AS OF THE GRANT ride the lessee record too: between
/// the grant and the lessee's first page write the tree's root has no
/// other durable home (the grant replaced the `Unleased` record that
/// carried it); the page's entry supersedes them once written (a newer
/// root seq). `seq_floor` = the floor the grant raised the lessee's ring
/// above (a re-adoption raises it again, idempotently).
pub const LEASED_LEN: usize = 1 + 1 + 4 + 4 + 8 + 8 + 8 + 8 + 4 + 8;

/// The tree-0 key of slot `slot`'s state record.
pub fn slot_state_key(slot: ForestSlot) -> Vec<u8> {
    let mut k = Vec::with_capacity(SLOT_STATE_KEY_LEN);
    k.extend_from_slice(SLOT_STATE_KEY_PREFIX);
    k.extend_from_slice(&slot.to_be_bytes());
    k
}

/// Inclusive `[start, end]` bounds covering every slot-state record —
/// the census walk's window.
pub fn slot_state_key_range() -> (Vec<u8>, Vec<u8>) {
    (slot_state_key(0), slot_state_key(ForestSlot::MAX))
}

/// Decode a slot-state key back to its slot; wrong prefix or length is
/// corruption.
pub fn decode_slot_state_key(key: &[u8]) -> Result<ForestSlot, KvError> {
    if key.len() != SLOT_STATE_KEY_LEN || !key.starts_with(SLOT_STATE_KEY_PREFIX) {
        return Err(KvError::Corrupt(format!(
            "slot_state key must be {SLOT_STATE_KEY_LEN} bytes under the {:?} prefix, got {} \
             bytes",
            String::from_utf8_lossy(SLOT_STATE_KEY_PREFIX),
            key.len()
        )));
    }
    let p = SLOT_STATE_KEY_PREFIX.len();
    Ok(ForestSlot::from_be_bytes([
        key[p],
        key[p + 1],
        key[p + 2],
        key[p + 3],
    ]))
}

/// One slot's durable state in tree 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// No lessee: the slot tree's root, its ino cursor, its lease
    /// generation, its extent count and the manager's seq at the last
    /// release (the tails that release recorded ride `slot_tails:{s}`).
    Unleased {
        root: RootPtr,
        cursor: u64,
        g: u32,
        slot_tree_extents: u32,
        last_written: u64,
        /// The seq-space floor (see [`UNLEASED_LEN`]).
        seq_floor: u64,
    },
    /// Leased: the lessee's identity — what makes slot resolution a
    /// control-plane projection (KD-SYM-17) — plus the tree's words as of
    /// the grant (the root's durable home until the lessee's first page
    /// write names a newer one). Written by the manager at every grant.
    Leased {
        appender_id: u32,
        g: u32,
        page_addr: u64,
        root: RootPtr,
        cursor: u64,
        slot_tree_extents: u32,
        /// The seq-space floor (see [`LEASED_LEN`]).
        seq_floor: u64,
    },
}

impl SlotState {
    /// Little-endian image, versioned (see the module docs).
    pub fn encode(&self) -> Vec<u8> {
        match self {
            SlotState::Unleased {
                root,
                cursor,
                g,
                slot_tree_extents,
                last_written,
                seq_floor,
            } => {
                let mut out = Vec::with_capacity(UNLEASED_LEN);
                out.push(SLOT_STATE_VERSION);
                out.push(VARIANT_UNLEASED);
                out.extend_from_slice(&root.addr.to_le_bytes());
                out.extend_from_slice(&root.seq.to_le_bytes());
                out.extend_from_slice(&cursor.to_le_bytes());
                out.extend_from_slice(&g.to_le_bytes());
                out.extend_from_slice(&slot_tree_extents.to_le_bytes());
                out.extend_from_slice(&last_written.to_le_bytes());
                out.extend_from_slice(&seq_floor.to_le_bytes());
                out
            }
            SlotState::Leased {
                appender_id,
                g,
                page_addr,
                root,
                cursor,
                slot_tree_extents,
                seq_floor,
            } => {
                let mut out = Vec::with_capacity(LEASED_LEN);
                out.push(SLOT_STATE_VERSION);
                out.push(VARIANT_LEASED);
                out.extend_from_slice(&appender_id.to_le_bytes());
                out.extend_from_slice(&g.to_le_bytes());
                out.extend_from_slice(&page_addr.to_le_bytes());
                out.extend_from_slice(&root.addr.to_le_bytes());
                out.extend_from_slice(&root.seq.to_le_bytes());
                out.extend_from_slice(&cursor.to_le_bytes());
                out.extend_from_slice(&slot_tree_extents.to_le_bytes());
                out.extend_from_slice(&seq_floor.to_le_bytes());
                out
            }
        }
    }

    /// Decode + validate (total: every failure is loud corruption; §9
    /// bounds rule — every length checked against its container).
    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        if value.len() < 2 {
            return Err(KvError::Corrupt(format!(
                "slot_state record too short ({} bytes)",
                value.len()
            )));
        }
        if value[0] != SLOT_STATE_VERSION {
            return Err(KvError::Corrupt(format!(
                "slot_state record version {} — this binary writes {SLOT_STATE_VERSION} and \
                 the format is forward-only (upgrade squeezefs)",
                value[0]
            )));
        }
        match value[1] {
            VARIANT_UNLEASED => {
                if value.len() != UNLEASED_LEN {
                    return Err(KvError::Corrupt(format!(
                        "slot_state Unleased record must be {UNLEASED_LEN} bytes, got {}",
                        value.len()
                    )));
                }
                Ok(SlotState::Unleased {
                    root: RootPtr {
                        addr: le64(value, 2),
                        seq: le64(value, 10),
                    },
                    cursor: le64(value, 18),
                    g: le32(value, 26),
                    slot_tree_extents: le32(value, 30),
                    last_written: le64(value, 34),
                    seq_floor: le64(value, 42),
                })
            }
            VARIANT_LEASED => {
                if value.len() != LEASED_LEN {
                    return Err(KvError::Corrupt(format!(
                        "slot_state Leased record must be {LEASED_LEN} bytes, got {}",
                        value.len()
                    )));
                }
                Ok(SlotState::Leased {
                    appender_id: le32(value, 2),
                    g: le32(value, 6),
                    page_addr: le64(value, 10),
                    root: RootPtr {
                        addr: le64(value, 18),
                        seq: le64(value, 26),
                    },
                    cursor: le64(value, 34),
                    slot_tree_extents: le32(value, 42),
                    seq_floor: le64(value, 46),
                })
            }
            other => Err(KvError::Corrupt(format!(
                "slot_state record carries unknown variant {other}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// `slot_tails:{slot}` — the flushed-leaf log tails the last release of a
// slot recorded (design-symmetric-metadata §5.8.2; PR 4 review round 3,
// Issue 21).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The custody QUARANTINE record — `custody_quarantine:{slot}` (PR 10, review
// round 8, Issue 36): a slot recovered from an EARLY death record refuses
// fresh custody grants until `until_ms`; the record is written in the
// recovery's own tree-0 entry (beside the slot's `Unleased`), read at every
// arm so a manager restart inside the window keeps the quarantine, and
// deleted once expired.
// ---------------------------------------------------------------------------

/// Key prefix of every custody-quarantine record in tree 0.
pub const CUSTODY_QUARANTINE_KEY_PREFIX: &[u8] = b"custody_quarantine:";
/// `prefix ‖ slot: u32 BE`.
pub const CUSTODY_QUARANTINE_KEY_LEN: usize = CUSTODY_QUARANTINE_KEY_PREFIX.len() + 4;
/// Record value version (byte 0).
pub const CUSTODY_QUARANTINE_VERSION: u8 = 1;
/// `version ‖ until_ms: u64 LE`.
pub const CUSTODY_QUARANTINE_LEN: usize = 1 + 8;

/// The tree-0 key of slot `slot`'s custody-quarantine record.
pub fn custody_quarantine_key(slot: ForestSlot) -> Vec<u8> {
    let mut k = Vec::with_capacity(CUSTODY_QUARANTINE_KEY_LEN);
    k.extend_from_slice(CUSTODY_QUARANTINE_KEY_PREFIX);
    k.extend_from_slice(&slot.to_be_bytes());
    k
}

/// Inclusive `[start, end]` bounds covering every custody-quarantine record.
pub fn custody_quarantine_key_range() -> (Vec<u8>, Vec<u8>) {
    (
        custody_quarantine_key(0),
        custody_quarantine_key(ForestSlot::MAX),
    )
}

/// Decode a custody-quarantine key back to its slot.
pub fn decode_custody_quarantine_key(key: &[u8]) -> Result<ForestSlot, KvError> {
    if key.len() != CUSTODY_QUARANTINE_KEY_LEN || !key.starts_with(CUSTODY_QUARANTINE_KEY_PREFIX) {
        return Err(KvError::Corrupt(format!(
            "custody_quarantine key must be {CUSTODY_QUARANTINE_KEY_LEN} bytes under the {:?} \
             prefix, got {} bytes",
            String::from_utf8_lossy(CUSTODY_QUARANTINE_KEY_PREFIX),
            key.len()
        )));
    }
    let p = CUSTODY_QUARANTINE_KEY_PREFIX.len();
    Ok(ForestSlot::from_be_bytes([
        key[p],
        key[p + 1],
        key[p + 2],
        key[p + 3],
    ]))
}

/// Encode a custody-quarantine value: `version ‖ until_ms` (Unix ms).
pub fn encode_custody_quarantine(until_ms: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(CUSTODY_QUARANTINE_LEN);
    v.push(CUSTODY_QUARANTINE_VERSION);
    v.extend_from_slice(&until_ms.to_le_bytes());
    v
}

/// Decode a custody-quarantine value to its `until_ms`; total over every
/// byte string (a wrong length or version refuses).
pub fn decode_custody_quarantine(value: &[u8]) -> Result<u64, KvError> {
    if value.len() != CUSTODY_QUARANTINE_LEN {
        return Err(KvError::Corrupt(format!(
            "custody_quarantine record must be {CUSTODY_QUARANTINE_LEN} bytes, got {}",
            value.len()
        )));
    }
    if value[0] != CUSTODY_QUARANTINE_VERSION {
        return Err(KvError::Corrupt(format!(
            "custody_quarantine record carries version {}, expected {CUSTODY_QUARANTINE_VERSION}",
            value[0]
        )));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&value[1..9]);
    Ok(u64::from_le_bytes(b))
}

/// Key prefix of every slot-tails record in tree 0.
pub const SLOT_TAILS_KEY_PREFIX: &[u8] = b"slot_tails:";
/// `prefix ‖ slot: u32 BE`.
pub const SLOT_TAILS_KEY_LEN: usize = SLOT_TAILS_KEY_PREFIX.len() + 4;
/// Record value version (byte 0).
pub const SLOT_TAILS_VERSION: u8 = 1;
/// `version ‖ g: u32 ‖ n_tails: u16` before the payload.
pub const SLOT_TAILS_FIXED_LEN: usize = 1 + 4 + 2;
/// One tail entry: `leaf addr: u64 ‖ tail: u32` — inline in the record or
/// in a spill extent's image.
pub const TAIL_ENTRY_LEN: usize = 8 + 4;
/// `n_tails` sentinel: the set is SPILLED — `n_runs: u16 ‖ (extent addr:
/// u64 ‖ count: u32) × n_runs ‖ xxh3: u64` follow instead of inline
/// entries.
pub const TAILS_SPILLED: u16 = u16::MAX;
const SPILL_FIXED_LEN: usize = 2;
const SPILL_RUN_LEN: usize = 8 + 4;
const SPILL_CHECKSUM_LEN: usize = 8;

/// The tree-0 key of slot `slot`'s tails record.
pub fn slot_tails_key(slot: ForestSlot) -> Vec<u8> {
    let mut k = Vec::with_capacity(SLOT_TAILS_KEY_LEN);
    k.extend_from_slice(SLOT_TAILS_KEY_PREFIX);
    k.extend_from_slice(&slot.to_be_bytes());
    k
}

/// Inclusive `[start, end]` bounds covering every slot-tails record.
pub fn slot_tails_key_range() -> (Vec<u8>, Vec<u8>) {
    (slot_tails_key(0), slot_tails_key(ForestSlot::MAX))
}

/// Decode a slot-tails key back to its slot.
pub fn decode_slot_tails_key(key: &[u8]) -> Result<ForestSlot, KvError> {
    if key.len() != SLOT_TAILS_KEY_LEN || !key.starts_with(SLOT_TAILS_KEY_PREFIX) {
        return Err(KvError::Corrupt(format!(
            "slot_tails key must be {SLOT_TAILS_KEY_LEN} bytes under the {:?} prefix, got {} \
             bytes",
            String::from_utf8_lossy(SLOT_TAILS_KEY_PREFIX),
            key.len()
        )));
    }
    let p = SLOT_TAILS_KEY_PREFIX.len();
    Ok(ForestSlot::from_be_bytes([
        key[p],
        key[p + 1],
        key[p + 2],
        key[p + 3],
    ]))
}

/// The inline tails a record of `value_cap` bytes can carry (the KV
/// value cap less the fixed part, and below the spill sentinel).
pub fn inline_tails_cap(value_cap: usize) -> usize {
    (value_cap.saturating_sub(SLOT_TAILS_FIXED_LEN) / TAIL_ENTRY_LEN)
        .min(usize::from(TAILS_SPILLED) - 1)
}

/// The entries one spill extent of `node_size` bytes holds.
pub fn tails_per_spill_extent(node_size: usize) -> usize {
    (node_size / TAIL_ENTRY_LEN).max(1)
}

/// A slot's tails set as the record carries it: INLINE while it fits the
/// value cap, else SPILLED to heap extents — each holding
/// [`tails_per_spill_extent`] entries as a raw image, the record's
/// checksum over the concatenated payload; the extents are written and
/// barriered BEFORE the record names them and released when the record
/// is superseded (the release site's law, `KvMetaBackend::stage_slot_tails`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotTails {
    Inline(Vec<(u64, u32)>),
    Spilled(TailsSpill),
}

/// The spill locator of a tails set past the inline cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailsSpill {
    /// `(extent byte address, entries in it)`, in payload order.
    pub runs: Vec<(u64, u32)>,
    /// xxh3 over the concatenated entry images.
    pub checksum: u64,
}

impl Default for SlotTails {
    fn default() -> Self {
        SlotTails::Inline(Vec::new())
    }
}

impl SlotTails {
    /// Entries the set names (inline or spilled).
    pub fn count(&self) -> usize {
        match self {
            SlotTails::Inline(v) => v.len(),
            SlotTails::Spilled(sp) => sp.runs.iter().map(|(_, n)| *n as usize).sum(),
        }
    }

    /// Whether a set of `n` entries fits inline under `value_cap`.
    pub fn fits_inline(n: usize, value_cap: usize) -> bool {
        n <= inline_tails_cap(value_cap)
    }

    /// The spill extents' byte addresses (empty when inline).
    pub fn spill_addrs(&self) -> Vec<u64> {
        match self {
            SlotTails::Inline(_) => Vec::new(),
            SlotTails::Spilled(sp) => sp.runs.iter().map(|(a, _)| *a).collect(),
        }
    }
}

/// The raw image of tail entries (a spill extent's payload;
/// [`checksum_tails`] covers the concatenation).
pub fn encode_tail_entries(tails: &[(u64, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(tails.len() * TAIL_ENTRY_LEN);
    for (leaf, tail) in tails {
        out.extend_from_slice(&leaf.to_le_bytes());
        out.extend_from_slice(&tail.to_le_bytes());
    }
    out
}

/// Decode `count` tail entries off a raw image.
pub fn decode_tail_entries(image: &[u8], count: usize) -> Result<Vec<(u64, u32)>, KvError> {
    if image.len() < count.saturating_mul(TAIL_ENTRY_LEN) {
        return Err(KvError::Corrupt(format!(
            "tails image of {} bytes holds fewer than {count} entries",
            image.len()
        )));
    }
    Ok((0..count)
        .map(|i| {
            let off = i * TAIL_ENTRY_LEN;
            (le64(image, off), le32(image, off + 8))
        })
        .collect())
}

/// xxh3 over a tails set's entry images (the spill checksum).
pub fn checksum_tails(tails: &[(u64, u32)]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&encode_tail_entries(tails))
}

/// Slot `s`'s recorded tails: `g` = the lease generation the release
/// attests (the frame screen's rule 2 reads them for `g` while the slot
/// is leased at `g + 1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotTailsRecord {
    pub g: u32,
    pub tails: SlotTails,
}

impl SlotTailsRecord {
    /// LE image, versioned. Refuses an inline count at or past the spill
    /// sentinel and a spill of more runs than a `u16` names — caller bugs
    /// (the release site decides inline vs spill against the value cap),
    /// never truncated.
    pub fn encode(&self) -> Result<Vec<u8>, KvError> {
        let payload = match &self.tails {
            SlotTails::Inline(v) => v.len() * TAIL_ENTRY_LEN,
            SlotTails::Spilled(sp) => {
                SPILL_FIXED_LEN + sp.runs.len() * SPILL_RUN_LEN + SPILL_CHECKSUM_LEN
            }
        };
        let mut out = Vec::with_capacity(SLOT_TAILS_FIXED_LEN + payload);
        out.push(SLOT_TAILS_VERSION);
        out.extend_from_slice(&self.g.to_le_bytes());
        match &self.tails {
            SlotTails::Inline(v) => {
                let n = u16::try_from(v.len())
                    .ok()
                    .filter(|n| *n != TAILS_SPILLED)
                    .ok_or_else(|| {
                        KvError::Corrupt(format!(
                            "slot_tails record cannot carry {} inline tails (the count is a u16 \
                             below the spill sentinel — spill them)",
                            v.len()
                        ))
                    })?;
                out.extend_from_slice(&n.to_le_bytes());
                out.extend_from_slice(&encode_tail_entries(v));
            }
            SlotTails::Spilled(sp) => {
                let n_runs = u16::try_from(sp.runs.len()).map_err(|_| {
                    KvError::Corrupt(format!(
                        "slot_tails spill cannot name {} runs (the count is a u16)",
                        sp.runs.len()
                    ))
                })?;
                out.extend_from_slice(&TAILS_SPILLED.to_le_bytes());
                out.extend_from_slice(&n_runs.to_le_bytes());
                for (addr, count) in &sp.runs {
                    out.extend_from_slice(&addr.to_le_bytes());
                    out.extend_from_slice(&count.to_le_bytes());
                }
                out.extend_from_slice(&sp.checksum.to_le_bytes());
            }
        }
        Ok(out)
    }

    /// Decode + validate (total; §9 bounds rule).
    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        if value.len() < SLOT_TAILS_FIXED_LEN {
            return Err(KvError::Corrupt(format!(
                "slot_tails record too short ({} bytes)",
                value.len()
            )));
        }
        if value[0] != SLOT_TAILS_VERSION {
            return Err(KvError::Corrupt(format!(
                "slot_tails record version {} — this binary writes {SLOT_TAILS_VERSION} and the \
                 format is forward-only (upgrade squeezefs)",
                value[0]
            )));
        }
        let g = le32(value, 1);
        let n_raw = u16::from_le_bytes([value[5], value[6]]);
        let tails = if n_raw == TAILS_SPILLED {
            if value.len() < SLOT_TAILS_FIXED_LEN + SPILL_FIXED_LEN {
                return Err(KvError::Corrupt(
                    "slot_tails record names a spill but is truncated before the run count"
                        .to_string(),
                ));
            }
            let n_runs = usize::from(u16::from_le_bytes([
                value[SLOT_TAILS_FIXED_LEN],
                value[SLOT_TAILS_FIXED_LEN + 1],
            ]));
            let want = SLOT_TAILS_FIXED_LEN
                + SPILL_FIXED_LEN
                + n_runs * SPILL_RUN_LEN
                + SPILL_CHECKSUM_LEN;
            if value.len() != want {
                return Err(KvError::Corrupt(format!(
                    "slot_tails record names {n_runs} spill runs ({want} bytes) but holds {} \
                     bytes",
                    value.len()
                )));
            }
            let runs = (0..n_runs)
                .map(|i| {
                    let off = SLOT_TAILS_FIXED_LEN + SPILL_FIXED_LEN + i * SPILL_RUN_LEN;
                    (le64(value, off), le32(value, off + 8))
                })
                .collect();
            let checksum = le64(value, want - SPILL_CHECKSUM_LEN);
            SlotTails::Spilled(TailsSpill { runs, checksum })
        } else {
            let n = usize::from(n_raw);
            let want = SLOT_TAILS_FIXED_LEN + n * TAIL_ENTRY_LEN;
            if value.len() != want {
                return Err(KvError::Corrupt(format!(
                    "slot_tails record names {n} tails ({want} bytes) but holds {} bytes",
                    value.len()
                )));
            }
            SlotTails::Inline(decode_tail_entries(&value[SLOT_TAILS_FIXED_LEN..], n)?)
        };
        Ok(Self { g, tails })
    }
}

// ---------------------------------------------------------------------------
// `extent_grant:{appender}` — the manager's attestation of an appender's
// extent grant (design-symmetric-metadata §5.3.3, PR 3).
// ---------------------------------------------------------------------------

/// Key prefix of every extent-grant record in tree 0.
pub const EXTENT_GRANT_KEY_PREFIX: &[u8] = b"extent_grant:";
/// `prefix ‖ appender_id: u32 BE`.
pub const EXTENT_GRANT_KEY_LEN: usize = EXTENT_GRANT_KEY_PREFIX.len() + 4;
/// Record value version (byte 0).
pub const EXTENT_GRANT_VERSION: u8 = 1;
/// `version ‖ n_runs: u16` before the runs (`start: u64 ‖ len: u32` each).
const EXTENT_GRANT_FIXED_LEN: usize = 1 + 2;
const GRANT_RUN_LEN: usize = 8 + 4;

/// The tree-0 key of appender `appender_id`'s grant record.
pub fn extent_grant_key(appender_id: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(EXTENT_GRANT_KEY_LEN);
    k.extend_from_slice(EXTENT_GRANT_KEY_PREFIX);
    k.extend_from_slice(&appender_id.to_be_bytes());
    k
}

/// Inclusive `[start, end]` bounds covering every extent-grant record.
pub fn extent_grant_key_range() -> (Vec<u8>, Vec<u8>) {
    (extent_grant_key(0), extent_grant_key(u32::MAX))
}

/// Decode an extent-grant key back to its appender id.
pub fn decode_extent_grant_key(key: &[u8]) -> Result<u32, KvError> {
    if key.len() != EXTENT_GRANT_KEY_LEN || !key.starts_with(EXTENT_GRANT_KEY_PREFIX) {
        return Err(KvError::Corrupt(format!(
            "extent_grant key must be {EXTENT_GRANT_KEY_LEN} bytes under the {:?} prefix, got \
             {} bytes",
            String::from_utf8_lossy(EXTENT_GRANT_KEY_PREFIX),
            key.len()
        )));
    }
    let p = EXTENT_GRANT_KEY_PREFIX.len();
    Ok(u32::from_be_bytes([
        key[p],
        key[p + 1],
        key[p + 2],
        key[p + 3],
    ]))
}

/// One appender's CURRENT extent grant as the manager attests it: every
/// run the manager carved for it and has not yet had returned — claimed
/// and unclaimed alike (the appender's page names the UNCLAIMED remainder;
/// the difference is what its images occupy). Runs are normalized:
/// ascending, non-empty, non-adjacent. A `ReturnExtents` rewrites the
/// record without the returned extents, so a returned extent the manager
/// reuses for its own images is never "granted" to anyone (fsck C13's
/// candidate set is the granted, claimed, unreachable extents).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtentGrantRecord {
    pub runs: Vec<super::appender::GrantRun>,
}

impl ExtentGrantRecord {
    /// A record over the sorted extent set `extents`, coalesced into runs.
    pub fn from_extents(extents: impl IntoIterator<Item = u64>) -> Self {
        let mut sorted: Vec<u64> = extents.into_iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut runs: Vec<super::appender::GrantRun> = Vec::new();
        for e in sorted {
            match runs.last_mut() {
                Some(r) if r.start + u64::from(r.len) == e && r.len < u32::MAX => r.len += 1,
                _ => runs.push(super::appender::GrantRun { start: e, len: 1 }),
            }
        }
        Self { runs }
    }

    /// Every extent the record grants, ascending.
    pub fn extents(&self) -> impl Iterator<Item = u64> + '_ {
        self.runs
            .iter()
            .flat_map(|r| r.start..r.start + u64::from(r.len))
    }

    /// Extents granted.
    pub fn len(&self) -> u64 {
        self.runs.iter().map(|r| u64::from(r.len)).sum()
    }

    /// Whether the record grants nothing.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Whether `extent` lies inside the grant.
    pub fn contains(&self, extent: u64) -> bool {
        self.runs
            .iter()
            .any(|r| extent >= r.start && extent < r.start + u64::from(r.len))
    }

    /// LE image, versioned. Refuses a run count the `u16` cannot express.
    pub fn encode(&self) -> Result<Vec<u8>, KvError> {
        let n = u16::try_from(self.runs.len()).map_err(|_| {
            KvError::Corrupt(format!(
                "extent_grant record cannot carry {} runs (the count is a u16)",
                self.runs.len()
            ))
        })?;
        let mut out = Vec::with_capacity(EXTENT_GRANT_FIXED_LEN + self.runs.len() * GRANT_RUN_LEN);
        out.push(EXTENT_GRANT_VERSION);
        out.extend_from_slice(&n.to_le_bytes());
        for r in &self.runs {
            out.extend_from_slice(&r.start.to_le_bytes());
            out.extend_from_slice(&r.len.to_le_bytes());
        }
        Ok(out)
    }

    /// Decode + validate (total; every failure is loud corruption). Runs
    /// must be normalized — ascending, non-empty, non-adjacent — so the
    /// image is canonical.
    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        let version = *value
            .first()
            .ok_or_else(|| KvError::Corrupt("extent_grant record is empty".to_string()))?;
        if version != EXTENT_GRANT_VERSION {
            return Err(KvError::Corrupt(format!(
                "extent_grant record version {version} — this binary writes \
                 {EXTENT_GRANT_VERSION} and the format is forward-only (upgrade squeezefs)"
            )));
        }
        if value.len() < EXTENT_GRANT_FIXED_LEN {
            return Err(KvError::Corrupt(
                "extent_grant record truncated before its run count".to_string(),
            ));
        }
        let n = usize::from(u16::from_le_bytes([value[1], value[2]]));
        let want = EXTENT_GRANT_FIXED_LEN + n * GRANT_RUN_LEN;
        if value.len() != want {
            return Err(KvError::Corrupt(format!(
                "extent_grant record names {n} runs ({want} bytes) but holds {} bytes",
                value.len()
            )));
        }
        let mut runs = Vec::with_capacity(n);
        for i in 0..n {
            let off = EXTENT_GRANT_FIXED_LEN + i * GRANT_RUN_LEN;
            let run = super::appender::GrantRun {
                start: le64(value, off),
                len: le32(value, off + 8),
            };
            if run.len == 0 {
                return Err(KvError::Corrupt(
                    "extent_grant record carries an empty run".to_string(),
                ));
            }
            if let Some(prev) = runs.last() {
                let prev: &super::appender::GrantRun = prev;
                let Some(prev_end) = prev.start.checked_add(u64::from(prev.len)) else {
                    return Err(KvError::Corrupt(
                        "extent_grant record run overflows the extent space".to_string(),
                    ));
                };
                if run.start <= prev_end {
                    return Err(KvError::Corrupt(format!(
                        "extent_grant record runs are not normalized ({} + {} then {})",
                        prev.start, prev.len, run.start
                    )));
                }
            }
            if run.start.checked_add(u64::from(run.len)).is_none() {
                return Err(KvError::Corrupt(
                    "extent_grant record run overflows the extent space".to_string(),
                ));
            }
            runs.push(run);
        }
        Ok(Self { runs })
    }
}

#[inline]
fn le64(v: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[off..off + 8]);
    u64::from_le_bytes(b)
}

#[inline]
fn le32(v: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&v[off..off + 4]);
    u32::from_le_bytes(b)
}

// ---------------------------------------------------------------------------
// The set-wide directory-rename lock (design-symmetric-metadata §5.6.4,
// KD-SYM-14; PR 6) — a tree-0 record on VOLUME 0 only (§5.4.2's control
// table). Its holder is an appender identity; the record is the durable
// witness `DirRenameLock` / `DirRenameUnlock` are idempotent against
// (KD-SYM-7). A crashed holder's record outlives it until the recovery
// driver releases it — the expiry law is the holder's membership lease,
// never a TTL on the record.
// ---------------------------------------------------------------------------

/// The ONE tree-0 key of the set-wide directory-rename lock.
pub const DIR_RENAME_KEY: &[u8] = b"dir_rename";
/// Record value version (byte 0).
pub const DIR_RENAME_VERSION: u8 = 1;
/// `version ‖ holder: u32 ‖ term: u64 ‖ since_ns: u64`.
pub const DIR_RENAME_LEN: usize = 1 + 4 + 8 + 8;

/// The lock record: who holds the set-wide directory-rename lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirRenameRecord {
    /// The holder's appender id (on volume 0).
    pub holder: u32,
    /// The SERVING manager's writer era at the take — operator-facing
    /// provenance (which manager incarnation granted the lease; the
    /// release-dead arm names it in its log line). Never a check: a wire
    /// holder's own term is not this volume's writer term.
    pub term: u64,
    /// CLOCK_REALTIME ns of the take (`SystemTime::now()` — operator-
    /// facing: how long the set has been renaming directories under one
    /// lease; readable from tree 0 on any process).
    pub since_ns: u64,
}

impl DirRenameRecord {
    /// LE image, versioned.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DIR_RENAME_LEN);
        out.push(DIR_RENAME_VERSION);
        out.extend_from_slice(&self.holder.to_le_bytes());
        out.extend_from_slice(&self.term.to_le_bytes());
        out.extend_from_slice(&self.since_ns.to_le_bytes());
        out
    }

    /// Decode + validate (total).
    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        let version = *value
            .first()
            .ok_or_else(|| KvError::Corrupt("dir_rename record is empty".to_string()))?;
        if version != DIR_RENAME_VERSION {
            return Err(KvError::Corrupt(format!(
                "dir_rename record version {version} — this binary writes {DIR_RENAME_VERSION} \
                 and the format is forward-only (upgrade squeezefs)"
            )));
        }
        if value.len() != DIR_RENAME_LEN {
            return Err(KvError::Corrupt(format!(
                "dir_rename record must be {DIR_RENAME_LEN} bytes, got {}",
                value.len()
            )));
        }
        Ok(Self {
            holder: le32(value, 1),
            term: le64(value, 5),
            since_ns: le64(value, 13),
        })
    }
}
