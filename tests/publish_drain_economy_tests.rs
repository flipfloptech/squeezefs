//! Rewrite-publish-drain campaign (2026-08-01) — the **Phase 2 build**
//! contracts (`.benchmarks/2026-08-01-rewrite-publish-drain.md`).
//!
//! Phase 1 closed the rewrite publish tax on the field venue (EXA
//! rewrite, 32.4 GB/s, closed residue): publish 4.80 ms/block =
//! conveyor queue_wait 1.92 (echo) + meta_commit 2.23 — and the
//! meta_commit interior is commit_tx_wait 2.20 (tx_queue 0.74 +
//! journal-conveyor pass 0.78 [92 % utilized server] + fan-out wake
//! ~0.68) with the meta DEVICES at 1.5 % util / 0.3 ms w_await. The tax
//! is commit-rate-coupled queueing/scheduling on a 93 %-busy client —
//! NOT rewrite mechanics (the nj8qd2 probe ran the same rewrite at
//! publish 0.29 ms/block) and NOT the journal device. Two levers cut
//! the REAL work:
//!
//! **Lever A — era-guarded RAM-coherent publish base.** Every rewrite
//! publish pass today refetches its RMW base from the backend
//! (`publish_base_fetches` == passes; fresh streams dodge it via the
//! dirty-RAM rule): a getxattr that folds the ino's ENTIRE unrebased
//! delta chain (field: 6.3 M delta folds / 19.5 per pass), decodes and
//! clones the whole map — and the refetch RESETS the caller-half chain
//! accounting to 0, so the on-disk chain NEVER re-bases (field:
//! `publish_full_save_chain_cap` = 0 under rewrite, 211 MB/row of
//! meta-node writeback churn). The fix: a clean cached entry whose
//! `layout_base_token` matches the ino's CURRENT fencing era serves as
//! the RMW base (every layout mutation path republishes the cache
//! under `INODE_META_LOCKS`, so a same-era clean entry is coherent by
//! construction; a foreign-era entry — lease lost and reacquired —
//! refetches exactly as today: the cross-era stale-map hazard stays
//! closed).
//!
//! **Lever B — per-volume aggregated publish commits.** Delta-class
//! layout saves (`merge_layout_and_size`) enqueue on a per-volume
//! conveyor; a leader-elected pass drains what accumulated during the
//! previous commit (the jbd2/M7 no-timer shape), takes the batch's
//! I-guards in ONE deduped ascending `lock_many` plan, stages every
//! ino's {layout delta + inode Put} into ONE KvTx and commits ONCE —
//! one journal entry, one ring write, one fan-out — instead of one
//! commit per ino. Whole-tx atomicity per ino is preserved (each ino's
//! size+map still ride one record set inside one checksummed entry —
//! generic/795); per-op isolation is preserved (a NotFound/failed
//! member fails ALONE); the drain caps derive from the existing
//! `SQUEEZEFS_META_COMMIT_BATCH_{TXS,BYTES}` ring-admission caps (no
//! new constants); `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1` is the A/B
//! lever (the pre-campaign per-save commit path, verbatim).
//!
//! RED against 5ee266d: neither lever exists.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::routing::{
    set_layout_delta_chain_override, set_publish_coalesce_override,
    set_publish_commit_group_override, DataRouter, LayoutMetadata,
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
        set_publish_commit_group_override(None);
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _s: TempDir,
}

/// The publish_coalesce_tests harness (caller-owned meta file so
/// drop-and-reopen replay contracts are possible).
async fn make(uuid: [u8; 16], alloc_ns: &str, meta: &Path, format: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
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

/// Reopen a just-dropped meta volume, tolerating the D0 writer flock's
/// release latency (detached pass tasks drop their per-batch backend
/// refs within ms of the last user Arc — a bounded retry on `Busy` is
/// the drop-without-shutdown reopen discipline, not a sync-by-sleep).
async fn reopen_backend_with_retry(meta: &Path) -> RoutedMetaBackend {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match KvMetaBackend::open(meta).await {
            Ok(be) => return RoutedMetaBackend::new(vec![be]),
            Err(e)
                if format!("{e}").contains("writer lock")
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("reopen failed: {e}"),
        }
    }
}

async fn persisted_layout_at(routed: &RoutedMetaBackend, ino: u64) -> LayoutMetadata {
    let bytes = routed
        .getxattr(ino, "layout")
        .await
        .expect("getxattr")
        .expect("layout present");
    bincode::deserialize::<LayoutMetadata>(&bytes).expect("bincode layout")
}

