//! **The shared-block index, the on-demand refcount probe and the clone
//! protocol's durable steps** (docs/design-symmetric-metadata.md §5.4.3 /
//! §5.4.4 / §5.8.5 C16; PR 7; KD-SYM-8).
//!
//! Under the slot-tree forest every block reference lives in its OWNER
//! ino's slot tree (the routed layer keys a forest volume's records by
//! the owner's LOCAL KEY ino — [`crate::meta_backend::RoutedMetaBackend`]'s
//! `forest_ref_ops`), and the packer packs per `(writer, slot, data
//! volume)`, so `refcount(b)` for a block nobody cloned is ONE range probe
//! in one slot tree ([`crate::meta_backend::kv::backend::KvMetaBackend::block_ref_probe`]). A block
//! referenced from TWO slots arises only through clone/reflink and goes
//! through the **shared-block index**:
//!
//! ```text
//! tree 0 of the index HOME volume ([`index_home_volume`](crate::meta_backend::kv::shared_refs::index_home_volume)):
//!   key   b"shared_ref:" ‖ vol_tag ‖ block_idx ‖ owner_ino ‖ block_index   (the 28 B ref key)
//!   value version: u8 ‖ reserved × 3
//! ```
//!
//! **The home** is resolved behind ONE function ([`index_home_volume`](crate::meta_backend::kv::shared_refs::index_home_volume)):
//! in PR 7 it is volume 0's tree 0 — the manager's control plane (the
//! records ride `write_control_entry`, the manager's one control-entry
//! writer, in ring 0's USER class). PR 8 re-points it to the data
//! volume's allocation-lease holder's control ino (the design's kind
//! `0x09` in that holder's slot tree, `TREE_SHARED_INDEX` — reserved,
//! unwritten here); the record shape is the same 28-byte reference under
//! a different prefix, so the move is a re-key, never a re-derivation.
//!
//! **The protocol** (§5.4.4): `MarkShared { F, b }` sets the SHARED bit on
//! F's reference at F's holder under F's 4a guard (an own-ring tx —
//! [`crate::meta_backend::kv::backend::KvMetaBackend::mark_block_ref_shared`]; absent ⇒ `Gone`, the
//! cloner aborts); `ShareBlock { b, source, target }` writes BOTH inos'
//! index entries at the home (idempotent — [`crate::meta_backend::kv::backend::KvMetaBackend::share_block`]);
//! the cloner publishes its layout with its own reference SHARED
//! (`BlockRefOp::taken_shared`, one tx — a clone of a clone inherits the
//! bit); a terminal free of a SHARED block is never decided locally:
//! `ReleaseShared { b, ino }` at the home deletes the releaser's entry and
//! answers from what remains ([`crate::meta_backend::kv::backend::KvMetaBackend::release_shared`]),
//! GC'ing an entry whose ino no longer holds a reference (the "C dies
//! after step 2" window). **Ordering law**: a reference is published only
//! after its SHARED mark is durable at the owner — step 1 acks after its
//! durability lane, step 3 runs after the ack.
//!
//! **C16** (§5.8.5, report-only): a SHARED-flagged reference without an
//! index entry — on the source OR the target ino — or an index entry
//! whose ino holds no SHARED reference ([`crate::meta_backend::kv::backend::KvMetaBackend::shared_index_scan`]
//! is fsck's input; `fsck_shared_index_drift` the gauge).

