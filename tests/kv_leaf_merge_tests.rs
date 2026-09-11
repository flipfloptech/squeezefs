//! Leaf merge — the §4.6a underfull-sibling SMO
//! (`docs/design-cow-kv-metadata.md` §4.6a; the motivating defect is the
//! 2026-09-11 metadata-full fix's stated limitation, `.benchmarks/
//! 2026-09-11-post-123-board-items-2-4.md` §1: the v1 tree never merged,
//! so deletes returned record space INSIDE leaves, never extents — a
//! filled volume resumed creates into the room its deletes freed and hit
//! `ENOSPC` again the moment a new leaf was needed, with most leaves
//! nearly empty).
//!
//! Contracts pinned here:
//!
//! 1. **the motivating one** — fill a small volume to `ENOSPC`, delete
//!    90 % of the files SPREAD across every leaf (no leaf empties on its
//!    own), run checkpoints; creates MUST resume for a population
//!    proportional to the deleted share, and `meta_kv_node_merges` moves.
//!    RED on `a5b2d5fc`: "after deleting 940 of 1045 files … creates
//!    resumed for only 4 files … free extents at full = 11, peak after
//!    the recovery cycles = 11".
//! 2. **fold equivalence** — after a merge storm (random inserts/deletes
//!    across a multi-level tree, sweeps between) every live key serves
//!    its LWW value and no dead key resurfaces, against a shadow map.
//! 3. **K2 gap-freeness + separator consistency** after merges and root
//!    collapses: every child abuts its neighbour, every separator names
//!    its child's `max_key`, the last child's `max_key` is the parent's.
//! 4. **replay-twice digest equality with merges in the window** — crash
//!    images taken between the successor write and the flip, between the
//!    flip and the checkpoint, and after a root collapse: replay
//!    converges, no acked key is lost, no deleted key resurfaces.
//! 5. **the SMO-vs-commit storm** — concurrent writers against a merging
//!    tree: every kept key survives, every deleted key stays gone; plus
//!    the deterministic revalidation contract (a writer holding a leaf a
//!    merge superseded gets `Stale`, `meta_kv_commit_smo_retries` moves,
//!    the retried insert lands).
//! 6. **root collapse** is reachable and correct: a height-3 tree deleted
//!    down collapses to a root leaf; readers mid-walk restart, never
//!    error.
//! 7. **the heap-space arithmetic** — a merge is admitted at the
//!    compaction floor (one extent above it), refused one below, and the
//!    volume's free extents grow by one per merge (plus one per collapse)
//!    once the covering advance runs.
//! 8. **the D4 face** — the census reports `mergeable_leaves`, the
//!    `measure_d4` report carries it, `defrag_merge_leaves` merges them
//!    (the in-process job drive is `tests/defrag_tests.rs`).
//! 9. **the derivation tie test** — the candidate bound is the split's ¾
//!    fill read backwards (drift is red).
//!
//! Two sandboxes: the `KvMetaBackend` shape of `tests/
//! meta_volume_full_tests.rs` (a small file-backed v3 volume with 64 KiB
//! nodes driven through the `Metadata` trait) and the K5 tree shape of
//! `tests/kv_tree_tests.rs` (a `KvTree` over a `NodeCache` + allocator,
//! SMOs driven directly through `&mut SmoContext`).

