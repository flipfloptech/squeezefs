//! PR M9 contract tests: **D7 record-fold slimming** — the fold-forward
//! overlay head (D7.a), the snapshot fold memo (D7.b), the fold-algebra
//! equivalence guard, and the §5.7 memory accounting
//! (`docs/design-metadata-throughput.md` §5.7; companion law
//! `docs/design-cow-kv-metadata.md` §4.2 — the fold FUNCTION is untouched;
//! D7 changes *when* it runs, never *what* it computes).
//!
//! Contracts pinned:
//! - **D7.a overlay head serves, zero decodes**: after an apply, a point
//!   lookup of the key is served from the materialized folded head riding
//!   the newest open-delta record — `meta_kv_fold_head_serves` moves,
//!   the `META_KV_FOLD_RECORD_DECODES` pin does **not** (the RED-first
//!   decode contract; the baseline measured this re-decode tax at ~16 % of
//!   daemon CPU).
//! - **D7.b memo serves**: the second fold of a bset-resident delta chain
//!   is a memo hit on the immutable snapshot — zero decodes, byte-equal.
//! - **Equivalence guard** (the theorem-preservation pin, risk R7): under
//!   randomized record histories — puts, deltas, tombstones, freezes,
//!   appends, and §4.4 pt 4-shaped mid-range rollback removals — the
//!   head/memo-served lookup byte-equals a from-scratch
//!   `fold_newest_first` over the surviving records, on the first read
//!   AND the repeat (memo) read. This must be able to catch any
//!   divergence (stale heads after mid-range removal are the designed
//!   hazard — a deterministic case pins that shape explicitly).
//! - **§5.7 memory accounting**: a node's budget charge grows past its
//!   extent size with overlay and memo bytes
//!   (`charged = extent + overlay + memo`); the
//!   `meta_kv_fold_memo_bytes` gauge tracks populate/drop exactly; and a
//!   tiny-budget churn keeps the gauge bounded through eviction (the M9
//!   tiny-budget storm gate in miniature).
//!
//! Counter assertions read the process-global `META_KV_*` statics as
//! before/after deltas — exact under the repo gate's `--test-threads=1`
//! (the `kv_tree_tests` precedent).

