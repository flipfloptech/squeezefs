//! **The kvmap A2 background sweep — PR 6b (`feat/kvmap-sweep`)** of the
//! PB-class file ladder (`docs/design-kvmap-block-map-tree.md` §3
//! Truncate/unlink, §6 A2, §12 Rev 1.6): size-flip-first truncate/unlink
//! with a durable `;sweep:K` head cursor, `JobType::KvmapSweep` on the
//! job fabric, KD-6 crash-resume by cursor-head plan regeneration, and
//! the write-during-sweep extend barrier.
//!
//! Contracts pinned here:
//!
//! 1. **The O(1) truncate tx.** An over-threshold shrink commits ONLY
//!    the new size + the cursor head Put — journal entries never scale
//!    with the removed set; reads clamp (a cold refetch excludes the
//!    shadowed residue from RAM write authority).
//! 2. **The chunked sweep to completion.** Records gone, references
//!    released (oracle drift 0), frees enqueued, cursor cleared by the
//!    terminal chunk — one tx per chunk.
//! 3. **Crash-resume from chunk boundaries.** The durable cursor IS the
//!    plan (KD-6): a remount's adoption scan regenerates the job and the
//!    counted-run law holds — record deletions across all attempts sum
//!    to exactly the residue population (idempotent, no double frees).
//! 4. **The write-during-sweep contract.** An extend into the swept
//!    range barriers the re-exposed span at publish (residue released,
//!    cursor advanced) and the sweep never deletes a minted record (the
//!    per-chunk CURRENT-size floor, re-read under the held 4a).
//! 5. **The mid-sweep fsck exemption.** C11 (orphan + empty-head arms)
//!    reports NOTHING against a live cursor — `fsck_map_orphan_records`
//!    stays 0 with residue standing.
//! 6. **The corpse unlink.** `delete_file` over threshold keeps the
//!    inode record + head alive (nlink 0, `;sweep:0`); C9/C10/C11 stay
//!    clean mid-corpse; the job's terminal chunk performs the destroy.
//! 7. **Below-threshold byte-identity.** Small truncates/unlinks run
//!    today's synchronous paths verbatim (no cursor ever planted).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{run as run_fsck, FsckCtx, FsckOptions};
use squeezefs::fuse_client::METRICS;
use squeezefs::jobs::{adopt_kvmap_sweeps, JobFabric, JobSpec, JobState, JobType, MoverCtx};
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_map::{parse_kvmap_head, MapEntry};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{
    kvmap_sweep_threshold_blocks, BlockMapOp, DataRouter, KvmapSweepProgress, LayoutFlip,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
const DATA_LEN: u64 = 32 * 1024 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-000000000000006b";
const BLOCK: u64 = 4 * 1024 * 1024;

/// `SQUEEZEFS_MAP_MIGRATE_CHUNK=64` (the registry floor) derives the
/// threshold `64 × 64 = 4096` removed blocks — the whole suite's A/B
/// seam (no new knob exists; the threshold DERIVES, Rev 1.6).
const CHUNK: u64 = 64;
const THRESHOLD: u64 = CHUNK * 64;
/// Enough mapped blocks that a truncate-to-`LIVE` removes > THRESHOLD.
const SPILL: u32 = (THRESHOLD + 104) as u32;
/// The surviving prefix after the over-threshold truncate.
const LIVE: u32 = 40;

// ---------------------------------------------------------------------------
// Serialization (process-global METRICS deltas + env knob mutation)
// ---------------------------------------------------------------------------

static SERIAL_HELD: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        std::thread::yield_now();
    }
    std::env::set_var("SQUEEZEFS_MAP_MIGRATE_CHUNK", CHUNK.to_string());
    assert_eq!(kvmap_sweep_threshold_blocks(), THRESHOLD);
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        std::env::remove_var("SQUEEZEFS_MAP_MIGRATE_CHUNK");
        squeezefs::jobs::clear_kvmap_sweep_submit();
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// The Rig (the crossing suite's fixture + the in-process job fabric)
// ---------------------------------------------------------------------------

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format with bit 16 (kvmap) + bit 9 (durable refs — the C8 oracle
/// grades every sweep's accounting), immune to the `SQUEEZEFS_TEST_STAMP_*`
/// seams.
async fn format_meta_kvmap(path: &Path) {
    format_v3(path, META_LEN, &opts())
        .await
        .expect("format v3 meta volume");
    let VolumeFormat::V3(mut sb) = classify_volume(path).await.expect("classify") else {
        panic!("expected v3");
    };
    let strip = FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS | FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE;
    if sb.features_incompat & strip != 0 {
        sb.features_incompat &= !strip;
        write_superblock_v3(path, &sb).await.expect("strip seams");
    }
    assert!(set_block_refcounts_bit(path).await.expect("stamp bit 9"));
    assert!(set_block_map_tree_bit(path).await.expect("stamp bit 16"));
}

struct Rig {
    router: DataRouter,
    alloc: Arc<BlockAllocator>,
    routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

async fn mount(meta: &Path, data: &Path) -> Rig {
    let kv = KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv]));
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
    alloc.set_capacity_bytes(DATA_LEN);
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, alloc.clone(), nvme);
    router.set_meta_backend(routed.clone());
    Rig {
        router,
        alloc,
        routed,
        _staging: staging,
    }
}