use bytes::Bytes;
use squeezefs::error::SqueezefsError;
use squeezefs::meta_backend::kv::alloc_ext::{
    compaction_floor_extents, compaction_reserve_extents, ExtentAllocator,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{digest_backend, BuilderConfig, ImageBuilder, ROOT_INO};
use squeezefs::meta_backend::kv::node::{key_successor, NodeLayout, DEFAULT_NODE_SIZE};
use squeezefs::meta_backend::kv::node_cache::{
    CachedNode, NodeCache, NodeCacheConfig, DEFAULT_WRITEBACK_DELTA_BYTES,
};
use squeezefs::meta_backend::kv::record::{RecordKind, TREE_DENTRIES, TREE_INODES};
use squeezefs::meta_backend::kv::tree::{
    decode_interior_value, test_smo_build_pause_release, ApplyOutcome, KvTree, MaintenanceOutcome,
    SmoContext, KEY_SPACE_MAX, TEST_SMO_BUILD_PAUSED, TEST_SMO_BUILD_PAUSE_TREE,
};
use squeezefs::meta_backend::kv::{
    KvError, META_KV_COMMIT_SMO_RETRIES, META_KV_NODE_MERGES, META_KV_ROOT_COLLAPSES,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Backend sandbox (the meta_volume_full_tests shape).
// ---------------------------------------------------------------------------

/// A 24 MiB heap of 64 KiB extents is ≈ 360 leaves; the ring is small so
/// the fill's checkpoints are frequent.
const VOL_LEN: u64 = 24 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
/// Under the 64 KiB node's `node_size/4` value cap (16 KiB): ~4 per leaf,
/// so the xattr tree is the extent-dominant one and every few files split
/// its rightmost leaf.
const PAYLOAD_LEN: usize = 12 * 1024;
const TEST_SEED: u64 = 0x4C4E_4D52_4745_0001;
const TEST_UUID: [u8; 16] = *b"kv-leaf-merge-01";
/// Wall bound on a fill (a wedged-not-failed volume parks its committers).
const FILL_BOUND: Duration = Duration::from_secs(240);

async fn fresh_volume() -> (Arc<KvMetaBackend>, NamedTempFile) {
    let _ = env_logger::builder().is_test(true).try_init();
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    let cfg = BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    };
    ImageBuilder::new(cfg)
        .unwrap()
        .build(file.path(), VOL_LEN)
        .await
        .expect("build empty v3 image");
    let be = KvMetaBackend::open(file.path()).await.expect("mount v3");
    (be, file)
}

fn name(i: u32) -> String {
    format!("f{i:06}")
}

/// One "inline file": a create plus its payload xattr — two commits.
async fn put_file(be: &KvMetaBackend, i: u32) -> Result<(), SqueezefsError> {
    let f = be
        .create(ROOT_INO, &name(i), libc::S_IFREG | 0o644, 0, 0)
        .await?;
    be.setxattr(f.ino, "user.payload", &vec![(i & 0xFF) as u8; PAYLOAD_LEN])
        .await
}

/// Fill until the first refusal; returns `(files landed, the refusal)`.
async fn fill_until_refused(be: &KvMetaBackend, from: u32) -> (u32, SqueezefsError) {
    let mut i = from;
    loop {
        match put_file(be, i).await {
            Ok(()) => i += 1,
            Err(e) => return (i - from, e),
        }
        assert!(
            i - from < 100_000,
            "a 24 MiB metadata volume absorbed 100k × 12 KiB payloads — harness bug"
        );
        assert!(
            !be.is_failed(),
            "the volume fail-stopped mid-fill (a full heap must never mark the volume \
             FAILED — §4.7)"
        );
    }
}

/// Unlink + destroy one file on a full volume. A delete whose leaf needs a
/// compaction is admitted down to the compaction floor (§4.7 amended); a
/// burst of them can exhaust that half of the reserve, in which case the
/// refusal is `ENOSPC` and one checkpoint cycle returns the budget —
/// bounded retries, exactly the operator's shape.
async fn delete_file(be: &KvMetaBackend, i: u32) {
    let mut unlinked: Option<u64> = None;
    for _ in 0..32 {
        if unlinked.is_none() {
            match be.unlink(ROOT_INO, &name(i)).await {
                Ok(ino) => unlinked = Some(ino),
                Err(e) if e.to_errno() == libc::ENOSPC => {
                    be.checkpoint_now()
                        .await
                        .expect("cycle behind a refused unlink");
                    continue;
                }
                Err(e) => panic!("unlink {} failed: {e:?}", name(i)),
            }
        }
        match be.destroy_inode(unlinked.expect("unlinked")).await {
            Ok(()) => return,
            Err(e) if e.to_errno() == libc::ENOSPC => {
                be.checkpoint_now()
                    .await
                    .expect("cycle behind a refused destroy");
            }
            Err(e) => panic!("destroy {} failed: {e:?}", name(i)),
        }
    }
    panic!("delete {} refused ENOSPC past the retry bound", name(i));
}

/// Run checkpoint cycles until the claimable extent count stops growing
/// for `quiet` consecutive cycles (the merge waves return extents one to
/// two barriered cycles after each sweep — §4.6a (d)). Returns the peak
/// free-extent count observed.
async fn cycle_until_quiescent(be: &KvMetaBackend, quiet: u32, max_cycles: u32) -> u64 {
    let mut peak = be.free_extents();
    let mut flat = 0u32;
    for _ in 0..max_cycles {
        be.checkpoint_now().await.expect("checkpoint cycle");
        assert!(
            !be.is_failed(),
            "a recovery cycle must never fail-stop the volume"
        );
        let free = be.free_extents();
        if free > peak {
            peak = free;
            flat = 0;
        } else {
            flat += 1;
            if flat >= quiet {
                break;
            }
        }
    }
    peak
}

// ---------------------------------------------------------------------------
// K5 tree sandbox (the kv_tree_tests shape).
// ---------------------------------------------------------------------------

/// A file-backed volume + cache + allocator + serialized SMO context.
struct Vol {
    _file: NamedTempFile,
    cache: Arc<NodeCache>,
    alloc: Arc<ExtentAllocator>,
    seq: Arc<AtomicU64>,
    ctx: SmoContext,
}

impl Vol {
    fn new(node_size: usize, extents: u64, reserve: u64) -> Self {
        let file = NamedTempFile::new().expect("temp volume");
        file.as_file()
            .set_len(extents * node_size as u64)
            .expect("size volume");
        let layout = NodeLayout::new(node_size).expect("layout");
        let cache = NodeCache::new(NodeCacheConfig {
            path: file.path().to_path_buf(),
            layout,
            heap_base: 0,
            budget_bytes: extents * node_size as u64,
            writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
        });
        let alloc = Arc::new(ExtentAllocator::format(extents, reserve, 4096));
        let seq = Arc::new(AtomicU64::new(0));
        let ctx = SmoContext::new(alloc.clone());
        Self {
            _file: file,
            cache,
            alloc,
            seq,
            ctx,
        }
    }

    async fn tree(&mut self, tree_id: u8) -> KvTree {
        KvTree::create(self.cache.clone(), &mut self.ctx, tree_id, self.seq.clone())
            .await
            .expect("create tree")
    }

    /// The K5 stand-in for a covering checkpoint: every seq minted so far
    /// counts as durable, so tombstones below it elide at the next fold
    /// (§4.2) and parked retirements release.
    fn cover_everything(&self) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.cache.set_durable_tail(seq);
        self.alloc.advance_durable(seq);
    }
}

fn ikey(i: u64) -> Vec<u8> {
    squeezefs::meta_backend::kv::record::inode_key(i).to_vec()
}

fn val(i: u64, len: usize) -> Vec<u8> {
    let mut v = vec![(i % 251) as u8; len];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

/// Drive threshold maintenance until quiescent.
async fn drain(tree: &KvTree, ctx: &mut SmoContext) {
    while tree.maintenance_pending() {
        tree.run_maintenance(ctx).await.expect("maintenance");
    }
}

/// Flush + sweep to a fixpoint: no merge and no collapse left to run.
async fn merge_to_fixpoint(tree: &KvTree, vol: &mut Vol) -> MaintenanceOutcome {
    let mut total = MaintenanceOutcome::default();
    for _ in 0..64 {
        tree.flush_dirty(&mut vol.ctx).await.expect("flush");
        vol.cover_everything();
        let sweep = tree
            .merge_underfull(&mut vol.ctx, false)
            .await
            .expect("merge sweep");
        total.merges += sweep.outcome.merges;
        total.root_collapses += sweep.outcome.root_collapses;
        assert!(
            !sweep.space_refused,
            "a roomy heap never refuses a merge for space"
        );
        if sweep.outcome.merges + sweep.outcome.root_collapses == 0 {
            return total;
        }
    }
    panic!("the merge sweep never reached a fixpoint");
}

/// A deterministic pseudo-random permutation of `0..n` (LCG-driven
/// Fisher–Yates) — spreads inserts/deletes across every leaf.
fn permutation(n: u64, seed: u64) -> Vec<u64> {
    let mut p: Vec<u64> = (0..n).collect();
    let mut s = seed | 1;
    for i in (1..p.len()).rev() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((s >> 33) as usize) % (i + 1);
        p.swap(i, j);
    }
    p
}

