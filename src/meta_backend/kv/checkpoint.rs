//! Root-ledger records: the checksummed A/B-style slot array checkpoints
//! flip tree roots through (design §4.1 "Root ledger", §4.6 pt 2).
//!
//! **PR K3 lands the ledger *record* part only** — slot encode/decode,
//! round-robin slot placement, durable write, and newest-valid-wins
//! selection with torn-slot fallback. Checkpoint *scheduling* (writeback
//! cadence, the tail rule, `reusable_upto` advancement wiring) is PR K6b.
//!
//! ## Layout
//!
//! 32 round-robin 4 KiB slots (128 KiB total). A record with checkpoint
//! seq `s` lives in slot `s % 32`; mount picks the newest slot whose
//! checksum verifies. A torn newest slot (a checkpoint racing power loss)
//! therefore falls back to its predecessor — correct by the pending-free
//! rule (§4.7) and its ring twin (§4.6 pt 3): nothing either record
//! references has been overwritten.
//!
//! ## Slot format (little-endian; §4.1's field list)
//!
//! ```text
//! [0..4)   magic: u32      ROOT_LEDGER_MAGIC
//! [4..8)   len: u32        payload byte length
//! [8..16)  seq: u64        checkpoint sequence (slot = seq % 32)
//! [16..24) xxh3_64: u64    over the slot header (checksum zeroed) + payload
//! payload:
//!   journal_tail_seq: u64
//!   next_ino: u64
//!   alloc_bitmap_generation: u64
//!   n_roots: u16
//!   n_roots × { tree_id: u8, node_addr: u64, node_seq: u64 }
//! ```
//!
//! Every length is bounds-checked against its container before use (§9);
//! slots are written as full zero-padded 4 KiB images so a shorter record
//! can never leave stale bytes of a longer predecessor parseable.

use super::KvError;
use std::path::Path;

/// Number of round-robin ledger slots (§4.1).
pub const ROOT_LEDGER_SLOTS: u64 = 32;
/// Slot size in bytes.
pub const ROOT_LEDGER_SLOT_LEN: u64 = 4096;
/// Total ledger extent length (128 KiB — the §3 mount-read unit).
pub const ROOT_LEDGER_LEN: u64 = ROOT_LEDGER_SLOTS * ROOT_LEDGER_SLOT_LEN;
/// Ledger slot magic (`"KVRL"`).
pub const ROOT_LEDGER_MAGIC: u32 = 0x4B56_524C;
/// Fixed slot header length (`magic | len | seq | xxh3`).
pub const ROOT_LEDGER_HDR_LEN: usize = 24;

/// Fixed payload prefix: `journal_tail_seq | next_ino |
/// alloc_bitmap_generation | n_roots`.
const PAYLOAD_FIXED_LEN: usize = 8 + 8 + 8 + 2;
/// Encoded size of one tree root: `tree_id | node_addr | node_seq`.
const ROOT_ENC_LEN: usize = 1 + 8 + 8;

/// One tree root named by a ledger record: `(tree_id, node_addr, node_seq)`
/// (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeRoot {
    pub tree_id: u8,
    pub node_addr: u64,
    pub node_seq: u64,
}

/// A root-ledger record (§4.1): the per-tree roots plus the journal tail,
/// the monotonic ino watermark (§4.8), and the allocator-bitmap generation
/// (§4.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRecord {
    /// Checkpoint sequence; strictly monotonic per volume. Slot = `seq % 32`.
    pub seq: u64,
    /// Per-tree roots.
    pub tree_roots: Vec<TreeRoot>,
    /// Replay starts here (§4.6 pt 2 tail rule — computed by K6b).
    pub journal_tail_seq: u64,
    /// Monotonic ino allocation watermark (§4.8).
    pub next_ino: u64,
    /// Allocator bitmap generation (§4.7 — consumed by K4).
    pub alloc_bitmap_generation: u64,
}

/// xxh3 over a slot image with the checksum field (bytes 16..24) zeroed —
/// the header-then-payload discipline the superblock and bsets use (§4.3).
fn slot_checksum(image: &[u8], payload_len: usize) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&image[..16]);
    h.update(&[0u8; 8]);
    h.update(&image[ROOT_LEDGER_HDR_LEN..ROOT_LEDGER_HDR_LEN + payload_len]);
    h.digest()
}

impl LedgerRecord {
    /// The slot index this record occupies (`seq % 32`).
    pub fn slot_index(&self) -> u64 {
        self.seq % ROOT_LEDGER_SLOTS
    }

