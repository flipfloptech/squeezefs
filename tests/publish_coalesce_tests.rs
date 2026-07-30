//! Write-commit-economy campaign (2026-07-30) — the **per-ino publish
//! coalescing** contracts (lever 1,
//! `.benchmarks/2026-07-30-write-commit-economy.md`).
//!
//! The conviction: per-file commit serialization — every completed
//! 4 MiB block paid its own `INODE_META_LOCKS` → RMW → whole-layout
//! save cycle (~19 ms/publish/file at rewrite in the field), so
//! 39/40 admitted pipeline blocks sat parked awaiting commits. The
//! lever: publishes enqueue on a per-ino conveyor (the loom-modeled
//! `ConveyorCore` — the M7 pattern one level up); a leader-elected
//! detached pass drains whatever accumulated during the previous
//! commit (no timers) and persists the WHOLE batch as ONE commit.
//!
//! Contracts:
//! 1. Engagement + economy: N pipeline publishes ride the conveyor
//!    (`layout_publish_batched_blocks` accounts for every one), commit
//!    in FEWER batches than blocks, stage layout DELTAS, and the
//!    per-phase journal entries stay under one-per-block.
//! 2. Durability: fsync returns only after every acked block's mapping
//!    is durably published (there is no parked window an fsync can
//!    race — the op future resolves only after its batch committed).
//! 3. Per-op fencing: a stale-era op in a batch fails ALONE with
//!    `FencingTokenExpired` and publishes nothing; fresh members
//!    proceed (the supersession law, batch face).
//! 4. Equivalence: the coalesced+delta path and the pre-campaign
//!    direct path (`SQUEEZEFS_PUBLISH_COALESCE_MAX=1` +
//!    `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN=0`) persist IDENTICAL layouts
//!    across a remount.
//! 5. SIZE-NEVER-LEADS-DATA through replay: a drop-without-shutdown
//!    reopen folds a size that never exceeds its mapped coverage
//!    (size and map ride one checksummed journal entry).
//! 6. The REWRITE shape (the field conviction's 6.7 GB/s face): a
//!    second pass over published blocks coalesces identically,
//!    displaces EVERY prior binding (the purge path engaged), keeps
//!    size exact, serves the new bytes, and remounts to the same fold.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::kv::META_KV_LAYOUT_DELTA_COMMITS;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::routing::{
    set_layout_delta_chain_override, set_publish_coalesce_override, DataRouter, LayoutFlip,
    LayoutMetadata,
};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    // A real memory budget: without the 1 Hz sampler (not running in
    // tests) `MEM_BUDGET.budget_bytes()` reads 0, which clamps the write
    // pipeline's R5 cap to ONE block (depth_target = bs) — uploads
    // serialize and the coalescing under test is hidden (found by the
    // in-test depth_target probe; real mounts run GiB-class budgets).
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore the campaign knobs on scope exit (knob hygiene).
struct KnobGuard;
impl Drop for KnobGuard {
    fn drop(&mut self) {
        set_publish_coalesce_override(None);
        set_layout_delta_chain_override(None);
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _s: TempDir,
}

/// The `write_pipeline_tests` harness with the META file owned by the
/// CALLER, so drop-and-reopen (replay/remount contracts) is possible.
async fn make(uuid: [u8; 16], alloc_ns: &str, meta: &Path, format: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // Pin the W1 patch path OFF (the coverage-suite reason: downscaled
    // BS would make sub-block segments patch-eligible and bypass the
    // publish machinery under test).
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    if format {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid,
        })
        .unwrap()
        .build(meta, 128 * 1024 * 1024)
        .await
        .unwrap();
    }
    let be = KvMetaBackend::open(meta).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        req,
        _b: b,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
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
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// The persisted (backend-folded) layout — the durable truth.
async fn persisted_layout(h: &H, ino: u64) -> LayoutMetadata {
    let bytes =
        h.fs.meta_backend
            .as_ref()
            .unwrap()
            .getxattr(ino, "layout")
            .await
            .expect("getxattr")
            .expect("layout present");
    bincode::deserialize::<LayoutMetadata>(&bytes).expect("bincode layout")
}

/// A striped fixture with a persisted base: 2 blocks written + fsync
/// (map published, pipeline drained).
async fn striped_fixture(h: &H, name: &str) -> u64 {
    let ino = create(h, name).await;
    let base = pattern(2 * BS as usize, 0x11);
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "fixture pipeline must drain"
    );
    ino
}