/// Whole-block rewrite stream + fsync + pipeline drain.
async fn stream_blocks(h: &H, ino: u64, start: u32, blocks: u32, tag: u8) {
    for b in start..start + blocks {
        write_at(
            h,
            ino,
            b as u64 * BS,
            &pattern(BS as usize, tag ^ (b as u8)),
        )
        .await;
    }
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "pipeline must drain"
    );
}

// =========================================================================
// Lever A, contract 1 — the steady rewrite serves its RMW base from the
// era-coherent RAM entry: at most ONE backend fetch (the first pass
// after fsync cleaned the entry), every later pass a RAM serve; bytes,
// size, and the persisted fold stay exact across a remount.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewrite_publish_base_serves_from_coherent_ram() {
    let _g = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    set_publish_commit_group_override(None);
    // The rewrite-shadow epoch (Idea 1) records rewrite publishes
    // RAM-only — this contract pins the durable Lever A/B rewrite
    // publish machinery specifically (base provenance / chain re-base),
    // so the lever is off (tests/rewrite_shadow_tests.rs owns the epoch
    // venue). Restored by ShadowOff's drop.
    struct ShadowOff;
    impl Drop for ShadowOff {
        fn drop(&mut self) {
            squeezefs::routing::set_rewrite_shadow(true);
        }
    }
    let _so = ShadowOff;
    squeezefs::routing::set_rewrite_shadow(false);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pd-a1-ram-base!!", "pd_ns_a1", meta.path(), true).await;
    let ino = create(&h, "a1").await;

    let n = 12u32;
    stream_blocks(&h, ino, 0, n, 0x21).await; // fresh (dirty-RAM base)

    // fsync persisted + cleaned the layout; the rewrite premise.
    let fetch_0 = METRICS.publish_base_fetches.load(Ordering::Relaxed);
    let ram_0 = METRICS.publish_base_ram_serves.load(Ordering::Relaxed);
    let batches_0 = METRICS.layout_publish_batches.load(Ordering::Relaxed);

    stream_blocks(&h, ino, 0, n, 0x22).await; // rewrite pass 1
    stream_blocks(&h, ino, 0, n, 0x23).await; // rewrite pass 2

    let fetch = METRICS.publish_base_fetches.load(Ordering::Relaxed) - fetch_0;
    let ram = METRICS.publish_base_ram_serves.load(Ordering::Relaxed) - ram_0;
    let batches = METRICS.layout_publish_batches.load(Ordering::Relaxed) - batches_0;

    assert!(batches >= 2, "fixture premise: rewrite passes must publish");
    assert!(
        fetch <= 1,
        "the rewrite may pay AT MOST one base fetch (the first pass after \
         the fsync clean); got {fetch} of {batches} passes — the coherent \
         RAM base is not engaging"
    );
    assert!(
        ram >= batches - 1,
        "every later pass must RAM-serve its base (ram {ram} vs batches {batches})"
    );

    // Correctness: the served bytes and the durable fold match the last
    // rewrite exactly, through a remount.
    let expect: Vec<u8> = (0..n)
        .flat_map(|b| pattern(BS as usize, 0x23 ^ (b as u8)))
        .collect();
    let got = read_at(&h, ino, 0, (n as u64 * BS) as usize).await;
    assert_eq!(got, expect, "rewrite bytes must serve exactly");
    let before = persisted_layout(&h, ino).await;
    drop(h);
    let reopened = reopen_backend_with_retry(meta.path()).await;
    let after = persisted_layout_at(&reopened, ino).await;
    assert_eq!(after.size, before.size, "remounted size must match");
    assert_eq!(
        after.block_map, before.block_map,
        "remounted map must fold identically"
    );
}