use super::super::block_refs::{
    block_range, decode_block_ref_key, decode_block_ref_value, volume_range, BlockRef, BlockRefOp,
    BLOCK_REF_KEY_LEN,
};
use super::super::forest::SlotTrees;
use super::super::record::{ForestSlot, Record, TREE_BLOCK_REFS, TREE_CONTROL};
use super::super::KvError;
use super::{EntryAdmission, KvMetaBackend, KvTx};
use crate::error::Result;
use crate::meta_backend::dlm::DlmGuard;
use crate::meta_backend::kv::node::key_successor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Gauges (§11 — the allocation-lease family's PR-7 members; 0 on every
// mount without an armed forest by construction).
// ---------------------------------------------------------------------------

/// `block_ref_probes`: on-demand refcount probes served (one range probe
/// in one slot tree, or the shared index).
pub static BLOCK_REF_PROBES: AtomicU64 = AtomicU64::new(0);
/// `share_block_calls`: `ShareBlock` executions at the index home.
pub static SHARE_BLOCK_CALLS: AtomicU64 = AtomicU64::new(0);
/// `release_shared_calls`: `ReleaseShared` executions at the index home.
pub static RELEASE_SHARED_CALLS: AtomicU64 = AtomicU64::new(0);
/// `mark_shared_calls`: `MarkShared` executions at a source's holder.
pub static MARK_SHARED_CALLS: AtomicU64 = AtomicU64::new(0);

/// The index HOME's volume ordinal in the routed set — **the ONE function
/// PR 8 re-points** to the data volume's allocation-lease holder. PR 7:
/// volume 0, whose manager holds the set-wide roles (KD-SYM-2).
pub const fn index_home_volume() -> usize {
    0
}

/// Key prefix of every shared-index record in the home's tree 0.
pub const SHARED_REF_KEY_PREFIX: &[u8] = b"shared_ref:";
/// `prefix ‖ the 28-byte block-reference key`.
pub const SHARED_REF_KEY_LEN: usize = SHARED_REF_KEY_PREFIX.len() + BLOCK_REF_KEY_LEN;
/// Record value version (byte 0); a future version refuses loud.
pub const SHARED_REF_VALUE_VERSION: u8 = 1;
/// `version ‖ reserved × 3` — the block-ref value's width, no flags.
pub const SHARED_REF_VALUE_LEN: usize = 4;

/// The tree-0 key of one index entry.
pub fn shared_ref_key(r: &BlockRef) -> Vec<u8> {
    let mut k = Vec::with_capacity(SHARED_REF_KEY_LEN);
    k.extend_from_slice(SHARED_REF_KEY_PREFIX);
    k.extend_from_slice(&r.key());
    k
}

/// The value of one index entry.
pub fn shared_ref_value() -> [u8; SHARED_REF_VALUE_LEN] {
    [SHARED_REF_VALUE_VERSION, 0, 0, 0]
}

/// Decode an index key back to its reference; wrong prefix or length is
/// corruption (the record class is fsck C16's).
pub fn decode_shared_ref_key(key: &[u8]) -> std::result::Result<BlockRef, KvError> {
    if key.len() != SHARED_REF_KEY_LEN || !key.starts_with(SHARED_REF_KEY_PREFIX) {
        return Err(KvError::Corrupt(format!(
            "shared_ref key must be {SHARED_REF_KEY_LEN} bytes under the {:?} prefix, got {} \
             bytes",
            String::from_utf8_lossy(SHARED_REF_KEY_PREFIX),
            key.len()
        )));
    }
    decode_block_ref_key(&key[SHARED_REF_KEY_PREFIX.len()..])
}

/// Decode + validate an index value.
pub fn decode_shared_ref_value(value: &[u8]) -> std::result::Result<(), KvError> {
    if value.len() != SHARED_REF_VALUE_LEN {
        return Err(KvError::Corrupt(format!(
            "shared_ref value must be {SHARED_REF_VALUE_LEN} bytes, got {}",
            value.len()
        )));
    }
    if value[0] != SHARED_REF_VALUE_VERSION {
        return Err(KvError::Corrupt(format!(
            "shared_ref value version {} — this binary writes {SHARED_REF_VALUE_VERSION} and \
             the format is forward-only",
            value[0]
        )));
    }
    if value[1..] != [0, 0, 0] {
        return Err(KvError::Corrupt(
            "shared_ref value reserved bytes are nonzero".to_string(),
        ));
    }
    Ok(())
}

/// Inclusive `[start, end]` tree-0 bounds over every index entry of ONE
/// block.
pub fn shared_ref_block_range(vol_tag: u64, block_idx: u64) -> (Vec<u8>, Vec<u8>) {
    let (lo, hi) = block_range(vol_tag, block_idx);
    (prefixed(&lo), prefixed(&hi))
}

/// Inclusive `[start, end]` tree-0 bounds over every index entry of ONE
/// data volume.
pub fn shared_ref_volume_range(vol_tag: u64) -> (Vec<u8>, Vec<u8>) {
    let (lo, hi) = volume_range(vol_tag);
    (prefixed(&lo), prefixed(&hi))
}

fn prefixed(bound: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(SHARED_REF_KEY_PREFIX.len() + bound.len());
    k.extend_from_slice(SHARED_REF_KEY_PREFIX);
    k.extend_from_slice(bound);
    k
}

/// `MarkShared`'s outcome at the source's holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkOutcome {
    /// The bit was set by this call (one own-ring tx, acked after its
    /// durability lane).
    Marked,
    /// The record already carried the bit (a replay, or a clone of a
    /// clone) — nothing written.
    Already,
    /// No reference record exists for `(b, owner, block_index)`: the block
    /// may be freed — the cloner aborts (ENOENT-class).
    Gone,
}

