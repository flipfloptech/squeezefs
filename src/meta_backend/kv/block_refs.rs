//! **Durable data-block reference records** — the on-disk representation
//! of block ownership (pre-RC engineering spec §6.2 **item 1**, "the
//! largest item"; ruling **D9**: built behind an incompat bit, NOT
//! stamped on existing volumes).
//!
//! ## The problem this closes
//!
//! [`crate::block_allocator::BlockAllocator`]'s refcount map and free
//! list have **no on-disk representation**: `recover_active_blocks_v3`
//! rebuilds both at mount by walking the live inode tree and re-deriving
//! from every ino's `layout` xattr — "both are mount-session RAM,
//! rebuilt at mount — pure derived state" (that file's own words). The
//! spec's verdict: *"Without durable shared ownership accounting, no
//! multi-writer data path is expressible."* Two writers each derive
//! their own private answer from the subset of the tree they walked, so
//! node A can hold a block at refcount 2 while node B reads 1 and its
//! W1 sole-owner patch rewrites, in place, a block A also references —
//! `patch_ineligible_shared` never increments, and on a passthrough
//! volume the corruption is silent.
//!
//! ## The representation: one record per reference (a backpointer)
//!
//! A *count* record would need read-modify-write at commit time — and
//! two inodes cloning the same block hold no common lock, so the RMW
//! loses updates. A *delta* record cannot express "the first reference"
//! (the §4.2 fold counts a Δ-without-base as an orphaned no-op). So the
//! durable unit is **one record per (owner, map slot) reference**, with
//! the refcount read out as the population of a key prefix:
//!
//! ```text
//! key (28 B, big-endian composites — memcmp order == logical order):
//!   [0..8)    vol_tag:     u64   durable data-volume identity ([`volume_tag`])
//!   [8..16)   block_idx:   u64   device offset / allocator chunk size
//!   [16..24)  owner_ino:   u64   the referencing inode (GLOBAL ino)
//!   [24..28)  block_index: u32   the ino's block-map index, or
//!                                [`BLOCK_INDEX_MAP_BLOB`] for the
//!                                indirect-block-map blob reference
//!
//! value (4 B):
//!   [0]       version: u8        [`BLOCK_REF_VALUE_VERSION`]
//!   [1]       flags:   u8        bit 0 = the reference is a map blob
//!   [2..4)    reserved: u16      zero; nonzero refuses loud
//! ```
//!
//! Properties this shape buys:
//!
//! * **`refcount(block) == live records under the 16-byte
//!   `(vol_tag, block_idx)` prefix`** ([`block_range`]) — an ordered
//!   range scan over one or two records, never a table walk.
//! * **No RMW, no lost updates.** Taking a reference is an idempotent
//!   `Put` at a key nobody else writes; dropping one is a `Delete`.
//!   Concurrent clones of one block touch distinct keys, so they need no
//!   mutual exclusion beyond the per-ino guard they already hold.
//! * **The delta rides the layout transaction.** A publish that gains
//!   block *b* stages one `Put`; a publish that displaces *b'* stages one
//!   `Delete` — into the **same** `KvTx` as the layout
//!   record and the inode record. One tx = one checksummed journal entry
//!   (§4.10) still holds: the accounting can never disagree with the
//!   layout that justifies it, not even across a torn write, and the
//!   write-commit-economy campaign's collapsed publish is not re-split.
//! * **O(batch), never O(file size).** The record count per publish is
//!   the number of block-map entries that changed, which is exactly what
//!   the `LayoutDelta` wire already carries.
//! * **The free list needs no separate structure.** It is the complement
//!   of the referenced set below the allocation cursor — precisely the
//!   arithmetic `recover_block`/`fsck_reconcile_accounting` already run
//!   (`free = highest − referenced`). Nothing new to keep consistent.
//! * **`begin_free` → reclaim → `finish_free` maps onto it with no
//!   intermediate durable state**: the durable effect of a terminal free
//!   IS the `Delete` that rode the publish which dropped the reference.
//!   A crash anywhere in the window therefore recovers to one of exactly
//!   two states — referenced (the block stays allocated) or unreferenced
//!   (the block is free) — so the window can neither leak the block nor
//!   double-free it. What a crash *does* lose is the device `BLKDISCARD`
//!   the reclaimer had queued: hygiene, not correctness (`trim --full`
//!   walks the whole free list precisely because the free list, not the
//!   debt tracker, is the durable truth).
//! * **The derived walk survives as the oracle.** Because there is one
//!   record per layout map entry, the durable census and the layout-walk
//!   census are the same multiset by construction — which is what makes
//!   the durable-vs-derived comparison a real test rather than a hope.
//!
//! ## What this does NOT do
//!
//! Nothing here is multi-writer. The records are per-**meta-volume**
//! (they live in the same volume as the inode whose publish stages them,
//! which is what lets them ride that tx), so a set's total refcount for
//! a block is the sum over mounted meta volumes — additive by
//! construction, and therefore already the right shape for a second
//! writer, but no cross-node protocol exists here. Per-writer key
//! scoping (spec §6.2 item 8) is deliberately absent: it belongs with
//! the `active_block:` writer-id work, and this key layout leaves room
//! for it (a writer-id component appends after `block_index` without
//! disturbing the `(vol_tag, block_idx)` prefix the refcount read uses).