/// The K2 structural walk (§4.6a (g)): every interior node's live
/// separators, in order, name children that partition the parent's range
/// gap-free — `child.min = parent.min` for the first, `= successor(prev
/// child.max)` after, `child.max = separator`, the last child's `max =
/// parent.max` — and every child's `node_seq` matches the pointer.
/// Returns `(leaves, height)`.
async fn k2_walk(tree: &KvTree, cache: &Arc<NodeCache>) -> (u64, u8) {
    async fn walk(cache: &Arc<NodeCache>, node: Arc<CachedNode>, leaves: &mut u64) {
        if node.level() == 0 {
            *leaves += 1;
            return;
        }
        let snap = node.snapshot();
        let mut expected_min: Vec<u8> = node.min_key().to_vec();
        let mut cursor: Vec<u8> = node.min_key().to_vec();
        let mut last_max: Option<Vec<u8>> = None;
        let mut children = 0u32;
        loop {
            let Some((sep, ptr)) = snap.next_live(&cursor, None).expect("next_live") else {
                break;
            };
            let (addr, seq) = decode_interior_value(&ptr).expect("interior value");
            let child = cache.get(addr).await.unwrap_or_else(|e| {
                panic!("child {addr:#x} of interior {:#x}: {e:?}", node.addr())
            });
            assert_eq!(
                child.node_seq(),
                seq,
                "separator {:x?} in interior {:#x} names child {addr:#x} at seq {seq}, the \
                 child holds {}",
                &sep[..],
                node.addr(),
                child.node_seq()
            );
            assert_eq!(
                child.level() + 1,
                node.level(),
                "child level must be exactly one below its parent"
            );
            assert_eq!(
                child.min_key(),
                &expected_min[..],
                "K2 gap: child {addr:#x} min {:x?} ≠ expected {:x?} (parent {:#x})",
                child.min_key(),
                expected_min,
                node.addr()
            );
            assert_eq!(
                child.max_key(),
                &sep[..],
                "separator {:x?} must equal its child's max_key {:x?}",
                &sep[..],
                child.max_key()
            );
            expected_min = key_successor(child.max_key());
            cursor = expected_min.clone();
            last_max = Some(child.max_key().to_vec());
            children += 1;
            Box::pin(walk(cache, child, leaves)).await;
        }
        assert!(
            children >= 1,
            "an interior node routes to at least one child"
        );
        assert_eq!(
            last_max.as_deref(),
            Some(node.max_key()),
            "the last child's max_key must be the parent's (interior {:#x})",
            node.addr()
        );
    }
    let root = cache.get(tree.root().addr).await.expect("root");
    assert_eq!(root.node_seq(), tree.root().seq);
    assert!(
        root.min_key().is_empty(),
        "the root spans the key space from ''"
    );
    assert_eq!(root.max_key(), &KEY_SPACE_MAX[..]);
    let height = root.level();
    let mut leaves = 0u64;
    walk(cache, root, &mut leaves).await;
    (leaves, height)
}

// ---------------------------------------------------------------------------
// (1) The motivating contract.
// ---------------------------------------------------------------------------

/// Fill → `ENOSPC` → delete 90 % spread across every leaf → checkpoints →
/// creates resume for a population proportional to the deleted share.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mostly_deleted_full_volume_keeps_creating() {
    let (be, _file) = fresh_volume().await;
    let merges0 = META_KV_NODE_MERGES.load(Ordering::Relaxed);

    let (landed, refusal) = tokio::time::timeout(FILL_BOUND, fill_until_refused(&be, 0))
        .await
        .expect("the fill must reach a refusal within the wall bound");
    assert_eq!(refusal.to_errno(), libc::ENOSPC, "got {refusal:?}");
    assert!(
        landed > 100,
        "a 24 MiB heap must absorb more than {landed} files"
    );
    let free_at_full = be.free_extents();

    // Delete 90 %, SPREAD: every file whose index is not a multiple of 10.
    // Keys are creation-ordered (monotonic inos), so this empties 90 % of
    // EVERY leaf and leaves no leaf empty on its own.
    let mut deleted = 0u32;
    for i in 0..landed {
        if i % 10 == 0 {
            continue;
        }
        delete_file(&be, i).await;
        deleted += 1;
    }
    assert!(!be.is_failed());

    // The recovery: checkpoint cycles run the flush pass, the heap-full
    // merge sweep, and release the merged extents as their entries are
    // covered (§4.6a (d) — waves).
    let peak_free = cycle_until_quiescent(&be, 6, 400).await;

    // Creates resume for a population proportional to the deleted share:
    // the deleted files' leaves were 90 % empty — merging them 3–4:1 into
    // ¾-fill successors returns most of their extents, so at least half
    // the deleted population lands again (a 5 % rightmost-leaf residue is
    // the pre-merge shape).
    let (relanded, second_refusal) =
        tokio::time::timeout(FILL_BOUND, fill_until_refused(&be, 500_000))
            .await
            .expect("the refill must reach a refusal within the wall bound");
    assert_eq!(second_refusal.to_errno(), libc::ENOSPC);
    assert!(!be.is_failed());
    assert!(
        relanded >= deleted / 2,
        "after deleting {deleted} of {landed} files (spread across every leaf) and \
         checkpointing, creates resumed for only {relanded} files — the deletes' \
         extents were not returned (the v1 'never merges' shape: the refill fits the \
         rightmost leaf's room, then ENOSPC at the first new leaf; free extents at \
         full = {free_at_full}, peak after the recovery cycles = {peak_free})"
    );
    let merges = META_KV_NODE_MERGES.load(Ordering::Relaxed) - merges0;
    assert!(
        merges > 0,
        "deleting 90 % of a full volume's files must merge underfull leaves \
         (meta_kv_node_merges is the engagement gauge)"
    );
    assert!(
        peak_free > free_at_full + 8,
        "merges must RETURN extents: free at full {free_at_full}, peak after the \
         recovery cycles {peak_free}"
    );
    assert!(
        be.merge_sweeps() >= 1,
        "the heap-full posture runs the merge sweep inside the checkpoint cycle"
    );

    // The survivors and the refilled files read back.
    for i in (0..landed).step_by(10).take(20) {
        let f = be
            .lookup(ROOT_INO, &name(i))
            .await
            .expect("survivor resolves");
        let v = be
            .getxattr(f.ino, "user.payload")
            .await
            .expect("getxattr")
            .expect("payload present");
        assert_eq!(v.len(), PAYLOAD_LEN);
        assert!(v.iter().all(|b| *b == (i & 0xFF) as u8));
    }
    for i in [500_000u32, 500_000 + relanded / 2, 500_000 + relanded - 1] {
        let f = be
            .lookup(ROOT_INO, &name(i))
            .await
            .expect("refilled file resolves");
        assert_eq!(
            be.getxattr(f.ino, "user.payload")
                .await
                .unwrap()
                .map(|v| v.len()),
            Some(PAYLOAD_LEN)
        );
    }
    // Every deleted name is gone.
    for i in (1..landed).step_by(97) {
        if i % 10 != 0 {
            assert!(
                be.lookup(ROOT_INO, &name(i)).await.is_err(),
                "deleted {} must not resurface across the merges",
                name(i)
            );
        }
    }
    be.shutdown().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------
