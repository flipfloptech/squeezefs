//! RAM-authoritative node cache (design §4.5): demand paging over the K2
//! node format and K4 extents, **latch-free reads** through arc-swap'd
//! immutable snapshots, clock eviction with dirty pinning and pinned
//! interior nodes, single-flight loads, and a byte budget knob.
//!
//! ## Shape (§4.5, normative)
//!
//! - [`NodeCache`] is an `scc::HashMap<node_addr, Arc<CachedNode>>`.
//! - [`CachedNode`] holds an **arc-swap'd immutable [`NodeSnapshot`]**
//!   (the merged-sorted record view over the on-disk bset list plus every
//!   frozen/open delta) and the mutable dirty delta guarded by the
//!   per-node `tokio::sync::RwLock` (§4.4 pt 1 — the lock covers RAM
//!   mutation only, never device I/O).
//! - **Reads are latch-free**: load the snapshot `Arc`, binary-search the
//!   merged index, hand out `Bytes` views into the snapshot's backing
//!   buffers — no lock, no copy. Writers swap a new snapshot after the RAM
//!   apply.
//! - **Demand paging**: a miss reads the whole extent with one
//!   `uring_fs::read_at` inside K2's [`load_node`] (header + bset
//!   verification + the §4.5 torn-tail classifier), then builds the
//!   snapshot. Loads are **single-flight** per node address (the
//!   `routing.rs` inflight-guard discipline: losers wait on a broadcast
//!   and re-check the map; a cancelled loader's drop guard wakes them).
//! - **Eviction**: a clock (FIFO + second-chance ref bit) over cache
//!   entries; clean nodes evict by dropping the `Arc` (in-flight readers
//!   keep their snapshot alive by refcount), **dirty/serializing nodes are
//!   pinned until writeback** ([`NodeState::try_evict`] refuses anything
//!   but the exact clean state), interior nodes and tree roots are pinned
//!   unconditionally. Budget: [`NodeCacheConfig::budget_bytes`] — the
//!   `SQUEEZEFS_META_NODE_CACHE_MB` knob arrives as a parameter here (CLI
//!   plumbing is PR K6a).
//!
//! The lifecycle bits behind eviction/supersede/freeze races are the
//! loom-modeled [`super::node_state_core`]; the tree logic (traversal,
//! writer revalidation, the §4.6 SMO protocol) is [`super::tree`].

use super::bset::BsetView;
use super::node::{
    encode_bset_frame, load_node, AppendDest, LoadedNode, NodeLayout, BSET_FRAME_LEN,
};
use super::node_state_core::NodeState;
use super::record::{
    fold_forward, fold_newest_first, Folded, FoldedHead, Record, RecordKind, RecordRef,
};
use super::KvError;
use arc_swap::ArcSwap;
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// The shipped node-cache budget (§3 / §4.5): 512 MiB — since the
/// 2026-08-04 derivation sweep this is the derived default's FLOOR
/// (never-regress-below-shipped): the live default is `max(budget/16,
/// 512 MiB)` of the resolved R5 memory budget
/// (`backend::resolve_node_cache_budget`; `SQUEEZEFS_META_NODE_CACHE_MB`
/// absolute / `SQUEEZEFS_META_NODE_CACHE_PCT` percentage win over it).
/// Tests plumb this constant directly as a deterministic budget.
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Default writeback threshold: freeze/append once a node's open delta
/// exceeds a bset worth (§4.6 pt 1 "when a node's dirty delta exceeds a
/// bset worth" — one 4 KiB append page).
pub const DEFAULT_WRITEBACK_DELTA_BYTES: usize = 4096;

/// PR M9 (design-metadata-throughput §5.7 D7.b): fixed per-node snapshot
/// fold-memo capacity — populate-once cells on the immutable snapshot,
/// first-come. Small and fixed by design ("the hot-parent-key case needs
/// 1"); the per-node memory bound the §5.7 budget accounting relies on.
pub const FOLD_MEMO_CAPACITY: usize = 8;

/// Placement + policy for one volume's node cache. Everything is a
/// parameter by design: the superblock/CLI plumbing is PR K6a's.
#[derive(Debug, Clone)]
pub struct NodeCacheConfig {
    /// The metadata volume (file or block device) holding the node heap.
    pub path: PathBuf,
    /// Validated node size (`--meta-node-kib` arrives in K6a).
    pub layout: NodeLayout,
    /// Byte offset of heap extent 0 (the superblock's `heap` pointer in
    /// K6a; tests place it directly).
    pub heap_base: u64,
    /// Cache budget in bytes (`SQUEEZEFS_META_NODE_CACHE_MB`); each cached
    /// node charges one extent (`node_size`).
    pub budget_bytes: u64,
    /// Open-delta size that triggers a writeback enqueue (§4.6 pt 1).
    pub writeback_delta_bytes: usize,
}

// ---------------------------------------------------------------------------
// The immutable snapshot: merged-sorted record view (§4.5).
// ---------------------------------------------------------------------------

/// One record's position in the merged index: which backing buffer and
/// where. 24 bytes packed; ~2,000-record leaves index in ~48 KiB.
#[derive(Debug, Clone, Copy)]
struct IdxRec {
    src: u16,
    kind: RecordKind,
    key_len: u16,
    key_off: u32,
    val_off: u32,
    val_len: u32,
    seq: u64,
}

/// The merged-sorted view over every durable/frozen record source of a
/// node: bset images (extent slices and frozen delta images) plus an index
/// ordered `(key asc, seq desc)` — the K1 fold algebra's input shape, so
/// one binary search + one fold serves point lookups (§4.5 "O(log 2,000)
/// memcmp on ~16 B keys").
struct RecordIndex {
    /// Backing bset images, oldest → newest. `Bytes` clones are refcounts:
    /// entries borrow these buffers zero-copy.
    sources: Vec<Bytes>,
    /// Merged records, `(key asc, seq desc, newer-source-first)`.
    entries: Vec<IdxRec>,
}

impl RecordIndex {
    /// Merge-build from bset images, **oldest → newest** (append order).
    /// Ties on `(key, seq)` order the newer source first — the
    /// [`super::bset::merge`] convention.
    fn build(sources: Vec<Bytes>) -> Result<Self, KvError> {
        let views: Vec<BsetView<'_>> = sources
            .iter()
            .map(|s| BsetView::parse(s))
            .collect::<Result<_, _>>()?;
        let total: usize = views.iter().map(|v| v.len()).sum();
        let mut entries = Vec::with_capacity(total);
        // K-way cursor merge over per-source (key asc, seq asc) views,
        // yielding (key asc, seq desc, newer source first) — the same
        // order bset::merge yields, tracked per source so entries can
        // reference their backing buffer.
        let mut cursors = vec![0usize; views.len()];
        loop {
            let mut best: Option<(usize, RecordRef<'_>)> = None;
            for si in (0..views.len()).rev() {
                // Newest source first so equal (key, seq) prefers it.
                if cursors[si] >= views[si].len() {
                    continue;
                }
                let r = views[si].record(cursors[si]);
                let better = match &best {
                    None => true,
                    Some((_, b)) => match r.key.cmp(b.key) {
                        std::cmp::Ordering::Less => true,
                        std::cmp::Ordering::Greater => false,
                        std::cmp::Ordering::Equal => r.seq > b.seq,
                    },
                };
                if better {
                    best = Some((si, r));
                }
            }
            let Some((si, r)) = best else { break };
            cursors[si] += 1;
            let base = sources[si].as_ptr() as usize;
            let key_off = r.key.as_ptr() as usize - base;
            let val_off = if r.value.is_empty() {
                0
            } else {
                r.value.as_ptr() as usize - base
            };
            debug_assert!(key_off + r.key.len() <= sources[si].len());
            entries.push(IdxRec {
                src: u16::try_from(si).map_err(|_| {
                    KvError::Corrupt(format!("record index with {} sources", sources.len()))
                })?,
                kind: r.kind,
                key_len: r.key.len() as u16,
                key_off: key_off as u32,
                val_off: val_off as u32,
                val_len: r.value.len() as u32,
                seq: r.seq,
            });
        }
        // Normalize each same-key run to `seq desc` (newer source breaking
        // exact ties). The cursor merge above orders records newest-first
        // ACROSS sources, but **within one source** a multi-seq key is
        // stored `seq asc` (bsets are `(key, seq)` ascending — a frozen
        // delta carrying `Put` then `Delete` of one key is the everyday
        // case), and the fold algebra's input contract is strictly
        // newest-first (§4.2).
        let key_of = |e: &IdxRec| {
            &sources[e.src as usize][e.key_off as usize..e.key_off as usize + e.key_len as usize]
        };
        let mut i = 0;
        while i < entries.len() {
            let mut j = i + 1;
            while j < entries.len() && key_of(&entries[j]) == key_of(&entries[i]) {
                j += 1;
            }
            if j - i > 1 {
                entries[i..j].sort_by(|a, b| b.seq.cmp(&a.seq).then(b.src.cmp(&a.src)));
            }
            i = j;
        }
        Ok(Self { sources, entries })
    }

    /// Extend the merged view with ONE newer bset image appended to
    /// `sources` — the freeze hot path (§4.6 pt 1). A two-way merge of
    /// the existing entries (already `(key asc, seq desc, src desc)`)
    /// with the new source's records: O(existing + new), never
    /// re-parsing the old images. `build` over all sources is a k-way
    /// cursor merge that scans every source per emitted entry — O(total
    /// × sources) with a full re-parse — which the K7 threshold-wake
    /// cadence turned into 37 % of the serial create path (one freeze
    /// per ~40 records instead of per tick).
    ///
    /// Exact-`(key, seq)` ties order the NEW source first (idempotent
    /// replay re-freezing a record that already reached a bset) — the
    /// `build`/`bset::merge` convention, by construction here since the
    /// new source has the highest `src`.
    fn extend_with(&self, new_source: Bytes) -> Result<Self, KvError> {
        let view = BsetView::parse(&new_source)?;
        let si = self.sources.len();
        let src = u16::try_from(si)
            .map_err(|_| KvError::Corrupt(format!("record index with {} sources", si + 1)))?;
        let mut sources = self.sources.clone(); // Bytes clones: refcounts only
        sources.push(new_source.clone());

        // The new view's entries in output order: bsets store a key's
        // records seq ASC — reverse every same-key run to seq DESC.
        let base = new_source.as_ptr() as usize;
        let mut fresh: Vec<IdxRec> = Vec::with_capacity(view.len());
        let mut i = 0usize;
        while i < view.len() {
            let mut j = i + 1;
            while j < view.len() && view.record(j).key == view.record(i).key {
                j += 1;
            }
            for k in (i..j).rev() {
                let r = view.record(k);
                let key_off = r.key.as_ptr() as usize - base;
                let val_off = if r.value.is_empty() {
                    0
                } else {
                    r.value.as_ptr() as usize - base
                };
                debug_assert!(key_off + r.key.len() <= new_source.len());
                fresh.push(IdxRec {
                    src,
                    kind: r.kind,
                    key_len: r.key.len() as u16,
                    key_off: key_off as u32,
                    val_off: val_off as u32,
                    val_len: r.value.len() as u32,
                    seq: r.seq,
                });
            }
            i = j;
        }

        // Linear merge on (key asc, seq desc, src desc).
        fn key_of<'a>(srcs: &'a [Bytes], e: &IdxRec) -> &'a [u8] {
            let s = &srcs[e.src as usize];
            &s[e.key_off as usize..e.key_off as usize + e.key_len as usize]
        }
        let mut entries = Vec::with_capacity(self.entries.len() + fresh.len());
        let (mut a, mut b) = (0usize, 0usize);
        while a < self.entries.len() && b < fresh.len() {
            let ea = &self.entries[a];
            let eb = &fresh[b];
            let take_a = match key_of(&sources, ea).cmp(key_of(&sources, eb)) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => ea.seq > eb.seq, // tie ⇒ new (higher src) first
            };
            if take_a {
                entries.push(*ea);
                a += 1;
            } else {
                entries.push(*eb);
                b += 1;
            }
        }
        entries.extend_from_slice(&self.entries[a..]);
        entries.extend_from_slice(&fresh[b..]);
        Ok(Self { sources, entries })
    }

    #[inline]
    fn key_at(&self, e: &IdxRec) -> &[u8] {
        &self.sources[e.src as usize][e.key_off as usize..e.key_off as usize + e.key_len as usize]
    }

    #[inline]
    fn value_at(&self, e: &IdxRec) -> &[u8] {
        &self.sources[e.src as usize][e.val_off as usize..e.val_off as usize + e.val_len as usize]
    }

    #[inline]
    fn record_ref(&self, i: usize) -> RecordRef<'_> {
        let e = &self.entries[i];
        RecordRef {
            key: self.key_at(e),
            seq: e.seq,
            kind: e.kind,
            value: self.value_at(e),
        }
    }

    /// `[lo, hi)` of entries whose key equals `key`.
    fn group_bounds(&self, key: &[u8]) -> std::ops::Range<usize> {
        let lo = self.entries.partition_point(|e| self.key_at(e) < key);
        let hi = self.entries.partition_point(|e| self.key_at(e) <= key);
        lo..hi
    }

    /// Index of the first entry with key ≥ `from`.
    fn first_at_or_after(&self, from: &[u8]) -> usize {
        self.entries.partition_point(|e| self.key_at(e) < from)
    }

    /// Zero-copy `Bytes` slice of entry `i`'s value.
    fn value_bytes(&self, i: usize) -> Bytes {
        let e = &self.entries[i];
        self.sources[e.src as usize]
            .slice(e.val_off as usize..e.val_off as usize + e.val_len as usize)
    }

    /// Zero-copy `Bytes` slice of entry `i`'s key.
    fn key_bytes(&self, i: usize) -> Bytes {
        let e = &self.entries[i];
        self.sources[e.src as usize]
            .slice(e.key_off as usize..e.key_off as usize + e.key_len as usize)
    }
}