use bytes::Bytes;
use proptest::prelude::*;
use squeezefs::meta_backend::kv::node::{write_node, NodeLayout, NodeWriteParams};
use squeezefs::meta_backend::kv::node_cache::{
    CachedNode, LiveLookup, NodeCache, NodeCacheConfig, FOLD_MEMO_CAPACITY,
};
use squeezefs::meta_backend::kv::record::{
    fold_newest_first, inode_key, Folded, InodeDelta, InodeValue, Record, RecordKind, TREE_INODES,
};
use squeezefs::meta_backend::kv::tree::{ApplyOutcome, KvTree, SmoContext};
use squeezefs::meta_backend::kv::{
    alloc_ext::ExtentAllocator, META_KV_FOLD_HEAD_SERVES, META_KV_FOLD_MEMO_BYTES,
    META_KV_FOLD_MEMO_HITS, META_KV_FOLD_MEMO_MISSES, META_KV_FOLD_RECORD_DECODES,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Harness (the kv_tree_tests `Vol` shape: file-backed volume + cache +
// allocator + serialized SMO context).
// ---------------------------------------------------------------------------

const NODE_SIZE: usize = 64 * 1024;

struct Vol {
    _file: NamedTempFile,
    cache: Arc<NodeCache>,
    seq: Arc<AtomicU64>,
    ctx: SmoContext,
}

impl Vol {
    fn new(extents: u64, budget_nodes: u64) -> Self {
        let file = NamedTempFile::new().expect("temp volume");
        file.as_file()
            .set_len(extents * NODE_SIZE as u64)
            .expect("size volume");
        let layout = NodeLayout::new(NODE_SIZE).expect("layout");
        let cache = NodeCache::new(NodeCacheConfig {
            path: file.path().to_path_buf(),
            layout,
            heap_base: 0,
            budget_bytes: budget_nodes * NODE_SIZE as u64,
            // Effectively disable threshold writeback: these tests drive
            // freezes explicitly to control record placement.
            writeback_delta_bytes: usize::MAX,
        });
        let alloc = Arc::new(ExtentAllocator::format(extents, 0, 4096));
        let seq = Arc::new(AtomicU64::new(0));
        let ctx = SmoContext::new(alloc.clone());
        Self {
            _file: file,
            cache,
            seq,
            ctx,
        }
    }

    async fn tree(&mut self) -> KvTree {
        KvTree::create(
            self.cache.clone(),
            &mut self.ctx,
            TREE_INODES,
            self.seq.clone(),
        )
        .await
        .expect("create tree")
    }
}

fn iv(seed: u64) -> InodeValue {
    InodeValue {
        mode: 0o100644 ^ (seed as u32 & 0xFFF),
        uid: 1000 + seed as u32,
        gid: 2000,
        nlink: 1,
        flags: 0,
        flags2: 0,
        size: seed.wrapping_mul(4096),
        atime: seed.wrapping_add(1),
        mtime: seed.wrapping_add(2),
        ctime: seed.wrapping_add(3),
    }
}

/// Apply one record through the commit-path shape (resolve → lock →
/// revalidate → apply) and return the seq it was minted (the harness owns
/// the seq counter and ops are serial, so `seq.load()` after the apply IS
/// the record's seq).
async fn apply(
    tree: &KvTree,
    leaf: &Arc<CachedNode>,
    seq: &Arc<AtomicU64>,
    key: &[u8],
    kind: RecordKind,
    value: Bytes,
) -> u64 {
    let out = tree
        .apply_at(leaf, key, kind, value)
        .await
        .expect("apply_at");
    assert_eq!(out, ApplyOutcome::Applied, "single-leaf tree never stales");
    seq.load(Ordering::Acquire)
}

/// Freeze the open delta and append it, so its records become
/// **bset-resident** (the D7.b memo shape). No-op when the overlay is
/// empty.
async fn freeze_and_append(cache: &Arc<NodeCache>, node: &Arc<CachedNode>) {
    let frozen = {
        let mut guard = node.lock().write().await;
        node.freeze_locked(&mut guard, &cache.config().layout)
            .expect("freeze")
    };
    if frozen.is_some() {
        assert!(
            cache.append_frozen(node).await.expect("append"),
            "test bsets always fit a 64 KiB node"
        );
    }
}

/// The user-visible projection of a lookup (§4.2 digest-walk framing).
fn live_bytes(l: &LiveLookup) -> Option<Vec<u8>> {
    match l {
        LiveLookup::Live(b) => Some(b.to_vec()),
        LiveLookup::Tombstone | LiveLookup::Absent => None,
    }
}

/// From-scratch reference fold over an explicit record history (newest
/// first), projected user-visibly.
fn reference_fold(records_newest_first: &[Record]) -> Option<Vec<u8>> {
    let folded = fold_newest_first(records_newest_first.iter().map(|r| r.record_ref()))
        .expect("reference fold");
    folded.live_value().map(<[u8]>::to_vec)
}

fn decodes() -> u64 {
    META_KV_FOLD_RECORD_DECODES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// D7.a — the fold-forward overlay head.
// ---------------------------------------------------------------------------

/// The create-storm hot shape: a parent inode `Put` accumulating Δtime
/// records in the open delta. After each apply, the point lookup must be
/// served from the materialized folded head — `meta_kv_fold_head_serves`
/// moves, and the decode pin does NOT (zero `InodeValue`/`InodeDelta`
/// decodes on the read).
#[tokio::test]
async fn overlay_head_serves_hot_parent_probe_with_zero_decodes() {
    let mut vol = Vol::new(64, 64);
    let tree = vol.tree().await;
    let key = inode_key(42);
    let leaf = tree.resolve_leaf(&key).await.expect("resolve");

    let base = iv(7);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Put,
        Bytes::from(base.encode()),
    )
    .await;
    let mut expected = base;
    for t in 1..=5u64 {
        let d = InodeDelta::times(1000 + t, 2000 + t);
        d.apply(&mut expected);
        apply(
            &tree,
            &leaf,
            &vol.seq,
            &key,
            RecordKind::Delta,
            Bytes::from(d.encode()),
        )
        .await;
    }

    let snap = leaf.snapshot();
    let d0 = decodes();
    let h0 = META_KV_FOLD_HEAD_SERVES.load(Ordering::Relaxed);
    let got = snap.lookup(&key).expect("lookup");
    assert_eq!(
        live_bytes(&got),
        Some(expected.encode()),
        "the folded head must byte-equal the from-scratch fold"
    );
    assert_eq!(
        META_KV_FOLD_HEAD_SERVES.load(Ordering::Relaxed),
        h0 + 1,
        "an overlay-resident delta chain must be served from the fold-forward head (D7.a)"
    );
    assert_eq!(
        decodes(),
        d0,
        "a head serve must perform ZERO record decodes (the §5.7 re-decode tax pin)"
    );

    // The head follows every subsequent apply (updated at apply time under
    // the node write lock — one fold at write replaces N folds at N reads).
    let d = InodeDelta::ctime(9999);
    d.apply(&mut expected);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Delta,
        Bytes::from(d.encode()),
    )
    .await;
    let snap = leaf.snapshot();
    let d1 = decodes();
    let got = snap.lookup(&key).expect("lookup");
    assert_eq!(live_bytes(&got), Some(expected.encode()));
    assert_eq!(
        decodes(),
        d1,
        "head serve after a further apply: zero decodes"
    );
}

/// Tombstones and plain Puts are folded heads too: the head serve must
/// reproduce the algebra for every record kind, still decode-free.
#[tokio::test]
async fn overlay_head_serves_tombstone_and_plain_put() {
    let mut vol = Vol::new(64, 64);
    let tree = vol.tree().await;
    let leaf = tree.resolve_leaf(&inode_key(1)).await.expect("resolve");

    // Plain Put — served from the head, zero decodes.
    let k_put = inode_key(1);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &k_put,
        RecordKind::Put,
        Bytes::from(iv(3).encode()),
    )
    .await;
    let snap = leaf.snapshot();
    let d0 = decodes();
    assert_eq!(
        live_bytes(&snap.lookup(&k_put).expect("lookup")),
        Some(iv(3).encode())
    );
    assert_eq!(decodes(), d0, "plain-Put head serve: zero decodes");

    // Delete shadowing a Put, then a Δ onto the tombstone: stays dead
    // (the algebra's tombstone rule), still decode-free on the read.
    let k_del = inode_key(2);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &k_del,
        RecordKind::Put,
        Bytes::from(iv(4).encode()),
    )
    .await;
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &k_del,
        RecordKind::Delete,
        Bytes::new(),
    )
    .await;
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &k_del,
        RecordKind::Delta,
        Bytes::from(InodeDelta::times(9, 9).encode()),
    )
    .await;
    let snap = leaf.snapshot();
    let d1 = decodes();
    assert_eq!(
        snap.lookup(&k_del).expect("lookup"),
        LiveLookup::Tombstone,
        "Δ onto a tombstone stays dead (§4.2)"
    );
    assert_eq!(decodes(), d1, "tombstone head serve: zero decodes");

    // Orphan delta (no base Put anywhere): folds to Absent.
    let k_orphan = inode_key(3);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &k_orphan,
        RecordKind::Delta,
        Bytes::from(InodeDelta::times(1, 1).encode()),
    )
    .await;
    let snap = leaf.snapshot();
    assert_eq!(
        snap.lookup(&k_orphan).expect("lookup"),
        LiveLookup::Absent,
        "Δ-without-base folds to absent (§4.2)"
    );
}

