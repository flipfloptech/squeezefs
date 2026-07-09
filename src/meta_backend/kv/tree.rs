//! The CoW btree over the node cache (PR K5): lookup / insert / delete /
//! range with the K1 fold, plus the **§4.6 SMO protocol** — splits
//! (including interior recursion), compactions, and writeback appends, all
//! executed on one serialized context.
//!
//! ## The §4.6 concurrency contract, implemented here
//!
//! - **All SMOs run serialized.** Every structure modification (writeback
//!   freeze, append, compact, split, root swap) takes `&mut`
//!   [`SmoContext`] — one per volume, `!Clone` — so SMO-vs-SMO races are
//!   unrepresentable at compile time. In K5 the "task" is whatever
//!   execution context owns the `SmoContext` (the tests drive it
//!   directly); PR K6b hands it to the real per-volume
//!   checkpoint/writeback task.
//! - **Interior locks are SMO-only ⇒ acyclic lock populations.** The
//!   commit path ([`KvTree::insert`] / [`KvTree::delete`] via
//!   [`KvTree::apply_at`]) locks **leaves only**; interior mutations
//!   (parent-pointer updates, new roots) happen exclusively inside the
//!   SMO's parent-then-child lock window. A deadlock cycle would need a
//!   commit-path task to hold a leaf and wait on an interior lock — which
//!   it structurally never does.
//! - **Snapshot-then-write writeback** (§4.6 pt 1): the node write lock is
//!   taken only to freeze the dirty delta (swap it out as an immutable
//!   bset image and open a fresh delta); the append runs **outside** the
//!   lock. Node locks never cover device I/O (§4.4 pt 1 / P1-10 analog).
//! - **Three-step node replacement** (§4.6): build successors from the
//!   frozen snapshot with **no locks held**; take parent-then-child write
//!   locks; move the delta that accumulated during the build into the
//!   successors (the bounded second merge), swap the cache mapping (old
//!   node marked *superseded*, snapshot left intact for in-flight
//!   readers), **assign the interior-pointer record seqs inside the lock
//!   window** (the §4.4 pt 2 reserve-inside-locks discipline — in K5 the
//!   seq source is a plain monotonic counter standing in for the K6b
//!   journal reservation), release. Old extents go to K4 pending-free
//!   (§4.7) — never reusable before their retiring seq is durable.
//! - **Writer lock-then-revalidate-then-retry**: a commit resolves its key
//!   to a leaf via the latch-free traversal, locks it, then revalidates
//!   under the lock — node not superseded, key within
//!   `[min_key, max_key]`. A failed revalidation unlocks, re-resolves, and
//!   retries (counted: [`super::META_KV_COMMIT_SMO_RETRIES`]). Reads
//!   retry the same way on a `child_node_seq` mismatch (§4.2).
//!
//! Keys are arbitrary memcmp-ordered byte strings below
//! [`KEY_SPACE_MAX`]; interior records map a child's inclusive `max_key`
//! separator to `(child_addr, child_seq)` (§4.2). Sibling ranges partition
//! the key space gap-free (`right.min = key_successor(left.max)` — the K2
//! split rule), so revalidation admits exactly the keys the separators
//! route.

use super::alloc_ext::ExtentAllocator;
use super::bset::{compact, BsetView};
use super::node::{
    key_successor, load_node, split_node, write_node, NodeWriteParams, SplitDest, BSET_FRAME_LEN,
    NODE_PAGE,
};
use super::node_cache::{CachedNode, LiveLookup, NodeCache, OwnedRec};
use super::record::{Record, RecordKind};
use super::KvError;
use arc_swap::ArcSwap;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Exclusive upper sentinel of the tree key space: greater (memcmp) than
/// any legal key. Real keys (K1: 8/16 B composites) must sort strictly
/// below it; [`KvTree::insert`] enforces this. Root nodes span
/// `["" ..= KEY_SPACE_MAX]`.
pub const KEY_SPACE_MAX: [u8; 32] = [0xFF; 32];

/// Bounded traversal / writer-retry budget: SMOs are rare and serialized,
/// so more than a handful of retries means a routing bug, failed loud.
const RETRY_BUDGET: usize = 256;