// (2) Fold equivalence across a merge storm.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fold_equivalence_holds_across_a_merge_storm() {
    const KEYS: u64 = 12_000;
    let mut vol = Vol::new(64 * 1024, 4096, 0);
    let tree = vol.tree(TREE_INODES).await;
    let mut shadow: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    // Random-order inserts across the whole key range: ~30 leaves.
    for (n, &i) in permutation(KEYS, 7).iter().enumerate() {
        let v = val(i, 80);
        tree.insert(&ikey(i), v.clone()).await.expect("insert");
        shadow.insert(ikey(i), v);
        if n % 256 == 255 {
            drain(&tree, &mut vol.ctx).await;
        }
    }
    tree.flush_dirty(&mut vol.ctx).await.expect("flush");
    assert!(
        tree.root_level().await.expect("level") >= 1,
        "the fixture must be multi-leaf"
    );
    let (leaves0, _) = k2_walk(&tree, &vol.cache).await;

    let mut total = MaintenanceOutcome::default();
    for round in 0..3u64 {
        // Delete 90 % of the live keys (random order), reinsert a third of
        // the earlier deletes with NEW values (an old value resurfacing is
        // detectable), sweep.
        let live: Vec<Vec<u8>> = shadow.keys().cloned().collect();
        let order = permutation(live.len() as u64, 11 + round);
        for (n, &idx) in order.iter().enumerate() {
            if n % 10 == 0 {
                continue;
            }
            let k = &live[idx as usize];
            tree.delete(k).await.expect("delete");
            shadow.remove(k);
        }
        for i in (0..KEYS).filter(|i| i % 3 == round) {
            let k = ikey(i);
            if !shadow.contains_key(&k) && i % 7 == 0 {
                let v = val(i * 1_000_003 + round, 80);
                tree.insert(&k, v.clone()).await.expect("reinsert");
                shadow.insert(k, v);
            }
        }
        let out = merge_to_fixpoint(&tree, &mut vol).await;
        total.merges += out.merges;
        total.root_collapses += out.root_collapses;

        // Every key: live ⇒ its LWW value, dead ⇒ absent.
        for i in 0..KEYS {
            let k = ikey(i);
            let got = tree.lookup(&k).await.expect("lookup");
            assert_eq!(
                got.as_deref(),
                shadow.get(&k).map(|v| &v[..]),
                "round {round}: key {i} diverged from the shadow after the merges"
            );
        }
        // The full range walk equals the shadow exactly (no duplicates,
        // no resurfaced tombstoned key, key order intact).
        let all = tree
            .range(&ikey(0), &ikey(KEYS), usize::MAX)
            .await
            .expect("range");
        let want: Vec<(&Vec<u8>, &Vec<u8>)> = shadow.iter().collect();
        assert_eq!(all.len(), want.len(), "round {round}: live population");
        for ((k, v), (wk, wv)) in all.iter().zip(want) {
            assert_eq!(&k[..], &wk[..]);
            assert_eq!(&v[..], &wv[..]);
        }
        k2_walk(&tree, &vol.cache).await;
    }
    assert!(
        total.merges > 0,
        "a 90 % delete storm across a multi-leaf tree must merge leaves"
    );
    let (leaves1, _) = k2_walk(&tree, &vol.cache).await;
    assert!(
        leaves1 < leaves0,
        "merges shrink the leaf population ({leaves0} → {leaves1})"
    );
}