use super::KvError;

/// Key length: `vol_tag | block_idx | owner_ino | block_index`.
pub const BLOCK_REF_KEY_LEN: usize = 8 + 8 + 8 + 4;

// ---------------------------------------------------------------------------
// The authority-side key resolver (DLM S11 rung 19 — the width-N refs
// composition).
// ---------------------------------------------------------------------------

/// Resolve a block KEY STRING to its durable reference — the data
/// router's `block_ref_for` behind a process-global hook, installed at
/// multi-writer arm (`multi_writer::arm_multi_writer`, beside the range
/// geometry source). The authority-COMPOSED layout commits (the
/// chain-onto-head merge, the custody-scoped Put) recompute their
/// accounting from the composition itself, and this is how the meta
/// backend — which owns no data-plane router — names the displaced and
/// inserted keys' `(vol_tag, block_idx)`. `None` from the resolver means
/// the key is not allocator-tracked (the same class the mount-time walk
/// skips); the caller counts it in `META_KV_BLOCK_REFS_UNRESOLVED`,
/// mirroring the router's own discipline.
pub type BlockRefResolverFn =
    std::sync::Arc<dyn Fn(&str, u64, u32) -> Option<BlockRef> + Send + Sync>;

static BLOCK_REF_RESOLVER: once_cell::sync::Lazy<arc_swap::ArcSwapOption<BlockRefResolverFn>> =
    once_cell::sync::Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the resolver (multi-writer arm / test fixture).
pub fn install_block_ref_resolver(resolver: BlockRefResolverFn) {
    BLOCK_REF_RESOLVER.store(Some(std::sync::Arc::new(resolver)));
}

/// Uninstall it (disarm / test teardown).
pub fn uninstall_block_ref_resolver() {
    BLOCK_REF_RESOLVER.store(None);
}

/// The installed resolver, if any — one relaxed load on every un-armed
/// mount.
pub fn block_ref_resolver() -> Option<BlockRefResolverFn> {
    BLOCK_REF_RESOLVER.load_full().map(|a| (*a).clone())
}

/// Length of the `(vol_tag, block_idx)` refcount prefix — the range a
/// refcount read scans ([`block_range`]).
pub const BLOCK_REF_PREFIX_LEN: usize = 16;

/// Value length: `version | flags | reserved`.
pub const BLOCK_REF_VALUE_LEN: usize = 4;

/// Current value version. A record carrying anything else refuses loud
/// (forward-only, the house directive): an unknown version means a newer
/// binary wrote accounting this one cannot interpret, and guessing would
/// silently mis-count shared ownership.
pub const BLOCK_REF_VALUE_VERSION: u8 = 1;

/// `flags` bit 0: this reference is the ino's **indirect block-map blob**
/// (a data block holding the map itself), not a map entry. Paired with
/// [`BLOCK_INDEX_MAP_BLOB`] in the key.
pub const BLOCK_REF_FLAG_MAP_BLOB: u8 = 1 << 0;