/// An interior record value: `(child_addr, child_seq)` (§4.2), 16 B LE.
pub fn encode_interior_value(child_addr: u64, child_seq: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(&child_addr.to_le_bytes());
    v.extend_from_slice(&child_seq.to_le_bytes());
    v
}

/// Decode an interior record value (§4.2).
pub fn decode_interior_value(v: &[u8]) -> Result<(u64, u64), KvError> {
    if v.len() != 16 {
        return Err(KvError::Corrupt(format!(
            "interior value must be 16 bytes, got {}",
            v.len()
        )));
    }
    Ok((
        u64::from_le_bytes(v[..8].try_into().expect("8-byte slice")),
        u64::from_le_bytes(v[8..].try_into().expect("8-byte slice")),
    ))
}

/// A tree root pointer: the ledger's `(node_addr, node_seq)` pair (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootPtr {
    pub addr: u64,
    pub seq: u64,
}

/// The serialized SMO execution context (§4.6): **one per volume**, owned
/// by whatever drives structure modifications — the tests in K5, the
/// checkpoint/writeback task in K6b. Passing it `&mut` into every SMO
/// entry point makes concurrent SMOs a compile error, which is the whole
/// "SMOs run on one task, one at a time" rule with the borrow checker as
/// the enforcer.
pub struct SmoContext {
    alloc: Arc<ExtentAllocator>,
}

impl SmoContext {
    pub fn new(alloc: Arc<ExtentAllocator>) -> Self {
        Self { alloc }
    }

    /// The extent allocator behind this volume's SMOs (§4.7).
    pub fn allocator(&self) -> &Arc<ExtentAllocator> {
        &self.alloc
    }
}

/// What [`KvTree::apply_at`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Records applied and snapshot swapped.
    Applied,
    /// Revalidation failed (the leaf was superseded, or the key left its
    /// range, between resolution and lock — §4.6): re-resolve and retry.
    Stale,
}

/// Counts of the maintenance work one [`KvTree::run_maintenance`] pass
/// performed (tests assert against these; the §10 global counters
/// aggregate across passes).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceOutcome {
    /// Frozen deltas appended in place (§4.6 pt 1).
    pub appends: u64,
    /// 1:1 CoW rewrites (log fold) — `meta_kv_node_compactions`.
    pub compactions: u64,
    /// Node splits (2-way and wider) — `meta_kv_node_splits`.
    pub splits: u64,
}

/// One logical btree (per `tree_id`) over a shared per-volume [`NodeCache`].
pub struct KvTree {
    tree_id: u8,
    cache: Arc<NodeCache>,
    root: ArcSwap<RootPtr>,
    /// Monotonic record-seq source — the K5 stand-in for the K6b journal
    /// reservation (§4.4 pt 2: assigned **inside** the node-lock window so
    /// per-key seq order equals RAM apply order). Shared across a volume's
    /// trees, like the journal head it stands in for.
    seq: Arc<AtomicU64>,
    /// Addresses whose open delta crossed the writeback threshold —
    /// drained by [`Self::run_maintenance`] (duplicates are benign).
    maintenance: scc::Queue<u64>,
}

impl std::fmt::Debug for KvTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvTree")
            .field("tree_id", &self.tree_id)
            .field("root", &self.root.load())
            .finish()
    }
}

impl KvTree {
    /// Format a fresh, empty tree: claim one extent (internal class — tree
    /// roots are checkpoint/format internals, §4.7), write an empty leaf
    /// root spanning the whole key space, publish it pinned.
    pub async fn create(
        cache: Arc<NodeCache>,
        ctx: &mut SmoContext,
        tree_id: u8,
        seq: Arc<AtomicU64>,
    ) -> Result<Self, KvError> {
        let extent = ctx.alloc.claim_internal()?;
        let addr = cache.extent_addr(extent);
        let node_seq = seq.fetch_add(1, Ordering::AcqRel) + 1;
        write_node(
            &cache.config().path,
            &cache.config().layout,
            &NodeWriteParams {
                node_addr: addr,
                node_seq,
                tree_id,
                level: 0,
                min_key: b"",
                max_key: &KEY_SPACE_MAX,
            },
            &[],
            0,
        )
        .await?;
        let loaded = load_node(
            &cache.config().path,
            &cache.config().layout,
            addr,
            cache.durable_tail(),
        )
        .await?;
        let node = CachedNode::from_loaded(loaded, true)?;
        cache.publish(node);
        Ok(Self {
            tree_id,
            cache,
            root: ArcSwap::from_pointee(RootPtr {
                addr,
                seq: node_seq,
            }),
            seq,
            maintenance: scc::Queue::default(),
        })
    }

