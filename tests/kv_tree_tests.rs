//! PR K5 integration tests: the CoW btree + node cache (design §4.5/§4.6)
//! — demand paging with arc-swap immutable snapshots (latch-free reads),
//! clock eviction with dirty pinning + pinned interior, single-flight
//! loads, the cache budget knob, and the §4.6 SMO protocol (serialized SMO
//! execution, interior-locks-SMO-only, snapshot-then-write writeback,
//! three-step node replacement, writer lock-then-revalidate-then-retry).
//!
//! Contracts pinned (design `docs/design-cow-kv-metadata.md`):
//! - §4.5 latch-free reads: lookups/scans never take a node lock — proven
//!   by reading while a writer HOLDS the leaf's write lock.
//! - §4.5 eviction: clean nodes evict by dropping the Arc while in-flight
//!   readers keep their snapshots alive by refcount; dirty nodes are
//!   pinned until writeback; interior nodes/roots never evict.
//! - §4.5 single-flight: N racing cold lookups of one node perform exactly
//!   one device load.
//! - §4.6 SMO protocol: writeback = freeze under the lock, append outside
//!   it; full log ⇒ compact; oversized fold ⇒ split (incl. interior
//!   recursion to a deeper root); commit-path writers revalidate under
//!   lock and retry on supersede (`meta_kv_commit_smo_retries`);
//!   `&mut SmoContext` serializes every SMO.
//! - §4.7 composition: old extents pending-free at the SMO, never
//!   claimable before their retiring seq is durable.
//! - Scale: a 1 M-key single-parent range scan streams in key order.
//!
//! The loom models for the node lifecycle word live in `loom-models/`
//! (`tests/run_loom.sh`); the K2 crash cases already attack the node
//! format this layer rides.