/// The `block_index` sentinel for the indirect-map-blob reference. `u32`
/// block indices address `2^32 × block_size` bytes (16 EiB at the
/// shipped 4 MiB block), so the top value is unreachable as a real map
/// index — the same reasoning `LayoutMetadata`'s `HashMap<u32, String>`
/// already relies on.
pub const BLOCK_INDEX_MAP_BLOB: u32 = u32::MAX;

/// The durable identity of a data volume, as 8 key bytes.
///
/// PR VL3 mints data-volume ids as `vol-{16 hex}` (KD-5: random, never
/// reused), so that form decodes to its own `u64` verbatim — the tag is
/// then literally the durable id, with no hash and no collision
/// argument to make. Grandfathered legacy ids (device basenames, byte-
/// identical per KD-5) hash with xxh3-64.
///
/// Stability is the whole contract: the tag is a **key component**, so
/// it must be identical on every mount of the same volume forever. It
/// derives only from the durable id string — never from a path, a
/// mount-order ordinal, or a set position (all three change).
pub fn volume_tag(volume_id: &str) -> u64 {
    if let Some(hex) = volume_id.strip_prefix("vol-") {
        if hex.len() == 16 {
            if let Ok(v) = u64::from_str_radix(hex, 16) {
                return v;
            }
        }
    }
    xxhash_rust::xxh3::xxh3_64(volume_id.as_bytes())
}

/// One durable block reference, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockRef {
    pub vol_tag: u64,
    pub block_idx: u64,
    pub owner_ino: u64,
    /// The owner's block-map index, or [`BLOCK_INDEX_MAP_BLOB`].
    pub block_index: u32,
}

impl BlockRef {
    /// `true` ⇔ this reference is the ino's indirect map blob.
    pub fn is_map_blob(&self) -> bool {
        self.block_index == BLOCK_INDEX_MAP_BLOB
    }

    /// The record key for this reference.
    pub fn key(&self) -> [u8; BLOCK_REF_KEY_LEN] {
        block_ref_key(
            self.vol_tag,
            self.block_idx,
            self.owner_ino,
            self.block_index,
        )
    }

    /// The record value for this reference.
    pub fn value(&self) -> [u8; BLOCK_REF_VALUE_LEN] {
        let flags = if self.is_map_blob() {
            BLOCK_REF_FLAG_MAP_BLOB
        } else {
            0
        };
        [BLOCK_REF_VALUE_VERSION, flags, 0, 0]
    }
}

/// One staged accounting operation: take (`Put`) or drop (`Delete`) a
/// reference. Built by the data router at the sites that mutate a block
/// map, drained into the layout transaction by the meta backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRefOp {
    pub reference: BlockRef,
    /// `true` = the reference is taken (`Put`); `false` = dropped
    /// (`Delete`).
    pub take: bool,
}

impl BlockRefOp {
    /// The reference is taken: one `Put`.
    pub fn taken(reference: BlockRef) -> Self {
        Self {
            reference,
            take: true,
        }
    }

    /// The reference is dropped: one `Delete`.
    pub fn released(reference: BlockRef) -> Self {
        Self {
            reference,
            take: false,
        }
    }
}

/// What a standalone reference RELEASE commit (the reclaim path's
/// `delete_file`, the mount-time corpse sweep) can say about the
/// references it dropped — the input to the RAM-refcount gate that closed
/// the generic/749 double release (2026-09-11).
///
/// The law: on a ledger-bearing volume a release may decrement the RAM
/// refcount ONLY if THIS owner's durable record existed at release time.
/// The ledger is the truth for "does this ino hold a reference to this
/// block"; a `Delete` of an absent key is a no-op in the tree (§4.10's
/// idempotent-replay posture) and must be a no-op in RAM too — the RAM
/// count was seeded from the records that exist, so a decrement with no
/// record behind it lands on whichever LIVE owner holds the offset now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseWitness {
    /// No ledger on the ino's home volume (incompat bit 9 absent): the
    /// derived posture — the layout walk seeded every reference every
    /// surviving layout names (corpses included), so every release the
    /// layout justifies stands.
    Derived,
    /// The ino's home volume is a peer's: the release shipped as a verb,
    /// and the FREE ships too — the authority's executor validates it
    /// against ITS ledger (`cowriter::execute_shipped_frees`' population
    /// shield); nothing is gated here.
    Shipped,
    /// The ledger is engaged: the releases whose record EXISTED at commit
    /// time (deduped by reference). Every other release named an absent
    /// record and justifies no RAM decrement.
    Ledger(Vec<BlockRef>),
}