/// `ReleaseShared`'s verdict at the index home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedRelease {
    /// The index never named the block: the caller's ordinary local
    /// verdict stands (the RAM refcount's).
    NotShared,
    /// Entries remain after the release (and the GC pass): the block is
    /// still shared — the free is NOT terminal.
    Held { remaining: usize },
    /// The release emptied the index for the block: the caller runs the
    /// terminal free.
    Freed,
}

/// One C16 finding (report-only; `fsck_shared_index_drift`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedIndexDrift {
    /// A reference record carries SHARED but the index has no entry for
    /// it (the source's or the target's).
    FlagWithoutEntry(BlockRef),
    /// An index entry whose ino holds no SHARED reference to the block.
    EntryWithoutFlag(BlockRef),
}

impl KvMetaBackend {
    /// **The on-demand refcount probe** (§5.4.3 law 2): the durable
    /// population of `(vol_tag, block_idx)` in ONE slot tree — the
    /// referencing ino's, which by the pack law holds every reference of
    /// a block nobody cloned — or, `slot == None` / a flat volume, the
    /// whole-volume count ([`Self::block_ref_count`]). `Ok(0)` when the
    /// slot has no tree or the ledger is not engaged. Counted on
    /// `block_ref_probes`.
    pub async fn block_ref_probe(
        &self,
        vol_tag: u64,
        block_idx: u64,
        slot: Option<ForestSlot>,
    ) -> std::result::Result<usize, KvError> {
        BLOCK_REF_PROBES.fetch_add(1, Ordering::Relaxed);
        if !self.block_refs_engaged() {
            return Ok(0);
        }
        let (Some(forest), Some(slot)) = (self.forest(), slot) else {
            return self.block_ref_count(vol_tag, block_idx).await;
        };
        let (start, end) = block_range(vol_tag, block_idx);
        Ok(forest.refs_probe(slot, &start, &end).await?.len())
    }

