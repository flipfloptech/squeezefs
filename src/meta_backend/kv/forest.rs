//! **The slot-tree forest router** — `SlotTrees`
//! (docs/design-symmetric-metadata.md §5.2 B(i), KD-SYM-2; incompat bit 17).
//!
//! On a forest volume there is no tree per record kind: there is ONE
//! mixed-kind [`KvTree`] per routing slot, plus the control tree
//! ([`TREE_CONTROL`], "tree 0"). This module owns that population and the
//! ONE routing function every backend site goes through:
//! [`SlotTrees::route_or_mint`] takes a `(kind, legacy key)` pair — exactly what
//! the shipped per-kind codecs produce — and answers the slot tree that
//! holds it together with the record's forest key (§5.2.1). Reads route
//! the same way and strip the kind byte on the way out, so every per-kind
//! decoder in the tree applies verbatim; range scans over a kind filter
//! the mixed leaf by kind and walk the slot trees in slot order, which IS
//! legacy key order (a slot is a contiguous key-ino range).
//!
//! Slot trees are minted **lazily**: a slot with no records has no root
//! and owns no extent (§5.2.2 — the per-slot extent floor is paid only
//! by slots that hold data). The native slot tree's root rides the fixed
//! ledger; every guest slot tree's root is published into tree 0 as a
//! [`super::slot_state`] record by the checkpoint task before it flushes
//! tree 0 and names tree 0's root in the ledger — the dying-floor law's
//! covering record for a slot tree's root swap.
//!
//! Every SMO of a slot tree is the owner's own ([`KvTree`]'s serialized
//! SMO context, one ring): nothing here adds a lock class, and the node
//! cache stays the one RAM-authoritative store for every tree.

use super::node_cache::NodeCache;
use super::record::{
    forest_key, forest_key_slot, split_forest_key, ForestSlot, KIND_INTERIOR, NATIVE_FOREST_SLOT,
    TREE_BLOCK_REFS, TREE_CONTROL,
};
use super::tree::{KvTree, RootPtr, SmoContext};
use super::KvError;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The content kinds a slot tree's leaf interleaves — inode, dentry,
/// xattr, block map, block reference (`record::is_slot_tree_kind`): the
/// kind-filtered range walk's over-fetch factor.
const SLOT_TREE_KINDS: usize = 5;

/// The forest of one mounted volume.
pub struct SlotTrees {
    /// Tree 0 — the control tree (`slot_state` records).
    control: Arc<KvTree>,
    /// Slot 0 — the native keyspace's tree; root in the fixed ledger.
    native: Arc<KvTree>,
    /// Guest slot trees, minted on first write / opened from tree 0.
    guests: scc::HashMap<ForestSlot, Arc<KvTree>>,
    /// Serializes guest mints: two committers racing the first record of
    /// one slot must agree on ONE root (the loser adopts the winner's).
    mint: crate::sqz_sync::SqzMutex<()>,
    /// Roots as last PUBLISHED into tree 0 — the checkpoint publishes
    /// only the slots whose live root moved.
    published: scc::HashMap<ForestSlot, RootPtr>,
    /// Guest slot trees minted this mount (`meta_kv_forest_slot_trees_minted`).
    minted: AtomicU64,
    /// `slot_state` records the checkpoint published (`meta_kv_forest_root_publishes`).
    publishes: AtomicU64,
}

/// A routed record: the slot tree holding it and its forest key.
pub struct Routed {
    pub slot: ForestSlot,
    pub tree: Arc<KvTree>,
    pub key: Vec<u8>,
}

