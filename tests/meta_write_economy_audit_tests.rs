//! Prong-2 of perf/meta-plane-writes (2026-07-30): the per-block meta
//! write economy audit + the freeze-time shadow-fold contract.
//!
//! Field facts (4-node cluster, f16d5a9, `.benchmarks/2026-07-30-meta-
//! plane-writes.md`): ~8 meta device-writes per fresh 4 MiB block, ~14.5
//! per rewritten block, 18.2 KiB mean journal entry (2.64 GB journal
//! bytes / 145 k entries), node compaction:append ratio 18,814:11,500
//! (design says > ~1:8 is mistuned — the field runs 1.6:1 INVERTED),
//! 3.79 GB of whole-node CoW rewrites. Two convictions:
//!
//! 1. **The journal side is O(file_size) per publish** — every block
//!    publish re-serializes the WHOLE layout value (block map included)
//!    into its journal entry, so per-block meta bytes grow linearly with
//!    block index (O(n²) total for a streamed file). Audited here
//!    (`audit_block_publish_meta_economy` prints the window table);
//!    the representation fix (layout delta records / publish
//!    coalescing) is FILED, not landed — it is a format-level change.
//!
//! 2. **The node-writeback side re-appends superseded intra-window
//!    versions** — `freeze_locked` froze the ENTIRE overlay, so W
//!    same-key publishes within one checkpoint cadence appended W
//!    full layout values (each shadowed by the next: fold algebra —
//!    a newer Put/Delete completely shadows older same-key records).
//!    FIXED here by the freeze-time shadow-fold: the frozen bset
//!    carries, per key, only the newest base-establishing record
//!    (Put/Delete) and any Deltas newer than it; delta-only runs keep
//!    everything (their base lives in older bsets). Crash safety is
//!    unchanged: a frozen bset either fully survives (carrying the
//!    shadowing record) or is fully dropped by the §4.5 torn-tail
//!    classifier (the journal window still covers every dropped seq —
//!    the tail cannot pass the freeze's records until the covering
//!    ledger record is durable).

use bytes::Bytes;
use squeezefs::meta_backend::kv::node::NodeLayout;
use squeezefs::meta_backend::kv::node_cache::{CachedNode, NodeCache, NodeCacheConfig};
use squeezefs::meta_backend::kv::record::{inode_key, RecordKind, TREE_INODES};
use squeezefs::meta_backend::kv::tree::{ApplyOutcome, KvTree, SmoContext};
use squeezefs::meta_backend::kv::{
    alloc_ext::ExtentAllocator, META_KV_NODE_APPENDS, META_KV_NODE_APPEND_BYTES,
    META_KV_NODE_COMPACTIONS, META_KV_NODE_FREEZE_SHADOW_DROPPED, META_KV_NODE_REWRITE_BYTES,
};
use squeezefs::meta_backend::{open_routed_meta_set, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Node-layer harness (the kv_fold_slimming_tests `Vol` shape).
// ---------------------------------------------------------------------------

const NODE_SIZE: usize = 64 * 1024;

struct Vol {
    _file: NamedTempFile,
    cache: Arc<NodeCache>,
    seq: Arc<squeezefs::meta_backend::kv::node_seq::NodeSeqHandle>,
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
            // Freezes are driven explicitly here.
            writeback_delta_bytes: usize::MAX,
        });
        let alloc = Arc::new(ExtentAllocator::format(extents, 0, 4096));
        let seq = Arc::new(squeezefs::meta_backend::kv::node_seq::NodeSeqHandle::shared(0));
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

async fn apply(tree: &KvTree, leaf: &Arc<CachedNode>, key: &[u8], kind: RecordKind, value: Bytes) {
    let out = tree
        .apply_at(leaf, key, kind, value)
        .await
        .expect("apply_at");
    assert_eq!(out, ApplyOutcome::Applied, "single-leaf tree never stales");
}