use bytes::Bytes;
use squeezefs::meta_backend::kv::node::{key_successor, load_node, NodeLayout, DEFAULT_NODE_SIZE};
use squeezefs::meta_backend::kv::node_cache::{
    LiveLookup, NodeCache, NodeCacheConfig, DEFAULT_CACHE_BUDGET_BYTES,
    DEFAULT_WRITEBACK_DELTA_BYTES,
};
use squeezefs::meta_backend::kv::node_state_core::{LifecycleState, NodeState};
use squeezefs::meta_backend::kv::record::{
    dentry_key, dentry_name_hash54, DentryValue, RecordKind, TREE_DENTRIES, TREE_INODES,
};
use squeezefs::meta_backend::kv::tree::{
    decode_interior_value, encode_interior_value, ApplyOutcome, KvTree, RootPtr, SmoContext,
    KEY_SPACE_MAX,
};
use squeezefs::meta_backend::kv::{
    alloc_ext::ExtentAllocator, KvError, META_KV_COMMIT_SMO_RETRIES, META_KV_NODE_CACHE_EVICTIONS,
    META_KV_NODE_CACHE_HITS, META_KV_NODE_CACHE_MISSES, META_KV_NODE_COMPACTIONS,
    META_KV_NODE_SPLITS,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Harness.
// ---------------------------------------------------------------------------

/// A file-backed volume + cache + allocator + serialized SMO context.
struct Vol {
    _file: NamedTempFile,
    cache: Arc<NodeCache>,
    alloc: Arc<ExtentAllocator>,
    seq: Arc<squeezefs::meta_backend::kv::node_seq::NodeSeqHandle>,
    ctx: SmoContext,
}

impl Vol {
    /// `node_size`-sized heap of `extents` extents at `heap_base`, with a
    /// cache budget of `budget_nodes` nodes and the given writeback
    /// threshold.
    fn new(
        node_size: usize,
        extents: u64,
        heap_base: u64,
        budget_nodes: u64,
        writeback_delta_bytes: usize,
    ) -> Self {
        let file = NamedTempFile::new().expect("temp volume");
        file.as_file()
            .set_len(heap_base + extents * node_size as u64)
            .expect("size volume");
        let layout = NodeLayout::new(node_size).expect("layout");
        let cache = NodeCache::new(NodeCacheConfig {
            path: file.path().to_path_buf(),
            layout,
            heap_base,
            budget_bytes: budget_nodes * node_size as u64,
            writeback_delta_bytes,
        });
        // No compaction reserve pressure in these tests; a roomy
        // pending-free FIFO (advance_durable is driven explicitly).
        let alloc = Arc::new(ExtentAllocator::format(extents, 0, 4096));
        let seq = Arc::new(squeezefs::meta_backend::kv::node_seq::NodeSeqHandle::shared(0));
        let ctx = SmoContext::new(alloc.clone());
        Self {
            _file: file,
            cache,
            alloc,
            seq,
            ctx,
        }
    }

    fn small() -> Self {
        // 64 KiB nodes keep splits reachable in hundreds of inserts.
        Self::new(64 * 1024, 512, 0, 512, DEFAULT_WRITEBACK_DELTA_BYTES)
    }

    async fn tree(&mut self, tree_id: u8) -> KvTree {
        KvTree::create(self.cache.clone(), &mut self.ctx, tree_id, self.seq.clone())
            .await
            .expect("create tree")
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

/// Drive maintenance until quiescent (the K5 stand-in for the K6b
/// checkpoint task's cadence).
async fn drain(tree: &KvTree, ctx: &mut SmoContext) -> (u64, u64, u64) {
    let (mut a, mut c, mut s) = (0, 0, 0);
    while tree.maintenance_pending() {
        let out = tree.run_maintenance(ctx).await.expect("maintenance");
        a += out.appends;
        c += out.compactions;
        s += out.splits;
    }
    (a, c, s)
}

// ---------------------------------------------------------------------------
// Point ops + range basics.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn point_ops_insert_overwrite_delete_reinsert() {
    let mut vol = Vol::small();
    let tree = vol.tree(TREE_INODES).await;

    assert_eq!(
        tree.lookup(&ikey(7)).await.expect("lookup"),
        None,
        "fresh tree has no keys"
    );

    tree.insert(&ikey(7), val(7, 32)).await.expect("insert");
    assert_eq!(
        tree.lookup(&ikey(7)).await.expect("lookup").as_deref(),
        Some(&val(7, 32)[..]),
        "inserted value folds live"
    );

    tree.insert(&ikey(7), val(700, 48))
        .await
        .expect("overwrite");
    assert_eq!(
        tree.lookup(&ikey(7)).await.expect("lookup").as_deref(),
        Some(&val(700, 48)[..]),
        "per-key LWW by seq: the newer Put wins (§4.2)"
    );

    tree.delete(&ikey(7)).await.expect("delete");
    assert_eq!(
        tree.lookup(&ikey(7)).await.expect("lookup"),
        None,
        "tombstone folds to absent"
    );

    tree.insert(&ikey(7), val(7000, 16))
        .await
        .expect("reinsert");
    assert_eq!(
        tree.lookup(&ikey(7)).await.expect("lookup").as_deref(),
        Some(&val(7000, 16)[..]),
        "a Put newer than the tombstone resurrects the key"
    );

    // Keys must be non-empty and below the sentinel.
    assert!(matches!(
        tree.insert(b"", Bytes::from_static(b"x")).await,
        Err(KvError::Corrupt(_))
    ));
    assert!(matches!(
        tree.insert(&KEY_SPACE_MAX, Bytes::from_static(b"x")).await,
        Err(KvError::Corrupt(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn value_cap_enforced_typed() {
    // 64 KiB nodes ⇒ 16 KiB value cap + the xattr envelope allowance
    // (§4.2; fstests generic/020 — the record budget carries the envelope
    // so a full cap-sized user VALUE fits regardless of name length).
    let mut vol = Vol::small();
    let tree = vol.tree(TREE_INODES).await;
    let cap = vol.cache.config().layout.record_value_cap();
    assert_eq!(cap, 16 * 1024 + 256);

    tree.insert(&ikey(1), vec![0u8; cap])
        .await
        .expect("at-cap value fits");
    match tree.insert(&ikey(2), vec![0u8; cap + 1]).await {
        Err(KvError::ValueTooLarge { len, cap: c }) => {
            assert_eq!((len, c), (cap + 1, cap));
        }
        other => panic!("expected ValueTooLarge, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn range_scan_folds_orders_and_resumes() {
    let mut vol = Vol::small();
    let tree = vol.tree(TREE_INODES).await;

    for i in 0..200u64 {
        tree.insert(&ikey(i), val(i, 24)).await.expect("insert");
    }
    // Deletions and overwrites must fold through the scan.
    for i in (0..200u64).step_by(3) {
        tree.delete(&ikey(i)).await.expect("delete");
    }
    tree.insert(&ikey(100), val(9999, 40))
        .await
        .expect("re-put over delete? no: 100 % 3 == 1");

    let all = tree
        .range(&ikey(0), &ikey(199), usize::MAX)
        .await
        .expect("range");
    let expected: Vec<u64> = (0..200u64).filter(|i| i % 3 != 0).collect();
    assert_eq!(all.len(), expected.len(), "tombstones folded out");
    for ((k, v), want) in all.iter().zip(&expected) {
        assert_eq!(&k[..], &ikey(*want)[..], "memcmp key order");
        if *want == 100 {
            assert_eq!(&v[..], &val(9999, 40)[..], "overwrite folded in");
        }
    }

    // Bounded + resumable: chunks of 7 stitch to the same stream.
    let mut resumed: Vec<(Bytes, Bytes)> = Vec::new();
    let mut start = ikey(0);
    loop {
        let chunk = tree.range(&start, &ikey(199), 7).await.expect("chunk");
        if chunk.is_empty() {
            break;
        }
        start = key_successor(&chunk[chunk.len() - 1].0);
        resumed.extend(chunk);
    }
    assert_eq!(resumed.len(), all.len(), "chunked resume covers the stream");
    assert_eq!(
        resumed.iter().map(|(k, _)| k.to_vec()).collect::<Vec<_>>(),
        all.iter().map(|(k, _)| k.to_vec()).collect::<Vec<_>>(),
        "chunk boundaries do not skip or duplicate"
    );

    // Sub-ranges respect both bounds.
    let mid = tree
        .range(&ikey(50), &ikey(60), usize::MAX)
        .await
        .expect("mid");
    let want: Vec<u64> = (50..=60).filter(|i| i % 3 != 0).collect();
    assert_eq!(mid.len(), want.len());
    assert_eq!(&mid[0].0[..], &ikey(want[0])[..]);
}

// ---------------------------------------------------------------------------
// Writeback → compaction → split (§4.6 pt 1) + allocator composition (§4.7).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writeback_appends_then_compacts_then_splits() {
    let mut vol = Vol::small();
    let tree = vol.tree(TREE_INODES).await;
    let root0 = tree.root();
    let compactions0 = META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed);
    let splits0 = META_KV_NODE_SPLITS.load(Ordering::Relaxed);

    // Phase 1: deltas past the threshold append bsets (§4.6 pt 1) — two
    // rounds so the log visibly grows frame by frame.
    for i in 0..200u64 {
        tree.insert(&ikey(i), val(i, 24)).await.expect("insert");
    }
    let (appends_a, _, _) = drain(&tree, &mut vol.ctx).await;
    for i in 200..400u64 {
        tree.insert(&ikey(i), val(i, 24)).await.expect("insert");
    }
    let (appends_b, _, _) = drain(&tree, &mut vol.ctx).await;
    assert!(
        appends_a >= 1 && appends_b >= 1,
        "threshold crossings must append ({appends_a}, {appends_b})"
    );
    let disk = load_node(
        &vol.cache.config().path,
        &vol.cache.config().layout,
        tree.root().addr,
        0,
    )
    .await
    .expect("load root extent");
    assert!(
        disk.bset_count() >= 2,
        "each writeback lands one bset frame on the log (got {})",
        disk.bset_count()
    );
    assert_eq!(tree.root(), root0, "appends never move the node");

    // Phase 2: keep overwriting one key until the 64 KiB log fills ⇒ the
    // node compacts to a fresh extent (CoW, §4.1) and the old extent goes
    // to pending-free, unclaimable until its retiring seq is durable (§4.7).
    let free_before = vol.alloc.free_extents();
    let mut spins = 0u32;
    while META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed) == compactions0 {
        for i in 0..200u64 {
            tree.insert(&ikey(i), val(i * 31, 24)).await.expect("churn");
        }
        drain(&tree, &mut vol.ctx).await;
        spins += 1;
        assert!(spins < 10_000, "compaction never triggered");
    }
    let root1 = tree.root();
    assert_ne!(
        root1.addr, root0.addr,
        "compaction rewrites CoW — never in place"
    );
    assert!(
        vol.alloc.pending_count() >= 1,
        "the old extent must sit in pending-free (§4.7)"
    );
    assert!(
        vol.alloc.free_extents() < free_before,
        "before durability the old extent is NOT reclaimed"
    );
    let released = vol.alloc.advance_durable(vol.seq.load());
    assert!(released >= 1, "durable advance releases the retired extent");

    // Data intact across the rewrite.
    for i in (0..200u64).step_by(17) {
        assert_eq!(
            tree.lookup(&ikey(i)).await.expect("lookup").as_deref(),
            Some(&val(i * 31, 24)[..]),
            "key {i} after compaction"
        );
    }

    // Phase 3: unique keys until the root leaf splits ⇒ a new interior
    // root (level 1) above two+ leaves.
    let mut i = 1_000u64;
    while META_KV_NODE_SPLITS.load(Ordering::Relaxed) == splits0 {
        for _ in 0..64 {
            tree.insert(&ikey(i), val(i, 40)).await.expect("insert");
            i += 1;
        }
        drain(&tree, &mut vol.ctx).await;
        assert!(i < 1_000_000, "split never triggered");
    }
    assert_eq!(
        tree.root_level().await.expect("level"),
        1,
        "root split promotes"
    );
    for probe in [1_000u64, 1_100, i - 1] {
        assert_eq!(
            tree.lookup(&ikey(probe)).await.expect("lookup").as_deref(),
            Some(&val(probe, 40)[..]),
            "key {probe} after split"
        );
    }
    // Old keys still live under the new root.
    assert_eq!(
        tree.lookup(&ikey(3)).await.expect("lookup").as_deref(),
        Some(&val(3 * 31, 24)[..])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interior_recursion_grows_root_to_depth_three() {
    // 64 KiB nodes + 4 KiB values ⇒ ~14 records/leaf ⇒ interior separators
    // accumulate fast enough that the level-1 root itself splits.
    let mut vol = Vol::new(64 * 1024, 4096, 0, 4096, DEFAULT_WRITEBACK_DELTA_BYTES);
    let tree = vol.tree(TREE_INODES).await;

    let mut i = 0u64;
    while tree.root_level().await.expect("level") < 2 {
        tree.insert(&ikey(i), val(i, 4096)).await.expect("insert");
        i += 1;
        if i.is_multiple_of(8) {
            drain(&tree, &mut vol.ctx).await;
        }
        assert!(
            i < 60_000,
            "depth-3 never reached (interior recursion broken)"
        );
    }
    drain(&tree, &mut vol.ctx).await;
    assert_eq!(tree.root_level().await.expect("level"), 2);

    // Every key must still resolve through two interior levels.
    for probe in (0..i).step_by((i as usize / 97).max(1)) {
        assert_eq!(
            tree.lookup(&ikey(probe)).await.expect("lookup").as_deref(),
            Some(&val(probe, 4096)[..]),
            "key {probe} after interior recursion"
        );
    }
    // And stream in order across the whole depth-3 tree.
    let all = tree
        .range(&ikey(0), &ikey(i), usize::MAX)
        .await
        .expect("range");
    assert_eq!(all.len() as u64, i, "no key lost or duplicated");
    for (n, (k, _)) in all.iter().enumerate() {
        assert_eq!(&k[..], &ikey(n as u64)[..], "order across leaves");
    }
}

// ---------------------------------------------------------------------------
// §4.6 writer lock-then-revalidate-then-retry.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_writer_revalidates_and_retries_after_smo() {
    let mut vol = Vol::small();
    let tree = vol.tree(TREE_INODES).await;
    for i in 0..200u64 {
        tree.insert(&ikey(i), val(i, 24)).await.expect("seed");
    }
    drain(&tree, &mut vol.ctx).await;

    // The unrolled commit protocol: resolve FIRST (latch-free)…
    let stale_leaf = tree.resolve_leaf(&ikey(50)).await.expect("resolve");

    // …then an SMO replaces the leaf before the writer locks: churn one
    // key until the log fills and the node compacts (deterministic — the
    // SMO runs on this task; &mut SmoContext is the serialization proof).
    let c0 = META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + META_KV_NODE_SPLITS.load(Ordering::Relaxed);
    let mut spins = 0u32;
    while META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed)
        + META_KV_NODE_SPLITS.load(Ordering::Relaxed)
        == c0
    {
        for i in 0..200u64 {
            tree.insert(&ikey(i), val(i * 7 + spins as u64, 24))
                .await
                .expect("churn");
        }
        drain(&tree, &mut vol.ctx).await;
        spins += 1;
        assert!(spins < 10_000, "SMO never triggered");
    }
    assert_eq!(
        stale_leaf.state().state(),
        LifecycleState::Superseded,
        "the resolved leaf was replaced"
    );

    // Lock-then-revalidate on the stale object: Stale + counted (§4.6).
    let retries0 = META_KV_COMMIT_SMO_RETRIES.load(Ordering::Relaxed);
    let outcome = tree
        .apply_at(
            &stale_leaf,
            &ikey(50),
            RecordKind::Put,
            Bytes::from(val(4242, 24)),
        )
        .await
        .expect("apply_at");
    assert_eq!(
        outcome,
        ApplyOutcome::Stale,
        "revalidation must fail on superseded"
    );
    assert_eq!(
        META_KV_COMMIT_SMO_RETRIES.load(Ordering::Relaxed),
        retries0 + 1,
        "meta_kv_commit_smo_retries counts the revalidation failure"
    );

    // The full writer loop (re-resolve → retry) succeeds.
    tree.insert(&ikey(50), val(4242, 24))
        .await
        .expect("retried insert");
    assert_eq!(
        tree.lookup(&ikey(50)).await.expect("lookup").as_deref(),
        Some(&val(4242, 24)[..])
    );
}

// ---------------------------------------------------------------------------
// Multi-threaded split-vs-commit storm (§4.6; design K5 test list).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn split_vs_commit_storm_loses_nothing() {
    const WRITERS: u64 = 8;
    const PER_WRITER: u64 = 3_000;

    let mut vol = Vol::new(64 * 1024, 4096, 0, 4096, 2048);
    let tree = Arc::new(vol.tree(TREE_DENTRIES).await);
    let splits0 = META_KV_NODE_SPLITS.load(Ordering::Relaxed);

    // One serialized SMO context on its own task — the §4.6 "checkpoint
    // task" role, driven concurrently with the writers. Coordination via
    // a watch channel; no sleeps.
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    let smo_tree = tree.clone();
    let mut smo_ctx = SmoContext::new(vol.alloc.clone());
    let smo = tokio::spawn(async move {
        let mut splits = 0u64;
        loop {
            let out = smo_tree
                .run_maintenance(&mut smo_ctx)
                .await
                .expect("maintenance under storm");
            splits += out.splits;
            if *done_rx.borrow() && !smo_tree.maintenance_pending() {
                return splits;
            }
            if out.appends + out.compactions + out.splits == 0 {
                // Queue momentarily empty: wait for either new work or done.
                if done_rx.changed().await.is_err() {
                    return splits;
                }
            }
        }
    });

    let mut writers = tokio::task::JoinSet::new();
    for w in 0..WRITERS {
        let tree = tree.clone();
        writers.spawn(async move {
            for i in 0..PER_WRITER {
                let k = ikey(w * PER_WRITER + i);
                tree.insert(&k, val(w * PER_WRITER + i, 48))
                    .await
                    .expect("storm insert");
                if i % 7 == 0 {
                    // Interleave latch-free reads with the storm.
                    let got = tree.lookup(&k).await.expect("storm lookup");
                    assert_eq!(got.as_deref(), Some(&val(w * PER_WRITER + i, 48)[..]));
                }
            }
        });
    }
    while let Some(r) = writers.join_next().await {
        r.expect("writer task");
    }
    done_tx.send(true).expect("signal done");
    let _smo_splits = smo.await.expect("smo task");

    // Every key present exactly once, in order, via a full range walk.
    let total = WRITERS * PER_WRITER;
    let all = tree
        .range(&ikey(0), &ikey(total), usize::MAX)
        .await
        .expect("full scan");
    assert_eq!(
        all.len() as u64,
        total,
        "no record lost or duplicated under the storm"
    );
    for (n, (k, v)) in all.iter().enumerate() {
        assert_eq!(&k[..], &ikey(n as u64)[..]);
        assert_eq!(&v[..], &val(n as u64, 48)[..]);
    }
    assert!(
        META_KV_NODE_SPLITS.load(Ordering::Relaxed) > splits0,
        "the storm must split (lock-then-revalidate-then-retry was exercised)"
    );
    // Retries are opportunistic (the resolve→lock window is narrow); the
    // deterministic revalidation test above pins the counter's semantics.
}

// ---------------------------------------------------------------------------
// 1M-key single-parent range scan (design K5 test list; §3 scale target).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn million_key_single_parent_range_scan() {
    // Default 256 KiB nodes — the production shape. 1 M dentry records
    // under ONE parent ino, inserted in name order, scanned in hash-key
    // order (§4.2 dentry keys; §5.1 readdir shape). Runtime is gated by
    // the serial-test budget: if this exceeds ~60 s on the gate box, scale
    // KEYS down and note it (task instruction).
    const KEYS: u64 = 1_000_000;
    const PARENT: u64 = 1;
    const HASH_SEED: u64 = 0x5EED_F00D;

    let mut vol = Vol::new(
        DEFAULT_NODE_SIZE,
        4096,
        0,
        // Whole working set cacheable: ~1 M × ~60 B ≈ 60 MiB ≈ 400 nodes.
        2048,
        DEFAULT_WRITEBACK_DELTA_BYTES,
    );
    let tree = vol.tree(TREE_DENTRIES).await;

    let mut expected: Vec<[u8; 16]> = Vec::with_capacity(KEYS as usize);
    for i in 0..KEYS {
        let name = format!("entry-{i:07}");
        let hash54 = dentry_name_hash54(name.as_bytes(), HASH_SEED);
        let key = dentry_key(PARENT, hash54, 0);
        // Collisions at 1 M keys are ~2.8e-5 expected (§4.2) — skip the
        // astronomically rare duplicate rather than model chains here.
        let value = DentryValue {
            child_ino: 2 + i,
            file_type: 8,
            name: name.into_bytes(),
        }
        .encode()
        .expect("encode dentry");
        tree.insert(&key, value).await.expect("insert dentry");
        expected.push(key);
        if i % 4096 == 0 {
            drain(&tree, &mut vol.ctx).await;
        }
    }
    drain(&tree, &mut vol.ctx).await;
    expected.sort_unstable();
    expected.dedup();

    // Stream the whole parent in bounded chunks (readdir shape).
    let lo = dentry_key(PARENT, 0, 0);
    let hi = dentry_key(PARENT, (1 << 54) - 1, 255);
    let mut got = 0u64;
    let mut cursor = lo.to_vec();
    let mut prev: Option<Bytes> = None;
    loop {
        let chunk = tree.range(&cursor, &hi, 65_536).await.expect("range chunk");
        if chunk.is_empty() {
            break;
        }
        for (k, v) in &chunk {
            if let Some(p) = &prev {
                assert!(p[..] < k[..], "strict memcmp order across chunks");
            }
            assert_eq!(
                &k[..],
                &expected[got as usize][..],
                "hash-key order matches"
            );
            let d = DentryValue::decode(v).expect("decode dentry");
            assert!(d.child_ino >= 2);
            prev = Some(k.clone());
            got += 1;
        }
        cursor = key_successor(&chunk[chunk.len() - 1].0);
    }
    assert_eq!(
        got as usize,
        expected.len(),
        "every dentry streamed exactly once"
    );
    assert!(
        tree.root_level().await.expect("level") >= 1,
        "1 M keys must have split into a multi-leaf tree"
    );

    // Point-probe a sample.
    for i in (0..KEYS).step_by(99_991) {
        let name = format!("entry-{i:07}");
        let key = dentry_key(PARENT, dentry_name_hash54(name.as_bytes(), HASH_SEED), 0);
        let v = tree.lookup(&key).await.expect("lookup").expect("present");
        assert_eq!(
            DentryValue::decode(&v).expect("decode").name,
            name.into_bytes()
        );
    }
}

// ---------------------------------------------------------------------------
// Eviction (§4.5): budget, dirty pinning, pinned interior, held snapshots.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_respects_budget_pins_dirty_and_interior_and_held_snapshots_survive() {
    // Budget of 4 nodes over a tree that needs many more.
    let mut vol = Vol::new(64 * 1024, 4096, 0, 4, DEFAULT_WRITEBACK_DELTA_BYTES);
    let tree = vol.tree(TREE_INODES).await;

    let mut i = 0u64;
    while tree.root_level().await.expect("level") < 1 || i < 4_000 {
        tree.insert(&ikey(i), val(i, 40)).await.expect("insert");
        i += 1;
        if i.is_multiple_of(64) {
            drain(&tree, &mut vol.ctx).await;
        }
    }
    drain(&tree, &mut vol.ctx).await;

    // Hold a value and its leaf snapshot, then force eviction pressure.
    let held_key = ikey(17);
    let held_val = tree
        .lookup(&held_key)
        .await
        .expect("lookup")
        .expect("present");
    let held_leaf = tree.resolve_leaf(&held_key).await.expect("resolve");
    let held_snap = held_leaf.snapshot();

    let ev0 = META_KV_NODE_CACHE_EVICTIONS.load(Ordering::Relaxed);
    for probe in (0..i).step_by(13) {
        let _ = tree.lookup(&ikey(probe)).await.expect("pressure lookup");
    }
    assert!(
        META_KV_NODE_CACHE_EVICTIONS.load(Ordering::Relaxed) > ev0,
        "a 4-node budget under a multi-leaf walk must evict"
    );
    assert!(
        vol.cache.cached_bytes() <= 6 * 64 * 1024,
        "cache stays near budget (transient overshoot bounded): {} bytes",
        vol.cache.cached_bytes()
    );

    // The held snapshot keeps serving its data by refcount, evicted or not.
    assert_eq!(
        match held_snap.lookup(&held_key).expect("snapshot lookup") {
            LiveLookup::Live(v) => v,
            other => panic!("held snapshot lost the key: {other:?}"),
        },
        held_val,
        "eviction never invalidates in-flight readers (§4.5)"
    );
    assert_eq!(&held_val[..8], &17u64.to_le_bytes()[..], "bytes intact");

    // Re-demand-page and compare.
    assert_eq!(
        tree.lookup(&held_key).await.expect("re-lookup").as_deref(),
        Some(&held_val[..])
    );

    // The root (interior) is pinned: still mapped despite the pressure.
    assert!(
        vol.cache.contains(tree.root().addr),
        "interior/root nodes never evict (§4.5)"
    );

    // Dirty pinning: make one leaf dirty and sweep — it must survive.
    tree.insert(&ikey(17), val(999_999, 40))
        .await
        .expect("dirty");
    let dirty_leaf = tree.resolve_leaf(&ikey(17)).await.expect("resolve dirty");
    assert!(dirty_leaf.state().is_dirty());
    for probe in (0..i).step_by(11) {
        let _ = tree.lookup(&ikey(probe)).await.expect("more pressure");
    }
    assert!(
        vol.cache.contains(dirty_leaf.addr()),
        "dirty nodes are pinned until writeback (§4.5)"
    );
    assert_eq!(
        tree.lookup(&ikey(17)).await.expect("lookup").as_deref(),
        Some(&val(999_999, 40)[..]),
        "the dirty record survived the sweep"
    );
}

// ---------------------------------------------------------------------------
// Demand paging + single-flight (§4.5).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn single_flight_collapses_racing_cold_loads() {
    let mut vol = Vol::small();
    let root;
    {
        let tree = vol.tree(TREE_INODES).await;
        // Grow past a root split so leaves are distinct from the (pinned,
        // open()-loaded) interior root — the racing lookups below must hit
        // a genuinely cold leaf.
        let mut i = 0u64;
        while tree.root_level().await.expect("level") < 1 {
            tree.insert(&ikey(i), val(i, 32)).await.expect("insert");
            i += 1;
            if i.is_multiple_of(64) {
                drain(&tree, &mut vol.ctx).await;
            }
            assert!(i < 100_000, "split never triggered");
        }
        tree.flush_dirty(&mut vol.ctx).await.expect("flush");
        root = tree.root();
    }

    // A cold cache over the same volume file.
    let cold_cache = NodeCache::new(NodeCacheConfig {
        path: vol.cache.config().path.clone(),
        layout: vol.cache.config().layout,
        heap_base: 0,
        budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    let tree = Arc::new(
        KvTree::open(
            cold_cache.clone(),
            TREE_INODES,
            root,
            Arc::new(squeezefs::meta_backend::kv::node_seq::NodeSeqHandle::shared(1 << 32)),
        )
        .await
        .expect("open"),
    );

    let hits0 = META_KV_NODE_CACHE_HITS.load(Ordering::Relaxed);
    let misses0 = META_KV_NODE_CACHE_MISSES.load(Ordering::Relaxed);

    // 32 racing lookups of the same key: barrier-released together.
    let barrier = Arc::new(tokio::sync::Barrier::new(32));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let tree = tree.clone();
        let barrier = barrier.clone();
        set.spawn(async move {
            barrier.wait().await;
            tree.lookup(&ikey(123)).await.expect("cold lookup")
        });
    }
    while let Some(r) = set.join_next().await {
        assert_eq!(
            r.expect("task").as_deref(),
            Some(&val(123, 32)[..]),
            "every racer sees the value"
        );
    }

    let loads = META_KV_NODE_CACHE_MISSES.load(Ordering::Relaxed) - misses0;
    let hits = META_KV_NODE_CACHE_HITS.load(Ordering::Relaxed) - hits0;
    // The tree is depth ≤ 1 here: root was pre-loaded by open(); the leaf
    // is the only cold node — 32 racers, exactly one device load.
    assert_eq!(loads, 1, "single-flight: racing cold loads collapse to one");
    assert!(
        hits >= 31,
        "the other racers are hits after waiting on the loader (got {hits})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_from_disk_after_flush_serves_everything() {
    let mut vol = Vol::small();
    let root;
    let n = 800u64;
    {
        let tree = vol.tree(TREE_INODES).await;
        for i in 0..n {
            tree.insert(&ikey(i), val(i, 48)).await.expect("insert");
        }
        for i in (0..n).step_by(5) {
            tree.delete(&ikey(i)).await.expect("delete");
        }
        tree.flush_dirty(&mut vol.ctx).await.expect("flush");
        root = tree.root();
    }

    let cache = NodeCache::new(NodeCacheConfig {
        path: vol.cache.config().path.clone(),
        layout: vol.cache.config().layout,
        heap_base: 0,
        budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    let tree = KvTree::open(
        cache,
        TREE_INODES,
        root,
        Arc::new(squeezefs::meta_backend::kv::node_seq::NodeSeqHandle::shared(1 << 32)),
    )
    .await
    .expect("open");

    for i in 0..n {
        let got = tree.lookup(&ikey(i)).await.expect("lookup");
        if i % 5 == 0 {
            assert_eq!(got, None, "tombstone {i} durable via writeback");
        } else {
            assert_eq!(got.as_deref(), Some(&val(i, 48)[..]), "key {i} durable");
        }
    }
    let all = tree
        .range(&ikey(0), &ikey(n), usize::MAX)
        .await
        .expect("range");
    assert_eq!(
        all.len() as u64,
        n - n.div_ceil(5),
        "folded scan after reopen"
    );

    // A bad root pointer fails loud.
    let cache2 = NodeCache::new(NodeCacheConfig {
        path: vol.cache.config().path.clone(),
        layout: vol.cache.config().layout,
        heap_base: 0,
        budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    assert!(
        KvTree::open(
            cache2,
            TREE_INODES,
            RootPtr {
                addr: root.addr,
                seq: root.seq + 1
            },
            Arc::new(squeezefs::meta_backend::kv::node_seq::NodeSeqHandle::shared(1 << 32)),
        )
        .await
        .is_err(),
        "stale ledger root_seq must be refused"
    );
}

// ---------------------------------------------------------------------------
// Latch-free reads (§4.5): a held write lock never blocks readers.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_proceed_while_writer_holds_the_node_lock() {
    let mut vol = Vol::small();
    let tree = Arc::new(vol.tree(TREE_INODES).await);
    for i in 0..300u64 {
        tree.insert(&ikey(i), val(i, 32)).await.expect("insert");
    }
    drain(&tree, &mut vol.ctx).await;

    let leaf = tree.resolve_leaf(&ikey(150)).await.expect("resolve");
    let guard = leaf.lock().write().await; // a writer mid-apply

    // Point lookup, snapshot fold, and a full range scan must all complete
    // while the write lock is HELD — reads are lock-free (arc-swap
    // snapshots), so a timeout here means a lock crept onto the read path.
    let read = async {
        assert_eq!(
            tree.lookup(&ikey(150)).await.expect("lookup").as_deref(),
            Some(&val(150, 32)[..])
        );
        let all = tree
            .range(&ikey(0), &ikey(299), usize::MAX)
            .await
            .expect("range");
        assert_eq!(all.len(), 300);
        match leaf.snapshot().lookup(&ikey(150)).expect("snapshot fold") {
            LiveLookup::Live(v) => assert_eq!(&v[..], &val(150, 32)[..]),
            other => panic!("expected live, got {other:?}"),
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), read)
        .await
        .expect("reads blocked behind a node write lock — the §4.5 latch-free contract is broken");
    drop(guard);
}

// ---------------------------------------------------------------------------
// Interior value encoding (§4.2) — the traversal's pointer format.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interior_value_roundtrip_and_bounds() {
    let v = encode_interior_value(0x1234_5678_9ABC_DEF0, 42);
    assert_eq!(v.len(), 16);
    assert_eq!(
        decode_interior_value(&v).expect("decode"),
        (0x1234_5678_9ABC_DEF0, 42)
    );
    assert!(
        decode_interior_value(&v[..15]).is_err(),
        "short value refused"
    );
    assert!(
        decode_interior_value(&[0u8; 17]).is_err(),
        "oversized value refused"
    );
}

// ---------------------------------------------------------------------------
// node_state_core sanity under std atomics (the exhaustive interleaving
// proofs live in loom-models/; this pins the single-threaded semantics).
// ---------------------------------------------------------------------------

#[test]
fn node_lifecycle_word_transitions() {
    let s = NodeState::new();
    assert_eq!(s.state(), LifecycleState::Clean);

    s.mark_dirty().expect("clean → dirty");
    assert_eq!(s.state(), LifecycleState::Dirty);

    s.begin_freeze().expect("dirty → serializing");
    assert_eq!(s.state(), LifecycleState::Serializing);
    assert!(!s.is_dirty(), "freeze swaps the delta out");
    assert!(s.begin_freeze().is_err(), "one freeze at a time");

    s.mark_dirty().expect("apply during serialize re-dirties");
    assert_eq!(s.state(), LifecycleState::Serializing, "freezing dominates");
    assert!(s.end_freeze(), "end_freeze reports re-accumulated dirt");
    assert_eq!(s.state(), LifecycleState::Dirty);

    // Freeze-then-abort restores dirt.
    s.begin_freeze().expect("freeze again");
    s.abort_freeze();
    assert_eq!(s.state(), LifecycleState::Dirty, "abort restores the delta");

    // try_evict refuses anything but clean.
    assert!(!s.try_evict(), "dirty nodes are pinned (§4.5)");
    s.begin_freeze().expect("freeze");
    assert!(!s.try_evict(), "serializing nodes are pinned");
    assert!(!s.end_freeze(), "no dirt re-accumulated this time");
    assert_eq!(s.state(), LifecycleState::Clean);

    // Supersede is terminal; applies are refused after it.
    let out = s.supersede().expect("clean supersede");
    assert!(!out.was_dirty && !out.was_freezing);
    assert!(s.supersede().is_err(), "double supersede refused");
    assert!(s.mark_dirty().is_err(), "apply after supersede refused");
    assert_eq!(s.state(), LifecycleState::Superseded);

    // Eviction is the same terminal transition, gated on clean.
    let e = NodeState::new();
    assert!(e.try_evict(), "clean nodes evict");
    assert!(
        e.mark_dirty().is_err(),
        "the evicted object never accepts dirt"
    );

    // Supersede reports displaced dirt for the successor build (§4.6).
    let d = NodeState::new();
    d.mark_dirty().expect("dirty");
    let out = d.supersede().expect("supersede");
    assert!(out.was_dirty, "the SMO must carry the open delta");
}