/// How — and whether — a slot tree may be minted for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintPolicy {
    /// The writer's commit path: USER growth — one `claim_user` extent,
    /// refused at the growth floor exactly like a new leaf (§4.7).
    User,
    /// The writer's mount replay: a RECOVERY act — `claim_internal`, which
    /// may draw the compaction reserve (the flat bit-9/16 mount-time
    /// mints' class). A volume that crashed in the `heap_full` posture
    /// with an unpublished mint in its window must still MOUNT: the user
    /// class would refuse the mount itself, every mount, for ever.
    Recovery,
    /// A mount that may not write (a `-o ro` reader, a co-writer, a
    /// probe, a peer-owned open): NEVER mints — a mint is an extent claim
    /// plus a device write, onto the very extents the live writer's
    /// unpublished roots occupy. Reaching a mint under this policy is an
    /// invariant violation (loud, counted), never a silent write.
    Refuse,
}

/// What a lazy mint needs from the backend: the volume's node cache, its
/// seq source, its allocator, the journal head at the mint (the new
/// tree's `root_floor`), the policy the mount's posture sets, and — for
/// a slot a declared appender leases — that appender's region, whose
/// grant the root extent is claimed from and whose ring head the floor is
/// (§5.3.3).
pub struct MintContext<'a> {
    pub cache: &'a Arc<NodeCache>,
    pub seq: &'a Arc<AtomicU64>,
    pub alloc: &'a Arc<super::alloc_ext::ExtentAllocator>,
    pub floor: u64,
    pub policy: MintPolicy,
    pub region: Option<super::tree::SmoRegion>,
}

impl SlotTrees {
    /// Assemble the forest from its opened trees (the mount path).
    pub fn new(
        control: Arc<KvTree>,
        native: Arc<KvTree>,
        guests: Vec<(ForestSlot, Arc<KvTree>)>,
    ) -> Self {
        let map = scc::HashMap::new();
        let published = scc::HashMap::new();
        for (slot, tree) in guests {
            let root = tree.root();
            let _ = map.insert_sync(slot, tree);
            let _ = published.insert_sync(slot, root);
        }
        Self {
            control,
            native,
            guests: map,
            mint: crate::sqz_sync::SqzMutex::new(()),
            published,
            minted: AtomicU64::new(0),
            publishes: AtomicU64::new(0),
        }
    }

    /// Tree 0.
    pub fn control(&self) -> &Arc<KvTree> {
        &self.control
    }