/// **The caller frame's RAM-only lifetimes** (finding 15's supply leak,
/// `.benchmarks/2026-09-06-cowriter-free-refcount-leak.md`): the DATA
/// blocks a publish frame both TAKES and RELEASES — a binding minted,
/// bound in RAM, displaced by a later durable merge and never persisted in
/// between (an epoch-fed overlay dest superseded by a write-through, a
/// same-epoch re-record's parked prior key). An owner-side recompute
/// replaces the frame with the head→composed diff, and that diff cannot
/// name a block NEITHER map ever held — so without this walk the frame
/// stands down (`recomputed`), the recompute frees only the head's
/// binding, and the RAM-only block is freed by nobody: not referenced, on
/// no free list, gone from the lane's recycle supply until an ownership
/// recovery walk (which a co-writer never runs).
///
/// Returns the frame's net-zero blocks (≥ 1 take, takes == releases, in
/// first-appearance order, deduped) that the recomputed op set does not
/// already name — the recompute's own release of a head-named block is
/// its business, and a block it TAKES is live. Map-blob custody is
/// excluded (the displaced-blob post-commit arm owns it). The caller still
/// filters out anything the head or the composed map names (a skewed frame
/// can re-take a durable block — see `recompute_refs_against_map`).
pub fn frame_ram_only_candidates(
    caller: &[BlockRefOp],
    recomputed: &[BlockRefOp],
) -> Vec<BlockRef> {
    let mut order: Vec<(u64, u64)> = Vec::new();
    let mut tally: std::collections::HashMap<(u64, u64), (u32, u32, BlockRef)> =
        std::collections::HashMap::new();
    for op in caller {
        if op.reference.is_map_blob() {
            continue;
        }
        let id = (op.reference.vol_tag, op.reference.block_idx);
        let slot = tally.entry(id).or_insert_with(|| {
            order.push(id);
            (0, 0, op.reference)
        });
        if op.take {
            slot.0 += 1;
        } else {
            slot.1 += 1;
        }
    }
    if order.is_empty() {
        return Vec::new();
    }
    let named: std::collections::HashSet<(u64, u64)> = recomputed
        .iter()
        .map(|o| (o.reference.vol_tag, o.reference.block_idx))
        .collect();
    order
        .into_iter()
        .filter_map(|id| {
            let (takes, releases, reference) = tally[&id];
            (takes >= 1 && takes == releases && !named.contains(&id)).then_some(reference)
        })
        .collect()
}

/// Build a reference key (big-endian composite, §4.2 key discipline).
pub fn block_ref_key(
    vol_tag: u64,
    block_idx: u64,
    owner_ino: u64,
    block_index: u32,
) -> [u8; BLOCK_REF_KEY_LEN] {
    let mut key = [0u8; BLOCK_REF_KEY_LEN];
    key[0..8].copy_from_slice(&vol_tag.to_be_bytes());
    key[8..16].copy_from_slice(&block_idx.to_be_bytes());
    key[16..24].copy_from_slice(&owner_ino.to_be_bytes());
    key[24..28].copy_from_slice(&block_index.to_be_bytes());
    key
}

/// Decode a reference key; length-checked (§9 bounds rule).
pub fn decode_block_ref_key(key: &[u8]) -> Result<BlockRef, KvError> {
    if key.len() != BLOCK_REF_KEY_LEN {
        return Err(KvError::Corrupt(format!(
            "block-ref key must be {BLOCK_REF_KEY_LEN} bytes, got {}",
            key.len()
        )));
    }
    Ok(BlockRef {
        vol_tag: u64::from_be_bytes(key[0..8].try_into().unwrap()),
        block_idx: u64::from_be_bytes(key[8..16].try_into().unwrap()),
        owner_ino: u64::from_be_bytes(key[16..24].try_into().unwrap()),
        block_index: u32::from_be_bytes(key[24..28].try_into().unwrap()),
    })
}

