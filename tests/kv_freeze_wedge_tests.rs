//! **The checkpoint AlreadyFreezing wedge** — 2026-08-19 field conviction
//! (real-fabric mw fleet, `sqz-mw-authority`): one maintenance pass staged
//! an oversize record ("record value length 66121 exceeds the per-volume
//! cap 65792"), its freeze errored MID-ENCODE, and from that tick EVERY
//! checkpoint cycle logged `corrupt KV encoding: freeze refused on node
//! 0x23c0000: AlreadyFreezing` forever — journal tail pinned, conveyor
//! degraded, co-writer self-fence, fsync EIO. Three mechanisms, each
//! pinned red-first:
//!
//! * **W-A (the latch)**: `CachedNode::freeze_locked` set `FREEZING`
//!   (`begin_freeze`) and then let `encode_bset_frame`/`extend_with`
//!   errors `?`-escape BEFORE `guard.frozen = Some(..)` — `FREEZING`
//!   stayed latched with no frozen delta, and `abort_freeze` (the
//!   designated restore, `node_state_core`) had ZERO production callers.
//!   Law: ANY error after a successful `begin_freeze` restores
//!   freezability (the overlay is untouched on every such path, so the
//!   restore is exactly the lifecycle word).
//! * **W-B (the sibling window)**: `Tree::compact_node_forced` entered
//!   `FREEZING` through `begin_forced_freeze` (empty frozen delta by
//!   construction) and cleaned up with a straight-line `end_freeze` a
//!   dropped future (job cancel) or unwind skips. Law: a drop guard owns
//!   the forced-freeze bit.
//! * **The tripwire**: a THIRD unknown window producing the latched shape
//!   (`FREEZING && frozen.is_none() && !superseded`, observed on the
//!   serialized flush task under the node write lock) degrades to ONE
//!   loud `invariant_tripwires` line + abort-and-retry (RES-22), never a
//!   permanent wedge. And the refusal itself is an honest protocol error
//!   (`KvError::FreezeRefused`), no longer "corrupt KV encoding" — the
//!   field hunt chased phantom device corruption on that text.
//!
//! Plus the ADMISSION honesty the trigger relied on: an over-cap record
//! now fails ITS OWN commit (`commit_tx`'s existing fail-alone step), not
//! the volume's checkpoint.
//!
//! The trigger itself (the owner-side chained merge composing past the
//! inline ceiling) is pinned in `tests/mw_widthn_refs_tests.rs` §1e.

use squeezefs::meta_backend::kv::alloc_ext::ExtentAllocator;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::node::NodeLayout;
use squeezefs::meta_backend::kv::node_cache::{
    NodeCache, NodeCacheConfig, TEST_FREEZE_ENCODE_FAIL,
};
use squeezefs::meta_backend::kv::record::{inode_key, TREE_INODES};
use squeezefs::meta_backend::kv::tree::{
    test_smo_build_pause_release, KvTree, SmoContext, TEST_SMO_BUILD_PAUSED,
    TEST_SMO_BUILD_PAUSE_TREE,
};
use squeezefs::meta_backend::Metadata;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// The KvTree harness (the kv_tree_tests fixture, shrunk): a file-backed
// volume + cache + allocator + serialized SMO context — freeze/flush
// mechanics without a full backend.
// ---------------------------------------------------------------------------

const NODE_SIZE: usize = 64 * 1024;

struct Vol {
    _file: NamedTempFile,
    cache: Arc<NodeCache>,
    ctx: SmoContext,
    seq: Arc<squeezefs::meta_backend::kv::node_seq::NodeSeqHandle>,
}

