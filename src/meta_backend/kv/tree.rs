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

use super::alloc_ext::{alloc_record, free_record, ExtentAllocator};
use super::bset::{compact, BsetView};
use super::journal::{entry_len_for, tag_for, JournalRing};
use super::node::{key_successor, load_node, split_node, write_node, NodeWriteParams, SplitDest};
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

/// Test seam (docs/design-smo-replay-currency.md §6 PR 1; the
/// `TEST_CONVEYOR_POISON_APPLY_INO` precedent): arm with a tree id
/// (`TREE_INODES`/`TREE_DENTRIES`/`TREE_XATTRS`) to park the next
/// `smo_replace` on that tree **in its build window** — after the
/// successor images are written and loaded back, before the §4.6
/// parent-then-child lock window — so a test can inject racing commits
/// whose journal reservations land *below* the SMO's in-lock flip
/// reservation. That is exactly the sub-mechanism (i) stranding shape
/// (design §1): the racing records reach the successors only as
/// `take_overlay()` leftovers whose RAM copies die with the process,
/// while single-pass replay routes them to the abandoned predecessor.
/// `0` = off; unarmed cost is one relaxed load per SMO.
pub static TEST_SMO_BUILD_PAUSE_TREE: AtomicU64 = AtomicU64::new(0);

/// While an SMO is parked on [`TEST_SMO_BUILD_PAUSE_TREE`], the paused
/// node's identity — the test reads the key range to target its racing
/// commits. `None` whenever no SMO is parked. Written only on the armed
/// path (zero cost unarmed).
pub static TEST_SMO_BUILD_PAUSED: once_cell::sync::Lazy<
    std::sync::Mutex<Option<TestSmoPauseInfo>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// The parked-SMO wake for [`TEST_SMO_BUILD_PAUSE_TREE`] (register-recheck
/// discipline — a stale release can never strand the SMO task).
static TEST_SMO_BUILD_NOTIFY: once_cell::sync::Lazy<squeezefs_ipc::sqz_notify::Notify> =
    once_cell::sync::Lazy::new(squeezefs_ipc::sqz_notify::Notify::new);

/// Release an SMO parked on [`TEST_SMO_BUILD_PAUSE_TREE`] (disarms first;
/// the notify wakes the register-recheck loop).
pub fn test_smo_build_pause_release() {
    TEST_SMO_BUILD_PAUSE_TREE.store(0, Ordering::SeqCst);
    TEST_SMO_BUILD_NOTIFY.notify_waiters();
}

/// The paused SMO's identity published through [`TEST_SMO_BUILD_PAUSED`].
#[derive(Debug, Clone)]
pub struct TestSmoPauseInfo {
    pub tree_id: u8,
    pub level: u8,
    /// Whether the node under replacement is the tree root (a root swap
    /// journals no pointer records — design §2 C′ carve-out; the
    /// stranding contract test requires `false`).
    pub is_root: bool,
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
}

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

/// The journal side of production SMOs (PR K6b; design §4.6): every
/// structure modification reserves its interior-pointer and alloc/free
/// records from the **checkpoint-task ring reserve** (§4.4 pt 5) inside
/// the lock window and writes the entry bytes after release — the same
/// split as every commit. Successor node images are made durable
/// (coalesced barrier) *before* the swap window, so a replayed pointer
/// record can never route to a torn successor.
pub struct SmoJournal {
    /// The volume's journal ring (admission from the checkpoint reserve).
    pub ring: Arc<JournalRing>,
    /// The historical §4.7 retire tag stamped into free-record VALUES
    /// (byte-for-byte format compat): the checkpoint seq expected to stop
    /// referencing extents freed *now* — the NEXT ledger record's seq;
    /// the checkpoint task keeps it at `last_written_seq + 1`. Since the
    /// Option-A coverage fix (design-smo-replay-currency §2-A) it no
    /// longer gates release — the pending-free gate is the free record's
    /// own journal seq against the durable tail.
    pub retire_seq: Arc<AtomicU64>,
    /// The volume's group-commit barrier (successor durability).
    pub sync: Arc<crate::meta_backend::sync_coalescer::SyncCoalescer>,
    /// The volume device (barrier target).
    pub path: std::path::PathBuf,
}

impl SmoJournal {
    /// One coalesced fdatasync on the volume device.
    async fn barrier(&self) -> Result<(), KvError> {
        let path = self.path.clone();
        self.sync
            .barrier(|| {
                let path = path.clone();
                async move { crate::uring_fs::fdatasync(path).await }
            })
            .await
            .map_err(KvError::Io)
    }
}

/// The serialized SMO execution context (§4.6): **one per volume**, owned
/// by whatever drives structure modifications — the tests in K5, the
/// checkpoint/writeback task in K6b. Passing it `&mut` into every SMO
/// entry point makes concurrent SMOs a compile error, which is the whole
/// "SMOs run on one task, one at a time" rule with the borrow checker as
/// the enforcer.
pub struct SmoContext {
    alloc: Arc<ExtentAllocator>,
    /// `None` = the K5 test shape (counter seqs, no journaling, no
    /// barrier); `Some` = the K6b production shape.
    journal: Option<SmoJournal>,
}

impl SmoContext {
    pub fn new(alloc: Arc<ExtentAllocator>) -> Self {
        Self {
            alloc,
            journal: None,
        }
    }