    /// Re-attach to an existing tree from its root pointer (K6a's mount
    /// path: superblock → ledger → roots; tests re-open across cache
    /// drops). Pins the root.
    pub async fn open(
        cache: Arc<NodeCache>,
        tree_id: u8,
        root: RootPtr,
        seq: Arc<AtomicU64>,
    ) -> Result<Self, KvError> {
        let node = cache.get(root.addr).await?;
        if node.node_seq() != root.seq {
            return Err(KvError::Corrupt(format!(
                "root pointer stale: ledger says node_seq {}, extent {:#x} holds {}",
                root.seq,
                root.addr,
                node.node_seq()
            )));
        }
        if node.tree_id() != tree_id {
            return Err(KvError::Corrupt(format!(
                "root tree_id mismatch: expected {tree_id}, extent {:#x} holds {}",
                root.addr,
                node.tree_id()
            )));
        }
        node.pin();
        Ok(Self {
            tree_id,
            cache,
            root: ArcSwap::from_pointee(root),
            seq,
            maintenance: scc::Queue::default(),
        })
    }

    /// The current root pointer (what a K6b checkpoint names in its
    /// ledger record).
    pub fn root(&self) -> RootPtr {
        **self.root.load()
    }

    /// The tree id (§4.2).
    pub fn tree_id(&self) -> u8 {
        self.tree_id
    }

    /// The shared node cache.
    pub fn cache(&self) -> &Arc<NodeCache> {
        &self.cache
    }

    /// Root level: 0 = single-leaf tree, 1 = one interior level, …
    pub async fn root_level(&self) -> Result<u8, KvError> {
        Ok(self.cache.get(self.root().addr).await?.level())
    }

    /// Whether maintenance work is queued (writeback thresholds crossed).
    pub fn maintenance_pending(&self) -> bool {
        !self.maintenance.is_empty()
    }

    // -----------------------------------------------------------------
    // Latch-free traversal (§4.5 reads; §4.6 writer resolution).
    // -----------------------------------------------------------------

