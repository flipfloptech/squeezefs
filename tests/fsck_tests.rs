//! PR VL6a — online report-only fsck, red-first
//! (docs/design-volume-lifecycle.md §5.6, KD-9, KD-17, gate G-VL-5 a–c):
//!
//! - **Seven check classes**: C1 meta node integrity (checksum ride-along
//!   bit-flip AND checksum-valid semantic damage), C2 block_map ↔
//!   allocator (leaked = allocated-unreferenced, lost =
//!   referenced-unallocated/out-of-range), C3 refcount verification
//!   (clone-aware, mover-ledger consulted), C4 orphan
//!   `active_block:`/`active_block_ext:` records, C5 staging generation
//!   validity, C6 capacity census drift, C7 data scrub (KD-17: AEAD on
//!   encrypted, frame decode on compressed, readability-only on plain).
//! - **Verify-before-report (KD-9)**: suspect → settle → re-check; for
//!   the cross-object classes C2/C3 the allocation-epoch filter
//!   (scan-latched side map, NOT the incarnation seqlock) whose two-epoch
//!   survival only ESCALATES to the in-flight allocation registry
//!   liveness check — normative order: **registry-absence FIRST, then
//!   re-verify reference state** (§5.6). Age alone is never a verdict.
//! - **FP-seeding (G-VL-5 a)**: a stalled flush unit and an R5-parked
//!   write (registry-held allocations spanning BOTH scan epochs) produce
//!   zero findings; dropping the owners makes the same shape a real
//!   finding — the exemption is load-bearing, not vacuous.
//! - **Registry TOCTOU (§5.6 check order)**: publish + deregister racing
//!   the final escalation through the pre-registry-check test hook
//!   produces no false positive.
//! - **`fsck_findings` = 0 on healthy volumes** — the tripwire.
//! - **Offline sharding**: `--shards k/N` ino-residue shards union
//!   (`merge_reports`) to the same findings as the full scan.
//! - **Throttle**: the KD-3 duty cycle stretches the scrub measurably.
//!
//! ## Measured census-walk baseline (G-VL-5 c — the floor derivation)
//!
//! `test_census_walk_baseline_measured` prints the measured census-walk
//! and fsck-scan rates on this fixture. Measured on the dev box
//! (2026-07-20, **release** build, file-backed tmp sandbox, 2,049 live
//! inodes, node cache warm): census walk = **1,142,885 inodes/s**
//! (0.002 s), fsck C1–C6 scan = **117,518 inodes/s** (0.017 s). The
//! instrument caveat is load-bearing (stated per §3, "every measurement
//! must state its instrument"): this walk is entirely RAM-authoritative
//! and cache-warm, so the scan's strictly-larger CPU work (THREE tree
//! walks with per-record decode + allocator cross-checks vs the walk's
//! one tree + one xattr read) shows at full magnitude — the ½-of-
//! baseline floor is defined against the RIG's device-backed census
//! (where the walk is I/O-bound and the scan's extra CPU hides behind
//! I/O), so here it is RECORDED, not asserted; the cargo assertion is a
//! loose collapse tripwire (scan ≥ census/20), and the gate-grade
//! ½-floor number rides the lifecycle rig + closing report.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{
    clear_pre_registry_check_hook, merge_reports, run as run_fsck, set_pre_registry_check_hook,
    volume_generation, FsckCtx, FsckOptions, FsckReport,
};
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_RETIRED};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::TempDir;

const BLOCK: usize = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 1000,
        gid: 1000,
        pid: 4321,
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
        meta_slot_map: None,
        meta_volumes: None,
    }
}

async fn format_meta_node(meta: &Path, data_lvs: &[&Path], node_size: usize) {
    let cfg = base_format_config(data_lvs);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

async fn format_meta(meta: &Path, data_lvs: &[&Path]) {
    format_meta_node(
        meta,
        data_lvs,
        squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
    )
    .await
}

/// Mount-shaped fixture (the VL4 drain-test shape): resolved volume
/// records drive `register_backend`, allocator refcount recovery runs
/// like a mount, and the fsck context points at the live meta + router.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    staging_path: PathBuf,
    _staging: TempDir,
}

async fn open_fixture(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new("local").unwrap();

    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), &first.id)
            .await
            .unwrap(),
    );
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }

    let staging = tempfile::tempdir().unwrap();
    let staging_path = staging.path().to_path_buf();
    let cache = TieredCache::new(
        vec![staging_path.clone()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
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
            .register_backend(rec, dlm.meta_client().clone())
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());
    router
        .backend_router
        .active_write_backend
        .store(Arc::new(first.id.clone()));

    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
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

    Fx {
        fs: Arc::new(fs),
        meta: routed,
        staging_path,
        _staging: staging,
    }
}