// ---------------------------------------------------------------------------
// D7.b — the snapshot fold memo.
// ---------------------------------------------------------------------------

/// Once the delta chain is bset-resident (frozen + appended; the overlay
/// is empty), the FIRST fold pays the decode walk and populates the memo;
/// the SECOND is a memo hit — zero decodes, byte-equal.
#[tokio::test]
async fn memo_serves_second_fold_of_bset_resident_key() {
    let mut vol = Vol::new(64, 64);
    let tree = vol.tree().await;
    let key = inode_key(77);
    let leaf = tree.resolve_leaf(&key).await.expect("resolve");

    let base = iv(11);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Put,
        Bytes::from(base.encode()),
    )
    .await;
    let mut expected = base;
    for t in 1..=4u64 {
        let d = InodeDelta::times(t, 100 + t);
        d.apply(&mut expected);
        apply(
            &tree,
            &leaf,
            &vol.seq,
            &key,
            RecordKind::Delta,
            Bytes::from(d.encode()),
        )
        .await;
    }
    freeze_and_append(&vol.cache, &leaf).await;

    let snap = leaf.snapshot();
    assert_eq!(snap.overlay_len(), 0, "the chain must be bset-resident");

    let m0_hit = META_KV_FOLD_MEMO_HITS.load(Ordering::Relaxed);
    let m0_miss = META_KV_FOLD_MEMO_MISSES.load(Ordering::Relaxed);
    let first = snap.lookup(&key).expect("first fold");
    assert_eq!(live_bytes(&first), Some(expected.encode()));
    assert_eq!(
        META_KV_FOLD_MEMO_MISSES.load(Ordering::Relaxed),
        m0_miss + 1,
        "the first fold of a bset-resident key is a memo miss (and populates)"
    );

    let d0 = decodes();
    let second = snap.lookup(&key).expect("second fold");
    assert_eq!(
        live_bytes(&second),
        Some(expected.encode()),
        "memo hit must byte-equal the from-scratch fold"
    );
    assert_eq!(
        META_KV_FOLD_MEMO_HITS.load(Ordering::Relaxed),
        m0_hit + 1,
        "the second fold of a bset-resident key must be a memo hit (D7.b)"
    );
    assert_eq!(decodes(), d0, "a memo hit must perform ZERO record decodes");

    // The memo dies with the snapshot at the next swap (§5.7): an apply to
    // ANY key publishes a fresh snapshot whose memo starts empty.
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &inode_key(78),
        RecordKind::Put,
        Bytes::from(iv(1).encode()),
    )
    .await;
    let fresh = leaf.snapshot();
    let m1_miss = META_KV_FOLD_MEMO_MISSES.load(Ordering::Relaxed);
    let refold = fresh.lookup(&key).expect("re-fold on the fresh snapshot");
    assert_eq!(live_bytes(&refold), Some(expected.encode()));
    assert_eq!(
        META_KV_FOLD_MEMO_MISSES.load(Ordering::Relaxed),
        m1_miss + 1,
        "a fresh snapshot's memo starts empty (populate-once cells die at the swap)"
    );
}

