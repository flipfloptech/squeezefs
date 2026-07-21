//! PR VL6b — fsck **repair**, red-first
//! (docs/design-volume-lifecycle.md §5.6a, KD-10 rewritten, gate
//! G-VL-5(d)): per-class repair actions, **dry-run default**,
//! **quarantine-first**, verify-before-repair.
//!
//! Contracts under test:
//!
//! - **Per-class G-VL-5(d) cycles ×3** (fresh sandbox per round,
//!   reusing VL6a's seeders): seed ⇒ detect (finding appears) ⇒
//!   `RepairOptions` dry run (plan matches the §5.6a table, NOTHING
//!   mutates) ⇒ apply ⇒ **re-fsck clean** ⇒ untouched-data manifest
//!   byte-intact.
//!   - C1 semantic (wrong-tree record): **rebuild-in-place** — the
//!     schema-violating record is dropped via an ordinary journaled
//!     CoW tx, bytes quarantined first.
//!   - C1 torn (bit-flipped node): **quarantine + report ONLY** — v3
//!     has no node replicas; the finding honestly PERSISTS on re-fsck
//!     (no fabrication — stated per the design table).
//!   - C2 leaked: **free** via the begin→purge→punch→finish law,
//!     block bytes quarantined first.
//!   - C2 lost, content verifies: **repair the allocator** (the data
//!     was fine, the accounting was wrong).
//!   - C2 lost, out-of-range: **quarantine the mapping** — replaced
//!     by an explicit `damaged:` marker that reads EIO.
//!   - C3: **recount-and-set** under the referencing inos' leases.
//!   - C4: **quarantine copy of record + staged payload, then
//!     discard** (the recovery discard law, made non-destructive).
//!   - C5: **quarantine (move aside), not delete**.
//!   - C6: **recompute** the derived accounting from the census.
//!   - C7: **quarantine the mapping** (`damaged:` ⇒ EIO); the
//!     physical block is left in place for forensics (still
//!     allocator-tracked).
//! - **Dry-run default**: `--repair` without apply plans + prints,
//!   mutates NOTHING (re-fsck still finds).
//! - **Findings-only + verify-before-repair**: a finding healed
//!   between scan and repair is a REFUSED repair, counted.
//! - **Idempotence**: repair on a repaired volume plans zero actions;
//!   re-applying a stale report refuses everything.
//! - **Quarantine manifest round-trip**: every discarded byte is in
//!   the quarantine copy; the manifest lists the files verbatim.
//! - **Kill-9 mid-apply convergence**: the abort hook severs the run
//!   between quarantine and commit; the re-run converges clean.
//! - **§10 stats**: `fsck_repairs_{planned,applied,refused}`,
//!   `fsck_quarantined_{records,blocks,bytes}`, per-class
//!   `fsck_repair_classC{1..7}` move.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{
    clear_repair_abort_hook, repair as run_repair, run as run_fsck, set_repair_abort_hook,
    volume_generation, FsckCtx, FsckOptions, FsckReport, RepairOptions, RepairReport,
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

/// Mount-shaped fixture (VL6a's shape, verbatim).
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

/// Probe-shaped fixture: open WITHOUT allocator recovery (the C1 torn
/// leg — a corrupt node must land as a finding, not a fixture panic).
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

fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = std::time::Duration::from_millis(100);
    o
}