    /// The K6b production context: SMOs journal their records through the
    /// checkpoint-task reserve and barrier successor images before the
    /// swap (§4.6).
    pub fn with_journal(alloc: Arc<ExtentAllocator>, journal: SmoJournal) -> Self {
        Self {
            alloc,
            journal: Some(journal),
        }
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
    /// The volume-shared record/node seq source this tree stamps with —
    /// what a RUNTIME tree mint ([`Self::create`] after open) must reuse
    /// so its root's `node_seq` joins the volume's one monotonic space
    /// (the kvmap PR-2 ratchet's mint; open-time mints get it from the
    /// ledger recovery).
    pub(super) fn seq_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.seq)
    }

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
        let node = CachedNode::from_loaded(
            loaded,
            true,
            cache.charge_gauge(),
            cache.heap_promise_gauge(),
            cache.node_env(),
        )?;
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

    /// **Reader-side root adoption** (pre-RC engineering spec §6.8 item 2):
    /// install the root a freshly-polled ledger record names, so the next
    /// traversal descends the writer's current tree instead of spinning its
    /// restart budget on a root pointer whose extent has been recycled.
    ///
    /// Legal **only** on a cache that armed reader revalidation — refused
    /// otherwise, because on a write mount the live root is authoritative
    /// and the ledger's is one checkpoint behind: adopting it would be time
    /// travel across every SMO since. The write path's own root swap stays
    /// where it belongs, inside `smo_replace`'s lock window.
    pub fn adopt_root(&self, root: RootPtr) -> Result<(), KvError> {
        if !self.cache.is_revalidating() {
            return Err(KvError::Corrupt(format!(
                "root adoption refused on tree {}: this mount has not armed reader \
                 revalidation, and its live root is authoritative over the ledger's \
                 (spec §6.8 item 2)",
                self.tree_id
            )));
        }
        self.root.store(Arc::new(root));
        Ok(())
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
        // Restart-reason tallies, carried into the exhaustion error: the
        // K7 1M-storm intermittent was diagnosed from exactly this shape
        // (`reasons=[0,0,0,256,0,0]` — every restart on a retired child
        // ⇒ the retire-before-route-flip ordering bug in `smo_replace`).
        // A budget exhaustion is always a protocol bug; the tallies make
        // the next one self-describing instead of a heisenbug hunt.
        let mut dbg_reasons: [u32; 5] = [0; 5];
        'restart: for attempt in 0..RETRY_BUDGET {
            if attempt > 0 {
                // Cooperative restart: every reason to be here is a racing
                // SMO's swap window (retired extent / stale seq / routing
                // hole), and the cached-node misses short-circuit
                // SYNCHRONOUSLY (`try_get` miss ⇒ `load` refusal on a
                // retired extent), so a spinning reader can burn the whole
                // budget inside one held window without ever letting the
                // SMO task finish it — "retry budget exhausted" fired
                // ~1/2 bench runs once §4.6 pt 1 threshold wakes made
                // SMOs frequent. Yielding turns the budget into 256
                // scheduling opportunities, not 256 spins.
                squeezefs_ipc::sqz_blocking::yield_now().await;
            }
            let root = self.root();
            let Some(mut cur) = (match self.cache.try_get(root.addr) {
                Some(n) => Some(n),
                None => self.cache.load(root.addr).await?,
            }) else {
                dbg_reasons[0] += 1;
                continue 'restart; // root extent retired: racing root swap
            };
            if cur.node_seq() != root.seq {
                dbg_reasons[1] += 1;
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
                let Some((_, ptr)) = snap.next_live(key, None)? else {
                    dbg_reasons[2] += 1;
                    // Routing hole: a stale snapshot raced an SMO — restart.
                    continue 'restart;
                };
                let (child_addr, child_seq) = decode_interior_value(&ptr)?;
                let child = match self.cache.try_get(child_addr) {
                    Some(n) => Some(n),
                    None => self.cache.load(child_addr).await?,
                };
                let Some(child) = child else {
                    dbg_reasons[3] += 1;
                    continue 'restart; // retired extent: stale route
                };
                if child.node_seq() != child_seq {
                    dbg_reasons[4] += 1;
                    continue 'restart; // §4.2 stale pointer
                }
                cur = child;
            }
        }
        Err(KvError::Corrupt(format!(
            "traversal retry budget exhausted descending to level {target_level} \
             (routing loop — SMO protocol bug) restarts \
             [root-retired, root-seq, routing-hole, child-retired, child-seq] \
             = {dbg_reasons:?}"
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

    /// **DUR-8b + spec §6.2 item 9** — the DURABLE delta-chain probe of
    /// `key`: how many `Delta` records sit above its newest base (the
    /// chain cap consults this instead of trusting a RAM counter a
    /// metadata-cache refill zeroes) plus the newest link's item-9
    /// `(base_version, version)` pair (the commit gate's durable-head
    /// name; `None` = bare base or unversioned head).
    pub async fn delta_chain_probe(
        &self,
        key: &[u8],
    ) -> Result<(u32, Option<(u64, u64)>), KvError> {
        let leaf = self.resolve_leaf(key).await?;
        Ok(leaf.snapshot().delta_chain_probe(key))
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
        self.apply_at_seq(leaf, key, kind, value, None).await
    }

    /// The shared lock-window protocol behind [`Self::apply_at`] (fresh
    /// seq assigned inside the window) and [`Self::apply_replayed`] (the
    /// record's journal seq reproduced — mount replay, PR K6a).
    ///
    /// `replay` carries `(record seq, entry start)`: the floor
    /// contribution rounds DOWN to the record's journal-entry start
    /// (FIND-SMO-TAIL §1b — see [`CachedNode::apply_locked`]). A fresh
    /// single-record apply is its own entry start.
    async fn apply_at_seq(
        &self,
        leaf: &Arc<CachedNode>,
        key: &[u8],
        kind: RecordKind,
        value: Bytes,
        replay: Option<(u64, u64)>,
    ) -> Result<ApplyOutcome, KvError> {
        let mut guard = leaf.lock().write().await;
        if leaf.state().is_superseded() || key < leaf.min_key() || key > leaf.max_key() {
            drop(guard);
            super::META_KV_COMMIT_SMO_RETRIES.fetch_add(1, Ordering::Relaxed);
            return Ok(ApplyOutcome::Stale);
        }
        let (seq, floor) = match replay {
            Some((seq, entry_start)) => {
                // Per-key LWW replay gate (§4.2 "replay fold"): an
                // old-ledger mount (mid-checkpoint kill) reads node bsets
                // that already MATERIALIZED part of the replay window —
                // writeback appends and SMO rewrites land in extents the
                // previous ledger still references. Re-applying such a
                // record would append an overlay record that sorts BELOW
                // the base for its key, breaking the newest-first fold
                // order (positionally-newer-but-seq-older = stale-value
                // LWW). A record whose seq is ≤ the node's newest for the
                // key is already folded in — skip it; the outcome is
                // identical by the fold theorem. Mount replay is
                // single-threaded and the checkpoint task is not running,
                // so the snapshot is exact under this lock.
                if leaf.snapshot().newest_seq_of(key).is_some_and(|n| n >= seq) {
                    return Ok(ApplyOutcome::Applied);
                }
                (seq, entry_start)
            }
            None => {
                let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
                (seq, seq)
            }
        };
        leaf.apply_locked(
            &mut guard,
            vec![OwnedRec::new(Bytes::copy_from_slice(key), seq, kind, value)],
            floor,
        )?;
        let over_threshold = guard.overlay_bytes() >= self.cache.config().writeback_delta_bytes;
        drop(guard);
        if over_threshold {
            self.maintenance.push(leaf.addr());
        }
        Ok(ApplyOutcome::Applied)
    }

    /// Mount-time replay apply (PR K6a; design §4.2 "Replay fold" / §4.5
    /// "read-only replay into the cache"): resolve → lock → revalidate →
    /// apply, exactly the commit path, except the record carries **its
    /// journal seq** — per-key LWW by seq must reproduce the pre-crash RAM
    /// history, so replay never mints fresh seqs. The tree's shared seq
    /// counter is floored above the replayed seq so post-replay
    /// assignments (K6b commits; K5-style test mutations) stay newer.
    /// `entry_start` is the record's journal-entry start — the floor
    /// contribution (FIND-SMO-TAIL §1b rounding).
    pub async fn apply_replayed(
        &self,
        key: &[u8],
        seq: u64,
        kind: RecordKind,
        value: Bytes,
        entry_start: u64,
    ) -> Result<(), KvError> {
        self.check_key(key)?;
        self.seq.fetch_max(seq, Ordering::AcqRel);
        for _ in 0..RETRY_BUDGET {
            let leaf = self.resolve_leaf(key).await?;
            match self
                .apply_at_seq(&leaf, key, kind, value.clone(), Some((seq, entry_start)))
                .await?
            {
                ApplyOutcome::Applied => return Ok(()),
                ApplyOutcome::Stale => continue,
            }
        }
        Err(KvError::Corrupt(
            "replay retry budget exhausted (revalidation never passed — SMO protocol bug)"
                .to_string(),
        ))
    }

    /// Mount-time replay of an SMO's **interior-pointer record** (K6b;
    /// §4.6 "replay applies the pointer record first"): route to the
    /// level-`level` node covering `key` and apply with the record's
    /// journal seq. Returns `false` — dropped, never loud — when the
    /// mounted structure cannot route it: the selected (older) ledger may
    /// predate a root growth, in which case the mounted tree is shorter
    /// than the crashed one and the pointer's target parent does not
    /// exist. That drop is sound: content records replay **by key**
    /// through the old routing, so the folded state is consistent; the
    /// successor extents the pointer named stay allocated-but-unreferenced
    /// (a bounded crash-window leak, reclaimed by a future fsck — not
    /// corruption). Mount replay is single-threaded and the checkpoint
    /// task is not yet running, so touching interior locks here cannot
    /// collide with the §4.6 SMO-only lock population.
    pub async fn apply_replayed_interior(
        &self,
        key: &[u8],
        level: u8,
        seq: u64,
        kind: RecordKind,
        value: Bytes,
        entry_start: u64,
    ) -> Result<bool, KvError> {
        self.check_interior_key(key)?;
        self.seq.fetch_max(seq, Ordering::AcqRel);
        if self.root_level().await? < level {
            return Ok(false); // shorter mounted structure: unroutable
        }
        for _ in 0..RETRY_BUDGET {
            let target = self.descend(key, level).await?;
            match self
                .apply_at_seq(&target, key, kind, value.clone(), Some((seq, entry_start)))
                .await?
            {
                ApplyOutcome::Applied => return Ok(true),
                ApplyOutcome::Stale => continue,
            }
        }
        Err(KvError::Corrupt(
            "interior replay retry budget exhausted (no SMO can be running at mount)".to_string(),
        ))
    }

    /// Enqueue a node for the next maintenance pass (the K6b commit
    /// pipeline applies records through the backend's multi-leaf lock
    /// window rather than [`Self::apply_at`], so it reports writeback
    /// pressure here; duplicates are benign).
    pub(crate) fn enqueue_maintenance(&self, addr: u64) {
        self.maintenance.push(addr);
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

    /// Interior (separator) keys live in a wider domain than content
    /// keys: a child's inclusive `max_key` — the rightmost sibling's is
    /// exactly `KEY_SPACE_MAX` (the K2/K5 gap-free partition rule), and
    /// every rightmost-leaf SMO journals a pointer record under that
    /// key. Replay must accept it (§4.1: nothing inside the window fails
    /// a mount loud — and this record is a correct artifact, not
    /// damage), so the guard admits the top separator inclusively.
    fn check_interior_key(&self, key: &[u8]) -> Result<(), KvError> {
        if key.is_empty() || key > &KEY_SPACE_MAX[..] {
            return Err(KvError::Corrupt(format!(
                "interior separator key must be non-empty and sort at-or-below \
                 KEY_SPACE_MAX (len {})",
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
                // Bounded by `end`: the walk must never fold tombstones
                // beyond the requested window (the chain probes' desert
                // cliff — see `next_live`'s doc).
                match snap.next_live(&pos, Some(end))? {
                    Some((k, v)) => {
                        pos = key_successor(&k);
                        out.push((k, v));
                    }
                    None => break,
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
        self.run_maintenance_until(ctx, None).await
    }

    /// [`Self::run_maintenance`] bounded by a deadline (finding 49): pops
    /// until the queue is empty OR `deadline` has passed — always at least
    /// one entry, so every pass makes progress. The checkpoint task runs
    /// its threshold drains under its cadence period: a storm that
    /// re-enqueues faster than the drain pops (every pop is a device
    /// round trip; an SMO is several) would otherwise hold the drain open
    /// indefinitely and the cadence tick — the only path to a ring-
    /// pressure checkpoint — behind it. Work left queued is the caller's
    /// to re-arm.
    pub(crate) async fn run_maintenance_until(
        &self,
        ctx: &mut SmoContext,
        deadline: Option<std::time::Instant>,
    ) -> Result<MaintenanceOutcome, KvError> {
        let mut out = MaintenanceOutcome::default();
        let mut first = true;
        while let Some(entry) = self.maintenance.pop() {
            let addr = **entry;
            if !first && deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                // Past the budget: hand the entry back for the next pass.
                self.maintenance.push(addr);
                break;
            }
            first = false;
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

    /// The checkpoint's per-node flush step (§4.6 pts 1–2): freeze the
    /// open delta and **take the dirty floor in the same lock window**
    /// (no applied record can slip between them), then append outside the
    /// lock — full logs compact/split through the SMO path. On failure
    /// the floor is restored, so the tail rule keeps respecting the
    /// records this pass could not make durable.
    pub(crate) async fn checkpoint_flush_node(
        &self,
        ctx: &mut SmoContext,
        addr: u64,
    ) -> Result<(), KvError> {
        let Some(node) = self.cache.try_get(addr) else {
            return Ok(()); // evicted/retired since the dirty walk
        };
        if node.state().is_superseded() || node.tree_id() != self.tree_id {
            return Ok(());
        }
        let (frozen, floor) = {
            let mut guard = node.lock().write().await;
            let frozen = node.freeze_locked(&mut guard, &self.cache.config().layout)?;
            let floor = node.take_dirty_floor();
            (frozen, floor)
        };
        if frozen.is_none() {
            // Nothing to write: either clean, or its bytes were already
            // appended by threshold maintenance — the caller's barrier
            // covers those appends, so the cleared floor is exact.
            return Ok(());
        }
        let out = async {
            if self.cache.append_frozen(&node).await? {
                return Ok(());
            }
            // Log area full ⇒ compact / split (§4.6 pt 1; smo_replace
            // owns the SMO counters). Restore the taken floor FIRST
            // (FIND-VS-A): the SMO retires this node, and `retire`'s
            // dying-floor fold is what keeps the tail clamped to the
            // frozen delta's records until a ledger record written after
            // the swap covers them — a floor still parked in this frame's
            // local would die silently with the object.
            node.restore_dirty_floor(floor);
            let mut o = MaintenanceOutcome::default();
            // Forced retirement: the flush pass is what discharges
            // tail-pinning floors — its compaction must never be refused
            // FIFO room (the §4.7 cycle-break; theorem on smo_replace).
            self.smo_replace(ctx, &node, &mut o, true).await
        }
        .await;
        if out.is_err() {
            node.restore_dirty_floor(floor);
        }
        out
    }

    /// PR VL7 (design-volume-lifecycle §5.7 D4): the **compaction nudge**
    /// — fold one node's serialized log through [`Self::smo_replace`]
    /// regardless of log fullness. Never a new compactor: the successor
    /// build, journaling, retire, and counters are `smo_replace` verbatim,
    /// and the caller holds the per-volume SMO mutex (the same
    /// serialization domain as the checkpoint task — lattice 4b respected
    /// by delegation). Returns `false` when the node vanished / was
    /// superseded since the census (idempotent no-op, exactly like a
    /// spurious maintenance enqueue). Freeze + floor discipline mirrors
    /// [`Self::checkpoint_flush_node`]'s compaction arm: the floor is
    /// restored BEFORE the SMO so the retire's dying-floor fold keeps the
    /// tail clamped (FIND-VS-A).
    pub(crate) async fn compact_node_forced(
        &self,
        ctx: &mut SmoContext,
        addr: u64,
        out: &mut MaintenanceOutcome,
    ) -> Result<bool, KvError> {
        let Some(node) = self.cache.try_get(addr) else {
            return Ok(false); // evicted/retired since the census
        };
        if node.state().is_superseded() || node.tree_id() != self.tree_id {
            return Ok(false);
        }
        // W-B drop guard (the 2026-08-19 AlreadyFreezing wedge's sibling
        // window): the forced-freeze bit is OWNED — `end_freeze` on drop
        // unless the success path disarms, so a dropped future (job
        // cancel) or an unwind mid-`smo_replace` can no longer leave
        // `FREEZING` latched on a live node (every later freeze then
        // refused `AlreadyFreezing` until remount). A SUPERSEDED node is
        // skipped: the SMO consumed the freeze in its lock window and
        // `smo_replace`'s own `end_freeze` bookkeeping ran (or the bit
        // is terminal-moot) — ending it again is the double-`end_freeze`
        // the state core's debug_assert refuses.
        struct ForcedFreezeGuard {
            node: Arc<CachedNode>,
            armed: bool,
        }
        impl Drop for ForcedFreezeGuard {
            fn drop(&mut self) {
                if self.armed && !self.node.state().is_superseded() {
                    self.node.state().end_freeze();
                }
            }
        }
        let (floor, mut forced_guard) = {
            let mut guard = node.lock().write().await;
            let frozen = node.freeze_locked(&mut guard, &self.cache.config().layout)?;
            // No delta to swap (the common nudge target — a clean node
            // with a dead-heavy log): enter FREEZING through the forced
            // transition so the SMO's supersede/end_freeze bookkeeping
            // sees a freeze-borne source (empty frozen delta).
            // (`freeze_locked` returns the EXISTING frozen delta when one
            // is in flight, so `None` ⇔ truly nothing frozen.)
            let forced = if frozen.is_none() {
                if node.state().begin_forced_freeze().is_err() {
                    return Ok(false); // superseded/racing — the census was stale
                }
                true
            } else {
                false
            };
            (
                node.take_dirty_floor(),
                ForcedFreezeGuard {
                    node: Arc::clone(&node),
                    armed: forced,
                },
            )
        };
        node.restore_dirty_floor(floor);
        // Admission posture (not the flush pass): the defrag nudge keeps
        // the FIFO valve + its bounded cycle-and-retry protocol.
        let out = self.smo_replace(ctx, &node, out, false).await;
        if out.is_ok() {
            // The SMO consumed the freeze (supersede + its own
            // `end_freeze`): the guard stands down.
            forced_guard.armed = false;
        }
        out?;
        Ok(true)
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
        // Phase 3: the log is full — compact; oversized ⇒ split (§4.6
        // pt 1). Admission posture: threshold SMOs keep the §4.7 FIFO
        // valve (at cap they refuse pre-swap and the maintenance arms
        // force progress-audited checkpoint cycles).
        self.smo_replace(ctx, &node, out, false).await
    }

    /// The §4.6 three-step node replacement. `node` is frozen (its
    /// unappended delta rides `extra_records`) and stays mapped until the
    /// swap below.
    ///
    /// Production journaling (K6b, `SmoContext::with_journal`): successor
    /// images are barriered durable **before** the swap window (a
    /// replayed pointer record must never route to a torn successor);
    /// ring admission for the SMO's records (interior pointers +
    /// alloc/free) is drawn from the checkpoint-task reserve **before**
    /// any lock (§4.4 pt 5 / §4.9 4b — never wait on ring space under a
    /// node lock; on refusal the caller runs a minimal drain and
    /// retries); the reservation happens **inside** the parent-then-child
    /// lock window and the entry bytes are written after release — the
    /// same split as every commit (§4.4 pt 2).
    ///
    /// `forced_retirement` selects the §4.7 pending-free posture (the
    /// cycle-break, P2 2026-07-26 §9 fix direction a):
    ///
    /// - `false` (threshold maintenance, the defrag nudge): the FIFO
    ///   headroom valve applies — at cap the SMO refuses PRE-swap
    ///   ([`KvError::PendingFreeFull`]) and the caller forces a
    ///   progress-audited checkpoint cycle. This is §4.7's "pressure
    ///   forces a checkpoint rather than unsafe reuse" pressure valve.
    /// - `true` (the checkpoint cycle's flush pass, exclusively): the
    ///   retirement is parked UNCONDITIONALLY — at cap it goes to the
    ///   allocator's unbounded overflow, gated on the same durable tail.
    ///   The flush pass's compactions are precisely the SMOs that
    ///   discharge tail-pinning dirty floors; refusing one at cap closed
    ///   the dependency cycle {parked frees await the tail} → {the tail
    ///   awaits this node's floor} → {the floor awaits this SMO} → {the
    ///   SMO awaits FIFO room} → {FIFO room awaits the parked frees} —
    ///   the wedge that livelocked `checkpoint_now` callers, latched the
    ///   maintenance arms' loud terminal on a RESOLVABLE shape, and
    ///   persisted across remounts (the preserved 2026-07-26 md-storm
    ///   image).
    ///
    /// **Progress theorem for the forced arm** (why no schedule can
    /// re-close the cycle): in any barriered checkpoint cycle at head
    /// `H`, the flush pass visits every dirty node once and — with
    /// `forced_retirement` — NO visit is refused for FIFO reasons, so
    /// every dirty floor `< H` is discharged (appended, or compacted
    /// with the old floor folded into the cycle's dying-floor clamp,
    /// which pins that cycle's tail but dies WITH that cycle's ledger
    /// record). Every floor live at the NEXT cycle therefore belongs to
    /// records ≥ H, so the next barriered cycle's tail is ≥ min(H, the
    /// oldest in-flight reservation) — past every retirement gate parked
    /// before H (gates are journal seqs < H by monotonicity). Hence any
    /// parked retirement is released after at most TWO barriered cycles
    /// unless the tail is pinned by something no flush can discharge (a
    /// stuck in-flight reservation) — which is exactly the shape the
    /// checkpoint cycle's bounded progress audit still fails loud.
    /// Overflow occupancy is bounded by the same argument: entries
    /// parked at cycle k drain at cycle k+1's barrier, so it never
    /// exceeds ~one flush pass of SMO retirements (≤ the dirty-node
    /// checkpoint cap).
    async fn smo_replace(
        &self,
        ctx: &mut SmoContext,
        node: &Arc<CachedNode>,
        out: &mut MaintenanceOutcome,
        forced_retirement: bool,
    ) -> Result<(), KvError> {
        let cfg = self.cache.config();
        let layout = &cfg.layout;
        let durable_tail = self.cache.durable_tail();

        // ---- Step 1: build successors from the frozen snapshot, no locks.
        let src = load_node(&cfg.path, layout, node.addr(), durable_tail).await?;
        // FIND-VS-A fold-source guard: the successors are folded from THIS
        // extent's on-disk log + THIS object's frozen delta. If the disk
        // image belongs to a different incarnation (extent reuse racing a
        // stale maintenance address) or its log view disagrees with the
        // object's own append cursor, folding it would build successors
        // missing acked records — the silent-loss shape the 2026-07-16
        // storm forensics caught (split successors missing predecessor
        // keys). Fail the SMO loud instead; the caller restores the floor
        // and the next cycle retries against a coherent view.
        let extra: Vec<Record> = {
            let guard = node.lock().read().await;
            if src.header().node_seq != node.node_seq() {
                return Err(KvError::Corrupt(format!(
                    "SMO fold-source incarnation mismatch at {:#x}: disk image has \
                     node_seq {}, the live object is {} — refusing to fold a stale \
                     source (acked records would be dropped)",
                    node.addr(),
                    src.header().node_seq,
                    node.node_seq()
                )));
            }
            if src.tail_offset() != guard.tail_offset() {
                return Err(KvError::Corrupt(format!(
                    "SMO fold-source log-view mismatch at {:#x}: disk walk ends at \
                     {}, the live object's append cursor is at {} — refusing to fold \
                     a diverged source (acked records would be dropped)",
                    node.addr(),
                    src.tail_offset(),
                    guard.tail_offset()
                )));
            }
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
        let usable = layout.fold_capacity();
        let total: usize = folded.iter().map(|r| r.record_ref().encoded_len()).sum();
        let parts: Vec<&[Record]> = if total <= usable {
            vec![&folded[..]]
        } else {
            partition_records(&folded, layout.split_part_capacity())
        };

        // Claim fresh extents (internal class, §4.7) + write images.
        // The whole fallible build — claims, image writes, load-backs, and
        // the optional new root — runs inside one block so ANY error
        // releases every claimed-but-unpublished extent (nothing routes
        // to them yet). Pre-fix, a mid-build failure (e.g. the second
        // claim of a split hitting NoSpace on a heap drained to zero
        // during ENOSPC recovery — the preserved md-storm image's shape)
        // leaked the earlier claims' bits until remount: poison exactly
        // when extents are scarcest.
        let mut claimed_extents: Vec<u64> = Vec::new();
        let build_out: Result<
            (
                Vec<(u64, u64)>,
                Vec<Arc<CachedNode>>,
                Option<Arc<CachedNode>>,
            ),
            KvError,
        > = async {
            let mut written: Vec<(u64, u64)> = Vec::new(); // (addr, node_seq)
            let mut claim = |ctx: &SmoContext, cache: &NodeCache| -> Result<u64, KvError> {
                let extent = ctx.alloc.claim_internal()?;
                claimed_extents.push(extent);
                Ok(cache.extent_addr(extent))
            };
            if parts.len() == 1 {
                let dst = claim(ctx, &self.cache)?;
                let dst_seq = self.next_seq();
                super::node::compact_node(
                    &cfg.path,
                    layout,
                    &src,
                    &extra,
                    dst,
                    dst_seq,
                    durable_tail,
                )
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
                successors.push(CachedNode::from_loaded(
                    loaded,
                    pinned,
                    self.cache.charge_gauge(),
                    self.cache.heap_promise_gauge(),
                    self.cache.node_env(),
                )?);
            }

            // Test seam (docs/design-smo-replay-currency.md §6 PR 1): park an
            // armed build HERE — successor images are fixed on disk, no locks
            // are held, and the flip's reservation is not yet taken — so user
            // commits injected while parked reserve BELOW the flip's seq and
            // reach the successors only as `take_overlay()` leftovers: the
            // sub-mechanism (i) stranding window, held open deterministically.
            // Unarmed cost: one relaxed load per SMO.
            if TEST_SMO_BUILD_PAUSE_TREE.load(Ordering::Relaxed) == u64::from(self.tree_id) {
                *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = Some(TestSmoPauseInfo {
                    tree_id: self.tree_id,
                    level: node.level(),
                    is_root: self.is_root(node),
                    min_key: node.min_key().to_vec(),
                    max_key: node.max_key().to_vec(),
                });
                while TEST_SMO_BUILD_PAUSE_TREE.load(Ordering::Relaxed) == u64::from(self.tree_id) {
                    let notified = TEST_SMO_BUILD_NOTIFY.notified();
                    if TEST_SMO_BUILD_PAUSE_TREE.load(Ordering::Relaxed) != u64::from(self.tree_id)
                    {
                        break;
                    }
                    notified.await;
                }
                *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = None;
            }

            // A multi-way replacement of the root needs a new root above the
            // successors — written before any lock is taken.
            let new_root: Option<Arc<CachedNode>> = if self.is_root(node) && written.len() > 1 {
                let dst = claim(ctx, &self.cache)?;
                let dst_seq = self.next_seq();
                let recs: Vec<Record> = successors
                    .iter()
                    .map(|s| {
                        // RECORD seq 0 — the builder's separator convention.
                        // Record seqs are the JOURNAL-domain per-key fold
                        // order; `self.next_seq()` is the NODE-seq counter
                        // (a different domain, uuid-based since the quick-
                        // reformat burial fix). Stamping node seqs here made
                        // every later pointer-flip record (ring-position seq)
                        // fold BELOW the bootstrap separator — a permanent
                        // stale route to a retired extent (the storm test's
                        // child-retired loop). A fresh root has no earlier
                        // records for these keys, so 0 is exact.
                        Record::put(
                            s.max_key().to_vec(),
                            0,
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
                Some(CachedNode::from_loaded(
                    loaded,
                    true,
                    self.cache.charge_gauge(),
                    self.cache.heap_promise_gauge(),
                    self.cache.node_env(),
                )?)
            } else {
                None
            };
            Ok((written, successors, new_root))
        }
        .await;
        let (written, successors, new_root) = match build_out {
            Ok(v) => v,
            Err(e) => {
                for ext in &claimed_extents {
                    ctx.alloc.release_unpublished(*ext);
                }
                return Err(e);
            }
        };
        if log::log_enabled!(log::Level::Debug) {
            // §4.7 heap admission's audit line: claims vs the promise this
            // node carried (an unpromised leaf claim is a draw on the
            // reserve the admission did not see).
            let promised = node.lock().read().await.promised();
            log::debug!(
                "SMO tree {} level {} node {:#x}: fold {total} B → {} part(s), claimed {} \
                 extent(s), {promised} promised",
                self.tree_id,
                node.level(),
                node.addr(),
                parts.len(),
                claimed_extents.len(),
            );
        }

        // ---- K6b production journaling (before any lock): make the
        // successor images durable, then admit the SMO's records from the
        // checkpoint-task reserve. Admission refusal aborts cleanly — the
        // built images were never published, the claims are released, and
        // the caller (the checkpoint task) drains and retries.
        let is_root_swap = self.is_root(node);
        let old_extent = self.cache.addr_extent(node.addr());
        let smo_prep = if let Some(j) = &ctx.journal {
            // Successors (and a new root, if any) durable BEFORE any
            // pointer record to them can exist in the ring: a replayed
            // pointer must never route to a torn image (§4.10).
            j.barrier().await?;
            // §4.7 at-cap admission headroom (design-smo-replay-currency
            // PR 4 clause a): this SMO will push exactly one pending free
            // at step 3, and the serialized SMO task is the FIFO's only
            // producer — headroom observed here still holds post-swap
            // (drains only vacate slots). Refusing NOW keeps
            // `PendingFreeFull` a clean pre-swap abort (claims released,
            // floor restored by the caller) the maintenance arms remedy
            // with a forced checkpoint cycle — never the post-swap
            // custody leak (the extent falling out of the live FIFO).
            // The flush pass is exempt (`forced_retirement` — its
            // retirement parks unconditionally at step 3): refusing THE
            // SMO that discharges a tail-pinning floor is the §4.7
            // closed dependency cycle (doc above).
            if !forced_retirement && !ctx.alloc.pending_has_room() {
                for e in &claimed_extents {
                    ctx.alloc.release_unpublished(*e);
                }
                return Err(KvError::PendingFreeFull {
                    pending: ctx.alloc.pending_count(),
                });
            }
            let mut recs: Vec<(u8, Record)> = Vec::new();
            if !is_root_swap {
                for s in &successors {
                    recs.push((
                        tag_for(self.tree_id, node.level() + 1),
                        Record::put(
                            s.max_key().to_vec(),
                            0, // stamped from the reservation inside the window
                            encode_interior_value(s.addr(), s.node_seq()),
                        ),
                    ));
                }
            }
            for e in &claimed_extents {
                recs.push(alloc_record(*e, 0));
            }
            let retire_tag = j.retire_seq.load(Ordering::Acquire);
            recs.push(free_record(old_extent, retire_tag, 0));
            let len = entry_len_for(&recs)?;
            match j
                .ring
                .try_admit(len, super::journal_core::AdmissionClass::Checkpoint)
            {
                Some(adm) => Some((adm, recs)),
                None => {
                    for e in &claimed_extents {
                        ctx.alloc.release_unpublished(*e);
                    }
                    return Err(KvError::JournalReserveExhausted { needed: len });
                }
            }
        } else {
            None
        };

        // ---- Step 2: parent-then-child locks; move the accumulated
        // delta; swap the mapping; assign pointer-record seqs INSIDE the
        // window; release. (§4.6 three-step replacement.)
        let parent = if is_root_swap {
            None
        } else {
            Some(self.resolve_parent(node).await?)
        };
        let smo_entry = {
            let mut parent_guard = match &parent {
                Some(p) => Some(p.lock().write().await),
                None => None,
            };
            let mut child_guard = node.lock().write().await;

            // §4.4 pt 2 / §4.6: the reservation happens INSIDE the lock
            // window — any commit that lands on a successor after the
            // swap reserves after this and carries higher seqs, so
            // replay applies the pointer record first.
            let smo_entry = smo_prep.map(|(adm, mut recs)| {
                let j = ctx.journal.as_ref().expect("prep implies hooks");
                let res = j.ring.reserve_registered(adm);
                for (i, (_tag, r)) in recs.iter_mut().enumerate() {
                    r.seq = res.start + i as u64;
                }
                (res, recs)
            });

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
            // Leftover floor (FIND-SMO-TAIL §1b, made EXACT by the
            // per-record `entry_floor` stamp — P2 2026-07-26 §9's second
            // cycle closer): each moved record carries its own entry-
            // start floor from its original apply, so every successor
            // gets exactly `min` over the records it actually receives.
            // The former predecessor-floor inheritance ("pred_floor
            // lower-bounds every one of them") was CORRECT but ruinously
            // pessimistic: under continuous same-leaf churn every
            // compaction has leftovers, so the predecessor's ancient
            // floor — whose own records are durable in the successor
            // IMAGES and coverage-clamped by the retire's dying-floor
            // fold for exactly one cycle — chained through every
            // successor generation and pinned the checkpoint tail
            // forever (the tail never passed the parked retirement
            // gates: the wedge's floor face). Exactness keeps §1b whole:
            // entry_floor IS the containing entry's start (never a raw
            // mid-entry seq), and the raw min-seq backstop only engages
            // for records that somehow never passed apply_locked (MAX
            // stamp) — never weaker than the pre-§1b behavior.
            for succ in &successors {
                let mut sg = succ.lock().write().await;
                let mine: Vec<OwnedRec> = leftovers
                    .iter()
                    .filter(|r| &r.key[..] >= succ.min_key() && &r.key[..] <= succ.max_key())
                    .cloned()
                    .collect();
                if !mine.is_empty() {
                    let floor = mine
                        .iter()
                        .map(|r| r.entry_floor.min(r.seq))
                        .min()
                        .unwrap_or(u64::MAX);
                    succ.apply_locked(&mut sg, mine, floor)?;
                    // §4.7 heap admission: the leftovers were admitted
                    // against the PREDECESSOR's promise (just released by
                    // `take_overlay`), but this SMO folded only the frozen
                    // delta — the leftovers still owe their flush. A
                    // successor whose inherited delta overflows its log
                    // carries the promise for the SMO it will need, so
                    // that claim is ledger budget and not a silent draw on
                    // the reserve (the storm shape: several commits land
                    // in the build window, the successor holds a two-node
                    // fold with nothing promised).
                    if sg.projected_log_end(layout, 0) > layout.node_size() {
                        let (fold, parts) = succ
                            .snapshot()
                            .fold_bytes_upper_with(&mut [], layout.split_part_capacity());
                        sg.promise(layout.smo_extents_for_parts(fold, parts), fold);
                    }
                }
            }

            // Swap the cache mapping: successors published FIRST, then
            // the route flip, and the old object retired LAST — all
            // inside the lock window, but the ORDER is what latch-free
            // readers observe. Retiring before the route flip opened a
            // reader-visible window in which the parent's current
            // snapshot still routed to an already-retired extent; with
            // the SMO task descheduled mid-window under storm load that
            // state persisted for milliseconds and readers burned the
            // whole (yielded) restart budget on one reason — the
            // intermittent 1M-storm budget exhaustion K7 caught,
            // `restarts = [0,0,0,256,0]` (all child-retired). With the
            // flip first, stragglers on the old route still resolve the
            // still-mapped pre-SMO object — readers get its intact
            // snapshot (§4.6: "snapshot left intact for in-flight
            // readers"), writers fail revalidation (superseded was set
            // above) and retry — and the retired state begins only once
            // the new route is reader-visible. Eviction cannot re-open
            // the window: `try_evict` is a clean-only CAS, so the
            // superseded-but-not-yet-retired object can never be dropped
            // from the map in between.
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
                    //
                    // FIND-VS-A: a root swap is the ONE routing change
                    // with no journaled pointer record and no parent
                    // floor — its durable form is exclusively the next
                    // ledger record's `tree_roots`. Clamp the next tail
                    // to this swap's journal position (dying-floor fold)
                    // so every commit applied under the new arrangement
                    // — and the SMO records that rebuild the routing —
                    // stays inside the replay window until a ledger
                    // record naming the new root covers them.
                    let swap_floor = smo_entry.as_ref().map(|(res, _)| res.start).unwrap_or(0);
                    self.cache.note_dying_floor(swap_floor);
                    let (addr, seq) = match &new_root {
                        Some(r) => (r.addr(), r.node_seq()),
                        None => written[0],
                    };
                    self.root.store(Arc::new(RootPtr { addr, seq }));
                }
                (Some(parent), Some(pg)) => {
                    // Interior pointer records — the §4.4 pt 2 discipline:
                    // seqs assigned inside the lock window (from the real
                    // journal reservation under the K6b hooks, from the
                    // K5 counter stand-in otherwise), bytes written after
                    // release (the journal entry below; the parent's node
                    // image catches up on its own later writeback).
                    let recs: Vec<OwnedRec> = successors
                        .iter()
                        .enumerate()
                        .map(|(i, s)| {
                            OwnedRec::new(
                                Bytes::copy_from_slice(s.max_key()),
                                match &smo_entry {
                                    // The first `successors.len()` journal
                                    // records ARE the pointer records, in
                                    // successor order — RAM apply and replay
                                    // must carry identical seqs.
                                    Some((_, recs)) => recs[i].1.seq,
                                    None => self.next_seq(),
                                },
                                RecordKind::Put,
                                Bytes::from(encode_interior_value(s.addr(), s.node_seq())),
                            )
                        })
                        .collect();
                    // The flips are the entry's records 0..n, so the
                    // first flip's seq IS the entry start (`res.start`
                    // under the K6b hooks) — SMO floors already pin at
                    // the §1b boundary.
                    let flip_floor = recs.first().map(|r| r.seq).unwrap_or(u64::MAX);
                    parent.apply_locked(pg, recs, flip_floor)?;
                }
                (Some(_), None) => unreachable!("parent guard taken with parent"),
            }
            // The old extent's retired state begins only now — after the
            // route flip (see the ordering comment above): a traversal
            // that already resolved the old address finds the still-
            // mapped pre-SMO object, never a load refusal reached
            // through a still-current route.
            self.cache.retire(node);
            drop(child_guard);
            // parent_guard drops here.
            smo_entry
        };
        node.state().end_freeze();

        // ---- Step 3: after release — the SMO's entry bytes (§4.6:
        // reserve-in-window / write-after-release), then the old extent
        // to pending-free gated on the FREE RECORD'S OWN SEQ (§4.7 +
        // design-smo-replay-currency §2-A: the free record is the entry's
        // highest seq — pushed last — so a durable tail past it proves
        // every flip of this entry is materialized; the historical
        // generation tag stays in the record VALUE, byte-for-byte), then
        // parent maintenance if its delta crossed the threshold.
        let free_gate_seq = match smo_entry {
            Some((res, recs)) => {
                let gate = recs
                    .last()
                    .map(|(_, r)| r.seq)
                    .expect("an SMO entry always carries its free record");
                let j = ctx.journal.as_ref().expect("entry implies hooks");
                if let Err(e) = j.ring.commit_entry(&res, &recs).await {
                    // The swap already happened and RAM is authoritative;
                    // an unwritten reserved range is exactly the §4.4
                    // pt 4 crash-equivalent hole — replay folds to the
                    // pre-SMO state plus the by-key window, which is
                    // consistent (§4.6). Loud in the log, never fatal.
                    log::warn!(
                        "SMO journal entry write failed on {:?} (crash-equivalent hole; \
                         state stays RAM-consistent): {e}",
                        j.path
                    );
                }
                gate
            }
            None => self.next_seq(),
        };
        if forced_retirement {
            // The flush-pass posture: park unconditionally — at cap the
            // retirement rides the allocator's unbounded overflow against
            // the coming cycle's tail (the §4.7 cycle-break; progress
            // theorem in the fn doc).
            ctx.alloc
                .free_pending_forced(self.cache.addr_extent(node.addr()), free_gate_seq);
        } else {
            // Defense-in-depth only: the admission headroom check before
            // `try_admit` (clause a above) makes this cap refusal
            // unreachable — the serialized SMO task is the FIFO's only
            // producer, so headroom at admission holds here. It stays as
            // a `?` because a FIFO refusal at this point would be a
            // protocol bug (a second producer), and the §4.4-pt-4-style
            // loud abort is the right failure mode for that.
            ctx.alloc
                .free_pending(self.cache.addr_extent(node.addr()), free_gate_seq)?;
        }
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

    /// Whether `addr` is this tree's current root (the heap admission's
    /// root-split projection: a root leaf's split also mints a new root).
    pub(crate) fn is_root_addr(&self, addr: u64) -> bool {
        self.root().addr == addr
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Current value of the shared per-volume node-seq mint counter —
    /// the checkpoint captures it as the ledger's `node_seq_watermark`
    /// so a mount can reseed at or above every seq ever stamped into a
    /// persisted frame (Finding A, 2026-07-13).
    pub(crate) fn node_seq_snapshot(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
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
        // Finding 41: a part boundary may only land on a KEY boundary —
        // same-key fold groups (the compact_fold lineage pair) must land
        // whole in ONE part (see split_node's cohesion rule; the byte
        // budget yields, the key law does not).
        if acc + len > budget && i > start && folded[i].key != folded[i - 1].key {
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