/// Latch-free memo probes race-free across threads (immutability makes the
/// populate-once cells safe by construction): concurrent lookups of one
/// snapshot agree byte-for-byte and the memo gauge stays inside the fixed
/// per-node capacity bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn memo_probes_race_free_across_threads() {
    let mut vol = Vol::new(64, 64);
    let tree = vol.tree().await;
    let leaf = tree.resolve_leaf(&inode_key(0)).await.expect("resolve");

    // 16 bset-resident delta chains (more keys than memo cells: the
    // capacity bound is exercised, not just the happy path).
    let mut expected: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for ino in 0..16u64 {
        let key = inode_key(ino);
        let base = iv(ino + 1);
        apply(
            &tree,
            &leaf,
            &vol.seq,
            &key,
            RecordKind::Put,
            Bytes::from(base.encode()),
        )
        .await;
        let mut want = base;
        for t in 1..=3u64 {
            let d = InodeDelta::times(ino * 10 + t, ino * 20 + t);
            d.apply(&mut want);
            apply(
                &tree,
                &leaf,
                &vol.seq,
                &key,
                RecordKind::Delta,
                Bytes::from(d.encode()),
            )
            .await;
        }
        expected.push((key.to_vec(), want.encode()));
    }
    freeze_and_append(&vol.cache, &leaf).await;

    let snap = leaf.snapshot();
    let g0 = META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed);
    let h0 = META_KV_FOLD_MEMO_HITS.load(Ordering::Relaxed);
    let expected = Arc::new(expected);
    let mut tasks = tokio::task::JoinSet::new();
    for w in 0..8usize {
        let snap = snap.clone();
        let expected = expected.clone();
        tasks.spawn(async move {
            for i in 0..500usize {
                let (key, want) = &expected[(i + w) % expected.len()];
                let got = snap.lookup(key).expect("concurrent lookup");
                assert_eq!(
                    live_bytes(&got).as_deref(),
                    Some(want.as_slice()),
                    "every interleaving must serve the same fold"
                );
            }
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.expect("no lookup task may panic");
    }

    let populated = META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed) - g0;
    assert!(
        populated > 0,
        "concurrent folds must have populated the memo"
    );
    // Capacity bound: one snapshot's memo holds at most FOLD_MEMO_CAPACITY
    // cells; each cell here is a 8 B key + ≤ 64 B folded inode value +
    // fixed overhead — 512 B/cell is a generous ceiling.
    assert!(
        populated <= (FOLD_MEMO_CAPACITY * 512) as u64,
        "memo bytes must respect the fixed per-node capacity (got {populated})"
    );
    assert!(
        META_KV_FOLD_MEMO_HITS.load(Ordering::Relaxed) > h0,
        "repeat folds across threads must hit the memo"
    );
}

// ---------------------------------------------------------------------------
// The equivalence guard (risk R7: the theorem-preservation pin).
// ---------------------------------------------------------------------------

