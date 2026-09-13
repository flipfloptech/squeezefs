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
//!    `measure_d4` report carries it, `defrag_merge_sweep` (the D4 arm's
//!    bounded, cursor-resumed sweep) merges them (the in-process job
//!    drive is `tests/defrag_tests.rs`).
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
use squeezefs::meta_backend::kv::record::{
    xattr_key, RecordKind, KIND_INTERIOR, NATIVE_FOREST_SLOT, TREE_DENTRIES, TREE_INODES,
    TREE_XATTRS,
};
use squeezefs::meta_backend::kv::tree::{
    decode_interior_value, test_smo_build_pause_arm_slot, test_smo_build_pause_release,
    ApplyOutcome, KvTree, MaintenanceOutcome, SmoContext, KEY_SPACE_MAX, TEST_SMO_BUILD_PAUSED,
    TEST_SMO_BUILD_PAUSE_TREE,
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
    merge_to_fixpoint_counted(tree, vol).await.0
}

/// [`merge_to_fixpoint`] that also reports how many flush + cover + sweep
/// PASSES the fixpoint took (the convergence-bound contracts' instrument;
/// the terminal no-op pass is counted). Every sweep is an unbounded lap
/// (`deadline = None`), so a pass is one whole census of the tree.
async fn merge_to_fixpoint_counted(tree: &KvTree, vol: &mut Vol) -> (MaintenanceOutcome, u32) {
    let mut total = MaintenanceOutcome::default();
    for pass in 1..=64u32 {
        tree.flush_dirty(&mut vol.ctx).await.expect("flush");
        vol.cover_everything();
        // One whole lap; the FIFO valve (thousands of retirements in one
        // lap) is answered the way the D4 arm answers it — cover (the K5
        // stand-in for a checkpoint cycle) and resume from the parked
        // cursor — so a refusal never splits the pass count.
        let mut pass_out = MaintenanceOutcome::default();
        let sweep = loop {
            match tree.merge_underfull(&mut vol.ctx, false, None).await {
                Ok(s) => {
                    pass_out.merges += s.outcome.merges;
                    pass_out.root_collapses += s.outcome.root_collapses;
                    pass_out.interior_merges += s.outcome.interior_merges;
                    if s.lap_complete {
                        break s;
                    }
                }
                Err(KvError::PendingFreeFull { .. }) => vol.cover_everything(),
                Err(e) => panic!("merge sweep: {e:?}"),
            }
        };
        total.merges += pass_out.merges;
        total.root_collapses += pass_out.root_collapses;
        total.interior_merges += pass_out.interior_merges;
        assert!(
            !sweep.space_refused,
            "a roomy heap never refuses a merge for space"
        );
        if pass_out.merges + pass_out.root_collapses == 0 {
            return (total, pass);
        }
    }
    panic!("the merge sweep never reached a fixpoint");
}