impl Fx {
    fn ctx(&self) -> FsckCtx {
        FsckCtx {
            meta: self.meta.clone(),
            router: self.fs.router.clone(),
            staging_dirs: vec![self.staging_path.clone()],
            expected_generation: None,
        }
    }

    fn default_allocator(&self, id: &str) -> Arc<BlockAllocator> {
        self.fs
            .router
            .backend_router
            .backends
            .get(id)
            .expect("backend registered")
            .value()
            .block_allocator
            .clone()
    }

    async fn close(self) {
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

/// Fast-settle online options (the engine still runs the full
/// suspect → settle → re-check machinery; tests just shrink the wall
/// clock).
fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = std::time::Duration::from_millis(100);
    o
}

async fn create_file(fx: &Fx, name: &str) -> u64 {
    fx.fs
        .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Striped burst: force striped layout, write `nblocks` distinct blocks,
/// fsync.
async fn striped_burst(fx: &Fx, ino: u64, nblocks: usize) -> Vec<u8> {
    let dummy = vec![0u8; BLOCK + 1];
    fx.fs
        .write(
            req(),
            ino,
            0,
            0,
            bytes::Bytes::copy_from_slice(&dummy),
            0,
            0,
        )
        .await
        .unwrap();
    let mut expected = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let data = vec![(b as u8) ^ 0xA7; BLOCK];
        expected.extend_from_slice(&data);
        fx.fs
            .write(
                req(),
                ino,
                0,
                (b * BLOCK) as u64,
                bytes::Bytes::copy_from_slice(&data),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("write of block {b} failed: {e:?}"));
    }
    fx.fs.fsync(req(), ino, 0, false).await.unwrap();
    expected
}

/// The ino's durable block mappings `(idx, mapping_string)`.
async fn block_mappings_of(fx: &Fx, ino: u64) -> Vec<(u32, String)> {
    let meta = fx
        .fs
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("layout");
    let mut out = Vec::new();
    if let Some(map) = meta.block_map.as_deref() {
        for (&b, mapping) in map {
            out.push((b, mapping.clone()));
        }
    }
    out.sort();
    out
}

fn assert_zero_findings(report: &FsckReport, what: &str) {
    assert!(
        report.findings.is_empty(),
        "{what}: fsck_findings must be 0 on a healthy volume (tripwire), got {:?}",
        report.findings
    );
}

// ---------------------------------------------------------------------------
// Baseline measurement (G-VL-5 c — VL6a's first implementation task)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_census_walk_baseline_measured() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // A metadata-heavy population: many inodes, a few carrying blocks.
    const INODES: usize = 2048;
    for i in 0..INODES {
        let ino = create_file(&fx, &format!("f{i}")).await;
        if i % 256 == 0 {
            striped_burst(&fx, ino, 2).await;
        }
    }

    // Census-walk baseline: the same inode-tree + layout walk `df` runs
    // (one paged range scan + one layout xattr read per ino).
    use squeezefs::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
    let t0 = std::time::Instant::now();
    let mut walked = 0u64;
    for kv in &fx.meta.volumes {
        let inodes = kv.trees()[0];
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            let page = inodes.range(&cursor, &end, 512).await.expect("walk");
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = squeezefs::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(local) = decode_inode_key(k) else {
                    continue;
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                if val.nlink == 0 {
                    continue;
                }
                let _ = kv.getxattr(local, "layout").await;
                walked += 1;
            }
        }
    }
    let census_secs = t0.elapsed().as_secs_f64().max(1e-9);
    let census_rate = walked as f64 / census_secs;

    // The fsck C1–C6 scan over the same population.
    let t1 = std::time::Instant::now();
    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let scan_secs = t1.elapsed().as_secs_f64().max(1e-9);
    let scan_rate = report.counters.inodes_scanned as f64 / scan_secs;

    println!(
        "BASELINE census walk: {walked} inodes in {census_secs:.3}s = {census_rate:.0} inodes/s"
    );
    println!(
        "BASELINE fsck scan:   {} inodes in {scan_secs:.3}s = {scan_rate:.0} inodes/s \
         (G-VL-5(c) floor = ½ census = {:.0} inodes/s)",
        report.counters.inodes_scanned,
        census_rate / 2.0
    );

    assert!(walked >= INODES as u64, "walk visited the dataset");
    assert!(
        report.counters.inodes_scanned >= INODES as u64,
        "fsck scanned the dataset"
    );
    assert_zero_findings(&report, "baseline population");
    // Loose sanity bound only — the gate-grade ½-floor number rides the
    // lifecycle rig / closing report (see the file header for the
    // recorded measurement and the reason this is not asserted exactly).
    assert!(
        scan_rate >= census_rate / 20.0,
        "fsck scan rate {scan_rate:.0}/s collapsed vs census baseline {census_rate:.0}/s"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Healthy volume ⇒ zero findings (the tripwire), counters engaged
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_healthy_volume_zero_findings() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // Striped + clone-shared + staged-partial content.
    let a = create_file(&fx, "a.bin").await;
    striped_burst(&fx, a, 8).await;
    let b = create_file(&fx, "b.bin").await;
    let a_tok = fx.fs.dlm().get_fencing_token_ino(a);
    let b_tok = fx.fs.dlm().get_fencing_token_ino(b);
    fx.fs
        .router
        .clone_file(
            &squeezefs::keys::inode_path(a),
            &squeezefs::keys::inode_path(b),
            Some(a_tok),
            Some(b_tok),
        )
        .await
        .expect("clone");
    let c = create_file(&fx, "c.bin").await;
    fx.fs
        .write(req(), c, 0, 0, bytes::Bytes::from(vec![7u8; 6000]), 0, 0)
        .await
        .unwrap();

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&report, "healthy mixed population");
    assert!(report.counters.inodes_scanned >= 3, "census engaged");
    assert!(report.counters.blocks_checked >= 8, "C2 engaged");
    assert!(report.counters.refcounts_checked >= 8, "C3 engaged");
    assert!(report.counters.nodes_walked >= 1, "C1 engaged");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C1 — both seeds