fn data_file() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(DATA_LEN)
        .unwrap();
    f
}

impl Rig {
    fn kv(&self) -> &Arc<KvMetaBackend> {
        &self.routed.volumes[0]
    }

    async fn mk_file(&self, name: &str) -> u64 {
        self.routed
            .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    fn token(&self, ino: u64) -> u64 {
        self.router.dlm.get_fencing_token_ino(ino)
    }

    /// Bind `n` blocks at indices `0..n` in ONE merge. `sequential`
    /// offsets coalesce into RUN records (the 6a seam); reversed
    /// offsets stay one POINT record per index — the many-chunk shape
    /// the boundary matrix needs.
    async fn publish_spill(&self, ino: u64, n: u32, sequential: bool) -> Vec<(u32, String)> {
        let mut offsets = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let offset = self.alloc.allocate_block().await.expect("allocate");
            self.alloc.publish_block(offset);
            offsets.push(offset);
        }
        if !sequential {
            offsets.reverse();
        }
        let entries: Vec<(u32, String)> = offsets
            .into_iter()
            .enumerate()
            .map(|(b, o)| (b as u32, o.to_string()))
            .collect();
        self.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&entries),
                u64::from(n) * BLOCK,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.token(ino),
            )
            .await
            .expect("merge a crossing map");
        entries
    }

    async fn durable_head(&self, ino: u64) -> LayoutMetadata {
        let bytes = self
            .kv()
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout exists");
        bincode::deserialize(&bytes).expect("bincode head")
    }

    async fn sweep_cursor(&self, ino: u64) -> Option<u32> {
        parse_kvmap_head(
            self.durable_head(ino)
                .await
                .block_map_id
                .as_deref()
                .expect("kvmap head id"),
        )
        .expect("canonical head")
        .sweep_cursor
    }

    /// Every tree-7 record of `ino`, expanded to per-index key strings.
    async fn tree_records(&self, ino: u64) -> Vec<(u32, String)> {
        let default_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
        let mut out = Vec::new();
        let mut cursor = 0u32;
        loop {
            let page = self
                .kv()
                .block_map_range(ino, cursor, 512)
                .await
                .expect("tree scan");
            let Some(last) = page.last().map(|(i, _)| *i) else {
                break;
            };
            for (idx, entry) in page {
                match entry {
                    MapEntry::String(bytes) => {
                        out.push((idx, String::from_utf8(bytes).expect("utf8 key")))
                    }
                    MapEntry::Point { vol_tag, offset } => {
                        assert_eq!(vol_tag, default_tag);
                        out.push((idx, offset.to_string()));
                    }
                    MapEntry::Run {
                        vol_tag,
                        start_offset,
                        len,
                    } => {
                        assert_eq!(vol_tag, default_tag);
                        let stride = self.alloc.chunk_size();
                        for d in 0..len {
                            out.push((idx + d, (start_offset + u64::from(d) * stride).to_string()));
                        }
                    }
                    other => panic!("this rig never mints stamped records: {other:?}"),
                }
            }
            cursor = match last.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }
        out
    }

    /// The C8 oracle: durable-vs-derived, exact or drifting.
    async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.router
            .backend_router
            .verify_durable_block_refs(&self.routed)
            .await
            .expect("verification pass")
    }

    fn fsck_ctx(&self) -> FsckCtx {
        FsckCtx {
            meta: self.routed.clone(),
            router: self.router.clone(),
            staging_dirs: vec![],
            expected_generation: None,
        }
    }

    async fn fabric(&self) -> Arc<JobFabric> {
        JobFabric::start(
            self.routed.clone(),
            1,
            100,
            Some(MoverCtx::router_only(self.router.clone())),
        )
        .await
        .expect("fabric start")
    }

    async fn shutdown(self) {
        self.routed.volumes[0]
            .shutdown()
            .await
            .expect("clean shutdown");
    }
}

fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = Duration::from_millis(100);
    o
}

/// Run one `KvmapSweep` job for `ino` on `fabric` to a terminal state.
async fn run_sweep_job(fabric: &Arc<JobFabric>, ino: u64) -> JobState {
    let existing = fabric
        .jobs_matching(|t| matches!(t, JobType::KvmapSweep { ino: i } if *i == ino))
        .into_iter()
        .find(|(_, st)| !st.is_terminal());
    let job_id = match existing {
        Some((id, _)) => id,
        None => fabric
            .submit(JobSpec {
                job_type: JobType::KvmapSweep { ino },
                throttle_pct: 100,
            })
            .await
            .expect("submit sweep"),
    };
    fabric
        .wait_terminal(&job_id, Duration::from_secs(300))
        .await
        .expect("sweep terminal")
}

// ===========================================================================
// 1. The O(1) size-flip-first truncate (design §3/A2)
// ===========================================================================

/// An over-threshold shrink commits ONLY size + cursor (journal entries
/// never scale with the removed set), leaves every record standing,
/// prunes the RAM map, and a COLD refetch excludes the shadowed residue
/// (reads clamp to size — the cursor-law fetch exclusion).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_threshold_truncate_is_o1_and_plants_the_durable_cursor() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("giant").await;
    rig.publish_spill(ino, SPILL, true).await;
    assert_eq!(rig.tree_records(ino).await.len() as u32, SPILL);

    let entries_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    rig.router
        .truncate_layout(ino, u64::from(LIVE) * BLOCK, rig.token(ino))
        .await
        .expect("over-threshold truncate");
    let tx_delta = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - entries_before;
    assert!(
        (1..=2).contains(&tx_delta),
        "the size-flip-first handoff is O(1) — got {tx_delta} journal entries for a \
         {}-record removed set (A2: never O(removed))",
        SPILL - LIVE
    );

    // The durable plan: `;sweep:K`, K = the first removed index.
    assert_eq!(rig.sweep_cursor(ino).await, Some(LIVE));
    let head = rig.durable_head(ino).await;
    assert_eq!(head.size, u64::from(LIVE) * BLOCK);

    // The DURABLE record/ref work deferred: every record still stands,
    // every reference still held — and the oracle reads ZERO drift on
    // the mid-sweep state (residue is still owned until swept).
    assert_eq!(rig.tree_records(ino).await.len() as u32, SPILL);
    assert!(
        rig.drift().await.is_empty(),
        "mid-sweep state drifts nothing"
    );

    // Reads clamp: a COLD refetch's RAM write authority excludes the
    // shadowed residue (rehydrating it would let the next save's diff
    // re-assert deleted records over freed blocks).
    rig.router.metadata_cache.invalidate(&ino);
    let fetched = rig
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("refetch");
    assert_eq!(fetched.size, u64::from(LIVE) * BLOCK);
    let map = fetched.block_map.as_deref().expect("tree-resolved map");
    assert_eq!(
        map.len() as u32,
        LIVE,
        "the cold refetch carries the live prefix only — residue never re-enters RAM \
         write authority"
    );
    assert!(map.keys().all(|b| *b < LIVE));
    rig.shutdown().await;
}

