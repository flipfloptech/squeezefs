//! **The kvmap walkers — PR 4 (`feat/kvmap-walkers`)** of the PB-class
//! file ladder (`docs/design-kvmap-block-map-tree.md` §3 fsck/walkers,
//! §4 rung 4): the remaining maintenance-verb arms that string-match
//! `indirect:` on layout heads but not `kvmap:` — defrag's victim census
//! (`walk_striped_files`) and the jobs/evacuation census (`census_for` /
//! `drain_preflight`) — every one wired through the SHARED extraction
//! (`BackendRouter::kvmap_layout_entries`; an arm that re-implemented the
//! tree walk would only ever test the re-implementation, Rev 1.1 #3).
//!
//! Contracts pinned here:
//!
//! 1. **Defrag gauges compute over a kvmap volume.** The D2 locality walk
//!    sees a kvmap-headed striped file's tree-7 entries exactly as it
//!    sees an inline/indirect map (a head that yields zero entries reads
//!    as "no striped files", and `--report-only` silently under-measures).
//! 2. **A drain moves kvmap-mapped blocks and republishes their map
//!    records.** The evacuation census must enumerate a kvmap ino's
//!    blocks (pre-PR it saw none: the volume retired with LIVE data
//!    still on it — the exact catastrophe the census exists to prevent),
//!    `drain_preflight` must price them, and the `move_one` republish —
//!    which rides `merge_block_mappings`, whose sticky-head save IS the
//!    kvmap arm (design §3 publish, A10 force-kvmap) — must leave the
//!    tree naming the survivor with the C8 oracle clean.
//!
//! The fixtures are the kvmap_crossing_tests Rig (single volume) and the
//! volume_drain_tests mount-shaped fixture (two data volumes + fabric).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::jobs::{JobFabric, JobState, MoverCtx};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_RETIRED};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
/// Sparse on purpose (the crossing fixture's shape): the legs allocate
/// > 1000 offsets without writing most of them.
const DATA_LEN: u64 = 32 * 1024 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-00000000000000d4";
/// Enough mapped blocks to push the encoded map past the 64 KiB-node
/// volume's ~16 KiB xattr cap — the crossing trigger.
const SPILL_BLOCKS: u32 = 1200;

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
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// The single-volume Rig (the kvmap_crossing_tests fixture)
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

/// Format WITHOUT bit 16 / bit 9 (immune to the `SQUEEZEFS_TEST_STAMP_*`
/// seams), then stamp both explicitly — the crossing suite's shape.
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

    /// Allocate `n` blocks and bind them at indices `0..n` in ONE merge —
    /// the crossing trigger.
    async fn publish_spill(&self, ino: u64, n: u32) -> Vec<(u32, String)> {
        let mut entries: Vec<(u32, String)> = Vec::new();
        for b in 0..n {
            let offset = self.alloc.allocate_block().await.expect("allocate");
            self.alloc.publish_block(offset);
            entries.push((b, offset.to_string()));
        }
        self.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&entries),
                u64::from(n) * 4 * 1024 * 1024,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.token(ino),
            )
            .await
            .expect("merge a crossing map");
        entries
    }

    /// The durable layout head, decoded (bincode — kvmap/inline heads).
    async fn durable_head(&self, ino: u64) -> squeezefs::layout_wire::LayoutMetadata {
        let bytes = self
            .kv()
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout exists");
        bincode::deserialize(&bytes).expect("bincode head")
    }

    async fn shutdown(self) {
        self.routed.volumes[0]
            .shutdown()
            .await
            .expect("clean shutdown");
    }
}

// ===========================================================================
// 1. Defrag: the D2 locality walk sees a kvmap ino's tree-7 entries
// ===========================================================================