// ---------------------------------------------------------------------------

/// C1 seed 1: a bit-flipped node body — the checksum ride-along arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c1_bitflip_node_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    // Small nodes so the inode tree SPLITS (the corruption must land on
    // a non-root leaf: the root is validated at open, a corrupt root is
    // a refused mount — fsck's business is the walkable-but-damaged
    // interior).
    format_meta_node(&meta, &[&oss1], 64 * 1024).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    // Populate enough inodes to split the tree + clean shutdown
    // (checkpointed durable nodes).
    {
        let fx = open_fixture(&meta, &recs).await;
        for i in 0..4000 {
            create_file(&fx, &format!("f{i}")).await;
        }
        fx.close().await;
    }

    // Locate a durable LEAF of the inode tree, then flip bytes in it.
    let leaf_addr = {
        let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(&meta)
            .await
            .expect("probe open");
        let tree = kv.trees()[0];
        let root_addr = tree.root().addr;
        let leaf = tree
            .resolve_leaf(&squeezefs::meta_backend::kv::record::inode_key(500))
            .await
            .expect("leaf resolves");
        let addr = leaf.addr();
        assert_ne!(
            addr, root_addr,
            "the inode tree must have split (grow the dataset if this fires)"
        );
        kv.shutdown().await.expect("probe shutdown");
        addr
    };
    {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&meta)
            .unwrap();
        f.seek(SeekFrom::Start(leaf_addr + 32)).unwrap();
        let mut buf = [0u8; 64];
        f.read_exact(&mut buf).unwrap();
        for b in buf.iter_mut() {
            *b ^= 0xFF;
        }
        f.seek(SeekFrom::Start(leaf_addr + 32)).unwrap();
        f.write_all(&buf).unwrap();
        f.sync_all().unwrap();
    }

    // Reopen (probe posture, no recovery walk) and fsck offline.
    let fx = open_fixture_probe(&meta, &recs).await;
    let mut opts = FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(50);
    let report = run_fsck(&fx.ctx(), &opts).await.expect("fsck runs");
    assert!(
        report.findings.iter().any(|f| f.class == "C1"),
        "bit-flipped node must surface a C1 finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

/// Probe-shaped fixture: open WITHOUT allocator recovery (a corrupt
/// node must land as a C1 finding, not a fixture panic).
async fn open_fixture_probe(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), &first.id)
            .await
            .unwrap(),
    );
    let staging = tempfile::tempdir().unwrap();
    let staging_path = staging.path().to_path_buf();
    let cache = TieredCache::new(
        vec![staging_path.clone()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    for rec in records {
        router
            .backend_router
            .register_backend(rec, dlm.meta_client().clone())
            .await
            .unwrap();
    }
    router.backend_router.set_volume_records(records.to_vec());
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open_probe(meta)
        .await
        .expect("probe open");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    Fx {
        fs: Arc::new(fs),
        meta: routed,
        staging_path,
        _staging: staging,
    }
}

/// C1 seed 2: a checksum-VALID semantic-damage record — a
/// dentry-shaped (wrong-tree) key injected into the inodes tree via the
/// tree's own insert (bytes intact, structure wrong).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c1_semantic_wrong_tree_record_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    for i in 0..16 {
        create_file(&fx, &format!("f{i}")).await;
    }
    // Healthy control first.
    let clean = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&clean, "pre-seed control");

    // The seed: a 16-byte dentry-shaped key in the 8-byte-keyed inodes
    // tree (checksum-valid; only the schema is violated).
    let bad_key = squeezefs::meta_backend::kv::record::dentry_key(42, 7, 0);
    fx.meta.volumes[0].trees()[0]
        .insert(&bad_key[..], bytes::Bytes::from_static(b"bogus"))
        .await
        .expect("raw insert");

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        report.findings.iter().any(|f| f.class == "C1"),
        "wrong-tree record must surface a C1 finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C2 — leaked and lost
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c2_leaked_block_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "a.bin").await;
    striped_burst(&fx, ino, 4).await;

    // The seed: an allocation with NO live owner (no registry guard, no
    // reference) — the crash-leak shape.
    let alloc = fx.default_allocator(&recs[0].id);
    let leaked_off = alloc.allocate_block().await.expect("allocate");

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let leaked: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.class == "C2" && f.evidence.contains("leaked"))
        .collect();
    assert!(
        leaked
            .iter()
            .any(|f| f.object.contains(&leaked_off.to_string())),
        "leaked offset {leaked_off} must surface as a C2 leaked finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c2_lost_block_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "a.bin").await;
    striped_burst(&fx, ino, 4).await;

    // The seed: a mapping referencing an offset the allocator never
    // minted (referenced-unallocated — the lost class).
    let phantom_off = 1024u64 * BLOCK as u64 * 1024; // far past any allocation
    let phantom_key = fx
        .fs
        .router
        .backend_router
        .persist_block_key(&recs[0].id, phantom_off);
    let token = fx.fs.dlm().get_fencing_token_ino(ino);
    let entries = [(400u32, phantom_key.clone())];
    fx.fs
        .router
        .merge_block_mappings(
            ino,
            squeezefs::routing::BlockMapOp::Merge(&entries),
            0,
            squeezefs::routing::LayoutFlip::KeepLayout,
            token,
        )
        .await
        .expect("merge phantom mapping");

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.class == "C2" && f.evidence.contains("lost")),
        "phantom-referenced offset must surface as a C2 lost finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C3 — wrong refcount (clone-aware)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c3_wrong_refcount_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // A clone-shared population proves the checker is clone-AWARE
    // (refcount 2 with 2 referencers is healthy, not a finding).
    let a = create_file(&fx, "a.bin").await;
    striped_burst(&fx, a, 4).await;
    let b = create_file(&fx, "b.bin").await;
    let a_tok = fx.fs.dlm().get_fencing_token_ino(a);
    let b_tok = fx.fs.dlm().get_fencing_token_ino(b);
    fx.fs
        .router
        .clone_file(
            &squeezefs::keys::inode_path(a),
            &squeezefs::keys::inode_path(b),
            Some(a_tok),
            Some(b_tok),
        )
        .await
        .expect("clone");
    let clean = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&clean, "clone-shared healthy population");

    // The seed: a phantom extra reference on one shared block —
    // refcount 3, referencers 2.
    let mappings = block_mappings_of(&fx, a).await;
    let (_, victim_mapping) = mappings.first().expect("striped block exists");
    assert!(
        fx.fs
            .router
            .backend_router
            .increment_refcount(victim_mapping),
        "seed refcount bump"
    );

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        report.findings.iter().any(|f| f.class == "C3"),
        "refcount 3 vs 2 referencers must surface a C3 finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C4 — orphan active_block_ext record
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c4_orphan_staged_record_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "live.bin").await;
    striped_burst(&fx, ino, 2).await;
    let clean = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&clean, "pre-seed control");

    // The seed: a staged extent record whose ino has no meta.
    let orphan_key = squeezefs::keys::active_block_ext(9_999_991, 0).to_string();
    squeezefs::cache::nvme::seed_staged_custody_for_test(&fx.staging_path, &orphan_key)
        .await
        .expect("seed orphan custody");

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.class == "C4" && f.object.contains("9999991")),
        "orphan active_block_ext record must surface a C4 finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C5 — stale staging generation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c5_stale_staging_generation_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "live.bin").await;
    striped_burst(&fx, ino, 2).await;

    let expected_gen = volume_generation(&fx.meta);

    // Healthy: marker bound to the mounted generation.
    squeezefs::cache::nvme::write_staging_generation_marker(&fx.staging_path, &expected_gen)
        .await
        .expect("stamp");
    let mut ctx = fx.ctx();
    ctx.expected_generation = Some(expected_gen.clone());
    let clean = run_fsck(&ctx, &online_opts()).await.expect("fsck");
    assert_zero_findings(&clean, "generation-bound staging");

    // The seed: the marker rebound to a dead generation.
    squeezefs::cache::nvme::write_staging_generation_marker(
        &fx.staging_path,
        "v3:deadbeefdeadbeefdeadbeefdeadbeef",
    )
    .await
    .expect("restamp");
    let report = run_fsck(&ctx, &online_opts()).await.expect("fsck");
    assert!(
        report.findings.iter().any(|f| f.class == "C5"),
        "stale staging generation must surface a C5 finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C6 — capacity census drift
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c6_census_drift_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "a.bin").await;
    striped_burst(&fx, ino, 4).await;

    // The seed: a begin_free-limbo block — used-blocks accounting counts
    // it, the refcount population does not, and no finish_free ever
    // comes (the wedged-freer shape). Drift = 1 block, stable across
    // both scan epochs.
    let alloc = fx.default_allocator(&recs[0].id);
    let off = alloc.allocate_block().await.expect("allocate");
    assert!(alloc.begin_free(off), "terminal begin_free");
    // deliberately NO finish_free

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        report.findings.iter().any(|f| f.class == "C6"),
        "stable used-vs-tracked drift must surface a C6 finding, got {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C7 — the three scrub arms (KD-17)