    /// Every SHARED-flagged reference of one data volume on THIS volume
    /// (fsck C16's flag side) — `(reference, slot)` pairs.
    pub async fn shared_flagged_refs(
        &self,
        vol_tag: u64,
    ) -> std::result::Result<Vec<BlockRef>, KvError> {
        if !self.block_refs_engaged() {
            return Ok(Vec::new());
        }
        let (start, end) = volume_range(vol_tag);
        let mut out = Vec::new();
        for (k, v) in self.block_refs_window(&start, &end).await? {
            let r = decode_block_ref_key(&k)?;
            if decode_block_ref_value(&v)?.shared {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Does `owner`'s reference to `(vol_tag, block_idx, block_index)`
    /// exist, and does it carry SHARED? `None` = no record.
    pub async fn block_ref_flags(
        &self,
        reference: &BlockRef,
    ) -> std::result::Result<Option<bool>, KvError> {
        if !self.block_refs_engaged() {
            return Ok(None);
        }
        match self.lookup_kind(TREE_BLOCK_REFS, &reference.key()).await? {
            Some(v) => Ok(Some(decode_block_ref_value(&v)?.shared)),
            None => Ok(None),
        }
    }

    /// **`MarkShared { F, b }` at F's holder** (§5.4.4 step 1): under F's
    /// 4a exclusive guard, set the SHARED bit on F's reference record for
    /// `b` — one own-ring tx, committed through the conveyor (the ack
    /// follows the durability lane) — or answer `Gone` when no record
    /// exists (the block may be freed; the cloner aborts). Idempotent: a
    /// record already carrying the bit is `Already`, nothing written.
    /// `owner` is the reference's key form on THIS volume (the local key
    /// ino on a forest — the routed layer's translation).
    pub async fn mark_block_ref_shared(&self, reference: &BlockRef) -> Result<MarkOutcome> {
        Ok(self
            .mark_block_refs_shared(std::slice::from_ref(reference))
            .await?
            .pop()
            .unwrap_or(MarkOutcome::Gone))
    }

    /// [`Self::mark_block_ref_shared`] over every reference of ONE owner
    /// (a clone's whole source map): one 4a guard, one tx — the bits a
    /// record already carries and the records that do not exist stage
    /// nothing; the per-reference outcomes come back in order.
    pub async fn mark_block_refs_shared(&self, refs: &[BlockRef]) -> Result<Vec<MarkOutcome>> {
        MARK_SHARED_CALLS.fetch_add(1, Ordering::Relaxed);
        let Some(first) = refs.first() else {
            return Ok(Vec::new());
        };
        if !self.block_refs_engaged() {
            return Ok(vec![MarkOutcome::Gone; refs.len()]);
        }
        if refs.iter().any(|r| r.owner_ino != first.owner_ino) {
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "MarkShared: one call marks one owner's references".to_string(),
            ));
        }
        self.write_gate()?;
        // F's 4a guard: the same lock its layout commits and its W1
        // predicate's caller hold, so the mark is ordered against both
        // (the design's "in ONE process, the fence pair composes").
        let guards: Arc<[DlmGuard]> =
            Arc::from(vec![self.dlm.lock_inode_exclusive(first.owner_ino).await]);
        let mut outcomes = Vec::with_capacity(refs.len());
        let mut ops = Vec::new();
        for r in refs {
            match self.lookup_kind(TREE_BLOCK_REFS, &r.key()).await? {
                None => outcomes.push(MarkOutcome::Gone),
                Some(v) if decode_block_ref_value(&v)?.shared => {
                    outcomes.push(MarkOutcome::Already);
                }
                Some(_) => {
                    ops.push(BlockRefOp::taken_shared(*r));
                    outcomes.push(MarkOutcome::Marked);
                }
            }
        }
        if !ops.is_empty() {
            let mut tx = KvTx::new();
            tx.stage_block_refs(&ops);
            tx.hold_guards(guards);
            self.commit_tx(tx).await?;
        }
        Ok(outcomes)
    }

    /// **`ShareBlock { b, inos }` at the index home** (§5.4.4 step 2): one
    /// index entry per `(b, ino, block_index)` named — the SOURCE's and
    /// the TARGET's, so C16 can check both sides — written as ONE control
    /// entry in the manager's ring; entries already present are skipped
    /// (idempotent on replay). Returns `(inserted, already)`.
    pub async fn share_block(
        &self,
        refs: &[BlockRef],
    ) -> std::result::Result<(usize, usize), KvError> {
        SHARE_BLOCK_CALLS.fetch_add(1, Ordering::Relaxed);
        let _set = self.manager_gate(false)?;
        let _g = self.manager_verbs.lock().await;
        let control = self.control_tree()?;
        let mut recs = Vec::with_capacity(refs.len());
        let mut already = 0usize;
        for r in refs {
            let key = shared_ref_key(r);
            if control.lookup(&key).await?.is_some() {
                already += 1;
                continue;
            }
            recs.push((
                super::super::journal::tag_for(TREE_CONTROL, 0),
                Record::put(key, 0, shared_ref_value().to_vec()),
            ));
        }
        let inserted = recs.len();
        if inserted > 0 {
            self.write_control_entry(recs, EntryAdmission::Try).await?;
        }
        Ok((inserted, already))
    }

    /// The index's entries for one block at the home: the referencing
    /// `(owner_ino, block_index)` pairs.
    pub async fn shared_index_population(
        &self,
        vol_tag: u64,
        block_idx: u64,
    ) -> std::result::Result<Vec<BlockRef>, KvError> {
        BLOCK_REF_PROBES.fetch_add(1, Ordering::Relaxed);
        let Some(forest) = self.forest() else {
            return Ok(Vec::new());
        };
        let (start, end) = shared_ref_block_range(vol_tag, block_idx);
        Self::scan_index(forest, &start, &end).await
    }

    /// Every index entry of one data volume at the home (fsck C16's index
    /// side).
    pub async fn shared_index_scan(
        &self,
        vol_tag: u64,
    ) -> std::result::Result<Vec<BlockRef>, KvError> {
        let Some(forest) = self.forest() else {
            return Ok(Vec::new());
        };
        let (start, end) = shared_ref_volume_range(vol_tag);
        Self::scan_index(forest, &start, &end).await
    }

    async fn scan_index(
        forest: &SlotTrees,
        start: &[u8],
        end: &[u8],
    ) -> std::result::Result<Vec<BlockRef>, KvError> {
        let control = forest.control();
        let mut out = Vec::new();
        let mut cursor = start.to_vec();
        loop {
            let page = control.range(&cursor, end, 512).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = key_successor(last);
            for (k, v) in &page {
                decode_shared_ref_value(v)?;
                out.push(decode_shared_ref_key(k)?);
            }
            if page.len() < 512 {
                break;
            }
        }
        Ok(out)
    }

    /// **`ReleaseShared { b, ino }` at the index home** (§5.4.4 step 4):
    /// delete `ino`'s entries for `b` (every `block_index`), then decide
    /// from what remains — `still_referenced` answers, per remaining
    /// entry, whether its ino still holds a reference to `b` (the GC arm
    /// for the "cloner died after step 2" window: an entry whose
    /// reference is gone is deleted too, never counted). ONE control entry
    /// carries every delete. `NotShared` when the index never named the
    /// block; `Freed` when nothing remains; `Held` otherwise. Idempotent:
    /// a replay finds the releaser's entries gone and re-decides.
    pub async fn release_shared<F, Fut>(
        &self,
        vol_tag: u64,
        block_idx: u64,
        releaser: Option<u64>,
        still_referenced: F,
    ) -> std::result::Result<SharedRelease, KvError>
    where
        F: Fn(BlockRef) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<bool, KvError>>,
    {
        RELEASE_SHARED_CALLS.fetch_add(1, Ordering::Relaxed);
        let _set = self.manager_gate(false)?;
        let _g = self.manager_verbs.lock().await;
        let population = self.shared_index_population(vol_tag, block_idx).await?;
        if population.is_empty() {
            return Ok(SharedRelease::NotShared);
        }
        let mut deletes = Vec::new();
        let mut remaining = 0usize;
        for r in population {
            let mine = releaser == Some(r.owner_ino);
            if mine || !still_referenced(r).await? {
                deletes.push((
                    super::super::journal::tag_for(TREE_CONTROL, 0),
                    Record::delete(shared_ref_key(&r), 0),
                ));
            } else {
                remaining += 1;
            }
        }
        if !deletes.is_empty() {
            self.write_control_entry(deletes, EntryAdmission::Try)
                .await?;
        }
        Ok(if remaining == 0 {
            SharedRelease::Freed
        } else {
            SharedRelease::Held { remaining }
        })
    }

    /// Tree 0, or the refusal every index verb answers on a volume that
    /// has none.
    fn control_tree(&self) -> std::result::Result<Arc<super::super::tree::KvTree>, KvError> {
        self.forest_control_tree().ok_or_else(|| {
            KvError::Busy(format!(
                "{}: the shared-block index lives in tree 0 and this volume has none (bit 17 \
                 absent)",
                self.path.display()
            ))
        })
    }
}

/// The 28-byte reference key with its owner rewritten — the routed
/// layer's GLOBAL → LOCAL KEY translation for a forest volume, and the
/// probe's inverse. Pure; the routing itself is the routed layer's.
pub fn with_owner(r: &BlockRef, owner_ino: u64) -> BlockRef {
    BlockRef { owner_ino, ..*r }
}

/// **fsck C16 — shared-index drift** (design §5.8.5, report-only): over
/// one data volume, the SHARED-flagged references of every mounted meta
/// volume (their owners resolved to the routed GLOBAL identity) against
/// the index home's entries — a flag without an entry on the source OR
/// the target ino, an entry whose ino holds no SHARED reference. Both
/// sets are read once per call; the caller confirms a finding by a
/// second call (the verify-before-report ladder). Empty on a set without
/// a forest home.
pub async fn shared_index_drift(
    mb: &crate::meta_backend::RoutedMetaBackend,
    vol_tag: u64,
) -> std::result::Result<Vec<SharedIndexDrift>, KvError> {
    let Some(home) = mb.volumes.get(index_home_volume()) else {
        return Ok(Vec::new());
    };
    if !home.symmetric_forest() {
        return Ok(Vec::new());
    }
    let mut flagged: std::collections::BTreeSet<(u64, u64, u32)> =
        std::collections::BTreeSet::new();
    let mut flagged_refs: std::collections::BTreeMap<(u64, u64, u32), BlockRef> =
        std::collections::BTreeMap::new();
    for (v, vol) in mb.volumes.iter().enumerate() {
        for r in vol.shared_flagged_refs(vol_tag).await? {
            // A forest keys the owner's LOCAL KEY form; the index keys the
            // GLOBAL ino. A raw control local with no global form cannot
            // own a data block — skipped, never invented.
            let owner = if vol.symmetric_forest() {
                match mb.try_make_global_ino(r.owner_ino, v) {
                    Some(g) => g,
                    None => continue,
                }
            } else {
                r.owner_ino
            };
            let id = (owner, r.block_idx, r.block_index);
            flagged.insert(id);
            flagged_refs.insert(id, with_owner(&r, owner));
        }
    }
    let mut indexed: std::collections::BTreeSet<(u64, u64, u32)> =
        std::collections::BTreeSet::new();
    let mut indexed_refs: std::collections::BTreeMap<(u64, u64, u32), BlockRef> =
        std::collections::BTreeMap::new();
    for r in home.shared_index_scan(vol_tag).await? {
        let id = (r.owner_ino, r.block_idx, r.block_index);
        indexed.insert(id);
        indexed_refs.insert(id, r);
    }
    let mut out = Vec::new();
    for id in flagged.difference(&indexed) {
        out.push(SharedIndexDrift::FlagWithoutEntry(flagged_refs[id]));
    }
    for id in indexed.difference(&flagged) {
        out.push(SharedIndexDrift::EntryWithoutFlag(indexed_refs[id]));
    }
    Ok(out)
}

/// A GLOBAL ino's LOCAL KEY form on a volume whose legacy keyspace hosts
/// routing slot `native` under the derived width `width` — the SAME
/// arithmetic `RoutedMetaBackend::route_ino` runs, for a caller that holds
/// the set's stamp and no routed set (the offline conversion's record
/// collector keys a forest volume's references by it). `width ≤ 1` is the
/// identity (the in-RAM test constructor's shape).
pub fn local_key_owner(global: u64, width: u64, native: Option<u16>) -> u64 {
    if width <= 1 {
        return global;
    }
    let (slot, raw) = crate::meta_backend::route_ino_width(global, width);
    if native == Some(slot as u16) {
        raw
    } else {
        crate::meta_backend::guest_local_ino(slot as u16, raw)
    }
}

/// The inverse of [`local_key_owner`]: a LOCAL KEY owner back to its
/// GLOBAL ino — how a forest volume's reference set is compared against a
/// flat one's (the relayout oracle) and how a probe's owner is reported
/// in the routed layer's identity. A raw native local on a volume with no
/// native slot has no global form and is returned verbatim.
pub fn global_owner(local_key: u64, width: u64, native: Option<u16>) -> u64 {
    if width <= 1 {
        return local_key;
    }
    match crate::meta_backend::split_guest_local(local_key) {
        Some((slot, raw)) => {
            crate::meta_backend::make_global_ino_width(raw, u64::from(slot), width)
        }
        None => match native {
            Some(n) => crate::meta_backend::make_global_ino_width(local_key, u64::from(n), width),
            None => local_key,
        },
    }
}

/// The 28-byte reference key `key` with its owner mapped through `f`.
pub fn rekey_owner(key: &[u8], f: impl Fn(u64) -> u64) -> Vec<u8> {
    use super::super::block_refs::BLOCK_REF_OWNER_OFF;
    let mut out = key.to_vec();
    if key.len() == BLOCK_REF_KEY_LEN {
        let owner = u64::from_be_bytes(
            key[BLOCK_REF_OWNER_OFF..BLOCK_REF_OWNER_OFF + 8]
                .try_into()
                .expect("length checked"),
        );
        out[BLOCK_REF_OWNER_OFF..BLOCK_REF_OWNER_OFF + 8].copy_from_slice(&f(owner).to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_key_roundtrips_and_bounds_one_block() {
        let r = BlockRef {
            vol_tag: 7,
            block_idx: 9,
            owner_ino: 11,
            block_index: 13,
        };
        let k = shared_ref_key(&r);
        assert_eq!(k.len(), SHARED_REF_KEY_LEN);
        assert_eq!(decode_shared_ref_key(&k).unwrap(), r);
        assert!(decode_shared_ref_key(&k[1..]).is_err());
        let (lo, hi) = shared_ref_block_range(7, 9);
        assert!(lo <= k && k < hi);
        let other = shared_ref_key(&BlockRef { block_idx: 10, ..r });
        assert!(!(lo <= other && other < hi));
        decode_shared_ref_value(&shared_ref_value()).unwrap();
        assert!(decode_shared_ref_value(&[2, 0, 0, 0]).is_err());
        assert!(decode_shared_ref_value(&[1, 1, 0, 0]).is_err());
        assert_eq!(index_home_volume(), 0);
    }
}