/// An open-delta record: the RAM-applied twin of [`Record`] with `Bytes`
/// payloads so latch-free readers hand out refcounted views, never copies.
#[derive(Debug, Clone)]
pub struct OwnedRec {
    pub key: Bytes,
    pub seq: u64,
    pub kind: RecordKind,
    pub value: Bytes,
    /// PR M9 (§5.7 D7.a): the materialized folded head — `fold(this
    /// record ∪ everything older for the key)` — riding **the newest**
    /// overlay record of each key only ([`CachedNode::apply_locked`]
    /// clears the previous newest's head as it supersedes it; older
    /// records keep `None`). `None` also means "invalidated — fall back
    /// to the from-scratch fold" (mid-range rollback removal, §4.4 pt 4).
    /// Never serialized: the freeze path writes [`Self::to_record`],
    /// which drops it — on-disk economics unchanged.
    pub(crate) folded: Option<FoldedHead>,
    /// The record's own §4.6 pt 2 floor contribution — its containing
    /// journal entry's START seq, stamped by [`CachedNode::apply_locked`]
    /// from the batch floor (every apply site passes exactly that).
    /// Carried so an SMO's leftover-overlay transfer can give each
    /// successor the EXACT floor of the records it actually receives:
    /// the former predecessor-floor inheritance chained an ancient floor
    /// through every compaction of a continuously-written leaf — the
    /// tail never advanced, the second closer of the §4.7 pinned-floor
    /// wedge (P2 2026-07-26 §9; the first was the at-cap SMO refusal).
    /// RAM bookkeeping only: freeze serialization drops it.
    pub(crate) entry_floor: u64,
}

/// Fixed per-head charge beyond the owned value buffer (the enum + the
/// `Option` framing inside the record) — the §5.7 budget accounting's
/// overhead constant.
const FOLD_HEAD_OVERHEAD: usize = std::mem::size_of::<Option<FoldedHead>>();

impl OwnedRec {
    /// A fresh record entering [`CachedNode::apply_locked`] (the head is
    /// materialized there, under the node write lock).
    pub fn new(key: Bytes, seq: u64, kind: RecordKind, value: Bytes) -> Self {
        Self {
            key,
            seq,
            kind,
            value,
            folded: None,
            // Stamped by `apply_locked` (the batch floor); MAX = "no
            // contribution" until then.
            entry_floor: u64::MAX,
        }
    }

    /// Borrow as the fold algebra's view.
    pub fn record_ref(&self) -> RecordRef<'_> {
        RecordRef {
            key: &self.key,
            seq: self.seq,
            kind: self.kind,
            value: &self.value,
        }
    }

    /// The §5.7 budget charge of this record's head (0 when none).
    fn head_bytes(&self) -> usize {
        self.folded
            .as_ref()
            .map(|h| FOLD_HEAD_OVERHEAD + h.owned_bytes())
            .unwrap_or(0)
    }

    /// Copy into the K1 owned record (bset build / append input).
    fn to_record(&self) -> Record {
        Record {
            key: self.key.to_vec(),
            seq: self.seq,
            kind: self.kind,
            value: self.value.to_vec(),
        }
    }
}

/// The outcome of folding one key in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveLookup {
    /// The key folds to a live value (zero-copy view into the snapshot).
    Live(Bytes),
    /// A tombstone shadows the key — definitively absent in this node.
    Tombstone,
    /// No records for the key in this node.
    Absent,
}

impl LiveLookup {
    /// The head-serve projection (D7.a): a stored [`FoldedHead`] as the
    /// read path's outcome — `Bytes` clones are refcounts, zero-copy.
    fn from_head(h: &FoldedHead) -> Self {
        match h {
            FoldedHead::Live { value, .. } => LiveLookup::Live(value.clone()),
            FoldedHead::Tombstone => LiveLookup::Tombstone,
            FoldedHead::Absent => LiveLookup::Absent,
        }
    }
}

// ---------------------------------------------------------------------------
// The snapshot fold memo (PR M9, design-metadata-throughput §5.7 D7.b).
// ---------------------------------------------------------------------------

/// One populated memo cell: `key → (folded outcome, horizon seq)`.
/// `horizon` is the newest record seq the fold consumed — with memos
/// scoped to one immutable snapshot it is vacuously current, and the
/// debug assert on every hit *pins that immutability*: if snapshots ever
/// mutate in place, debug builds explode here instead of serving stale
/// folds.
#[derive(Debug)]
struct MemoCell {
    key: Bytes,
    look: LiveLookup,
    horizon: u64,
}

/// The bounded per-node fold memo living on the **immutable** arc-swap'd
/// snapshot (§5.7 D7.b): [`FOLD_MEMO_CAPACITY`] populate-once
/// `OnceLock` cells, first-come. Probes are latch-free (`OnceLock::get`
/// is an atomic load); populates race only on the same cell, arbitrated
/// by `OnceLock::set` (the loser re-checks and claims the next cell). The
/// memo is race-free by construction — every populate of one key computes
/// the same deterministic fold of the same immutable snapshot — and it
/// **dies with the snapshot at the next swap** (every RAM apply publishes
/// a fresh snapshot with an empty memo).
///
/// Memory: `bytes` tracks this memo's charge (keys + owned folded values
/// + a fixed per-cell overhead); populate adds it to both the global
/// `meta_kv_fold_memo_bytes` gauge and the owning cache's budget charge,
/// and [`Drop`] subtracts exactly what was added — the §5.7 accounting is
/// Drop-owned and leak-free.
struct FoldMemo {
    cells: [OnceLock<MemoCell>; FOLD_MEMO_CAPACITY],
    /// Bytes this memo has charged (subtracted on drop).
    bytes: AtomicU64,
    /// The owning cache's budget gauge (`NodeCache::cached_bytes`).
    charge: Arc<AtomicU64>,
}

/// Fixed per-cell charge beyond the key and any owned value buffer.
const MEMO_CELL_OVERHEAD: usize = std::mem::size_of::<MemoCell>();

impl FoldMemo {
    fn new(charge: Arc<AtomicU64>) -> Self {
        Self {
            cells: std::array::from_fn(|_| OnceLock::new()),
            bytes: AtomicU64::new(0),
            charge,
        }
    }

    /// Latch-free probe: scan the (≤ [`FOLD_MEMO_CAPACITY`]) populated
    /// cells for `key`. Cells may populate out of order under racing
    /// claims, so every slot is inspected — 8 `Bytes` compares on 8–16 B
    /// keys, no decodes, no locks.
    fn probe(&self, key: &[u8]) -> Option<&MemoCell> {
        self.cells
            .iter()
            .filter_map(|c| c.get())
            .find(|c| c.key[..] == *key)
    }

    /// Populate-once claim: first empty cell wins; a same-key racer's
    /// duplicate is prevented by the post-loss re-check (both computed
    /// byte-identical folds — immutability — so even the unreachable
    /// duplicate would be benign). A full memo drops the entry — the
    /// fixed capacity IS the §5.7 per-node memory bound.
    fn populate(&self, key: Bytes, look: LiveLookup, horizon: u64) {
        let charge = key.len()
            + MEMO_CELL_OVERHEAD
            + match &look {
                // The owned-fold case charges its buffer; plain-Put /
                // tombstone outcomes refcount snapshot memory the node
                // already pays for.
                LiveLookup::Live(v) => v.len(),
                LiveLookup::Tombstone | LiveLookup::Absent => 0,
            };
        let mut cell = MemoCell { key, look, horizon };
        for slot in &self.cells {
            match slot.set(cell) {
                Ok(()) => {
                    let charge = charge as u64;
                    self.bytes.fetch_add(charge, Ordering::AcqRel);
                    self.charge.fetch_add(charge, Ordering::AcqRel);
                    super::META_KV_FOLD_MEMO_BYTES.fetch_add(charge, Ordering::AcqRel);
                    return;
                }
                Err(back) => {
                    // Lost the claim: if the winner memoized OUR key,
                    // we're done; otherwise try the next cell.
                    if slot.get().is_some_and(|c| c.key == back.key) {
                        return;
                    }
                    cell = back;
                }
            }
        }
    }
}