// ---------------------------------------------------------------------------

fn test_pem() -> String {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let mut rng = rand::thread_rng();
    let key = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
    key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .unwrap()
        .to_string()
}

/// Corrupt the stored image of `ino`'s first striped block on the data
/// device; returns the corrupted offset.
async fn corrupt_first_block(fx: &Fx, ino: u64, dev_path: &Path) -> u64 {
    let mappings = block_mappings_of(fx, ino).await;
    let (_, mapping) = mappings.first().expect("striped block");
    let clean = mapping
        .find("://")
        .map(|p| {
            let rest = &mapping[p + 3..];
            format!(
                "{}://{}",
                &mapping[..p],
                rest.split(':').next().unwrap_or(rest)
            )
        })
        .unwrap_or_else(|| mapping.split(':').next().unwrap_or(mapping).to_string());
    let (_be, off) = fx
        .fs
        .router
        .backend_router
        .parse_block_key(&clean)
        .expect("parse key");
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev_path)
        .unwrap();
    // Flip bytes INSIDE the stored image body (past the 4-byte frame
    // header) so the corruption hits AEAD/frame payload, not just the
    // header.
    f.seek(SeekFrom::Start(off + 8)).unwrap();
    let mut buf = [0u8; 32];
    f.read_exact(&mut buf).unwrap();
    for b in buf.iter_mut() {
        *b ^= 0x5A;
    }
    f.seek(SeekFrom::Start(off + 8)).unwrap();
    f.write_all(&buf).unwrap();
    f.sync_all().unwrap();
    off
}

