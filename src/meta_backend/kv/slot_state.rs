//! **Tree 0 — the control tree's `slot_state` records**
//! (docs/design-symmetric-metadata.md §5.2.2 / §5.4.2; incompat bit 17).
//!
//! Under the slot-tree forest every non-native slot tree's root has a
//! durable home outside the fixed ledger: a `slot_state:{s}` record in
//! the volume's control tree ([`super::record::TREE_CONTROL`]), whose own
//! root the ledger names. An `Unleased` record names the tree's root,
//! its ino cursor, its lease generation `g`, its extent count (the
//! affinity cap's durable input, §5.1.2), the manager's seq at the last
//! release (`prefer: unleased-then-idle`'s ordering key, §5.1.2) and the
//! flushed-leaf log tails the last release recorded (§5.8.2 — consumed by
//! PR 5's frame screen); a `Leased` record names the LESSEE (KD-SYM-17,
//! PR 4) — the root then rides the lessee's appender page. Both are
//! versioned and refuse a future version loud (forward-only format;
//! version 2 since PR 4 added `slot_tree_extents` and `last_written` to
//! the `Unleased` image — bit 17 is stamped by no field volume, so no
//! version-1 record exists outside a test tempdir).
//!
//! Keys are memcmp-ordered: `b"slot_state:" ‖ slot: u32 BE`, so a range
//! walk over the prefix yields slots in index order — the census's
//! shape.

use super::record::ForestSlot;
use super::tree::RootPtr;
use super::KvError;

/// Key prefix of every slot-state record in tree 0.
pub const SLOT_STATE_KEY_PREFIX: &[u8] = b"slot_state:";
/// `prefix ‖ slot: u32 BE`.
pub const SLOT_STATE_KEY_LEN: usize = SLOT_STATE_KEY_PREFIX.len() + 4;

/// Record value version (byte 0 of every image).
pub const SLOT_STATE_VERSION: u8 = 2;
const VARIANT_UNLEASED: u8 = 1;
const VARIANT_LEASED: u8 = 2;
/// `version ‖ variant ‖ root.addr ‖ root.seq ‖ cursor ‖ g ‖
/// slot_tree_extents: u32 ‖ last_written: u64 ‖ seq_floor: u64 ‖
/// n_tails: u16` before the tail entries (`leaf addr: u64 ‖ tail: u32`
/// each). `seq_floor` is the seq-space law's word (design §5.1.4 /
/// §5.8.2, PR 4 review round 2): the departing ring's stamp frontier at
/// the release — every record of the slot carries a seq strictly below
/// it, and the next lessee's ring is raised above it at the grant.
pub const UNLEASED_FIXED_LEN: usize = 1 + 1 + 8 + 8 + 8 + 4 + 4 + 8 + 8 + 2;
const TAIL_ENTRY_LEN: usize = 8 + 4;
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
    /// generation, its extent count, the manager's seq at the last
    /// release and the flushed-leaf log tails that release recorded
    /// (§5.8.2 — the handover writes them; PR 5's frame screen reads them).
    Unleased {
        root: RootPtr,
        cursor: u64,
        g: u32,
        slot_tree_extents: u32,
        last_written: u64,
        /// The seq-space floor (see [`UNLEASED_FIXED_LEN`]).
        seq_floor: u64,
        tails: Vec<(u64, u32)>,
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
    /// Little-endian image, versioned (see the module docs). Refuses a
    /// tails vector the `u16` count cannot express (the design's inline
    /// bound is ≈ 4,000 entries — the KV value cap — so a longer one is a
    /// caller bug, never truncated).
    pub fn encode(&self) -> Result<Vec<u8>, KvError> {
        match self {
            SlotState::Unleased {
                root,
                cursor,
                g,
                slot_tree_extents,
                last_written,
                seq_floor,
                tails,
            } => {
                let n = u16::try_from(tails.len()).map_err(|_| {
                    KvError::Corrupt(format!(
                        "slot_state record cannot carry {} tails (the count is a u16)",
                        tails.len()
                    ))
                })?;
                let mut out = Vec::with_capacity(UNLEASED_FIXED_LEN + tails.len() * TAIL_ENTRY_LEN);
                out.push(SLOT_STATE_VERSION);
                out.push(VARIANT_UNLEASED);
                out.extend_from_slice(&root.addr.to_le_bytes());
                out.extend_from_slice(&root.seq.to_le_bytes());
                out.extend_from_slice(&cursor.to_le_bytes());
                out.extend_from_slice(&g.to_le_bytes());
                out.extend_from_slice(&slot_tree_extents.to_le_bytes());
                out.extend_from_slice(&last_written.to_le_bytes());
                out.extend_from_slice(&seq_floor.to_le_bytes());
                out.extend_from_slice(&n.to_le_bytes());
                for (leaf, tail) in tails {
                    out.extend_from_slice(&leaf.to_le_bytes());
                    out.extend_from_slice(&tail.to_le_bytes());
                }
                Ok(out)
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
                Ok(out)
            }
        }
    }

    /// Decode + validate; every failure is loud corruption (a slot root
    /// this record mis-states is a whole slot tree unreachable).
    pub fn decode(value: &[u8]) -> Result<Self, KvError> {
        let version = *value
            .first()
            .ok_or_else(|| KvError::Corrupt("slot_state record is empty".to_string()))?;
        if version != SLOT_STATE_VERSION {
            return Err(KvError::Corrupt(format!(
                "slot_state record version {version} — this binary writes {SLOT_STATE_VERSION} \
                 and the format is forward-only (upgrade squeezefs)"
            )));
        }
        let variant = *value.get(1).ok_or_else(|| {
            KvError::Corrupt("slot_state record truncated before its variant byte".to_string())
        })?;
        match variant {
            VARIANT_UNLEASED => {
                if value.len() < UNLEASED_FIXED_LEN {
                    return Err(KvError::Corrupt(format!(
                        "slot_state Unleased record of {} bytes is shorter than its \
                         {UNLEASED_FIXED_LEN}-byte fixed part",
                        value.len()
                    )));
                }
                let addr = le64(value, 2);
                let seq = le64(value, 10);
                let cursor = le64(value, 18);
                let g = le32(value, 26);
                let slot_tree_extents = le32(value, 30);
                let last_written = le64(value, 34);
                let seq_floor = le64(value, 42);
                let n = usize::from(u16::from_le_bytes([value[50], value[51]]));
                let want = UNLEASED_FIXED_LEN + n * TAIL_ENTRY_LEN;
                if value.len() != want {
                    return Err(KvError::Corrupt(format!(
                        "slot_state Unleased record names {n} tails ({want} bytes) but holds {} \
                         bytes",
                        value.len()
                    )));
                }
                let tails = (0..n)
                    .map(|i| {
                        let off = UNLEASED_FIXED_LEN + i * TAIL_ENTRY_LEN;
                        (le64(value, off), le32(value, off + 8))
                    })
                    .collect();
                Ok(SlotState::Unleased {
                    root: RootPtr { addr, seq },
                    cursor,
                    g,
                    slot_tree_extents,
                    last_written,
                    seq_floor,
                    tails,
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