impl Drop for FoldMemo {
    fn drop(&mut self) {
        let bytes = self.bytes.load(Ordering::Acquire);
        if bytes > 0 {
            self.charge.fetch_sub(bytes, Ordering::AcqRel);
            super::META_KV_FOLD_MEMO_BYTES.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

impl std::fmt::Debug for FoldMemo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FoldMemo")
            .field(
                "populated",
                &self.cells.iter().filter(|c| c.get().is_some()).count(),
            )
            .field("bytes", &self.bytes.load(Ordering::Relaxed))
            .finish()
    }
}

/// Published open-delta records newer than the last `stable` re-merge:
/// the per-apply snapshot publish clones only this run, so it stays
/// small by construction (PR K7: the whole-overlay clone per commit was
/// ~44 % of the serial create path — Bytes refcount storms on clone AND
/// on the previous snapshot's drop).
const OVERLAY_TAIL_MAX: usize = 8;

/// The immutable, latch-free read view of one node (§4.5): a merged
/// record index over every serialized source plus the open-delta overlay.
/// Readers load the `Arc`, search, and hand out `Bytes` — no lock, no
/// copy; a writer swaps a whole new snapshot after its RAM apply.
pub struct NodeSnapshot {
    base: Arc<RecordIndex>,
    /// Open-delta records, two key-sorted runs. `stable` is Arc-SHARED
    /// across publishes (O(1) per apply); `tail` holds the ≤
    /// [`OVERLAY_TAIL_MAX`] records applied since the last re-merge and
    /// is the only per-apply clone. Fold precedence: per key, every
    /// `tail` record is at least as new as every `stable` record, and
    /// both are newer than `base` — enforced by the monotonic per-node
    /// seq mint, with the apply path forcing a re-merge on any observed
    /// seq inversion (replayed original seqs at mount).
    stable: Arc<Vec<OwnedRec>>,
    tail: Arc<Vec<OwnedRec>>,
    /// PR M9 (§5.7 D7.b): the bounded populate-once fold memo for
    /// bset-resident keys. Fresh (empty) on every snapshot publish; dies
    /// with this snapshot — see [`FoldMemo`].
    memo: FoldMemo,
}

impl NodeSnapshot {
    /// Records in the open-delta overlay (tests / writeback sizing).
    pub fn overlay_len(&self) -> usize {
        self.stable.len() + self.tail.len()
    }

    /// Total indexed records across serialized sources (not folded).
    pub fn indexed_len(&self) -> usize {
        self.base.entries.len()
    }

    /// PR VL7 (design-volume-lifecycle §5.7 D4): the dead-record census of
    /// this snapshot's SERIALIZED sources — `(total indexed records,
    /// distinct keys)`. A key's group keeps exactly one live head after a
    /// compaction fold, so `total − distinct` is the superseded ("dead
    /// bset") record population a nudge can reclaim. Entries are
    /// key-ascending by the index invariant: one linear pass.
    pub fn indexed_record_census(&self) -> (u64, u64) {
        let total = self.base.entries.len() as u64;
        let mut distinct = 0u64;
        let mut prev: Option<&[u8]> = None;
        for e in self.base.entries.iter() {
            let k = self.base.key_at(e);
            if prev != Some(k) {
                distinct += 1;
                prev = Some(k);
            }
        }
        (total, distinct)
    }

    fn run_group(run: &[OwnedRec], key: &[u8]) -> std::ops::Range<usize> {
        let lo = run.partition_point(|r| &r.key[..] < key);
        let hi = run.partition_point(|r| &r.key[..] <= key);
        lo..hi
    }

    /// The newest seq this node holds for `key` across every source —
    /// overlay runs AND the on-disk base bsets — or `None` when the key is
    /// unknown here. Order-independent by construction (max per group), so
    /// it is safe to consult even mid-replay, before the fold-order
    /// invariant is re-established. Used by mount replay's per-key LWW
    /// gate: a replayed window record whose seq is ≤ this is ALREADY
    /// materialized in the node (an old-ledger mount after a mid-checkpoint
    /// kill reads bsets that cover part of the replay window) and must not
    /// be re-applied — appending it would sort the overlay BELOW the base
    /// for that key and break the newest-first fold order (stale-value
    /// LWW).
    pub fn newest_seq_of(&self, key: &[u8]) -> Option<u64> {
        let mut newest: Option<u64> = None;
        let tg = Self::run_group(&self.tail, key);
        if !tg.is_empty() {
            // Runs are (key, seq) ascending: the group's last is its newest.
            newest = Some(self.tail[tg.end - 1].seq);
        }
        let sg = Self::run_group(&self.stable, key);
        if !sg.is_empty() {
            let s = self.stable[sg.end - 1].seq;
            newest = Some(newest.map_or(s, |n| n.max(s)));
        }
        let bg = self.base.group_bounds(key);
        if !bg.is_empty() {
            // Base groups are (seq desc): the group's first is its newest.
            let b = self.base.record_ref(bg.start).seq;
            newest = Some(newest.map_or(b, |n| n.max(b)));
        }
        newest
    }

    /// Fold one key — **the D7 slimmed read path** (PR M9, §5.7):
    ///
    /// 1. **Overlay head serve (D7.a)**: the newest open-delta record of
    ///    the key carries a materialized folded head (kept current at
    ///    apply time under the node write lock) — zero record decodes.
    /// 2. **Memo serve (D7.b)**: an overlay-absent (bset-resident) key
    ///    probes this snapshot's populate-once memo cells — latch-free,
    ///    zero decodes.
    /// 3. **From-scratch fold**: the single K1 algebra over the chained
    ///    gather (no per-fold allocation — PR K7), exactly as before D7;
    ///    memo-eligible outcomes populate a cell for the next read.
    ///
    /// The fold FUNCTION is untouched (design-cow-kv-metadata §4.2); the
    /// head/memo paths only change *when* it runs — pinned equivalent by
    /// the R7 proptest guard (`tests/kv_fold_slimming_tests.rs`).
    pub fn lookup(&self, key: &[u8]) -> Result<LiveLookup, KvError> {
        let tg = Self::run_group(&self.tail, key);
        let sg = Self::run_group(&self.stable, key);

        // (1) D7.a: the newest overlay record for the key (tail ≥ stable
        // per key — the run-precedence invariant above).
        let newest_overlay = if !tg.is_empty() {
            Some(&self.tail[tg.end - 1])
        } else if !sg.is_empty() {
            Some(&self.stable[sg.end - 1])
        } else {
            None
        };
        if let Some(newest) = newest_overlay {
            if let Some(head) = &newest.folded {
                super::META_KV_FOLD_HEAD_SERVES.fetch_add(1, Ordering::Relaxed);
                return Ok(LiveLookup::from_head(head));
            }
        } else {
            // (2) D7.b: bset-resident key — probe the snapshot memo.
            if let Some(cell) = self.memo.probe(key) {
                super::META_KV_FOLD_MEMO_HITS.fetch_add(1, Ordering::Relaxed);
                // Immutability pin: the memoized horizon must still be
                // this snapshot's newest seq for the key (see MemoCell).
                debug_assert_eq!(
                    self.newest_seq_of(key),
                    Some(cell.horizon),
                    "snapshot mutated under a memo (immutability violated)"
                );
                return Ok(cell.look.clone());
            }
        }

        // (3) From-scratch fold (head invalidated, or first fold of a
        // bset-resident key).
        let bg = self.base.group_bounds(key);
        let gather = tg
            .clone()
            .rev()
            .map(|i| self.tail[i].record_ref())
            .chain(sg.clone().rev().map(|i| self.stable[i].record_ref()))
            .chain(bg.clone().map(|i| self.base.record_ref(i)));
        // Memo scope (§5.7 D7.b: "for bset-resident DELTAS"): a fold that
        // had to decode-and-apply deltas returns `Cow::Owned` — exactly
        // the re-decode tax the memo exists to kill. Plain-`Put` folds
        // (borrowed, zero decodes), tombstone scans, no-record probes
        // (the create storm's ENOENT dentry lookups), and orphan-absents
        // are already free — populating them would burn the fixed cells,
        // pay a per-lookup key alloc, and (measured) tax the distinct-key
        // hot-lookup path for zero possible win.
        let mut delta_materialized = false;
        let look = match fold_newest_first(gather)? {
            Folded::Absent => LiveLookup::Absent,
            Folded::Tombstone { .. } => LiveLookup::Tombstone,
            Folded::Put { value, .. } => LiveLookup::Live(match value {
                std::borrow::Cow::Owned(v) => {
                    delta_materialized = true;
                    Bytes::from(v)
                }
                std::borrow::Cow::Borrowed(v) => {
                    self.materialize(tg, sg, bg.clone(), v).ok_or_else(|| {
                        KvError::Corrupt(
                            "folded borrow does not match any gathered record".to_string(),
                        )
                    })?
                }
            }),
        };
        if newest_overlay.is_none() && delta_materialized {
            // Misses count exactly where a populate follows, so
            // hits/(hits+misses) reads as the D7.b effectiveness rate.
            super::META_KV_FOLD_MEMO_MISSES.fetch_add(1, Ordering::Relaxed);
            let horizon = self.base.entries[bg.start].seq;
            self.memo
                .populate(Bytes::copy_from_slice(key), look.clone(), horizon);
        }
        Ok(look)
    }

    /// **DUR-8b** — how many `Delta` records are stacked above the key's
    /// newest base (`Put`/`Delete`) in THIS snapshot, i.e. the DURABLE
    /// chain depth a fold would have to apply. The publish path's chain
    /// cap was a RAM-only counter that a metadata-cache refill reset to
    /// zero, so the durable chain was bounded only by node compaction —
    /// not by the knob that claims to bound it.
    ///
    /// Same gather as [`Self::lookup`], newest-first, counting only
    /// (never decoding): the deltas above the base are exactly the chain.
    pub fn delta_depth(&self, key: &[u8]) -> u32 {
        let tg = Self::run_group(&self.tail, key);
        let sg = Self::run_group(&self.stable, key);
        let bg = self.base.group_bounds(key);
        let gather = tg
            .clone()
            .rev()
            .map(|i| self.tail[i].record_ref())
            .chain(sg.clone().rev().map(|i| self.stable[i].record_ref()))
            .chain(bg.map(|i| self.base.record_ref(i)));
        let mut depth = 0u32;
        for r in gather {
            match r.kind {
                super::record::RecordKind::Delta => depth += 1,
                _ => break,
            }
        }
        depth
    }

    /// Map a fold's borrowed value back to its provider for a zero-copy
    /// `Bytes` (the borrow is always one gathered record's value slice;
    /// the groups are re-walked by pointer — bounded by the chain length).
    fn materialize(
        &self,
        tg: std::ops::Range<usize>,
        sg: std::ops::Range<usize>,
        bg: std::ops::Range<usize>,
        v: &[u8],
    ) -> Option<Bytes> {
        let hit = |val: &[u8]| std::ptr::eq(val.as_ptr(), v.as_ptr()) && val.len() == v.len();
        for i in tg {
            if hit(&self.tail[i].value) {
                return Some(self.tail[i].value.clone());
            }
        }
        for i in sg {
            if hit(&self.stable[i].value) {
                return Some(self.stable[i].value.clone());
            }
        }
        for i in bg {
            if hit(self.base.value_at(&self.base.entries[i])) {
                return Some(self.base.value_bytes(i));
            }
        }
        None
    }

    /// The newest record seq present for `key` across the overlay runs
    /// and every serialized source, or `None` when the key has no records
    /// — the §4.4 pt 4 rollback's seq-conditional probe ("rollback
    /// restores a key only if its newest dirty record still bears this
    /// tx's seq").
    pub fn newest_record_seq(&self, key: &[u8]) -> Option<u64> {
        let tg = Self::run_group(&self.tail, key);
        let sg = Self::run_group(&self.stable, key);
        let tail_newest = tg.clone().next_back().map(|i| self.tail[i].seq);
        let stable_newest = sg.clone().next_back().map(|i| self.stable[i].seq);
        let bg = self.base.group_bounds(key);
        // Base groups are (seq desc): the first entry is the newest.
        let base_newest = (!bg.is_empty()).then(|| self.base.entries[bg.start].seq);
        match (tail_newest, stable_newest, base_newest) {
            (None, None, None) => None,
            (a, b, c) => Some(a.unwrap_or(0).max(b.unwrap_or(0)).max(c.unwrap_or(0))),
        }
    }