// =========================================================================
// Lever A, contract 2 — the on-disk chain RE-BASES: with the coherent
// RAM base the caller-half chain accounting survives across passes, so
// sustained rewrite pays a chain-cap full save every C deltas (today's
// per-pass refetch resets the chain to 0 and the on-disk chain grows
// unbounded until node compaction).
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewrite_chain_rebases_at_the_cap() {
    let _g = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_publish_commit_group_override(None);
    set_layout_delta_chain_override(Some(4)); // C = 4: cadence visible fast
                                              // The rewrite-shadow epoch (Idea 1) records rewrite publishes
                                              // RAM-only — this contract pins the durable Lever A/B rewrite
                                              // publish machinery specifically (base provenance / chain re-base),
                                              // so the lever is off (tests/rewrite_shadow_tests.rs owns the epoch
                                              // venue). Restored by ShadowOff's drop.
    struct ShadowOff;
    impl Drop for ShadowOff {
        fn drop(&mut self) {
            squeezefs::routing::set_rewrite_shadow(true);
        }
    }
    let _so = ShadowOff;
    squeezefs::routing::set_rewrite_shadow(false);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pd-a2-chaincap!!", "pd_ns_a2", meta.path(), true).await;
    let ino = create(&h, "a2").await;

    let n = 4u32;
    stream_blocks(&h, ino, 0, n, 0x31).await;

    let cap_0 = METRICS.publish_full_save_chain_cap.load(Ordering::Relaxed);
    let delta_0 = squeezefs::meta_backend::kv::META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed);
    // 10 rewrite passes over the same blocks: enough delta saves to
    // cross the C=4 cap repeatedly.
    for pass in 0..10u8 {
        stream_blocks(&h, ino, 0, n, 0x40 ^ pass).await;
    }
    let cap_hits = METRICS.publish_full_save_chain_cap.load(Ordering::Relaxed) - cap_0;
    let deltas =
        squeezefs::meta_backend::kv::META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed) - delta_0;
    assert!(
        cap_hits >= deltas / 8,
        "sustained rewrite must re-base at the chain-cap cadence \
         (C=4; {deltas} delta saves but only {cap_hits} chain-cap full \
         saves — the chain accounting is being reset instead of deepened)"
    );
    // And the fold stays exact through it all.
    let expect: Vec<u8> = (0..n)
        .flat_map(|b| pattern(BS as usize, 0x40 ^ 9 ^ (b as u8)))
        .collect();
    let got = read_at(&h, ino, 0, (n as u64 * BS) as usize).await;
    assert_eq!(got, expect, "bytes after the re-base cadence");
}

// =========================================================================
// Lever A, contract 3 — the cross-era guard: a clean cached entry whose
// base token is NOT the ino's current fencing era must never serve as
// the RMW base (the lease-loss stale-map hazard) — the pass refetches
// from the backend and the poisoned RAM map does not leak into the
// persisted layout.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_era_ram_base_is_refused_and_refetched() {
    let _g = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    set_publish_commit_group_override(None);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pd-a3-era-guard!", "pd_ns_a3", meta.path(), true).await;
    let ino = create(&h, "a3").await;
    stream_blocks(&h, ino, 0, 4, 0x51).await;

    // Poison the RAM entry: clean, map present, but stamped with a
    // FOREIGN era token (a pre-lease-loss survivor) and a WRONG map
    // binding for block 0. A coherent-base serve would leak it.
    let mut entry =
        h.fs.router
            .metadata_cache
            .get(&ino)
            .expect("cached entry present after fsync");
    assert!(!entry.layout_dirty, "fixture premise: clean after fsync");
    let true_map = persisted_layout(&h, ino).await.block_map.unwrap();
    let mut poisoned = (*entry.block_map.take().expect("map present")).clone();
    poisoned.insert(0, "backend_0://66600000".to_string());
    entry.block_map = Some(Arc::new(poisoned));
    entry.layout_base_token = entry.layout_base_token.wrapping_add(1); // foreign era
    h.fs.router.metadata_cache.insert(ino, entry);

    let fetch_0 = METRICS.publish_base_fetches.load(Ordering::Relaxed);
    // Rewrite block 1 only: the publish pass must REFETCH (foreign era)
    // and base on the backend map — block 0's true binding survives.
    stream_blocks(&h, ino, 1, 1, 0x52).await;
    let fetch = METRICS.publish_base_fetches.load(Ordering::Relaxed) - fetch_0;
    assert!(
        fetch >= 1,
        "a foreign-era clean entry must never serve as the RMW base"
    );
    let after = persisted_layout(&h, ino).await;
    let map = after.block_map.as_ref().expect("map");
    assert_eq!(
        map.get(&0),
        true_map.get(&0),
        "the poisoned foreign-era binding must not leak into the persisted map"
    );
}