async fn freeze_and_append(cache: &Arc<NodeCache>, node: &Arc<CachedNode>) -> (usize, u64) {
    let n_records = {
        let mut guard = node.lock().write().await;
        node.freeze_locked(&mut guard, &cache.config().layout)
            .expect("freeze")
            .expect("non-empty overlay must freeze");
        guard.frozen_records().len()
    };
    let bytes_before = META_KV_NODE_APPEND_BYTES.load(Ordering::Relaxed);
    assert!(
        cache.append_frozen(node).await.expect("append"),
        "test bsets always fit a 64 KiB node"
    );
    let appended = META_KV_NODE_APPEND_BYTES.load(Ordering::Relaxed) - bytes_before;
    (n_records, appended)
}

// ---------------------------------------------------------------------------
// The shadow-fold contract (RED against whole-overlay freezes)
// ---------------------------------------------------------------------------

/// W same-key Puts inside one freeze window: the frozen bset must carry
/// ONLY the newest (older versions are completely shadowed by the fold
/// algebra — appending them is pure device-byte waste; the field's
/// block-publish storm paid W × O(block_map) node-append bytes per
/// cadence window for exactly this).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_appends_only_unshadowed_records() {
    let mut vol = Vol::new(64, 32);
    let tree = vol.tree().await;
    let key_a = inode_key(7);
    let key_b = inode_key(9);
    let leaf = tree.resolve_leaf(&key_a).await.expect("resolve");

    // 16 superseding 3 KiB "layout publishes" of key A + one bystander.
    const W: usize = 16;
    const VAL_LEN: usize = 3 * 1024;
    for v in 0..W {
        apply(
            &tree,
            &leaf,
            &key_a,
            RecordKind::Put,
            Bytes::from(vec![v as u8; VAL_LEN]),
        )
        .await;
    }
    apply(
        &tree,
        &leaf,
        &key_b,
        RecordKind::Put,
        Bytes::from(vec![0xBB; VAL_LEN]),
    )
    .await;

    let (n_records, appended) = freeze_and_append(&vol.cache, &leaf).await;
    assert_eq!(
        n_records, 2,
        "the frozen bset must carry exactly the newest Put per key \
         (got {n_records} records for 2 keys — superseded versions are \
         shadowed by construction and must not reach the device)"
    );
    assert!(
        appended <= (3 * VAL_LEN) as u64,
        "one freeze of {W} superseding {VAL_LEN} B puts must append ~2 \
         values' worth, not the whole overlay — appended {appended} B \
         (whole-overlay freeze appends ≥ {} B)",
        W * VAL_LEN
    );

    // Read equivalence: the newest version serves after the fold.
    let got = tree
        .lookup(&key_a)
        .await
        .expect("lookup")
        .expect("live value");
    assert_eq!(
        got.to_vec(),
        vec![(W - 1) as u8; VAL_LEN],
        "shadow-folded freeze must serve the newest version"
    );
}