fn scrub_opts() -> FsckOptions {
    let mut o = online_opts();
    o.scrub = true;
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

/// Striped burst: force striped layout, write `nblocks` distinct blocks,
/// fsync. Returns the expected file content.
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

async fn read_back(fx: &Fx, ino: u64, len: usize) -> Vec<u8> {
    let reply = fx
        .fs
        .read(req(), ino, 0, 0, len as u32, 0)
        .await
        .expect("read back");
    reply.data.as_ref().to_vec()
}

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

fn assert_clean(report: &FsckReport, what: &str) {
    assert!(
        report.findings.is_empty(),
        "{what}: re-fsck must be CLEAN after repair, got {:?}",
        report.findings
    );
}

fn assert_finding(report: &FsckReport, class: &str, what: &str) {
    assert!(
        report.findings.iter().any(|f| f.class == class),
        "{what}: expected a {class} finding, got {:?}",
        report.findings
    );
}

/// One fresh single-volume sandbox per round.
struct Round {
    _dir: TempDir,
    oss1: PathBuf,
    recs: Vec<DataVolumeRecord>,
    fx: Fx,
}

async fn fresh_round() -> Round {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    Round {
        _dir: dir,
        oss1,
        recs,
        fx,
    }
}

/// The quarantine manifest(s) written under the fixture's staging dir.
fn quarantine_manifests(staging: &Path) -> Vec<(PathBuf, serde_json::Value)> {
    let qhome = staging.join("quarantine");
    let mut out = Vec::new();
    let Ok(runs) = std::fs::read_dir(&qhome) else {
        return out;
    };
    for run in runs.flatten() {
        let mf = run.path().join("manifest.json");
        if let Ok(bytes) = std::fs::read(&mf) {
            let v: serde_json::Value =
                serde_json::from_slice(&bytes).expect("manifest.json is valid JSON");
            out.push((run.path(), v));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// C1 semantic — rebuild-in-place (drop the schema-violating record via a
// journaled CoW tx), quarantined first, re-fsck clean, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c1_semantic_repair_rebuild_in_place_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        let ino = create_file(&r.fx, "keep.bin").await;
        let expected = striped_burst(&r.fx, ino, 4).await;

        // VL6a's seed: a dentry-shaped key in the inodes tree.
        let bad_key = squeezefs::meta_backend::kv::record::dentry_key(42, 7, 0);
        r.fx.meta.volumes[0].trees()[0]
            .insert(&bad_key[..], bytes::Bytes::from_static(b"bogus"))
            .await
            .expect("raw insert");

        let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&report, "C1", "round {round} seed");

        // Dry run: plan matches the table, nothing mutates.
        let plan = run_repair(&r.fx.ctx(), &report, &dry_run())
            .await
            .expect("plan");
        assert!(plan.dry_run, "round {round}: default is a dry run");
        assert!(
            plan.planned
                .iter()
                .any(|a| a.class == "C1" && a.action == "rebuild-in-place"),
            "round {round}: C1 semantic plans rebuild-in-place, got {:?}",
            plan.planned
        );
        assert!(plan.applied.is_empty(), "dry run applies nothing");
        let still = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&still, "C1", "round {round}: dry run must not repair");

        // Apply: the record is dropped, quarantined first.
        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C1" && a.action == "rebuild-in-place"),
            "round {round}: applied actions {:?}",
            rep.applied
        );
        assert!(rep.counters.quarantined_records >= 1);
        let gone = r.fx.meta.volumes[0].trees()[0]
            .lookup(&bad_key[..])
            .await
            .expect("lookup");
        assert!(gone.is_none(), "round {round}: wrong-tree record removed");

        let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C1 semantic");
        assert_eq!(
            read_back(&r.fx, ino, expected.len()).await,
            expected,
            "round {round}: untouched data manifest intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C1 torn — quarantine + report ONLY (no fabrication): the finding
// honestly persists on re-fsck
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c1_torn_node_quarantine_report_only() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta_node(&meta, &[&oss1], 64 * 1024).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    {
        let fx = open_fixture(&meta, &recs).await;
        for i in 0..4000 {
            create_file(&fx, &format!("f{i}")).await;
        }
        fx.close().await;
    }

    // Bit-flip a durable non-root leaf (VL6a's seed).
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
        assert_ne!(addr, root_addr, "inode tree must have split");
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

    let fx = open_fixture_probe(&meta, &recs).await;
    let mut opts = FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(50);
    let report = run_fsck(&fx.ctx(), &opts).await.expect("fsck");
    assert_finding(&report, "C1", "bit-flipped node");

    let rep = run_repair(&fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    // The honest action: quarantine + report, never fabrication.
    assert!(
        rep.applied
            .iter()
            .any(|a| a.class == "C1" && a.action == "quarantine-report-only"),
        "torn node applies quarantine-report-only, got {:?}",
        rep.applied
    );
    // No replicas ⇒ the finding PERSISTS (data loss made visible, not
    // silently repaired).
    let still = run_fsck(&fx.ctx(), &opts).await.expect("fsck");
    assert_finding(&still, "C1", "torn node persists (no fabrication)");
    // …and the manifest records the identity.
    let manifests = quarantine_manifests(&fx.staging_path);
    assert!(
        manifests.iter().any(|(_, v)| v["entries"]
            .as_array()
            .is_some_and(|e| e.iter().any(|x| x["class"] == "C1"))),
        "manifest records the torn-node identity: {manifests:?}"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// C2 leaked — free via the full law, quarantined copy first, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c2_leaked_repair_free_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        let ino = create_file(&r.fx, "keep.bin").await;
        let expected = striped_burst(&r.fx, ino, 4).await;

        let alloc = r.fx.default_allocator(&r.recs[0].id);
        let leaked_off = alloc.allocate_block().await.expect("allocate");

        let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&report, "C2", "round {round} leaked seed");

        let plan = run_repair(&r.fx.ctx(), &report, &dry_run())
            .await
            .expect("plan");
        assert!(
            plan.planned
                .iter()
                .any(|a| a.class == "C2" && a.action == "free-leaked-block"),
            "round {round}: plan {:?}",
            plan.planned
        );
        assert!(
            alloc.refcount(leaked_off).is_some(),
            "round {round}: dry run must not free"
        );

        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C2" && a.action == "free-leaked-block"),
            "round {round}: applied {:?}",
            rep.applied
        );
        assert!(
            alloc.refcount(leaked_off).is_none(),
            "round {round}: leaked block freed"
        );
        assert!(
            rep.counters.quarantined_blocks >= 1,
            "round {round}: the freed bytes were quarantined first: {:?}",
            rep.counters
        );

        let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C2 leaked");
        assert_eq!(
            read_back(&r.fx, ino, expected.len()).await,
            expected,
            "round {round}: untouched data intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C2 lost, content verifies — repair the allocator (the data was fine,
// the accounting was wrong), ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c2_lost_verified_content_repairs_allocator_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        let ino = create_file(&r.fx, "keep.bin").await;
        let expected = striped_burst(&r.fx, ino, 4).await;

        // The seed: a block with REAL durable content that the allocator
        // forgot (allocated + written + published, then allocator-level
        // free with no device punch), still referenced by the map.
        let alloc = r.fx.default_allocator(&r.recs[0].id);
        let off = alloc.allocate_block().await.expect("allocate");
        let (_, dev) = r
            .fx
            .fs
            .router
            .backend_router
            .get_backend(&r.recs[0].id)
            .expect("backend");
        dev.write_block(off, bytes::Bytes::from(vec![0x5Au8; BLOCK]))
            .await
            .expect("device write");
        alloc.publish_block(off);
        alloc.free_block(off).await.expect("allocator-level free");
        let key = r
            .fx
            .fs
            .router
            .backend_router
            .persist_block_key(&r.recs[0].id, off);
        let token = r.fx.fs.dlm().get_fencing_token_ino(ino);
        let entries = [(400u32, key.clone())];
        r.fx.fs
            .router
            .merge_block_mappings(
                ino,
                squeezefs::routing::BlockMapOp::Merge(&entries),
                0,
                squeezefs::routing::LayoutFlip::KeepLayout,
                token,
            )
            .await
            .expect("merge lost mapping");

        let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&report, "C2", "round {round} lost seed");

        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C2" && a.action == "repair-allocator"),
            "round {round}: content verified ⇒ allocator repaired, got {:?}",
            rep.applied
        );
        assert_eq!(
            alloc.refcount(off),
            Some(1),
            "round {round}: offset re-tracked at the counted reference count"
        );

        let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C2 lost (verified)");
        assert_eq!(
            read_back(&r.fx, ino, expected.len()).await,
            expected,
            "round {round}: untouched data intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C2 lost, out-of-range — quarantine the mapping (`damaged:` ⇒ EIO),
// never fabricate a hole, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c2_lost_out_of_range_quarantines_mapping_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        let ino = create_file(&r.fx, "keep.bin").await;
        let expected = striped_burst(&r.fx, ino, 4).await;

        // VL6a's phantom seed: an offset far past the device capacity.
        let phantom_off = 1024u64 * BLOCK as u64 * 1024;
        let phantom_key = r
            .fx
            .fs
            .router
            .backend_router
            .persist_block_key(&r.recs[0].id, phantom_off);
        let token = r.fx.fs.dlm().get_fencing_token_ino(ino);
        let entries = [(400u32, phantom_key.clone())];
        r.fx.fs
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

        let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&report, "C2", "round {round} out-of-range seed");

        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C2" && a.action == "quarantine-mapping"),
            "round {round}: out-of-range lost ⇒ quarantine-mapping, got {:?}",
            rep.applied
        );

        // The mapping is now the explicit damaged marker.
        let mappings = block_mappings_of(&r.fx, ino).await;
        let flipped = mappings
            .iter()
            .find(|(b, _)| *b == 400)
            .expect("block 400 mapped");
        assert!(
            flipped.1.starts_with("damaged:"),
            "round {round}: mapping flipped to damaged marker, got {}",
            flipped.1
        );

        let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C2 lost (quarantined)");
        assert_eq!(
            read_back(&r.fx, ino, expected.len()).await,
            expected,
            "round {round}: untouched data intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C3 — recount-and-set under the referencing inos' leases, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c3_recount_and_set_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;

        // Clone-shared population (refcount 2 with 2 referencers is
        // healthy; the checker is clone-aware).
        let a = create_file(&r.fx, "a.bin").await;
        let expected_a = striped_burst(&r.fx, a, 4).await;
        let b = create_file(&r.fx, "b.bin").await;
        let a_tok = r.fx.fs.dlm().get_fencing_token_ino(a);
        let b_tok = r.fx.fs.dlm().get_fencing_token_ino(b);
        r.fx.fs
            .router
            .clone_file(
                &squeezefs::keys::inode_path(a),
                &squeezefs::keys::inode_path(b),
                Some(a_tok),
                Some(b_tok),
            )
            .await
            .expect("clone");

        // The seed: a phantom extra reference (refcount 3, referencers 2).
        let mappings = block_mappings_of(&r.fx, a).await;
        let (_, victim_mapping) = mappings.first().expect("striped block exists");
        assert!(r
            .fx
            .fs
            .router
            .backend_router
            .increment_refcount(victim_mapping));
        let (_, victim_off) = r
            .fx
            .fs
            .router
            .backend_router
            .parse_block_key(victim_mapping)
            .expect("parse");
        let alloc = r.fx.default_allocator(&r.recs[0].id);
        assert_eq!(alloc.refcount(victim_off), Some(3), "seed landed");

        let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&report, "C3", "round {round} refcount seed");

        let plan = run_repair(&r.fx.ctx(), &report, &dry_run())
            .await
            .expect("plan");
        assert!(
            plan.planned
                .iter()
                .any(|a| a.class == "C3" && a.action == "recount-and-set-refcount"),
            "round {round}: plan {:?}",
            plan.planned
        );
        assert_eq!(alloc.refcount(victim_off), Some(3), "dry run mutates nothing");

        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C3" && a.action == "recount-and-set-refcount"),
            "round {round}: applied {:?}",
            rep.applied
        );
        assert_eq!(
            alloc.refcount(victim_off),
            Some(2),
            "round {round}: refcount set to the counted value"
        );

        let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C3");
        assert_eq!(
            read_back(&r.fx, a, expected_a.len()).await,
            expected_a,
            "round {round}: clone A intact"
        );
        assert_eq!(
            read_back(&r.fx, b, expected_a.len()).await,
            expected_a,
            "round {round}: clone B intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C4 — quarantine copy of record + staged payload, then discard, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c4_orphan_quarantine_then_discard_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        let ino = create_file(&r.fx, "keep.bin").await;
        let expected = striped_burst(&r.fx, ino, 2).await;

        let orphan_key = squeezefs::keys::active_block_ext(9_999_991, 0).to_string();
        squeezefs::cache::nvme::seed_staged_custody_for_test(&r.fx.staging_path, &orphan_key)
            .await
            .expect("seed orphan custody");

        let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&report, "C4", "round {round} orphan seed");

        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C4" && a.action == "quarantine-then-discard-custody"),
            "round {round}: applied {:?}",
            rep.applied
        );
        assert!(rep.counters.quarantined_records >= 1);
        assert!(rep.counters.quarantined_bytes > 0);

        // Custody discarded (the scan no longer sees a live record).
        let keys = squeezefs::cache::nvme::scan_live_staged_custody(&r.fx.staging_path, 1000)
            .await
            .expect("scan");
        assert!(
            !keys.iter().any(|k| k == &orphan_key),
            "round {round}: orphan custody discarded, scan sees {keys:?}"
        );

        let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C4");
        assert_eq!(
            read_back(&r.fx, ino, expected.len()).await,
            expected,
            "round {round}: untouched data intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C5 — quarantine (move aside), not delete, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c5_stale_generation_quarantine_move_aside_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        let ino = create_file(&r.fx, "keep.bin").await;
        let expected = striped_burst(&r.fx, ino, 2).await;

        let expected_gen = volume_generation(&r.fx.meta);
        squeezefs::cache::nvme::write_staging_generation_marker(
            &r.fx.staging_path,
            "v3:deadbeefdeadbeefdeadbeefdeadbeef",
        )
        .await
        .expect("stale restamp");

        let mut ctx = r.fx.ctx();
        ctx.expected_generation = Some(expected_gen.clone());
        let report = run_fsck(&ctx, &online_opts()).await.expect("fsck");
        assert_finding(&report, "C5", "round {round} stale generation");

        let rep = run_repair(&ctx, &report, &apply()).await.expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C5" && a.action == "quarantine-staging-dir"),
            "round {round}: applied {:?}",
            rep.applied
        );

        // Moved aside, not deleted: the marker is gone from the dir…
        let marker = squeezefs::cache::nvme::read_staging_generation_marker(&r.fx.staging_path)
            .await
            .expect("marker read");
        assert!(
            marker.is_none(),
            "round {round}: stale marker moved aside, still reads {marker:?}"
        );
        // …and a verbatim copy lives in quarantine.
        let manifests = quarantine_manifests(&r.fx.staging_path);
        assert!(
            manifests.iter().any(|(_, v)| v["entries"]
                .as_array()
                .is_some_and(|e| e.iter().any(|x| x["class"] == "C5"))),
            "round {round}: manifest records the C5 move-aside"
        );

        let clean = run_fsck(&ctx, &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C5");
        assert_eq!(
            read_back(&r.fx, ino, expected.len()).await,
            expected,
            "round {round}: untouched data intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C6 — recompute the derived accounting, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c6_drift_recompute_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        let ino = create_file(&r.fx, "keep.bin").await;
        let expected = striped_burst(&r.fx, ino, 4).await;

        // VL6a's seed: begin_free-limbo (the wedged-freer shape).
        let alloc = r.fx.default_allocator(&r.recs[0].id);
        let off = alloc.allocate_block().await.expect("allocate");
        assert!(alloc.begin_free(off), "terminal begin_free");
        // deliberately NO finish_free

        let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_finding(&report, "C6", "round {round} drift seed");

        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C6" && a.action == "recompute-accounting"),
            "round {round}: applied {:?}",
            rep.applied
        );
        let used = alloc
            .highest_block_index()
            .saturating_sub(alloc.free_blocks_count());
        assert_eq!(
            used,
            alloc.tracked_offsets().len() as u64,
            "round {round}: accounting recomputed to match the tracked population"
        );

        let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
        assert_clean(&clean, "round {round} C6");
        assert_eq!(
            read_back(&r.fx, ino, expected.len()).await,
            expected,
            "round {round}: untouched data intact"
        );
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// C7 — quarantine the mapping; the physical block stays for forensics, ×3
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c7_scrub_failure_quarantines_mapping_x3() {
    let _serial = serial().await;
    for round in 0..3 {
        let r = fresh_round().await;
        r.fx.fs
            .router
            .set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
                "lz4".to_string(),
                "none".to_string(),
                None,
            ));

        let ino = create_file(&r.fx, "comp.bin").await;
        striped_burst(&r.fx, ino, 4).await;

        // Corrupt the first striped block's stored image (VL6a's seed).
        let mappings = block_mappings_of(&r.fx, ino).await;
        let (victim_idx, victim_mapping) = mappings.first().expect("striped block").clone();
        let clean_key = victim_mapping
            .split(':')
            .next()
            .unwrap_or(&victim_mapping)
            .to_string();
        let (_, victim_off) = r
            .fx
            .fs
            .router
            .backend_router
            .parse_block_key(&clean_key)
            .expect("parse");
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&r.oss1)
                .unwrap();
            f.seek(SeekFrom::Start(victim_off + 8)).unwrap();
            let mut buf = [0u8; 32];
            f.read_exact(&mut buf).unwrap();
            for b in buf.iter_mut() {
                *b ^= 0x5A;
            }
            f.seek(SeekFrom::Start(victim_off + 8)).unwrap();
            f.write_all(&buf).unwrap();
            f.sync_all().unwrap();
        }

        let report = run_fsck(&r.fx.ctx(), &scrub_opts()).await.expect("scrub");
        assert_finding(&report, "C7", "round {round} frame corruption");

        let alloc = r.fx.default_allocator(&r.recs[0].id);
        let rep = run_repair(&r.fx.ctx(), &report, &apply())
            .await
            .expect("apply");
        assert!(
            rep.applied
                .iter()
                .any(|a| a.class == "C7" && a.action == "quarantine-mapping"),
            "round {round}: applied {:?}",
            rep.applied
        );

        // Mapping flipped; the PHYSICAL block stays tracked (forensics).
        let mappings = block_mappings_of(&r.fx, ino).await;
        let flipped = mappings
            .iter()
            .find(|(b, _)| *b == victim_idx)
            .expect("victim still mapped");
        assert!(
            flipped.1.starts_with("damaged:"),
            "round {round}: mapping flipped to damaged, got {}",
            flipped.1
        );
        assert!(
            alloc.refcount(victim_off).is_some(),
            "round {round}: physical block left in place for forensics"
        );

        // The damaged block reads EIO; re-scrub is clean.
        let eio = r
            .fx
            .fs
            .read(
                req(),
                ino,
                0,
                (victim_idx as usize * BLOCK) as u64,
                BLOCK as u32,
                0,
            )
            .await;
        assert!(
            eio.is_err(),
            "round {round}: quarantined mapping must read EIO, got Ok"
        );
        let clean = run_fsck(&r.fx.ctx(), &scrub_opts()).await.expect("scrub");
        assert_clean(&clean, "round {round} C7");
        r.fx.close().await;
    }
}

