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
use super::super::journal::entry_len_for;
use super::super::record::{ForestSlot, Record, TREE_BLOCK_REFS, TREE_CONTROL};
use super::super::KvError;
use super::{HeldAdmission, KvMetaBackend, KvTx};
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
/// `shared_release_failures` (must-stay-0): a SHARED block's release the
/// index home could not decide (the journal-failure class) — its
/// reference and mark were KEPT, the block stays allocated; never a
/// discarded failure.
pub static SHARED_RELEASE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// How many times `release_shared` re-sizes its parking pre-admission
/// when the index population grows under it (a racing `ShareBlock` of
/// the same block — a clone storm on one block; each attempt re-reads
/// the population, so the bound is a loud-refusal belt, never a wait).
const RELEASE_RESIZE_ATTEMPTS: u32 = 8;

/// The index HOME's volume ordinal in the routed set when no allocation
/// lease is known — PR 7's default: volume 0, whose manager holds the
/// set-wide roles (KD-SYM-2). [`index_home_volume_for`] is the resolver
/// every caller uses.
pub const fn index_home_volume() -> usize {
    0
}

/// **The index HOME for data volume `vol_tag`** — PR 8's re-point of the
/// ONE function (design §5.4.4 / §5.5): the volume the data volume's
/// ALLOCATION-LEASE HOLDER is homed on (`alloc_lease:{vol_tag}.home_vol`,
/// read off this process's holding — the holder is this mount on every PR
/// 8 shape), else PR 7's default. A wire writer's projection of a lease it
/// does not hold (the record read off the coordinator's tree 0) is PR 12's
/// join ladder's; until then a non-holder reaches the default home.
pub fn index_home_volume_for(vol_tag: u64) -> usize {
    crate::meta_backend::kv::alloc_lease::holding(vol_tag)
        .map(|h| usize::from(h.home_vol()))
        .unwrap_or_else(index_home_volume)
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

/// The one-slot probe's answer: the population of `(vol_tag, block_idx)`
/// in the probed slot tree and whether ANY of those references carries
/// SHARED — the W1 durable clause reads both (design §5.4.3 law 2: the
/// predicate "adds the SHARED flag"; a count of 1 under a set flag is a
/// clone's source between its `MarkShared` and its publish, never sole
/// ownership).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefProbe {
    pub count: usize,
    pub shared: bool,
}

/// What the ROUTED layer lends the volume-level executors (installed by
/// `DataRouter::arm_shared_refs`; absent on every mount without an armed
/// forest): the durable flags of a reference named by its GLOBAL owner
/// (the served `ShareBlock`'s screen — PR 3/4's law that a wire word acts
/// on nothing until durable state confirms it — and `ReleaseShared`'s GC
/// arm), and the RAM SHARED mark a served `MarkShared` sets beside its
/// durable bit (the accelerator the W1 predicate reads synchronously; the
/// durable flag stays the authority).
pub trait RoutedSharedRefs: Send + Sync {
    /// `Some(shared)` when `owner_ino` (GLOBAL) holds a reference to the
    /// block at `block_index` on its own volume, `None` when no record
    /// exists.
    fn ref_flags(
        &self,
        r: BlockRef,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<Option<bool>, KvError>>
                + Send
                + '_,
        >,
    >;
    /// The RAM mark of `(vol_tag, block_idx)` set on this mount.
    fn note_marked(&self, vol_tag: u64, block_idx: u64);
}

/// The installed hooks — a `RwLock` rather than an `ArcSwap` because the
/// trait object is a fat pointer `ArcSwap` cannot hold without a second
/// `Arc`; every reader is a served verb or a GC arm, never a hot path.
static ROUTED: std::sync::RwLock<Option<Arc<dyn RoutedSharedRefs>>> = std::sync::RwLock::new(None);

/// Install the routed layer's hooks (one per process — the armed mount's;
/// a later arm replaces an earlier rig's in the same process). The hooks
/// hold WEAK handles to the router and the set, so an installed rig's
/// death leaves them inert, never kept alive.
pub fn install_routed(hooks: Arc<dyn RoutedSharedRefs>) {
    *ROUTED.write().unwrap_or_else(|e| e.into_inner()) = Some(hooks);
}

/// Forget the installed hooks (an UNARMED arm in a process that armed an
/// earlier rig — fixture hygiene; production runs one mount per process).
pub fn clear_routed() {
    *ROUTED.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The installed hooks, `None` on a mount that never armed.
pub fn routed() -> Option<Arc<dyn RoutedSharedRefs>> {
    ROUTED.read().unwrap_or_else(|e| e.into_inner()).clone()
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
        Ok(self
            .block_ref_probe_flags(vol_tag, block_idx, slot)
            .await?
            .count)
    }

    /// [`Self::block_ref_probe`] with the SHARED flag folded over the
    /// probed records — the W1 durable clause's face (`RefProbe`). The
    /// whole-volume arm (`slot == None` / a flat volume) walks the by-block
    /// window it already reads for the count.
    pub async fn block_ref_probe_flags(
        &self,
        vol_tag: u64,
        block_idx: u64,
        slot: Option<ForestSlot>,
    ) -> std::result::Result<RefProbe, KvError> {
        BLOCK_REF_PROBES.fetch_add(1, Ordering::Relaxed);
        if !self.block_refs_engaged() {
            return Ok(RefProbe::default());
        }
        let (start, end) = block_range(vol_tag, block_idx);
        let records = match (self.forest(), slot) {
            (Some(forest), Some(slot)) => forest.refs_probe(slot, &start, &end).await?,
            _ => self.block_refs_window(&start, &end).await?,
        };
        let mut probe = RefProbe {
            count: records.len(),
            shared: false,
        };
        // Both halves decoded for every record (the by-block family's
        // discipline): a malformed accounting record is loud corruption,
        // never a silently skipped — or silently counted — reference.
        for (k, v) in &records {
            decode_block_ref_key(k)?;
            probe.shared |= decode_block_ref_value(v)?.shared;
        }
        Ok(probe)
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
        // The USER clone path: admit the entry's worst case — every named
        // reference a fresh put — PARKING, before the verb mutex (PR 4's
        // door law: a park under `manager_verbs` deadlocks with the
        // checkpoint task's grant verbs; a `Try` fails the clone on a busy
        // ring 0). The exact length is split off under the mutex, the
        // rest released; every early return hands the budget back.
        let worst = entry_len_for(&self.shared_index_puts(refs))?;
        let pre = self.pre_admit_control(worst).await?;
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
            self.write_control_entry(recs, pre.into_entry()).await?;
        }
        Ok((inserted, already))
    }