    /// The next **live** `(key, value)` with key ≥ `from` (fold-walked:
    /// tombstoned/orphaned keys are skipped) — the range-scan / interior
    /// routing primitive. Zero-copy on both key and value.
    ///
    /// `end_inclusive` bounds the walk: a candidate key beyond it returns
    /// `None` **before** any fold. Without the bound a narrow window scan
    /// (the ≤ 256-key dentry/xattr chain probes behind every
    /// lookup/create/unlink) would fold-walk the whole tombstone desert a
    /// create/unlink storm leaves in the leaf until compaction — the
    /// ~1000× post-storm lookup cliff the K7 §8 micro gate caught
    /// (`tests/kv_scale_tests.rs::lookup_p50_immune_to_tombstone_desert`).
    /// `None` leaves the walk unbounded (interior routing wants the next
    /// live separator wherever it is).
    pub fn next_live(
        &self,
        from: &[u8],
        end_inclusive: Option<&[u8]>,
    ) -> Result<Option<(Bytes, Bytes)>, KvError> {
        let mut cursor: Vec<u8> = from.to_vec();
        loop {
            let bi = self.base.first_at_or_after(&cursor);
            let si = self.stable.partition_point(|r| &r.key[..] < &cursor[..]);
            let ti = self.tail.partition_point(|r| &r.key[..] < &cursor[..]);
            let bk =
                (bi < self.base.entries.len()).then(|| self.base.key_at(&self.base.entries[bi]));
            let sk = (si < self.stable.len()).then(|| &self.stable[si].key[..]);
            let tk = (ti < self.tail.len()).then(|| &self.tail[ti].key[..]);
            // Minimum candidate key across the three sorted sources.
            let mut key: Option<&[u8]> = bk;
            for cand in [sk, tk] {
                key = match (key, cand) {
                    (None, c) => c,
                    (k, None) => k,
                    (Some(k), Some(c)) => Some(if c < k { c } else { k }),
                };
            }
            let Some(key) = key else {
                return Ok(None);
            };
            if let Some(end) = end_inclusive {
                if key > end {
                    return Ok(None);
                }
            }
            let key_bytes = if bk == Some(key) {
                self.base.key_bytes(bi)
            } else if sk == Some(key) {
                self.stable[si].key.clone()
            } else {
                self.tail[ti].key.clone()
            };
            match self.lookup(key_bytes.as_ref())? {
                LiveLookup::Live(v) => return Ok(Some((key_bytes, v))),
                LiveLookup::Tombstone | LiveLookup::Absent => {
                    // Advance past this key: successor = key ⧺ 0x00.
                    cursor.clear();
                    cursor.extend_from_slice(&key_bytes);
                    cursor.push(0);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The cached node.
// ---------------------------------------------------------------------------

/// The mutable half of a node, guarded by the per-node write lock
/// (§4.4 pt 1: locks cover RAM mutation only — the append itself runs
/// outside, on the serialized writeback/SMO task).
pub struct NodeDirty {
    /// Open-delta records, `(key asc, seq asc)` — the AUTHORITATIVE
    /// merged view (freeze, SMO take-over, and rollback all operate on
    /// it, exactly as before the two-run publish split below).
    overlay: Vec<OwnedRec>,
    /// Encoded size of `overlay` (writeback threshold input).
    overlay_bytes: usize,
    /// PR M9 (§5.7): bytes of materialized folded heads riding `overlay`
    /// records (the D7.a overlay-head charge — maintained alongside
    /// `overlay_bytes` under the same lock; no second synchronization
    /// regime, per risk R7's mitigation).
    head_bytes: usize,
    /// Publish mirrors of `overlay`, split so the per-apply snapshot swap
    /// clones only a bounded tail (PR K7): `merge(snap_stable, snap_tail)
    /// == overlay` at every publish point. `snap_stable` is Arc-shared
    /// with published snapshots; `snap_tail` holds the ≤
    /// [`OVERLAY_TAIL_MAX`] records applied since the last re-merge.
    snap_stable: Arc<Vec<OwnedRec>>,
    snap_tail: Vec<OwnedRec>,
    /// Highest record seq ever applied to this node — the run-precedence
    /// guard: an apply carrying a seq at-or-below it (mount replay's
    /// original seqs) forces a stable re-merge, keeping "tail ≥ stable
    /// per key" true by construction everywhere else.
    max_applied_seq: u64,
    /// A frozen delta not yet appended: `(records, bset image)` — the bset
    /// image is already merged into the snapshot base; the records are the
    /// append/compact input (§4.6 pt 1).
    frozen: Option<FrozenDelta>,
    /// Node-relative offset of the unwritten tail (advances per append).
    tail_offset: usize,
    /// PR M9 (§5.7): what this open delta currently contributes to the
    /// owning cache's budget charge (`overlay_bytes + head_bytes` as of
    /// the last [`Self::resync_charge`]) — diffed on every locked
    /// mutation, subtracted exactly on drop.
    charged: u64,
    /// The owning cache's budget gauge (`NodeCache::cached_bytes`).
    charge: Arc<AtomicU64>,
}

/// A frozen-but-unwritten delta (§4.6 pt 1 snapshot-then-write).
#[derive(Clone)]
pub struct FrozenDelta {
    records: Arc<Vec<Record>>,
    /// Max record seq — the bset `journal_seq_horizon`.
    horizon: u64,
    /// Encoded frame length (fit check before the append attempt).
    frame_len: usize,
}

impl NodeDirty {
    /// Encoded bytes currently in the open delta.
    pub fn overlay_bytes(&self) -> usize {
        self.overlay_bytes
    }

    /// Records currently in the open delta.
    pub fn overlay_len(&self) -> usize {
        self.overlay.len()
    }

    /// Whether a frozen delta awaits its append/compact.
    pub fn has_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    /// The frozen-but-unwritten delta records (the SMO's `extra_records`
    /// fold input — §4.6). Empty when no freeze is outstanding.
    pub fn frozen_records(&self) -> Vec<Record> {
        self.frozen
            .as_ref()
            .map(|f| f.records.as_ref().clone())
            .unwrap_or_default()
    }

    /// Take the open delta (the §4.6 "delta that accumulated during the
    /// build") — SMO-only, under the child's write lock; the records move
    /// into the successors' open deltas (their heads ride along: the
    /// successor base folds identically for every moved key — the K1
    /// compaction theorem — so a head valid here is valid there). The
    /// budget charge moves with them: released here, re-charged by the
    /// successor's `apply_locked`. Published snapshots are immutable and
    /// keep serving the pre-swap view (the mirrors are cleared but no new
    /// snapshot is published here — §4.6: "snapshot left intact").
    pub fn take_overlay(&mut self) -> Vec<OwnedRec> {
        self.overlay_bytes = 0;
        self.head_bytes = 0;
        self.snap_stable = Arc::new(Vec::new());
        self.snap_tail.clear();
        let out = std::mem::take(&mut self.overlay);
        self.resync_charge();
        out
    }

    /// Node-relative unwritten-tail offset.
    pub fn tail_offset(&self) -> usize {
        self.tail_offset
    }

    /// PR M9 (§5.7): bring the cache budget gauge in line with this open
    /// delta's current bytes (`overlay + heads`). Called at the end of
    /// every locked mutation; drop subtracts the residue exactly.
    fn resync_charge(&mut self) {
        let new = (self.overlay_bytes + self.head_bytes) as u64;
        match new.cmp(&self.charged) {
            std::cmp::Ordering::Greater => {
                self.charge.fetch_add(new - self.charged, Ordering::AcqRel);
            }
            std::cmp::Ordering::Less => {
                self.charge.fetch_sub(self.charged - new, Ordering::AcqRel);
            }
            std::cmp::Ordering::Equal => {}
        }
        self.charged = new;
    }
}

impl Drop for NodeDirty {
    fn drop(&mut self) {
        // §5.7 accounting is Drop-owned: whatever this open delta still
        // charges leaves the gauge with it (eviction drops clean nodes —
        // charge 0; unmount / supersede-replacement drop the object with
        // whatever residue remains).
        if self.charged > 0 {
            self.charge.fetch_sub(self.charged, Ordering::AcqRel);
        }
    }
}

/// One cached node: immutable identity + lifecycle word + arc-swap'd
/// snapshot + the lock-guarded dirty half (§4.5).
pub struct CachedNode {
    addr: u64,
    node_seq: u64,
    tree_id: u8,
    level: u8,
    min_key: Vec<u8>,
    max_key: Vec<u8>,
    state: NodeState,
    snapshot: ArcSwap<NodeSnapshot>,
    dirty: tokio::sync::RwLock<NodeDirty>,
    /// Clock second-chance bit (set on access).
    ref_bit: AtomicBool,
    /// Interior nodes and tree roots never evict (§4.5).
    pinned: AtomicBool,
    /// §4.6 pt 2 `oldest_dirty_seq`: the smallest record seq applied to
    /// this node that is not yet in a **durable-covered** bset
    /// (`u64::MAX` = none). Maintained by [`Self::apply_locked`]; the
    /// checkpoint task swaps it out per flush pass and restores it if the
    /// pass fails — the tail rule takes the min over these floors.
    dirty_floor: AtomicU64,
    /// PR M9 (§5.7): the owning cache's budget gauge — handed to every
    /// snapshot's [`FoldMemo`] and the open delta's charge accounting so
    /// a node's charged size is `extent + overlay + memo` bytes.
    charge: Arc<AtomicU64>,
}

impl std::fmt::Debug for CachedNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedNode")
            .field("addr", &self.addr)
            .field("node_seq", &self.node_seq)
            .field("tree_id", &self.tree_id)
            .field("level", &self.level)
            .field("state", &self.state.state())
            .field("pinned", &self.pinned.load(Ordering::Relaxed))
            .finish()
    }
}

impl CachedNode {
    /// Build from a K2 [`LoadedNode`] (demand paging or an SMO successor
    /// load), taking ownership of the verified extent buffer zero-copy.
    /// Born clean; an SMO successor inherits the displaced open delta via
    /// [`Self::apply_locked`] under the SMO's lock window (the §4.6
    /// "bounded second merge"). `charge` is the owning cache's budget
    /// gauge ([`NodeCache::cached_bytes`] — §5.7: overlay + memo bytes
    /// ride the node-cache budget).
    pub fn from_loaded(
        loaded: LoadedNode,
        pinned: bool,
        charge: Arc<AtomicU64>,
    ) -> Result<Arc<Self>, KvError> {
        let (header, buf, bset_ranges, tail_offset) = loaded.into_parts();
        let sources: Vec<Bytes> = bset_ranges.iter().map(|r| buf.slice(r.clone())).collect();
        let base = Arc::new(RecordIndex::build(sources)?);
        Ok(Arc::new(Self {
            addr: header.node_addr,
            node_seq: header.node_seq,
            tree_id: header.tree_id,
            level: header.level,
            min_key: header.min_key,
            max_key: header.max_key,
            state: NodeState::new(),
            snapshot: ArcSwap::from_pointee(NodeSnapshot {
                base,
                stable: Arc::new(Vec::new()),
                tail: Arc::new(Vec::new()),
                memo: FoldMemo::new(charge.clone()),
            }),
            dirty: tokio::sync::RwLock::new(NodeDirty {
                overlay: Vec::new(),
                overlay_bytes: 0,
                head_bytes: 0,
                snap_stable: Arc::new(Vec::new()),
                snap_tail: Vec::new(),
                max_applied_seq: 0,
                frozen: None,
                tail_offset,
                charged: 0,
                charge: charge.clone(),
            }),
            ref_bit: AtomicBool::new(true),
            pinned: AtomicBool::new(pinned),
            dirty_floor: AtomicU64::new(u64::MAX),
            charge,
        }))
    }

    /// Extent byte address (the cache key).
    pub fn addr(&self) -> u64 {
        self.addr
    }

    /// Node incarnation (§4.2 `child_node_seq` stale-pointer detection).
    pub fn node_seq(&self) -> u64 {
        self.node_seq
    }

    pub fn tree_id(&self) -> u8 {
        self.tree_id
    }

    /// 0 = leaf; interior levels are pinned and SMO-lock-only (§4.6).
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Inclusive key-space lower bound (§4.6 revalidation input).
    pub fn min_key(&self) -> &[u8] {
        &self.min_key
    }

    /// Inclusive key-space upper bound (§4.6 revalidation input).
    pub fn max_key(&self) -> &[u8] {
        &self.max_key
    }

    /// The lock-free lifecycle word (loom-modeled, §4.6).
    pub fn state(&self) -> &NodeState {
        &self.state
    }

    /// Latch-free read entry: the current immutable snapshot.
    pub fn snapshot(&self) -> Arc<NodeSnapshot> {
        self.snapshot.load_full()
    }

    /// The per-node write lock (§4.4 pt 1 / §4.9 4b). Commit-path writers
    /// take it on **leaves only**; interior locks belong to the serialized
    /// SMO task — that split is what keeps the lock populations acyclic.
    pub fn lock(&self) -> &tokio::sync::RwLock<NodeDirty> {
        &self.dirty
    }

    /// Pin (tree roots after [`super::tree::KvTree::open`]; interior nodes
    /// pin at construction).
    pub fn pin(&self) {
        self.pinned.store(true, Ordering::Release);
    }

    /// Whether this node is exempt from eviction.
    pub fn is_pinned(&self) -> bool {
        self.pinned.load(Ordering::Acquire)
    }

    fn touch(&self) {
        self.ref_bit.store(true, Ordering::Relaxed);
    }

    /// Apply records to the open delta **under the held write lock** and
    /// swap a new snapshot (the §4.4 RAM apply). Records must carry seqs
    /// assigned inside this lock window (§4.4 pt 2 ordering). Errors with
    /// the lifecycle word's verdict if the node was superseded — callers
    /// revalidate first, so this is the caught-bug path, not control flow.
    ///
    /// `floor` is the §4.6 pt 2 dirty-floor contribution, **rounded DOWN
    /// to the records' journal-entry start** (FIND-SMO-TAIL,
    /// docs/design-smo-replay-currency.md §1b): record seqs are stamped
    /// `entry_start + i` across a multi-leaf tx, so folding raw
    /// `rec.seq` let a node holding only `rec[j>0]` pin the checkpoint
    /// tail STRICTLY INSIDE the entry — replay parses at the tail and a
    /// mid-entry tail drops the entry's ≥-tail acked records plus
    /// collateral entries to the resync point. Callers pass the entry
    /// start (conveyor members, replay applies) or an already-boundary
    /// seq (SMO flips pin at `res.start`; single-record commits are
    /// their own entry start; an SMO's leftover move passes the
    /// predecessor's floor, itself entry-start-rounded). A lower tail
    /// only lengthens the replay window — absorbed idempotently by the
    /// per-key LWW replay gate.
    ///
    /// **PR M9 (§5.7 D7.a)**: each applied record's folded head is
    /// materialized here — one [`fold_forward`] step against the previous
    /// head, under the very lock the committer already holds ("one fold
    /// at write replaces N folds at N reads"). The head rides only the
    /// **newest** record per key (the superseded head below it is
    /// cleared); a per-key seq inversion (mount-replay interleavings —
    /// unreachable through the gated paths, guarded anyway) invalidates
    /// the stale heads above it instead of guessing. A record arriving
    /// with a head already attached (an SMO moving the displaced overlay
    /// into its successor) keeps it — the successor base folds
    /// identically for that key (the K1 compaction theorem).
    pub fn apply_locked(
        &self,
        guard: &mut NodeDirty,
        records: Vec<OwnedRec>,
        floor: u64,
    ) -> Result<(), KvError> {
        if self.state.mark_dirty().is_err() {
            return Err(KvError::Corrupt(format!(
                "apply on superseded node {:#x} (revalidation bypassed?)",
                self.addr
            )));
        }
        // §4.6 pt 2: every apply lowers the not-yet-durable floor; the
        // checkpoint's tail rule reads it back. The floor is RING-
        // POSITION domain (the tail must keep the records' journal entry
        // inside the replay window until they are durable-covered) —
        // production record seqs coincide with positions (`entry_start +
        // i`), but the on-disk contract only requires per-key LWW order,
        // and replayed window entries may legitimately carry fold-domain
        // seqs below their entry position (K6a-era committers) — the
        // entry start is the coverage target either way.
        if !records.is_empty() {
            self.dirty_floor.fetch_min(floor, Ordering::AcqRel);
        }
        for mut rec in records {
            // Stamp (or tighten) the record's own floor contribution so
            // an SMO leftover transfer can rebuild successor floors
            // EXACTLY (see the `entry_floor` field doc). An SMO-moved
            // record arrives with its original stamp and keeps it (the
            // successor apply's batch floor derives from these very
            // stamps, so `min` is idempotent there).
            rec.entry_floor = rec.entry_floor.min(floor);
            let pos = guard
                .overlay
                .partition_point(|r| (&r.key[..], r.seq) <= (&rec.key[..], rec.seq));
            // End of the whole key group: rec is the key's newest record
            // iff nothing of the same key sorts above its insertion point.
            let gend = guard.overlay.partition_point(|r| r.key[..] <= rec.key[..]);
            let key_newest = pos == gend;
            let prev_is_same_key = pos > 0 && guard.overlay[pos - 1].key[..] == rec.key[..];

            // D7.a head materialization: fold-forward against the folded
            // outcome of everything older than `rec` for this key. `Put`
            // and `Delete` shadow everything below (§4.2 LWW / tombstone
            // rules), so their heads are self-defining — the fold-below
            // is fetched only for `Delta`, the one kind that folds into
            // prior state (and the storm shape D7.a exists for).
            if rec.folded.is_none() {
                rec.folded = match rec.kind {
                    RecordKind::Put => Some(FoldedHead::Live {
                        value: rec.value.clone(),
                        decoded: None,
                        materialized: false,
                    }),
                    RecordKind::Delete => Some(FoldedHead::Tombstone),
                    RecordKind::Delta => {
                        let prev_head: Result<FoldedHead, KvError> = if prev_is_same_key {
                            match &guard.overlay[pos - 1].folded {
                                Some(h) => Ok(h.clone()),
                                // Invalidated below (rollback residue):
                                // one cold re-fold over the authoritative
                                // overlay group + the base — under the
                                // lock, so exact.
                                None => {
                                    let glo =
                                        guard.overlay.partition_point(|r| r.key[..] < rec.key[..]);
                                    self.fold_below_locked(&guard.overlay[glo..pos], &rec.key)
                                }
                            }
                        } else {
                            // No overlay records below: the fold below rec
                            // is the base fold (the published snapshot's
                            // base is current under this lock; its memo
                            // may already carry it).
                            self.fold_below_locked(&[], &rec.key)
                        };
                        match prev_head {
                            Ok(prev) => fold_forward(&prev, rec.kind, &rec.value).ok(),
                            // A corrupt base/delta surfaces at read time
                            // exactly as before D7 — the apply itself
                            // never changes semantics over it.
                            Err(_) => None,
                        }
                    }
                };
            }
            // Single-head-per-key: the superseded newest below loses its
            // head (never read again — lookups take the group's newest).
            if prev_is_same_key {
                let hb = guard.overlay[pos - 1].head_bytes();
                guard.head_bytes -= hb;
                guard.overlay[pos - 1].folded = None;
            }
            // Per-key inversion guard: records above `rec` folded without
            // it — their heads are stale. Invalidate; reads fall back to
            // the from-scratch fold (byte-identical by the §4.2 theorem).
            if !key_newest {
                for i in pos..gend {
                    let hb = guard.overlay[i].head_bytes();
                    guard.head_bytes -= hb;
                    guard.overlay[i].folded = None;
                }
            }
            guard.head_bytes += rec.head_bytes();
            guard.overlay_bytes += rec.record_ref().encoded_len();
            // Run-precedence guard: mount replay applies ORIGINAL seqs,
            // which may sort below records already published (idempotent
            // duplicates and window interleavings). Fold order requires
            // "tail ≥ stable per key", so a non-monotonic apply re-merges
            // the whole overlay into a fresh stable run instead of
            // riding the tail (replay-only cost, bounded by the window).
            let monotonic = rec.seq > guard.max_applied_seq;
            guard.max_applied_seq = guard.max_applied_seq.max(rec.seq);
            if monotonic {
                let tpos = guard
                    .snap_tail
                    .partition_point(|r| (&r.key[..], r.seq) <= (&rec.key[..], rec.seq));
                guard.snap_tail.insert(tpos, rec.clone());
                guard.overlay.insert(pos, rec);
            } else {
                guard.overlay.insert(pos, rec);
                guard.snap_stable = Arc::new(guard.overlay.clone());
                guard.snap_tail.clear();
            }
        }
        // Publish: Arc-share the stable run, clone only the small tail
        // (PR K7 — the whole-overlay clone per apply was ~44 % of the
        // serial create path). Tail overflow re-merges into a fresh
        // stable run, amortizing the full clone over OVERLAY_TAIL_MAX
        // applies.
        if guard.snap_tail.len() > OVERLAY_TAIL_MAX {
            guard.snap_stable = Arc::new(guard.overlay.clone());
            guard.snap_tail.clear();
        }
        debug_assert_eq!(
            guard.snap_stable.len() + guard.snap_tail.len(),
            guard.overlay.len(),
            "publish mirrors must partition the authoritative overlay"
        );
        guard.resync_charge();
        let cur = self.snapshot.load();
        self.snapshot.store(Arc::new(NodeSnapshot {
            base: cur.base.clone(),
            stable: guard.snap_stable.clone(),
            tail: Arc::new(guard.snap_tail.clone()),
            memo: FoldMemo::new(self.charge.clone()),
        }));
        Ok(())
    }

    /// Writer-side fold of "everything older than the record being
    /// applied" for one key: the given (authoritative, lock-held) overlay
    /// group below it, newest-first, then the current base group. With no
    /// overlay records below, the published snapshot's memo may already
    /// carry the base fold — probed without touching the D7.b read
    /// counters (this is the write path). Cold by construction: it runs
    /// once per key per invalidation/freeze cycle, never per read.
    fn fold_below_locked(
        &self,
        overlay_below: &[OwnedRec],
        key: &[u8],
    ) -> Result<FoldedHead, KvError> {
        let cur = self.snapshot.load();
        if overlay_below.is_empty() {
            if let Some(cell) = cur.memo.probe(key) {
                return Ok(match &cell.look {
                    LiveLookup::Live(v) => FoldedHead::Live {
                        value: v.clone(),
                        decoded: None,
                        materialized: false,
                    },
                    LiveLookup::Tombstone => FoldedHead::Tombstone,
                    LiveLookup::Absent => FoldedHead::Absent,
                });
            }
        }
        let bg = cur.base.group_bounds(key);
        let gather = overlay_below
            .iter()
            .rev()
            .map(|r| r.record_ref())
            .chain(bg.map(|i| cur.base.record_ref(i)));
        Ok(match fold_newest_first(gather)? {
            Folded::Absent => FoldedHead::Absent,
            Folded::Tombstone { .. } => FoldedHead::Tombstone,
            Folded::Put { value, .. } => FoldedHead::Live {
                value: match value {
                    std::borrow::Cow::Owned(v) => Bytes::from(v),
                    // Cold path: one copy beats threading source
                    // provenance through the fold (the head then serves
                    // refcounted for its whole life).
                    std::borrow::Cow::Borrowed(v) => Bytes::copy_from_slice(v),
                },
                decoded: None,
                // Either arm owns a fresh buffer (folded or copied) —
                // charge it (the honest-budget face of `owned_bytes`).
                materialized: true,
            },
        })
    }

    /// Current §4.6 pt 2 dirty floor (`u64::MAX` = clean of un-durable
    /// records).
    pub fn dirty_floor(&self) -> u64 {
        self.dirty_floor.load(Ordering::Acquire)
    }

    /// Checkpoint flush pass: take the floor (leaving `u64::MAX`) —
    /// records applied after this call re-lower it and belong to the
    /// next cycle. Call under the node write lock, in the same window as
    /// the freeze, so no applied record can slip between freeze and take.
    pub fn take_dirty_floor(&self) -> u64 {
        self.dirty_floor.swap(u64::MAX, Ordering::AcqRel)
    }

    /// Restore a floor after a failed flush (I/O error before the
    /// barrier): the records are still not durable-covered, so the tail
    /// must keep respecting them.
    pub fn restore_dirty_floor(&self, floor: u64) {
        self.dirty_floor.fetch_min(floor, Ordering::AcqRel);
    }

    /// §4.4 pt 4 seq-conditional rollback, removal half: under the held
    /// write lock, remove every open-overlay record of `key` whose seq is
    /// inside the failing tx's reserved range `[lo, hi)` and swap a fresh
    /// snapshot. Records that already left the overlay (a freeze raced
    /// the failed write) are the caller's compensation problem — detected
    /// via [`NodeSnapshot::newest_record_seq`]. Returns how many records
    /// were removed.
    pub fn remove_overlay_records_locked(
        &self,
        guard: &mut NodeDirty,
        key: &[u8],
        lo: u64,
        hi: u64,
    ) -> usize {
        let before = guard.overlay.len();
        guard.overlay.retain(|r| {
            let mine = r.key[..] == *key && r.seq >= lo && r.seq < hi;
            !mine
        });
        let removed = before - guard.overlay.len();
        if removed > 0 {
            // D7.a stale-head guard (§4.4 pt 4 / risk R7): a surviving
            // record NEWER than the removed range folded the removed
            // records into its materialized head — the concurrent-Δtime
            // rollback race shape. Invalidate those heads; reads fall
            // back to the from-scratch fold over the survivors (older
            // survivors' heads folded nothing that was removed and stay).
            let glo = guard.overlay.partition_point(|r| r.key[..] < *key);
            let gend = guard.overlay.partition_point(|r| r.key[..] <= *key);
            for i in glo..gend {
                if guard.overlay[i].seq > lo {
                    guard.overlay[i].folded = None;
                }
            }
            guard.overlay_bytes = guard
                .overlay
                .iter()
                .map(|r| r.record_ref().encoded_len())
                .sum();
            guard.head_bytes = guard.overlay.iter().map(|r| r.head_bytes()).sum();
            // Rollback is cold: resync the publish mirrors with a full
            // re-merge and publish the corrected view.
            guard.snap_stable = Arc::new(guard.overlay.clone());
            guard.snap_tail.clear();
            guard.resync_charge();
            let cur = self.snapshot.load();
            self.snapshot.store(Arc::new(NodeSnapshot {
                base: cur.base.clone(),
                stable: guard.snap_stable.clone(),
                tail: Arc::new(Vec::new()),
                memo: FoldMemo::new(self.charge.clone()),
            }));
        }
        removed
    }

    /// The §4.6 pt 1 freeze-swap, **under the held write lock**: move the
    /// open delta out as an immutable frozen bset, merge it into the
    /// snapshot's base index (readers see identical content, now
    /// serialized), and leave the append/compact to run **outside** the
    /// lock. No-op returning the existing frozen delta if one is already
    /// awaiting I/O.
    ///
    /// **Freeze-time shadow-fold** (perf/meta-plane-writes, 2026-07-30;
    /// contract in `tests/meta_write_economy_audit_tests.rs`): the frozen
    /// bset carries, per key, only the newest **base-establishing**
    /// record (`Put`/`Delete`) and any `Delta`s newer than it — records
    /// completely shadowed by the fold algebra (everything older than
    /// the newest base; §4.2) never reach the device. Delta-only runs
    /// keep every record (their base lives in an older bset/the base
    /// index). The field's block-publish storm appended W full layout
    /// values per cadence window for exactly this supersession waste.
    ///
    /// Crash safety is unchanged: within one freeze the shadowing record
    /// and its dropped victims share a bset, which either fully survives
    /// (the shadow serves — fold-identical) or is fully dropped by the
    /// §4.5 torn-tail classifier (the journal window still re-supplies
    /// every dropped seq — the checkpoint tail cannot pass this freeze's
    /// records until the covering ledger record is durable). Shadowed
    /// records are always from COMMITTED transactions: a failing tx
    /// holds its 4a DLM guards across rollback, so no newer same-key
    /// record can exist above it at freeze time (its record — if a
    /// freeze raced the failed write — is the newest of its key run and
    /// is kept, preserving the §4.4 pt 4 compensation detection).
    pub fn freeze_locked(
        &self,
        guard: &mut NodeDirty,
        layout: &NodeLayout,
    ) -> Result<Option<FrozenDelta>, KvError> {
        if guard.frozen.is_some() {
            return Ok(guard.frozen.clone());
        }
        if guard.overlay.is_empty() {
            return Ok(None);
        }
        self.state.begin_freeze().map_err(|e| {
            KvError::Corrupt(format!("freeze refused on node {:#x}: {e:?}", self.addr))
        })?;
        // The overlay is sorted by (key, seq) ascending (apply_locked's
        // partition_point insert), so shadow-folding is one linear pass
        // over per-key runs.
        let overlay = &guard.overlay;
        let mut records: Vec<Record> = Vec::with_capacity(overlay.len());
        let mut dropped = 0u64;
        let mut i = 0;
        while i < overlay.len() {
            let mut j = i + 1;
            while j < overlay.len() && overlay[j].key == overlay[i].key {
                j += 1;
            }
            // Newest base-establishing record in the run (runs are seq-
            // ascending); none ⇒ delta-only run, keep everything.
            let keep_from = overlay[i..j]
                .iter()
                .rposition(|r| matches!(r.kind, RecordKind::Put | RecordKind::Delete))
                .map_or(i, |base| i + base);
            dropped += (keep_from - i) as u64;
            records.extend(overlay[keep_from..j].iter().map(|r| r.to_record()));
            i = j;
        }
        if dropped > 0 {
            super::META_KV_NODE_FREEZE_SHADOW_DROPPED
                .fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
        }
        // The horizon covers the WHOLE overlay (its max seq is the newest
        // record of some key run, which is always kept — asserted by the
        // equality of the two computations in debug builds).
        let horizon = guard.overlay.iter().map(|r| r.seq).max().unwrap_or(0);
        debug_assert_eq!(
            horizon,
            records.iter().map(|r| r.seq).max().unwrap_or(0),
            "the overlay's newest record must survive the shadow-fold"
        );
        let frame = encode_bset_frame(layout, self.node_seq, &records, horizon)?;
        let frame_len = frame.len();
        let frame = Bytes::from(frame);
        // The frame's embedded bset becomes one more index source —
        // merged INCREMENTALLY (O(existing + new)): the k-way rebuild
        // was 37 % of the serial create path once threshold wakes made
        // freezes per-bset-worth frequent (§4.6 pt 1, PR K7).
        let bset_len = u32::from_le_bytes([frame[20], frame[21], frame[22], frame[23]]) as usize;
        let bset_image = frame.slice(BSET_FRAME_LEN..BSET_FRAME_LEN + bset_len);
        let cur = self.snapshot.load();
        let base = Arc::new(cur.base.extend_with(bset_image)?);
        guard.overlay.clear();
        guard.overlay_bytes = 0;
        // D7.a: the heads freeze away with their records (the overlay is
        // the head's home — §5.7); the first post-freeze read repopulates
        // through the D7.b memo instead.
        guard.head_bytes = 0;
        guard.snap_stable = Arc::new(Vec::new());
        guard.snap_tail.clear();
        guard.resync_charge();
        let frozen = FrozenDelta {
            records: Arc::new(records),
            horizon,
            frame_len,
        };
        guard.frozen = Some(frozen.clone());
        self.snapshot.store(Arc::new(NodeSnapshot {
            base,
            stable: Arc::new(Vec::new()),
            tail: Arc::new(Vec::new()),
            memo: FoldMemo::new(self.charge.clone()),
        }));
        Ok(Some(frozen))
    }
}

// ---------------------------------------------------------------------------
// The cache.
// ---------------------------------------------------------------------------

/// Removes the inflight single-flight entry and wakes waiters even if the
/// loading future is cancelled mid-load (the `routing.rs` inflight-guard
/// discipline).
struct InflightLoadGuard<'a> {
    cache: &'a NodeCache,
    addr: u64,
    tx: tokio::sync::broadcast::Sender<()>,
}

impl Drop for InflightLoadGuard<'_> {
    fn drop(&mut self) {
        self.cache
            .inflight
            .remove_if_sync(&self.addr, |tx| tx.same_channel(&self.tx));
        let _ = self.tx.send(());
    }
}

/// The per-volume node cache (§4.5). All extent I/O flows through the K2
/// node layer (`crate::uring_fs`, io_uring-only).
pub struct NodeCache {
    cfg: NodeCacheConfig,
    map: scc::HashMap<u64, Arc<CachedNode>>,
    inflight: scc::HashMap<u64, tokio::sync::broadcast::Sender<()>>,
    /// Clock ring: FIFO of candidate addresses + per-node second-chance
    /// ref bits (the sharded-clock family of `src/cache/lru.rs`, sized for
    /// node counts). Stale entries (evicted/superseded nodes) fall out on
    /// pop.
    clock: scc::Queue<u64>,
    /// The budget gauge (§4.5 + PR M9 §5.7): Σ mapped extents (charged at
    /// publish, released at evict/retire) **+ overlay bytes incl. folded
    /// heads + snapshot memo bytes** — the latter two owned by
    /// [`NodeDirty`]/[`FoldMemo`] through this shared handle (Drop-exact;
    /// a dying snapshot's memo bytes leave when its readers do). Arc'd so
    /// nodes charge without a back-reference cycle.
    cached_bytes: Arc<AtomicU64>,
    /// The durable journal tail (§4.5 torn-tail classifier input, §4.2
    /// tombstone elision floor). K6b's checkpoint advances it; tests drive
    /// it directly.
    durable_tail: AtomicU64,
    /// Extents an SMO retired whose **disk image lags the RAM-authoritative
    /// state that superseded it** (the open delta moved into successor
    /// nodes' RAM, never onto this extent). A traversal holding a pre-SMO
    /// snapshot may still route here; demand-loading the lagging image
    /// would time-travel acked records, so [`Self::load`] refuses these
    /// addresses (`Ok(None)`) and the traversal restarts through current
    /// snapshots. Cleared when the extent hosts a mapped live node again
    /// ([`Self::publish`] of a reused extent). In-flight readers that
    /// already hold the superseded object's `Arc` keep reading its intact
    /// snapshot (§4.6) — this set only guards re-loads from disk.
    retired: scc::HashSet<u64>,
    /// FIND-VS-A tail-rule fix: the min `dirty_floor` of every node whose
    /// mapping LEFT this cache (SMO retire, eviction, publish-replace)
    /// since the checkpoint task last drained it. The §4.6 pt 2 tail rule
    /// walks LIVE nodes' floors — a floor that dies with its mapping
    /// otherwise becomes invisible, the next ledger's `journal_tail_seq`
    /// sails past its record seqs, and a crash before the covering state
    /// is durably *reachable* loses acked commits (observed as the
    /// scoreboard's post-crash ENOENT on 1–4 % of acked creates; forensics
    /// in `.benchmarks/2026-07-16-find-vs-a-fix.md`). Draining it into the
    /// tail computation keeps every such record inside the replay window
    /// until a ledger record written AFTER its floor died covers it.
    dying_floors: AtomicU64,
}

impl std::fmt::Debug for NodeCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeCache")
            .field("nodes", &self.map.len())
            .field("cached_bytes", &self.cached_bytes.load(Ordering::Relaxed))
            .field("budget_bytes", &self.cfg.budget_bytes)
            .finish()
    }
}