// ---------------------------------------------------------------------------
// Dry-run default: plans, prints, mutates NOTHING
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repair_dry_run_default_mutates_nothing() {
    let _serial = serial().await;
    let r = fresh_round().await;
    let orphan_key = squeezefs::keys::active_block_ext(9_999_993, 0).to_string();
    squeezefs::cache::nvme::seed_staged_custody_for_test(&r.fx.staging_path, &orphan_key)
        .await
        .expect("seed");

    let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_finding(&report, "C4", "seed");

    let plan = run_repair(&r.fx.ctx(), &report, &dry_run())
        .await
        .expect("plan");
    assert!(plan.dry_run);
    assert!(!plan.planned.is_empty(), "the plan names the actions");
    assert!(plan.applied.is_empty(), "dry run applies nothing");
    assert_eq!(plan.counters.applied, 0);
    assert!(plan.counters.planned >= 1);

    // Nothing mutated: the custody record is still there, no quarantine
    // dir was created, and re-fsck still finds.
    let keys = squeezefs::cache::nvme::scan_live_staged_custody(&r.fx.staging_path, 1000)
        .await
        .expect("scan");
    assert!(keys.iter().any(|k| k == &orphan_key), "custody untouched");
    assert!(
        quarantine_manifests(&r.fx.staging_path).is_empty(),
        "dry run writes no quarantine"
    );
    let still = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_finding(&still, "C4", "dry run repaired nothing");
    r.fx.close().await;
}

