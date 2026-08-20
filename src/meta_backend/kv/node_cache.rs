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
//!   per-node `SqzRwLock` (§4.4 pt 1 — the lock covers RAM
//!   mutation only, never device I/O).
//! - **Reads are latch-free**: load the snapshot `Arc`, binary-search the
//!   merged index, hand out `Bytes` views into the snapshot's backing
//!   buffers — no lock, no copy. Writers swap a new snapshot after the RAM
//!   apply.
//! - **Demand paging**: a miss reads the whole extent with one
//!   `uring_fs::read_at` inside K2's [`load_node`] (header + bset
//!   verification + the §4.5 torn-tail classifier), then builds the
//!   snapshot. Loads are **single-flight** per node address (the
//!   `routing.rs` inflight-guard discipline: losers wait on a
//!   single-flight completion and re-check the map; a cancelled loader's
//!   drop guard wakes them).
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
//!
//! ## Multi-process coherence: partitioning, and a lagging reader
//!
//! Everything above is **load-once RAM-authoritative**: a node is read from
//! the device exactly once and thereafter served from immutable arc-swapped
//! snapshots. Pre-RC engineering spec §6.2 names that the runtime item
//! *"arguably harder than any of the ten"* durable-format single-writer
//! assumptions, and gives the verdict: *"the tractable answer is to ensure
//! two writers never cache the same node — **partitioning, not cache
//! coherence**."* Two mechanisms implement that verdict here, and there is
//! deliberately **no cross-node coherence protocol**:
//!
//! ### 1. A coherent READER lags by one polled checkpoint (§6.8 item 2)
//!
//! [`NodeCache::arm_revalidation`] declares this cache a reader as of one
//! A/B root-ledger record; [`NodeCache::revalidate`] adopts a newer record
//! and drops every cached node not covered by it. The consistency model an
//! operator reads is in `docs/operations.md` § *Read-only coherent mounts*;
//! the mechanism's three load-bearing pieces are:
//!
//! * the **epoch stamp** on every node ([`CachedNode::epoch_stamp`]) versus
//!   the cache's epoch word — one relaxed compare on the hit path, which is
//!   the lazy half of the drop pass and makes "an operation that starts
//!   after a poll sees only that epoch's nodes" true even mid-sweep;
//! * the **two-word publication law** in [`super::epoch_core`]: the durable
//!   tail is published before the epoch that vouches for it, and read after
//!   it, so a node can never claim currency for a checkpoint whose tail it
//!   was not classified against (the silent-record-loss direction);
//! * the **R-6 purge trigger** ([`EpochPurgeSink`]): a metadata epoch step
//!   is a reader's only evidence that the writer may have recycled data
//!   block keys, and §6.8 item 5's observation is that the invalidation
//!   primitive (`TieredCache::purge_block_key`) is already complete — only
//!   this trigger was missing.
//!
//! An un-armed cache — every write mount today — holds epoch
//! [`UNARMED_EPOCH`], stamps every node with it, and is structurally inert:
//! `revalidate` refuses to advance, so no mapping a local mount owns can be
//! dropped by machinery it never opted into.
//!
//! ### 2. The WRITER populations are partitioned, and violations are loud
//!
//! Which nodes may one appender cache, with the enforcement point for each:
//!
//! | Population | Who may cache/mutate it | Enforced |
//! |---|---|---|
//! | interior nodes (`level > 0`), tree roots | the **root authority** alone — the RAM face of lock order 4b (interior locks belong exclusively to the serialized per-volume checkpoint/SMO task) and of `read_partitioned_ledger`'s refusal of a non-authority record naming roots | [`CachedNode::apply_locked`] refuses a non-authority structural mutation loud (`meta_kv_node_partition_refusals`) |
//! | leaves | the appender the **slot map** assigns (spec §6.2 items 4/8) — deliberately NOT arbitrated here, so this gate is never mistaken for cross-writer custody | a peer's append into a node's log is detected at [`NodeCache::append_frozen`] instead of being silently overwritten |
//! | any node, on a reader | nobody | the reader arm of the same gate |
//!
//! Disjointness of the two *cacher* populations is therefore structural for
//! interior nodes and *detected* for leaves — at runtime by the append
//! probe, and at replay by `journal::detect_partition_violations`'
//! `PartitionViolation::Key`. The residual hole is stated where it belongs:
//! a leaf a peer wrote in an **earlier** window, which the authority cached
//! before that window closed, leaves no evidence in either place. What
//! closes it is arming revalidation on the peer as well (a non-authority
//! appender is, with respect to structure, a reader) — see the argument and
//! its limits in `.benchmarks/2026-08-05-mw-node-cache-coherence.md` §4.