// =========================================================================
// 1 + 2. Engagement, economy, and the fsync durability face.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_publishes_coalesce_and_fsync_is_durably_complete() {
    let _s = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pc-t1-coalesce!!", "pc_ns_t1", meta.path(), true).await;
    let ino = striped_fixture(&h, "t1").await;
    // The pass-delay seam reproduces the field's ms-scale commit latency
    // on this µs-commit sandbox: publishes that complete during a pass
    // MUST batch into the next one (restored below).
    squeezefs::routing::TEST_PUBLISH_PASS_DELAY_MS.store(5, Ordering::Relaxed);

    const N: u32 = 64;
    let ring_entries_0 = {
        let routed = h.fs.meta_backend.as_ref().unwrap();
        routed.volumes[0].journal_ring().written_entries()
    };
    let batched_0 = METRICS
        .layout_publish_batched_blocks
        .load(Ordering::Relaxed);
    let batches_0 = METRICS.layout_publish_batches.load(Ordering::Relaxed);
    let deltas_0 = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed);

    // Stream N whole blocks (block 2..2+N) CONCURRENTLY (the parallel
    // dio shape): each ACKs with its upload detached on the pipeline;
    // the conveyor batches whatever completes while the previous batch
    // commits.
    let payload = pattern(BS as usize, 0x42);
    let mut writers = Vec::new();
    for b in 2..2 + N {
        let fs = h.fs.clone();
        let req = h.req;
        let data = bytes::Bytes::copy_from_slice(&payload);
        writers.push(tokio::spawn(async move {
            let w = fs
                .write(req, ino, 0, b as u64 * BS, data, 0, 0)
                .await
                .unwrap();
            assert_eq!(w.written as u64, BS, "short write at block {b}");
        }));
    }
    for w in writers {
        w.await.unwrap();
    }
    // fsync = THE durability point: every acked byte's mapping must be
    // durably published when it returns.
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "pipeline must drain"
    );
    squeezefs::routing::TEST_PUBLISH_PASS_DELAY_MS.store(0, Ordering::Relaxed);

    let batched = METRICS
        .layout_publish_batched_blocks
        .load(Ordering::Relaxed)
        - batched_0;
    let batches = METRICS.layout_publish_batches.load(Ordering::Relaxed) - batches_0;
    let deltas = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed) - deltas_0;
    let entries = {
        let routed = h.fs.meta_backend.as_ref().unwrap();
        routed.volumes[0].journal_ring().written_entries() - ring_entries_0
    };
    println!(
        "phase: {batched} publishes in {batches} batches ({deltas} delta commits, \
         {entries} journal entries for {N} blocks)"
    );
    assert!(
        batched >= N as u64,
        "every pipeline publish must ride the conveyor \
         (layout_publish_batched_blocks {batched} < {N})"
    );
    assert!(
        deltas >= 1,
        "streaming publishes onto a persisted base must engage the delta path"
    );
    assert!(
        entries <= (N as u64) + 8,
        "one-entry-per-block is the ceiling; coalescing must never EXCEED it \
         (entries {entries} for {N} blocks)"
    );
    assert!(
        batches < batched,
        "concurrent publishes against ms-scale commits must coalesce \
         (got {batches} batches for {batched} publishes — the conveyor \
         regressed to the serialized per-block posture)"
    );

    // Durability face: the PERSISTED layout (backend fold, not RAM)
    // carries every acked block and the exact size.
    let layout = persisted_layout(&h, ino).await;
    let map = layout.block_map.as_ref().expect("map persisted");
    for b in 0..2 + N {
        assert!(
            map.contains_key(&b),
            "fsync returned but block {b}'s mapping is not durably published"
        );
    }
    assert_eq!(
        layout.size,
        (2 + N) as u64 * BS,
        "persisted size must be exactly the acked coverage"
    );

    // Read-back correctness through the published map.
    let got = read_at(&h, ino, 5 * BS, BS as usize).await;
    assert_eq!(got, payload, "read-back through the coalesced publish");
}