/// `defrag --report-only`'s D2 gauge computes over a kvmap volume: the
/// victim-census walk (`walk_striped_files`) must resolve a `kvmap:` head
/// through the shared extraction — a head that yields zero entries makes
/// the file invisible and the gauge a silent lie.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn defrag_d2_gauges_compute_over_a_kvmap_volume() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("kvmapped").await;
    rig.publish_spill(ino, SPILL_BLOCKS).await;
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1"),
        "fixture: the ino crossed"
    );

    let d2 = squeezefs::defrag::measure_d2(&rig.routed, &rig.router)
        .await
        .expect("D2 walk over a kvmap volume");
    assert!(
        d2.files >= 1,
        "the D2 locality walk must see the kvmap-headed striped file \
         (files = {}; a kvmap head resolving to zero entries is the \
         missing walker arm)",
        d2.files
    );
    assert!(
        d2.pairs >= u64::from(SPILL_BLOCKS) - 1,
        "every logically-adjacent pair of the kvmap map is measured \
         (pairs = {})",
        d2.pairs
    );
    // Sequentially allocated ascending offsets on one backend: local.
    assert!(
        d2.locality > 0.9,
        "the rig's map is same-backend ascending (locality = {})",
        d2.locality
    );
    rig.shutdown().await;
}

// ===========================================================================
// 2. Drain/evacuation: census prices, mover moves, republish stays kvmap
// ===========================================================================

const BLOCK: usize = 4096;
/// Past the 64 KiB-node inline cap with `oss2://off` keys (~27 B/entry
/// encoded vs the ~16 KiB cap).
const DRAIN_SPILL: u32 = 800;

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
        ..Default::default()
    }
}

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn base_format_config(data_lvs: &[&Path]) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK as u64,
        capacity: 1 << 34,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(
            data_lvs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    }
}

/// Restore `SQUEEZEFS_DEFAULT_BLOCK_SIZE` on every exit — the crossing
/// Rig legs in this file run block-size-default.
struct BlockSizeGuard;

impl Drop for BlockSizeGuard {
    fn drop(&mut self) {
        std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    }
}

/// The volume_drain_tests mount-shaped fixture: two data volumes, live
/// fabric with the mover wired, allocator census recovered like a mount.
struct DrainFx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<RoutedMetaBackend>,
    fabric: Arc<JobFabric>,
    _staging: TempDir,
}

async fn open_drain_fixture(meta: &Path, records: &[DataVolumeRecord]) -> DrainFx {
    let dlm = DlmClient::new().unwrap();
    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in records {
        if rec.state == VOL_STATE_RETIRED {
            continue;
        }
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());

    let kv = KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    for kv in &routed.volumes {
        for entry in fs.router.backend_router.backends.iter() {
            entry
                .value()
                .block_allocator
                .recover_active_blocks_v3(kv, &fs.router.backend_router)
                .await
                .expect("allocator recovery");
        }
    }
    let fs = Arc::new(fs);
    let fabric = JobFabric::start(
        routed.clone(),
        2,
        100,
        Some(MoverCtx::new(fs.router.clone(), fs.mover_quiesce_probe())),
    )
    .await
    .expect("fabric start");
    fs.job_fabric.store(Arc::new(Some(fabric.clone())));
    DrainFx {
        fs,
        meta: routed,
        fabric,
        _staging: staging,
    }
}