use super::bset::BsetView;
use super::checkpoint::{LedgerRecord, TreeRoot};
use super::epoch_core::{NodeEnv, UNARMED_EPOCH};
use super::node::{
    encode_bset_frame, load_node, page_holds_live_frame, AppendDest, LoadedNode, NodeLayout,
    BSET_FRAME_LEN, NODE_PAGE,
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

/// Test seam (the freeze-wedge suite, 2026-08-19 field conviction; the
/// `TEST_SMO_BUILD_PAUSE_TREE` precedent): how many
/// [`CachedNode::freeze_locked`] serializations to FAIL right after
/// `begin_freeze` succeeds — the W-A window, where an encode/extend error
/// historically escaped between the lifecycle-word transition and the
/// frozen-delta publication and latched the node `AlreadyFreezing`
/// forever. Each engaged freeze decrements it. Unarmed cost: one relaxed
/// load per freeze.
pub static TEST_FREEZE_ENCODE_FAIL: AtomicU64 = AtomicU64::new(0);

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

    /// **DUR-8b + spec §6.2 item 9** — the key's durable delta-chain
    /// probe: how many `Delta` records are stacked above the newest
    /// base (`Put`/`Delete`) in THIS snapshot (the DURABLE chain depth
    /// a fold would have to apply — the publish chain cap's input,
    /// because the caller's RAM counter is reset by every
    /// metadata-cache refill), plus the newest link's §6.2 item-9
    /// `(base_version, version)` pair (`None` when the chain is empty
    /// or its head is an unversioned record) — the commit gate's
    /// durable-head name.
    ///
    /// Same gather as [`Self::lookup`], newest-first, counting only —
    /// the versions ride the fixed-offset peek
    /// ([`crate::layout_wire::layout_delta_versions`]), never a record
    /// decode.
    pub fn delta_chain_probe(&self, key: &[u8]) -> (u32, Option<(u64, u64)>) {
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
        let mut head: Option<(u64, u64)> = None;
        for r in gather {
            match r.kind {
                super::record::RecordKind::Delta => {
                    if depth == 0 {
                        head = crate::layout_wire::layout_delta_versions(r.value);
                    }
                    depth += 1;
                }
                _ => break,
            }
        }
        (depth, head)
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
    /// The owning cache's shared environment (`epoch_core`): the
    /// revalidation pair and the mutation gates. The node needs it to
    /// answer "may I be mutated?" at the ONE choke point
    /// ([`Self::apply_locked`]) — one relaxed load, and on the leaf commit
    /// path the level test short-circuits before even that.
    env: Arc<NodeEnv>,
    /// The revalidation epoch this object was **loaded under** (spec §6.8
    /// item 2). Stamped by [`NodeCache::publish_stamped`] from the
    /// pre-device-read snapshot, so a node can never claim currency for a
    /// checkpoint it was not classified against. [`UNARMED_EPOCH`] on
    /// every write mount, which is what makes the hit-path compare free.
    epoch_stamp: AtomicU64,
    addr: u64,
    node_seq: u64,
    tree_id: u8,
    level: u8,
    min_key: Vec<u8>,
    max_key: Vec<u8>,
    state: NodeState,
    snapshot: ArcSwap<NodeSnapshot>,
    dirty: crate::sqz_sync::SqzRwLock<NodeDirty>,
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
        env: Arc<NodeEnv>,
    ) -> Result<Arc<Self>, KvError> {
        let (header, buf, bset_ranges, tail_offset) = loaded.into_parts();
        let sources: Vec<Bytes> = bset_ranges.iter().map(|r| buf.slice(r.clone())).collect();
        let base = Arc::new(RecordIndex::build(sources)?);
        Ok(Arc::new(Self {
            env,
            epoch_stamp: AtomicU64::new(UNARMED_EPOCH),
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
            dirty: crate::sqz_sync::SqzRwLock::new(NodeDirty {
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

    /// The revalidation epoch this object was loaded under (spec §6.8
    /// item 2). One relaxed load — the hit path's whole share of the
    /// reader machinery.
    #[inline]
    pub fn epoch_stamp(&self) -> u64 {
        self.epoch_stamp.load(Ordering::Relaxed)
    }

    /// Stamp the load epoch. Called exactly once, by
    /// [`NodeCache::publish_stamped`], before the object is reachable
    /// through the map.
    #[inline]
    fn stamp_epoch(&self, epoch: u64) {
        self.epoch_stamp.store(epoch, Ordering::Relaxed);
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
    pub fn lock(&self) -> &crate::sqz_sync::SqzRwLock<NodeDirty> {
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
        // ---- The partitioning gate (pre-RC engineering spec §6.2 closing,
        // §6.3; ruling D8's S8 prerequisite). This is the ONE place every
        // RAM mutation of every node passes, so it is where "which nodes may
        // this appender cache and mutate" stops being an argument and
        // becomes enforcement:
        //
        //  * an armed READER (§6.8 item 2) mutates nothing — its cache is a
        //    projection of somebody else's tree, and a local mutation would
        //    diverge from the volume it is reading with no way back;
        //  * interior nodes have ONE cacher and ONE mutator, the root
        //    authority. That is the RAM face of lock order 4b ("interior-node
        //    locks belong exclusively to the serialized per-volume
        //    checkpoint/SMO task") and of `read_partitioned_ledger`'s refusal
        //    of a non-authority record carrying tree roots. A peer appender
        //    that mutated structure would produce two divergent trees whose
        //    checkpoints destroy each other — spec §6.2 item 4's hazard.
        //
        // Leaves are deliberately NOT arbitrated here: which appender owns
        // which key range is the slot map's job (§6.2 items 4/8), and a gate
        // that pretended otherwise would be mistaken for cross-writer
        // custody. Cost: `level > 0` short-circuits the leaf commit path
        // before the word is even read, and on a solo volume the word is 0.
        let gate = self.env.gate.load();
        if gate.reader {
            super::META_KV_NODE_PARTITION_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Corrupt(format!(
                "mutation of node {:#x} refused: this mount armed reader revalidation \
                 (spec §6.8 item 2) and may not write the volume it is reading",
                self.addr
            )));
        }
        if self.level > 0 && !gate.is_authority() {
            super::META_KV_NODE_PARTITION_REFUSALS.fetch_add(1, Ordering::Relaxed);
            return Err(KvError::Corrupt(format!(
                "structural mutation of interior node {:#x} (level {}) refused: appender {} \
                 of {} is not the volume's root authority — interior nodes have ONE cacher \
                 (spec §6.2 closing: partitioning, not cache coherence; lock order 4b)",
                self.addr, self.level, gate.writer_id, gate.writers
            )));
        }
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
        // Test seam: fail the serialization right AFTER the lifecycle
        // word transitioned — the exact W-A window the 2026-08-19 field
        // wedge escaped through (an oversize record failing
        // `encode_bset_frame` below).
        if TEST_FREEZE_ENCODE_FAIL
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
            .is_ok()
        {
            return Err(KvError::Corrupt(format!(
                "test seam: injected freeze-encode failure on node {:#x}",
                self.addr
            )));
        }
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
// Reader-side revalidation (pre-RC engineering spec §6.8 item 2).
// ---------------------------------------------------------------------------

/// One durable checkpoint, as a reader observes it: the projection of an
/// A/B root-ledger record ([`LedgerRecord`]) that a node cache needs.
///
/// **Reading one is the whole poll**: a single 128 KiB `read_at` of the
/// ledger extent (`checkpoint::read_newest_ledger`), no tree traversal, no
/// journal replay, no locks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootEpoch {
    /// Checkpoint sequence — the epoch identity. Strictly monotonic per
    /// volume, and (crucially for the cadence being free) written only by a
    /// cycle that had work: `checkpoint.rs::tick` runs a cycle only when
    /// `final_cycle || dirty_nodes > 0 || distance > 0`, so an idle writer
    /// mints no records and a reader's polls stay inert.
    pub ledger_seq: u64,
    /// The record's replay tail — the §4.5 torn-tail classifier's input for
    /// every node this reader loads from now on.
    pub journal_tail_seq: u64,
    /// §4.8 monotonic ino watermark (informational for a reader; the ino
    /// minting half is spec §6.2 item 5, a sibling's).
    pub next_ino: u64,
    /// §4.5 node-seq mint watermark: **a peer's structural mint counter.**
    /// A non-authority appender can compare it against its own mints to
    /// learn that structure moved under it — see the partitioning argument
    /// in the module docs.
    pub node_seq_watermark: u64,
    /// §4.7 allocator bitmap generation (informational for a reader).
    pub alloc_bitmap_generation: u64,
    /// Per-tree roots the record names.
    pub roots: Vec<TreeRoot>,
}

impl RootEpoch {
    /// Project a ledger record. On a partitioned volume the record to
    /// project is the **root authority's**
    /// (`PartitionedLedger::authority`), because tree roots live only on
    /// its records (spec §6.2 item 4).
    pub fn from_ledger(rec: &LedgerRecord) -> Self {
        Self {
            ledger_seq: rec.seq,
            journal_tail_seq: rec.journal_tail_seq,
            next_ino: rec.next_ino,
            node_seq_watermark: rec.node_seq_watermark,
            alloc_bitmap_generation: rec.alloc_bitmap_generation,
            roots: rec.tree_roots.clone(),
        }
    }

    /// An epoch built without a ledger record: `(ledger_seq, tail, roots)`.
    /// For callers that have the facts but not the record — the cache-level
    /// contracts, and the S4 wiring's `writers == 1` seed.
    pub fn synthetic(ledger_seq: u64, journal_tail_seq: u64, roots: &[TreeRoot]) -> Self {
        Self {
            ledger_seq,
            journal_tail_seq,
            next_ino: 0,
            node_seq_watermark: 0,
            alloc_bitmap_generation: 0,
            roots: roots.to_vec(),
        }
    }

    /// The root this record names for `tree_id`.
    pub fn root_of(&self, tree_id: u8) -> Option<TreeRoot> {
        self.roots.iter().copied().find(|r| r.tree_id == tree_id)
    }
}

/// The **remote trigger** for the R-6 unified block-key purge (spec §6.8
/// item 5: *"the invalidation primitive already exists and is complete;
/// only the remote trigger is missing"*).
///
/// A metadata epoch advance is the reader's only evidence that the writer
/// may have freed, reallocated, or rewritten data blocks whose bytes the
/// reader's block-key-addressed tiers still hold (§6.3's block-key binding
/// hazard). The cache fires this once per advance; **scope selection
/// belongs to the implementation**, because bounding it exactly is the
/// §6.8 item-3 freed-offset grace period — a separate, weeks-scale item.
/// The shipped implementation is
/// [`super::revalidate::TieredEpochPurge`], which routes every suspect key
/// through `TieredCache::purge_block_key` and nothing else.
pub trait EpochPurgeSink: Send + Sync + std::fmt::Debug {
    /// Purge whatever the epoch step invalidated; return the number of
    /// block keys purged (surfaced as `meta_kv_revalidate_keys_purged`).
    fn on_epoch_advance(&self, from_epoch: u64, to_epoch: u64) -> u64;
}

/// What one revalidation poll did — the RO mount's per-poll ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RevalidateOutcome {
    /// Whether the polled record was newer (an epoch step). `false` = an
    /// inert poll: nothing was dropped, nothing purged, no `Arc` replaced.
    pub advanced: bool,
    /// The epoch before this poll.
    pub from_epoch: u64,
    /// The epoch now in force (unchanged when `!advanced`).
    pub epoch: u64,
    /// Durable tail now in force.
    pub tail: u64,
    /// Mappings the drop pass released.
    pub dropped: u64,
    /// Extent bytes credited back to the budget gauge — exactly
    /// `dropped × node_size`, which is the charge-conservation law.
    pub bytes_credited: u64,
    /// Mappings kept because they were already stamped with the new epoch
    /// (a loader that raced the advance).
    pub retained: u64,
    /// **Must stay 0**: dirty nodes the pass refused to drop.
    pub skipped_dirty: u64,
    /// Block keys the R-6 purge sink dropped for this step.
    pub keys_purged: u64,
}

/// Bounded re-reads for a reader whose extent read raced the writer's
/// in-flight append. The writer's node writes are single `write_at`s, so a
/// reader's whole-extent read can legitimately observe a torn frame
/// followed by a complete one — the shape §4.5 calls corruption on a
/// crashed writer. Three re-reads with a yield between them close the
/// window (the write is already submitted); a verdict that survives them is
/// evidence, not a race, and is returned unchanged.
const READER_LOAD_RETRIES: u32 = 3;

/// Whether a load verdict can be a live-writer artifact rather than
/// corruption: the §4.5 loud tear classification and a header/bset checksum
/// mismatch (a half-landed `write_node` image). Everything else — geometry,
/// self-address, short reads — is structural and never retried.
fn verdict_may_be_a_write_race(e: &KvError) -> bool {
    matches!(
        e,
        KvError::CheckpointCoveredBsetAfterTear { .. } | KvError::ChecksumMismatch { .. }
    )
}

/// Run `load` and, in reader mode only, retry a verdict that a racing
/// append could have produced (see [`READER_LOAD_RETRIES`]).
async fn retry_racing_reader_load<T, F, Fut>(
    reader_mode: bool,
    addr: u64,
    mut load: F,
) -> Result<T, KvError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, KvError>>,
{
    let mut attempt = 0u32;
    loop {
        match load().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !reader_mode
                    || attempt >= READER_LOAD_RETRIES
                    || !verdict_may_be_a_write_race(&e)
                {
                    return Err(e);
                }
                attempt += 1;
                super::META_KV_READER_LOAD_RETRIES.fetch_add(1, Ordering::Relaxed);
                log::debug!(
                    "reader load of node {addr:#x} hit {e} on attempt {attempt} — \
                     re-reading (a writer's in-flight append looks exactly like this)"
                );
                squeezefs_ipc::sqz_blocking::yield_now().await;
            }
        }
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
    tx: Arc<squeezefs_ipc::sqz_flight::Sender<()>>,
}

impl Drop for InflightLoadGuard<'_> {
    fn drop(&mut self) {
        self.cache
            .inflight
            .remove_if_sync(&self.addr, |tx| Arc::ptr_eq(tx, &self.tx));
        self.tx.send(());
    }
}

/// The per-volume node cache (§4.5). All extent I/O flows through the K2
/// node layer (`crate::uring_fs`, io_uring-only).
pub struct NodeCache {
    cfg: NodeCacheConfig,
    map: scc::HashMap<u64, Arc<CachedNode>>,
    inflight: scc::HashMap<u64, Arc<squeezefs_ipc::sqz_flight::Sender<()>>>,
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
    /// The shared per-cache environment (`epoch_core`): the **durable
    /// journal tail** (§4.5 torn-tail classifier input, §4.2 tombstone
    /// elision floor — K6b's checkpoint advances it, tests drive it
    /// directly) published as one ordered pair with the §6.8 item-2
    /// **revalidation epoch**, plus the §6.2-closing append gates. Every
    /// node holds a clone.
    env: Arc<NodeEnv>,
    /// The R-6 purge trigger a reader installs at arm time (spec §6.8
    /// item 5). `None` on every write mount — set once, never replaced.
    purge_sink: OnceLock<Arc<dyn EpochPurgeSink>>,
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
            env: Arc::new(NodeEnv::new(0)),
            purge_sink: OnceLock::new(),
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
        self.env.epoch.tail()
    }

    /// Advance the durable tail (monotonic).
    pub fn set_durable_tail(&self, tail: u64) {
        self.env.epoch.advance_tail(tail);
    }

    /// The shared node environment — what [`CachedNode::from_loaded`] takes
    /// so a node can answer the §6.2-closing mutation gates (the tree layer
    /// builds SMO successors and fresh roots itself).
    pub(crate) fn node_env(&self) -> Arc<NodeEnv> {
        self.env.clone()
    }

    // -----------------------------------------------------------------
    // Reader-side revalidation (spec §6.8 item 2). The API an RO mount
    // consumes: `arm_revalidation` once at open, then `revalidate` on the
    // derived cadence (`super::revalidate::RevalidationPoller`).
    // -----------------------------------------------------------------

    /// Declare this cache a **coherent reader** as of `epoch`, installing
    /// the optional R-6 purge trigger. Seeds the epoch and the durable tail
    /// and drops nothing — the caller's cache is current as of the record
    /// it opened.
    ///
    /// Arming is a **once-per-mount declaration with teeth**: from here on
    /// every node mutation is refused loud
    /// ([`CachedNode::apply_locked`]), because a cache that is a projection
    /// of another process's tree cannot also be authoritative. `Err` if the
    /// cache is already armed or `epoch.ledger_seq` is 0 (a volume whose
    /// ledger has no record cannot be read coherently).
    pub fn arm_revalidation(
        &self,
        epoch: &RootEpoch,
        purge: Option<Arc<dyn EpochPurgeSink>>,
    ) -> Result<(), KvError> {
        if !self.env.epoch.arm(epoch.ledger_seq) {
            return Err(KvError::Corrupt(format!(
                "node cache already armed at epoch {} (or asked to arm at 0): reader \
                 revalidation is a once-per-mount declaration",
                self.env.epoch.probe()
            )));
        }
        self.env.gate.set_reader();
        self.env.epoch.advance_tail(epoch.journal_tail_seq);
        // Nodes mapped BEFORE the arm (the roots `KvTree::open` had to load
        // to open the trees at all) carry the un-armed stamp; re-stamp them
        // into the armed epoch instead of making the reader re-read its own
        // roots on its first traversal. Sound because the open protocol
        // reads the ledger record FIRST — those nodes were loaded from a
        // device state at-or-after the record they are now stamped with,
        // which is the same "content may be a hair newer than the epoch"
        // posture every in-cycle load already has (staleness is a bound,
        // not a snapshot).
        //
        // AND absolve the bootstrap replay's dirty residue (rung-6 fleet
        // finding #1, 2026-08-15): a mount that opened into a non-empty
        // writer journal tail replayed it as DIRTY records — correct on a
        // writer, whose checkpoint task discharges the floors, but those
        // records are the WRITER's to persist and a reader has no
        // checkpoint task. A floor kept here can never be discharged, so
        // the drop pass would refuse the node on EVERY epoch step
        // (`dirty_skips` climbing, the view pinned at mount-time state —
        // an unbounded violation of the published staleness bound).
        // Absolution is sound: the records keep serving from RAM until the
        // next epoch step, and any epoch this reader adopts is a writer
        // checkpoint whose flush pass covered the very journal window the
        // replay read — or, in the deferred-flush corner (a clamped tail),
        // the dropped node re-pages to exactly the checkpoint state the S5
        // contract promises, never below it. From here on a dirty node in
        // the drop pass again means exactly what the tripwire says:
        // revalidation armed on a mount that WRITES.
        self.for_each_node(|n| {
            n.stamp_epoch(epoch.ledger_seq);
            let _ = n.take_dirty_floor();
        });
        if let Some(sink) = purge {
            let _ = self.purge_sink.set(sink);
        }
        log::info!(
            "node cache armed for coherent reads at checkpoint epoch {} (tail {}): cached \
             nodes are dropped on every epoch step, and this mount may not write",
            epoch.ledger_seq,
            epoch.journal_tail_seq
        );
        Ok(())
    }

    /// Declare this mount appender `writer_id` of `writers` on a
    /// partitioned volume (spec §6.2 item 4). Solo mounts never call it and
    /// keep word 0 — the shipped posture.
    pub fn set_appender(&self, writers: u16, writer_id: u16) -> Result<(), KvError> {
        self.env
            .gate
            .set_appender(writers, writer_id)
            .map_err(|()| {
                KvError::Corrupt(format!(
                    "illegal append partition: appender {writer_id} of {writers}"
                ))
            })
    }

    /// The epoch in force (0 = un-armed; the shipped write-mount posture).
    pub fn revalidation_epoch(&self) -> u64 {
        self.env.epoch.probe()
    }

    /// Whether a reader armed revalidation on this cache.
    pub fn is_revalidating(&self) -> bool {
        self.env.epoch.is_armed()
    }

    /// **One revalidation poll** (spec §6.8 item 2): adopt `epoch` and drop
    /// every cached node not covered by it.
    ///
    /// What "covered" means, precisely — and why it is so nearly empty:
    /// a ledger record proves currency only for the identities it names,
    /// and even a byte-identical root identity is **not** a currency proof,
    /// because a leaf (or root-leaf) append grows the extent's log while
    /// leaving `(node_addr, node_seq)` untouched. The only nodes provably
    /// current after an advance are therefore the ones **loaded under the
    /// new epoch** (a loader that raced the advance) — counted as
    /// `retained`. Everything else is dropped and demand-paged again. That
    /// is the honest cost of the cheapest credible design, and it is
    /// bounded by the poll cadence, not by the writer's checkpoint rate.
    ///
    /// Sound by construction on three edges:
    /// * **un-armed caches are inert** — `publish` refuses to advance an
    ///   un-armed epoch, so a stray call on a write mount can never drop a
    ///   mapping it owns;
    /// * **dirty nodes are never dropped** — they hold RAM records no disk
    ///   image has (`skipped_dirty` is the must-stay-0 tripwire saying
    ///   revalidation was armed on a mount that writes);
    /// * **the eager pass and the lazy hit-path gate agree by
    ///   construction** — both drop exactly "stamped ≠ current", and the
    ///   epoch is published *before* the sweep walks, so an operation that
    ///   starts after this call can never adopt a stale node even while the
    ///   pass is still walking.
    ///
    /// Charge accounting: every released mapping credits exactly one extent
    /// through the same `remove_if_sync(ptr_eq)`-gated `fetch_sub` that
    /// eviction and retire use, so racing sweepers cannot double-credit
    /// (a double credit wraps the `u64` and reads as "full forever").
    pub fn revalidate(&self, epoch: &RootEpoch) -> RevalidateOutcome {
        super::META_KV_REVALIDATE_POLLS.fetch_add(1, Ordering::Relaxed);
        let Some((from, to)) = self
            .env
            .epoch
            .publish(epoch.journal_tail_seq, epoch.ledger_seq)
        else {
            // Inert: the same (or an older) record, or an un-armed cache.
            let cur = self.env.epoch.probe();
            return RevalidateOutcome {
                advanced: false,
                from_epoch: cur,
                epoch: cur,
                tail: self.env.epoch.tail(),
                ..Default::default()
            };
        };
        super::META_KV_REVALIDATE_EPOCHS.fetch_add(1, Ordering::Relaxed);

        // The drop pass. Collected first so the map is not mutated under its
        // own iterator; `for_each_node` is explicitly not a consistent
        // snapshot, which is fine — anything published after the epoch
        // advance is stamped `to` and belongs to the new epoch.
        let mut stale: Vec<Arc<CachedNode>> = Vec::new();
        let mut retained = 0u64;
        self.for_each_node(|n| {
            if n.epoch_stamp() == to {
                retained += 1;
            } else {
                stale.push(n.clone());
            }
        });
        let node_size = self.cfg.layout.node_size() as u64;
        let mut out = RevalidateOutcome {
            advanced: true,
            from_epoch: from,
            epoch: to,
            tail: self.env.epoch.tail(),
            retained,
            ..Default::default()
        };
        for node in stale {
            if node.dirty_floor() != u64::MAX || node.lock().try_read().is_err() {
                // Unflushed RAM records, or a mutation in flight: keep it.
                // A reader has neither, so this is the tripwire.
                out.skipped_dirty += 1;
                super::META_KV_REVALIDATE_DIRTY_SKIPS.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "revalidation kept node {:#x}: it still holds un-durable records \
                     (dirty floor {}). Reader revalidation on a mount that WRITES is a \
                     bug — the pass will never drop such a node",
                    node.addr(),
                    node.dirty_floor()
                );
                continue;
            }
            if self
                .map
                .remove_if_sync(&node.addr(), |v| Arc::ptr_eq(v, &node))
                .is_some()
            {
                self.cached_bytes.fetch_sub(node_size, Ordering::AcqRel);
                out.dropped += 1;
                out.bytes_credited += node_size;
            }
        }
        super::META_KV_REVALIDATE_NODES_DROPPED.fetch_add(out.dropped, Ordering::Relaxed);

        // The clock holds one entry per publish; a reader re-publishes its
        // working set every epoch, so without this drain the ring would
        // grow one entry per reload forever (evictions only pop while over
        // budget). Re-push exactly the survivors.
        let mut survivors: Vec<u64> = Vec::new();
        while let Some(entry) = self.clock.pop() {
            let addr = **entry;
            if self.map.contains_sync(&addr) {
                survivors.push(addr);
            }
        }
        for addr in survivors {
            self.clock.push(addr);
        }

        // The R-6 remote trigger (§6.8 item 5): the metadata step is the
        // reader's only evidence that block keys may have been reused.
        if let Some(sink) = self.purge_sink.get() {
            out.keys_purged = sink.on_epoch_advance(from, to);
            super::META_KV_REVALIDATE_KEYS_PURGED.fetch_add(out.keys_purged, Ordering::Relaxed);
        }
        log::debug!(
            "revalidated node cache {} → {} (tail {}): dropped {}, retained {}, \
             skipped-dirty {}, purged {} block keys",
            from,
            to,
            out.tail,
            out.dropped,
            out.retained,
            out.skipped_dirty,
            out.keys_purged
        );
        out
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
    ///
    /// Carries the §6.8 item-2 **lazy staleness gate**: a node stamped under
    /// a superseded epoch is a MISS, so an operation that begins after a
    /// revalidation can never adopt a stale node even while the drop pass is
    /// still walking. The cost is one relaxed load of the cache's epoch word
    /// and one of the node's stamp, both already in cache; on every write
    /// mount both read [`UNARMED_EPOCH`] and the compare always agrees
    /// (priced in `benches/meta_lv_bench.rs::kv_node_cache`).
    pub fn try_get(&self, addr: u64) -> Option<Arc<CachedNode>> {
        let node = self.map.read_sync(&addr, |_, v| v.clone())?;
        if node.epoch_stamp() != self.env.epoch.probe() {
            super::META_KV_REVALIDATE_STALE_SERVES.fetch_add(1, Ordering::Relaxed);
            return None;
        }
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
                // The same lazy staleness gate as `try_get`: a stale-stamped
                // mapping must be re-read from the device, not served.
                if node.epoch_stamp() != self.env.epoch.probe() {
                    super::META_KV_REVALIDATE_STALE_SERVES.fetch_add(1, Ordering::Relaxed);
                    // Release the mapping so the demand page below re-reads
                    // it; whoever holds an Arc keeps its snapshot (§4.6).
                    if self
                        .map
                        .remove_if_sync(&addr, |v| Arc::ptr_eq(v, &node))
                        .is_some()
                    {
                        self.cached_bytes
                            .fetch_sub(self.cfg.layout.node_size() as u64, Ordering::AcqRel);
                        super::META_KV_REVALIDATE_NODES_DROPPED.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
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
                        let rx = e.get().subscribe();
                        drop(e);
                        // Sender dropped (loader done or cancelled) also
                        // wakes us; either way, re-check the map.
                        let _ = rx.wait().await;
                        continue;
                    }
                    scc::hash_map::Entry::Vacant(e) => {
                        let (tx, _rx) = squeezefs_ipc::sqz_flight::channel::<()>();
                        let tx = Arc::new(tx);
                        e.insert_entry(tx.clone());
                        InflightLoadGuard {
                            cache: self,
                            addr,
                            tx,
                        }
                    }
                }
            };
            // The §6.8 item-2 coherent pair, read ONCE and BEFORE the device
            // read (`epoch_core`'s two-word law): the epoch a node loaded now
            // may claim, and the tail its torn-tail classification uses.
            // Reading it first keeps the stamp conservative — a node can
            // never claim currency for a checkpoint whose tail it was not
            // classified against.
            let snap = self.env.epoch.load_snapshot();
            let reader_mode = snap.epoch != UNARMED_EPOCH;
            let loaded = retry_racing_reader_load(reader_mode, addr, || {
                load_node(&self.cfg.path, &self.cfg.layout, addr, snap.tail)
            })
            .await?;
            // Re-check after the read: a retire during our load means the
            // bytes we hold are the lagging image (a mapping existed until
            // [`Self::retire`] ran, and retire marks the set BEFORE
            // dropping the mapping — so a loader that missed the map is
            // guaranteed to see the mark here).
            if self.retired.contains_sync(&addr) {
                return Ok(None);
            }
            let node = CachedNode::from_loaded(
                loaded,
                false,
                self.cached_bytes.clone(),
                self.env.clone(),
            )?;
            if node.level() > 0 {
                node.pin(); // §4.5: interior nodes always pinned.
            }
            self.publish_stamped(node.clone(), snap.epoch);
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
        // A caller-built node (SMO successor, fresh root) is by definition
        // this mount's own current state, so it belongs to the epoch in
        // force. Only the demand-page path has an older, conservative
        // answer, and it passes it explicitly.
        self.publish_stamped(node, self.env.epoch.probe())
    }

    /// [`Self::publish`] with the **load epoch** stamped explicitly — the
    /// demand-page path's entry (spec §6.8 item 2): the stamp is the epoch
    /// snapshotted *before* the device read, never the (possibly newer) one
    /// in force at publish time, so a node published across a racing
    /// revalidation reads as stale and is re-read rather than trusted.
    pub(crate) fn publish_stamped(&self, node: Arc<CachedNode>, epoch: u64) {
        node.stamp_epoch(epoch);
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
        // ---- The foreign-append probe (spec §6.2 closing / §6.3), on a
        // PARTITIONED volume only. `append_bset` writes at the tail offset we
        // remember and validates only the node incarnation, so a peer that
        // appended into this node's log while we held it cached would be
        // silently overwritten — its acked records lost with no counter, the
        // exact shape the partitioned-append formats detect at *replay* and
        // could not prevent at *runtime*. One 4 KiB read of the destination
        // page turns it into a loud refusal.
        //
        // Solo volumes have no peers by construction and pay nothing: the
        // probe is behind `is_solo()`, which is word 0 on every shipped
        // mount.
        if !self.env.gate.load().is_solo() {
            let page =
                crate::uring_fs::read_at(&self.cfg.path, node.addr() + tail as u64, NODE_PAGE)
                    .await
                    .map_err(KvError::Io)?;
            if page_holds_live_frame(&page, node.node_seq()) {
                super::META_KV_NODE_PARTITION_REFUSALS.fetch_add(1, Ordering::Relaxed);
                return Err(KvError::Corrupt(format!(
                    "foreign append detected at node {:#x} offset {tail}: the destination page \
                     already holds a verified frame of this incarnation, so a peer appender \
                     wrote into a node we cache — appending here would overwrite its acked \
                     records (spec §6.2 closing: two writers must never cache the same node)",
                    node.addr()
                )));
            }
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

    /// `META_KV_FOLD_MEMO_BYTES` is a PROCESS-GLOBAL gauge, and the five
    /// tests below assert a delta against it — which is an exclusivity
    /// claim libtest does not grant: it runs tests in parallel by default.
    /// Serialized here rather than by `--test-threads=1`, because the
    /// criterion bench smoke (`cargo bench --benches -- --test`, part of
    /// the required gate) runs this lib test target with libtest's default
    /// thread count and reproduced the interference 5/5 with just the two
    /// racing tests selected (`racing_distinct_key_…` failing its
    /// "both gauges must agree" assertion on another test's live memo).
    /// The lock is held for the whole test body, including drop-credit
    /// checks — a concurrent memo's Drop is exactly what perturbs it.
    fn global_gauge_guard() -> std::sync::MutexGuard<'static, ()> {
        static GAUGE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        GAUGE.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn memo_populate_charges_both_gauges_and_drop_credits_exactly() {
        let _serialized = global_gauge_guard();
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
        let _serialized = global_gauge_guard();
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
        let _serialized = global_gauge_guard();
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
        let _serialized = global_gauge_guard();
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
        let _serialized = global_gauge_guard();
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

    // -----------------------------------------------------------------
    // The revalidation path's share of the same laws (spec §6.8 item 2).
    //
    // A drop pass is a MASS credit — the one shape most likely to lose or
    // duplicate one — so the charge conservation above is re-asserted
    // across it, and the lazy hit-path gate (which the eager sweep must
    // agree with by construction) is pinned where it can be driven
    // directly.
    // -----------------------------------------------------------------

    /// Build `n` empty single-page node images in a scratch file and load
    /// them: the cheapest real cache population (no tree, no allocator).
    async fn populated_cache(n: u64) -> (tempfile::NamedTempFile, Arc<NodeCache>, Vec<u64>) {
        use super::super::node::{write_node, NodeWriteParams, MIN_NODE_SIZE};
        let file = tempfile::NamedTempFile::new().expect("temp volume");
        let node_size = MIN_NODE_SIZE;
        file.as_file()
            .set_len((n + 2) * node_size as u64)
            .expect("size volume");
        let cache = NodeCache::new(NodeCacheConfig {
            path: file.path().to_path_buf(),
            layout: NodeLayout::new(node_size).expect("layout"),
            heap_base: 0,
            budget_bytes: (n + 2) * node_size as u64,
            writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
        });
        let mut addrs = Vec::new();
        for e in 0..n {
            let addr = cache.extent_addr(e);
            write_node(
                cache.config().path.clone(),
                &cache.config().layout,
                &NodeWriteParams {
                    node_addr: addr,
                    node_seq: e + 1,
                    tree_id: super::super::record::TREE_INODES,
                    level: 0,
                    min_key: b"",
                    max_key: &[0xff; 8],
                },
                &[],
                0,
            )
            .await
            .expect("write node image");
            cache.load(addr).await.expect("load").expect("mapped");
            addrs.push(addr);
        }
        (file, cache, addrs)
    }

    #[tokio::test]
    async fn a_node_dropped_on_revalidation_credits_its_extent_exactly_once() {
        let (_f, cache, addrs) = populated_cache(4).await;
        let node_size = cache.config().layout.node_size() as u64;
        assert_eq!(cache.cached_bytes(), 4 * node_size);
        cache
            .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), None)
            .expect("arm");

        let out = cache.revalidate(&RootEpoch::synthetic(2, 0, &[]));
        assert_eq!(out.dropped, 4);
        assert_eq!(out.bytes_credited, 4 * node_size);
        assert_eq!(
            cache.cached_bytes(),
            0,
            "a lost credit reads as 'fuller than it is' and evicts forever; \
             an over-credit WRAPS the u64 and reads as 'full forever'"
        );
        // A second pass over an empty map credits nothing (the wrap guard).
        let again = cache.revalidate(&RootEpoch::synthetic(3, 0, &[]));
        assert_eq!(again.dropped, 0);
        assert_eq!(again.bytes_credited, 0);
        assert_eq!(cache.cached_bytes(), 0);
        for a in &addrs {
            assert!(!cache.contains(*a));
        }
    }

    /// Deliberately NOT a `#[tokio::test]`: the process-global gauge guard
    /// has to be held across the WHOLE body (a concurrent memo's `Drop` is
    /// exactly what perturbs the assertion), and a std `MutexGuard` held
    /// across an `await` is both a clippy error and the wrong shape. The
    /// async work runs inside one `block_on` instead.
    #[test]
    fn a_dropped_snapshot_memo_leaves_the_global_gauge_at_its_baseline() {
        let _serialized = global_gauge_guard();
        let global0 = baseline();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let (_f, cache, addrs) = populated_cache(2).await;
            // Populate a memo cell on each node's live snapshot, then let the
            // drop pass take the snapshot with it.
            for a in &addrs {
                let node = cache.try_get(*a).expect("mapped");
                node.snapshot().memo.populate(
                    Bytes::from_static(b"memo-key"),
                    LiveLookup::Live(Bytes::from_static(b"0123456789")),
                    1,
                );
            }
            assert!(
                baseline() > global0,
                "fixture: the memos charged the global gauge"
            );
            cache
                .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), None)
                .expect("arm");
            assert_eq!(
                cache.revalidate(&RootEpoch::synthetic(2, 0, &[])).dropped,
                2
            );
            assert_eq!(
                baseline(),
                global0,
                "the memo charge is Drop-owned: it leaves with the snapshot the \
                 drop pass released"
            );
            assert_eq!(cache.cached_bytes(), 0, "and so does the extent charge");
        });
    }

    #[tokio::test]
    async fn racing_revalidations_credit_each_mapping_exactly_once() {
        let (_f, cache, _addrs) = populated_cache(8).await;
        let node_size = cache.config().layout.node_size() as u64;
        cache
            .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), None)
            .expect("arm");
        // Two threads sweep the SAME advance; scc's removal arbitration
        // must make the credit exactly-once, or the gauge wraps.
        let a = Arc::clone(&cache);
        let b = Arc::clone(&cache);
        let ta = std::thread::spawn(move || a.revalidate(&RootEpoch::synthetic(2, 0, &[])).dropped);
        let tb = std::thread::spawn(move || b.revalidate(&RootEpoch::synthetic(2, 0, &[])).dropped);
        let dropped = ta.join().expect("no panic") + tb.join().expect("no panic");
        assert_eq!(dropped, 8, "each mapping is dropped by exactly one sweeper");
        assert_eq!(cache.cached_bytes(), 0);
        assert!(
            cache.cached_bytes() < node_size,
            "and the gauge never wrapped through zero"
        );
    }

    #[tokio::test]
    async fn a_stale_stamped_node_is_never_served_by_the_hit_path() {
        let (_f, cache, addrs) = populated_cache(1).await;
        cache
            .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), None)
            .expect("arm");
        let node = cache.try_get(addrs[0]).expect("armed at the load epoch");
        // Re-publish the SAME object stamped in the previous epoch — what a
        // loader that snapshotted before the advance produces.
        cache.publish_stamped(node.clone(), 0);
        assert!(
            cache.try_get(addrs[0]).is_none(),
            "the lazy gate refuses a stale-stamped node even before the sweep \
             reaches it — the eager pass and the gate agree by construction"
        );
        // And the sweep then removes exactly that mapping, once.
        let out = cache.revalidate(&RootEpoch::synthetic(2, 0, &[]));
        assert_eq!(out.dropped, 1);
        assert_eq!(cache.cached_bytes(), 0);
    }

    #[test]
    fn the_epoch_publication_order_is_tail_then_epoch() {
        // The §6.8 item-2 two-word law (epoch_core): a loader that
        // observes epoch E must observe a tail at least as new as E's, so
        // it can never stamp a node "current as of E" after classifying
        // its torn tail against an OLDER tail (which silently drops
        // records E covers).
        let env = NodeEnv::new(10);
        assert_eq!(env.epoch.probe(), 0, "unarmed");
        assert!(env.epoch.arm(4));
        assert!(!env.epoch.arm(5), "arming is once");
        let moved = env.epoch.publish(40, 7).expect("advance");
        assert_eq!(moved, (4, 7));
        let snap = env.epoch.load_snapshot();
        assert_eq!((snap.epoch, snap.tail), (7, 40));
        assert!(
            env.epoch.publish(30, 7).is_none(),
            "a repeat epoch never advances"
        );
        assert_eq!(env.epoch.tail(), 40, "and the tail is monotone");
    }

    #[tokio::test]
    async fn a_reader_retries_a_load_that_raced_the_writers_append() {
        // A reader's 256 KiB extent read can catch a writer's single
        // `write_at` mid-flight: a torn frame followed by a complete one is
        // §4.5's loud corruption verdict on a crashed writer and a plain
        // read/append race here. Bounded re-reads turn the race into a
        // retry; a verdict that survives them is evidence.
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let out: Result<u32, KvError> = retry_racing_reader_load(true, 0x1000, || {
            let n = attempts.fetch_add(1, Ordering::AcqRel);
            async move {
                if n == 0 {
                    Err(KvError::CheckpointCoveredBsetAfterTear {
                        node_addr: 0x1000,
                        bset_offset: 4096,
                        horizon: 1,
                        durable_tail: 2,
                    })
                } else {
                    Ok(42u32)
                }
            }
        })
        .await;
        assert_eq!(out.expect("the retry succeeded"), 42);
        assert_eq!(attempts.load(Ordering::Acquire), 2, "exactly one retry");

        // A writer never retries — for it the verdict IS the contract.
        let calls = std::sync::atomic::AtomicU32::new(0);
        let err: Result<u32, KvError> = retry_racing_reader_load(false, 0x1000, || {
            calls.fetch_add(1, Ordering::AcqRel);
            async move {
                Err(KvError::CheckpointCoveredBsetAfterTear {
                    node_addr: 0x1000,
                    bset_offset: 4096,
                    horizon: 1,
                    durable_tail: 2,
                })
            }
        })
        .await;
        assert!(err.is_err());
        assert_eq!(calls.load(Ordering::Acquire), 1, "no writer-side retry");
    }
}
