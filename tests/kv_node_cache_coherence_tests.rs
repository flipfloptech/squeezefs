//! **KV node-cache coherence** — the runtime item pre-RC engineering spec
//! §6.2 calls *"arguably harder than any of the ten"* durable-format
//! assumptions: the node cache is load-once RAM-authoritative, with no
//! revalidation path, because in a single-writer design there cannot be
//! another appender.
//!
//! Two halves, exactly as the spec splits them:
//!
//! | Half | Spec | What this file pins |
//! |---|---|---|
//! | **Reader** (§6.8 item 2 — the S5 prerequisite) | poll the A/B root ledger at a bounded cadence and drop every cached node not covered by the new roots | arming, epoch advance, the drop pass, the lazy hit-path gate, tail adoption, root adoption, the R-6 purge trigger (§6.8 item 5), the derived cadence and the stated staleness bound |
//! | **Writer** (§6.2 closing — *"partitioning, not cache coherence"*) | two writers must never cache the same node | the interior-node population has ONE cacher (the root authority, by lock order 4b), enforced loud at the ONE RAM-mutation choke point; a foreign append into a node's log is detected instead of overwritten; an armed reader may mutate nothing |
//!
//! **What is deliberately NOT here**: cross-node runtime coherence. The
//! spec's verdict is partitioning, and a coherence protocol would be the
//! wrong answer (it would also need the §6.8 item-3 freed-offset grace
//! period, which is weeks of work and not this item).
//!
//! Harness note: `KvMetaBackend::open` takes `flock(LOCK_EX)`
//! unconditionally (§6.4 — a real RO mount mode is the sibling item), so
//! the "one writer + one reader" shape here is a writer backend plus a
//! **hand-built reader** — `NodeCache` + `KvTree::open` off the same
//! device, which is exactly the surface an RO mount wires.

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::checkpoint::read_newest_ledger;
use squeezefs::meta_backend::kv::node::{
    write_node, NodeLayout, NodeWriteParams, DEFAULT_NODE_SIZE, MIN_NODE_SIZE,
};
use squeezefs::meta_backend::kv::node_cache::{
    EpochPurgeSink, NodeCache, NodeCacheConfig, RootEpoch, DEFAULT_WRITEBACK_DELTA_BYTES,
};
use squeezefs::meta_backend::kv::record::{inode_key, InodeValue, Record, TREE_INODES};
use squeezefs::meta_backend::kv::superblock::{classify_volume, VolumeFormat};
use squeezefs::meta_backend::kv::{
    META_KV_NODE_PARTITION_REFUSALS, META_KV_REVALIDATE_DIRTY_SKIPS,
};
use squeezefs::meta_backend::kv::revalidate::{
    resolve_revalidate_interval_ms, revalidate_trees, RevalidationPoller, REVALIDATE_INTERVAL_ENV,
};
use squeezefs::meta_backend::kv::tree::{KvTree, RootPtr, SmoContext};
use squeezefs::meta_backend::Metadata;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

const VOL: u64 = 64 * 1024 * 1024;

/// A formatted v3 volume plus its single write mount (the appender).
async fn writer() -> (NamedTempFile, Arc<KvMetaBackend>) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL).expect("size volume");
    format_v3(
        file.path(),
        VOL,
        &FormatV3Options {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let be = KvMetaBackend::open(file.path()).await.expect("mount v3");
    (file, be)
}

/// A hand-built reader over the same device: its own node cache and its
/// own `KvTree` handles, opened from the newest ledger record. This is the
/// surface the RO-mount wiring consumes.
async fn reader(path: &std::path::Path) -> (Arc<NodeCache>, Vec<KvTree>, RootEpoch) {
    let sb = match classify_volume(path).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 volume, got {other:?}"),
    };
    let rec = read_newest_ledger(path, sb.root_ledger.start)
        .await
        .expect("read ledger")
        .expect("a formatted volume has a bootstrap record");
    let epoch = RootEpoch::from_ledger(&rec);
    let cache = NodeCache::new(NodeCacheConfig {
        path: path.to_path_buf(),
        layout: NodeLayout::new(sb.node_size as usize).expect("layout"),
        heap_base: sb.heap.start,
        budget_bytes: 64 * 1024 * 1024,
        writeback_delta_bytes: DEFAULT_WRITEBACK_DELTA_BYTES,
    });
    cache
        .arm_revalidation(&epoch, None)
        .expect("a fresh cache arms");
    let seq = Arc::new(AtomicU64::new(rec.seq.max(rec.node_seq_watermark)));
    let mut trees = Vec::new();
    for tree_id in [
        squeezefs::meta_backend::kv::record::TREE_INODES,
        squeezefs::meta_backend::kv::record::TREE_DENTRIES,
        squeezefs::meta_backend::kv::record::TREE_XATTRS,
    ] {
        let root = epoch.root_of(tree_id).expect("every tree names a root");
        trees.push(
            KvTree::open(
                cache.clone(),
                tree_id,
                RootPtr {
                    addr: root.node_addr,
                    seq: root.node_seq,
                },
                seq.clone(),
            )
            .await
            .expect("open reader tree"),
        );
    }
    (cache, trees, epoch)
}

