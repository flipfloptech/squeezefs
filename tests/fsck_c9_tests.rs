//! fsck detection class **C9 — unreferenced inodes**, red-first
//! (`docs/design-volume-lifecycle.md` §5.6/§5.6a for the ladder and the
//! repair posture; `docs/design-cow-kv-metadata.md` §4.10a for the
//! damage this class is owed against).
//!
//! An inode record that exists in `TREE_INODES` and is named by **no**
//! dentry in `TREE_DENTRIES`. Nothing else in the tree can see it: the
//! kernel never learned the ino exists, so it will never FORGET it, and
//! `reclaim_orphaned_batch` admits only `nlink == 0` FORGET'd inos;
//! C2/C3 ask "is this BLOCK referenced by nobody", never the same
//! question about an inode.
//!
//! Three shapes produce it (§4.10a "deliberately NOT converted"):
//!
//! 1. a cross-volume `create` crashed between its two commits — one
//!    inode record, `nlink == 1`, no dentry, no data blocks;
//! 2. **pre-S3.5 field damage**: a filesystem that ran the old code and
//!    crashed mid `link`/`unlink` across volumes carries this shape
//!    *plus every block the inode owned* ("invisible and unreclaimable…
//!    the inode and all its blocks leak permanently"). S3.5 stops new
//!    occurrences and does nothing about existing ones — this class is
//!    the only way an operator learns the volume carries such damage,
//!    and its repair is the only cleanup path;
//! 3. future S9 causes (a dead writer's in-flight create; a recovered
//!    cross-volume plan whose `MintInode` applied while its
//!    `InsertDentry` volume was fenced).
//!
//! Contracts under test:
//!
//! - **A prior-era unreferenced inode owning blocks is ONE C9
//!   finding** — and no block class fires alongside it, because the
//!   orphan's layout still *names* those blocks, so the allocator
//!   census agrees with the tree (the interaction statement).
//! - **`fsck_findings` = 0 on healthy volumes**, including across a
//!   remount (every inode prior-era, every inode named) — the standing
//!   tripwire a spurious new class would break.
//! - **The live-create false positive cannot fire**: a create
//!   legitimately has an inode record before its dentry, so the
//!   candidate filter is the **writer-era ino floor** (DLM S2's era,
//!   whose per-keyspace ino floor is captured at open) — an inode
//!   minted by THIS mount is never a candidate, and the same fixture's
//!   prior-era orphan IS reported, so the guard is load-bearing and not
//!   vacuous.
//! - **Repair (§5.6a posture)**: dry run plans and mutates nothing;
//!   apply quarantines the record + xattrs first, then frees the blocks
//!   through the ordinary terminal-free law and destroys the record in
//!   one journaled transaction — after which a re-scan is clean and the
//!   durable block-reference ledger (class C8) still shows zero drift.
//! - **Offline sharding**: a volume carrying exactly one unreferenced
//!   inode yields exactly ONE C9 finding across `merge-reports`, from
//!   exactly one shard (a shard filters dentries by their TARGET ino,
//!   so it holds the full referenced set for its own inode residue).
//! - **Creates racing a scan** (multi-thread) produce no C9 finding.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{
    merge_reports, repair as run_repair, run as run_fsck, FsckCtx, FsckOptions, FsckReport,
    RepairOptions,
};
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::record::DentryValue;
use squeezefs::meta_backend::Metadata;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_RETIRED};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
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

async fn format_meta(meta: &Path, data_lvs: &[&Path]) {
    let cfg = base_format_config(data_lvs);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        },
    )
    .await
    .expect("format v3 meta volume");
}

/// Mount-shaped fixture (VL6a's shape): resolved volume records drive
/// `register_backend`, the allocator refcount census is rebuilt from the
/// tree exactly as a mount rebuilds it, and the fsck context points at
/// the live meta + router.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    staging_path: PathBuf,
    _staging: TempDir,
}

async fn open_fixture(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BLOCK.to_string());
    let dlm = DlmClient::new().unwrap();

    let first = &records[0];
    let first_dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let first_alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
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

/// Fast-settle online options (the full suspect → settle → re-check
/// machinery still runs; only the wall clock shrinks).
fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = std::time::Duration::from_millis(100);
    o
}

fn offline_opts() -> FsckOptions {
    let mut o = FsckOptions::offline();
    o.settle = std::time::Duration::from_millis(10);
    o
}

fn dry_run() -> RepairOptions {
    RepairOptions {
        apply: false,
        quarantine_dir: None,
    }
}