// =========================================================================
// Lever B, contract 1 — aggregation engagement: with the layout-merge
// pass held, k inos' delta saves accumulate and commit as ONE journal
// entry (one aggregated KvTx), every submitter resolves Ok, and every
// ino's durable fold carries its publish.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_ino_publishes_aggregate_into_one_commit() {
    let _g = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    set_publish_commit_group_override(None);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pd-b1-agg-one!!!", "pd_ns_b1", meta.path(), true).await;

    // k striped inos with persisted bases (delta-eligible).
    const K: usize = 4;
    let mut inos = Vec::new();
    for i in 0..K {
        let ino = create(&h, &format!("b1_{i}")).await;
        stream_blocks(&h, ino, 0, 2, 0x60 ^ i as u8).await;
        inos.push(ino);
    }

    let groups_0 = METRICS.publish_commit_groups.load(Ordering::Relaxed);
    let saves_0 = METRICS.publish_commit_group_saves.load(Ordering::Relaxed);

    // Hold the layout-merge pass so the k saves accumulate into one
    // drain (the TEST_PUBLISH_PASS_DELAY_MS pattern, backend edition).
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(120, Ordering::Relaxed);
    let ring_0 = {
        let routed = h.fs.meta_backend.as_ref().unwrap();
        routed.volumes[0].journal_ring().written_entries()
    };
    let mut joins = Vec::new();
    for (i, &ino) in inos.iter().enumerate() {
        let router = h.fs.router.clone();
        let token = h.fs.dlm().get_fencing_token_ino(ino);
        joins.push(tokio::spawn(async move {
            router
                .merge_block_mappings_coalesced(
                    ino,
                    vec![(5, format!("backend_0://{}", 0x500000 + i * 0x10000))],
                    6 * BS,
                    squeezefs::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                    token,
                )
                .await
        }));
    }
    for j in joins {
        j.await.unwrap().expect("aggregated publish must succeed");
    }
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(0, Ordering::Relaxed);

    let groups = METRICS.publish_commit_groups.load(Ordering::Relaxed) - groups_0;
    let saves = METRICS.publish_commit_group_saves.load(Ordering::Relaxed) - saves_0;
    let ring = {
        let routed = h.fs.meta_backend.as_ref().unwrap();
        routed.volumes[0].journal_ring().written_entries()
    } - ring_0;

    assert_eq!(saves, K as u64, "every save rides the aggregation ledger");
    assert!(
        groups < K as u64,
        "k concurrent-ino saves must aggregate (groups {groups} vs saves {saves})"
    );
    assert!(
        (ring as usize) < K,
        "k inos' publishes must commit in FEWER than k journal entries \
         (got {ring}) — the one-KvTx aggregation is not engaging"
    );
    // Durable folds carry every ino's publish.
    for (i, &ino) in inos.iter().enumerate() {
        let l = persisted_layout(&h, ino).await;
        let map = l.block_map.as_ref().expect("map");
        assert_eq!(
            map.get(&5),
            Some(&format!("backend_0://{}", 0x500000 + i * 0x10000)),
            "ino {ino} publish must be durable"
        );
        assert_eq!(l.size, 6 * BS, "ino {ino} size floor rides the same entry");
    }
}

// =========================================================================
// Lever B, contract 2 — per-op isolation: a batch member whose ino was
// destroyed (NotFound inode) fails ALONE; the surviving members commit
// and fold durably.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aggregated_batch_member_fails_alone() {
    let _g = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    set_publish_commit_group_override(None);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pd-b2-isolate!!!", "pd_ns_b2", meta.path(), true).await;

    let alive = create(&h, "b2_alive").await;
    stream_blocks(&h, alive, 0, 2, 0x71).await;
    let dead = create(&h, "b2_dead").await;
    stream_blocks(&h, dead, 0, 2, 0x72).await;
    // Destroy the second ino's inode record out from under its publish
    // (the reclaimed-ino face): backend-level unlink (nlink → 0; the
    // routing cache entry stays coherent so the op still rides the
    // delta/aggregated path) + the batched destroy reap.
    h.fs.meta_backend
        .as_ref()
        .unwrap()
        .unlink(1, "b2_dead")
        .await
        .expect("backend unlink");
    h.fs.meta_backend
        .as_ref()
        .unwrap()
        .destroy_inodes(&[dead])
        .await
        .expect("destroy");

    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(120, Ordering::Relaxed);
    let router = h.fs.router.clone();
    let t_alive = h.fs.dlm().get_fencing_token_ino(alive);
    let t_dead = h.fs.dlm().get_fencing_token_ino(dead);
    let ja = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .merge_block_mappings_coalesced(
                    alive,
                    vec![(7, "backend_0://770000".to_string())],
                    8 * BS,
                    squeezefs::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                    t_alive,
                )
                .await
        }
    });
    let jd = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .merge_block_mappings_coalesced(
                    dead,
                    vec![(7, "backend_0://780000".to_string())],
                    8 * BS,
                    squeezefs::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                    t_dead,
                )
                .await
        }
    });
    let (ra, rd) = (ja.await.unwrap(), jd.await.unwrap());
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(0, Ordering::Relaxed);

    ra.expect("the live member must commit");
    assert!(
        rd.is_err(),
        "the destroyed-ino member must fail ALONE (got {rd:?})"
    );
    let l = persisted_layout(&h, alive).await;
    assert_eq!(
        l.block_map.as_ref().unwrap().get(&7),
        Some(&"backend_0://770000".to_string()),
        "the surviving member's publish must be durable"
    );
}

