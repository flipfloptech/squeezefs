//! Rewrite-shadow **supersession coherence** — the 2026-08-04 field
//! corruption fix (squeeze-test EXA battery, daemon `d0451b4d`).
//!
//! Field signature: fio read rows EIO ("did not settle after 4 serialized
//! settle attempts"), `invariant_tripwires` = 4 × `stale_binding_escalations`
//! (the settle arm NEVER wins), online fsck reporting **C2Lost** (a live
//! map binds an allocator-untracked offset — the deterministic-EIO face),
//! **C2Leaked** (allocated, zero referencers) and **C3** (2 map refs,
//! count 1). Remount healed everything: pure in-session poison.
//!
//! Root cause (the stale-shadow resurrection): an open rewrite epoch's
//! `shadow` map is written ONLY by `rewrite_shadow_record`, but the same
//! `(ino, b)` can be re-published mid-epoch by the DURABLE merge paths —
//! the flush legs (`grow_size_to_block_end=false` demands durability NOW
//! and never shadows), `pipeline_upload_serialized`, truncate — which
//! displace the shadow's B1 binding from the RAM map, stage its release,
//! and free it. `epoch.shadow[b]` still names the FREED B1, so any
//! KD-1.9 refetch-compose (TTL refill, publish-pass base fetch, the
//! close's own resolve) overlays b→B1 back over the fetched map and marks
//! the entry DIRTY — the dirty-authority law then persists the
//! resurrected freed binding (C2Lost + EIO), leaks the durable B2
//! (C2Leaked), and a later shadow record over the resurrection parks B1
//! for a SECOND free (the run17 double-free class → C3 + cross-file
//! corruption once the offset is reallocated).
//!
//! Contracts (RED against `d0451b4d`):
//! 1. **Durable merge supersedes the shadow**: a mid-epoch durable merge
//!    of a shadowed index evicts the epoch's stale binding — a cache
//!    eviction + refill must resolve the index to the DURABLE key, never
//!    the displaced-and-freed shadow key.
//! 2. **No resurrection double-free / leak**: the full field sequence
//!    (shadow record → durable merge displaces+frees B1 → eviction →
//!    rewrite → close) frees the durable B2 exactly once at the swap,
//!    never re-frees B1, and leaves every mapped offset
//!    allocator-tracked (the C2Lost face) with read-back exact.
//! 3. **Truncate supersedes shadowed indexes**: a mid-epoch truncate
//!    that prunes a shadowed index also drops the shadow binding — the
//!    refill must not resurrect a binding past the truncation point.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore default posture on scope exit (knob hygiene; the
/// `rewrite_shadow_tests` idiom). W1 patches are disabled so lone
/// whole-block overwrites ride the write-through pipeline.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::set_rewrite_shadow(true);
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
        squeezefs::device_overlay::clear_device_overlay_for_tests();
    }
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    alloc: Arc<BlockAllocator>,
    _backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make_harness(test_id: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    // B4c-ii: the overwrite OVERLAY (default ON) would take this
    // suite's mapped whole-block overwrites instead of the ACCUMULATION
    // shadow feed under test — lever OFF (LeverGuard restores; the
    // overlay-fed epoch laws are pinned in
    // tests/overlay_overwrite_tests.rs).
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(false);
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    squeezefs::meta_backend::kv::builder::format_v3(
        m.path(),
        128 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs: Arc::new(fs),
        req,
        alloc: ba,
        _backing: backing,
        _m: m,
        _s: s,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write");
}

async fn read_all(h: &H, ino: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, 0, len as u32, 0)
        .await
        .expect("read")
        .data
        .to_vec()
}

async fn quiesce(h: &H) {
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
}

/// Fresh striped fixture of `blocks` full blocks, drained + fsync'd
/// (the `rewrite_shadow_tests` idiom).
async fn striped_fixture(h: &H, name: &str, blocks: u64, seed: u8) -> u64 {
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new(name),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    write_at(h, ino, 0, &pattern((blocks * FBS) as usize, seed)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    quiesce(h).await;
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    assert_eq!(
        meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0),
        blocks as usize,
        "fixture premise: every block mapped"
    );
    ino
}