    /// Encode into a full zero-padded 4 KiB slot image. Errors when the
    /// record cannot fit a slot (structurally impossible for the ≤ 5 trees
    /// of §4.2 — defense in depth for the record count).
    pub fn encode_slot(&self) -> Result<Vec<u8>, KvError> {
        let payload_len = PAYLOAD_FIXED_LEN + self.tree_roots.len() * ROOT_ENC_LEN;
        if ROOT_LEDGER_HDR_LEN + payload_len > ROOT_LEDGER_SLOT_LEN as usize
            || self.tree_roots.len() > usize::from(u16::MAX)
        {
            return Err(KvError::Corrupt(format!(
                "ledger record with {} tree roots does not fit a {}-byte slot",
                self.tree_roots.len(),
                ROOT_LEDGER_SLOT_LEN
            )));
        }
        let mut image = vec![0u8; ROOT_LEDGER_SLOT_LEN as usize];
        image[0..4].copy_from_slice(&ROOT_LEDGER_MAGIC.to_le_bytes());
        image[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
        image[8..16].copy_from_slice(&self.seq.to_le_bytes());
        // Checksum stamped below.
        let mut pos = ROOT_LEDGER_HDR_LEN;
        image[pos..pos + 8].copy_from_slice(&self.journal_tail_seq.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.next_ino.to_le_bytes());
        pos += 8;
        image[pos..pos + 8].copy_from_slice(&self.alloc_bitmap_generation.to_le_bytes());
        pos += 8;
        image[pos..pos + 2].copy_from_slice(&(self.tree_roots.len() as u16).to_le_bytes());
        pos += 2;
        for root in &self.tree_roots {
            image[pos] = root.tree_id;
            image[pos + 1..pos + 9].copy_from_slice(&root.node_addr.to_le_bytes());
            image[pos + 9..pos + 17].copy_from_slice(&root.node_seq.to_le_bytes());
            pos += ROOT_ENC_LEN;
        }
        let sum = slot_checksum(&image, payload_len);
        image[16..24].copy_from_slice(&sum.to_le_bytes());
        Ok(image)
    }

    /// Decode + verify one slot image: magic, bounds-checked lengths (§9),
    /// checksum. Any failure means "this slot holds no valid record" —
    /// the selection logic treats it as absent, never loud.
    pub fn decode_slot(buf: &[u8]) -> Result<Self, KvError> {
        if buf.len() < ROOT_LEDGER_HDR_LEN {
            return Err(KvError::Corrupt(format!(
                "truncated ledger slot: {} of {ROOT_LEDGER_HDR_LEN} header bytes",
                buf.len()
            )));
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != ROOT_LEDGER_MAGIC {
            return Err(KvError::Corrupt(format!(
                "bad ledger slot magic {magic:#010x} (expected {ROOT_LEDGER_MAGIC:#010x})"
            )));
        }
        let payload_len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        // §9: the length is validated against the container before any
        // byte it governs is touched.
        if payload_len < PAYLOAD_FIXED_LEN || ROOT_LEDGER_HDR_LEN + payload_len > buf.len() {
            return Err(KvError::Corrupt(format!(
                "ledger payload length {payload_len} does not fit its slot"
            )));
        }
        let seq = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let stored = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let computed = slot_checksum(buf, payload_len);
        if stored != computed {
            return Err(KvError::ChecksumMismatch { stored, computed });
        }
        let payload = &buf[ROOT_LEDGER_HDR_LEN..ROOT_LEDGER_HDR_LEN + payload_len];
        let journal_tail_seq = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let next_ino = u64::from_le_bytes(payload[8..16].try_into().unwrap());
        let alloc_bitmap_generation = u64::from_le_bytes(payload[16..24].try_into().unwrap());
        let n_roots = usize::from(u16::from_le_bytes(payload[24..26].try_into().unwrap()));
        if PAYLOAD_FIXED_LEN + n_roots * ROOT_ENC_LEN != payload_len {
            return Err(KvError::Corrupt(format!(
                "ledger n_roots {n_roots} inconsistent with payload length {payload_len}"
            )));
        }
        let mut tree_roots = Vec::with_capacity(n_roots);
        let mut pos = PAYLOAD_FIXED_LEN;
        for _ in 0..n_roots {
            tree_roots.push(TreeRoot {
                tree_id: payload[pos],
                node_addr: u64::from_le_bytes(payload[pos + 1..pos + 9].try_into().unwrap()),
                node_seq: u64::from_le_bytes(payload[pos + 9..pos + 17].try_into().unwrap()),
            });
            pos += ROOT_ENC_LEN;
        }
        Ok(Self {
            seq,
            tree_roots,
            journal_tail_seq,
            next_ino,
            alloc_bitmap_generation,
        })
    }
}

/// Write `rec` to its round-robin slot at `ledger_base` in `path` via
/// `uring_fs` (one full-slot `write_at`). Durability rides the caller's
/// barrier (§4.6 pt 2: "the root record's own durability rides the next
/// barrier").
pub async fn write_ledger_slot(
    path: &Path,
    ledger_base: u64,
    rec: &LedgerRecord,
) -> Result<(), KvError> {
    let image = rec.encode_slot()?;
    let off = ledger_base + rec.slot_index() * ROOT_LEDGER_SLOT_LEN;
    crate::uring_fs::write_at(path, off, image).await?;
    Ok(())
}

/// Read all 32 slots (one 128 KiB `uring_fs::read_at`) and return the
/// newest-seq record whose checksum verifies, or `None` when no slot holds
/// a valid record (fresh volume — the all-slots-invalid *loud* policy on a
/// non-fresh volume is mount wiring, PR K6a). A torn newest slot simply
/// loses the race to its intact predecessor (§4.1). `Err` is real device
/// I/O failure only ([`KvError::Io`]) — ledger *contents* never fail loud
/// here.
pub async fn read_newest_ledger(
    path: &Path,
    ledger_base: u64,
) -> Result<Option<LedgerRecord>, KvError> {
    let got = crate::uring_fs::read_at(path, ledger_base, ROOT_LEDGER_LEN as usize).await?;
    let mut newest: Option<LedgerRecord> = None;
    for slot in 0..ROOT_LEDGER_SLOTS as usize {
        let start = slot * ROOT_LEDGER_SLOT_LEN as usize;
        if start >= got.len() {
            break; // short extent: the rest reads as absent
        }
        let end = (start + ROOT_LEDGER_SLOT_LEN as usize).min(got.len());
        if let Ok(rec) = LedgerRecord::decode_slot(&got[start..end]) {
            if newest.as_ref().is_none_or(|n| rec.seq > n.seq) {
                newest = Some(rec);
            }
        }
    }
    Ok(newest)
}