fn apply() -> RepairOptions {
    RepairOptions {
        apply: true,
        quarantine_dir: None,
    }
}

async fn create_file(fx: &Fx, name: &str) -> u64 {
    fx.fs
        .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Striped burst: force the striped layout, write `nblocks` distinct
/// blocks, fsync (so the layout is durable and the blocks are real).
async fn striped_burst(fx: &Fx, ino: u64, nblocks: usize) {
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
    for b in 0..nblocks {
        let data = vec![(b as u8) ^ 0xA7; BLOCK];
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
}

/// The ino's durable block mappings `(idx, mapping)`.
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

/// **The damage fixture**: delete the dentry record that NAMES `ino`,
/// leaving its inode record (and, where it has them, its blocks and its
/// layout) behind — the pre-S3.5 DUR-7 residue verbatim, and the same
/// shape a crashed cross-volume `create` leaves.
///
/// Deleting the record through the dentry tree is what fsck's own C1
/// repair does; it leaves no trace in the tree that a name ever existed,
/// which is precisely the point.
async fn orphan_inode(fx: &Fx, ino: u64) {
    use squeezefs::meta_backend::kv::node::key_successor;
    use squeezefs::meta_backend::kv::tree::KEY_SPACE_MAX;
    for kv in &fx.meta.volumes {
        let dentries = kv.trees()[1];
        let mut cursor: Vec<u8> = vec![0u8];
        loop {
            let page = dentries
                .range(&cursor, &KEY_SPACE_MAX, 512)
                .await
                .expect("dentry walk");
            let Some((last, _)) = page.last() else { break };
            cursor = key_successor(last);
            for (k, v) in &page {
                if let Ok(d) = DentryValue::decode(v) {
                    if d.child_ino == ino {
                        dentries.delete(k).await.expect("drop the naming dentry");
                        return;
                    }
                }
            }
        }
    }
    panic!("no dentry names ino {ino}");
}

fn c9_findings(report: &FsckReport) -> Vec<&squeezefs::fsck::FsckFinding> {
    report.findings.iter().filter(|f| f.class == "C9").collect()
}

fn assert_zero_findings(report: &FsckReport, what: &str) {
    assert!(
        report.findings.is_empty(),
        "{what}: fsck_findings must be 0 on a healthy volume (tripwire), got {:?}",
        report.findings
    );
}

// ---------------------------------------------------------------------------
// Shape 2: a prior-era unreferenced inode that OWNS BLOCKS
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_prior_era_unreferenced_inode_with_blocks_is_one_c9_finding() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    // Healthy neighbours, so the class must discriminate.
    for i in 0..8 {
        let ino = create_file(&fx, &format!("keep{i}")).await;
        if i % 4 == 0 {
            striped_burst(&fx, ino, 2).await;
        }
    }
    let doomed = create_file(&fx, "damaged.bin").await;
    striped_burst(&fx, doomed, 4).await;
    let mappings = block_mappings_of(&fx, doomed).await;
    assert_eq!(mappings.len(), 4, "the fixture owns four blocks");
    orphan_inode(&fx, doomed).await;
    // The residue must be from a PRIOR mount (the era boundary is what
    // makes the class false-positive-free): close and re-open, exactly
    // like a crash + remount.
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let c9 = c9_findings(&report);
    assert_eq!(
        c9.len(),
        1,
        "exactly one unreferenced inode must be reported: {:?}",
        report.findings
    );
    assert!(
        c9[0].object.contains(&doomed.to_string()),
        "the finding names the orphan ino {doomed}: {:?}",
        c9[0]
    );
    assert!(
        c9[0].evidence.contains("no dentry"),
        "the evidence says what was observed: {:?}",
        c9[0]
    );
    // The interaction statement: an unreferenced inode's layout still
    // NAMES its blocks, so the allocator census agrees with the tree and
    // no block class fires. C9 is the only class that sees this damage.
    assert_eq!(
        report.findings.len(),
        1,
        "no block class may fire on an orphan whose layout still names its blocks: {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The tripwire: healthy across a remount ⇒ zero findings
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_healthy_volume_reports_zero_findings_across_a_remount() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    for i in 0..24 {
        let ino = create_file(&fx, &format!("f{i}")).await;
        if i % 6 == 0 {
            striped_burst(&fx, ino, 2).await;
        }
    }
    // A directory tree too: the root is named by NO dentry, and every
    // subdirectory's `.`/`..` are synthesized, never records — a naive
    // "no dentry names me" walk fires on all of them.
    fx.fs
        .mkdir(req(), 1, OsStr::new("d"), libc::S_IFDIR | 0o755, 0)
        .await
        .unwrap();
    let sub = fx
        .fs
        .lookup(req(), 1, OsStr::new("d"))
        .await
        .unwrap()
        .attr
        .ino;
    fx.fs
        .create(req(), sub, OsStr::new("nested"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "same-mount population",
    );
    fx.close().await;

    // Every inode is now PRIOR-era — the shape the class actually
    // examines — and every one of them is still named.
    let fx = open_fixture(&meta, &recs).await;
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "post-remount population (every inode prior-era)",
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The false positive that matters: a live create has an inode before it
// has a dentry
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_current_era_unreferenced_inode_is_never_reported() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    // Seed the PRIOR-era residue first (this is the control: the guard
    // must not be vacuous).
    let fx = open_fixture(&meta, &recs).await;
    let prior = create_file(&fx, "prior.bin").await;
    orphan_inode(&fx, prior).await;
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    // …and now the CURRENT-era shape: an inode this mount minted whose
    // name is not in the tree. That is byte-identical to the state every
    // in-flight create passes through, so it must never be reported.
    let current = create_file(&fx, "in-flight.bin").await;
    orphan_inode(&fx, current).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let c9 = c9_findings(&report);
    assert_eq!(
        c9.len(),
        1,
        "the prior-era orphan must be reported and the current-era one must not: {:?}",
        report.findings
    );
    assert!(
        c9[0].object.contains(&prior.to_string()),
        "the reported orphan is the PRIOR-era one ({prior}), not the current-era one \
         ({current}): {:?}",
        c9[0]
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Repair: dry run mutates nothing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c9_repair_dry_run_plans_and_mutates_nothing() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    let doomed = create_file(&fx, "damaged.bin").await;
    striped_burst(&fx, doomed, 2).await;
    let mappings = block_mappings_of(&fx, doomed).await;
    orphan_inode(&fx, doomed).await;
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    let alloc = fx.default_allocator(&recs[0].id);
    let offsets: Vec<u64> = mappings
        .iter()
        .map(|(_, m)| {
            fx.fs
                .router
                .backend_router
                .parse_block_key(m)
                .expect("parse")
                .1
        })
        .collect();

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(c9_findings(&report).len(), 1, "{:?}", report.findings);

    let plan = run_repair(&fx.ctx(), &report, &dry_run())
        .await
        .expect("plan");
    assert!(
        plan.dry_run
            && plan
                .planned
                .iter()
                .any(|a| a.class == "C9" && a.action == "destroy-unreferenced-inode"),
        "the plan names the §5.6a verb: {:?}",
        plan.planned
    );
    assert!(plan.applied.is_empty(), "a dry run applies nothing");
    for off in &offsets {
        assert!(
            alloc.refcount(*off).is_some(),
            "dry run must not free block {off}"
        );
    }
    assert!(
        fx.meta.getattr(doomed).await.is_ok(),
        "dry run must not destroy the inode record"
    );
    // Still detectable afterwards (nothing healed).
    let again = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(c9_findings(&again).len(), 1, "{:?}", again.findings);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Repair applied: the record is destroyed, the blocks are freed, the
// accounting closes, and the durable-reference ledger (C8) stays clean
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c9_repair_applied_frees_blocks_and_accounting_closes() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    // Engage the durable block-reference ledger (incompat bit 8) so the
    // C8 oracle grades this repair's accounting: a destroy that frees
    // blocks without releasing their durable references is exactly the
    // drift `meta_kv_block_refs_drift` must never show.
    std::env::set_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS", "1");
    format_meta(&meta, &[&oss1]).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS");
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    assert!(
        fx.meta.volumes[0].block_refs_engaged(),
        "the durable ledger must be engaged for this leg"
    );
    let keep = create_file(&fx, "keep.bin").await;
    striped_burst(&fx, keep, 2).await;
    let doomed = create_file(&fx, "damaged.bin").await;
    striped_burst(&fx, doomed, 4).await;
    let mappings = block_mappings_of(&fx, doomed).await;
    let keep_mappings = block_mappings_of(&fx, keep).await;
    orphan_inode(&fx, doomed).await;
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    let alloc = fx.default_allocator(&recs[0].id);
    let parse = |m: &String| -> u64 {
        fx.fs
            .router
            .backend_router
            .parse_block_key(m)
            .expect("parse")
            .1
    };
    let doomed_offsets: Vec<u64> = mappings.iter().map(|(_, m)| parse(m)).collect();
    let keep_offsets: Vec<u64> = keep_mappings.iter().map(|(_, m)| parse(m)).collect();

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(c9_findings(&report).len(), 1, "{:?}", report.findings);

    let rep = run_repair(&fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert!(
        rep.applied
            .iter()
            .any(|a| a.class == "C9" && a.action == "destroy-unreferenced-inode"),
        "applied: {:?} / refused: {:?}",
        rep.applied,
        rep.refused
    );
    // Quarantine-first: the record (and its xattrs — the layout that
    // names the blocks) were copied before anything was destroyed.
    assert!(
        rep.counters.quarantined_records >= 1 && rep.counters.quarantined_bytes > 0,
        "quarantine-first: {:?}",
        rep.counters
    );
    let qdir = rep.quarantine_dir.clone().expect("quarantine dir recorded");
    let manifest = std::fs::read_to_string(Path::new(&qdir).join("manifest.json"))
        .expect("quarantine manifest");
    assert!(
        manifest.contains("C9") && manifest.contains(&doomed.to_string()),
        "the manifest records the destroyed identity: {manifest}"
    );

    // The record is gone and its blocks were freed through the ordinary
    // terminal-free law.
    assert!(
        fx.meta.getattr(doomed).await.is_err(),
        "the unreferenced inode record is destroyed"
    );
    for off in &doomed_offsets {
        assert!(
            alloc.refcount(*off).is_none(),
            "block {off} of the destroyed orphan must be freed"
        );
    }
    for off in &keep_offsets {
        assert!(
            alloc.refcount(*off).is_some(),
            "the live file's block {off} must be untouched"
        );
    }

    // Accounting closes: a re-scan is clean — no C9, and no C2/C3/C6/C8
    // drift left behind by the free.
    let clean = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&clean, "after the C9 repair");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Offline sharding: exactly one finding across the merge, no double count
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sharded_scan_reports_exactly_one_c9_finding_across_the_merge() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    // Enough neighbours that every shard holds inodes NAMED by dentries
    // outside its own residue — the sharding trap: a shard that filtered
    // dentries by key (parent) instead of by target ino would report
    // every one of them.
    for i in 0..30 {
        create_file(&fx, &format!("f{i}")).await;
    }
    let doomed = create_file(&fx, "damaged.bin").await;
    orphan_inode(&fx, doomed).await;
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    let ctx = fx.ctx();
    let full = run_fsck(&ctx, &offline_opts()).await.expect("full scan");
    assert_eq!(c9_findings(&full).len(), 1, "{:?}", full.findings);

    const N: u32 = 3;
    let mut shard_reports = Vec::new();
    for k in 0..N {
        let mut opts = offline_opts();
        opts.shard = Some((k, N));
        let r = run_fsck(&ctx, &opts).await.expect("shard scan");
        assert!(
            c9_findings(&r).len() <= 1,
            "shard {k}/{N} reported more than its own residue: {:?}",
            r.findings
        );
        shard_reports.push(r);
    }
    let owners = shard_reports
        .iter()
        .filter(|r| !c9_findings(r).is_empty())
        .count();
    assert_eq!(
        owners, 1,
        "exactly one shard owns the orphan's ino residue (no double counting)"
    );
    let merged = merge_reports(&shard_reports);
    let merged_c9 = c9_findings(&merged);
    assert_eq!(
        merged_c9.len(),
        1,
        "the union must carry exactly one C9 finding: {:?}",
        merged.findings
    );
    assert_eq!(
        merged_c9[0].object,
        c9_findings(&full)[0].object,
        "the sharded union names the same inode as the full scan"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Creates racing a scan (the concurrency shape the era guard exists for)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_creates_racing_a_scan_produce_no_c9_findings() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    // A populated, healthy, PRIOR-era volume: every inode the scan walks
    // is a candidate by age, so only the era floor can save the creates
    // that land during the walk.
    let fx = open_fixture(&meta, &recs).await;
    for i in 0..64 {
        create_file(&fx, &format!("old{i}")).await;
    }
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    let fs = fx.fs.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_writer = stop.clone();
    let creator = tokio::spawn(async move {
        let mut n = 0u32;
        while !stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = fs
                .create(
                    req(),
                    1,
                    OsStr::new(&format!("new{n}")),
                    libc::S_IFREG | 0o644,
                    0,
                )
                .await;
            n += 1;
            tokio::task::yield_now().await;
        }
        n
    });

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let created = creator.await.expect("creator task");
    assert!(created > 0, "the racing creator must have done work");
    assert!(
        c9_findings(&report).is_empty(),
        "creates racing the scan produced C9 findings: {:?}",
        report.findings
    );
    fx.close().await;
}