/// The KEEP side of the fold: Deltas newer than the newest Put ride the
/// freeze with it; a Delete is base-establishing (older records drop,
/// the tombstone itself is kept); delta-only runs (base in an older
/// bset) keep every delta — dropping any of them would change the fold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn freeze_keeps_deltas_newer_than_base_and_delta_only_runs() {
    // InodeValue wire shape: deltas must decode against the base, so use
    // real encodings for this contract.
    use squeezefs::meta_backend::kv::record::{InodeDelta, InodeValue};
    let base_value = |seed: u64| InodeValue {
        mode: 0o100644,
        uid: seed as u32,
        gid: 0,
        nlink: 1,
        flags: 0,
        rdev: 0,
        size: seed * 4096,
        atime: 1,
        mtime: 2,
        ctime: 3,
    };

    let mut vol = Vol::new(64, 32);
    let tree = vol.tree().await;
    let key = inode_key(3);
    let leaf = tree.resolve_leaf(&key).await.expect("resolve");

    // Run shape: Put(old), Put(new), Δtime, Δctime — the freeze must
    // keep the newest Put + BOTH newer deltas (3 records).
    apply(
        &tree,
        &leaf,
        &key,
        RecordKind::Put,
        Bytes::from(base_value(1).encode()),
    )
    .await;
    apply(
        &tree,
        &leaf,
        &key,
        RecordKind::Put,
        Bytes::from(base_value(2).encode()),
    )
    .await;
    apply(
        &tree,
        &leaf,
        &key,
        RecordKind::Delta,
        Bytes::from(InodeDelta::times(777, 778).encode()),
    )
    .await;
    apply(
        &tree,
        &leaf,
        &key,
        RecordKind::Delta,
        Bytes::from(InodeDelta::ctime(779).encode()),
    )
    .await;
    let (n_records, _) = freeze_and_append(&vol.cache, &leaf).await;
    assert_eq!(
        n_records, 3,
        "newest Put + its 2 newer deltas must survive the freeze fold"
    );
    let folded = tree
        .lookup(&key)
        .await
        .expect("lookup")
        .expect("live value");
    let v = InodeValue::decode(&folded).expect("decode");
    assert_eq!(v.mtime, 777, "Δtime must still fold onto the base");
    assert_eq!(v.ctime, 779, "the newer Δctime must still fold");
    assert_eq!(v.uid, 2, "newest base must win");

    // Delta-only window (base is now bset-resident below): every delta
    // must be kept.
    apply(
        &tree,
        &leaf,
        &key,
        RecordKind::Delta,
        Bytes::from(InodeDelta::times(1001, 1002).encode()),
    )
    .await;
    apply(
        &tree,
        &leaf,
        &key,
        RecordKind::Delta,
        Bytes::from(InodeDelta::ctime(1003).encode()),
    )
    .await;
    let (n_records, _) = freeze_and_append(&vol.cache, &leaf).await;
    assert_eq!(
        n_records, 2,
        "delta-only runs keep every record (their base lives below)"
    );
    let folded = tree
        .lookup(&key)
        .await
        .expect("lookup")
        .expect("live value");
    let v = InodeValue::decode(&folded).expect("decode");
    assert_eq!(v.mtime, 1001);
    assert_eq!(v.ctime, 1003);
    assert_eq!(v.uid, 2, "bset-resident base still folds");

    // Delete is base-establishing: Put, Delete in one window freezes to
    // the tombstone alone — and still serves as deleted.
    let key_d = inode_key(5);
    apply(
        &tree,
        &leaf,
        &key_d,
        RecordKind::Put,
        Bytes::from(base_value(9).encode()),
    )
    .await;
    apply(&tree, &leaf, &key_d, RecordKind::Delete, Bytes::new()).await;
    let (n_records, _) = freeze_and_append(&vol.cache, &leaf).await;
    assert_eq!(n_records, 1, "the tombstone shadows its window's Put");
    let gone = tree.lookup(&key_d).await.expect("lookup");
    assert!(gone.is_none(), "the delete must survive the fold");
}

// ---------------------------------------------------------------------------
// The economy audit (backend level — the evidence-note table)
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 256 * 1024 * 1024;

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn layout_bytes(blocks: usize) -> Vec<u8> {
    let mut map = std::collections::HashMap::new();
    for b in 0..blocks as u32 {
        map.insert(b, format!("backend_0://{}", b as u64 * 4 * 1024 * 1024));
    }
    let layout = squeezefs::routing::LayoutMetadata {
        file_type: "striped".to_string(),
        size: blocks as u64 * 4 * 1024 * 1024,
        block_map_id: Some("block_map_audit".to_string()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map),
    };
    bincode::serialize(&layout).expect("serialize layout")
}