    /// Hold the mint serialization: while held no slot tree is between
    /// its root claim and its publication into the forest (fsck C13's
    /// coverage of the lazy mint — the SMO mutex covers every other
    /// claim).
    pub(super) async fn mint_guard(&self) -> crate::sqz_sync::SqzMutexGuard<'_, ()> {
        self.mint.lock().await
    }

    /// The native slot tree.
    pub fn native(&self) -> &Arc<KvTree> {
        &self.native
    }

    /// The slot tree of `slot`, if it has been minted.
    pub fn tree(&self, slot: ForestSlot) -> Option<Arc<KvTree>> {
        if slot == NATIVE_FOREST_SLOT {
            return Some(Arc::clone(&self.native));
        }
        self.guests.read_sync(&slot, |_, t| Arc::clone(t))
    }

    /// Every slot tree that exists, in slot order (native first).
    pub fn slot_trees(&self) -> Vec<(ForestSlot, Arc<KvTree>)> {
        let mut out: Vec<(ForestSlot, Arc<KvTree>)> = Vec::with_capacity(self.guests.len() + 1);
        out.push((NATIVE_FOREST_SLOT, Arc::clone(&self.native)));
        self.guests.iter_sync(|slot, t| {
            out.push((*slot, Arc::clone(t)));
            true
        });
        out.sort_by_key(|(s, _)| *s);
        out
    }

    /// Every tree the checkpoint must flush and root: tree 0 + every slot
    /// tree (slot order).
    pub fn all(&self) -> Vec<Arc<KvTree>> {
        let mut v = Vec::with_capacity(self.guests.len() + 2);
        v.push(Arc::clone(&self.control));
        v.extend(self.slot_trees().into_iter().map(|(_, t)| t));
        v
    }

    /// Number of slot trees that exist (tree 0 excluded).
    pub fn slot_tree_count(&self) -> usize {
        self.guests.len() + 1
    }

    /// Guest slot roots currently published in tree 0 (the RAM mirror of
    /// its `slot_state` population).
    pub fn published_count(&self) -> usize {
        self.published.len()
    }

    /// Guest slot trees minted since mount.
    pub fn minted(&self) -> u64 {
        self.minted.load(Ordering::Relaxed)
    }

    /// `slot_state` records published since mount.
    pub fn publishes(&self) -> u64 {
        self.publishes.load(Ordering::Relaxed)
    }

    /// The tree a dirty cache node belongs to: tree 0 by its header id,
    /// a slot tree by the RAM stamp its owner left on the node.
    pub fn tree_for_node(
        &self,
        node: &super::node_cache::CachedNode,
    ) -> Result<Arc<KvTree>, KvError> {
        if node.tree_id() == TREE_CONTROL {
            return Ok(Arc::clone(&self.control));
        }
        if node.tree_id() != KIND_INTERIOR {
            return Err(KvError::Corrupt(format!(
                "forest volume holds a node {:#x} of tree id {} — neither tree 0 nor a slot tree",
                node.addr(),
                node.tree_id()
            )));
        }
        let slot = node.forest_slot().ok_or_else(|| {
            KvError::Corrupt(format!(
                "dirty slot-tree node {:#x} carries no owner stamp (a node was mutated outside \
                 its tree's resolve path)",
                node.addr()
            ))
        })?;
        self.tree(slot).ok_or_else(|| {
            KvError::Corrupt(format!(
                "dirty node {:#x} names slot {slot}, which has no tree",
                node.addr()
            ))
        })
    }

    /// Route a `(kind, legacy key)` pair to its slot tree, minting the
    /// tree on the slot's first record. `mint_alloc` is the volume's
    /// allocator (one extent per fresh tree, internal class).
    pub async fn route_or_mint(
        &self,
        kind: u8,
        legacy: &[u8],
        mint: &MintContext<'_>,
    ) -> Result<Routed, KvError> {
        let key = forest_key(kind, legacy)?;
        let slot = forest_key_slot(&key)?;
        let tree = self.slot_or_mint(slot, mint).await?;
        Ok(Routed { slot, tree, key })
    }

    /// The forest key and slot of a `(kind, legacy key)` pair — the routing
    /// decision without the mint, for a caller that builds the mint
    /// context PER SLOT (a leased slot's mint claims from its lessee's
    /// grant).
    pub fn route_key(&self, kind: u8, legacy: &[u8]) -> Result<(ForestSlot, Vec<u8>), KvError> {
        let key = forest_key(kind, legacy)?;
        let slot = forest_key_slot(&key)?;
        Ok((slot, key))
    }

    /// The slot a CONTENT forest key names, counting a refused key on
    /// `meta_kv_forest_key_violations`.
    pub fn slot_of_forest_key(&self, key: &[u8]) -> Result<ForestSlot, KvError> {
        forest_key_slot(key).map_err(|e| {
            super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
            e
        })
    }

    /// Route a journaled / replayed CONTENT forest key to its slot tree,
    /// minting on first touch. (Interior records name their slot on the
    /// journal key — [`split_interior_journal_key`] + [`Self::slot_or_mint`].)
    pub async fn route_forest_key_or_mint(
        &self,
        key: &[u8],
        mint: &MintContext<'_>,
    ) -> Result<(ForestSlot, Arc<KvTree>), KvError> {
        let slot = forest_key_slot(key).map_err(|e| {
            super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
            e
        })?;
        Ok((slot, self.slot_or_mint(slot, mint).await?))
    }

    /// The slot tree of `slot`, minted on first touch.
    pub async fn slot_or_mint(
        &self,
        slot: ForestSlot,
        mint: &MintContext<'_>,
    ) -> Result<Arc<KvTree>, KvError> {
        match self.tree(slot) {
            Some(t) => Ok(t),
            None => self.mint(slot, mint).await,
        }
    }

    /// Route a `(kind, legacy key)` READ: `None` when the slot has no tree
    /// (nothing was ever written there — the read is a miss by
    /// construction, no extent minted for it).
    pub fn route_read(&self, kind: u8, legacy: &[u8]) -> Result<Option<Routed>, KvError> {
        let key = forest_key(kind, legacy)?;
        let slot = forest_key_slot(&key)?;
        Ok(self.tree(slot).map(|tree| Routed { slot, tree, key }))
    }

    /// Mint the slot tree of `slot`: ONE extent — `claim_user` on the
    /// commit path (§4.7 user growth, refused at the growth floor),
    /// `claim_internal` at the writer's replay (a recovery act may draw
    /// the reserve) — and ONE device write + load-back of its empty root,
    /// on the caller's task — the one place the commit pipeline's resolve
    /// step touches the device (once per slot per volume, before any node
    /// lock; design-symmetric-metadata §5.3.3 moves the extent to the
    /// appender's grant in PR 3). A non-writer's policy refuses HERE,
    /// before any claim or write. The tree is born with `root_floor =
    /// mint.floor` (the ring head): nothing applied to it can sit below
    /// that position, and the checkpoint tail must not pass it until tree
    /// 0 names the root.
    async fn mint(&self, slot: ForestSlot, mint: &MintContext<'_>) -> Result<Arc<KvTree>, KvError> {
        let class = match mint.policy {
            MintPolicy::User => super::alloc_ext_core::AllocClass::User,
            MintPolicy::Recovery => super::alloc_ext_core::AllocClass::Internal,
            MintPolicy::Refuse => {
                crate::note_invariant_tripwire(
                    "forest_mint_on_non_writer",
                    &format!(
                        "slot {slot}: a mount that may not write reached a slot-tree mint (an \
                         extent claim + a device write) — refused"
                    ),
                );
                return Err(KvError::Corrupt(format!(
                    "slot {slot} has no published root and this mount may not write: a reader / \
                     co-writer never mints a slot tree (the records are served after the \
                     writer publishes — S5 bounded staleness)"
                )));
            }
        };
        let _g = self.mint.lock().await;
        if let Some(t) = self.tree(slot) {
            return Ok(t); // the racing minter won
        }
        let mut ctx = SmoContext::new(Arc::clone(mint.alloc));
        ctx.set_region(mint.region.clone());
        let tree = Arc::new(
            KvTree::create_slot_tree(
                Arc::clone(mint.cache),
                &mut ctx,
                slot,
                Arc::clone(mint.seq),
                mint.floor,
                class,
            )
            .await?,
        );
        let _ = self.guests.insert_sync(slot, Arc::clone(&tree));
        self.minted.fetch_add(1, Ordering::Relaxed);
        super::META_KV_FOREST_SLOT_TREES_MINTED.fetch_add(1, Ordering::Relaxed);
        Ok(tree)
    }

    /// A reader's adoption of a guest tree it did not know (opened from
    /// tree 0's `slot_state` at an epoch step — never minted).
    pub fn adopt_guest(&self, slot: ForestSlot, tree: Arc<KvTree>) {
        let root = tree.root();
        let _ = self.guests.insert_sync(slot, tree);
        let _ = self.published.upsert_sync(slot, root);
    }

    /// The journal position every UNPUBLISHED guest root came into force
    /// at, PER SLOT (empty = every guest root is named in tree 0): the
    /// checkpoint tail's clamp while a publication is pending or
    /// deferred — until tree 0 durably names a root, every record applied
    /// under it must stay in the replay window. A floor is a position in
    /// the ring the slot's records journal into, so a partitioned volume
    /// folds each into ITS region's tail.
    pub fn unpublished_root_floors(&self) -> std::collections::BTreeMap<ForestSlot, u64> {
        let mut out = std::collections::BTreeMap::new();
        self.guests.iter_sync(|slot, tree| {
            let live = tree.root();
            let stale = self
                .published
                .read_sync(slot, |_, p| *p != live)
                .unwrap_or(true);
            if stale {
                out.insert(*slot, tree.root_floor());
            }
            true
        });
        out
    }

    /// Split a slot-tree key into `(kind, legacy key)`, or `None` for a
    /// key the §5.2.1 codec refuses — counted on
    /// `meta_kv_forest_key_violations` (must stay 0) and logged. The
    /// kind-routed RECORD walks skip such a record rather than fail the
    /// page (see [`Self::range`]); fsck's raw forest walk is what reports
    /// it. The by-block refs family does NOT ride this — it refuses
    /// ([`Self::refs_window`]).
    fn decode_or_skip(key: &[u8]) -> Option<(u8, Vec<u8>)> {
        match split_forest_key(key) {
            Ok(split) => Some(split),
            Err(e) => {
                super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                log::error!(
                    "slot-tree record under a key the forest codec refuses ({} bytes, {:02x?}…): \
                     {e} — skipped by the kind-routed walk; fsck class C1 reports it",
                    key.len(),
                    &key[..key.len().min(12)]
                );
                None
            }
        }
    }

    /// Point lookup of a `(kind, legacy key)`.
    pub async fn lookup(&self, kind: u8, legacy: &[u8]) -> Result<Option<Bytes>, KvError> {
        match self.route_read(kind, legacy)? {
            Some(r) => r.tree.lookup(&r.key).await,
            None => Ok(None),
        }
    }

    /// The durable delta-chain probe of a `(kind, legacy key)`
    /// ([`KvTree::delta_chain_probe`]).
    pub async fn delta_chain_probe(
        &self,
        kind: u8,
        legacy: &[u8],
    ) -> Result<(u32, Option<(u64, u64)>), KvError> {
        match self.route_read(kind, legacy)? {
            Some(r) => r.tree.delta_chain_probe(&r.key).await,
            None => Ok((0, None)),
        }
    }

    /// Range scan of kind `kind` over the LEGACY window `[start, end]`
    /// (inclusive, memcmp order), at most `max` records, keys returned in
    /// their legacy form. Walks the slot trees the window spans in slot
    /// order — legacy key order — filtering each mixed leaf by kind.
    pub async fn range(
        &self,
        kind: u8,
        start: &[u8],
        end: &[u8],
        max: usize,
    ) -> Result<Vec<(Bytes, Bytes)>, KvError> {
        let mut out: Vec<(Bytes, Bytes)> = Vec::new();
        if max == 0 || start > end {
            return Ok(out);
        }
        if !super::record::is_slot_tree_kind(kind) {
            return Err(KvError::Corrupt(format!(
                "range scan of kind {kind} — not a slot-tree content kind"
            )));
        }
        if kind == TREE_BLOCK_REFS {
            // By-block keys sort block-major inside a tree but the forest
            // is slot-major, so a legacy-key cursor cannot name a resume
            // point across trees: the refs family is scanned whole-window,
            // one cursor per tree — `refs_window`.
            return Err(KvError::Corrupt(
                "block references are not range-paged on a forest volume — use the \
                 whole-window scan"
                    .to_string(),
            ));
        }
        let (fstart, first_slot) = forest_bound(kind, start, false);
        let (fend, last_slot) = forest_bound(kind, end, true);
        // The common case — a chain scan, an ino's xattrs, a directory's
        // entries — spans ONE slot: probe that tree alone. Only a census
        // window walks the forest (slot order = legacy key order).
        let trees: Vec<(ForestSlot, Arc<KvTree>)> = if first_slot == last_slot {
            match self.tree(first_slot) {
                Some(t) => vec![(first_slot, t)],
                None => return Ok(out), // no tree ⇒ nothing was ever written there
            }
        } else {
            self.slot_trees()
                .into_iter()
                .filter(|(slot, _)| *slot >= first_slot && *slot <= last_slot)
                .collect()
        };
        for (_slot, tree) in trees {
            let mut cursor = fstart.clone();
            'tree: loop {
                // A page of the MIXED leaf: a window that spans kinds (a
                // census window over inos) yields one wanted record per
                // `SLOT_TREE_KINDS` raw records at an even interleave, so
                // a page of that many times the remainder returns the
                // remainder in one round trip; a single-kind window (a
                // chain scan) is over-read by the same factor at most,
                // bounded by the window itself.
                let want = (max - out.len()).saturating_mul(SLOT_TREE_KINDS);
                let page = tree.range(&cursor, &fend, want).await?;
                let Some((last, _)) = page.last() else {
                    break 'tree;
                };
                cursor = super::node::key_successor(last);
                for (k, v) in &page {
                    // A key the codec refuses is corruption (or a writer
                    // bug) — counted on the must-stay-0 tripwire and
                    // logged, then SKIPPED: failing the whole page would
                    // hide every other record of the leaf from the caller
                    // (fsck's census walks in particular — a truncated
                    // census reads live blocks as unreferenced, and the
                    // repair of THAT finding frees them). fsck's forest C1
                    // walk reads the slot trees raw and reports the key.
                    let Some((k_kind, legacy)) = Self::decode_or_skip(k) else {
                        continue;
                    };
                    if k_kind != kind {
                        continue;
                    }
                    out.push((Bytes::from(legacy), v.clone()));
                    if out.len() >= max {
                        return Ok(out);
                    }
                }
                if page.len() < want {
                    break 'tree;
                }
            }
        }
        Ok(out)
    }

    /// Every block reference inside the LEGACY window `[start, end]`
    /// (the `block_range` / `volume_range` shapes), across EVERY slot tree
    /// — a reference lives in the referencing ino's slot (§5.4.2), so the
    /// population of one block is the union over the forest. Each tree is
    /// paged with its OWN cursor from the window's start: the caller's
    /// legacy-key cursor cannot express a forest resume point (refs sort
    /// block-major inside a tree, the forest is slot-major). Keys come
    /// back in legacy form.
    ///
    /// A key in the window the codec refuses is REFUSED, never skipped —
    /// the flat decode's behaviour (`decode_block_ref_key` → `Err` at the
    /// caller): the population of a block decides a terminal free, a W1
    /// sole-owner patch and an S9 free's validation, and an answer one
    /// short is the exact failure the ledger exists to prevent. The
    /// skip-not-fail law is the kind-routed RECORD walks' ([`Self::range`]),
    /// where a truncated census is the worse outcome and fsck's raw C1 walk
    /// is the detector. Counted on the tripwire before the refusal.
    pub async fn refs_window(
        &self,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<(Bytes, Bytes)>, KvError> {
        // The refs prefix sorts after every ino-major key, so the window
        // is the same forest window in every tree.
        let mut fstart = Vec::with_capacity(start.len() + 1);
        fstart.push(TREE_BLOCK_REFS);
        fstart.extend_from_slice(start);
        let mut fend = Vec::with_capacity(end.len() + 1);
        fend.push(TREE_BLOCK_REFS);
        fend.extend_from_slice(end);
        // One leaf's worth of refs per page: a 29 B key + 4 B value + the
        // record header is ~50 B, so 512 records ≈ 25 KiB — under one node.
        const PAGE: usize = 512;
        let mut out: Vec<(Bytes, Bytes)> = Vec::new();
        for (_slot, tree) in self.slot_trees() {
            let mut cursor = fstart.clone();
            loop {
                let page = tree.range(&cursor, &fend, PAGE).await?;
                let Some((last, _)) = page.last() else {
                    break;
                };
                cursor = super::node::key_successor(last);
                for (k, v) in &page {
                    let (k_kind, legacy) = match split_forest_key(k) {
                        Ok(split) => split,
                        Err(e) => {
                            super::META_KV_FOREST_KEY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
                            return Err(KvError::Corrupt(format!(
                                "block-reference record under a key the forest codec refuses \
                                 ({} bytes, {:02x?}…): {e} — the population is refused, never \
                                 answered short; fsck class C1 names the record",
                                k.len(),
                                &k[..k.len().min(12)]
                            )));
                        }
                    };
                    if k_kind != TREE_BLOCK_REFS {
                        continue;
                    }
                    out.push((Bytes::from(legacy), v.clone()));
                }
                if page.len() < PAGE {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The live roots of every guest slot tree whose root moved since it
    /// was last published (or was never published) — what the checkpoint
    /// writes into tree 0 before flushing it. Records the publication
    /// when `commit` is `true`.
    pub fn roots_to_publish(&self) -> Vec<(ForestSlot, RootPtr)> {
        let mut out = Vec::new();
        self.guests.iter_sync(|slot, tree| {
            let live = tree.root();
            let stale = self
                .published
                .read_sync(slot, |_, p| *p != live)
                .unwrap_or(true);
            if stale {
                out.push((*slot, live));
            }
            true
        });
        out.sort_by_key(|(s, _)| *s);
        out
    }

    /// Note that `slot`'s root `root` has been published into tree 0.
    pub fn note_published(&self, slot: ForestSlot, root: RootPtr) {
        let _ = self.published.upsert_sync(slot, root);
        self.publishes.fetch_add(1, Ordering::Relaxed);
        super::META_KV_FOREST_ROOT_PUBLISHES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Translate a LEGACY range bound of an INO-MAJOR kind — exact-length key
/// or a short/long sentinel such as `[0]` / `KEY_SPACE_MAX` — into the
/// forest bound with the same cut, plus the slot it falls in. A bound of
/// ≥ 8 bytes takes the kind byte at offset 8 exactly like a key; a
/// shorter one is a prefix of the key-ino bytes and cuts the forest order
/// at the same place verbatim. The slot is read from the ino bytes
/// present, padded with `0x00` for a start bound and `0xFF` for an end
/// bound. (The by-block family never comes here — [`SlotTrees::refs_window`].)
fn forest_bound(kind: u8, legacy: &[u8], is_end: bool) -> (Vec<u8>, ForestSlot) {
    let pad = if is_end { 0xFF } else { 0x00 };
    let mut fb = Vec::with_capacity(legacy.len() + 1);
    if legacy.len() >= 8 {
        fb.extend_from_slice(&legacy[..8]);
        fb.push(kind);
        fb.extend_from_slice(&legacy[8..]);
    } else {
        fb.extend_from_slice(legacy);
    }
    let mut ino = [pad; 8];
    let have = legacy.len().min(8);
    ino[..have].copy_from_slice(&legacy[..have]);
    (
        fb,
        super::record::forest_slot_of_ino(u64::from_be_bytes(ino)),
    )
}

/// Bytes of the slot prefix on a slot tree's interior JOURNAL record key.
pub const INTERIOR_JOURNAL_SLOT_LEN: usize = 4;

/// The journal key of a slot tree's interior pointer record: `slot: u32 BE
/// ‖ separator`. A slot tree's interior records carry kind 0 (§5.2.1) and
/// a separator alone cannot name the tree — the rightmost child's is
/// `KEY_SPACE_MAX`, which every tree's top separator shares, and the refs
/// family means a slot tree's key space is not one contiguous slot range
/// — so the producer prefixes the slot and replay strips it
/// ([`split_interior_journal_key`]).
pub fn interior_journal_key(slot: ForestSlot, separator: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(INTERIOR_JOURNAL_SLOT_LEN + separator.len());
    k.extend_from_slice(&slot.to_be_bytes());
    k.extend_from_slice(separator);
    k
}

/// Split a slot tree's interior journal key into `(slot, separator)`.
/// Refuses a key too short to carry a separator (an interior separator is
/// never empty — `KvTree::check_interior_key`).
pub fn split_interior_journal_key(key: &[u8]) -> Result<(ForestSlot, &[u8]), KvError> {
    if key.len() <= INTERIOR_JOURNAL_SLOT_LEN {
        return Err(KvError::Corrupt(format!(
            "slot-tree interior journal key of {} bytes carries no separator",
            key.len()
        )));
    }
    let slot = ForestSlot::from_be_bytes([key[0], key[1], key[2], key[3]]);
    Ok((slot, &key[INTERIOR_JOURNAL_SLOT_LEN..]))
}