/// The RAM-authoritative map (what reads resolve through).
async fn ram_block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    (*h.fs
        .router
        .fetch_metadata(&path)
        .await
        .expect("meta")
        .block_map
        .expect("mapped"))
    .clone()
}

fn open_epochs() -> u64 {
    METRICS.rewrite_shadow_open_epochs.load(Ordering::Relaxed)
}

/// Mint a fresh published block on the harness's allocator and return
/// its backend-true key — the flush-leg upload's DMA product, minus the
/// device write (irrelevant to map/accounting coherence).
async fn mint_published_key(h: &H, template_key: &str) -> (String, u64) {
    let offset = h.alloc.allocate_block().await.expect("allocate");
    h.alloc.publish_block(offset);
    let (be_id, _) =
        h.fs.router
            .backend_router
            .parse_block_key(template_key)
            .expect("parse template key");
    (
        h.fs.router.backend_router.persist_block_key(&be_id, offset),
        offset,
    )
}

fn offset_of(h: &H, key: &str) -> u64 {
    h.fs.router
        .backend_router
        .parse_block_key(key)
        .expect("parse")
        .1
}

// ---------------------------------------------------------------------------
// Contract 1 — a mid-epoch durable merge supersedes the epoch's shadow
// binding: eviction + refill resolves to the durable key, never the
// displaced shadow key.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_merge_supersedes_stale_shadow_binding() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_supersede_refill").await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f1", blocks, 1).await;
    let map_a = ram_block_map(&h, ino).await;

    // Open the epoch: whole-block ACK-path overwrite of block 0 records
    // shadow[0] = B1 (RAM-only; durable still names A0).
    let ob0 = open_epochs();
    write_at(&h, ino, 0, &pattern(FBS as usize, 7)).await;
    quiesce(&h).await;
    assert_eq!(open_epochs() - ob0, 1, "premise: the epoch is open");
    let ram = ram_block_map(&h, ino).await;
    let b1 = ram.get(&0).expect("block 0 mapped").clone();
    assert_ne!(
        &b1,
        map_a.get(&0).unwrap(),
        "premise: shadow rebound block 0"
    );

    // The flush-leg durable publish of the SAME index: displaces B1 from
    // the RAM map and persists b→B2 (the `grow_size_to_block_end=false`
    // arm — durable NOW, never shadowed).
    let (b2, _b2_off) = mint_published_key(&h, &b1).await;
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    let displaced =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[(0, b2.clone())]),
                0,
                LayoutFlip::ToStripedKeepStagedIdentity,
                token,
            )
            .await
            .expect("durable merge");
    assert_eq!(displaced, vec![b1.clone()], "the merge displaced B1");
    // The flush leg frees its displaced key (upload_block_publish_phase's
    // caller discipline).
    h.fs.router
        .backend_router
        .free_block(&b1)
        .await
        .expect("free displaced B1");

    // Eviction + refill (KD-1.9 refetch-compose). The epoch is still
    // open — the compose must NOT resurrect the freed B1 over the
    // durable B2.
    h.fs.router.metadata_cache.remove(&ino);
    let refilled = ram_block_map(&h, ino).await;
    assert_eq!(
        refilled.get(&0),
        Some(&b2),
        "refill must resolve block 0 to the durable B2 — resurrecting the \
         displaced-and-freed shadow key B1 is the field's C2Lost/EIO face"
    );
}