impl NodeCache {
    pub fn new(cfg: NodeCacheConfig) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            map: scc::HashMap::default(),
            inflight: scc::HashMap::default(),
            clock: scc::Queue::default(),
            cached_bytes: Arc::new(AtomicU64::new(0)),
            durable_tail: AtomicU64::new(0),
            retired: scc::HashSet::default(),
            dying_floors: AtomicU64::new(u64::MAX),
        })
    }

    /// Fold a departing node's `dirty_floor` into the dying-floor
    /// accumulator (see the field doc). `u64::MAX` (clean) is a no-op.
    /// Crate-visible: the tree layer also folds ROOT-SWAP positions here
    /// (the one routing change whose only durable form is the next ledger
    /// record — FIND-VS-A).
    pub(crate) fn note_dying_floor(&self, floor: u64) {
        if floor != u64::MAX {
            self.dying_floors.fetch_min(floor, Ordering::AcqRel);
        }
    }

    /// Drain the dying-floor accumulator — the checkpoint task's tail
    /// computation calls this once per cycle (after the flush pass) and
    /// clamps `journal_tail_seq` to the result. Callers that fail to
    /// write the covering ledger record must fold the value back
    /// ([`Self::restore_dying_floors`]) so the next cycle still respects
    /// it.
    pub fn take_dying_floors(&self) -> u64 {
        self.dying_floors.swap(u64::MAX, Ordering::AcqRel)
    }

    /// Fold a drained dying-floor value back after a failed ledger write
    /// (the covering record never landed — see [`Self::take_dying_floors`]).
    pub fn restore_dying_floors(&self, floor: u64) {
        if floor != u64::MAX {
            self.dying_floors.fetch_min(floor, Ordering::AcqRel);
        }
    }

    /// The shared budget gauge — what [`CachedNode::from_loaded`] takes so
    /// overlay/memo bytes charge this cache (§5.7). Crate-internal: the
    /// tree layer builds SMO successors and fresh roots itself.
    pub(crate) fn charge_gauge(&self) -> Arc<AtomicU64> {
        self.cached_bytes.clone()
    }

    /// The cache's placement/policy config.
    pub fn config(&self) -> &NodeCacheConfig {
        &self.cfg
    }

    /// Byte address of heap extent `extent`.
    pub fn extent_addr(&self, extent: u64) -> u64 {
        self.cfg.heap_base + extent * self.cfg.layout.node_size() as u64
    }

    /// Heap extent index of node address `addr`.
    pub fn addr_extent(&self, addr: u64) -> u64 {
        (addr - self.cfg.heap_base) / self.cfg.layout.node_size() as u64
    }

    /// Current durable journal tail (§4.6 pt 2's checkpoint output; a test
    /// / K6b input here).
    pub fn durable_tail(&self) -> u64 {
        self.durable_tail.load(Ordering::Acquire)
    }

    /// Advance the durable tail (monotonic).
    pub fn set_durable_tail(&self, tail: u64) {
        self.durable_tail.fetch_max(tail, Ordering::AcqRel);
    }

    /// Bytes currently charged against the budget.
    pub fn cached_bytes(&self) -> u64 {
        self.cached_bytes.load(Ordering::Acquire)
    }

    /// Whether `addr` is currently mapped (tests).
    pub fn contains(&self, addr: u64) -> bool {
        self.map.contains_sync(&addr)
    }

    /// Visit every mapped node (the K6b checkpoint's dirty-set walk; the
    /// tree's `flush_dirty` uses it today). Not a consistent snapshot —
    /// racing inserts/evictions may or may not be visited, which is fine
    /// for its callers (they re-check per node under its lock).
    pub fn for_each_node(&self, mut f: impl FnMut(&Arc<CachedNode>)) {
        self.map.iter_sync(|_, v| {
            f(v);
            true
        });
    }

    /// Latch-free map read: `Some` is a cache hit (counted). The returned
    /// `Arc` stays valid across eviction — readers keep their snapshots by
    /// refcount.
    pub fn try_get(&self, addr: u64) -> Option<Arc<CachedNode>> {
        let node = self.map.read_sync(&addr, |_, v| v.clone())?;
        node.touch();
        super::META_KV_NODE_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
        Some(node)
    }

    /// Single-flight demand page (§4.5): exactly one loader per address
    /// reads the extent (K2 [`load_node`]: one `uring_fs::read_at`, header
    /// + bset verification, torn-tail classification); losers wait on the
    /// loader's broadcast and re-check the map.
    ///
    /// `Ok(None)` = the address is a **retired extent** ([`Self::retire`]):
    /// its disk image lags the RAM state that superseded it, so serving it
    /// would time-travel acked records. Only a traversal holding a
    /// pre-SMO parent snapshot can reach one — it must restart from the
    /// tree root through current snapshots (see [`super::tree`]).
    pub async fn load(&self, addr: u64) -> Result<Option<Arc<CachedNode>>, KvError> {
        loop {
            if let Some(node) = self.map.read_async(&addr, |_, v| v.clone()).await {
                node.touch();
                super::META_KV_NODE_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(node));
            }
            if self.retired.contains_sync(&addr) {
                return Ok(None);
            }
            // Miss: exactly one loader per address; losers wait and re-check.
            let guard = {
                match self.inflight.entry_async(addr).await {
                    scc::hash_map::Entry::Occupied(e) => {
                        let mut rx = e.get().subscribe();
                        drop(e);
                        // Sender dropped (loader done or cancelled) also
                        // wakes us; either way, re-check the map.
                        let _ = rx.recv().await;
                        continue;
                    }
                    scc::hash_map::Entry::Vacant(e) => {
                        let (tx, _rx) = tokio::sync::broadcast::channel(1);
                        e.insert_entry(tx.clone());
                        InflightLoadGuard {
                            cache: self,
                            addr,
                            tx,
                        }
                    }
                }
            };
            let loaded =
                load_node(&self.cfg.path, &self.cfg.layout, addr, self.durable_tail()).await?;
            // Re-check after the read: a retire during our load means the
            // bytes we hold are the lagging image (a mapping existed until
            // [`Self::retire`] ran, and retire marks the set BEFORE
            // dropping the mapping — so a loader that missed the map is
            // guaranteed to see the mark here).
            if self.retired.contains_sync(&addr) {
                return Ok(None);
            }
            let node = CachedNode::from_loaded(loaded, false, self.cached_bytes.clone())?;
            if node.level() > 0 {
                node.pin(); // §4.5: interior nodes always pinned.
            }
            self.publish(node.clone());
            super::META_KV_NODE_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
            drop(guard);
            return Ok(Some(node));
        }
    }

    /// [`Self::try_get`] or [`Self::load`], erroring on a retired address
    /// — for callers that resolve through current state by construction
    /// (tree open, the SMO's own reloads, tests).
    pub async fn get(&self, addr: u64) -> Result<Arc<CachedNode>, KvError> {
        if let Some(node) = self.try_get(addr) {
            return Ok(node);
        }
        self.load(addr).await?.ok_or_else(|| {
            KvError::Corrupt(format!(
                "node {addr:#x} is a retired extent (stale pointer outside a traversal)"
            ))
        })
    }

    /// Publish a node built by the caller (an SMO successor, a fresh tree
    /// root, or a demand load): insert into the map, un-retire the extent
    /// (a reused extent hosts current state again — stale pointers to its
    /// previous life are caught by the §4.2 `node_seq` check), charge the
    /// budget, enter the clock, evict down to budget if needed.
    pub fn publish(&self, node: Arc<CachedNode>) {
        let addr = node.addr();
        let evictable = !node.is_pinned();
        match self.map.entry_sync(addr) {
            scc::hash_map::Entry::Occupied(mut e) => {
                // Replacing a mapping (an SMO successor over a stale
                // demand-loaded object): sever the old one. Its floor —
                // and any dirt the displaced object still held — must
                // keep clamping the tail (FIND-VS-A: a floor that dies
                // with its mapping otherwise lets the ledger tail pass
                // un-covered acked records).
                let old = e.get().clone();
                let outcome = old.state().supersede();
                self.note_dying_floor(old.dirty_floor());
                if let Ok(o) = outcome {
                    if o.was_dirty || o.was_freezing {
                        // Displaced RAM records cannot be carried here
                        // (this is not the SMO path): loud, and the floor
                        // fold above keeps them replay-covered.
                        log::error!(
                            "node cache publish over a NON-CLEAN mapping at {:#x} \
                             (was_dirty={} was_freezing={} floor={}): displaced open-delta \
                             records stay replay-covered via the dying-floor clamp",
                            addr,
                            o.was_dirty,
                            o.was_freezing,
                            old.dirty_floor()
                        );
                    }
                }
                *e.get_mut() = node;
            }
            scc::hash_map::Entry::Vacant(e) => {
                e.insert_entry(node);
                self.cached_bytes
                    .fetch_add(self.cfg.layout.node_size() as u64, Ordering::AcqRel);
            }
        }
        self.retired.remove_sync(&addr);
        if evictable {
            self.clock.push(addr);
        }
        self.evict_to_budget();
    }

    /// Sever an SMO-superseded node: mark its extent retired **before**
    /// dropping the mapping (that order is what lets [`Self::load`]'s
    /// post-read re-check catch every racing loader), then release the
    /// budget charge. The caller (the serialized SMO task, holding the
    /// node's write lock) owns the [`NodeState::supersede`] transition and
    /// the pending-free of the extent (§4.7). In-flight readers keep the
    /// object's snapshot alive by refcount (§4.6).
    pub fn retire(&self, node: &Arc<CachedNode>) {
        // FIND-VS-A: the retiring node's floor leaves the live-floor walk
        // with this mapping. Its records ARE durable in the successor
        // images (barriered before the swap), but their *reachability*
        // — SMO pointer records / the root the next ledger names — is not
        // durably tied down until a ledger record written after this
        // point. Clamping the next tail to the dead floor keeps every
        // such record inside the replay window until then.
        self.note_dying_floor(node.dirty_floor());
        self.retired.insert_sync(node.addr()).ok();
        if self
            .map
            .remove_if_sync(&node.addr(), |v| Arc::ptr_eq(v, node))
            .is_some()
        {
            self.cached_bytes
                .fetch_sub(self.cfg.layout.node_size() as u64, Ordering::AcqRel);
        }
    }

    /// Clock sweep (§4.5): second-chance FIFO; clean unpinned nodes evict
    /// by dropping the Arc (in-flight readers keep their snapshots alive);
    /// dirty/serializing nodes are pinned by [`NodeState::try_evict`]'s
    /// clean-only CAS; interior/root pins are skipped outright. Bounded to
    /// two laps so an all-pinned cache cannot spin.
    ///
    /// **Externally-held nodes are skipped** (PR M9 tiny-budget finding):
    /// evicting a node some task still holds an `Arc` to frees **no
    /// memory** — the holder keeps node + snapshot alive by refcount —
    /// while severing the mapping, which forces the commit path's
    /// resolve → lock → revalidate window into reload-thrash (under a
    /// budget below the working set, all the way to its bounded-retry
    /// EINVAL). The sweep is only allowed to reclaim what dropping the
    /// map reference would actually free: `strong_count == 2` (the map's
    /// reference + the sweep's own probe). Racing grabs after the CAS
    /// keep the object alive by refcount exactly as before — this gate
    /// narrows eviction, never weakens it.
    fn evict_to_budget(&self) {
        let mut attempts = 2 * (self.map.len() + 1);
        while self.cached_bytes.load(Ordering::Acquire) > self.cfg.budget_bytes && attempts > 0 {
            attempts -= 1;
            let Some(entry) = self.clock.pop() else { break };
            let addr = **entry;
            let Some(node) = self.map.read_sync(&addr, |_, v| v.clone()) else {
                continue; // stale clock entry
            };
            if node.is_pinned() {
                continue; // pinned entries never re-enter the clock
            }
            if node.ref_bit.swap(false, Ordering::AcqRel) {
                self.clock.push(addr); // second chance
                continue;
            }
            if Arc::strong_count(&node) > 2 {
                // Held outside the cache (a resolver mid-commit, a
                // traversal, a checkpoint walk): reclaims nothing now —
                // re-enter the clock and try again once released.
                self.clock.push(addr);
                continue;
            }
            if node.state().try_evict() {
                // A state-clean node can still carry a floor: threshold
                // maintenance appends its bytes (they are on disk) but
                // only a checkpoint's barrier makes them durable-covered.
                // The floor must survive the eviction (FIND-VS-A dying-
                // floor clamp) or the tail could pass records whose
                // appends a power-cut would still tear away.
                self.note_dying_floor(node.dirty_floor());
                if self
                    .map
                    .remove_if_sync(&addr, |v| Arc::ptr_eq(v, &node))
                    .is_some()
                {
                    self.cached_bytes
                        .fetch_sub(self.cfg.layout.node_size() as u64, Ordering::AcqRel);
                    super::META_KV_NODE_CACHE_EVICTIONS.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                // Dirty / serializing: pinned until writeback (§4.5).
                self.clock.push(addr);
            }
        }
    }

    /// Append the frozen delta of `node` to its extent tail — the §4.6
    /// pt 1 write **outside** the node lock, on the serialized
    /// writeback/SMO task. `Ok(true)` = appended (bookkeeping updated,
    /// freeze ended); `Ok(false)` = the frame does not fit
    /// ([`KvError::NodeFull`] downgraded to a signal) — the caller
    /// compacts/splits instead (§4.6 pt 1).
    pub async fn append_frozen(&self, node: &Arc<CachedNode>) -> Result<bool, KvError> {
        let (frozen, tail) = {
            let g = node.lock().read().await;
            match &g.frozen {
                None => return Ok(true), // nothing to do
                Some(f) => (f.clone(), g.tail_offset),
            }
        };
        if tail + frozen.frame_len > self.cfg.layout.node_size() {
            return Ok(false);
        }
        let dest = AppendDest {
            node_addr: node.addr(),
            node_seq: node.node_seq(),
            tail_offset: tail,
        };
        match super::node::append_bset(
            &self.cfg.path,
            &self.cfg.layout,
            &dest,
            &frozen.records,
            frozen.horizon,
        )
        .await
        {
            Ok(new_tail) => {
                let mut g = node.lock().write().await;
                g.tail_offset = new_tail;
                g.frozen = None;
                drop(g);
                node.state().end_freeze();
                Ok(true)
            }
            Err(KvError::NodeFull { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Contracts (spec §11 TEST-5/TEST-9)
// ---------------------------------------------------------------------------
//
// `node_cache.rs` carries 19 atomics and had ZERO in-module tests — its
// only coverage was incidental, through the KV integration suites. The
// pieces below are the ones whose failure mode is SILENT rather than a
// wrong answer: the §5.7 memory-charge gauges.
//
// A charge gauge that loses a credit reads as "the cache is fuller than
// it is" and evicts forever; one that over-credits WRAPS a `u64` through
// zero and reads as "the cache is full forever" — exactly the bug class
// `gauge_core` was extracted and loom-modelled for after it shipped once.
// The memo's accounting is Drop-owned, so the law is conservation across
// populate/drop under racing claims.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn baseline() -> u64 {
        super::super::META_KV_FOLD_MEMO_BYTES.load(Ordering::Acquire)
    }

    #[test]
    fn memo_populate_charges_both_gauges_and_drop_credits_exactly() {
        let charge = Arc::new(AtomicU64::new(0));
        let global0 = baseline();
        {
            let memo = FoldMemo::new(Arc::clone(&charge));
            memo.populate(
                Bytes::from_static(b"key-one"),
                LiveLookup::Live(Bytes::from_static(b"0123456789")),
                7,
            );
            let charged = charge.load(Ordering::Acquire);
            assert_eq!(
                charged,
                (b"key-one".len() + MEMO_CELL_OVERHEAD + 10) as u64,
                "the charge must be key + fixed cell overhead + owned value"
            );
            assert_eq!(
                super::super::META_KV_FOLD_MEMO_BYTES.load(Ordering::Acquire) - global0,
                charged,
                "the per-cache budget and the global gauge must move together"
            );
            // A tombstone/absent outcome refcounts snapshot memory the
            // node already pays for: key + overhead only.
            memo.populate(Bytes::from_static(b"key-two"), LiveLookup::Tombstone, 8);
            assert_eq!(
                charge.load(Ordering::Acquire) - charged,
                (b"key-two".len() + MEMO_CELL_OVERHEAD) as u64,
                "a tombstone memo must not charge a value it does not own"
            );
        }
        assert_eq!(
            charge.load(Ordering::Acquire),
            0,
            "Drop must credit EXACTLY what populate charged — a lost credit \
             makes the cache read fuller than it is and evict forever"
        );
        assert_eq!(
            baseline(),
            global0,
            "and the global fold-memo gauge must return to its baseline"
        );
    }

    #[test]
    fn a_full_memo_drops_entries_rather_than_growing_its_charge() {
        let charge = Arc::new(AtomicU64::new(0));
        let global0 = baseline();
        {
            let memo = FoldMemo::new(Arc::clone(&charge));
            for i in 0..(FOLD_MEMO_CAPACITY * 4) {
                memo.populate(
                    Bytes::from(format!("k{i:04}")),
                    LiveLookup::Live(Bytes::from_static(b"v")),
                    i as u64,
                );
            }
            let per_cell = (5 + MEMO_CELL_OVERHEAD + 1) as u64;
            assert_eq!(
                charge.load(Ordering::Acquire),
                per_cell * FOLD_MEMO_CAPACITY as u64,
                "the fixed capacity IS the §5.7 per-node memory bound — a \
                 32-key storm must charge 8 cells, not 32"
            );
            // Everything that fit is still probeable; the overflow is gone.
            assert!(memo.probe(b"k0000").is_some(), "the first claim survives");
            assert!(
                memo.probe(b"k0031").is_none(),
                "an over-capacity entry is dropped, not stored"
            );
        }
        assert_eq!(charge.load(Ordering::Acquire), 0);
        assert_eq!(baseline(), global0);
    }

    #[test]
    fn racing_same_key_populates_charge_at_most_once() {
        // The populate-once claim: a same-key racer's duplicate is
        // prevented by the post-loss re-check. Double-charging one key
        // would inflate the budget with no memory behind it.
        let charge = Arc::new(AtomicU64::new(0));
        let global0 = baseline();
        {
            let memo = Arc::new(FoldMemo::new(Arc::clone(&charge)));
            let mut hs = Vec::new();
            for _ in 0..8 {
                let memo = Arc::clone(&memo);
                hs.push(std::thread::spawn(move || {
                    memo.populate(
                        Bytes::from_static(b"contended"),
                        LiveLookup::Live(Bytes::from_static(b"value")),
                        1,
                    );
                }));
            }
            for h in hs {
                h.join().expect("no populate may panic");
            }
            let one = (b"contended".len() + MEMO_CELL_OVERHEAD + 5) as u64;
            let got = charge.load(Ordering::Acquire);
            assert!(
                got == one,
                "8 racing populates of ONE key must charge it once (got \
                 {got}, one cell is {one})"
            );
            assert!(memo.probe(b"contended").is_some());
        }
        assert_eq!(
            charge.load(Ordering::Acquire),
            0,
            "and the contended charge credits back exactly"
        );
        assert_eq!(baseline(), global0);
    }

    #[test]
    fn racing_distinct_key_populates_conserve_the_charge_across_drop() {
        // The conservation law under full contention: whatever N racing
        // distinct-key claims charge, Drop credits back to zero. A gauge
        // that over-credits WRAPS a u64 through zero and then reads as
        // "full forever" — the bug class `gauge_core` exists for.
        let charge = Arc::new(AtomicU64::new(0));
        let global0 = baseline();
        {
            let memo = Arc::new(FoldMemo::new(Arc::clone(&charge)));
            let mut hs = Vec::new();
            for t in 0..8u64 {
                let memo = Arc::clone(&memo);
                hs.push(std::thread::spawn(move || {
                    memo.populate(
                        Bytes::from(format!("k{t}")),
                        LiveLookup::Live(Bytes::from_static(b"vv")),
                        t,
                    );
                }));
            }
            for h in hs {
                h.join().expect("no populate may panic");
            }
            let charged = charge.load(Ordering::Acquire);
            assert!(charged > 0, "the racers charged something");
            assert_eq!(
                super::super::META_KV_FOLD_MEMO_BYTES.load(Ordering::Acquire) - global0,
                charged,
                "both gauges must agree after a contended populate storm"
            );
        }
        assert_eq!(
            charge.load(Ordering::Acquire),
            0,
            "conservation: charge - credit == 0, never a wrap"
        );
        assert_eq!(baseline(), global0);
    }

    #[test]
    fn an_empty_memo_charges_nothing_and_credits_nothing() {
        let charge = Arc::new(AtomicU64::new(0));
        let global0 = baseline();
        {
            let memo = FoldMemo::new(Arc::clone(&charge));
            assert!(memo.probe(b"absent").is_none());
            assert_eq!(charge.load(Ordering::Acquire), 0);
        }
        assert_eq!(charge.load(Ordering::Acquire), 0, "no spurious credit");
        assert_eq!(baseline(), global0);
    }
}