/// Below the derived threshold, truncate AND unlink run today's
/// synchronous paths VERBATIM: no cursor is ever planted, records prune
/// in the call, the corpse registry stays empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn below_threshold_truncate_and_unlink_stay_verbatim() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;

    // Truncate: removed set (1160) < threshold (4096) ⇒ synchronous.
    let small = 1200u32;
    let ino = rig.mk_file("small").await;
    rig.publish_spill(ino, small, true).await;
    rig.router
        .truncate_layout(ino, u64::from(LIVE) * BLOCK, rig.token(ino))
        .await
        .expect("below-threshold truncate");
    let head = rig.durable_head(ino).await;
    assert_eq!(
        parse_kvmap_head(head.block_map_id.as_deref().unwrap())
            .expect("canonical head")
            .sweep_cursor,
        None,
        "below the threshold no cursor is ever planted"
    );
    assert_eq!(
        rig.tree_records(ino).await.len() as u32,
        LIVE,
        "the synchronous path pruned the records in the truncate itself"
    );
    assert!(rig.drift().await.is_empty());

    // Unlink: the bounded synchronous sweep, verbatim (PR 2's contract).
    let doomed = rig.mk_file("doomed-small").await;
    rig.publish_spill(doomed, small, true).await;
    rig.routed.unlink(1, "doomed-small").await.expect("unlink");
    rig.router
        .delete_file(&squeezefs::keys::inode_path(doomed))
        .await
        .expect("delete_file");
    assert!(
        rig.tree_records(doomed).await.is_empty(),
        "the bounded synchronous sweep leaves NO records"
    );
    assert!(
        !rig.router.kvmap_sweep_corpse_pending(doomed),
        "no corpse handoff below the threshold"
    );
    rig.routed.destroy_inodes(&[doomed]).await.expect("destroy");
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ===========================================================================
// 2. The chunked sweep to completion (the per-chunk one-tx law)
// ===========================================================================

/// The job drains the residue in throttled chunks: records gone,
/// references released (oracle drift 0), frees enqueued, cursor cleared
/// by the terminal chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sweep_job_drains_residue_and_clears_the_cursor() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("swept").await;
    let entries = rig.publish_spill(ino, SPILL, true).await;
    rig.router
        .truncate_layout(ino, u64::from(LIVE) * BLOCK, rig.token(ino))
        .await
        .expect("handoff");
    assert_eq!(rig.sweep_cursor(ino).await, Some(LIVE));

    let chunks_before = METRICS.map_sweep_chunks.load(Ordering::Relaxed);
    let records_before = METRICS.map_sweep_records.load(Ordering::Relaxed);
    let fabric = rig.fabric().await;
    assert_eq!(run_sweep_job(&fabric, ino).await, JobState::Completed);
    fabric.shutdown_abrupt().await;

    // Records: exactly the live prefix survives.
    let live: Vec<(u32, String)> = entries.into_iter().take(LIVE as usize).collect();
    assert_eq!(rig.tree_records(ino).await, live);
    assert_eq!(
        rig.sweep_cursor(ino).await,
        None,
        "terminal chunk cleared it"
    );
    assert!(
        METRICS.map_sweep_chunks.load(Ordering::Relaxed) > chunks_before,
        "chunk txs accounted"
    );
    assert!(
        METRICS.map_sweep_records.load(Ordering::Relaxed) > records_before,
        "swept records accounted"
    );

    // References: released with their Deletes — the oracle is exact.
    assert!(rig.drift().await.is_empty(), "post-sweep drift 0");

    // Frees: the swept offsets reached the allocator's free list once the
    // reclaim queue drained (RES-1: enqueued after each chunk's commit).
    rig.router.backend_router.reclaim_drain().await;
    let chunk_size = rig.alloc.chunk_size();
    let free = rig.alloc.free_block_indices();
    let probe = u64::from(LIVE + 1) * BLOCK / chunk_size; // block LIVE+1's offset (sequential spill)
    assert!(
        free.contains(&probe),
        "a swept offset must reach the free list (got {} free indices)",
        free.len()
    );
    rig.shutdown().await;
}

// ===========================================================================
// 3. Crash-resume from chunk boundaries (KD-6 + the counted-run law)
// ===========================================================================