// ---------------------------------------------------------------------------
// Contract 2 — the full field sequence: no double-free, no leak, every
// mapped offset stays allocator-tracked, read-back exact.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resurrection_sequence_cannot_double_free_or_leak() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_supersede_free_ledger").await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f2", blocks, 2).await;

    // Epoch open: shadow[0] = B1.
    write_at(&h, ino, 0, &pattern(FBS as usize, 8)).await;
    quiesce(&h).await;
    let b1 = ram_block_map(&h, ino).await.get(&0).unwrap().clone();

    // Mid-epoch durable merge of block 0 → B2; free the displaced B1.
    let (b2, b2_off) = mint_published_key(&h, &b1).await;
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    h.fs.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(0, b2.clone())]),
            0,
            LayoutFlip::ToStripedKeepStagedIdentity,
            token,
        )
        .await
        .expect("durable merge");
    h.fs.router
        .backend_router
        .free_block(&b1)
        .await
        .expect("free displaced B1");
    assert_eq!(
        h.alloc.refcount(b2_off),
        Some(1),
        "premise: B2 live and tracked after the durable merge"
    );

    // Eviction mid-epoch (the resurrection window), then the next
    // rewrite of block 0 — under the bug this parks the FREED B1 for a
    // second free; correct behavior parks B2.
    h.fs.router.metadata_cache.remove(&ino);
    let v_final = pattern(FBS as usize, 9);
    write_at(&h, ino, 0, &v_final).await;
    quiesce(&h).await;

    // Close the epoch (fsync trigger): the swap saves the map, then the
    // parked displaced keys free.
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync close");
    quiesce(&h).await;

    // The C2Leaked face: B2 was displaced by the final rewrite, so the
    // swap must have freed it — an allocated-but-unreferenced B2 is the
    // leak the field fsck reported.
    assert_eq!(
        h.alloc.refcount(b2_off),
        None,
        "B2 must be freed by the close (displaced by the final rewrite); \
         a lingering allocation is the field's C2Leaked face"
    );

    // The C2Lost face: every offset the final map binds must be
    // allocator-tracked (refcount >= 1). A map entry naming an untracked
    // offset is the deterministic-EIO poison.
    let final_map = ram_block_map(&h, ino).await;
    assert_eq!(final_map.len(), blocks as usize, "every block mapped");
    for (b, key) in &final_map {
        let off = offset_of(&h, key);
        assert!(
            h.alloc.refcount(off).is_some(),
            "block {b} (key {key}): mapped offset must stay allocator-tracked \
             — an untracked mapped offset is the field's C2Lost/EIO face"
        );
    }

    // Read-back exact (the silent-staleness face).
    let got = read_all(&h, ino, FBS as usize).await;
    assert_eq!(got, v_final, "block 0 serves the final rewrite's bytes");
}

// ---------------------------------------------------------------------------
// Contract 3 — a mid-epoch truncate prunes the epoch's shadow bindings
// past the cut: refill must not resurrect them.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_supersedes_shadowed_indexes_past_the_cut() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("shadow_supersede_truncate").await;
    let blocks = 4u64;
    let ino = striped_fixture(&h, "f3", blocks, 3).await;

    // Epoch open: shadow[3] = B1 (rewrite the LAST block).
    write_at(&h, ino, 3 * FBS, &pattern(FBS as usize, 11)).await;
    quiesce(&h).await;
    assert!(
        ram_block_map(&h, ino).await.contains_key(&3),
        "premise: block 3 mapped in RAM"
    );

    // Mid-epoch truncate to 2 blocks: prunes indexes 2 and 3 durably
    // (BlockMapOp::TruncateFrom through the one-merge primitive) and
    // frees the displaced keys.
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    let removed =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::TruncateFrom { new_size: 2 * FBS },
                2 * FBS,
                LayoutFlip::KeepLayout,
                token,
            )
            .await
            .expect("truncate");
    for key in &removed {
        h.fs.router
            .backend_router
            .free_block(key)
            .await
            .expect("free truncated key");
    }

    // Eviction + refill: the compose must not resurrect the pruned
    // (freed) shadow binding for block 3.
    h.fs.router.metadata_cache.remove(&ino);
    let refilled = ram_block_map(&h, ino).await;
    assert!(
        !refilled.contains_key(&3),
        "refill resurrected the truncated shadow binding for block 3 — a \
         freed key past the cut (the C2Lost face; extends silently corrupt)"
    );
}
