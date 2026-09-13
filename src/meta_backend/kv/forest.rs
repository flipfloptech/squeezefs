//! **The slot-tree forest router** — `SlotTrees`
//! (docs/design-symmetric-metadata.md §5.2 B(i), KD-SYM-2; incompat bit 17).
//!
//! On a forest volume there is no tree per record kind: there is ONE
//! mixed-kind [`KvTree`] per routing slot, plus the control tree
//! ([`TREE_CONTROL`], "tree 0"). This module owns that population and the
//! ONE routing function every backend site goes through:
//! [`SlotTrees::route`] takes a `(kind, legacy key)` pair — exactly what
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
    forest_key, forest_key_kind, forest_key_slot, split_forest_key, ForestSlot, KIND_INTERIOR,
    NATIVE_FOREST_SLOT, TREE_BLOCK_REFS, TREE_CONTROL,
};
use super::tree::{KvTree, RootPtr, SmoContext};
use super::KvError;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
        cache: &Arc<NodeCache>,
        seq: &Arc<AtomicU64>,
        mint_alloc: &Arc<super::alloc_ext::ExtentAllocator>,
    ) -> Result<Routed, KvError> {
        let key = forest_key(kind, legacy)?;
        let slot = forest_key_slot(&key)?;
        let tree = match self.tree(slot) {
            Some(t) => t,
            None => self.mint(slot, cache, seq, mint_alloc).await?,
        };
        Ok(Routed { slot, tree, key })
    }

    /// Route a FOREST key (a journaled record's, a replayed record's, an
    /// interior separator's) to its slot tree, minting on first touch.
    pub async fn route_forest_key_or_mint(
        &self,
        key: &[u8],
        cache: &Arc<NodeCache>,
        seq: &Arc<AtomicU64>,
        mint_alloc: &Arc<super::alloc_ext::ExtentAllocator>,
    ) -> Result<(ForestSlot, Arc<KvTree>), KvError> {
        let slot = separator_slot(key)?;
        let tree = match self.tree(slot) {
            Some(t) => t,
            None => self.mint(slot, cache, seq, mint_alloc).await?,
        };
        Ok((slot, tree))
    }

    /// Route a `(kind, legacy key)` READ: `None` when the slot has no tree
    /// (nothing was ever written there — the read is a miss by
    /// construction, no extent minted for it).
    pub fn route_read(&self, kind: u8, legacy: &[u8]) -> Result<Option<Routed>, KvError> {
        let key = forest_key(kind, legacy)?;
        let slot = forest_key_slot(&key)?;
        Ok(self.tree(slot).map(|tree| Routed { slot, tree, key }))
    }

    async fn mint(
        &self,
        slot: ForestSlot,
        cache: &Arc<NodeCache>,
        seq: &Arc<AtomicU64>,
        mint_alloc: &Arc<super::alloc_ext::ExtentAllocator>,
    ) -> Result<Arc<KvTree>, KvError> {
        let _g = self.mint.lock().await;
        if let Some(t) = self.tree(slot) {
            return Ok(t); // the racing minter won
        }
        let mut ctx = SmoContext::new(Arc::clone(mint_alloc));
        let tree = Arc::new(
            KvTree::create_slot_tree(Arc::clone(cache), &mut ctx, slot, Arc::clone(seq)).await?,
        );
        let _ = self.guests.insert_sync(slot, Arc::clone(&tree));
        self.minted.fetch_add(1, Ordering::Relaxed);
        super::META_KV_FOREST_SLOT_TREES_MINTED.fetch_add(1, Ordering::Relaxed);
        Ok(tree)
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
        let (fstart, first_slot) = forest_bound(kind, start, false);
        let (fend, last_slot) = forest_bound(kind, end, true);
        for (slot, tree) in self.slot_trees() {
            if slot < first_slot || slot > last_slot {
                continue;
            }
            let mut cursor = fstart.clone();
            'tree: loop {
                // A page of the mixed leaf; over-fetch by the kind mix so
                // a directory's dentries do not starve behind its xattrs.
                let want = (max - out.len()).saturating_mul(2).max(64);
                let page = tree.range(&cursor, &fend, want).await?;
                let Some((last, _)) = page.last() else {
                    break 'tree;
                };
                cursor = super::node::key_successor(last);
                for (k, v) in &page {
                    if forest_key_kind(k)? != kind {
                        continue;
                    }
                    let (_, legacy) = split_forest_key(k)?;
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

/// Translate a LEGACY range bound of kind `kind` — exact-length key or a
/// short/long sentinel such as `[0]` / `KEY_SPACE_MAX` — into the forest
/// bound with the same cut, plus the slot it falls in. A bound of ≥ 8
/// bytes takes the kind byte at offset 8 exactly like a key (prefix
/// `0x06` for refs); a shorter one is a prefix of the key-ino bytes and
/// cuts the forest order at the same place verbatim. The slot is read
/// from the ino bytes present, padded with `0x00` for a start bound and
/// `0xFF` for an end bound (a refs bound too short to carry its owner
/// spans every slot).
fn forest_bound(kind: u8, legacy: &[u8], is_end: bool) -> (Vec<u8>, ForestSlot) {
    let pad = if is_end { 0xFF } else { 0x00 };
    let mut fb = Vec::with_capacity(legacy.len() + 1);
    let ino_off;
    if kind == TREE_BLOCK_REFS {
        fb.push(kind);
        fb.extend_from_slice(legacy);
        ino_off = 16; // owner ino inside the LEGACY refs key
    } else if legacy.len() >= 8 {
        fb.extend_from_slice(&legacy[..8]);
        fb.push(kind);
        fb.extend_from_slice(&legacy[8..]);
        ino_off = 0;
    } else {
        fb.extend_from_slice(legacy);
        ino_off = 0;
    }
    let mut ino = [pad; 8];
    let have = legacy.len().saturating_sub(ino_off).min(8);
    if have > 0 {
        ino[..have].copy_from_slice(&legacy[ino_off..ino_off + have]);
    }
    (
        fb,
        super::record::forest_slot_of_ino(u64::from_be_bytes(ino)),
    )
}

/// The slot an INTERIOR separator or content forest key routes to. A
/// separator of a slot tree is a real key of that tree (§5.2.1 — "routed
/// to its slot tree by the separator key's slot"), so the same arithmetic
/// applies; the by-block family reads its owner at offset 17.
pub fn separator_slot(key: &[u8]) -> Result<ForestSlot, KvError> {
    match key.first() {
        Some(&TREE_BLOCK_REFS) => {
            let off = 1 + 8 + 8;
            if key.len() < off + 8 {
                return Err(KvError::Corrupt(format!(
                    "forest by-block key of {} bytes carries no owner ino",
                    key.len()
                )));
            }
            Ok(super::record::forest_slot_of_ino(u64::from_be_bytes(
                key[off..off + 8].try_into().expect("length checked"),
            )))
        }
        Some(_) if key.len() >= 8 => Ok(super::record::forest_slot_of_ino(u64::from_be_bytes(
            key[..8].try_into().expect("length checked"),
        ))),
        _ => Err(KvError::Corrupt(format!(
            "forest key of {} bytes carries no key ino",
            key.len()
        ))),
    }
}