/// The audit: a single file streamed to K blocks through the exact
/// commit the write path issues per published block
/// (`set_layout_and_size` with the re-serialized whole layout), then a
/// full rewrite pass. Prints the per-window journal economy (the
/// evidence-note table) and asserts the invariants that must hold
/// regardless of representation:
/// - exactly ~1 journal entry per publish (the count economy is
///   healthy; the BYTES are the conviction),
/// - per-publish journal bytes track the serialized layout size (the
///   O(block_map) growth — filed as the representation conviction).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_block_publish_meta_economy() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "audit", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3(
        &meta,
        VOL_LEN,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let paths = vec![meta.display().to_string()];
    let routed = open_routed_meta_set(&paths).await.expect("open");

    let inode = routed
        .create(1, "streamed", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");

    const K: usize = 256;
    const WINDOWS: [usize; 4] = [64, 128, 192, 256];
    let ring = routed.volumes[0].journal_ring();
    let mut rows = Vec::new();
    let mut prev = (ring.written_entries(), ring.written_bytes());
    let appends_before = META_KV_NODE_APPENDS.load(Ordering::Relaxed);
    let append_bytes_before = META_KV_NODE_APPEND_BYTES.load(Ordering::Relaxed);
    let rewrite_bytes_before = META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed);
    let compactions_before = META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed);
    let shadow_before = META_KV_NODE_FREEZE_SHADOW_DROPPED.load(Ordering::Relaxed);

    let mut window = 0;
    for k in 1..=K {
        let bytes = layout_bytes(k);
        routed
            .set_layout_and_size(inode.ino, &bytes, (k as u64) * 4 * 1024 * 1024, &[])
            .await
            .expect("publish");
        if k == WINDOWS[window] {
            let now = (ring.written_entries(), ring.written_bytes());
            rows.push((
                WINDOWS[window],
                now.0 - prev.0,
                now.1 - prev.1,
                layout_bytes(k).len(),
            ));
            prev = now;
            window += 1;
        }
    }
    // Rewrite pass: republish every block at full map size (the field's
    // steady-state destroy+republish shape, meta side).
    let full = layout_bytes(K);
    let rw_before = (ring.written_entries(), ring.written_bytes());
    for _ in 0..K {
        routed
            .set_layout_and_size(inode.ino, &full, (K as u64) * 4 * 1024 * 1024, &[])
            .await
            .expect("republish");
    }
    let rw = (
        ring.written_entries() - rw_before.0,
        ring.written_bytes() - rw_before.1,
    );

    println!("== per-block publish journal economy (fresh stream, K={K}) ==");
    println!("window        entries  bytes      bytes/publish  layout_len@end");
    let mut lo = 0usize;
    for (end, entries, bytes, layout_len) in &rows {
        let n = end - lo;
        println!(
            "({lo:>3},{end:>4}]   {entries:>7}  {bytes:>9}  {:>13.0}  {layout_len:>14}",
            *bytes as f64 / n as f64
        );
        lo = *end;
    }
    println!(
        "rewrite x{K}   {:>7}  {:>9}  {:>13.0}  {:>14}",
        rw.0,
        rw.1,
        rw.1 as f64 / K as f64,
        full.len()
    );
    println!(
        "node writeback: {} append frames / {} B, {} compactions / {} rewrite B, \
         {} shadow-dropped records",
        META_KV_NODE_APPENDS.load(Ordering::Relaxed) - appends_before,
        META_KV_NODE_APPEND_BYTES.load(Ordering::Relaxed) - append_bytes_before,
        META_KV_NODE_COMPACTIONS.load(Ordering::Relaxed) - compactions_before,
        META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed) - rewrite_bytes_before,
        META_KV_NODE_FREEZE_SHADOW_DROPPED.load(Ordering::Relaxed) - shadow_before,
    );

    // Count economy: ~1 entry per publish in every window (heartbeats /
    // times drains allow a small margin).
    for (end, entries, _, _) in &rows {
        let n = 64u64; // windows are 64 publishes wide
        assert!(
            *entries >= n && *entries <= n + 8,
            "window ending {end}: {entries} entries for {n} publishes — \
             publish count economy must stay ~1 entry/op"
        );
    }
    // Byte conviction (the FILED representation issue, asserted as a
    // documented fact so its future fix flips this audit loudly): the
    // last fresh window's per-publish journal bytes exceed the first's
    // by the O(block_map) growth factor.
    let first = rows.first().expect("windows recorded");
    let last = rows.last().expect("windows recorded");
    let first_per = first.2 as f64 / 64.0;
    let last_per = last.2 as f64 / 64.0;
    assert!(
        last_per > first_per * 2.0,
        "per-publish journal bytes are expected O(block_map) under the \
         current whole-layout representation (first window {first_per:.0} \
         B/publish, last {last_per:.0}) — if this stopped growing, the \
         layout-delta economy landed: move this audit's conviction note \
         to the fixed column in the evidence file"
    );

    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}
