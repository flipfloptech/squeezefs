//! **Tree 0 — the control tree's `slot_state` records**
//! (docs/design-symmetric-metadata.md §5.2.2 / §5.4.2; incompat bit 17).
//!
//! Under the slot-tree forest every non-native slot tree's root has a
//! durable home outside the fixed ledger: a `slot_state:{s}` record in
//! the volume's control tree ([`super::record::TREE_CONTROL`]), whose own
//! root the ledger names. PR 1 writes the `Unleased` form only — one
//! appender, roots published by its checkpoint task (`tails` stays empty
//! until PR 2's handover path records flushed-leaf log tails); the
//! `Leased` form is the PR-4 lessee record readers resolve slots by
//! (KD-SYM-17). Both are versioned and refuse a future version loud
//! (forward-only format).
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
pub const SLOT_STATE_VERSION: u8 = 1;
const VARIANT_UNLEASED: u8 = 1;
const VARIANT_LEASED: u8 = 2;
/// `version ‖ variant ‖ root.addr ‖ root.seq ‖ cursor ‖ g ‖ n_tails: u16`
/// before the tail entries (`leaf addr: u64 ‖ tail: u32` each).
const UNLEASED_FIXED_LEN: usize = 1 + 1 + 8 + 8 + 8 + 4 + 2;
const TAIL_ENTRY_LEN: usize = 8 + 4;
/// `version ‖ variant ‖ appender_id: u32 ‖ g: u32 ‖ page_addr: u64`.
const LEASED_LEN: usize = 1 + 1 + 4 + 4 + 8;

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
    /// generation and the flushed-leaf log tails recorded at the last
    /// release (§5.8.2 — empty until PR 2 records them).
    Unleased {
        root: RootPtr,
        cursor: u64,
        g: u32,
        tails: Vec<(u64, u32)>,
    },
    /// Leased: the lessee's identity — what makes slot resolution a
    /// control-plane projection (KD-SYM-17). No writer until PR 4: it is
    /// decoded here so a volume a PR-4 binary leased refuses THIS binary
    /// with the right message ("slot leases are not part of this binary's
    /// forest") instead of a generic unknown-variant corruption.
    Leased {
        appender_id: u32,
        g: u32,
        page_addr: u64,
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
            } => {
                let mut out = Vec::with_capacity(LEASED_LEN);
                out.push(SLOT_STATE_VERSION);
                out.push(VARIANT_LEASED);
                out.extend_from_slice(&appender_id.to_le_bytes());
                out.extend_from_slice(&g.to_le_bytes());
                out.extend_from_slice(&page_addr.to_le_bytes());
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
                let n = usize::from(u16::from_le_bytes([value[30], value[31]]));
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
                })
            }
            other => Err(KvError::Corrupt(format!(
                "slot_state record carries unknown variant {other}"
            ))),
        }
    }

    /// The slot tree's root when this record names one (`Unleased`).
    pub fn root(&self) -> Option<RootPtr> {
        match self {
            SlotState::Unleased { root, .. } => Some(*root),
            SlotState::Leased { .. } => None,
        }
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