// ---------------------------------------------------------------------------
// Verify-before-repair: a manually-healed finding is a REFUSED repair
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repair_refuses_stale_finding() {
    let _serial = serial().await;
    let r = fresh_round().await;
    let orphan_key = squeezefs::keys::active_block_ext(9_999_994, 0).to_string();
    squeezefs::cache::nvme::seed_staged_custody_for_test(&r.fx.staging_path, &orphan_key)
        .await
        .expect("seed");

    let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_finding(&report, "C4", "seed");

    // HEAL manually: the seeded shard file disappears between scan and
    // repair — the state moved on.
    std::fs::remove_file(
        r.fx.staging_path
            .join("staging_segment")
            .join("seeded_shard"),
    )
    .expect("manual heal");

    let before = squeezefs::fuse_client::METRICS
        .fsck_repairs_refused
        .load(Ordering::Relaxed);
    let rep = run_repair(&r.fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert!(
        rep.applied.iter().all(|a| a.class != "C4"),
        "a healed finding must not be repaired: {:?}",
        rep.applied
    );
    assert!(
        rep.refused.iter().any(|a| a.class == "C4"),
        "the stale finding is a refused repair: {:?}",
        rep.refused
    );
    assert!(rep.counters.refused >= 1);
    let after = squeezefs::fuse_client::METRICS
        .fsck_repairs_refused
        .load(Ordering::Relaxed);
    assert!(after > before, "fsck_repairs_refused moved");
    r.fx.close().await;
}

// ---------------------------------------------------------------------------
// Idempotence: repair on a repaired volume plans ZERO actions; a stale
// report re-applied refuses everything
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repair_idempotent_second_run_plans_zero() {
    let _serial = serial().await;
    let r = fresh_round().await;
    let ino = create_file(&r.fx, "keep.bin").await;
    striped_burst(&r.fx, ino, 2).await;
    let alloc = r.fx.default_allocator(&r.recs[0].id);
    let leaked_off = alloc.allocate_block().await.expect("allocate");

    let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_finding(&report, "C2", "seed");

    let rep = run_repair(&r.fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert!(rep.counters.applied >= 1);
    assert!(alloc.refcount(leaked_off).is_none(), "freed");

    // Re-fsck: clean; repair of the CLEAN report plans zero.
    let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_clean(&clean, "post-repair");
    let second = run_repair(&r.fx.ctx(), &clean, &apply())
        .await
        .expect("second apply");
    assert_eq!(
        second.counters.planned, 0,
        "repair on a repaired volume plans zero actions: {:?}",
        second.planned
    );
    assert_eq!(second.counters.applied, 0);

    // Re-applying the STALE report converges too: every action refuses
    // (the finding no longer exists), nothing double-frees.
    let stale = run_repair(&r.fx.ctx(), &report, &apply())
        .await
        .expect("stale re-apply");
    assert_eq!(
        stale.counters.applied, 0,
        "stale re-apply must apply nothing: {:?}",
        stale.applied
    );
    assert!(stale.counters.refused >= 1);
    let still_clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_clean(&still_clean, "after stale re-apply");
    r.fx.close().await;
}

// ---------------------------------------------------------------------------
// Kill-9 mid-apply: the abort hook severs the run between quarantine and
// commit; the re-run converges clean
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repair_kill9_mid_apply_convergence() {
    let _serial = serial().await;
    let r = fresh_round().await;
    let orphan_key = squeezefs::keys::active_block_ext(9_999_995, 0).to_string();
    squeezefs::cache::nvme::seed_staged_custody_for_test(&r.fx.staging_path, &orphan_key)
        .await
        .expect("seed");

    let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_finding(&report, "C4", "seed");

    // Abort between quarantine and commit — the kill-9 window.
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired2 = fired.clone();
    set_repair_abort_hook(Arc::new(move |what: &str| {
        if what.starts_with("C4:") {
            fired2.store(true, Ordering::SeqCst);
            true // die here
        } else {
            false
        }
    }));
    let severed = run_repair(&r.fx.ctx(), &report, &apply()).await;
    clear_repair_abort_hook();
    assert!(fired.load(Ordering::SeqCst), "the abort hook fired");
    assert!(
        severed.is_err(),
        "the severed run surfaces the abort loudly"
    );

    // The custody record is STILL live (quarantine happened, the commit
    // did not — a half-completed action is re-detected, never lost).
    let keys = squeezefs::cache::nvme::scan_live_staged_custody(&r.fx.staging_path, 1000)
        .await
        .expect("scan");
    assert!(
        keys.iter().any(|k| k == &orphan_key),
        "custody survives the severed run (commit never happened)"
    );

    // Convergence: re-detect + re-repair lands clean.
    let report2 = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_finding(&report2, "C4", "re-detected after the severed run");
    let rep = run_repair(&r.fx.ctx(), &report2, &apply())
        .await
        .expect("re-run");
    assert!(rep.counters.applied >= 1);
    let clean = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_clean(&clean, "converged after kill-9 window");
    r.fx.close().await;
}

// ---------------------------------------------------------------------------
// Quarantine manifest round-trip: every discarded byte is retrievable
// from the quarantine copy the manifest lists
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_quarantine_manifest_roundtrip() {
    let _serial = serial().await;
    let r = fresh_round().await;
    let orphan_key = squeezefs::keys::active_block_ext(9_999_996, 0).to_string();
    squeezefs::cache::nvme::seed_staged_custody_for_test(&r.fx.staging_path, &orphan_key)
        .await
        .expect("seed");

    let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    assert_finding(&report, "C4", "seed");
    let rep = run_repair(&r.fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert!(rep.counters.quarantined_records >= 1);
    let qdir = rep
        .quarantine_dir
        .as_ref()
        .expect("apply with quarantined work names its dir");

    // The manifest lists the quarantined files; each exists with its
    // recorded byte length, and the custody record's bytes (key +
    // payload) are retrievable verbatim.
    let manifest_bytes =
        std::fs::read(Path::new(qdir).join("manifest.json")).expect("manifest exists");
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).expect("valid JSON");
    let entries = manifest["entries"].as_array().expect("entries array");
    let c4 = entries
        .iter()
        .find(|e| e["class"] == "C4")
        .expect("C4 entry in manifest");
    let files = c4["files"].as_array().expect("files array");
    assert!(!files.is_empty(), "the discarded record was copied");
    let mut recovered = Vec::new();
    for f in files {
        let name = f["name"].as_str().expect("file name");
        let bytes = f["bytes"].as_u64().expect("byte length");
        let path = Path::new(qdir).join(name);
        let data = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("quarantined file {name} unreadable: {e}"));
        assert_eq!(data.len() as u64, bytes, "manifest length matches {name}");
        recovered.extend_from_slice(&data);
    }
    // The record image carries the custody key AND the staged payload
    // ("x" — the seeder's value) verbatim.
    let hay = recovered;
    let key_bytes = orphan_key.as_bytes();
    assert!(
        hay.windows(key_bytes.len()).any(|w| w == key_bytes),
        "quarantined bytes contain the custody key"
    );
    r.fx.close().await;
}

// ---------------------------------------------------------------------------
// §10 stats: the repair families move
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repair_metrics_published() {
    let _serial = serial().await;
    let m = &*squeezefs::fuse_client::METRICS;
    let planned0 = m.fsck_repairs_planned.load(Ordering::Relaxed);
    let applied0 = m.fsck_repairs_applied.load(Ordering::Relaxed);
    let qrec0 = m.fsck_quarantined_records.load(Ordering::Relaxed);
    let qbytes0 = m.fsck_quarantined_bytes.load(Ordering::Relaxed);
    let c4_0 = m.fsck_repair_class[3].load(Ordering::Relaxed);

    let r = fresh_round().await;
    let orphan_key = squeezefs::keys::active_block_ext(9_999_997, 0).to_string();
    squeezefs::cache::nvme::seed_staged_custody_for_test(&r.fx.staging_path, &orphan_key)
        .await
        .expect("seed");
    let report = run_fsck(&r.fx.ctx(), &online_opts()).await.expect("fsck");
    let rep: RepairReport = run_repair(&r.fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert!(rep.counters.applied >= 1);

    assert!(
        m.fsck_repairs_planned.load(Ordering::Relaxed) > planned0,
        "fsck_repairs_planned moved"
    );
    assert!(
        m.fsck_repairs_applied.load(Ordering::Relaxed) > applied0,
        "fsck_repairs_applied moved"
    );
    assert!(
        m.fsck_quarantined_records.load(Ordering::Relaxed) > qrec0,
        "fsck_quarantined_records moved"
    );
    assert!(
        m.fsck_quarantined_bytes.load(Ordering::Relaxed) > qbytes0,
        "fsck_quarantined_bytes moved"
    );
    assert!(
        m.fsck_repair_class[3].load(Ordering::Relaxed) > c4_0,
        "fsck_repair_classC4 moved"
    );
    r.fx.close().await;
}