    /// The served `ShareBlock` (the wire's untrusted words — PR 3/4's law:
    /// a frame acts on nothing until durable state confirms every word):
    /// each named `(owner_ino, block_index)` must hold a reference to the
    /// block on its own volume AND carry SHARED (the cloner's `MarkShared`
    /// precedes its `ShareBlock`, so a legitimate frame always does);
    /// otherwise the WHOLE frame is `Rejected` and nothing is written. The
    /// screen reads the routed view the arm installed; a mount without it
    /// (no armed router) cannot confirm and rejects.
    pub async fn share_block_screened(
        &self,
        refs: &[BlockRef],
    ) -> std::result::Result<(usize, usize), KvError> {
        let set = self.manager_gate(false)?;
        let reject = |why: String| {
            set.verbs.rejected.fetch_add(1, Ordering::Relaxed);
            KvError::Rejected(format!("ShareBlock: {why} (manager_verb_rejected)"))
        };
        let Some(routed) = routed() else {
            return Err(reject(
                "no routed view to confirm the frame's references against".to_string(),
            ));
        };
        for r in refs {
            match routed.ref_flags(*r).await? {
                Some(true) => {}
                Some(false) => {
                    return Err(reject(format!(
                        "ino {}'s reference to block {} of data volume {:#x} is not SHARED — \
                         MarkShared precedes ShareBlock",
                        r.owner_ino, r.block_idx, r.vol_tag
                    )));
                }
                None => {
                    return Err(reject(format!(
                        "ino {} holds no reference to block {} of data volume {:#x}",
                        r.owner_ino, r.block_idx, r.vol_tag
                    )));
                }
            }
        }
        self.share_block(refs).await
    }

    /// Every named reference as a fresh index put — the worst case
    /// `share_block` pre-admits for.
    fn shared_index_puts(&self, refs: &[BlockRef]) -> Vec<(u8, Record)> {
        refs.iter()
            .map(|r| {
                (
                    super::super::journal::tag_for(TREE_CONTROL, 0),
                    Record::put(shared_ref_key(r), 0, shared_ref_value().to_vec()),
                )
            })
            .collect()
    }