// =========================================================================
// 2b. The REWRITE shape (the field's 6.7 GB/s conviction face): a second
//     streaming pass over already-published blocks must coalesce
//     identically, DISPLACE every prior binding (each rewritten block's
//     key changes — the displaced-key purge engaged), keep size exact
//     (rewrite grows nothing), serve the NEW bytes, and remount to the
//     same folded truth.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewrite_pass_coalesces_and_displaces_every_prior_binding() {
    let _s = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    // This contract pins the CoW DISPLACEMENT rewrite's conveyor economy
    // (displaced-key purge per prior binding); the write-wall iteration-1
    // in-place default routes eligible whole-block rewrites at their own
    // offsets (no displacement — inplace_overwrite_tests owns that
    // venue), so the CoW-always lever pins this one.
    struct InplaceOn;
    impl Drop for InplaceOn {
        fn drop(&mut self) {
            squeezefs::fuse_client::set_inplace_overwrite(true);
        }
    }
    let _i = InplaceOn;
    squeezefs::fuse_client::set_inplace_overwrite(false);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pc-t1b-rewrite!!", "pc_ns_t1b", meta.path(), true).await;
    let ino = striped_fixture(&h, "t1b").await;

    const N: u32 = 48;
    // Fresh pass: stream N whole blocks and make them durable.
    let fresh = pattern(BS as usize, 0x42);
    let mut writers = Vec::new();
    for b in 2..2 + N {
        let fs = h.fs.clone();
        let req = h.req;
        let data = bytes::Bytes::copy_from_slice(&fresh);
        writers.push(tokio::spawn(async move {
            let w = fs
                .write(req, ino, 0, b as u64 * BS, data, 0, 0)
                .await
                .unwrap();
            assert_eq!(w.written as u64, BS, "short fresh write at block {b}");
        }));
    }
    for w in writers {
        w.await.unwrap();
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "fresh pipeline must drain"
    );
    let before = persisted_layout(&h, ino).await;
    let map_before = before.block_map.clone().expect("fresh map persisted");

    // Rewrite pass: same blocks, new bytes, concurrent (the parallel dio
    // shape), against ms-scale commits (the pass-delay seam).
    squeezefs::routing::TEST_PUBLISH_PASS_DELAY_MS.store(5, Ordering::Relaxed);
    let batched_0 = METRICS
        .layout_publish_batched_blocks
        .load(Ordering::Relaxed);
    let batches_0 = METRICS.layout_publish_batches.load(Ordering::Relaxed);
    let deltas_0 = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed);
    let rewrite = pattern(BS as usize, 0x77);
    let mut writers = Vec::new();
    for b in 2..2 + N {
        let fs = h.fs.clone();
        let req = h.req;
        let data = bytes::Bytes::copy_from_slice(&rewrite);
        writers.push(tokio::spawn(async move {
            let w = fs
                .write(req, ino, 0, b as u64 * BS, data, 0, 0)
                .await
                .unwrap();
            assert_eq!(w.written as u64, BS, "short rewrite at block {b}");
        }));
    }
    for w in writers {
        w.await.unwrap();
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "rewrite pipeline must drain"
    );
    squeezefs::routing::TEST_PUBLISH_PASS_DELAY_MS.store(0, Ordering::Relaxed);

    let batched = METRICS
        .layout_publish_batched_blocks
        .load(Ordering::Relaxed)
        - batched_0;
    let batches = METRICS.layout_publish_batches.load(Ordering::Relaxed) - batches_0;
    let deltas = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed) - deltas_0;
    println!("rewrite phase: {batched} publishes in {batches} batches ({deltas} delta commits)");
    assert!(
        batched >= N as u64,
        "every rewrite publish must ride the conveyor ({batched} < {N})"
    );
    assert!(
        batches < batched,
        "rewrite publishes against ms-scale commits must coalesce \
         (got {batches} batches for {batched} publishes)"
    );
    assert!(
        deltas >= 1,
        "rewrites onto a persisted base must engage the delta path"
    );

    // Displacement face: every rewritten block's binding CHANGED (the
    // pipeline uploads to fresh offsets; the batch apply displaced and
    // purged the prior key — a rewrite that reuses the old binding
    // never displaced anything and the purge path went untested).
    let after = persisted_layout(&h, ino).await;
    let map_after = after.block_map.clone().expect("rewritten map persisted");
    assert_eq!(
        map_before.keys().collect::<std::collections::BTreeSet<_>>(),
        map_after.keys().collect::<std::collections::BTreeSet<_>>(),
        "a pure rewrite maps exactly the same block set"
    );
    let changed = (2..2 + N)
        .filter(|b| map_before.get(b) != map_after.get(b))
        .count();
    assert_eq!(
        changed, N as usize,
        "every rewritten block must displace its prior binding \
         ({changed}/{N} changed — the displaced-key purge did not engage)"
    );
    assert_eq!(
        after.size, before.size,
        "a pure rewrite grows nothing (size must be exact)"
    );

    // The NEW bytes serve — through RAM and through the durable fold.
    let got = read_at(&h, ino, 7 * BS, BS as usize).await;
    assert_eq!(got, rewrite, "read-back must serve the rewritten bytes");

    // Remount equivalence: clean shutdown → reopen → the fold agrees.
    let routed = h.fs.meta_backend.clone().unwrap();
    drop(h);
    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
    drop(routed);
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let reopened = RoutedMetaBackend::new(vec![be]);
    let bytes = reopened
        .getxattr(ino, "layout")
        .await
        .expect("getxattr")
        .expect("layout present after remount");
    let refolded = bincode::deserialize::<LayoutMetadata>(&bytes).expect("layout");
    assert_eq!(refolded.size, after.size, "remounted size must match");
    assert_eq!(
        refolded.block_map.as_ref().expect("map"),
        &map_after,
        "the remounted fold must reconstruct the rewritten map exactly"
    );
    for vol in &reopened.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