/// The §4.4 pt 4 hazard shape, deterministically: a mid-range rollback
/// removal (the failing tx's record UNDER a newer concurrent Δtime) must
/// leave lookups folding exactly the surviving records — a stale
/// materialized head that still carries the removed record's effect is
/// the bug this test exists to catch.
#[tokio::test]
async fn mid_range_rollback_removal_folds_exactly() {
    let mut vol = Vol::new(64, 64);
    let tree = vol.tree().await;
    let key = inode_key(5);
    let leaf = tree.resolve_leaf(&key).await.expect("resolve");

    let base = iv(1);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Put,
        Bytes::from(base.encode()),
    )
    .await;
    // The failing tx's record (will be rolled back)…
    let d_failing = InodeDelta::times(111, 222);
    let s_failing = apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Delta,
        Bytes::from(d_failing.encode()),
    )
    .await;
    // …and the concurrent shared-parent-lock Δtime that committed above it.
    let d_newer = InodeDelta::ctime(999);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Delta,
        Bytes::from(d_newer.encode()),
    )
    .await;

    // §4.4 pt 4 removal half: strike the failing tx's seq range only.
    {
        let mut guard = leaf.lock().write().await;
        let removed =
            leaf.remove_overlay_records_locked(&mut guard, &key, s_failing, s_failing + 1);
        assert_eq!(removed, 1, "exactly the failing record is removed");
    }

    // Expected: base + d_newer, WITHOUT d_failing's mtime/ctime.
    let mut expected = base;
    d_newer.apply(&mut expected);
    let snap = leaf.snapshot();
    let got = snap.lookup(&key).expect("post-rollback fold");
    assert_eq!(
        live_bytes(&got),
        Some(expected.encode()),
        "post-rollback fold must exclude the removed record (stale-head hazard)"
    );
    // And the repeat read (whatever fast path serves it) agrees.
    let again = snap.lookup(&key).expect("repeat fold");
    assert_eq!(live_bytes(&again), Some(expected.encode()));

    // A fresh apply after the rollback re-materializes cleanly.
    let d_after = InodeDelta::times(333, 444);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Delta,
        Bytes::from(d_after.encode()),
    )
    .await;
    d_after.apply(&mut expected);
    let snap = leaf.snapshot();
    assert_eq!(
        live_bytes(&snap.lookup(&key).expect("fold after re-apply")),
        Some(expected.encode())
    );
}

/// One mirrored operation on the reference history.
#[derive(Debug, Clone)]
enum Op {
    Put {
        key: u8,
        seed: u64,
    },
    DeltaTimes {
        key: u8,
        t: u64,
    },
    DeltaCtime {
        key: u8,
        t: u64,
    },
    Delete {
        key: u8,
    },
    /// Freeze the open delta and append it (records become bset-resident).
    Freeze,
    /// §4.4 pt 4-shaped removal: strike the `depth`-newest overlay record
    /// of `key` (single-seq range) — a no-op if it already froze.
    Remove {
        key: u8,
        depth: u8,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..4u8, any::<u64>()).prop_map(|(key, seed)| Op::Put { key, seed }),
        (0..4u8, any::<u64>()).prop_map(|(key, t)| Op::DeltaTimes { key, t }),
        (0..4u8, any::<u64>()).prop_map(|(key, t)| Op::DeltaCtime { key, t }),
        (0..4u8).prop_map(|key| Op::Delete { key }),
        Just(Op::Freeze),
        (0..4u8, 0..3u8).prop_map(|(key, depth)| Op::Remove { key, depth }),
    ]
}