/// Kill the sweep at a chunk boundary (chunks are the job's only commit
/// points — stopping between calls IS the kill-9 shape), remount, and
/// let the adoption scan regenerate the plan from the durable cursor.
/// Counted-run law: record deletions across ALL attempts sum to exactly
/// the residue population — idempotent, never a double free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_resume_regenerates_the_plan_from_the_durable_cursor() {
    let _serial = serial();
    // A REVERSED spill defeats run coalescing: one POINT per index, so
    // the residue spans many chunks and the boundary matrix is real.
    for boundary in [0usize, 1, 7] {
        let meta = NamedTempFile::new().unwrap();
        format_meta_kvmap(meta.path()).await;
        let data = data_file();
        let ino;
        let live_records;
        let records_before = METRICS.map_sweep_records.load(Ordering::Relaxed);
        {
            let rig = mount(meta.path(), data.path()).await;
            ino = rig.mk_file("boundary").await;
            let entries = rig.publish_spill(ino, SPILL, false).await;
            live_records = entries.len().min(LIVE as usize) as u64;
            rig.router
                .truncate_layout(ino, u64::from(LIVE) * BLOCK, rig.token(ino))
                .await
                .expect("handoff");
            for _ in 0..boundary {
                match rig
                    .router
                    .kvmap_sweep_chunk(ino, CHUNK as usize)
                    .await
                    .expect("chunk")
                {
                    KvmapSweepProgress::Progress { .. } => {}
                    other => panic!("boundary {boundary} exhausted early: {other:?}"),
                }
            }
            // CRASH: drop without shutdown — the cursor is the survivor.
            drop(rig);
        }
        let rig = mount(meta.path(), data.path()).await;
        let cursor = rig.sweep_cursor(ino).await;
        assert!(
            cursor.is_some(),
            "boundary {boundary}: the durable cursor survived the crash"
        );
        // KD-6: the mount-side cursor-head scan regenerates the job.
        let resumed_before = METRICS.map_sweep_resumed.load(Ordering::Relaxed);
        let fabric = rig.fabric().await;
        let adopted = adopt_kvmap_sweeps(&fabric, &rig.router)
            .await
            .expect("adoption scan");
        assert_eq!(adopted, 1, "boundary {boundary}: one plan regenerated");
        assert_eq!(
            METRICS.map_sweep_resumed.load(Ordering::Relaxed) - resumed_before,
            1
        );
        let (job_id, _) = fabric
            .jobs_matching(|t| matches!(t, JobType::KvmapSweep { ino: i } if *i == ino))
            .into_iter()
            .next()
            .expect("adopted job");
        assert_eq!(
            fabric
                .wait_terminal(&job_id, Duration::from_secs(300))
                .await
                .expect("terminal"),
            JobState::Completed,
            "boundary {boundary}"
        );
        fabric.shutdown_abrupt().await;

        assert_eq!(
            rig.tree_records(ino).await.len() as u64,
            live_records,
            "boundary {boundary}: only the live prefix survives"
        );
        assert_eq!(rig.sweep_cursor(ino).await, None);
        assert!(rig.drift().await.is_empty(), "boundary {boundary}: drift 0");
        // The counted-run law: deletions across BOTH attempts sum to the
        // residue exactly — a re-run never re-deletes (record-true) and
        // never double-frees (each key freed by the one chunk that
        // deleted its record).
        let swept_total = METRICS.map_sweep_records.load(Ordering::Relaxed) - records_before;
        assert_eq!(
            swept_total,
            u64::from(SPILL - LIVE),
            "boundary {boundary}: sweep deletions are exactly-once across the crash"
        );
        rig.shutdown().await;
    }
}

// ===========================================================================
// 4. The write-during-sweep contract (design §3's cursor law)
// ===========================================================================