fn scrub_opts() -> FsckOptions {
    let mut o = online_opts();
    o.scrub = true;
    o
}

/// C7 arm 1: AEAD verification on an encrypted volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c7_aead_corruption_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    let pem = test_pem();
    fx.fs
        .router
        .set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "none".to_string(),
            "aes256gcm-rsa".to_string(),
            Some(&pem),
        ));

    let ino = create_file(&fx, "enc.bin").await;
    striped_burst(&fx, ino, 4).await;

    // Healthy: every block AEAD-verifies.
    let clean = run_fsck(&fx.ctx(), &scrub_opts()).await.expect("scrub");
    assert_zero_findings(&clean, "encrypted healthy scrub");
    assert!(
        clean.counters.scrub_aead_verified >= 4,
        "AEAD arm engaged: {:?}",
        clean.counters
    );
    assert_eq!(clean.counters.scrub_failures, 0);

    // The seed: corrupt one stored image body.
    corrupt_first_block(&fx, ino, &oss1).await;
    let report = run_fsck(&fx.ctx(), &scrub_opts()).await.expect("scrub");
    assert!(
        report.findings.iter().any(|f| f.class == "C7"),
        "AEAD-corrupted block must surface a C7 finding, got {:?}",
        report.findings
    );
    assert!(report.counters.scrub_failures >= 1);
    fx.close().await;
}