/// A mirrored record: what the node should hold for the key.
#[derive(Debug, Clone)]
struct MirrorRec {
    rec: Record,
    frozen: bool,
    removed: bool,
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        // Real io_uring file I/O per case: keep the search bounded and
        // deterministic-fast; regressions shrink like any proptest.
        max_shrink_iters: 256,
        .. ProptestConfig::default()
    })]

    /// **The K1 fold-algebra equivalence guard, extended over D7** (§5.7):
    /// for randomized histories, every key's lookup — first read (head or
    /// cold fold) AND repeat read (memo) — byte-equals a from-scratch
    /// `fold_newest_first` over the records that survive in the node.
    #[test]
    fn fold_head_and_memo_equal_from_scratch_fold(ops in proptest::collection::vec(op_strategy(), 1..48)) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let mut vol = Vol::new(64, 64);
            let tree = vol.tree().await;
            let leaf = tree.resolve_leaf(&inode_key(0)).await.expect("resolve");
            let mut mirror: Vec<MirrorRec> = Vec::new();

            for op in &ops {
                match op {
                    Op::Put { key, seed } => {
                        let k = inode_key(u64::from(*key));
                        let rec_value = iv(*seed).encode();
                        let seq = apply(&tree, &leaf, &vol.seq, &k, RecordKind::Put,
                                        Bytes::from(rec_value.clone())).await;
                        mirror.push(MirrorRec {
                            rec: Record::put(k.to_vec(), seq, rec_value),
                            frozen: false,
                            removed: false,
                        });
                    }
                    Op::DeltaTimes { key, t } => {
                        let k = inode_key(u64::from(*key));
                        let d = InodeDelta::times(*t, t.wrapping_add(1));
                        let seq = apply(&tree, &leaf, &vol.seq, &k, RecordKind::Delta,
                                        Bytes::from(d.encode())).await;
                        mirror.push(MirrorRec {
                            rec: Record::delta(k.to_vec(), seq, &d),
                            frozen: false,
                            removed: false,
                        });
                    }
                    Op::DeltaCtime { key, t } => {
                        let k = inode_key(u64::from(*key));
                        let d = InodeDelta::ctime(*t);
                        let seq = apply(&tree, &leaf, &vol.seq, &k, RecordKind::Delta,
                                        Bytes::from(d.encode())).await;
                        mirror.push(MirrorRec {
                            rec: Record::delta(k.to_vec(), seq, &d),
                            frozen: false,
                            removed: false,
                        });
                    }
                    Op::Delete { key } => {
                        let k = inode_key(u64::from(*key));
                        let seq = apply(&tree, &leaf, &vol.seq, &k, RecordKind::Delete,
                                        Bytes::new()).await;
                        mirror.push(MirrorRec {
                            rec: Record::delete(k.to_vec(), seq),
                            frozen: false,
                            removed: false,
                        });
                    }
                    Op::Freeze => {
                        freeze_and_append(&vol.cache, &leaf).await;
                        for m in mirror.iter_mut() {
                            if !m.removed {
                                m.frozen = true;
                            }
                        }
                    }
                    Op::Remove { key, depth } => {
                        let k = inode_key(u64::from(*key));
                        // The depth-newest LIVE overlay record of this key.
                        let candidates: Vec<usize> = mirror
                            .iter()
                            .enumerate()
                            .filter(|(_, m)| {
                                !m.frozen && !m.removed && m.rec.key == k.to_vec()
                            })
                            .map(|(i, _)| i)
                            .collect();
                        if candidates.is_empty() {
                            continue;
                        }
                        let pick = candidates[candidates.len().saturating_sub(1)
                            .saturating_sub(usize::from(*depth) % candidates.len())];
                        let seq = mirror[pick].rec.seq;
                        let mut guard = leaf.lock().write().await;
                        let removed =
                            leaf.remove_overlay_records_locked(&mut guard, &k, seq, seq + 1);
                        drop(guard);
                        prop_assert_eq!(removed, 1, "the mirrored overlay record must exist");
                        mirror[pick].removed = true;
                    }
                }
            }

            // Verify every key: first read and repeat (memo) read equal the
            // from-scratch fold over the surviving records.
            let snap = leaf.snapshot();
            for key in 0..4u8 {
                let k = inode_key(u64::from(key));
                let mut survivors: Vec<Record> = mirror
                    .iter()
                    .filter(|m| !m.removed && m.rec.key == k.to_vec())
                    .map(|m| m.rec.clone())
                    .collect();
                survivors.sort_by_key(|r| std::cmp::Reverse(r.seq)); // newest first
                let want = reference_fold(&survivors);
                let got1 = live_bytes(&snap.lookup(&k).expect("first fold"));
                prop_assert_eq!(&got1, &want, "first read diverges for key {}", key);
                let got2 = live_bytes(&snap.lookup(&k).expect("repeat fold"));
                prop_assert_eq!(&got2, &want, "repeat (memo) read diverges for key {}", key);
            }
            Ok(())
        })?;
    }
}

// ---------------------------------------------------------------------------
// §5.7 memory accounting: charged = extent + overlay + memo; gauge bounded
// under eviction.
// ---------------------------------------------------------------------------

/// The node-cache budget charge grows past the extent size with overlay
/// records+heads and with memo cells, and falls back when the overlay
/// freezes (`a node's charged size = extent bytes + overlay bytes + memo
/// bytes` — §5.7 revision-1 memory accounting).
#[tokio::test]
async fn node_charge_includes_overlay_and_memo_bytes() {
    let mut vol = Vol::new(64, 64);
    let tree = vol.tree().await;
    let key = inode_key(9);
    let leaf = tree.resolve_leaf(&key).await.expect("resolve");

    let extent_only = vol.cache.cached_bytes();
    assert_eq!(
        extent_only, NODE_SIZE as u64,
        "a fresh single-leaf tree charges exactly one extent"
    );

    // Overlay charge: a Put plus a Δ chain must grow the charge.
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &key,
        RecordKind::Put,
        Bytes::from(iv(2).encode()),
    )
    .await;
    for t in 1..=8u64 {
        apply(
            &tree,
            &leaf,
            &vol.seq,
            &key,
            RecordKind::Delta,
            Bytes::from(InodeDelta::times(t, t).encode()),
        )
        .await;
    }
    let with_overlay = vol.cache.cached_bytes();
    assert!(
        with_overlay > extent_only,
        "overlay records + folded heads must charge the budget \
         (extent-only {extent_only}, with overlay {with_overlay})"
    );

    // Freeze + append: the open delta empties — the overlay charge drops.
    freeze_and_append(&vol.cache, &leaf).await;
    let after_freeze = vol.cache.cached_bytes();
    assert!(
        after_freeze < with_overlay,
        "freezing must release the overlay charge \
         (with overlay {with_overlay}, after freeze {after_freeze})"
    );

    // Memo charge: folding the bset-resident chain populates the memo and
    // the charge grows again — and the global gauge tracks the same bytes.
    let g0 = META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed);
    let snap = leaf.snapshot();
    let _ = snap.lookup(&key).expect("populate fold");
    let with_memo = vol.cache.cached_bytes();
    let gauge_delta = META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed) - g0;
    assert!(
        gauge_delta > 0,
        "populating the memo must move the meta_kv_fold_memo_bytes gauge"
    );
    assert!(
        with_memo >= after_freeze + gauge_delta,
        "memo bytes must ride the node-cache budget charge \
         (after freeze {after_freeze}, with memo {with_memo}, gauge {gauge_delta})"
    );

    // The memo dies with its snapshot: a fresh apply swaps a new snapshot;
    // when the old Arc drops, its memo bytes leave the gauge.
    drop(snap);
    apply(
        &tree,
        &leaf,
        &vol.seq,
        &inode_key(10),
        RecordKind::Put,
        Bytes::from(iv(1).encode()),
    )
    .await;
    let g_final = META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed);
    assert_eq!(
        g_final, g0,
        "the dead snapshot's memo bytes must leave the gauge exactly (Drop-owned)"
    );
}