/// A write/extend into the swept range: the publish's extend barrier
/// releases exactly the re-exposed residue and advances the cursor; the
/// background sweep — whose per-chunk floor re-reads the CURRENT size
/// under the held 4a — never deletes the record the write minted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_into_the_swept_range_never_deletes_minted_records() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("regrow").await;
    rig.publish_spill(ino, SPILL, true).await;
    rig.router
        .truncate_layout(ino, u64::from(LIVE) * BLOCK, rig.token(ino))
        .await
        .expect("handoff");
    assert_eq!(rig.sweep_cursor(ino).await, Some(LIVE));

    // The extend: a new block minted INSIDE the swept range, size grown
    // past the cursor.
    let minted_idx = LIVE + 60;
    let new_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(new_off);
    let minted = vec![(minted_idx, new_off.to_string())];
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&minted),
            u64::from(minted_idx + 1) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("extend into the swept range");

    // The barrier: the cursor advanced to the new size's floor, the
    // re-exposed span's residue is GONE (its stale records would have
    // become servable the instant the size committed), the minted
    // record stands, and the ledger is exact.
    assert_eq!(rig.sweep_cursor(ino).await, Some(minted_idx + 1));
    let now = rig.tree_records(ino).await;
    assert!(
        now.iter()
            .any(|(b, k)| *b == minted_idx && *k == new_off.to_string()),
        "the minted record survived the barrier"
    );
    assert!(
        !now.iter().any(|(b, _)| *b >= LIVE && *b < minted_idx),
        "the re-exposed span's residue is gone (stale data would resurrect under the \
         regrown size)"
    );
    assert!(rig.drift().await.is_empty(), "barrier releases are exact");

    // The refetch view: live prefix + the minted block, nothing stale.
    rig.router.metadata_cache.invalidate(&ino);
    let fetched = rig
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("refetch");
    let map = fetched.block_map.as_deref().expect("map");
    assert_eq!(map.len() as u32, LIVE + 1);
    assert_eq!(map.get(&minted_idx), Some(&new_off.to_string()));

    // The sweep completes over the remainder and NEVER touches the
    // minted record (the per-chunk CURRENT-size floor).
    let fabric = rig.fabric().await;
    assert_eq!(run_sweep_job(&fabric, ino).await, JobState::Completed);
    fabric.shutdown_abrupt().await;
    let after = rig.tree_records(ino).await;
    assert_eq!(after.len() as u32, LIVE + 1);
    assert!(after.iter().any(|(b, _)| *b == minted_idx));
    assert_eq!(rig.sweep_cursor(ino).await, None);
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ===========================================================================
// 5. The mid-sweep fsck exemption (C11's cursor law)
// ===========================================================================

/// A live cursor's residue is legitimate mid-plan state: fsck C11's
/// orphan and empty-head arms record NOTHING (`fsck_map_orphan_records`
/// stays 0 — the exemption's proof), and the whole pass is finding-free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mid_sweep_fsck_reports_nothing() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("audited").await;
    rig.publish_spill(ino, SPILL, true).await;
    rig.router
        .truncate_layout(ino, u64::from(LIVE) * BLOCK, rig.token(ino))
        .await
        .expect("handoff");
    // One chunk in — cursor advanced, residue partially standing: the
    // exact mid-plan shape a concurrent fsck must exempt.
    let _ = rig
        .router
        .kvmap_sweep_chunk(ino, CHUNK as usize)
        .await
        .expect("one chunk");
    assert!(rig.sweep_cursor(ino).await.is_some(), "still mid-plan");

    let rep = run_fsck(&rig.fsck_ctx(), &online_opts())
        .await
        .expect("fsck pass");
    assert_eq!(
        rep.counters.map_orphan_records, 0,
        "fsck_map_orphan_records stays 0 across a mid-sweep pass"
    );
    assert_eq!(rep.counters.map_empty_heads, 0);
    assert!(
        !rep.has_findings(),
        "mid-sweep residue under a live cursor is exempt-in-range: {:?}",
        rep.findings
    );
    rig.shutdown().await;
}

// ===========================================================================
// 6. The corpse unlink (design §12's named design point — landed shape)
// ===========================================================================