/// Re-read the newest ledger record as a fresh epoch (one 128 KiB read —
/// the §6.8 item-2 poll).
async fn poll_epoch(path: &std::path::Path) -> RootEpoch {
    let sb = match classify_volume(path).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 volume, got {other:?}"),
    };
    let rec = read_newest_ledger(path, sb.root_ledger.start)
        .await
        .expect("read ledger")
        .expect("record");
    RootEpoch::from_ledger(&rec)
}

fn tree_of<'a>(trees: &'a [KvTree], tree_id: u8) -> &'a KvTree {
    trees
        .iter()
        .find(|t| t.tree_id() == tree_id)
        .expect("tree opened")
}

/// A standalone cache over a scratch volume with `n` hand-written node
/// images loaded (no tree — the cache-level contracts do not need one).
async fn cache_with_nodes(n: u64, level: u8) -> (NamedTempFile, Arc<NodeCache>, Vec<u64>) {
    let file = NamedTempFile::new().expect("temp volume");
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
                tree_id: TREE_INODES,
                level,
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

fn inode_value(seed: u64) -> InodeValue {
    InodeValue {
        mode: 0o100644,
        nlink: 1,
        size: seed,
        mtime: seed,
        ctime: seed,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// 1. The un-armed posture: byte-identical, cost-neutral, inert.
// ---------------------------------------------------------------------------

/// A cache nobody armed is the shipped single-writer cache: epoch 0, not
/// revalidating, and a stray `revalidate` call drops nothing. This is the
/// cost-neutrality contract — a write mount can never lose cached state to
/// machinery it never opted into.
#[tokio::test]
async fn an_unarmed_cache_never_revalidates_and_never_drops() {
    let (_f, cache, addrs) = cache_with_nodes(4, 0).await;
    assert_eq!(cache.revalidation_epoch(), 0, "unarmed reads as epoch 0");
    assert!(!cache.is_revalidating());
    let before = cache.cached_bytes();

    // An epoch from a far-future ledger record: an un-armed cache ignores
    // it entirely (no advance, no drop, no credit).
    let out = cache.revalidate(&RootEpoch::synthetic(9_999, 4_242, &[]));
    assert!(!out.advanced, "an un-armed cache never advances");
    assert_eq!(out.dropped, 0);
    assert_eq!(cache.cached_bytes(), before, "and never credits a byte");
    for a in &addrs {
        assert!(cache.contains(*a), "every mapping survives");
    }
}

// ---------------------------------------------------------------------------
// 2. Arming, the inert poll, and the drop pass (§6.8 item 2).
// ---------------------------------------------------------------------------

/// Arming seeds the epoch and the durable tail from the mounted record and
/// drops nothing: the cache the reader already built is current as of the
/// record it opened.
#[tokio::test]
async fn arming_seeds_the_epoch_and_the_tail_and_drops_nothing() {
    let (_f, be) = writer().await;
    let path = be.device_path().to_path_buf();
    be.create(1, "seed", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    let (cache, trees, epoch) = reader(&path).await;

    assert_eq!(cache.revalidation_epoch(), epoch.ledger_seq);
    assert!(cache.is_revalidating());
    assert_eq!(cache.durable_tail(), epoch.journal_tail_seq);
    assert!(
        cache.cached_bytes() > 0,
        "opening three trees mapped their roots"
    );
    // The reader can read what the writer checkpointed.
    let inodes = tree_of(&trees, TREE_INODES);
    let root_ino = inodes.lookup(&inode_key(1)).await.expect("lookup ino 1");
    assert!(root_ino.is_some(), "the reader serves the root inode");
    be.shutdown().await.expect("shutdown");
}

/// A poll that finds the same ledger record is **inert**: no advance, no
/// drop, no credit, and the very same `Arc`s stay mapped. This is what
/// makes the derived cadence free on an idle writer (a checkpoint cycle
/// only runs when there is work — `checkpoint.rs::tick`).
#[tokio::test]
async fn an_unchanged_ledger_poll_is_inert() {
    let (_f, be) = writer().await;
    let path = be.device_path().to_path_buf();
    be.create(1, "seed", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    let (cache, trees, _) = reader(&path).await;
    let root_addr = tree_of(&trees, TREE_INODES).root().addr;
    let held = cache.try_get(root_addr).expect("root is mapped");
    let bytes = cache.cached_bytes();

    for _ in 0..2 {
        let fresh = poll_epoch(&path).await;
        let out = revalidate_trees(&cache, &trees.iter().collect::<Vec<_>>(), &fresh);
        assert!(!out.advanced, "the same record must not advance the epoch");
        assert_eq!(out.dropped, 0);
        assert_eq!(out.bytes_credited, 0);
    }
    assert_eq!(cache.cached_bytes(), bytes, "the charge never moved");
    let still = cache.try_get(root_addr).expect("still mapped");
    assert!(
        Arc::ptr_eq(&held, &still),
        "an inert poll must not even replace the cached object"
    );
    be.shutdown().await.expect("shutdown");
}

/// The drop pass: when the writer's ledger advances, EVERY stale-stamped
/// node leaves the map — **including pinned roots and interior nodes**,
/// which are exactly the ones a reader must not keep (they are pinned
/// against *eviction*, not against staleness).
#[tokio::test]
async fn an_advanced_epoch_drops_every_stale_node_including_pinned_roots() {
    let (_f, be) = writer().await;
    let path = be.device_path().to_path_buf();
    be.create(1, "a", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    let (cache, trees, _) = reader(&path).await;
    let refs: Vec<&KvTree> = trees.iter().collect();
    // Touch every tree so all three roots are mapped and pinned.
    for t in &trees {
        t.lookup(&inode_key(1)).await.expect("lookup");
    }
    let mapped: Vec<u64> = trees.iter().map(|t| t.root().addr).collect();
    let charged = cache.cached_bytes();
    assert!(charged > 0);

    // The writer moves on and checkpoints.
    be.create(1, "b", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");

    let fresh = poll_epoch(&path).await;
    let out = revalidate_trees(&cache, &refs, &fresh);
    assert!(out.advanced, "a newer ledger record advances the epoch");
    assert!(out.dropped >= mapped.len() as u64, "every node dropped");
    assert_eq!(
        out.bytes_credited,
        out.dropped * cache.config().layout.node_size() as u64,
        "each dropped mapping credits exactly one extent"
    );
    assert_eq!(
        cache.cached_bytes(),
        0,
        "a full drop pass returns the budget gauge to zero"
    );
    for a in &mapped {
        assert!(
            !cache.contains(*a),
            "pinned root {a:#x} must NOT survive an epoch advance"
        );
    }
    be.shutdown().await.expect("shutdown");
}

/// The soundness argument, pinned so a future "optimization" cannot break
/// it: a node the NEW record names as a root with a byte-identical
/// `(node_addr, node_seq)` is **still dropped**. Root identity is not a
/// currency proof — a leaf (or root-leaf) append leaves both unchanged
/// while the extent's log grows, so the only nodes provably current after
/// an advance are the ones loaded under the new epoch.
#[tokio::test]
async fn a_root_named_identically_by_the_new_record_is_still_dropped() {
    let (_f, be) = writer().await;
    let path = be.device_path().to_path_buf();
    be.create(1, "a", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    let (cache, trees, epoch0) = reader(&path).await;
    let refs: Vec<&KvTree> = trees.iter().collect();
    let inodes = tree_of(&trees, TREE_INODES);
    inodes.lookup(&inode_key(1)).await.expect("lookup");
    let root0 = inodes.root();

    // A plain record append: no SMO, so the root pointer cannot move.
    be.create(1, "b", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    let fresh = poll_epoch(&path).await;
    let root1 = fresh.root_of(TREE_INODES).expect("root named");
    assert_eq!(
        (root1.node_addr, root1.node_seq),
        (root0.addr, root0.seq),
        "fixture precondition: a record-only checkpoint leaves root identity IDENTICAL"
    );
    assert!(fresh.ledger_seq > epoch0.ledger_seq, "the record advanced");

    let out = revalidate_trees(&cache, &refs, &fresh);
    assert!(out.advanced);
    assert!(
        !cache.contains(root0.addr),
        "identical root identity is NOT a currency proof — the node must drop"
    );
    be.shutdown().await.expect("shutdown");
}

/// **The consistency model, as a test.** A reader serves the state of the
/// checkpoint it last polled: the writer's newer create is invisible
/// before revalidation and visible after it. Nothing in between, and never
/// backwards.
#[tokio::test]
async fn the_reader_lags_by_exactly_one_polled_checkpoint() {
    let (_f, be) = writer().await;
    let path = be.device_path().to_path_buf();
    be.create(1, "before", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    let (cache, trees, _) = reader(&path).await;
    let refs: Vec<&KvTree> = trees.iter().collect();
    let dentries = tree_of(&trees, squeezefs::meta_backend::kv::record::TREE_DENTRIES);
    let name_key = |n: &str| {
        squeezefs::meta_backend::kv::record::dentry_key(
            1,
            squeezefs::meta_backend::kv::record::dentry_name_hash54(n),
            0,
        )
    };
    assert!(
        dentries
            .lookup(&name_key("before"))
            .await
            .expect("lookup")
            .is_some(),
        "the polled checkpoint's dentry is visible"
    );

    be.create(1, "after", 0o644, 0, 0).await.expect("create");
    be.checkpoint_now().await.expect("checkpoint");
    assert!(
        dentries
            .lookup(&name_key("after"))
            .await
            .expect("lookup")
            .is_none(),
        "a checkpoint the reader has not polled must NOT be visible \
         (bounded staleness is the model; peeking would be unbounded incoherence)"
    );

    let fresh = poll_epoch(&path).await;
    assert!(revalidate_trees(&cache, &refs, &fresh).advanced);
    assert!(
        dentries
            .lookup(&name_key("after"))
            .await
            .expect("lookup")
            .is_some(),
        "after the poll the reader serves the newer checkpoint"
    );
    assert!(
        dentries
            .lookup(&name_key("before"))
            .await
            .expect("lookup")
            .is_some(),
        "and never loses what it already saw"
    );
    be.shutdown().await.expect("shutdown");
}

/// A dirty node is NEVER dropped by a revalidation pass — it holds RAM
/// records no disk image has yet — and the skip is counted as the
/// must-stay-0 tripwire that says "someone armed reader revalidation on a
/// mount that writes".
#[tokio::test]
async fn revalidation_never_drops_a_dirty_node_and_counts_the_tripwire() {
    let (_f, cache, addrs) = cache_with_nodes(2, 0).await;
    let alloc = Arc::new(
        squeezefs::meta_backend::kv::alloc_ext::ExtentAllocator::format(8, 0, 4096),
    );
    let mut ctx = SmoContext::new(alloc);
    let seq = Arc::new(AtomicU64::new(1));
    // A tree over the same cache gives us a legitimate dirty node.
    let tree = KvTree::create(cache.clone(), &mut ctx, TREE_INODES, seq)
        .await
        .expect("create tree");
    tree.insert(&inode_key(7), inode_value(7).encode())
        .await
        .expect("insert");
    let leaf = tree.resolve_leaf(&inode_key(7)).await.expect("resolve");
    assert_ne!(leaf.dirty_floor(), u64::MAX, "the leaf is dirty");

    cache
        .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), None)
        .expect("arm");
    let before = META_KV_REVALIDATE_DIRTY_SKIPS.load(Ordering::Acquire);
    let out = cache.revalidate(&RootEpoch::synthetic(2, 0, &[]));
    assert!(out.advanced);
    assert_eq!(out.skipped_dirty, 1, "the dirty leaf was skipped");
    assert_eq!(
        META_KV_REVALIDATE_DIRTY_SKIPS.load(Ordering::Acquire) - before,
        1,
        "and the tripwire counted it"
    );
    assert!(
        cache.contains(leaf.addr()),
        "a dirty node keeps its mapping — dropping it would lose RAM records"
    );
    // The clean hand-written nodes went.
    for a in &addrs {
        assert!(!cache.contains(*a));
    }
}

// ---------------------------------------------------------------------------
// 3. The R-6 purge trigger (§6.8 item 5: "the invalidation primitive
//    already exists and is complete; only the remote trigger is missing").
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CountingSink {
    calls: AtomicU64,
    last: std::sync::Mutex<Option<(u64, u64)>>,
}

impl EpochPurgeSink for CountingSink {
    fn on_epoch_advance(&self, from: u64, to: u64) -> u64 {
        self.calls.fetch_add(1, Ordering::AcqRel);
        *self.last.lock().unwrap() = Some((from, to));
        3 // "keys purged", so the plumbing of the return value is pinned too
    }
}

/// The trigger fires exactly once per epoch ADVANCE, carrying the epoch
/// pair, and never on an inert poll.
#[tokio::test]
async fn the_purge_trigger_fires_once_per_advance_and_never_on_an_inert_poll() {
    let (_f, cache, _addrs) = cache_with_nodes(2, 0).await;
    let sink = Arc::new(CountingSink::default());
    cache
        .arm_revalidation(&RootEpoch::synthetic(5, 0, &[]), Some(sink.clone()))
        .expect("arm");

    let inert = cache.revalidate(&RootEpoch::synthetic(5, 0, &[]));
    assert!(!inert.advanced);
    assert_eq!(sink.calls.load(Ordering::Acquire), 0, "no advance, no purge");

    let out = cache.revalidate(&RootEpoch::synthetic(6, 0, &[]));
    assert!(out.advanced);
    assert_eq!(sink.calls.load(Ordering::Acquire), 1);
    assert_eq!(*sink.last.lock().unwrap(), Some((5, 6)));
    assert_eq!(out.keys_purged, 3, "the sink's count rides the outcome");
}

/// The shipped sink routes every suspect block key through the **one**
/// unified purge (`TieredCache::purge_block_key`, the R-6 law), and drains
/// its suspect set exactly once.
#[tokio::test]
async fn the_tiered_purge_sink_routes_suspects_through_the_unified_purge() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::cache::TieredCache;
    use squeezefs::meta_backend::kv::revalidate::TieredEpochPurge;
    use squeezefs::nvme_dev::NvmeBlockDev;

    let dev = NamedTempFile::new().expect("temp dev");
    dev.as_file().set_len(64 * 1024 * 1024).expect("size dev");
    let nvme = Arc::new(NvmeBlockDev::new(dev.path().to_str().expect("utf8 path")));
    let ba = Arc::new(
        BlockAllocator::new("node_cache_coherence_test")
            .await
            .expect("allocator"),
    );
    let staging = tempfile::tempdir().expect("staging dir");
    let tiers = Arc::new(
        TieredCache::new(
            vec![staging.path().to_path_buf()],
            Some("8MB"),
            Some("8MB"),
            Some("8MB"),
            Some("8MB"),
            ba,
            nvme,
            None,
        )
        .await
        .expect("tiers"),
    );
    let key = "vol-0:4194304";
    tiers.read_lru.put(key, bytes::Bytes::from_static(b"stale"));
    assert!(tiers.read_lru.get(key).is_some(), "fixture: the tier holds it");

    let sink = TieredEpochPurge::new(tiers.clone());
    sink.note_suspect(key);
    assert_eq!(sink.pending(), 1);

    let (_f, cache, _addrs) = cache_with_nodes(1, 0).await;
    cache
        .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), Some(sink.clone()))
        .expect("arm");
    let out = cache.revalidate(&RootEpoch::synthetic(2, 0, &[]));
    assert_eq!(out.keys_purged, 1);
    assert!(
        tiers.read_lru.get(key).is_none(),
        "the epoch advance purged the suspect key through the unified purge"
    );
    assert_eq!(sink.pending(), 0, "the suspect set drains exactly once");

    // A second advance with nothing registered purges nothing.
    assert_eq!(cache.revalidate(&RootEpoch::synthetic(3, 0, &[])).keys_purged, 0);
}

// ---------------------------------------------------------------------------
// 4. The derived cadence and the stated staleness bound.
// ---------------------------------------------------------------------------

/// The cadence is DERIVED from the writer's own checkpoint guarantee
/// (`checkpoint.rs`'s ≤ 1 s ceiling and the flush cadence), never a
/// free-floating constant — the standing derivation law. The env override
/// wins verbatim; a malformed value keeps the derivation (the knob's
/// startup refusal gate owns the loud rejection).
#[test]
fn the_revalidation_cadence_derives_from_the_checkpoint_guarantee() {
    // Faster flush cadences cannot beat the checkpoint ceiling: a poll
    // more often than records can appear buys staleness nothing and pays
    // a drop pass for it.
    assert_eq!(resolve_revalidate_interval_ms(50, None), 1000);
    assert_eq!(resolve_revalidate_interval_ms(0, None), 1000);
    // A slower operator-chosen cadence moves the derivation with it.
    assert_eq!(resolve_revalidate_interval_ms(5000, None), 5000);
    // Explicit wins verbatim, in both directions.
    assert_eq!(resolve_revalidate_interval_ms(50, Some("250")), 250);
    assert_eq!(resolve_revalidate_interval_ms(5000, Some("100000")), 100_000);
    // Malformed keeps the derived value.
    assert_eq!(resolve_revalidate_interval_ms(50, Some("soon")), 1000);
    assert_eq!(
        REVALIDATE_INTERVAL_ENV, "SQUEEZEFS_META_REVALIDATE_MS",
        "the knob name is part of the operator contract"
    );
}

/// The poller's staleness bound is machine-readable, not prose: a reader
/// can be behind by its own poll interval **plus** the writer's checkpoint
/// interval (a record written just after a poll is seen at the next one).
#[test]
fn the_poller_states_its_staleness_bound() {
    let p = RevalidationPoller::new(1000);
    assert_eq!(p.interval(), Duration::from_millis(1000));
    assert_eq!(
        p.staleness_bound(),
        Duration::from_millis(2000),
        "interval + the ≤1 s checkpoint ceiling"
    );
    let slow = RevalidationPoller::new(5000);
    assert_eq!(slow.staleness_bound(), Duration::from_millis(6000));
}

/// Due-ness is decided against an injected clock — no sleeps anywhere in
/// this machinery's tests.
#[test]
fn the_poller_is_due_only_after_its_interval_elapses() {
    let t0 = Instant::now();
    let p = RevalidationPoller::new(1000);
    assert!(p.due_at(t0), "a poller that has never polled is due");
    p.mark(t0);
    assert!(!p.due_at(t0 + Duration::from_millis(999)));
    assert!(p.due_at(t0 + Duration::from_millis(1000)));
}

// ---------------------------------------------------------------------------
// 5. The backend-level entry point (what the RO mount calls).
// ---------------------------------------------------------------------------

/// `revalidate_reader` refuses on a mount that has not declared itself a
/// reader: adopting the ledger's roots on a write mount whose SMOs have
/// moved past them would be time travel, so the declaration is mandatory
/// and the refusal is loud.
#[tokio::test]
async fn revalidate_reader_refuses_an_undeclared_mount() {
    let (_f, be) = writer().await;
    let err = be
        .revalidate_reader()
        .await
        .expect_err("an unarmed mount must refuse");
    assert!(
        format!("{err}").contains("reader"),
        "the refusal names the missing declaration: {err}"
    );
    be.shutdown().await.expect("shutdown");
}

/// End to end through the real ledger: a declared reader revalidates, its
/// trees adopt the record's roots, its cache drops the stale nodes, and
/// every record the writer checkpointed is still served afterwards.
#[tokio::test]
async fn a_declared_reader_revalidates_through_the_real_ledger() {
    let (_f, be) = writer().await;
    for i in 0..8 {
        be.create(1, &format!("f{i}"), 0o644, 0, 0)
            .await
            .expect("create");
    }
    be.checkpoint_now().await.expect("checkpoint");

    be.arm_reader_revalidation(None).expect("declare reader");
    let out = be.revalidate_reader().await.expect("revalidate");
    assert!(
        !out.advanced,
        "the mount's own newest record is the epoch it armed at"
    );
    for i in 0..8 {
        assert!(
            be.lookup(1, &format!("f{i}")).await.expect("lookup").is_some(),
            "reads survive a revalidation pass"
        );
    }
    // A declared reader may not mutate: the enforcement, not a comment.
    let err = be
        .create(1, "illegal", 0o644, 0, 0)
        .await
        .expect_err("a declared reader must refuse to write");
    assert!(
        format!("{err}").to_lowercase().contains("read")
            || format!("{err}").to_lowercase().contains("reader"),
        "the refusal says why: {err}"
    );
}

// ---------------------------------------------------------------------------
// 6. The writer half: partitioning, not coherence (§6.2 closing).
// ---------------------------------------------------------------------------

/// **The interior population has ONE cacher.** Lock order 4b gives
/// interior-node mutation exclusively to the serialized per-volume
/// checkpoint/SMO task, and `read_partitioned_ledger` already refuses a
/// non-authority record that names tree roots. This pins the RAM half of
/// that argument at the ONE choke point every node mutation passes: a
/// non-authority appender applying to a `level > 0` node is refused loud
/// and counted.
#[tokio::test]
async fn a_non_authority_appender_may_not_mutate_an_interior_node() {
    let (_f, cache, addrs) = cache_with_nodes(1, 1).await; // level 1 = interior
    cache.set_appender(4, 2).expect("declare appender 2 of 4");
    let node = cache.try_get(addrs[0]).expect("mapped");
    let before = META_KV_NODE_PARTITION_REFUSALS.load(Ordering::Acquire);

    let mut guard = node.lock().write().await;
    let err = node
        .apply_locked(
            &mut guard,
            vec![squeezefs::meta_backend::kv::node_cache::OwnedRec::new(
                bytes::Bytes::from_static(b"k"),
                1,
                squeezefs::meta_backend::kv::record::RecordKind::Put,
                bytes::Bytes::from_static(b"v"),
            )],
            1,
        )
        .expect_err("a non-authority appender must not mutate structure");
    assert!(
        format!("{err}").contains("authority"),
        "the refusal names the partitioning law: {err}"
    );
    assert_eq!(
        META_KV_NODE_PARTITION_REFUSALS.load(Ordering::Acquire) - before,
        1,
        "and the must-stay-0 tripwire counted it"
    );
}

/// The same appender may mutate LEAVES — the cache does not pretend to own
/// the leaf partition (that is the slot map's job, spec §6.2 items 4/8).
/// Stating the scope honestly is what keeps the gate from being mistaken
/// for cross-writer arbitration.
#[tokio::test]
async fn the_partition_gate_does_not_arbitrate_leaves() {
    let (_f, cache, addrs) = cache_with_nodes(1, 0).await; // level 0 = leaf
    cache.set_appender(4, 2).expect("declare appender 2 of 4");
    let node = cache.try_get(addrs[0]).expect("mapped");
    let mut guard = node.lock().write().await;
    node.apply_locked(
        &mut guard,
        vec![squeezefs::meta_backend::kv::node_cache::OwnedRec::new(
            bytes::Bytes::from_static(b"k"),
            1,
            squeezefs::meta_backend::kv::record::RecordKind::Put,
            bytes::Bytes::from_static(b"v"),
        )],
        1,
    )
    .expect("leaf applies are the appender's own business");
}

/// Solo is the shipped posture: the gate is structurally inert, so a
/// single-writer mount's SMO task keeps mutating interior nodes exactly as
/// it does today.
#[tokio::test]
async fn the_partition_gate_is_inert_on_a_solo_volume() {
    let (_f, cache, addrs) = cache_with_nodes(1, 1).await;
    let node = cache.try_get(addrs[0]).expect("mapped");
    let mut guard = node.lock().write().await;
    node.apply_locked(
        &mut guard,
        vec![squeezefs::meta_backend::kv::node_cache::OwnedRec::new(
            bytes::Bytes::from_static(b"k"),
            1,
            squeezefs::meta_backend::kv::record::RecordKind::Put,
            bytes::Bytes::from_static(b"v"),
        )],
        1,
    )
    .expect("a solo appender IS the authority");
}

/// An armed reader mutates nothing — the declaration is enforced at the
/// same choke point, so a mount that arms revalidation and then tries to
/// write fails loud instead of diverging from the volume it is reading.
#[tokio::test]
async fn an_armed_reader_may_not_mutate_any_node() {
    let (_f, cache, addrs) = cache_with_nodes(1, 0).await;
    cache
        .arm_revalidation(&RootEpoch::synthetic(1, 0, &[]), None)
        .expect("arm");
    let node = cache.try_get(addrs[0]).expect("mapped");
    let mut guard = node.lock().write().await;
    let err = node
        .apply_locked(
            &mut guard,
            vec![squeezefs::meta_backend::kv::node_cache::OwnedRec::new(
                bytes::Bytes::from_static(b"k"),
                1,
                squeezefs::meta_backend::kv::record::RecordKind::Put,
                bytes::Bytes::from_static(b"v"),
            )],
            1,
        )
        .expect_err("an armed reader must not mutate");
    assert!(format!("{err}").contains("reader"), "{err}");
}

/// **The runtime detector the partitioned-append formats said did not
/// exist.** A peer that appends into a node's log while we hold it cached
/// would be silently overwritten by our next append (`append_bset` writes
/// at our remembered tail offset and only checks the node incarnation).
/// On a partitioned volume the append probes its destination page first
/// and refuses loud.
#[tokio::test]
async fn a_foreign_append_into_our_log_is_refused_instead_of_overwritten() {
    let (file, cache, addrs) = cache_with_nodes(1, 0).await;
    cache.set_appender(2, 1).expect("declare appender 1 of 2");
    let node = cache.try_get(addrs[0]).expect("mapped");
    // Our RAM says the log is empty; a peer appends a frame at our tail.
    let tail = {
        let g = node.lock().read().await;
        g.tail_offset()
    };
    let frame = squeezefs::meta_backend::kv::node::encode_bset_frame(
        &cache.config().layout,
        node.node_seq(),
        &[Record::put(inode_key(9).to_vec(), 9, inode_value(9).encode())],
        9,
    )
    .expect("encode a peer frame");
    squeezefs::uring_fs::write_at(file.path(), addrs[0] + tail as u64, frame)
        .await
        .expect("peer append");

    // Now freeze our own records and try to append them there.
    let mut guard = node.lock().write().await;
    node.apply_locked(
        &mut guard,
        vec![squeezefs::meta_backend::kv::node_cache::OwnedRec::new(
            bytes::Bytes::from(inode_key(10).to_vec()),
            10,
            squeezefs::meta_backend::kv::record::RecordKind::Put,
            bytes::Bytes::from(inode_value(10).encode()),
        )],
        10,
    )
    .expect("leaf apply");
    node.freeze_locked(&mut guard, &cache.config().layout)
        .expect("freeze");
    drop(guard);

    let before = META_KV_NODE_PARTITION_REFUSALS.load(Ordering::Acquire);
    let err = cache
        .append_frozen(&node)
        .await
        .expect_err("appending over a peer's frame must refuse");
    assert!(
        format!("{err}").contains("foreign"),
        "the refusal names the foreign append: {err}"
    );
    assert_eq!(
        META_KV_NODE_PARTITION_REFUSALS.load(Ordering::Acquire) - before,
        1
    );
}

/// The probe is **non-solo only**: a solo volume has no peers by
/// construction, so the shipped append path pays no extra device read.
/// (Proven by behavior: the same shape a partitioned volume refuses is
/// appended without complaint here.)
#[tokio::test]
async fn the_foreign_append_probe_is_inert_on_a_solo_volume() {
    let (file, cache, addrs) = cache_with_nodes(1, 0).await;
    let node = cache.try_get(addrs[0]).expect("mapped");
    let tail = {
        let g = node.lock().read().await;
        g.tail_offset()
    };
    let frame = squeezefs::meta_backend::kv::node::encode_bset_frame(
        &cache.config().layout,
        node.node_seq(),
        &[Record::put(inode_key(9).to_vec(), 9, inode_value(9).encode())],
        9,
    )
    .expect("encode frame");
    squeezefs::uring_fs::write_at(file.path(), addrs[0] + tail as u64, frame)
        .await
        .expect("write frame");

    let mut guard = node.lock().write().await;
    node.apply_locked(
        &mut guard,
        vec![squeezefs::meta_backend::kv::node_cache::OwnedRec::new(
            bytes::Bytes::from(inode_key(10).to_vec()),
            10,
            squeezefs::meta_backend::kv::record::RecordKind::Put,
            bytes::Bytes::from(inode_value(10).encode()),
        )],
        10,
    )
    .expect("leaf apply");
    node.freeze_locked(&mut guard, &cache.config().layout)
        .expect("freeze");
    drop(guard);
    assert!(
        cache.append_frozen(&node).await.expect("solo append"),
        "solo appends never consult the probe"
    );
}