// =========================================================================
// 3. Per-op fencing inside a batch (the supersession law, batch face).
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_token_op_fails_alone_and_publishes_nothing() {
    let _s = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pc-t2-fencing!!!", "pc_ns_t2", meta.path(), true).await;
    let ino = striped_fixture(&h, "t2").await;

    let current = h.fs.dlm().get_fencing_token_ino(ino);
    assert!(current > 0, "the write path must have acquired a lease");

    // A stale-era publish and two fresh ones, submitted concurrently.
    let router = h.fs.router.clone();
    let stale = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .merge_block_mappings_coalesced(
                    ino,
                    vec![(90, "backend_0://90000000".to_string())],
                    91 * BS,
                    LayoutFlip::ToStripedKeepStagedIdentity,
                    current - 1,
                )
                .await
        }
    });
    let fresh_a = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .merge_block_mappings_coalesced(
                    ino,
                    vec![(91, "backend_0://91000000".to_string())],
                    92 * BS,
                    LayoutFlip::ToStripedKeepStagedIdentity,
                    current,
                )
                .await
        }
    });
    let fresh_b = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .merge_block_mappings_coalesced(
                    ino,
                    vec![(92, "backend_0://92000000".to_string())],
                    93 * BS,
                    LayoutFlip::ToStripedKeepStagedIdentity,
                    current,
                )
                .await
        }
    });
    let stale_res = stale.await.unwrap();
    assert!(
        matches!(stale_res, Err(SqueezefsError::FencingTokenExpired { .. })),
        "the stale-era op must fail alone with FencingTokenExpired (got {stale_res:?})"
    );
    fresh_a.await.unwrap().expect("fresh op A must publish");
    fresh_b.await.unwrap().expect("fresh op B must publish");

    let layout = persisted_layout(&h, ino).await;
    let map = layout.block_map.as_ref().expect("map");
    assert!(
        !map.contains_key(&90),
        "a fenced op must publish NOTHING (the remount law, batch face)"
    );
    assert!(map.contains_key(&91) && map.contains_key(&92));
}

// =========================================================================
// 4. Remount equivalence: coalesced+delta ≡ direct full-Put.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coalesced_delta_and_direct_paths_persist_identical_layouts() {
    let _s = serial().await;

    async fn run_sequence(
        uuid: [u8; 16],
        ns: &str,
        meta: &Path,
        coalesce: Option<usize>,
        chain: Option<u32>,
    ) -> LayoutMetadata {
        let _k = KnobGuard;
        set_publish_coalesce_override(coalesce);
        set_layout_delta_chain_override(chain);
        let h = make(uuid, ns, meta, true).await;
        let ino = striped_fixture(&h, "eq").await;
        let payload = pattern(BS as usize, 0x37);
        for b in 2..34u32 {
            write_at(&h, ino, b as u64 * BS, &payload).await;
        }
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        assert!(h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await);
        // Cold remount: shut the volume down cleanly, reopen, fold.
        let routed = h.fs.meta_backend.clone().unwrap();
        drop(h);
        for vol in &routed.volumes {
            vol.shutdown().await.expect("shutdown");
        }
        drop(routed);
        let be = KvMetaBackend::open(meta).await.unwrap();
        let reopened = RoutedMetaBackend::new(vec![be]);
        let bytes = reopened
            .getxattr(ino, "layout")
            .await
            .expect("getxattr")
            .expect("layout present");
        let out = bincode::deserialize::<LayoutMetadata>(&bytes).expect("layout");
        for vol in &reopened.volumes {
            vol.shutdown().await.expect("shutdown");
        }
        out
    }

    let meta_a = NamedTempFile::new().unwrap();
    meta_a.as_file().set_len(128 * 1024 * 1024).unwrap();
    let meta_b = NamedTempFile::new().unwrap();
    meta_b.as_file().set_len(128 * 1024 * 1024).unwrap();

    // A: the campaign defaults (coalescing + deltas).
    let a = run_sequence(*b"pc-t3-eq-deltas!", "pc_ns_t3a", meta_a.path(), None, None).await;
    // B: the pre-campaign posture (serialized per-op, full Puts only).
    let b = run_sequence(
        *b"pc-t3-eq-direct!",
        "pc_ns_t3b",
        meta_b.path(),
        Some(1),
        Some(0),
    )
    .await;

    assert_eq!(a.file_type, b.file_type, "file_type must match");
    assert_eq!(a.size, b.size, "size must match");
    assert_eq!(a.block_map_id, b.block_map_id, "block_map_id must match");
    assert_eq!(a.file_id, b.file_id, "file_id must match");
    assert_eq!(a.data_key, b.data_key, "data_key must match");
    let (ma, mb) = (a.block_map.unwrap(), b.block_map.unwrap());
    assert_eq!(
        ma.keys().collect::<std::collections::BTreeSet<_>>(),
        mb.keys().collect::<std::collections::BTreeSet<_>>(),
        "the mapped block set must be identical across the two representations"
    );
}