/// `delete_file` on an over-threshold kvmap ino keeps the inode record +
/// head ALIVE as a corpse (nlink 0, unreachable, `;sweep:0`) and the
/// sweep job's TERMINAL chunk performs the destroy. C9/C10/C11 all stay
/// clean on the mid-corpse state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_threshold_unlink_leaves_a_corpse_the_job_destroys() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("giant-doomed").await;
    rig.publish_spill(ino, SPILL, true).await;

    // Production reclaim order: unlink → delete_file; destroy is the
    // JOB's (the caller withholds it via the corpse registry).
    rig.routed.unlink(1, "giant-doomed").await.expect("unlink");
    rig.router
        .delete_file(&squeezefs::keys::inode_path(ino))
        .await
        .expect("delete_file takes the corpse handoff");

    // The corpse: record alive (nlink 0), head carries `;sweep:0`, size
    // flipped to 0, EVERY record + reference still standing, registry
    // marked — and the oracle reads zero drift (a corpse still owns its
    // blocks until swept: the corpse-census correction).
    let inode = rig.routed.getattr(ino).await.expect("corpse record alive");
    assert_eq!(inode.nlink, 0);
    assert_eq!(rig.sweep_cursor(ino).await, Some(0));
    assert_eq!(rig.durable_head(ino).await.size, 0);
    assert_eq!(rig.tree_records(ino).await.len() as u32, SPILL);
    assert!(rig.router.kvmap_sweep_corpse_pending(ino));
    assert!(rig.drift().await.is_empty(), "mid-corpse drift 0");

    // The mid-corpse fsck: C9 (nlink == 0 unnamed is outside its scope),
    // C10 (no name exists), C11 (live record + kvmap head) — all clean.
    let rep = run_fsck(&rig.fsck_ctx(), &online_opts())
        .await
        .expect("fsck pass");
    assert_eq!(rep.counters.map_orphan_records, 0);
    assert!(
        !rep.has_findings(),
        "the corpse is legitimate mid-plan state: {:?}",
        rep.findings
    );

    // The job: drains the records, then performs the terminal destroy
    // (record + xattrs, one tx — the C9 quarantine-then-destroy shape's
    // destroy half).
    let fabric = rig.fabric().await;
    assert_eq!(run_sweep_job(&fabric, ino).await, JobState::Completed);
    fabric.shutdown_abrupt().await;
    assert!(rig.tree_records(ino).await.is_empty());
    assert!(
        rig.routed.getattr(ino).await.is_err(),
        "the terminal chunk destroyed the corpse record"
    );
    assert!(!rig.router.kvmap_sweep_corpse_pending(ino));
    assert!(rig.drift().await.is_empty(), "post-destroy drift 0");

    // ...and the post-destroy fsck stays clean (no orphan records — the
    // destroy ran strictly after the tree emptied).
    let rep = run_fsck(&rig.fsck_ctx(), &online_opts())
        .await
        .expect("fsck pass");
    assert!(!rep.has_findings(), "{:?}", rep.findings);
    rig.shutdown().await;
}

// ===========================================================================
// 7. Shipped/claims trains vs a live cursor (the co-writer law)
// ===========================================================================

/// The kv-level entry_key closure for direct train calls: this rig's one
/// volume, bare-offset keys, 4 MiB stride.
fn entry_key_for_tests(entry: &MapEntry, delta: u32) -> Option<String> {
    match entry {
        MapEntry::String(b) if delta == 0 => String::from_utf8(b.clone()).ok(),
        MapEntry::Point { offset, .. } if delta == 0 => Some(offset.to_string()),
        MapEntry::Run {
            start_offset, len, ..
        } if delta < *len => Some((start_offset + u64::from(delta) * BLOCK).to_string()),
        _ => None,
    }
}

fn kvmap_head_bytes(size: u64) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".to_string(),
        size,
        block_map_id: Some("kvmap:1".to_string()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: None,
    })
    .expect("head encodes")
}