// =========================================================================
// Lever B, contract 3 — replay: an aggregated multi-ino entry survives
// drop-without-shutdown (journal replay folds BOTH inos' publishes;
// whole-tx atomicity transfers to the aggregated shape verbatim).
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aggregated_commit_survives_replay() {
    let _g = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    set_publish_commit_group_override(None);

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pd-b3-replay!!!!", "pd_ns_b3", meta.path(), true).await;

    let i1 = create(&h, "b3_1").await;
    stream_blocks(&h, i1, 0, 2, 0x81).await;
    let i2 = create(&h, "b3_2").await;
    stream_blocks(&h, i2, 0, 2, 0x82).await;

    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(120, Ordering::Relaxed);
    let router = h.fs.router.clone();
    let t1 = h.fs.dlm().get_fencing_token_ino(i1);
    let t2 = h.fs.dlm().get_fencing_token_ino(i2);
    let j1 = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .merge_block_mappings_coalesced(
                    i1,
                    vec![(9, "backend_0://910000".to_string())],
                    10 * BS,
                    squeezefs::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                    t1,
                )
                .await
        }
    });
    let j2 = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .merge_block_mappings_coalesced(
                    i2,
                    vec![(9, "backend_0://920000".to_string())],
                    10 * BS,
                    squeezefs::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                    t2,
                )
                .await
        }
    });
    j1.await.unwrap().expect("i1 publish");
    j2.await.unwrap().expect("i2 publish");
    squeezefs::meta_backend::kv::backend::TEST_LAYOUT_MERGE_HOLD_MS.store(0, Ordering::Relaxed);

    // Drop WITHOUT shutdown: replay must fold both publishes. The
    // test-local router clone must drop FIRST (it pins the backend Arc
    // — and with it the D0 writer flock — across the reopen).
    drop(router);
    drop(h);
    let reopened = reopen_backend_with_retry(meta.path()).await;
    for (ino, key) in [(i1, "backend_0://910000"), (i2, "backend_0://920000")] {
        let l = persisted_layout_at(&reopened, ino).await;
        assert_eq!(
            l.block_map.as_ref().unwrap().get(&9),
            Some(&key.to_string()),
            "replayed aggregated entry must carry ino {ino}"
        );
        assert_eq!(l.size, 10 * BS, "size rides the same replayed record set");
    }
}

// =========================================================================
// Lever B, contract 4 — the A/B lever: `SQUEEZEFS_PUBLISH_COMMIT_GROUP_
// MAX=1` restores the per-save commit path verbatim (no aggregation
// ledger growth) and persists an identical layout.
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_max_one_is_the_per_save_commit_path() {
    let _g = serial().await;
    let _k = KnobGuard;
    set_publish_coalesce_override(None);
    set_layout_delta_chain_override(None);
    set_publish_commit_group_override(Some(1));

    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    let h = make(*b"pd-b4-ab-lever!!", "pd_ns_b4", meta.path(), true).await;
    let ino = create(&h, "b4").await;
    let groups_0 = METRICS.publish_commit_groups.load(Ordering::Relaxed);
    stream_blocks(&h, ino, 0, 6, 0x91).await;
    stream_blocks(&h, ino, 0, 6, 0x92).await;
    assert_eq!(
        METRICS.publish_commit_groups.load(Ordering::Relaxed) - groups_0,
        0,
        "group_max=1 must ride the pre-campaign per-save commit path"
    );
    let expect: Vec<u8> = (0..6u32)
        .flat_map(|b| pattern(BS as usize, 0x92 ^ (b as u8)))
        .collect();
    let got = read_at(&h, ino, 0, (6 * BS) as usize).await;
    assert_eq!(got, expect, "A/B path must persist identical bytes");
}