/// Decoded value fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRefValue {
    pub map_blob: bool,
}

/// Decode + validate a reference value: version-gated, reserved-zero
/// checked, length-checked (§9). Every failure is loud corruption —
/// silently accepting an unreadable accounting record would under-count
/// shared ownership, which is the exact failure mode this whole
/// structure exists to prevent.
pub fn decode_block_ref_value(value: &[u8]) -> Result<BlockRefValue, KvError> {
    if value.len() != BLOCK_REF_VALUE_LEN {
        return Err(KvError::Corrupt(format!(
            "block-ref value must be {BLOCK_REF_VALUE_LEN} bytes, got {}",
            value.len()
        )));
    }
    if value[0] != BLOCK_REF_VALUE_VERSION {
        return Err(KvError::Corrupt(format!(
            "block-ref value version {} is not {BLOCK_REF_VALUE_VERSION} (a newer \
             binary's accounting — refusing to guess shared ownership)",
            value[0]
        )));
    }
    if value[1] & !BLOCK_REF_FLAG_MAP_BLOB != 0 {
        return Err(KvError::Corrupt(format!(
            "block-ref value carries unknown flags {:#04x}",
            value[1]
        )));
    }
    if u16::from_le_bytes(value[2..4].try_into().unwrap()) != 0 {
        return Err(KvError::Corrupt(
            "block-ref value reserved field is nonzero".to_string(),
        ));
    }
    Ok(BlockRefValue {
        map_blob: value[1] & BLOCK_REF_FLAG_MAP_BLOB != 0,
    })
}

/// Inclusive-start / exclusive-end key range covering **every**
/// reference on one data volume — the mount-recovery scan unit.
pub fn volume_range(vol_tag: u64) -> (Vec<u8>, Vec<u8>) {
    let lo = block_ref_key(vol_tag, 0, 0, 0).to_vec();
    let hi = match vol_tag.checked_add(1) {
        Some(next) => block_ref_key(next, 0, 0, 0).to_vec(),
        // The last tag has no successor prefix: a key one byte LONGER
        // than any real key, all-`0xFF`, sorts strictly after every
        // 28-byte key (memcmp: a prefix precedes its extensions).
        None => vec![0xFF; BLOCK_REF_KEY_LEN + 1],
    };
    (lo, hi)
}