/// §4.6a (d) as a checkable law — the tree is at its MERGE FIXED POINT:
/// at every level, no two adjacent nodes under ONE parent are jointly
/// mergeable by the candidate law, and no two adjacent nodes under
/// DIFFERENT parents are jointly mergeable unless their parents are
/// themselves NOT jointly mergeable (the one shape the sibling-only SMO
/// cannot reach: a boundary between two interiors whose separator folds
/// exceed the pair capacity together). Returns `(stranded_pairs,
/// heavy_boundaries)` — the former is always ≤ the latter by construction
/// (the walk panics on any pair the SMO should have merged).
async fn merge_fixed_point(tree: &KvTree, cache: &Arc<NodeCache>) -> (u64, u64) {
    let layout = cache.config().layout;
    let tail = cache.durable_tail();
    let f = |n: &Arc<CachedNode>| {
        n.snapshot()
            .fold_bytes_upper_with(&mut [], layout.merge_pair_capacity(), tail)
            .0
    };
    let pair = |a: usize, b: usize| {
        a + b <= layout.merge_pair_capacity() && a.min(b) <= layout.merge_candidate_capacity()
    };
    // Per level: (node, parent addr, fold) in key order.
    let root = cache.get(tree.root().addr).await.expect("root");
    let mut level: Vec<(Arc<CachedNode>, u64, usize)> = vec![(root.clone(), u64::MAX, f(&root))];
    let (mut stranded, mut heavy) = (0u64, 0u64);
    while level[0].0.level() > 0 {
        let mut next: Vec<(Arc<CachedNode>, u64, usize)> = Vec::new();
        for (node, _, _) in &level {
            let snap = node.snapshot();
            let mut cursor = node.min_key().to_vec();
            while let Some((_, ptr)) = snap.next_live(&cursor, None).expect("next_live") {
                let (addr, _) = decode_interior_value(&ptr).expect("interior value");
                let child = cache.get(addr).await.expect("child");
                cursor = key_successor(child.max_key());
                let fc = f(&child);
                next.push((child, node.addr(), fc));
            }
        }
        let parents = &level;
        for w in next.windows(2) {
            let ((a, pa, fa), (b, pb, fb)) = (&w[0], &w[1]);
            if !pair(*fa, *fb) {
                continue;
            }
            assert_ne!(
                pa,
                pb,
                "adjacent SIBLINGS {:#x} ({fa} B) and {:#x} ({fb} B) at level {} are jointly \
                 mergeable — the sweep is not at its fixed point",
                a.addr(),
                b.addr(),
                a.level()
            );
            let fpa = parents
                .iter()
                .find(|(p, _, _)| p.addr() == *pa)
                .map(|(_, _, f)| *f);
            let fpb = parents
                .iter()
                .find(|(p, _, _)| p.addr() == *pb)
                .map(|(_, _, f)| *f);
            let (fpa, fpb) = (fpa.expect("parent a"), fpb.expect("parent b"));
            assert!(
                !pair(fpa, fpb),
                "adjacent nodes {:#x} and {:#x} at level {} straddle parents {pa:#x} ({fpa} B) \
                 and {pb:#x} ({fpb} B) that are themselves jointly mergeable — the interior \
                 merge the sibling law owes did not run",
                a.addr(),
                b.addr(),
                a.level()
            );
            stranded += 1;
        }
        // A stranded child pair sits under exactly one adjacent-parent
        // boundary whose parents cannot merge — count those boundaries.
        for w in level.windows(2) {
            if !pair(w[0].2, w[1].2) {
                heavy += 1;
            }
        }
        level = next;
    }
    assert!(stranded <= heavy, "stranded pairs exceed heavy boundaries");
    (stranded, heavy)
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

    // Concurrent latch-free readers over the five keys that survive BOTH
    // phases, for the whole collapse: they may restart (a root swap
    // lowers the height mid-walk) but never error and never see a wrong
    // value.
    let survivors: Vec<u64> = (0..n).filter(|i| i % 50 == 0).collect();
    let keep: Vec<u64> = survivors.iter().copied().take(5).collect();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut readers = tokio::task::JoinSet::new();
    for r in 0..4u64 {
        let tree = tree.clone();
        let keep = keep.clone();
        let mut stop_rx = stop_rx.clone();
        readers.spawn(async move {
            let mut served = 0u64;
            let mut i = r as usize;
            while !*stop_rx.borrow_and_update() {
                let k = keep[i % keep.len()];
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

    // Phase B: delete down to the 5 kept keys — the tree collapses to a
    // root leaf (height 0), the readers still running.
    for &k in &survivors {
        if !keep.contains(&k) {
            tree.delete(&ikey(k)).await.expect("delete");
        }
    }
    let out_b = merge_to_fixpoint(&tree, &mut vol).await;
    stop_tx.send(true).expect("stop readers");
    let mut served = 0u64;
    while let Some(r) = readers.join_next().await {
        served += r.expect("reader task");
    }
    assert!(
        served > 0,
        "the readers must have served during the collapse"
    );
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
                .merge_underfull(&mut smo_ctx, false, None)
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
            // The create/unlink storm: delete 90 %, keep every 10th, with
            // latch-free reads interleaved (a kept key serves, a deleted
            // one is gone — through every merge racing this writer).
            for i in 0..PER_WRITER {
                if i % 10 != 0 {
                    tree.delete(&ikey(base + i)).await.expect("storm delete");
                }
                if i % 13 == 0 {
                    let got = tree.lookup(&ikey(base + i)).await.expect("storm lookup");
                    let want = (i % 10 == 0).then(|| val(base + i, 48));
                    assert_eq!(got.as_deref(), want.as_deref());
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
    let kept: Vec<u64> = (0..total)
        .filter(|i| (i % PER_WRITER).is_multiple_of(10))
        .collect();
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
            .merge_underfull(&mut vol.ctx, false, None)
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
            .merge_underfull(&mut vol.ctx, false, None)
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
            .merge_underfull(&mut vol.ctx, false, None)
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

/// The delete stride that lets a create-only population of `n` files
/// collapse to a root leaf on THIS volume's layout (see the crash-window
/// test): the heaviest tree's surviving bytes ≤ ½ × `merge_pair_capacity`
/// — half the merge threshold, the other half being room for the
/// tombstones the folds have not yet elided. Per-file bytes are the
/// admission's own (`RECORD_HEADER_LEN` + staged key + value): a flat
/// volume's heaviest tree holds the inode record alone; a forest's mixed
/// tree holds the inode AND the dentry (each key one kind byte longer).
fn survivor_stride(be: &KvMetaBackend, n: u32) -> u32 {
    use squeezefs::meta_backend::kv::record::{DentryValue, InodeValue, RECORD_HEADER_LEN};
    let layout = NodeLayout::new(be.superblock().node_size as usize).expect("layout");
    let key_extra = usize::from(be.symmetric_forest());
    let inode_rec = RECORD_HEADER_LEN
        + 8
        + key_extra
        + InodeValue {
            mode: libc::S_IFREG | 0o644,
            nlink: 1,
            ..Default::default()
        }
        .encode()
        .len();
    let dentry_rec = RECORD_HEADER_LEN
        + 16
        + key_extra
        + DentryValue {
            child_ino: 1,
            file_type: (libc::S_IFREG >> 12) as u8,
            name: name(0).into_bytes(),
        }
        .encode()
        .expect("dentry value")
        .len();
    let per_file = if be.symmetric_forest() {
        inode_rec + dentry_rec
    } else {
        inode_rec.max(dentry_rec)
    };
    let budget = layout.merge_pair_capacity() / 2;
    let survivors_max = (budget / per_file).max(1) as u32;
    n.div_ceil(survivors_max).max(2)
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

    // 3,000 creates (inode + dentry records; no payload), then all but
    // every `stride`-th deleted — so the tree(s) carrying them collapse
    // to root leaves. The stride DERIVES from the merge law (§4.6a): a
    // tree collapses only if its SURVIVING records fit the merge
    // threshold (`merge_pair_capacity`, the split's ¾ fill read
    // backwards) with room for the tombstones a covering cycle has not
    // yet let the folds elide — the heaviest tree's survivors are held
    // to HALF that threshold. A flat volume's heaviest tree carries one
    // record per file (the inode record); a forest's mixed tree carries
    // a file's inode AND its dentry, so the same law keeps fewer
    // survivors there (at the flat stride the forest's survivors sat at
    // 85 % of the threshold and the last merge was a coin flip on the
    // un-elided tombstones — review round 3, Issue 17).
    const N: u32 = 3_000;
    let stride = survivor_stride(&be, N);
    for i in 0..N {
        be.create(ROOT_INO, &name(i), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
        if i % 500 == 499 {
            be.checkpoint_now().await.expect("checkpoint");
        }
    }
    for i in 0..N {
        if i % stride != 0 {
            delete_file(&be, i).await;
        }
    }
    // Cover the tombstones so the merge folds elide them (§4.2).
    be.checkpoint_now().await.expect("checkpoint after deletes");
    be.checkpoint_now().await.expect("second covering cycle");

    let census = be.dead_bset_census();
    assert!(
        census.merge_candidates.len() >= 2,
        "deleting all but every {stride}-th file leaves underfull leaves ({} candidates over {} \
         leaves)",
        census.merge_candidates.len(),
        census.leaves
    );

    // ---- Window (a): kill between the successor write and the flip. Arm
    // the build-pause seam on the tree holding the dentry records (the
    // dentry tree; on a forest the native slot tree, whose nodes carry
    // header tree id 0 — the seam arms it by SLOT), drive the merges on a
    // task, copy the volume while the first merge is parked (its successor
    // image is on the device, no pointer record exists), release.
    let forest = be.symmetric_forest();
    if forest {
        test_smo_build_pause_arm_slot(NATIVE_FOREST_SLOT);
    } else {
        TEST_SMO_BUILD_PAUSE_TREE.store(u64::from(TREE_DENTRIES), Ordering::SeqCst);
    }
    let merges0 = META_KV_NODE_MERGES.load(Ordering::Relaxed);
    let driver = {
        let be = be.clone();
        tokio::spawn(async move { be.defrag_merge_sweep(None).await })
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
    if forest {
        assert_eq!(
            info.tree_id, KIND_INTERIOR,
            "a slot tree's nodes carry header id 0"
        );
        assert_eq!(info.forest_slot, Some(NATIVE_FOREST_SLOT));
    } else {
        assert_eq!(info.tree_id, TREE_DENTRIES);
    }
    let crash_a = NamedTempFile::new().expect("crash image a");
    std::fs::copy(file.path(), crash_a.path()).expect("copy the parked image");
    test_smo_build_pause_release();
    *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = None;
    let report = driver
        .await
        .expect("driver task")
        .expect("defrag_merge_sweep");
    assert!(
        report.merges >= 1,
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
        let r = be
            .defrag_merge_sweep(None)
            .await
            .expect("merge to fixpoint");
        if r.merges + r.root_collapses == 0 {
            break;
        }
    }
    assert!(
        META_KV_ROOT_COLLAPSES.load(Ordering::Relaxed) > collapses0,
        "3,000 creates minus all but every {stride}-th must collapse a tree to its root leaf"
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
        for i in (0..N).step_by(stride as usize) {
            x.lookup(ROOT_INO, &name(i))
                .await
                .unwrap_or_else(|e| panic!("window {label}: survivor {} lost: {e:?}", name(i)));
        }
        for i in (1..N).step_by(7) {
            if i % stride != 0 {
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
    let report = be.defrag_merge_sweep(None).await.expect("defrag merge arm");
    assert!(
        report.lap_complete,
        "an unbounded arm call completes its lap"
    );
    assert!(report.merges >= 1, "the defrag arm merges the candidates");
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
    // Idempotence: the arm at its fixed point merges nothing, and its
    // published count is the census's (one source of truth).
    let again = be.defrag_merge_sweep(None).await.expect("re-run");
    assert_eq!(
        again.merges + again.root_collapses,
        0,
        "nothing mergeable ⇒ nothing merged"
    );
    assert_eq!(again.candidates, be.merge_candidates());
    assert_eq!(
        again.candidates,
        be.dead_bset_census().merge_candidates.len() as u64,
        "the arm's lap count is the census's count"
    );
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

// ---------------------------------------------------------------------------
// Finalization (the three "stated, not done" items, owner ruling
// 2026-09-11): (10) cross-parent shrinkage proven + the convergence
// bound; (11) the exact candidates gauge; (12)/(13) the bounded,
// cursor-resumed sweep and its budget derivation.
// ---------------------------------------------------------------------------

/// Live children of an interior node, in key order.
async fn children_of(cache: &Arc<NodeCache>, node: &Arc<CachedNode>) -> Vec<Arc<CachedNode>> {
    let snap = node.snapshot();
    let mut out = Vec::new();
    let mut cursor = node.min_key().to_vec();
    while let Some((_, ptr)) = snap.next_live(&cursor, None).expect("next_live") {
        let (addr, _) = decode_interior_value(&ptr).expect("interior value");
        let child = cache.get(addr).await.expect("child");
        cursor = key_successor(child.max_key());
        out.push(child);
    }
    out
}

/// (10) §4.6a (d) PROVEN: underfull leaves on BOTH sides of every parent
/// boundary — each level-1 interior keeps one record in its leftmost and
/// one in its rightmost leaf, everything else deleted — can only merge
/// after their parents merge. The sweep must (a) run the interior merges
/// (`interior_merges ≥ parents − 1`), (b) reach the merge fixed point
/// with NO stranded pair (every parent here is tiny, so no boundary is
/// heavy), (c) collapse the height, and (d) converge within the derived
/// bound: `passes ≤ 3·h₀ + 1` — per level, one pass merges it, one pass
/// (after coverage elides the tombstones the merges minted above) merges
/// the parent level, one pass regroups the level under its merged
/// parents; plus the terminal no-op pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_parent_underfull_leaves_shrink_through_interior_merges_to_a_fixed_point() {
    // 64 KiB nodes + 6 KiB values ⇒ ~9 records per full leaf, ~4–5 per
    // split half; two survivors (12.3 KB) sit under the ¼C = 15.3 KB
    // candidate bound, three do not pair with three (37 KB + 37 KB > ¾C).
    const VAL: usize = 6 * 1024;
    let mut vol = Vol::new(64 * 1024, 8192, 0);
    let tree = vol.tree(TREE_INODES).await;
    let interior_merges0 =
        squeezefs::meta_backend::kv::META_KV_INTERIOR_MERGES.load(Ordering::Relaxed);

    // Grow until the root is level 2 with at least four level-1 interiors
    // (~3,000 leaves): the K5 stand-in for the checkpoint releases the
    // splits' retirements as it goes, or the heap would fill with parked
    // predecessors long before the shape forms.
    let mut n = 0u64;
    loop {
        tree.insert(&ikey(n), val(n, VAL)).await.expect("insert");
        n += 1;
        if n.is_multiple_of(8) {
            drain(&tree, &mut vol.ctx).await;
            vol.cover_everything();
        }
        if tree.root_level().await.expect("level") == 2 {
            let root = vol.cache.get(tree.root().addr).await.expect("root");
            if children_of(&vol.cache, &root).await.len() >= 4 {
                break;
            }
        }
        assert!(
            n < 80_000,
            "the height-3 / four-parent fixture never formed"
        );
    }
    tree.flush_dirty(&mut vol.ctx).await.expect("flush");
    let (leaves0, height0) = k2_walk(&tree, &vol.cache).await;
    assert_eq!(height0, 2);

    // The delete plan: per level-1 interior, keep the FIRST record of its
    // leftmost leaf and of its rightmost leaf; delete everything else.
    let root = vol.cache.get(tree.root().addr).await.expect("root");
    let parents = children_of(&vol.cache, &root).await;
    let parents0 = parents.len() as u64;
    let mut keep: Vec<Vec<u8>> = Vec::new();
    for p in &parents {
        let leaves = children_of(&vol.cache, p).await;
        for leaf in [&leaves[0], &leaves[leaves.len() - 1]] {
            let first = tree
                .range(leaf.min_key(), leaf.max_key(), 1)
                .await
                .expect("range")
                .into_iter()
                .next()
                .expect("a leaf holds a record");
            keep.push(first.0.to_vec());
        }
    }
    keep.dedup();
    for i in 0..n {
        if !keep.contains(&ikey(i)) {
            tree.delete(&ikey(i)).await.expect("delete");
        }
    }

    let (out, passes) = merge_to_fixpoint_counted(&tree, &mut vol).await;
    assert!(
        out.interior_merges >= parents0 - 1,
        "collapsing {parents0} level-1 interiors to one takes ≥ {} interior merges (ran {})",
        parents0 - 1,
        out.interior_merges
    );
    assert!(
        squeezefs::meta_backend::kv::META_KV_INTERIOR_MERGES.load(Ordering::Relaxed)
            - interior_merges0
            >= parents0 - 1,
        "meta_kv_interior_merges is the level-≥1 face of meta_kv_node_merges"
    );
    let bound = 3 * u32::from(height0) + 1;
    assert!(
        passes <= bound,
        "the fixed point took {passes} passes, above the derived bound 3·h₀+1 = {bound} \
         (h₀ = {height0})"
    );
    let (stranded, heavy) = merge_fixed_point(&tree, &vol.cache).await;
    assert_eq!(
        (stranded, heavy),
        (0, 0),
        "every parent here is tiny: no boundary is heavy, so nothing may stay stranded"
    );
    let (leaves1, height1) = k2_walk(&tree, &vol.cache).await;
    assert!(
        leaves1 < leaves0 / 8,
        "the leaf population collapses ({leaves0} → {leaves1})"
    );
    assert_eq!(
        height1,
        if leaves1 > 1 { 1 } else { 0 },
        "with {leaves1} leaves left the tree is one interior level (or a root leaf)"
    );
    assert!(
        height1 < height0,
        "the height collapsed ({height0} → {height1})"
    );
    for k in &keep {
        assert!(
            tree.lookup(k).await.expect("lookup").is_some(),
            "a kept record survives the cross-parent shrinkage"
        );
    }
    assert_eq!(
        tree.range(&ikey(0), &ikey(n), usize::MAX)
            .await
            .expect("range")
            .len(),
        keep.len(),
        "exactly the kept records remain"
    );
}

/// (12) The sweep is BOUNDED per call and RESUMES from its cursor: a
/// call whose deadline has already passed processes at least one node
/// (the progress law) and returns `lap_complete == false`; repeated
/// bounded calls complete the lap the unbounded call would have, at the
/// same fixed point, publishing the same exact candidate count; a
/// completed lap resets the cursor so the next call starts afresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_sweep_is_bounded_per_call_and_resumes_from_its_cursor() {
    let mut vol = Vol::new(64 * 1024, 4096, 0);
    let tree = vol.tree(TREE_INODES).await;
    for (k, &i) in permutation(6_000, 3).iter().enumerate() {
        tree.insert(&ikey(i), val(i, 80)).await.expect("insert");
        if k % 256 == 255 {
            drain(&tree, &mut vol.ctx).await;
        }
    }
    for i in 0..6_000u64 {
        if i % 10 != 0 {
            tree.delete(&ikey(i)).await.expect("delete");
        }
    }
    tree.flush_dirty(&mut vol.ctx).await.expect("flush");
    vol.cover_everything();
    let (leaves0, _) = k2_walk(&tree, &vol.cache).await;
    assert!(leaves0 >= 4, "a multi-leaf fixture ({leaves0} leaves)");

    // An already-expired deadline: exactly the progress law's one node.
    let mut calls = 0u32;
    let mut merges = 0u64;
    loop {
        let sweep = tree
            .merge_underfull(&mut vol.ctx, false, Some(std::time::Instant::now()))
            .await
            .expect("bounded sweep");
        calls += 1;
        merges += sweep.outcome.merges + sweep.outcome.root_collapses;
        assert!(
            sweep.projections >= 1,
            "a bounded call always makes progress (≥ 1 node projected)"
        );
        if sweep.lap_complete {
            break;
        }
        assert!(
            sweep.projections <= 2,
            "an expired deadline bounds the call to the progress minimum (projected {})",
            sweep.projections
        );
        assert!(calls < 10_000, "the bounded lap never completed");
    }
    assert!(
        calls > 2,
        "a {leaves0}-leaf lap under an expired deadline must span several calls ({calls})"
    );
    assert!(merges > 0, "the bounded lap merged the underfull leaves");
    // Cover the tombstones the merges minted, then finish to the fixpoint
    // with unbounded laps — the bounded laps must have left the SAME fixed
    // point an unbounded sweep reaches.
    let (rest, _) = merge_to_fixpoint_counted(&tree, &mut vol).await;
    merge_fixed_point(&tree, &vol.cache).await;
    // The published count is exact: the last lap's `candidates` equals a
    // census over the same tree with the same predicate.
    let last = tree
        .merge_underfull(&mut vol.ctx, false, None)
        .await
        .expect("a fresh lap");
    assert!(last.lap_complete);
    assert_eq!(
        last.outcome.merges + last.outcome.root_collapses,
        0,
        "at the fixpoint"
    );
    assert_eq!(
        last.candidates,
        tree.merge_candidate_census(),
        "the sweep's published candidate count is the census's"
    );
    let _ = rest;
}

/// (13) The sweep budget derives from the checkpoint tick period — the
/// SAME law finding 49 gave the threshold drain ("bounded by one cadence
/// period; at least one item per pass") — never a constant of its own.
#[test]
fn merge_sweep_budget_derives_from_the_checkpoint_tick_period() {
    use squeezefs::meta_backend::kv::checkpoint::{
        checkpoint_tick_period_ms, merge_sweep_budget_ms,
    };
    // The shipped 50 ms flush, strict mode's 100 ms tick, a slow venue.
    assert_eq!(merge_sweep_budget_ms(50), 50);
    assert_eq!(merge_sweep_budget_ms(0), 100);
    assert_eq!(merge_sweep_budget_ms(2_000), 2_000);
    for ms in [0u64, 1, 50, 100, 250, 1_000, 5_000, 60_000] {
        assert_eq!(
            merge_sweep_budget_ms(ms),
            checkpoint_tick_period_ms(ms),
            "the budget IS the tick period ({ms} ms)"
        );
    }
}

/// (11) `meta_kv_merge_candidates` is EXACT: after a sweep that the
/// compaction floor cut mid-wave, the gauge equals a census taken under
/// the tail it was counted with (RED on the landed sweep: it counted only
/// the leaves it saw before the refusal — its own trace read 'merge sweep
/// … 1 underfull leaves … backlog=true' against a ~300-leaf tree); at
/// quiescence it equals the D4 report's `mergeable_leaves`. The audit
/// reads the gauge, a census under the gauge's tail and a census under the
/// current tail atomically under the SMO mutex: `gauge ==
/// census_at_gauge_tail` is the law (no user commit touched the tree since
/// the lap), `census_now ≥ census_at_gauge_tail` its monotone corollary (a
/// later tail only elides more tombstones). Also pins the wave bound: the
/// sweeps a heap-full recovery takes are bounded by the height term plus a
/// geometric wave term — each wave's merges fund the next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_candidates_gauge_is_exact_mid_wave_and_at_quiescence() {
    // Park the cadence and drive every cycle by hand: the fill and the
    // deletes cycle for ring room themselves, so the only sweeps are the
    // ones this test's cycles run.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (be, _file) = fresh_volume().await;
    let mut landed = 0u32;
    let refusal = loop {
        match put_file(&be, landed).await {
            Ok(()) => landed += 1,
            Err(e) => break e,
        }
        if landed.is_multiple_of(16) {
            be.checkpoint_now().await.expect("fill cycle");
        }
        assert!(landed < 100_000, "harness bug: the fill never refused");
    };
    assert_eq!(refusal.to_errno(), libc::ENOSPC, "got {refusal:?}");
    be.checkpoint_now().await.expect("post-fill cycle");

    // Throttle the wave: a foreign claimant holds every claimable extent
    // but ONE above the compaction floor, so a sweep admits at most one
    // merge (plus what its own returns fund) before the floor refuses it
    // — the deletes' compactions still land, one promise at a time.
    let alloc = Arc::clone(be.allocator());
    let floor = compaction_floor_extents(alloc.reserve_extents());
    let mut held: Vec<u64> = Vec::new();
    while alloc.free_extents() > floor + 1 {
        held.push(alloc.claim_internal().expect("drain claim"));
    }
    for i in 0..landed {
        if i % 10 != 0 {
            delete_file(&be, i).await;
        }
        if i % 32 == 31 {
            be.checkpoint_now().await.expect("delete cycle");
        }
        // Re-take what the cycles returned as they land: the throttle is
        // ONE admitted merge per lap, and a merge's returned extents (one
        // to two barriered cycles later) would otherwise fund the next
        // lap's second — enough laps (the deletes' own ring-full cycles
        // land at layout-dependent points) and the wave completes before
        // the mid-wave read below.
        while alloc.free_extents() > floor + 1 {
            held.push(alloc.claim_internal().expect("re-drain claim"));
        }
    }
    // The volume is full again by construction (the claimant re-takes the
    // room the deletes' few merges returned): creates land in whatever
    // in-place room remains, then a split is refused, the heap-full
    // posture latches, and the next cycle runs the sweep — cut at the
    // floor with hundreds of underfull leaves standing: mid-wave.
    while alloc.free_extents() > floor + 1 {
        held.push(alloc.claim_internal().expect("re-drain claim"));
    }
    let (probes, refusal) = fill_until_refused(&be, 700_000).await;
    assert_eq!(refusal.to_errno(), libc::ENOSPC, "got {refusal:?}");
    assert!(
        probes < 16,
        "a drained heap refuses within the in-place room ({probes})"
    );
    assert!(be.heap_full(), "the refused create latches the posture");
    let laps0 = be.merge_laps();
    be.checkpoint_now().await.expect("the mid-wave cycle");
    assert!(
        be.merge_laps() > laps0,
        "the heap-full posture ran a sweep lap"
    );
    let a = be.merge_candidates_audit().await;
    assert_eq!(
        a.gauge, a.census_at_gauge_tail,
        "mid-wave the gauge must be the complete count under its tail (gauge {}, census \
         {}) — a sweep cut at the floor still counts every underfull leaf",
        a.gauge, a.census_at_gauge_tail
    );
    assert!(
        a.census_now >= a.census_at_gauge_tail,
        "a later tail can only add candidates ({} → {})",
        a.census_at_gauge_tail,
        a.census_now
    );
    assert!(
        a.gauge > 8,
        "a 90 %-deleted volume mid-wave has many underfull leaves ({})",
        a.gauge
    );
    let candidates0 = a.gauge;
    // The height term: the tree holding the xattr records (the xattr
    // tree on a flat volume, the native slot tree on a forest one).
    let height0 = be
        .record_locator(TREE_XATTRS, &xattr_key(1, 0, 0))
        .expect("locator")
        .expect("the xattr records' tree exists")
        .0
        .root_level()
        .await
        .expect("xattr root level");

    // Release the claimant: the recovery wave runs to quiescence; the gauge
    // from the LAST lap must equal the census under its tail, and the D4
    // report's census — the same predicate — must equal ITS own publish.
    for ext in held {
        alloc.release_unpublished(ext);
    }
    let room0 = be
        .free_extents()
        .saturating_sub(compaction_floor_extents(alloc.reserve_extents()))
        .max(1);
    let sweeps0 = be.merge_sweeps();
    cycle_until_quiescent(&be, 6, 400).await;
    let a = be.merge_candidates_audit().await;
    assert_eq!(
        a.gauge, a.census_at_gauge_tail,
        "at quiescence the last lap's gauge is the census under its tail"
    );
    let routed = Arc::new(RoutedMetaBackend::new(vec![be.clone()]));
    let rows = squeezefs::defrag::measure_d4(&routed)
        .await
        .expect("measure_d4");
    assert_eq!(
        rows[0].mergeable_leaves,
        be.merge_candidates(),
        "the D4 report's mergeable_leaves IS meta_kv_merge_candidates (one predicate, one publish)"
    );
    let a = be.merge_candidates_audit().await;
    assert_eq!(
        (a.gauge, a.census_at_gauge_tail),
        (rows[0].mergeable_leaves, rows[0].mergeable_leaves),
        "the report's census re-counts exactly under the tail it was taken with"
    );
    // The wave bound: the recovery's sweeps ≤ the level term (3·h₀ + 1, the
    // K5 contract's) + two cycles per wave, waves ≤ ⌈log₂(candidates/room₀
    // + 1)⌉ + 1 (each wave returns its merges' extents to fund the next),
    // + the terminal fixed-point lap and the quiescence probe's slack.
    let waves = ((candidates0 as f64 / room0 as f64 + 1.0).log2().ceil() as u64) + 1;
    let bound = 3 * u64::from(height0) + 1 + 2 * waves + 2;
    let sweeps = be.merge_sweeps() - sweeps0;
    assert!(
        sweeps <= bound,
        "the recovery took {sweeps} sweeps, above the derived bound {bound} (h₀ {height0}, \
         candidates {candidates0}, room₀ {room0}, waves {waves})"
    );
    be.shutdown().await.expect("shutdown");
}

/// (14) **A refused member strands no heap promise** (§4.7 P1, review
/// round 3 Issue 17 — the stamped mid-wave test's ENOSPC fixpoint,
/// attributed from its debug trace: `heap_promised = 1` for 32 cycles
/// with `pending-free = 0`, so `claimable = free − 1 = compaction floor`
/// and neither a delete's compaction nor a sweep's merge could ever be
/// admitted again). The admission promises a member's leaves ONE AT A
/// TIME in record order; when a later leaf of the SAME member is refused
/// the member never lands — but the earlier leaf kept its promise, on a
/// node with nothing pending for the flush pass to consume it with.
///
/// The shape, built to the byte on either layout with the admission's
/// own arithmetic (`projected_log_end` over `RecordRef::encoded_len`):
/// leaf D (the leaf the new name's dentry lands in) and leaf I (the tail
/// leaf holding the last file's inode) are each padded with DEAD bytes
/// — D by renames of one file between two names that hash into it, I by
/// `setattr` inode-record shadows — until the next pad op would
/// overflow, so each leaf's fold stays small and its next append is a
/// COMPACTION (need 1); the final name is long enough that its dentry
/// record is bigger than a rename's footprint, so it overflows D for
/// certain. The heap is drained to exactly one claimable extent above
/// the compaction floor. `create` stages the inode Put first: I is
/// promised, D is refused, the member is refused. The law: after the
/// refusal `heap_promised` is 0 and a single-leaf compaction is still
/// admissible (with the strand, `claimable − 1 < floor` refuses it for
/// ever — the wedge).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_multi_leaf_member_strands_no_heap_promise() {
    use squeezefs::meta_backend::kv::record::{
        dentry_key, dentry_name_hash54, inode_key, DentryValue, InodeValue, RECORD_HEADER_LEN,
    };
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (be, _file) = fresh_volume().await;
    let layout = NodeLayout::new(be.superblock().node_size as usize).expect("layout");
    let node_size = layout.node_size();
    let seed = be.superblock().hash_seed;
    // The staged key is one kind byte longer on a forest.
    let key_extra = usize::from(be.symmetric_forest());

    // Enough files that the inode records span ≥ 2 leaves on both
    // layouts (the root inode's leaf, the last inode's leaf and the new
    // name's dentry leaf must be distinct where the shape needs them),
    // few enough that the dentry leaf keeps room for the padding.
    const N: u32 = 800;
    for i in 0..N {
        be.create(ROOT_INO, &name(i), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
    }
    be.checkpoint_now().await.expect("checkpoint");

    // The LIVE leaf a key resolves to (the admission's own resolve),
    // through the one router — re-read at every probe, never cached: a
    // compaction under the padding would leave a stale object behind.
    let leaf_of = |kind: u8, legacy: Vec<u8>| {
        let be = be.clone();
        async move {
            let (tree, key) = be
                .record_locator(kind, &legacy)
                .expect("locator")
                .expect("tree exists");
            tree.resolve_leaf(&key).await.expect("leaf resolves")
        }
    };
    let projected = |kind: u8, legacy: Vec<u8>, extra: usize| {
        let leaf_of = &leaf_of;
        async move {
            let leaf = leaf_of(kind, legacy).await;
            let g = leaf.lock().read().await;
            (leaf.addr(), g.projected_log_end(&layout, extra))
        }
    };
    let rec_len = |key_len: usize, value_len: usize| RECORD_HEADER_LEN + key_len + value_len;
    let inode_value_len = InodeValue {
        mode: libc::S_IFREG | 0o644,
        nlink: 1,
        ..Default::default()
    }
    .encode()
    .len();
    let inode_put = rec_len(8 + key_extra, inode_value_len);
    let dentry_put = |nm: &str| {
        rec_len(
            16 + key_extra,
            DentryValue {
                child_ino: 1,
                file_type: (libc::S_IFREG >> 12) as u8,
                name: nm.as_bytes().to_vec(),
            }
            .encode()
            .expect("dentry value")
            .len(),
        )
    };
    let dentry_delete = rec_len(16 + key_extra, 0);

    // The final name: long enough that its dentry record outweighs a
    // 9-char rename's whole D footprint (Delete + Put), so it overflows
    // whatever room the padding leaves.
    let new_name = "zz-strand-the-member-whose-second-leaf-is-refused";
    let dkey = dentry_key(ROOT_INO, dentry_name_hash54(new_name.as_bytes(), seed), 0).to_vec();
    let d_addr = projected(TREE_DENTRIES, dkey.clone(), 0).await.0;
    // Does the parent's own inode record share D's leaf (a forest's
    // first-leaf shape)? Then every dentry mutation's parent Put lands
    // on D too — in the padding AND in the final create alike.
    let parent_on_d = projected(TREE_INODES, inode_key(ROOT_INO).to_vec(), 0)
        .await
        .0
        == d_addr;
    let parent_put = if parent_on_d { inode_put } else { 0 };
    let create_d_bytes = dentry_put(new_name) + parent_put;
    // Two 9-char names hashing into D for the rename padding.
    let mut pad_names: Vec<String> = Vec::new();
    let mut k = 0u32;
    while pad_names.len() < 2 {
        let candidate = format!("zz-{k:06}");
        k += 1;
        assert!(k < 20_000, "no pad name hashed into leaf D");
        let key = dentry_key(ROOT_INO, dentry_name_hash54(candidate.as_bytes(), seed), 0);
        if projected(TREE_DENTRIES, key.to_vec(), 0).await.0 == d_addr {
            pad_names.push(candidate);
        }
    }
    let rename_d_bytes = dentry_delete + dentry_put(&pad_names[0]) + parent_put;
    assert!(
        create_d_bytes > rename_d_bytes,
        "the final dentry record must outweigh a pad rename's footprint"
    );

    // Pad D: the pad file renamed back and forth between the two names —
    // dead bytes only, the fold stays put — until the next rename would
    // overflow D. One CYCLE per pad op: frames are page-aligned, so a
    // probe taken with records still in the open delta can be split by
    // the threshold writeback into a second frame before the next op is
    // admitted (the op the probe said fits then overflows — and is
    // promised); landing every op first makes the probe the admission's
    // exact input. Every pad op is a fits-in-place commit while the heap
    // has room.
    be.create(ROOT_INO, &pad_names[0], libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("pad file");
    be.checkpoint_now().await.expect("land the pad file");
    let mut cur = 0usize;
    loop {
        let (addr, end) = projected(TREE_DENTRIES, dkey.clone(), rename_d_bytes).await;
        assert_eq!(
            addr, d_addr,
            "D compacted under the padding — the fold grew"
        );
        if end > node_size {
            break;
        }
        be.rename(ROOT_INO, &pad_names[cur], ROOT_INO, &pad_names[1 - cur], 0)
            .await
            .expect("pad rename");
        cur = 1 - cur;
        be.checkpoint_now().await.expect("land the pad rename");
    }
    assert!(
        projected(TREE_DENTRIES, dkey.clone(), create_d_bytes)
            .await
            .1
            > node_size,
        "the final create's dentry overflows D"
    );

    // Pad I (the tail leaf — the last created file's): inode-record
    // shadows, one cycle each, until the next one would overflow.
    let last = be
        .lookup(ROOT_INO, &pad_names[cur])
        .await
        .expect("pad file")
        .ino;
    let ikey = inode_key(last).to_vec();
    let i_addr = projected(TREE_INODES, ikey.clone(), 0).await.0;
    assert_ne!(i_addr, d_addr, "I and D are distinct leaves");
    loop {
        let (addr, end) = projected(TREE_INODES, ikey.clone(), inode_put).await;
        assert_eq!(addr, i_addr, "I compacted under the padding");
        if end > node_size {
            break;
        }
        be.setattr(last, Some(0o600), None, None, None, None, None, None)
            .await
            .expect("pad setattr");
        be.checkpoint_now().await.expect("land the pad setattr");
    }

    // Everything is landed; a covering cycle lets every setup return
    // finish, so nothing is pending or promised when the heap is drained.
    be.checkpoint_now().await.expect("covering cycle");
    assert_eq!(be.heap_promised(), 0, "the setup's SMOs all ran");
    let (addr, end) = projected(TREE_DENTRIES, dkey.clone(), create_d_bytes).await;
    assert!(
        addr == d_addr && end > node_size,
        "D flushed in place, one record from overflow"
    );
    let (addr, end) = projected(TREE_INODES, ikey.clone(), inode_put).await;
    assert!(
        addr == i_addr && end > node_size,
        "I flushed in place, one record from overflow (addr {addr:#x} vs {i_addr:#x}, end {end} \
         vs node {node_size}, inode_put {inode_put})"
    );

    // One claimable extent above the compaction floor, nothing pending.
    let alloc = Arc::clone(be.allocator());
    let floor = compaction_floor_extents(alloc.reserve_extents());
    let mut held: Vec<u64> = Vec::new();
    while alloc.free_extents() > floor + 1 {
        held.push(alloc.claim_internal().expect("drain claim"));
    }
    assert_eq!(alloc.pending_count(), 0, "no returns in flight");

    // The two-leaf member: inode Put on I (compaction, need 1 — the one
    // extent is promised), dentry Put on D (compaction, need 1 — refused
    // at the floor). Refused as a whole.
    let refused = be
        .create(ROOT_INO, new_name, libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect_err("the two-leaf create is refused at the compaction floor");
    assert_eq!(refused.to_errno(), libc::ENOSPC, "got {refused:?}");
    assert!(
        be.lookup(ROOT_INO, new_name).await.is_err(),
        "a refused member landed nothing"
    );
    assert_eq!(
        be.heap_promised(),
        0,
        "a refused member's promises are returned to the ledger — a promise on a node with \
         nothing pending is a permanent claimable deficit (the mid-wave ENOSPC fixpoint)"
    );
    // The consequence the law protects: with the one extent still
    // claimable, a single-leaf compaction is admissible (I's next inode
    // shadow), exactly as it would have been before the refusal.
    be.setattr(last, Some(0o640), None, None, None, None, None, None)
        .await
        .expect("a single-leaf compaction is admitted at floor + 1");
    for ext in held {
        alloc.release_unpublished(ext);
    }
    be.checkpoint_now().await.expect("checkpoint");
    assert!(!be.is_failed());
    be.shutdown().await.expect("shutdown");
}