impl Vol {
    fn new() -> Self {
        let file = NamedTempFile::new().expect("temp volume");
        file.as_file()
            .set_len(512 * NODE_SIZE as u64)
            .expect("size volume");
        let layout = NodeLayout::new(NODE_SIZE).expect("layout");
        let cache = NodeCache::new(NodeCacheConfig {
            path: file.path().to_path_buf(),
            layout,
            heap_base: 0,
            budget_bytes: 512 * NODE_SIZE as u64,
            // Large threshold: nothing auto-enqueues mid-test — the
            // flush passes below are the only freeze drivers.
            writeback_delta_bytes: 1024 * 1024,
        });
        let alloc = Arc::new(ExtentAllocator::format(512, 0, 4096));
        let seq = Arc::new(squeezefs::meta_backend::kv::node_seq::NodeSeqHandle::shared(0));
        let ctx = SmoContext::new(alloc.clone());
        Self {
            _file: file,
            cache,
            ctx,
            seq,
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

fn ikey(i: u64) -> Vec<u8> {
    inode_key(i).to_vec()
}

fn val(i: u64) -> Vec<u8> {
    let mut v = vec![(i % 251) as u8; 48];
    v[..8].copy_from_slice(&i.to_le_bytes());
    v
}

fn tripwires() -> u64 {
    squeezefs::fuse_client::METRICS
        .invariant_tripwires
        .load(Ordering::Relaxed)
}

/// W-A drop-guard restore: an error escaping `freeze_locked` AFTER
/// `begin_freeze` succeeded (the injected seam = the field's oversize
/// `encode_bset_frame` failure) must leave the node FREEZABLE — the very
/// next flush pass serializes it. Red on the pre-fix tree: the second
/// pass refuses `AlreadyFreezing` forever (the 2026-08-19 wedge).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_encode_failure_inside_freeze_leaves_the_node_freezable() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            TEST_FREEZE_ENCODE_FAIL.store(0, Ordering::SeqCst);
        }
    }
    let _cleanup = Cleanup;
    let mut vol = Vol::new();
    let tree = vol.tree().await;
    for i in 0..24u64 {
        tree.insert(&ikey(i), val(i)).await.expect("insert");
    }

    let t0 = tripwires();
    TEST_FREEZE_ENCODE_FAIL.store(1, Ordering::SeqCst);
    let err = tree
        .flush_dirty(&mut vol.ctx)
        .await
        .expect_err("the injected freeze-encode failure propagates");
    assert!(
        format!("{err}").contains("injected freeze-encode failure"),
        "the propagated error is the injection, nothing else: {err}"
    );
    assert_eq!(
        TEST_FREEZE_ENCODE_FAIL.load(Ordering::SeqCst),
        0,
        "the seam engaged exactly once"
    );

    // The node must still be freezable: the same flush arm, one tick
    // later, serializes the SAME records — no AlreadyFreezing, ever.
    tree.flush_dirty(&mut vol.ctx).await.expect(
        "the node stays freezable after a failed freeze — the error path \
         restored the lifecycle word (abort_freeze), so the next cycle \
         retries instead of wedging on AlreadyFreezing",
    );
    assert_eq!(
        tripwires() - t0,
        0,
        "the restore is the error path's own duty — the RES-22 tripwire \
         (the third-window backstop) must NOT be what saved this"
    );
    // The records survived the failed freeze (the overlay was untouched)
    // and the successful one.
    for i in 0..24u64 {
        assert_eq!(
            tree.lookup(&ikey(i)).await.expect("lookup").as_deref(),
            Some(&val(i)[..]),
            "record {i} serves after the failed-then-retried freeze"
        );
    }
}

/// The tripwire arm (RES-22): a hand-latched `FREEZING` with no frozen
/// delta — the shape a THIRD unknown window would leave — is observed by
/// the serialized flush arm, reported through `invariant_tripwires`
/// (exactly once), RECOVERED (abort + retry), and the pass completes.
/// Red on the pre-fix tree: the flush errors `AlreadyFreezing` and every
/// later pass errors identically — the permanent wedge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hand_latched_freeze_recovers_loud_with_one_tripwire() {
    let mut vol = Vol::new();
    let tree = vol.tree().await;
    for i in 0..24u64 {
        tree.insert(&ikey(i), val(i)).await.expect("insert");
    }
    // Hand-latch the wedge shape via the state core: FREEZING set (the
    // forced transition — no frozen delta by construction), overlay
    // non-empty, not superseded.
    let node = tree.resolve_leaf(&ikey(0)).await.expect("leaf");
    node.state()
        .begin_forced_freeze()
        .expect("hand-latch FREEZING");

    let t0 = tripwires();
    tree.flush_dirty(&mut vol.ctx).await.expect(
        "the flush arm recovers a latched freeze (abort + retry) instead \
         of wedging the volume's checkpoint forever",
    );
    assert_eq!(
        tripwires() - t0,
        1,
        "exactly one loud invariant_tripwires line for the recovery"
    );
    for i in 0..24u64 {
        assert_eq!(
            tree.lookup(&ikey(i)).await.expect("lookup").as_deref(),
            Some(&val(i)[..]),
            "record {i} serves after the recovered flush"
        );
    }
    // A healthy follow-up pass is silent: the latch was healed, not
    // merely bypassed.
    tree.flush_dirty(&mut vol.ctx)
        .await
        .expect("a clean follow-up flush");
    assert_eq!(tripwires() - t0, 1, "no further tripwires on clean passes");
}