// =========================================================================
// 5. SIZE-NEVER-LEADS-DATA through journal replay.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_size_never_exceeds_mapped_coverage() {
    let _s = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let ino;
    {
        let h = make(*b"pc-t4-replay!!!!", "pc_ns_t4", meta.path(), true).await;
        ino = striped_fixture(&h, "t4").await;
        let payload = pattern(BS as usize, 0x59);
        for b in 2..26u32 {
            write_at(&h, ino, b as u64 * BS, &payload).await;
        }
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        assert!(h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await);
        // Drop WITHOUT shutdown: the reopen must REPLAY the delta-bearing
        // journal entries (whole-tx atomicity — size rides its map).
    }
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let reopened = RoutedMetaBackend::new(vec![be]);
    let bytes = reopened
        .getxattr(ino, "layout")
        .await
        .expect("getxattr")
        .expect("layout must replay");
    let layout = bincode::deserialize::<LayoutMetadata>(&bytes).expect("layout");
    let map = layout.block_map.as_ref().expect("map");
    // Every byte inside the replayed size is backed by a mapping — size
    // can never lead its data's map (they share one journal entry).
    let blocks_needed = layout.size.div_ceil(BS) as u32;
    for b in 0..blocks_needed {
        assert!(
            map.contains_key(&b),
            "replayed size {} covers block {b} but the map does not \
             (size led its data across the crash boundary)",
            layout.size
        );
    }
    assert_eq!(
        layout.size,
        26 * BS,
        "the full acked coverage must replay (fsync'd bytes are durable)"
    );
    for vol in &reopened.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}
// =========================================================================
// 6. The conveyor batching contract, isolated at the router level
//    (deterministic: the pass-delay seam holds one pass open while the
//    concurrent submitters enqueue — they MUST ride the next batch).
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_router_publishes_batch_into_few_passes() {
    let _s = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pc-probe-batch!!", "pc_ns_probe", meta.path(), true).await;
    let ino = striped_fixture(&h, "probe").await;
    squeezefs::routing::TEST_PUBLISH_PASS_DELAY_MS.store(5, Ordering::Relaxed);
    let batches_0 = METRICS.layout_publish_batches.load(Ordering::Relaxed);
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let mut tasks = Vec::new();
    for b in 10..42u32 {
        let router = h.fs.router.clone();
        tasks.push(tokio::spawn(async move {
            router
                .merge_block_mappings_coalesced(
                    ino,
                    vec![(b, format!("backend_0://{}", b as u64 * 65536))],
                    (b as u64 + 1) * 65536,
                    LayoutFlip::ToStripedKeepStagedIdentity,
                    token,
                )
                .await
                .unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    squeezefs::routing::TEST_PUBLISH_PASS_DELAY_MS.store(0, Ordering::Relaxed);
    let batches = METRICS.layout_publish_batches.load(Ordering::Relaxed) - batches_0;
    println!("32 concurrent router-level publishes in {batches} batches");
    assert!(
        batches <= 8,
        "concurrent publishes against an in-flight pass must coalesce \
         (got {batches} batches for 32 ops)"
    );
}