/// C7 arm 2: frame decode on a compressed volume (incl. the bit-31 raw
/// escape path — `striped_burst` writes constant bytes, so frames are
/// genuinely compressed; corruption breaks the decode).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c7_frame_corruption_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    fx.fs
        .router
        .set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "lz4".to_string(),
            "none".to_string(),
            None,
        ));

    let ino = create_file(&fx, "comp.bin").await;
    striped_burst(&fx, ino, 4).await;

    let clean = run_fsck(&fx.ctx(), &scrub_opts()).await.expect("scrub");
    assert_zero_findings(&clean, "compressed healthy scrub");
    assert!(
        clean.counters.scrub_frame_verified >= 4,
        "frame arm engaged: {:?}",
        clean.counters
    );

    corrupt_first_block(&fx, ino, &oss1).await;
    let report = run_fsck(&fx.ctx(), &scrub_opts()).await.expect("scrub");
    assert!(
        report.findings.iter().any(|f| f.class == "C7"),
        "frame-corrupted block must surface a C7 finding, got {:?}",
        report.findings
    );
    assert!(report.counters.scrub_failures >= 1);
    fx.close().await;
}

/// C7 arm 3: device read error on a plain volume (`scrub_readability_only`
/// is the honesty gauge). The error is injected at the scrub's device
/// read seam — the cargo-tier "dm-error or equivalent" (the file-backed
/// substrate cannot produce a real EIO: the uring read path completes
/// past-EOF reads at full size, so truncation is invisible here; the
/// root-privileged rig owns real-error-target coverage).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c7_read_error_detected() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 64 << 20);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "plain.bin").await;
    striped_burst(&fx, ino, 4).await;

    let clean = run_fsck(&fx.ctx(), &scrub_opts()).await.expect("scrub");
    assert_zero_findings(&clean, "plain healthy scrub");
    assert!(
        clean.counters.scrub_readability_only >= 4,
        "plain blocks are readability-only (the honesty gauge): {:?}",
        clean.counters
    );

    // The seed: EIO on one allocated block's read offset.
    let mappings = block_mappings_of(&fx, ino).await;
    let (_, m) = mappings.first().expect("striped block");
    let clean_key = m.split(':').next().unwrap_or(m).to_string();
    let (_, victim_off) = fx
        .fs
        .router
        .backend_router
        .parse_block_key(&clean_key)
        .expect("parse");
    squeezefs::fsck::set_scrub_read_fault_hook(Arc::new(move |off| off == victim_off));

    let report = run_fsck(&fx.ctx(), &scrub_opts()).await.expect("scrub");
    squeezefs::fsck::clear_scrub_read_fault_hook();
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.class == "C7" && f.evidence.contains("device read error")),
        "device read failure must surface a C7 finding, got {:?}",
        report.findings
    );
    assert!(report.counters.scrub_failures >= 1);
    // The healthy blocks still verified readable.
    assert!(report.counters.scrub_readability_only >= 3);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// G-VL-5(a) FP-seeding: stalled flush unit + R5-parked write spanning