/// The claims-train face of the cursor law (a co-writer's shipped save
/// meeting a mid-sweep head on the OWNER): claims/desired indices
/// at/above the live cursor — or a size-raising ship — refuse
/// retried-class (`map_refused`'s class: re-exposing residue would serve
/// stale data); a claim strictly below composes and the flip head
/// carries the cursor forward verbatim. Beside it, the AUTHORITY gate:
/// the handoff itself never runs on a foreign-home ino
/// (`publishes_locally` — a co-writer's SETATTR ships and the owner's
/// own truncate execution plants the cursor).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claims_train_meeting_a_live_cursor_refuses_at_or_above_it() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("shared").await;
    let entries = rig.publish_spill(ino, 1200, true).await;
    // Plant the cursor through the backend seam (the handoff's own
    // commit): live map [0, LIVE), residue [LIVE, 1200).
    rig.routed
        .kvmap_truncate_handoff(ino, u64::from(LIVE) * BLOCK, LIVE, &[])
        .await
        .expect("cursor planted");

    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    let claim_at = |idx: u32| squeezefs::meta_backend::kv::backend::MapTrainClaims {
        base_gen: None,
        take: [idx].into_iter().collect(),
        release: std::collections::BTreeSet::new(),
        // The cursor-refusal law under test is the SHIPPED trains'
        // (§12b #5 / §14: local claims trains barrier instead).
        served: true,
    };
    // (a) A take AT/ABOVE the cursor refuses retried-class.
    let above = rig
        .kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(LIVE) * BLOCK),
            u64::from(LIVE) * BLOCK,
            &[],
            &[(
                LIVE + 5,
                MapEntry::Point {
                    vol_tag,
                    offset: 7 * BLOCK,
                },
            )],
            512,
            Some(&claim_at(LIVE + 5)),
            ino,
            &entry_key_for_tests,
            0,
            &|_key, _idx| None,
        )
        .await;
    let err = format!("{:?}", above.expect_err("shadowed-index claim refuses"));
    assert!(
        err.contains("kvmap sweep cursor"),
        "the refusal names the cursor law: {err}"
    );
    // (b) A size-RAISING ship refuses the same way (residue below the
    // resurrected size would become servable).
    let grow = rig
        .kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(LIVE + 100) * BLOCK),
            u64::from(LIVE + 100) * BLOCK,
            &[],
            &[],
            512,
            Some(&claim_at(1)),
            ino,
            &entry_key_for_tests,
            0,
            &|_key, _idx| None,
        )
        .await;
    let err = format!("{:?}", grow.expect_err("size-raising ship refuses"));
    assert!(err.contains("kvmap sweep cursor"), "{err}");
    // (c) A claim strictly BELOW the cursor composes, and the committed
    // flip head carries the cursor FORWARD (the publish-preserves-plan
    // law).
    let new_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(new_off);
    rig.kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(LIVE) * BLOCK),
            u64::from(LIVE) * BLOCK,
            &[],
            &[(
                3u32,
                MapEntry::Point {
                    vol_tag,
                    offset: new_off,
                },
            )],
            512,
            Some(&claim_at(3)),
            ino,
            &entry_key_for_tests,
            0,
            &|_key, _idx| None,
        )
        .await
        .expect("below-cursor claim composes")
        .expect("engaged tree");
    assert_eq!(
        rig.sweep_cursor(ino).await,
        Some(LIVE),
        "a committed claims train preserves the sweep plan verbatim"
    );
    // The residue population is untouched by the claims train (+1: the
    // adopted point SUPERSEDES inside the covering run — the §2 read
    // law's shape — so the raw expansion carries both).
    assert_eq!(rig.tree_records(ino).await.len(), entries.len() + 1);
    rig.shutdown().await;
}

// ===========================================================================
// 8. The handoff submit seam (the coordinator half of KD-6)
// ===========================================================================

/// A wired fabric receives the sweep job AT the truncate handoff (the
/// live-mount half; the durable cursor + adoption scan is the crash
/// half), and duplicate submits dedupe on the live job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_truncate_handoff_submits_the_job_through_the_wired_fabric() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("wired").await;
    rig.publish_spill(ino, SPILL, true).await;

    let fabric = rig.fabric().await;
    squeezefs::jobs::wire_kvmap_sweep_submit(&fabric);
    let jobs_before = METRICS.map_sweep_jobs.load(Ordering::Relaxed);
    rig.router
        .truncate_layout(ino, u64::from(LIVE) * BLOCK, rig.token(ino))
        .await
        .expect("handoff");
    assert_eq!(
        METRICS.map_sweep_jobs.load(Ordering::Relaxed) - jobs_before,
        1
    );
    // The submit hook spawns onto the meta lanes; the job lands and runs
    // to completion without any manual submit.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let job_id = loop {
        if let Some((id, _)) = fabric
            .jobs_matching(|t| matches!(t, JobType::KvmapSweep { ino: i } if *i == ino))
            .into_iter()
            .next()
        {
            break id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the wired handoff must submit the job"
        );
        squeezefs_ipc::sqz_time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        fabric
            .wait_terminal(&job_id, Duration::from_secs(300))
            .await
            .expect("terminal"),
        JobState::Completed
    );
    fabric.shutdown_abrupt().await;
    assert_eq!(rig.sweep_cursor(ino).await, None);
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}