// ---------------------------------------------------------------------------
// (3) + (6) K2 gap-freeness, interior recursion, root collapse, readers.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn height_three_tree_collapses_to_a_root_leaf_gap_free_with_readers_mid_walk() {
    // 64 KiB nodes + 4 KiB values ⇒ ~12 records/leaf ⇒ the level-1 root
    // itself splits after ~1,100 leaves (the kv_tree_tests depth-3 shape).
    let mut vol = Vol::new(64 * 1024, 4096, 0);
    let tree = Arc::new(vol.tree(TREE_INODES).await);
    let collapses0 = META_KV_ROOT_COLLAPSES.load(Ordering::Relaxed);

    let mut n = 0u64;
    while tree.root_level().await.expect("level") < 2 {
        tree.insert(&ikey(n), val(n, 4096)).await.expect("insert");
        n += 1;
        if n.is_multiple_of(8) {
            drain(&tree, &mut vol.ctx).await;
        }
        assert!(n < 60_000, "depth-3 never reached");
    }
    tree.flush_dirty(&mut vol.ctx).await.expect("flush");
    let (leaves0, height0) = k2_walk(&tree, &vol.cache).await;
    assert_eq!(height0, 2);

    // Concurrent latch-free readers over the survivors for the whole
    // collapse: they may restart (a root swap lowers the height mid-walk)
    // but never error and never see a wrong value.
    let survivors: Vec<u64> = (0..n).filter(|i| i % 50 == 0).collect();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut readers = tokio::task::JoinSet::new();
    for r in 0..4u64 {
        let tree = tree.clone();
        let survivors = survivors.clone();
        let mut stop_rx = stop_rx.clone();
        readers.spawn(async move {
            let mut served = 0u64;
            let mut i = r as usize;
            while !*stop_rx.borrow_and_update() {
                let k = survivors[i % survivors.len()];
                let got = tree
                    .lookup(&ikey(k))
                    .await
                    .unwrap_or_else(|e| panic!("reader errored mid-collapse: {e:?}"));
                assert_eq!(
                    got.as_deref(),
                    Some(&val(k, 4096)[..]),
                    "reader saw a wrong value for survivor {k}"
                );
                served += 1;
                i += 3;
                tokio::task::yield_now().await;
            }
            served
        });
    }

    // Phase A: delete 98 % (keep every 50th) — leaves merge many-to-one,
    // the level-1 interiors shrink and merge, the level-2 root is left
    // with one child and collapses to height 2 (level 1).
    for i in 0..n {
        if i % 50 != 0 {
            tree.delete(&ikey(i)).await.expect("delete");
        }
    }
    let out_a = merge_to_fixpoint(&tree, &mut vol).await;
    assert!(out_a.merges > 0, "phase A must merge leaves");
    let (leaves_a, height_a) = k2_walk(&tree, &vol.cache).await;
    assert!(
        leaves_a < leaves0 / 4,
        "a 98 % delete must shrink the leaf population by far more than 4× \
         ({leaves0} → {leaves_a})"
    );
    assert!(
        height_a < height0,
        "the root must collapse at least once ({height0} → {height_a})"
    );
    for &k in &survivors {
        assert_eq!(
            tree.lookup(&ikey(k)).await.expect("lookup").as_deref(),
            Some(&val(k, 4096)[..]),
            "survivor {k} after phase A"
        );
    }

    // Phase B: delete down to 5 survivors — the tree collapses to a root
    // leaf (height 0).
    let keep: Vec<u64> = survivors.iter().copied().take(5).collect();
    for &k in &survivors {
        if !keep.contains(&k) {
            tree.delete(&ikey(k)).await.expect("delete");
        }
    }
    stop_tx.send(true).expect("stop readers");
    let mut served = 0u64;
    while let Some(r) = readers.join_next().await {
        served += r.expect("reader task");
    }
    assert!(
        served > 0,
        "the readers must have served during the collapse"
    );
    let out_b = merge_to_fixpoint(&tree, &mut vol).await;
    let (leaves_b, height_b) = k2_walk(&tree, &vol.cache).await;
    assert_eq!(
        (leaves_b, height_b),
        (1, 0),
        "five records must collapse to a single root leaf"
    );
    assert!(
        META_KV_ROOT_COLLAPSES.load(Ordering::Relaxed) - collapses0 >= 2,
        "height 2 → 0 is at least two root collapses ({} phase A + {} phase B)",
        out_a.root_collapses,
        out_b.root_collapses
    );
    for &k in &keep {
        assert_eq!(
            tree.lookup(&ikey(k)).await.expect("lookup").as_deref(),
            Some(&val(k, 4096)[..])
        );
    }
    // The tree is still a tree: growth after the collapse splits again.
    for i in 100_000..100_200u64 {
        tree.insert(&ikey(i), val(i, 4096)).await.expect("regrow");
        if i.is_multiple_of(8) {
            drain(&tree, &mut vol.ctx).await;
        }
    }
    tree.flush_dirty(&mut vol.ctx).await.expect("flush");
    let (leaves_c, height_c) = k2_walk(&tree, &vol.cache).await;
    assert!(leaves_c > 1 && height_c >= 1, "regrowth splits again");
    for i in (100_000..100_200u64).step_by(37) {
        assert_eq!(
            tree.lookup(&ikey(i)).await.expect("lookup").as_deref(),
            Some(&val(i, 4096)[..])
        );
    }
}