/// The M9 tiny-budget gate in miniature: churning demand loads through a
/// deliberately tiny node-cache budget, with every load's key folded (memo
/// populated), must keep `meta_kv_fold_memo_bytes` bounded — eviction
/// drops snapshots and their memos with them; no OOM-class growth.
#[tokio::test]
async fn tiny_budget_eviction_bounds_memo_gauge() {
    const EXTENTS: u64 = 48;
    const BUDGET_NODES: u64 = 4;
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file()
        .set_len(EXTENTS * NODE_SIZE as u64)
        .expect("size volume");
    let layout = NodeLayout::new(NODE_SIZE).expect("layout");
    let cache = NodeCache::new(NodeCacheConfig {
        path: file.path().to_path_buf(),
        layout,
        heap_base: 0,
        budget_bytes: BUDGET_NODES * NODE_SIZE as u64,
        writeback_delta_bytes: usize::MAX,
    });

    // 48 single-leaf nodes, each holding a bset-resident delta chain (the
    // memo-eligible shape), written directly through the K2 node layer.
    for ext in 0..EXTENTS {
        let addr = ext * NODE_SIZE as u64;
        let ino = 1000 + ext;
        let mut records = vec![Record::put(
            inode_key(ino).to_vec(),
            ext * 10 + 1,
            iv(ino).encode(),
        )];
        for t in 1..=3u64 {
            records.push(Record::delta(
                inode_key(ino).to_vec(),
                ext * 10 + 1 + t,
                &InodeDelta::times(t, t),
            ));
        }
        write_node(
            file.path(),
            &layout,
            &NodeWriteParams {
                node_addr: addr,
                node_seq: ext + 1,
                tree_id: TREE_INODES,
                level: 0,
                min_key: &inode_key(ino),
                max_key: &inode_key(ino),
            },
            &records,
            ext * 10 + 4,
        )
        .await
        .expect("write node");
    }

    let g0 = META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed);
    let mut peak_gauge = 0u64;
    // Three churn laps: every lap demand-pages every node (evicting down
    // to budget as it goes) and folds its key twice (populate + hit).
    for _lap in 0..3 {
        for ext in 0..EXTENTS {
            let addr = ext * NODE_SIZE as u64;
            let node = cache
                .load(addr)
                .await
                .expect("load")
                .expect("extent never retired");
            let key = inode_key(1000 + ext);
            let snap = node.snapshot();
            let mut want = iv(1000 + ext);
            for t in 1..=3u64 {
                InodeDelta::times(t, t).apply(&mut want);
            }
            assert_eq!(
                live_bytes(&snap.lookup(&key).expect("fold")),
                Some(want.encode())
            );
            let _ = snap.lookup(&key).expect("repeat fold");
            drop(snap);
            peak_gauge = peak_gauge.max(META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed) - g0);
        }
    }

    assert!(peak_gauge > 0, "the churn must have populated memos");
    // Bounded: the gauge may cover the mapped set (≤ budget nodes + the
    // in-flight node) plus transiently-dying snapshots — but NEVER the
    // whole 48-node × 3-lap churn. 512 B/cell × capacity × (budget + 4)
    // nodes is a generous ceiling; unbounded growth would exceed it in
    // lap 1.
    let bound = (FOLD_MEMO_CAPACITY * 512) as u64 * (BUDGET_NODES + 4);
    assert!(
        peak_gauge <= bound,
        "eviction must bound the memo gauge (peak {peak_gauge} > bound {bound}) — \
         the §5.7 tiny-budget contract"
    );
    // And the budget itself holds (extent + overlay + memo accounting).
    assert!(
        cache.cached_bytes() <= (BUDGET_NODES + 2) * NODE_SIZE as u64,
        "cached bytes must converge to the tiny budget (got {})",
        cache.cached_bytes()
    );

    // Quiescence: dropping the cache releases every mapped snapshot — the
    // gauge returns to its baseline (Drop-owned, leak-free).
    drop(cache);
    assert_eq!(
        META_KV_FOLD_MEMO_BYTES.load(Ordering::Relaxed),
        g0,
        "every memo byte must leave the gauge when its snapshot dies"
    );
}