    /// Descend from the current root to the node of `target_level` whose
    /// range holds `key` — **latch-free**: every step reads an arc-swap
    /// snapshot; no node lock is ever taken. Stale-pointer detection is
    /// the §4.2 `child_node_seq` check plus the retired-extent refusal
    /// ([`NodeCache::load`]); both restart the walk from the (possibly
    /// swapped) root, bounded by [`RETRY_BUDGET`].
    async fn descend(&self, key: &[u8], target_level: u8) -> Result<Arc<CachedNode>, KvError> {
        'restart: for _ in 0..RETRY_BUDGET {
            let root = self.root();
            let Some(mut cur) = (match self.cache.try_get(root.addr) {
                Some(n) => Some(n),
                None => self.cache.load(root.addr).await?,
            }) else {
                continue 'restart; // root extent retired: racing root swap
            };
            if cur.node_seq() != root.seq {
                continue 'restart; // racing root swap
            }
            loop {
                if cur.level() == target_level {
                    return Ok(cur);
                }
                if cur.level() < target_level {
                    // v1 trees never shrink (no merges); a mid-walk root
                    // swap can still surface this — restart.
                    continue 'restart;
                }
                let snap = cur.snapshot();
                let Some((_, ptr)) = snap.next_live(key)? else {
                    // Routing hole: a stale snapshot raced an SMO — restart.
                    continue 'restart;
                };
                let (child_addr, child_seq) = decode_interior_value(&ptr)?;
                let child = match self.cache.try_get(child_addr) {
                    Some(n) => Some(n),
                    None => self.cache.load(child_addr).await?,
                };
                let Some(child) = child else {
                    continue 'restart; // retired extent: stale route
                };
                if child.node_seq() != child_seq {
                    continue 'restart; // §4.2 stale pointer
                }
                cur = child;
            }
        }
        Err(KvError::Corrupt(format!(
            "traversal retry budget exhausted descending to level {target_level} \
             (routing loop — SMO protocol bug)"
        )))
    }

    /// Resolve `key` to its leaf — the commit path's latch-free resolution
    /// step (§4.6). Public because it is one half of the
    /// resolve → lock-revalidate-apply writer protocol ([`Self::apply_at`]
    /// is the other) that K6b's multi-leaf transactions compose.
    pub async fn resolve_leaf(&self, key: &[u8]) -> Result<Arc<CachedNode>, KvError> {
        self.descend(key, 0).await
    }

    // -----------------------------------------------------------------
    // Point reads.
    // -----------------------------------------------------------------

    /// Latch-free point lookup: pinned-interior traverse + leaf snapshot
    /// fold (§4.5). `None` for tombstoned and never-written keys alike.
    pub async fn lookup(&self, key: &[u8]) -> Result<Option<Bytes>, KvError> {
        let leaf = self.resolve_leaf(key).await?;
        match leaf.snapshot().lookup(key)? {
            LiveLookup::Live(v) => Ok(Some(v)),
            LiveLookup::Tombstone | LiveLookup::Absent => Ok(None),
        }
    }

    // -----------------------------------------------------------------
    // The commit path (§4.4 / §4.6): resolve → lock → revalidate → apply.
    // -----------------------------------------------------------------

    /// Lock `leaf`, revalidate (§4.6: not superseded, key within
    /// `[min_key, max_key]`), and on success assign the record seq
    /// **inside the lock window** (§4.4 pt 2) and apply + snapshot-swap.
    /// [`ApplyOutcome::Stale`] means an SMO swapped the leaf between
    /// resolution and lock — counted in
    /// [`super::META_KV_COMMIT_SMO_RETRIES`]; the caller re-resolves and
    /// retries. Never performs I/O and never touches interior locks.
    pub async fn apply_at(
        &self,
        leaf: &Arc<CachedNode>,
        key: &[u8],
        kind: RecordKind,
        value: Bytes,
    ) -> Result<ApplyOutcome, KvError> {
        let mut guard = leaf.lock().write().await;
        if leaf.state().is_superseded() || key < leaf.min_key() || key > leaf.max_key() {
            drop(guard);
            super::META_KV_COMMIT_SMO_RETRIES.fetch_add(1, Ordering::Relaxed);
            return Ok(ApplyOutcome::Stale);
        }
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        leaf.apply_locked(
            &mut guard,
            vec![OwnedRec {
                key: Bytes::copy_from_slice(key),
                seq,
                kind,
                value,
            }],
        )?;
        let over_threshold = guard.overlay_bytes() >= self.cache.config().writeback_delta_bytes;
        drop(guard);
        if over_threshold {
            self.maintenance.push(leaf.addr());
        }
        Ok(ApplyOutcome::Applied)
    }

    fn check_key(&self, key: &[u8]) -> Result<(), KvError> {
        if key.is_empty() || key >= &KEY_SPACE_MAX[..] {
            return Err(KvError::Corrupt(format!(
                "tree key must be non-empty and sort below KEY_SPACE_MAX (len {})",
                key.len()
            )));
        }
        Ok(())
    }

    async fn mutate(&self, key: &[u8], kind: RecordKind, value: Bytes) -> Result<(), KvError> {
        self.check_key(key)?;
        for _ in 0..RETRY_BUDGET {
            let leaf = self.resolve_leaf(key).await?;
            match self.apply_at(&leaf, key, kind, value.clone()).await? {
                ApplyOutcome::Applied => return Ok(()),
                ApplyOutcome::Stale => continue,
            }
        }
        Err(KvError::Corrupt(
            "writer retry budget exhausted (revalidation never passed — SMO protocol bug)"
                .to_string(),
        ))
    }

    /// Insert / overwrite `key` (a `Put` record; §4.2 per-key LWW by seq).
    /// Values above the per-volume cap `min(65,536, node_size/4)` are
    /// refused typed ([`KvError::ValueTooLarge`]).
    pub async fn insert(&self, key: &[u8], value: impl Into<Bytes>) -> Result<(), KvError> {
        let value = value.into();
        let cap = self.cache.config().layout.record_value_cap();
        if value.len() > cap {
            return Err(KvError::ValueTooLarge {
                len: value.len(),
                cap,
            });
        }
        self.mutate(key, RecordKind::Put, value).await
    }

    /// Delete `key` (a `Delete` tombstone; unconditional — folding an
    /// absent key to a tombstone is legal and elided at compaction once
    /// durable, §4.2).
    pub async fn delete(&self, key: &[u8]) -> Result<(), KvError> {
        self.mutate(key, RecordKind::Delete, Bytes::new()).await
    }

    // -----------------------------------------------------------------
    // Range scans (readdir shape: bounded, resumable).
    // -----------------------------------------------------------------

    /// Collect up to `max` live records with `start ≤ key ≤ end`
    /// (memcmp order), fold-walked leaf by leaf over held snapshots —
    /// latch-free; concurrent SMOs at worst retry the *next* leaf's
    /// resolution. Resume by calling again with
    /// `start = key_successor(last returned key)`.
    pub async fn range(
        &self,
        start: &[u8],
        end: &[u8],
        max: usize,
    ) -> Result<Vec<(Bytes, Bytes)>, KvError> {
        let mut out: Vec<(Bytes, Bytes)> = Vec::new();
        if max == 0 || start > end {
            return Ok(out);
        }
        let mut cursor: Vec<u8> = start.to_vec();
        loop {
            let leaf = self.descend(&cursor, 0).await?;
            let snap = leaf.snapshot();
            let mut pos: Vec<u8> = cursor.clone();
            while out.len() < max {
                match snap.next_live(&pos)? {
                    Some((k, v)) if &k[..] <= end => {
                        pos = key_successor(&k);
                        out.push((k, v));
                    }
                    _ => break,
                }
            }
            if out.len() >= max || leaf.max_key() >= end {
                return Ok(out);
            }
            // Next leaf: the partition rule makes successor(max_key) the
            // right sibling's exact min_key.
            cursor = key_successor(leaf.max_key());
        }
    }

    // -----------------------------------------------------------------
    // Writeback + SMOs (§4.6) — the serialized context.
    // -----------------------------------------------------------------

    /// Drain the maintenance queue: freeze-and-append dirty deltas
    /// (§4.6 pt 1), compact full logs, split oversized folds — including
    /// interior recursion when parent pointer records push an interior
    /// node over its own thresholds. Serialized by `&mut SmoContext`.
    pub async fn run_maintenance(
        &self,
        ctx: &mut SmoContext,
    ) -> Result<MaintenanceOutcome, KvError> {
        let mut out = MaintenanceOutcome::default();
        while let Some(entry) = self.maintenance.pop() {
            let addr = **entry;
            self.maintain_node(ctx, addr, &mut out).await?;
        }
        Ok(out)
    }

    /// Force-writeback every dirty node of this tree regardless of
    /// threshold — the K6b checkpoint's flush shape; tests use it to make
    /// volumes reopenable ([`Self::open`]) from disk alone.
    pub async fn flush_dirty(&self, ctx: &mut SmoContext) -> Result<MaintenanceOutcome, KvError> {
        // Two passes bound interior recursion: leaf writebacks enqueue
        // parent pointer work; the queue drain inside run_maintenance
        // handles cascades within a pass.
        let mut out = MaintenanceOutcome::default();
        for _ in 0..RETRY_BUDGET {
            let mut dirty: Vec<u64> = Vec::new();
            self.cache_map_dirty_addrs(&mut dirty);
            if dirty.is_empty() && !self.maintenance_pending() {
                return Ok(out);
            }
            for addr in dirty {
                self.maintenance.push(addr);
            }
            let pass = self.run_maintenance(ctx).await?;
            out.appends += pass.appends;
            out.compactions += pass.compactions;
            out.splits += pass.splits;
        }
        Err(KvError::Corrupt(
            "flush_dirty never converged (writeback keeps re-dirtying — SMO bug)".to_string(),
        ))
    }

    fn cache_map_dirty_addrs(&self, out: &mut Vec<u64>) {
        self.cache.for_each_node(|node| {
            if node.tree_id() == self.tree_id
                && (node.state().is_dirty() || node.state().is_freezing())
                && !node.state().is_superseded()
            {
                out.push(node.addr());
            }
        });
    }

    /// Writeback / SMO for one node (§4.6): freeze under the lock, append
    /// outside it; a full log compacts; an oversized fold splits.
    async fn maintain_node(
        &self,
        ctx: &mut SmoContext,
        addr: u64,
        out: &mut MaintenanceOutcome,
    ) -> Result<(), KvError> {
        let Some(node) = self.cache.try_get(addr) else {
            return Ok(()); // evicted/retired since enqueue
        };
        if node.state().is_superseded() || node.tree_id() != self.tree_id {
            return Ok(());
        }
        // Phase 1 (§4.6 pt 1): freeze under the lock — RAM only.
        let frozen = {
            let mut guard = node.lock().write().await;
            node.freeze_locked(&mut guard, &self.cache.config().layout)?
        };
        if frozen.is_none() {
            return Ok(()); // spurious enqueue
        }
        // Phase 2: append outside the lock.
        if self.cache.append_frozen(&node).await? {
            out.appends += 1;
            // Re-enqueue if commits re-dirtied past the threshold while
            // the append was in flight.
            let guard = node.lock().read().await;
            let re = guard.overlay_bytes() >= self.cache.config().writeback_delta_bytes;
            drop(guard);
            if re {
                self.maintenance.push(addr);
            }
            return Ok(());
        }
        // Phase 3: the log is full — compact; oversized ⇒ split (§4.6 pt 1).
        self.smo_replace(ctx, &node, out).await
    }

    /// The §4.6 three-step node replacement. `node` is frozen (its
    /// unappended delta rides `extra_records`) and stays mapped until the
    /// swap below.
    async fn smo_replace(
        &self,
        ctx: &mut SmoContext,
        node: &Arc<CachedNode>,
        out: &mut MaintenanceOutcome,
    ) -> Result<(), KvError> {
        let cfg = self.cache.config();
        let layout = &cfg.layout;
        let durable_tail = self.cache.durable_tail();

        // ---- Step 1: build successors from the frozen snapshot, no locks.
        let src = load_node(&cfg.path, layout, node.addr(), durable_tail).await?;
        let extra: Vec<Record> = {
            let guard = node.lock().read().await;
            guard.frozen_records()
        };
        // Fold the whole log + frozen delta with THE K1 algebra (§4.2).
        let extra_image = if extra.is_empty() {
            Vec::new()
        } else {
            super::bset::build_bset(&extra, extra.iter().map(|r| r.seq).max().unwrap_or(0))?
        };
        let mut views: Vec<BsetView<'_>> = Vec::new();
        if !extra_image.is_empty() {
            views.push(BsetView::parse(&extra_image)?);
        }
        views.extend(src.bset_views_newest_first()?);
        let horizon = views
            .iter()
            .map(|v| v.journal_seq_horizon())
            .max()
            .unwrap_or(0);
        let folded = compact(&views, durable_tail)?;

        // Partition by encoded bytes. One part = compaction; more = split.
        // Split parts fill to ~3/4 so appends have headroom (§4.4 fill
        // accounting); a single-part rewrite may fill the node (it fit
        // before folding, folding only shrinks).
        let usable = layout.node_size() - NODE_PAGE - BSET_FRAME_LEN - super::bset::BSET_HEADER_LEN;
        let total: usize = folded.iter().map(|r| r.record_ref().encoded_len()).sum();
        let parts: Vec<&[Record]> = if total <= usable {
            vec![&folded[..]]
        } else {
            partition_records(&folded, usable * 3 / 4)
        };

        // Claim fresh extents (internal class, §4.7) + write images.
        let mut written: Vec<(u64, u64)> = Vec::new(); // (addr, node_seq)
        let claim = |ctx: &SmoContext, cache: &NodeCache| -> Result<u64, KvError> {
            Ok(cache.extent_addr(ctx.alloc.claim_internal()?))
        };
        if parts.len() == 1 {
            let dst = claim(ctx, &self.cache)?;
            let dst_seq = self.next_seq();
            super::node::compact_node(&cfg.path, layout, &src, &extra, dst, dst_seq, durable_tail)
                .await?;
            written.push((dst, dst_seq));
        } else if parts.len() == 2 {
            let (l, r) = (claim(ctx, &self.cache)?, claim(ctx, &self.cache)?);
            let (ls, rs) = (self.next_seq(), self.next_seq());
            split_node(
                &cfg.path,
                layout,
                &src,
                &extra,
                &SplitDest {
                    node_addr: l,
                    node_seq: ls,
                },
                &SplitDest {
                    node_addr: r,
                    node_seq: rs,
                },
                durable_tail,
            )
            .await?;
            written.push((l, ls));
            written.push((r, rs));
        } else {
            // Wider than two (a huge backlog folded at once): write each
            // partition directly with the same gap-free bounds rule.
            let mut min_key: Vec<u8> = node.min_key().to_vec();
            for (i, part) in parts.iter().enumerate() {
                let dst = claim(ctx, &self.cache)?;
                let dst_seq = self.next_seq();
                let max_key: Vec<u8> = if i + 1 == parts.len() {
                    node.max_key().to_vec()
                } else {
                    part[part.len() - 1].key.clone()
                };
                write_node(
                    &cfg.path,
                    layout,
                    &NodeWriteParams {
                        node_addr: dst,
                        node_seq: dst_seq,
                        tree_id: node.tree_id(),
                        level: node.level(),
                        min_key: &min_key,
                        max_key: &max_key,
                    },
                    part,
                    horizon,
                )
                .await?;
                written.push((dst, dst_seq));
                min_key = key_successor(&max_key);
            }
        }
        // Load-back the written successors (verifies the images; builds
        // the snapshots zero-copy from the fresh extents).
        let mut successors: Vec<Arc<CachedNode>> = Vec::with_capacity(written.len());
        for (dst, _) in &written {
            let loaded = load_node(&cfg.path, layout, *dst, durable_tail).await?;
            let pinned = node.level() > 0 || (self.is_root(node) && written.len() == 1);
            successors.push(CachedNode::from_loaded(loaded, pinned)?);
        }

        // A multi-way replacement of the root needs a new root above the
        // successors — written before any lock is taken.
        let new_root: Option<Arc<CachedNode>> = if self.is_root(node) && written.len() > 1 {
            let dst = claim(ctx, &self.cache)?;
            let dst_seq = self.next_seq();
            let recs: Vec<Record> = successors
                .iter()
                .map(|s| {
                    Record::put(
                        s.max_key().to_vec(),
                        self.next_seq(),
                        encode_interior_value(s.addr(), s.node_seq()),
                    )
                })
                .collect();
            let recs = sorted_by_key_seq(recs);
            write_node(
                &cfg.path,
                layout,
                &NodeWriteParams {
                    node_addr: dst,
                    node_seq: dst_seq,
                    tree_id: node.tree_id(),
                    level: node.level() + 1,
                    min_key: node.min_key(),
                    max_key: node.max_key(),
                },
                &recs,
                horizon,
            )
            .await?;
            let loaded = load_node(&cfg.path, layout, dst, durable_tail).await?;
            Some(CachedNode::from_loaded(loaded, true)?)
        } else {
            None
        };

        // ---- Step 2: parent-then-child locks; move the accumulated
        // delta; swap the mapping; assign pointer-record seqs INSIDE the
        // window; release. (§4.6 three-step replacement.)
        let parent = if self.is_root(node) {
            None
        } else {
            Some(self.resolve_parent(node).await?)
        };
        {
            let mut parent_guard = match &parent {
                Some(p) => Some(p.lock().write().await),
                None => None,
            };
            let mut child_guard = node.lock().write().await;

            // The bounded second merge: partition the delta that
            // accumulated during the build into the successors.
            let leftovers = child_guard.take_overlay();
            let outcome = node.state().supersede().map_err(|_| {
                KvError::Corrupt(format!(
                    "double supersede of node {:#x} (SMO serialization violated)",
                    node.addr()
                ))
            })?;
            debug_assert!(
                outcome.was_freezing,
                "SMO node must hold its frozen delta until the swap"
            );
            for succ in &successors {
                let mut sg = succ.lock().write().await;
                let mine: Vec<OwnedRec> = leftovers
                    .iter()
                    .filter(|r| &r.key[..] >= succ.min_key() && &r.key[..] <= succ.max_key())
                    .cloned()
                    .collect();
                if !mine.is_empty() {
                    succ.apply_locked(&mut sg, mine)?;
                }
            }

            // Swap the cache mapping: old object retired (readers keep
            // its snapshot), successors published.
            self.cache.retire(node);
            for succ in &successors {
                self.cache.publish(succ.clone());
            }
            if let Some(root) = &new_root {
                self.cache.publish(root.clone());
            }

            match (&parent, &mut parent_guard) {
                (None, _) => {
                    // Root replacement: swap the root pointer inside the
                    // lock window (traversals are latch-free; ArcSwap).
                    let (addr, seq) = match &new_root {
                        Some(r) => (r.addr(), r.node_seq()),
                        None => written[0],
                    };
                    self.root.store(Arc::new(RootPtr { addr, seq }));
                }
                (Some(parent), Some(pg)) => {
                    // Interior pointer records — the §4.4 pt 2 discipline:
                    // seqs assigned inside the lock window (K6b swaps in
                    // the real journal reservation), bytes written by the
                    // parent's own later writeback, outside these locks.
                    let recs: Vec<OwnedRec> = successors
                        .iter()
                        .map(|s| OwnedRec {
                            key: Bytes::copy_from_slice(s.max_key()),
                            seq: self.next_seq(),
                            kind: RecordKind::Put,
                            value: Bytes::from(encode_interior_value(s.addr(), s.node_seq())),
                        })
                        .collect();
                    parent.apply_locked(pg, recs)?;
                }
                (Some(_), None) => unreachable!("parent guard taken with parent"),
            }
            drop(child_guard);
            // parent_guard drops here.
        }
        node.state().end_freeze();

        // ---- Step 3: after release — old extent to pending-free (§4.7:
        // reusable only once the retiring seq is durable), parent
        // maintenance if its delta crossed the threshold.
        ctx.alloc
            .free_pending(self.cache.addr_extent(node.addr()), self.next_seq())?;
        if let Some(parent) = &parent {
            let pg = parent.lock().read().await;
            let re = pg.overlay_bytes() >= cfg.writeback_delta_bytes;
            drop(pg);
            if re {
                self.maintenance.push(parent.addr());
            }
        }

        if written.len() == 1 {
            super::META_KV_NODE_COMPACTIONS.fetch_add(1, Ordering::Relaxed);
            out.compactions += 1;
        } else {
            super::META_KV_NODE_SPLITS.fetch_add(1, Ordering::Relaxed);
            out.splits += 1;
        }
        Ok(())
    }

    fn is_root(&self, node: &Arc<CachedNode>) -> bool {
        self.root().addr == node.addr()
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Resolve `node`'s parent: descend to `node.level() + 1` routing by
    /// `node.min_key()` (the partition rule makes the child's own
    /// separator the first one ≥ its min). Stable during an SMO — SMOs
    /// are serialized, so no other task can swap interiors mid-walk.
    async fn resolve_parent(&self, node: &Arc<CachedNode>) -> Result<Arc<CachedNode>, KvError> {
        let parent = self.descend(node.min_key(), node.level() + 1).await?;
        Ok(parent)
    }
}

/// Greedy partition of key-ascending folded records into chunks of
/// ≤ `budget` encoded bytes (≥ 2 chunks, each non-empty).
fn partition_records(folded: &[Record], budget: usize) -> Vec<&[Record]> {
    let mut parts: Vec<&[Record]> = Vec::new();
    let mut start = 0usize;
    let mut acc = 0usize;
    for (i, r) in folded.iter().enumerate() {
        let len = r.record_ref().encoded_len();
        if acc + len > budget && i > start {
            parts.push(&folded[start..i]);
            start = i;
            acc = 0;
        }
        acc += len;
    }
    parts.push(&folded[start..]);
    parts
}

/// Sort records into the strict `(key, seq)` ascending order bset build
/// requires (root separator records are generated in child order, which is
/// already key order — this keeps the invariant explicit and cheap).
fn sorted_by_key_seq(mut recs: Vec<Record>) -> Vec<Record> {
    recs.sort_by(|a, b| a.key.cmp(&b.key).then(a.seq.cmp(&b.seq)));
    recs
}