/// The error class: a freeze refusal is a PROTOCOL state, not encoding
/// corruption. The 2026-08-19 field hunt chased phantom device corruption
/// on the "corrupt KV encoding: freeze refused …" text for hours. The
/// refusal keeps its content (node addr + refusal kind) and drops the
/// corruption claim. Exercised through the real construction site: a
/// direct `freeze_locked` on a superseded node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_freeze_refusal_is_a_protocol_error_not_encoding_corruption() {
    let mut vol = Vol::new();
    let tree = vol.tree().await;
    for i in 0..8u64 {
        tree.insert(&ikey(i), val(i)).await.expect("insert");
    }
    let node = tree.resolve_leaf(&ikey(0)).await.expect("leaf");
    // Sever the node by hand: `begin_freeze` refuses `Superseded` — the
    // one refusal the self-heal deliberately never intercepts.
    node.state().supersede().expect("hand supersede");
    let mut guard = node.lock().write().await;
    let err = match node.freeze_locked(&mut guard, &vol.cache.config().layout) {
        Err(e) => e,
        Ok(_) => panic!("a superseded node must refuse to freeze"),
    };
    let text = format!("{err}");
    assert!(
        text.contains("freeze refused on node") && text.contains("Superseded"),
        "the refusal keeps its content (node addr, refusal kind): {text}"
    );
    assert!(
        !text.contains("corrupt KV encoding"),
        "a protocol-state refusal must not claim encoding corruption \
         (the 2026-08-19 phantom-corruption hunt): {text}"
    );
}