    /// A PARKING ring-0 admission for a control entry of at most `len`
    /// bytes, taken holding nothing (the door's pre-admission shape);
    /// released on drop unless handed into the entry.
    async fn pre_admit_control(&self, len: u64) -> std::result::Result<HeldAdmission<'_>, KvError> {
        // PR 8: the park pass is dropped with the admission's use here — a
        // control entry, not a user batch (its durable write is the
        // verb's terminal outcome; the PR-4 manager pre-admission's shape).
        let (adm, _park_pass) = self.admit_user_budget(&self.ring, 0, len).await?;
        Ok(HeldAdmission::new(self.ring.core(), Some(adm)))
    }

    /// The index's entries for one block at the home: the referencing
    /// `(owner_ino, block_index)` pairs.
    pub async fn shared_index_population(
        &self,
        vol_tag: u64,
        block_idx: u64,
    ) -> std::result::Result<Vec<BlockRef>, KvError> {
        BLOCK_REF_PROBES.fetch_add(1, Ordering::Relaxed);
        self.index_population_uncounted(vol_tag, block_idx).await
    }

    /// [`Self::shared_index_population`] without the gauge — the reads
    /// inside ONE decision (`release_shared`'s sizing read, its re-read
    /// under the mutex, a re-size), which count as one probe.
    async fn index_population_uncounted(
        &self,
        vol_tag: u64,
        block_idx: u64,
    ) -> std::result::Result<Vec<BlockRef>, KvError> {
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

    /// **`ReleaseShared { b, (ino, block_index) }` at the index home**
    /// (§5.4.4 step 4): delete the releaser's ONE entry for `b` — keyed on
    /// `(owner_ino, block_index)`, since the legal same-`off` nested
    /// clone+clip class gives one ino two references to one block and the
    /// other must stand — then decide from what remains:
    /// `still_referenced` answers, per remaining entry, whether its ino
    /// still holds a reference to `b` (the GC arm for the "cloner died
    /// after step 2" window: an entry whose reference is gone is deleted
    /// too, never counted). ONE control entry carries every delete, its
    /// admission taken PARKING before the verb mutex (the FREE path never
    /// fails on a busy ring — Issue 3). `NotShared` when the index never
    /// named the block; `Freed` when nothing remains; `Held` otherwise.
    /// Idempotent: a replay finds the releaser's entry gone and re-decides.
    pub async fn release_shared<F, Fut>(
        &self,
        vol_tag: u64,
        block_idx: u64,
        releaser: Option<(u64, u32)>,
        still_referenced: F,
    ) -> std::result::Result<SharedRelease, KvError>
    where
        F: Fn(BlockRef) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<bool, KvError>>,
    {
        RELEASE_SHARED_CALLS.fetch_add(1, Ordering::Relaxed);
        // ONE probe per release decision, however many reads size it.
        BLOCK_REF_PROBES.fetch_add(1, Ordering::Relaxed);
        let _set = self.manager_gate(false)?;
        // The pre-admission is sized from a population read holding
        // nothing; under the mutex the population is re-read, and one that
        // GREW past the admission (a racing `ShareBlock`) releases it and
        // re-sizes — a bounded loop, never a park under the mutex.
        let mut attempts = 0u32;
        loop {
            let sizing = self.index_population_uncounted(vol_tag, block_idx).await?;
            if sizing.is_empty() {
                return Ok(SharedRelease::NotShared);
            }
            let worst = entry_len_for(&Self::shared_index_deletes(&sizing))?;
            let pre = self.pre_admit_control(worst).await?;
            let _g = self.manager_verbs.lock().await;
            let population = self.index_population_uncounted(vol_tag, block_idx).await?;
            if population.is_empty() {
                return Ok(SharedRelease::NotShared);
            }
            if population.len() > sizing.len() {
                attempts += 1;
                if attempts < RELEASE_RESIZE_ATTEMPTS {
                    drop(_g);
                    drop(pre);
                    continue;
                }
                return Err(KvError::Busy(format!(
                    "{}: ReleaseShared of block {block_idx} on data volume {vol_tag:#x}: the \
                     index population grew under {RELEASE_RESIZE_ATTEMPTS} sizing attempts",
                    self.path.display()
                )));
            }
            let mut deletes = Vec::new();
            let mut remaining = 0usize;
            for r in population {
                let mine = releaser == Some((r.owner_ino, r.block_index));
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
                self.write_control_entry(deletes, pre.into_entry()).await?;
            }
            return Ok(if remaining == 0 {
                SharedRelease::Freed
            } else {
                SharedRelease::Held { remaining }
            });
        }
    }

    /// Every entry of a population as a delete — the worst case
    /// `release_shared` pre-admits for.
    fn shared_index_deletes(population: &[BlockRef]) -> Vec<(u8, Record)> {
        population
            .iter()
            .map(|r| {
                (
                    super::super::journal::tag_for(TREE_CONTROL, 0),
                    Record::delete(shared_ref_key(r), 0),
                )
            })
            .collect()
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
    let Some(home) = mb.volumes.get(index_home_volume_for(vol_tag)) else {
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