/// Inclusive-start / exclusive-end key range covering every reference to
/// **one block** — the refcount read (`refcount == records in range`).
pub fn block_range(vol_tag: u64, block_idx: u64) -> (Vec<u8>, Vec<u8>) {
    let lo = block_ref_key(vol_tag, block_idx, 0, 0).to_vec();
    let hi = match block_idx.checked_add(1) {
        Some(next) => block_ref_key(vol_tag, next, 0, 0).to_vec(),
        None => {
            let mut top = block_ref_key(vol_tag, block_idx, u64::MAX, u32::MAX).to_vec();
            // One past the largest key with this prefix.
            top.push(0);
            top
        }
    };
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The durable `vol-{16 hex}` id decodes to itself — no hash, so the
    /// key component IS the durable identity (KD-5).
    #[test]
    fn vol_hex_ids_decode_to_themselves() {
        assert_eq!(volume_tag("vol-0000000000000001"), 1);
        assert_eq!(volume_tag("vol-ffffffffffffffff"), u64::MAX);
        // Legacy/grandfathered ids hash, stably.
        assert_eq!(volume_tag("nvme0n1"), volume_tag("nvme0n1"));
        assert_ne!(volume_tag("nvme0n1"), volume_tag("nvme0n2"));
        // A malformed vol- id must not silently alias a valid one.
        assert_ne!(volume_tag("vol-zzzz"), 0);
    }

    /// Keys are memcmp-ordered in every dimension, most significant
    /// first — what makes the prefix scans exact.
    #[test]
    fn keys_are_memcmp_ordered_by_dimension() {
        let a = block_ref_key(1, 1, 1, 1);
        assert!(block_ref_key(0, u64::MAX, u64::MAX, u32::MAX) < a);
        assert!(block_ref_key(1, 0, u64::MAX, u32::MAX) < a);
        assert!(block_ref_key(1, 1, 0, u32::MAX) < a);
        assert!(block_ref_key(1, 1, 1, 0) < a);
        assert!(a < block_ref_key(1, 1, 1, 2));
    }

    #[test]
    fn key_roundtrip_and_length_refusal() {
        let r = BlockRef {
            vol_tag: 0xDEAD_BEEF_CAFE_F00D,
            block_idx: 42,
            owner_ino: 7,
            block_index: 3,
        };
        assert_eq!(decode_block_ref_key(&r.key()).unwrap(), r);
        assert!(decode_block_ref_key(&r.key()[..27]).is_err());
        assert!(decode_block_ref_key(&[]).is_err());
    }

    #[test]
    fn value_roundtrip_and_refusals() {
        let plain = BlockRef {
            vol_tag: 1,
            block_idx: 2,
            owner_ino: 3,
            block_index: 4,
        };
        assert!(!decode_block_ref_value(&plain.value()).unwrap().map_blob);
        let blob = BlockRef {
            block_index: BLOCK_INDEX_MAP_BLOB,
            ..plain
        };
        assert!(decode_block_ref_value(&blob.value()).unwrap().map_blob);
        assert!(blob.is_map_blob() && !plain.is_map_blob());

        assert!(decode_block_ref_value(&[]).is_err(), "length");
        assert!(decode_block_ref_value(&[2, 0, 0, 0]).is_err(), "version");
        assert!(decode_block_ref_value(&[1, 0x80, 0, 0]).is_err(), "flags");
        assert!(decode_block_ref_value(&[1, 0, 1, 0]).is_err(), "reserved");
    }

    /// The refcount read's range must contain exactly the block's
    /// references — every owner/index, and nothing from the neighbouring
    /// blocks or volumes.
    #[test]
    fn block_range_bounds_exactly_one_block() {
        let (lo, hi) = block_range(5, 9);
        let inside = |k: [u8; BLOCK_REF_KEY_LEN]| k[..] >= lo[..] && k[..] < hi[..];
        assert!(inside(block_ref_key(5, 9, 0, 0)));
        assert!(inside(block_ref_key(5, 9, u64::MAX, u32::MAX)));
        assert!(!inside(block_ref_key(5, 8, u64::MAX, u32::MAX)));
        assert!(!inside(block_ref_key(5, 10, 0, 0)));
        assert!(!inside(block_ref_key(4, 9, 0, 0)));
        assert!(!inside(block_ref_key(6, 9, 0, 0)));
    }

    /// The volume scan's range covers every block of one volume and
    /// nothing of its neighbours — including at the `u64::MAX` edge,
    /// where the naive `tag + 1` end bound overflows.
    #[test]
    fn volume_range_bounds_exactly_one_volume() {
        let (lo, hi) = volume_range(5);
        let inside = |k: [u8; BLOCK_REF_KEY_LEN]| k[..] >= lo[..] && k[..] < hi[..];
        assert!(inside(block_ref_key(5, 0, 0, 0)));
        assert!(inside(block_ref_key(5, u64::MAX, u64::MAX, u32::MAX)));
        assert!(!inside(block_ref_key(4, u64::MAX, u64::MAX, u32::MAX)));
        assert!(!inside(block_ref_key(6, 0, 0, 0)));

        // The `u64::MAX` edge: the naive `tag + 1` end bound overflows,
        // and an all-`0xFF` 28-byte bound would EXCLUDE the volume's own
        // largest key.
        let (lo, hi) = volume_range(u64::MAX);
        let top = block_ref_key(u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        assert!(block_ref_key(u64::MAX, 0, 0, 0)[..] >= lo[..]);
        assert!(
            top[..] < hi[..],
            "the last volume's largest key must fall inside its own scan range"
        );
    }
}