/// **A flush that appends a PARKED frozen delta keeps the floor of the
/// records applied since** (PR 13 — the eight-writer storm's "deleted
/// stays deleted" loss, an acked-loss class reproduced 5/5 at N = 8 and
/// pinned by `LEAVE-DIFF`: the leaving daemon's RAM said `Tombstone`, the
/// released leaf's image said `Live`). The shape: an SMO freezes a node
/// for its fold (`freeze_for_smo` — a REAL freeze-swap) and fails before
/// its swap (a merge or compaction refused an extent — `GrantExhausted`,
/// the joined appender's common case under a grant storm), leaving the
/// frozen delta PARKED with the node's dirty floor intact; commits keep
/// applying into the OPEN delta (`mark_dirty` admits an apply while
/// FREEZING). The next `checkpoint_flush_node` gets the parked delta
/// back from `freeze_locked`, TOOK the whole dirty floor and appended the
/// parked delta alone — the newer records stayed in the overlay with
/// `dirty_floor == MAX`: no later flush walked them, nothing clamped the
/// tail, and a leave released the tree without them. Pinned: after the
/// first flush the node is STILL DIRTY (its floor = the newer records'),
/// `meta_kv_flush_floor_kept` counted once, the second flush appends
/// them, and every record — the parked delta's and the newer ones —
/// reads back from the DEVICE image.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flush_of_a_parked_frozen_delta_keeps_the_floor_of_the_records_applied_since() {
    let mut vol = Vol::new();
    let tree = vol.tree().await;
    for i in 0..24u64 {
        tree.insert(&ikey(i), val(i)).await.expect("insert");
    }
    tree.flush_dirty(&mut vol.ctx).await.expect("a clean base");
    // The parked delta: records 100..108 applied, then frozen by hand —
    // the failed SMO's `freeze_for_smo` (a real freeze-swap; the dirty
    // floor untouched, exactly as the SMO leaves it).
    for i in 100..108u64 {
        tree.insert(&ikey(i), val(i)).await.expect("insert");
    }
    let node = tree.resolve_leaf(&ikey(0)).await.expect("leaf");
    {
        let mut guard = node.lock().write().await;
        let frozen = node
            .freeze_locked(&mut guard, &vol.cache.config().layout)
            .expect("the SMO's freeze");
        assert!(frozen.is_some(), "the parked delta");
    }
    let floor_before = node.dirty_floor();
    assert_ne!(
        floor_before,
        u64::MAX,
        "the node is dirty with the parked delta"
    );
    // Newer records land in the OPEN delta while the frozen one is parked
    // (the storm's last unlinks — here: deletes of half the base).
    for i in 0..12u64 {
        tree.delete(&ikey(i)).await.expect("delete");
    }
    let kept0 = squeezefs::meta_backend::kv::META_KV_FLUSH_FLOOR_KEPT.load(Ordering::Relaxed);
    // The checkpoint's flush step: appends the PARKED delta …
    tree.checkpoint_flush_node(&mut vol.ctx, node.addr())
        .await
        .expect("the flush step");
    // … and the node stays DIRTY for the records it did not take.
    assert_ne!(
        node.dirty_floor(),
        u64::MAX,
        "the floor of the newer records is KEPT (red before: taken with the parked delta's)"
    );
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_FLUSH_FLOOR_KEPT.load(Ordering::Relaxed),
        kept0 + 1,
        "the gauge counts the kept floor once"
    );
    // The next pass flushes them; then the node is clean.
    tree.checkpoint_flush_node(&mut vol.ctx, node.addr())
        .await
        .expect("the second flush step");
    assert_eq!(node.dirty_floor(), u64::MAX, "clean after the second pass");
    // Every record — the parked delta's and the newer deletes — is in the
    // DEVICE image: a fresh cache over the same file reads the tree back.
    let layout = vol.cache.config().layout;
    let path = vol.cache.config().path.clone();
    let root = tree.root();
    let fresh = NodeCache::new(NodeCacheConfig {
        path,
        layout,
        heap_base: 0,
        budget_bytes: 512 * NODE_SIZE as u64,
        writeback_delta_bytes: 1024 * 1024,
    });
    let reopened = KvTree::open(fresh, TREE_INODES, root, vol.seq.clone())
        .await
        .expect("open the tree at its root from the device");
    for i in 0..12u64 {
        assert_eq!(
            reopened.lookup(&ikey(i)).await.expect("lookup"),
            None,
            "deleted {i} stays deleted on the device"
        );
    }
    for i in 12..24u64 {
        assert_eq!(
            reopened.lookup(&ikey(i)).await.expect("lookup").as_deref(),
            Some(&val(i)[..])
        );
    }
    for i in 100..108u64 {
        assert_eq!(
            reopened.lookup(&ikey(i)).await.expect("lookup").as_deref(),
            Some(&val(i)[..]),
            "the parked delta's record {i} is on the device"
        );
    }
}

// ---------------------------------------------------------------------------
// W-B: the forced-compaction window (a full backend — the only public
// route to `compact_node_forced` is `defrag_compact_nodes`).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 256 * 1024 * 1024;

async fn backend_sandbox() -> (Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        file.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: NODE_SIZE,
            journal_len_override: Some(8 * 1024 * 1024),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = KvMetaBackend::open(file.path()).await.expect("open");
    (kv, file)
}