/// Tiny-budget liveness (the M9 acceptance storm's finding): evicting a
/// node someone still HOLDS frees no memory — the holder's `Arc` keeps
/// the node and its snapshot alive — but severs the mapping and forces
/// the holder into a resolve→evict→reload retry. Under a budget smaller
/// than the working set (where §5.7's overlay/memo charge keeps the
/// cache persistently one byte over), that turned the commit path's
/// bounded revalidation retry into a loud EINVAL at storm scale.
/// Eviction must skip externally-held nodes (physically honest: nothing
/// would be freed) and reclaim them once released.
#[tokio::test]
async fn eviction_skips_externally_held_nodes() {
    const BUDGET_NODES: u64 = 1;
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file()
        .set_len(8 * NODE_SIZE as u64)
        .expect("size volume");
    let layout = NodeLayout::new(NODE_SIZE).expect("layout");
    let cache = NodeCache::new(NodeCacheConfig {
        path: file.path().to_path_buf(),
        layout,
        heap_base: 0,
        budget_bytes: BUDGET_NODES * NODE_SIZE as u64,
        writeback_delta_bytes: usize::MAX,
    });
    for ext in 0..6u64 {
        let addr = ext * NODE_SIZE as u64;
        let ino = 100 + ext;
        write_node(
            file.path(),
            &layout,
            &NodeWriteParams {
                node_addr: addr,
                node_seq: ext + 1,
                tree_id: TREE_INODES,
                level: 0,
                min_key: &inode_key(ino),
                max_key: &inode_key(ino),
            },
            &[Record::put(inode_key(ino).to_vec(), 1, iv(ino).encode())],
            1,
        )
        .await
        .expect("write node");
    }

    // Hold node 0 (the commit path's resolve→lock shape), then churn the
    // 1-node budget with five more loads — every publish sweeps.
    let held = cache
        .load(0)
        .await
        .expect("load")
        .expect("extent never retired");
    for ext in 1..6u64 {
        let _ = cache
            .load(ext * NODE_SIZE as u64)
            .await
            .expect("load")
            .expect("extent never retired");
    }
    assert!(
        cache.contains(held.addr()),
        "an externally-held node must survive budget sweeps (evicting it frees nothing)"
    );
    assert!(
        !held.state().is_superseded(),
        "an externally-held node must not be severed by eviction"
    );
    // Its mapping still serves — no reload, no retry loop.
    assert_eq!(
        live_bytes(&held.snapshot().lookup(&inode_key(100)).expect("fold")),
        Some(iv(100).encode())
    );

    // Released, it becomes an ordinary victim: more churn reclaims it.
    drop(held);
    for lap in 0..4u64 {
        for ext in 1..6u64 {
            let _ = cache.load(ext * NODE_SIZE as u64).await.expect("load");
        }
        if !cache.contains(0) {
            break;
        }
        assert!(lap < 3, "a released clean node must eventually evict");
    }
    assert!(!cache.contains(0), "released node reclaimed by the sweep");
}

// ---------------------------------------------------------------------------
// Reference-fold sanity (the harness itself is under test here: the mirror
// must reproduce the §4.2 fold on a known history).
// ---------------------------------------------------------------------------

#[test]
fn reference_fold_matches_known_history() {
    let k = inode_key(1).to_vec();
    let base = iv(5);
    let d = InodeDelta::times(7, 8);
    let mut want = base;
    d.apply(&mut want);
    let recs = vec![
        Record::delta(k.clone(), 3, &d),
        Record::put(k.clone(), 2, base.encode()),
        Record::put(k, 1, iv(9).encode()),
    ];
    assert_eq!(reference_fold(&recs), Some(want.encode()));
    let folded = fold_newest_first(recs.iter().map(|r| r.record_ref())).expect("fold");
    assert!(matches!(folded, Folded::Put { .. }));
}