impl DrainFx {
    async fn close(self) {
        self.fabric.shutdown_abrupt().await;
        self.fs.router.backend_router.reclaim_drain().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

/// A drain of a volume holding kvmap-mapped blocks: the census prices
/// them, `move_one` moves them, and the republish — riding
/// `merge_block_mappings`' sticky-head kvmap arm — leaves the tree
/// naming the survivor, oracle clean. Pre-PR the census saw ZERO kvmap
/// blocks: the volume retired with live data still on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drain_moves_kvmap_blocks_and_republishes_the_map() {
    let _serial = serial();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let _bs = BlockSizeGuard;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    let cfg = base_format_config(&[&oss1, &oss2]);
    // 64 KiB nodes lower the inline cap so DRAIN_SPILL entries cross.
    format_v3(
        &meta,
        META_LEN,
        &FormatV3Options {
            node_size: 64 * 1024,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
    // Bit 9 so the C8 oracle grades the move's accounting (idempotent —
    // a default format may already carry it); bit 16 self-arms at the
    // crossing (finding 43).
    let _ = set_block_refcounts_bit(&meta).await.unwrap();
    let recs = cfg.resolved_data_volumes();
    let fx = open_drain_fixture(&meta, &recs).await;

    // A kvmap-headed ino whose EVERY block lives on the victim (oss2):
    // allocate + write + publish there, then bind in one crossing merge.
    let ino = fx
        .fs
        .create(
            req(),
            1,
            OsStr::new("kvmapped.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap()
        .attr
        .ino;
    let victim_be = fx
        .fs
        .router
        .backend_router
        .backends
        .get("oss2")
        .expect("oss2 registered")
        .value()
        .clone();
    let mut entries: Vec<(u32, String)> = Vec::new();
    for b in 0..DRAIN_SPILL {
        let off = victim_be.block_allocator.allocate_block().await.unwrap();
        let payload = vec![(b % 251) as u8; BLOCK];
        victim_be
            .device
            .write_block(off, bytes::Bytes::from(payload))
            .await
            .unwrap();
        victim_be.block_allocator.publish_block(off);
        entries.push((b, format!("oss2://{off}")));
    }
    let token = fx.fs.dlm().get_fencing_token_ino(ino);
    fx.fs
        .router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&entries),
            u64::from(DRAIN_SPILL) * BLOCK as u64,
            LayoutFlip::ToStripedKeepStagedIdentity,
            token,
        )
        .await
        .expect("crossing merge");
    let head_bytes = fx.meta.volumes[0]
        .getxattr(ino, "layout")
        .await
        .unwrap()
        .expect("layout exists");
    let head: squeezefs::layout_wire::LayoutMetadata =
        bincode::deserialize(&head_bytes).expect("bincode head");
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("kvmap:1"),
        "fixture: the ino crossed (self-armed bit 16)"
    );

    // The preflight must PRICE the kvmap blocks (the census walk arm).
    let pf = squeezefs::jobs::drain_preflight(
        &fx.meta,
        &MoverCtx::router_only(fx.fs.router.clone()),
        "oss2",
        2,
    )
    .await
    .expect("preflight census");
    assert!(
        pf.needed_bytes >= u64::from(DRAIN_SPILL) * BLOCK as u64,
        "the evacuation census must enumerate the kvmap ino's {DRAIN_SPILL} victim \
         blocks (needed_bytes = {}; zero means the kvmap walker arm is missing and \
         a drain would retire the volume with LIVE data on it)",
        pf.needed_bytes
    );

    let moved_before = METRICS.evacuate_blocks_moved.load(Ordering::Relaxed);

    // The online remove: preflight → durable draining → evacuation job.
    let job_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");
    let end = fx
        .fabric
        .wait_terminal(&job_id, std::time::Duration::from_secs(180))
        .await
        .expect("evacuation terminal");
    assert_eq!(end, JobState::Completed, "the drain must converge");

    // Every mapping republished onto the survivor, via the tree.
    let fetched = fx
        .fs
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("refetch (tree-resolved)");
    assert_eq!(
        fetched.block_map_id.as_deref(),
        Some("kvmap:1"),
        "the mover republish keeps the sticky kvmap head"
    );
    let map = fetched.block_map.as_deref().expect("tree-resolved map");
    assert_eq!(map.len(), DRAIN_SPILL as usize);
    for (b, mapping) in map.iter() {
        let clean = squeezefs::routing::clean_block_key(mapping);
        let (be_id, _off) = fx
            .fs
            .router
            .backend_router
            .parse_block_key(&clean)
            .expect("parse republished mapping");
        assert_ne!(
            be_id, "oss2",
            "block {b} still names the retired victim ({mapping}) — the census \
             missed it or the republish did not land"
        );
    }
    // Engagement: the evacuate ledger accounts for the kvmap blocks
    // (AGENTS: a drain row is INVALID unless the deltas account for the
    // victim's used bytes).
    let moved = METRICS.evacuate_blocks_moved.load(Ordering::Relaxed) - moved_before;
    assert!(
        moved >= u64::from(DRAIN_SPILL),
        "evacuate_blocks_moved ({moved}) must account for the {DRAIN_SPILL} kvmap blocks"
    );
    // The C8 oracle across the whole move: durable == derived.
    let drift = fx
        .fs
        .router
        .backend_router
        .verify_durable_block_refs(&fx.meta)
        .await
        .expect("oracle pass");
    assert!(
        drift.is_empty(),
        "the mover republish rode the kvmap arm's own transactions — zero drift \
         ({drift:?})"
    );
    fx.close().await;
}