/// W-B: a `compact_node_forced` future DROPPED while parked in its SMO
/// build window (the job-cancel shape — `TEST_SMO_BUILD_PAUSE_TREE` holds
/// it open deterministically) leaves the node FREEZABLE: the next
/// checkpoint tick over re-accumulated dirt succeeds. Red on the pre-fix
/// tree: the straight-line `end_freeze` cleanup is skipped by the drop,
/// `FREEZING` stays latched, and `checkpoint_now` refuses
/// `AlreadyFreezing` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_forced_compaction_leaves_the_node_freezable() {
    // Park the checkpoint cadence (the kv_smo_crash_completeness
    // precedent): the flush passes below are the ones this test drives.
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

    let (kv, _file) = backend_sandbox().await;

    // Dead records in an inode leaf: create, checkpoint (bset 1 = the
    // creates), overwrite via setattr, checkpoint (bset 2 = the
    // overwrites) — the census then names the leaf a compaction
    // candidate with an EMPTY overlay (the forced-freeze precondition).
    let mut inos = Vec::with_capacity(80);
    for i in 0..80u32 {
        let ino = kv
            .create(1, &format!("w{i:03}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        inos.push(ino);
    }
    kv.checkpoint_now().await.expect("checkpoint the creates");
    for ino in &inos {
        kv.setattr(
            *ino,
            Some(libc::S_IFREG | 0o600),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("setattr");
    }
    kv.checkpoint_now()
        .await
        .expect("checkpoint the overwrites");
    let census = kv.dead_bset_census();
    let &(tree_id, addr) = census
        .candidates
        .iter()
        .find(|(t, _)| *t == TREE_INODES)
        .expect("an inode leaf with dead records (the setattr overwrites)");

    // Park the forced compaction in its build window, then DROP it (the
    // job-cancel / unwind shape).
    TEST_SMO_BUILD_PAUSE_TREE.store(u64::from(tree_id), Ordering::SeqCst);
    let kv2 = Arc::clone(&kv);
    let handle = tokio::spawn(async move { kv2.defrag_compact_nodes(&[(tree_id, addr)]).await });
    let mut parked = false;
    for _ in 0..2500 {
        if TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex").is_some() {
            parked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(
        parked,
        "the forced compaction must park in its build window"
    );
    handle.abort();
    let _ = handle.await;
    test_smo_build_pause_release();
    *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = None;

    // Re-dirty the same leaf (commits keep landing during/after a freeze
    // by design) and run the next checkpoint tick: it must serialize the
    // records — no AlreadyFreezing, no tripwire (the drop guard restored
    // the bit; the RES-22 backstop had nothing to heal).
    let t0 = tripwires();
    const MARKER: u32 = libc::S_IFREG | 0o751;
    for ino in &inos {
        kv.setattr(*ino, Some(MARKER), None, None, None, None, None, None)
            .await
            .expect("re-dirty setattr acked");
    }
    kv.checkpoint_now().await.expect(
        "the checkpoint tick after a dropped forced compaction proceeds — \
         the forced-freeze bit is drop-guard-owned, never latched",
    );
    assert_eq!(
        tripwires() - t0,
        0,
        "the drop guard is the mechanism — the RES-22 tripwire must not \
         be what saved this tick"
    );
    for ino in &inos {
        assert_eq!(
            kv.getattr(*ino).await.expect("getattr").mode,
            MARKER,
            "the re-dirtied records serialized and serve"
        );
    }
    kv.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// Admission honesty: the over-cap record fails ITS OWN commit.
// ---------------------------------------------------------------------------

/// The trigger's admission backstop: a staged record whose value exceeds
/// the per-volume record cap (`min(65536, node_size/4)` + envelope) is
/// refused at COMMIT admission — the one choke point every `KvTx`
/// passes — so a future oversize-staging site fails its own commit loud
/// instead of acking the record into RAM and wedging the volume's
/// checkpoint at freeze time (the 2026-08-19 shape: admission passed,
/// the freeze failed, the volume never checkpointed again). Red on the
/// pre-fix tree: the commit ACKS and the wedge is armed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_cap_record_fails_its_own_commit_not_the_checkpoint() {
    let (kv, _file) = backend_sandbox().await;
    let ino = kv
        .create(1, "big.bin", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    // Past the 64 KiB-node record cap (16,384 + 256 B envelope); the
    // layout bytes are staged verbatim by `set_layout_and_size` and
    // nothing upstream of the commit screens them (the field's exact
    // ingress class).
    let oversize = vec![0x5au8; 17_000];
    let err = kv
        .set_layout_and_size(ino, &oversize, 17_000, &[])
        .await
        .expect_err(
            "an over-cap record must fail ITS OWN commit — acked into the \
             overlay it can only fail at freeze, wedging the checkpoint",
        );
    assert!(
        format!("{err}").contains("exceeds the per-volume cap"),
        "the refusal names the record-value cap: {err}"
    );
    // Nothing reached the overlay: the layout slot is empty and the
    // volume's checkpoint stays healthy.
    assert_eq!(
        kv.getxattr(ino, "layout").await.expect("getxattr"),
        None,
        "the refused record never landed"
    );
    kv.checkpoint_now()
        .await
        .expect("the volume checkpoints clean — the wedge is unreachable");
    kv.shutdown().await.expect("shutdown");
}