// ---------------------------------------------------------------------------
// (5) SMO-vs-commit: the deterministic revalidation contract + the storm.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_writer_revalidates_and_retries_after_a_merge() {
    let mut vol = Vol::new(64 * 1024, 512, 0);
    let tree = vol.tree(TREE_INODES).await;
    // Grow to two leaves under a level-1 root.
    let mut n = 0u64;
    while tree.root_level().await.expect("level") < 1 {
        tree.insert(&ikey(n), val(n, 64)).await.expect("seed");
        n += 1;
        if n.is_multiple_of(64) {
            drain(&tree, &mut vol.ctx).await;
        }
        assert!(n < 100_000, "split never triggered");
    }
    tree.flush_dirty(&mut vol.ctx).await.expect("flush");

    // The unrolled commit protocol: resolve FIRST (latch-free)…
    let stale_leaf = tree.resolve_leaf(&ikey(5)).await.expect("resolve");
    assert_eq!(stale_leaf.level(), 0);

    // …then a MERGE replaces the leaf before the writer locks: delete 95 %
    // so both leaves are underfull, sweep (deterministic — the SMO runs on
    // this task; &mut SmoContext is the serialization proof).
    for i in 0..n {
        if i % 20 != 0 {
            tree.delete(&ikey(i)).await.expect("delete");
        }
    }
    let out = merge_to_fixpoint(&tree, &mut vol).await;
    assert!(out.merges >= 1, "two underfull leaves must merge");
    assert!(
        stale_leaf.state().is_superseded(),
        "the resolved leaf was replaced by the merge"
    );

    // Lock-then-revalidate on the stale object: Stale + counted (§4.6).
    let retries0 = META_KV_COMMIT_SMO_RETRIES.load(Ordering::Relaxed);
    let outcome = tree
        .apply_at(
            &stale_leaf,
            &ikey(5),
            RecordKind::Put,
            Bytes::from(val(4242, 64)),
        )
        .await
        .expect("apply_at");
    assert_eq!(
        outcome,
        ApplyOutcome::Stale,
        "revalidation must fail on a merge-superseded leaf"
    );
    assert_eq!(
        META_KV_COMMIT_SMO_RETRIES.load(Ordering::Relaxed),
        retries0 + 1,
        "meta_kv_commit_smo_retries counts the revalidation failure"
    );
    // The full writer loop (re-resolve → retry) lands on the successor.
    tree.insert(&ikey(5), val(4242, 64))
        .await
        .expect("retried insert");
    assert_eq!(
        tree.lookup(&ikey(5)).await.expect("lookup").as_deref(),
        Some(&val(4242, 64)[..])
    );
    // The kept keys the merge folded from BOTH leaves still serve.
    for i in (0..n).step_by(20) {
        assert_eq!(
            tree.lookup(&ikey(i)).await.expect("lookup").as_deref(),
            Some(&val(i, 64)[..]),
            "kept key {i} after the merge"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn merge_vs_commit_storm_loses_nothing() {
    const WRITERS: u64 = 8;
    const PER_WRITER: u64 = 2_500;

    let mut vol = Vol::new(64 * 1024, 4096, 0);
    let tree = Arc::new(vol.tree(TREE_DENTRIES).await);
    let merges0 = META_KV_NODE_MERGES.load(Ordering::Relaxed);

    // One serialized SMO context on its own task — the checkpoint task's
    // role: threshold maintenance plus the merge sweep, driven
    // concurrently with the writers.
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    let smo_tree = tree.clone();
    let mut smo_ctx = SmoContext::new(vol.alloc.clone());
    let smo_cache = vol.cache.clone();
    let smo_seq = vol.seq.clone();
    let smo_alloc = vol.alloc.clone();
    let smo = tokio::spawn(async move {
        let mut merges = 0u64;
        loop {
            let out = smo_tree
                .run_maintenance(&mut smo_ctx)
                .await
                .expect("maintenance under storm");
            let seq = smo_seq.load(Ordering::Relaxed);
            smo_cache.set_durable_tail(seq);
            smo_alloc.advance_durable(seq);
            let sweep = smo_tree
                .merge_underfull(&mut smo_ctx, false)
                .await
                .expect("merge sweep under storm");
            merges += sweep.outcome.merges;
            if *done_rx.borrow() && !smo_tree.maintenance_pending() {
                return merges;
            }
            if out.appends + out.compactions + out.splits + sweep.outcome.merges == 0
                && done_rx.changed().await.is_err()
            {
                return merges;
            }
        }
    });

    let mut writers = tokio::task::JoinSet::new();
    for w in 0..WRITERS {
        let tree = tree.clone();
        writers.spawn(async move {
            let base = w * PER_WRITER;
            for i in 0..PER_WRITER {
                tree.insert(&ikey(base + i), val(base + i, 48))
                    .await
                    .expect("storm insert");
            }
            // The create/unlink storm: delete 90 %, keep every 10th.
            for i in 0..PER_WRITER {
                if i % 10 != 0 {
                    tree.delete(&ikey(base + i)).await.expect("storm delete");
                }
                if i % 13 == 0 {
                    let got = tree.lookup(&ikey(base + i)).await.expect("storm lookup");
                    assert_eq!(got.as_deref(), Some(&val(base + i, 48)[..]));
                }
            }
        });
    }
    while let Some(r) = writers.join_next().await {
        r.expect("writer task");
    }
    done_tx.send(true).expect("signal done");
    smo.await.expect("smo task");
    // Quiesce: the storm's tail may leave merges for the sweep.
    merge_to_fixpoint(&tree, &mut vol).await;

    // Every kept key present with its value, every deleted key gone.
    let total = WRITERS * PER_WRITER;
    let all = tree
        .range(&ikey(0), &ikey(total), usize::MAX)
        .await
        .expect("full scan");
    let kept: Vec<u64> = (0..total).filter(|i| i % PER_WRITER % 10 == 0).collect();
    assert_eq!(
        all.len(),
        kept.len(),
        "the live population after the storm is exactly the kept keys"
    );
    for ((k, v), want) in all.iter().zip(kept) {
        assert_eq!(&k[..], &ikey(want)[..]);
        assert_eq!(&v[..], &val(want, 48)[..]);
    }
    assert!(
        META_KV_NODE_MERGES.load(Ordering::Relaxed) > merges0,
        "the storm's deletes must merge leaves (lock-then-revalidate-then-retry \
         exercised against the merge SMO)"
    );
    k2_walk(&tree, &vol.cache).await;
}

// ---------------------------------------------------------------------------
// (7) The heap-space arithmetic.
// ---------------------------------------------------------------------------

/// A merge claims one extent and frees two: it is admitted at the
/// compaction floor (one claimable extent above it), refused one below
/// (`space_refused`, nothing changed), and the covering advance returns
/// +1 per merge and +1 per root collapse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_is_admitted_at_the_compaction_floor_and_returns_extents() {
    const EXTENTS: u64 = 96;
    const RESERVE: u64 = 8;
    let floor = compaction_floor_extents(RESERVE);
    assert_eq!(floor, 4);

    // Two leaves under a level-1 root, everything flushed and covered.
    async fn two_leaf_tree(vol: &mut Vol) -> (KvTree, u64) {
        let tree = vol.tree(TREE_INODES).await;
        let mut n = 0u64;
        while tree.root_level().await.expect("level") < 1 {
            tree.insert(&ikey(n), val(n, 64)).await.expect("seed");
            n += 1;
            if n.is_multiple_of(64) {
                drain(&tree, &mut vol.ctx).await;
            }
            assert!(n < 100_000, "split never triggered");
        }
        tree.flush_dirty(&mut vol.ctx).await.expect("flush");
        vol.cover_everything();
        for i in 0..n {
            if i % 20 != 0 {
                tree.delete(&ikey(i)).await.expect("delete");
            }
        }
        tree.flush_dirty(&mut vol.ctx).await.expect("flush deletes");
        vol.cover_everything();
        (tree, n)
    }
    /// Hold claims until exactly `free` extents remain claimable.
    fn drain_to(alloc: &ExtentAllocator, free: u64) -> Vec<u64> {
        let mut held = Vec::new();
        while alloc.free_extents() > free {
            held.push(alloc.claim_internal().expect("drain claim"));
        }
        assert_eq!(alloc.free_extents(), free);
        held
    }

    // ---- Refused one below the floor: nothing changes.
    {
        let mut vol = Vol::new(64 * 1024, EXTENTS, RESERVE);
        let (tree, _) = two_leaf_tree(&mut vol).await;
        let held = drain_to(&vol.alloc, floor);
        let root_before = tree.root();
        let sweep = tree
            .merge_underfull(&mut vol.ctx, false)
            .await
            .expect("sweep at the floor");
        assert!(
            sweep.space_refused,
            "a merge whose claim would dip below the compaction floor is refused"
        );
        assert_eq!(sweep.outcome.merges, 0);
        assert_eq!(sweep.outcome.root_collapses, 0);
        assert_eq!(tree.root(), root_before, "a refused merge changes nothing");
        assert_eq!(vol.alloc.free_extents(), floor, "no claim leaked");
        assert!(
            sweep.candidates >= 1,
            "the underfull leaves are still candidates"
        );
        // One extent returned: admitted.
        vol.alloc.release_unpublished(held[0]);
        let sweep = tree
            .merge_underfull(&mut vol.ctx, false)
            .await
            .expect("sweep one above the floor");
        assert!(!sweep.space_refused);
        assert_eq!(sweep.outcome.merges, 1, "admitted at the floor + 1");
    }

    // ---- Admitted at the floor + 1, and the arithmetic: two leaves and a
    // root → one root leaf frees three extents against one claim.
    {
        let mut vol = Vol::new(64 * 1024, EXTENTS, RESERVE);
        let (tree, n) = two_leaf_tree(&mut vol).await;
        let _held = drain_to(&vol.alloc, floor + 1);
        let free_before = vol.alloc.free_extents();
        let sweep = tree
            .merge_underfull(&mut vol.ctx, false)
            .await
            .expect("sweep");
        assert_eq!(sweep.outcome.merges, 1);
        assert_eq!(
            sweep.outcome.root_collapses, 1,
            "a root left with one child collapses in the same sweep"
        );
        assert_eq!(
            vol.alloc.free_extents(),
            free_before - 1,
            "before coverage the successor's claim is the only budget movement"
        );
        assert_eq!(
            vol.alloc.pending_count(),
            3,
            "both old leaves and the old root are parked, gated on their entries"
        );
        vol.cover_everything();
        assert_eq!(
            vol.alloc.free_extents(),
            free_before + 2,
            "+1 per merge, +1 per collapse once the covering advance runs"
        );
        assert_eq!(tree.root_level().await.expect("level"), 0);
        for i in (0..n).step_by(20) {
            assert_eq!(
                tree.lookup(&ikey(i)).await.expect("lookup").as_deref(),
                Some(&val(i, 64)[..])
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (4) Replay-twice digest equality with merges in the window.
// ---------------------------------------------------------------------------

/// Crash-equivalent reopen of a copied image (no flock holder on a copy).
async fn open_copy(path: &std::path::Path) -> Arc<KvMetaBackend> {
    for _ in 0..200 {
        match KvMetaBackend::open(path).await {
            Ok(be) => return be,
            Err(KvError::Busy(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(e) => panic!("open crash image {}: {e:?}", path.display()),
        }
    }
    panic!("crash image never opened");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_converges_across_crash_windows_of_a_merge_and_a_root_collapse() {
    // Park the cadence: every checkpoint below is one this test drives.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            test_smo_build_pause_release();
            *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = None;
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (be, file) = fresh_volume().await;
    let collapses0 = META_KV_ROOT_COLLAPSES.load(Ordering::Relaxed);

    // 3,000 creates (inode + dentry records; no payload — so the two trees
    // that carry them collapse to root leaves once 90 % is gone).
    const N: u32 = 3_000;
    for i in 0..N {
        be.create(ROOT_INO, &name(i), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
        if i % 500 == 499 {
            be.checkpoint_now().await.expect("checkpoint");
        }
    }
    for i in 0..N {
        if i % 10 != 0 {
            delete_file(&be, i).await;
        }
    }
    // Cover the tombstones so the merge folds elide them (§4.2).
    be.checkpoint_now().await.expect("checkpoint after deletes");
    be.checkpoint_now().await.expect("second covering cycle");

    let census = be.dead_bset_census();
    assert!(
        census.merge_candidates.len() >= 2,
        "a 90 % delete leaves underfull leaves ({} candidates over {} leaves)",
        census.merge_candidates.len(),
        census.leaves
    );

    // ---- Window (a): kill between the successor write and the flip. Arm
    // the build-pause seam on the dentry tree, drive the merges on a task,
    // copy the volume while the first merge is parked (its successor image
    // is on the device, no pointer record exists), release.
    TEST_SMO_BUILD_PAUSE_TREE.store(u64::from(TREE_DENTRIES), Ordering::SeqCst);
    let merges0 = META_KV_NODE_MERGES.load(Ordering::Relaxed);
    let driver = {
        let be = be.clone();
        let candidates = census.merge_candidates.clone();
        tokio::spawn(async move { be.defrag_merge_leaves(&candidates).await })
    };
    let mut parked = None;
    for _ in 0..2_000 {
        if let Some(info) = TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex").clone() {
            parked = Some(info);
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let info = parked.expect("a dentry-tree merge must park in its build window");
    assert!(
        info.is_merge,
        "the parked SMO is the merge (not a compaction)"
    );
    assert_eq!(info.tree_id, TREE_DENTRIES);
    let crash_a = NamedTempFile::new().expect("crash image a");
    std::fs::copy(file.path(), crash_a.path()).expect("copy the parked image");
    test_smo_build_pause_release();
    *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = None;
    let merged = driver
        .await
        .expect("driver task")
        .expect("defrag_merge_leaves");
    assert!(
        merged >= 1,
        "the released merge (and its siblings) run to completion"
    );
    assert!(META_KV_NODE_MERGES.load(Ordering::Relaxed) > merges0);

    // ---- Window (b): kill between the flips and the checkpoint (no cycle
    // has run since the merges).
    let crash_b = NamedTempFile::new().expect("crash image b");
    std::fs::copy(file.path(), crash_b.path()).expect("copy post-merge image");

    // ---- Window (c): after a root collapse, before any checkpoint. Keep
    // merging to the fixpoint — the inode and dentry trees are each ~10 %
    // of a leaf and collapse to root leaves.
    for _ in 0..8 {
        let census = be.dead_bset_census();
        if census.merge_candidates.is_empty() {
            break;
        }
        be.defrag_merge_leaves(&census.merge_candidates)
            .await
            .expect("merge to fixpoint");
    }
    assert!(
        META_KV_ROOT_COLLAPSES.load(Ordering::Relaxed) > collapses0,
        "3,000 creates minus 90 % must collapse a tree to its root leaf"
    );
    let crash_c = NamedTempFile::new().expect("crash image c");
    std::fs::copy(file.path(), crash_c.path()).expect("copy post-collapse image");

    // The live volume itself stays consistent through a checkpoint and a
    // clean shutdown.
    be.checkpoint_now()
        .await
        .expect("checkpoint after the collapses");
    assert!(!be.is_failed());
    be.shutdown().await.expect("clean shutdown");
    drop(be);

    // Every crash image: replay converges (digest equal across two
    // reopens), every acked survivor resolves, every deleted name is gone.
    for (label, img) in [("a", &crash_a), ("b", &crash_b), ("c", &crash_c)] {
        let x = open_copy(img.path()).await;
        assert!(
            !x.is_failed(),
            "window {label}: a replayed merge window is not a failure"
        );
        let d1 = digest_backend(&x).await.expect("digest 1");
        for i in (0..N).step_by(10) {
            x.lookup(ROOT_INO, &name(i))
                .await
                .unwrap_or_else(|e| panic!("window {label}: survivor {} lost: {e:?}", name(i)));
        }
        for i in (1..N).step_by(7) {
            if i % 10 != 0 {
                assert!(
                    x.lookup(ROOT_INO, &name(i)).await.is_err(),
                    "window {label}: deleted {} resurfaced",
                    name(i)
                );
            }
        }
        drop(x);
        let y = open_copy(img.path()).await;
        let d2 = digest_backend(&y).await.expect("digest 2");
        assert_eq!(
            d1, d2,
            "window {label}: replay-twice digests diverge — the merge window's replay \
             is not deterministic"
        );
        // The replayed volume keeps working: a create lands and a cycle runs.
        y.create(
            ROOT_INO,
            &format!("post_{label}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .expect("create on the replayed volume");
        y.checkpoint_now()
            .await
            .expect("checkpoint on the replayed volume");
        assert!(!y.is_failed());
        y.shutdown().await.expect("shutdown");
    }
}

// ---------------------------------------------------------------------------
// (8) The D4 face.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn d4_census_reports_mergeable_leaves_and_the_defrag_arm_merges_them() {
    let (be, _file) = fresh_volume().await;
    // ~40 payload files ⇒ ~10 xattr leaves; keep every 10th.
    for i in 0..40 {
        put_file(&be, i).await.expect("put");
    }
    be.checkpoint_now().await.expect("checkpoint");
    for i in 0..40 {
        if i % 10 != 0 {
            delete_file(&be, i).await;
        }
    }
    be.checkpoint_now().await.expect("checkpoint after deletes");
    be.checkpoint_now().await.expect("covering cycle");

    let routed = Arc::new(RoutedMetaBackend::new(vec![be.clone()]));
    let rows = squeezefs::defrag::measure_d4(&routed)
        .await
        .expect("measure_d4");
    assert_eq!(rows.len(), 1);
    let census = be.dead_bset_census();
    assert!(
        census.merge_candidates.len() >= 2,
        "the census must find the underfull leaves ({} of {})",
        census.merge_candidates.len(),
        census.leaves
    );
    assert_eq!(
        rows[0].mergeable_leaves,
        census.merge_candidates.len() as u64,
        "the D4 report's mergeable_leaves face is the census's candidate count"
    );
    assert_eq!(be.merge_candidates(), rows[0].mergeable_leaves);

    let merges0 = META_KV_NODE_MERGES.load(Ordering::Relaxed);
    let merged = be
        .defrag_merge_leaves(&census.merge_candidates)
        .await
        .expect("defrag merge arm");
    assert!(merged >= 1, "the defrag arm merges the candidates");
    assert!(META_KV_NODE_MERGES.load(Ordering::Relaxed) > merges0);
    let after = be.dead_bset_census();
    assert!(
        after.leaves < census.leaves,
        "the leaf population shrinks ({} → {})",
        census.leaves,
        after.leaves
    );
    assert!(
        after.merge_candidates.len() < census.merge_candidates.len(),
        "fewer candidates remain ({} → {})",
        census.merge_candidates.len(),
        after.merge_candidates.len()
    );
    for i in (0..40).step_by(10) {
        let f = be.lookup(ROOT_INO, &name(i)).await.expect("survivor");
        let v = be
            .getxattr(f.ino, "user.payload")
            .await
            .expect("getxattr")
            .expect("payload");
        assert!(v.iter().all(|b| *b == (i & 0xFF) as u8));
    }
    // Idempotence: a clean re-run over an empty candidate list is a no-op.
    assert_eq!(be.defrag_merge_leaves(&[]).await.expect("re-run"), 0);
    be.checkpoint_now().await.expect("checkpoint");
    be.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// (9) The derivation tie test.
// ---------------------------------------------------------------------------

/// The merge bounds are the split's ¾ fill read backwards — never a
/// constant of their own (the `derivation_sweep_tests` drift-is-red
/// convention): the pair capacity IS the split part capacity, and the
/// underfull bound is that fill target less the split's byte-midpoint
/// balance point (½ of the fold capacity) = ¼ of it. The structural
/// hysteresis follows: a fresh split half (> ½C) never completes a pair
/// with a candidate.
#[test]
fn merge_bounds_derive_from_the_split_fill_target() {
    for node_size in [64 * 1024usize, DEFAULT_NODE_SIZE, 1024 * 1024] {
        let layout = NodeLayout::new(node_size).unwrap();
        let cap = layout.fold_capacity();
        assert_eq!(
            layout.merge_pair_capacity(),
            layout.split_part_capacity(),
            "the pair fits exactly a fresh split part ({node_size})"
        );
        assert_eq!(layout.merge_pair_capacity(), cap * 3 / 4);
        assert_eq!(
            layout.merge_candidate_capacity(),
            layout.split_part_capacity() - cap / 2,
            "underfull = fill target − balance point ({node_size})"
        );
        assert_eq!(layout.merge_candidate_capacity(), cap / 4);
        // Hysteresis: a fresh half plus any candidate exceeds the pair cap.
        assert!(cap / 2 + 1 + layout.merge_candidate_capacity() > layout.merge_pair_capacity());
        // A successor needs more than a candidate's worth of growth before
        // it can split again.
        assert!(cap - layout.merge_pair_capacity() >= layout.merge_candidate_capacity());
    }
    // The merge's claim is admitted at the compaction floor — the §4.7
    // amended reserve split, not a bound of its own.
    for total in [64u64, 4096, 65_536] {
        let reserve = compaction_reserve_extents(total);
        assert_eq!(compaction_floor_extents(reserve), reserve / 2);
    }
}