// BOTH scan epochs ⇒ zero findings (the case age-only logic provably
// false-positives on)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_fp_seed_stalled_units_span_both_epochs_zero_findings() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "a.bin").await;
    striped_burst(&fx, ino, 4).await;

    // Two live owners in the retry-forever / R5-parked shape: allocated
    // BEFORE scan start (so the epoch filter can NOT exempt them), never
    // publishing across the whole run — exactly the FIND-M11-A /
    // parked_gate adversary. Their in-flight registry entries are the
    // ONLY thing between them and a false leaked finding.
    let alloc = fx.default_allocator(&recs[0].id);
    let stalled_flush_off = alloc.allocate_block().await.expect("allocate");
    let stalled_flush_guard = alloc.inflight_register(stalled_flush_off);
    let parked_write_off = alloc.allocate_block().await.expect("allocate");
    let parked_write_guard = alloc.inflight_register(parked_write_off);

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&report, "stalled flush unit + R5-parked write");
    assert!(
        report.counters.inflight_exempted >= 2,
        "both stalled owners must be registry-exempted, got {:?}",
        report.counters
    );

    // Prove the exemption was load-bearing: drop the owners (the units
    // died without publishing) and the SAME shape becomes real leaked
    // findings.
    drop(stalled_flush_guard);
    drop(parked_write_guard);
    let report2 = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let leaked = report2
        .findings
        .iter()
        .filter(|f| f.class == "C2" && f.evidence.contains("leaked"))
        .count();
    assert!(
        leaked >= 2,
        "dead owners' allocations must now surface as C2 leaked, got {:?}",
        report2.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// §5.6 registry TOCTOU: publish + deregister racing the final
// escalation (the normative registry-absence-FIRST check order)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_registry_toctou_publish_deregister_race_no_fp() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "a.bin").await;
    striped_burst(&fx, ino, 2).await;

    // An owner mid-flight: allocated + device-written, publish pending.
    let alloc = fx.default_allocator(&recs[0].id);
    let off = alloc.allocate_block().await.expect("allocate");
    let guard = alloc.inflight_register(off);
    let (_, dev) = fx
        .fs
        .router
        .backend_router
        .get_backend(&recs[0].id)
        .expect("backend");
    dev.write_block(off, bytes::Bytes::from(vec![0x42u8; BLOCK]))
        .await
        .expect("device write");
    alloc.publish_block(off);
    let key = fx
        .fs
        .router
        .backend_router
        .persist_block_key(&recs[0].id, off);

    // The barrier: at the exact moment fsck begins the FINAL escalation
    // for our offset (just BEFORE the registry-absence read), the owner
    // completes its publish (merge commit — durable AND visible to the
    // reads fsck performs) and only THEN deregisters (drops the guard) —
    // the §5.6 deregister-after-publish-visible contract. The normative
    // check order (registry-absence FIRST, then re-verify reference
    // state) is what makes this interleaving safe: the owner
    // deregistered before the registry read, so its publish is already
    // visible, and the post-registry reference re-read clears the
    // suspect. At scan time the offset was allocated-unreferenced —
    // without the escalation machinery this is exactly a false leaked
    // finding.
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let guard_slot = Arc::new(parking_lot::Mutex::new(Some(guard)));

    let fs2 = fx.fs.clone();
    let key2 = key.clone();
    let guard_slot2 = guard_slot.clone();
    let publisher = tokio::spawn(async move {
        // Park until the hook signals the escalation boundary.
        tokio::task::spawn_blocking(move || go_rx.recv())
            .await
            .expect("join")
            .expect("hook signal");
        let token = fs2.dlm().get_fencing_token_ino(ino);
        let entries = [(300u32, key2.clone())];
        fs2.router
            .merge_block_mappings(
                ino,
                squeezefs::routing::BlockMapOp::Merge(&entries),
                0,
                squeezefs::routing::LayoutFlip::KeepLayout,
                token,
            )
            .await
            .expect("publish");
        // Deregister ONLY after the publish committed (visible to the
        // reads fsck performs) — the registry contract.
        drop(guard_slot2.lock().take());
        done_tx.send(()).expect("done signal");
    });

    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_key = key.clone();
    let hook_fired = fired.clone();
    let done_rx = std::sync::Mutex::new(done_rx);
    set_pre_registry_check_hook(Arc::new(move |suspect_key: &str| {
        if suspect_key == hook_key && !hook_fired.swap(true, Ordering::SeqCst) {
            go_tx.send(()).expect("go signal");
            // Block THIS engine thread until publish + deregister landed
            // (the publisher runs on another runtime worker).
            done_rx
                .lock()
                .expect("poisoned")
                .recv()
                .expect("publish completed");
        }
    }));

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    clear_pre_registry_check_hook();
    publisher.await.expect("publisher join");
    assert!(
        fired.load(Ordering::SeqCst),
        "the suspect must have reached the final escalation (hook fired)"
    );
    assert_zero_findings(&report, "publish/deregister racing final escalation");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Mover-ledger exemption (C3 during an active mover)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mover_ledger_exempts_prepublish_refcounts() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let a = create_file(&fx, "a.bin").await;
    striped_burst(&fx, a, 2).await;

    // The §5.4-step-2 shape a live mover holds: dst refcount raised
    // ABOVE its published reference count, offset parked in the
    // pre-publish ledger. Simulated through the ledger's own seam (the
    // mover writes it through the same function).
    let mappings = block_mappings_of(&fx, a).await;
    let (_, mapping) = mappings.first().expect("block");
    assert!(fx.fs.router.backend_router.increment_refcount(mapping));
    let clean_key = mapping.split(':').next().unwrap_or(mapping).to_string();
    squeezefs::jobs::test_mover_ledger_insert(&clean_key);

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&report, "ledger-parked pre-publish refcount");
    assert!(
        report.counters.mover_ledger_exempted >= 1,
        "ledger exemption must be counted, got {:?}",
        report.counters
    );
    squeezefs::jobs::test_mover_ledger_remove(&clean_key);

    // Ledger cleared (task terminal): the same shape is now a C3
    // finding — §5.6 "a C3 violation on a ledger-listed offset after
    // task-terminal is a finding (and a mover bug)".
    let report2 = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        report2.findings.iter().any(|f| f.class == "C3"),
        "post-terminal phantom refcount must be a C3 finding, got {:?}",
        report2.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Offline shards + merge equivalence (k/N union == full scan)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_offline_shards_merge_equivalence() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    for i in 0..48 {
        let ino = create_file(&fx, &format!("f{i}")).await;
        if i % 8 == 0 {
            striped_burst(&fx, ino, 2).await;
        }
    }
    // Durable seeds that survive into every shard run: a C4 orphan and
    // a C5 stale generation.
    let orphan_key = squeezefs::keys::active_block_ext(9_999_992, 0).to_string();
    squeezefs::cache::nvme::seed_staged_custody_for_test(&fx.staging_path, &orphan_key)
        .await
        .expect("seed");
    squeezefs::cache::nvme::write_staging_generation_marker(
        &fx.staging_path,
        "v3:deadbeefdeadbeefdeadbeefdeadbeef",
    )
    .await
    .expect("restamp");

    let mut ctx = fx.ctx();
    ctx.expected_generation = Some(volume_generation(&fx.meta));

    let mut full_opts = FsckOptions::offline();
    full_opts.settle = std::time::Duration::from_millis(10);
    let full = run_fsck(&ctx, &full_opts).await.expect("full scan");
    assert!(
        full.findings.iter().any(|f| f.class == "C4")
            && full.findings.iter().any(|f| f.class == "C5"),
        "seeds visible to the full scan: {:?}",
        full.findings
    );

    const N: u32 = 3;
    let mut shard_reports: Vec<FsckReport> = Vec::new();
    for k in 0..N {
        let mut opts = FsckOptions::offline();
        opts.settle = std::time::Duration::from_millis(10);
        opts.shard = Some((k, N));
        shard_reports.push(run_fsck(&ctx, &opts).await.expect("shard scan"));
    }
    let merged = merge_reports(&shard_reports);

    let mut full_findings = full.findings.clone();
    full_findings.sort();
    let mut merged_findings = merged.findings.clone();
    merged_findings.sort();
    assert_eq!(
        full_findings, merged_findings,
        "k/N shard union must equal the full scan"
    );
    assert_eq!(
        merged.counters.inodes_scanned, full.counters.inodes_scanned,
        "every ino lands in exactly one shard"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Throttle adherence (KD-3 duty cycle stretches the scrub)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_scrub_throttle_stretches_duty_cycle() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    let ino = create_file(&fx, "big.bin").await;
    striped_burst(&fx, ino, 256).await;

    let t0 = std::time::Instant::now();
    let unthrottled = run_fsck(&fx.ctx(), &scrub_opts()).await.expect("scrub");
    let full_speed = t0.elapsed();
    assert_zero_findings(&unthrottled, "throttle fixture");
    assert!(unthrottled.counters.scrub_blocks_scanned >= 256);

    let mut throttled_opts = scrub_opts();
    throttled_opts.throttle_pct = 5;
    let t1 = std::time::Instant::now();
    let throttled = run_fsck(&fx.ctx(), &throttled_opts).await.expect("scrub");
    let slow = t1.elapsed();
    assert_zero_findings(&throttled, "throttled fixture");

    assert!(
        slow > full_speed * 2,
        "a 5% duty cycle must stretch the scrub (full {full_speed:?} vs throttled {slow:?})"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Report shape: JSON round-trip + merge dedupe
// ---------------------------------------------------------------------------

#[test]
fn test_report_json_roundtrip_and_merge_dedupe() {
    use squeezefs::fsck::{FsckCounters, FsckFinding};
    let f = FsckFinding {
        class: "C4".to_string(),
        object: "active_block_ext:inode_7:block_0".to_string(),
        evidence: "no live inode".to_string(),
        identity: None,
    };
    let r1 = FsckReport {
        schema: 1,
        mode: "offline".to_string(),
        shard: Some("0/2".to_string()),
        findings: vec![f.clone()],
        counters: FsckCounters {
            inodes_scanned: 3,
            findings: 1,
            ..Default::default()
        },
        partial: None,
        repair: None,
    };
    let r2 = FsckReport {
        schema: 1,
        mode: "offline".to_string(),
        shard: Some("1/2".to_string()),
        findings: vec![f.clone()],
        counters: FsckCounters {
            inodes_scanned: 4,
            findings: 1,
            ..Default::default()
        },
        partial: None,
        repair: None,
    };
    let json = serde_json::to_string(&r1).unwrap();
    let back: FsckReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.findings, r1.findings);

    // Cross-shard classes dedupe at merge: C1/C4/C5 findings repeated by
    // shards union to ONE finding.
    let merged = merge_reports(&[r1, r2]);
    assert_eq!(merged.findings, vec![f]);
    assert_eq!(merged.counters.inodes_scanned, 7);
    assert_eq!(merged.counters.findings, 1);
    assert!(merged.shard.is_none());
}
