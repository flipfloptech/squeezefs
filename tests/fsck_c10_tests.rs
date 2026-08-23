//! fsck detection class **C10 — inode-plane reference consistency**,
//! red-first (`docs/design-volume-lifecycle.md` §5.6/§5.6a for the ladder
//! and the repair posture; `docs/design-cow-kv-metadata.md` §4.10a for
//! the damage class C9 opened and this one finishes).
//!
//! C9 finds an inode **no dentry names** (a leak). Enumerating what real
//! pre-S3.5 crash damage can also contain left two shapes with no
//! detector at all, and C10 is both of them:
//!
//! 1. **`nlink` too high** — a cross-volume `link` that incremented the
//!    count but never inserted the dentry leaves `nlink == 2` with ONE
//!    name. C9 stays correctly silent (a name exists), and the inode plus
//!    every block it owns can never be reclaimed: a permanent leak that
//!    reads as healthy.
//! 2. **`nlink` too low, or a dangling name** — a cross-volume `unlink`
//!    that decremented the count but left the name leaves a dentry
//!    resolving to an inode whose count no longer covers it. When a count
//!    reaches 0 while a name still exists, **ordinary reclaim is entitled
//!    to destroy an inode a live path can still resolve** — that
//!    direction is DATA LOSS, not a leak, and it is reachable on field
//!    volumes today. S3.5's `24ef223c` made such a dentry *removable*;
//!    nothing ever **found** one.
//!
//! Contracts under test:
//!
//! - **Both directions are found and are distinguishable**: the leak
//!   direction (`nlink` > names) and the dangerous direction (`nlink` <
//!   names, `nlink == 0` with a live name, a name resolving to nothing)
//!   carry different evidence and different repair verbs, so an operator
//!   can triage which one means stop-and-read.
//! - **Zero false positives** on the C9 ladder: settle → a FRESH dentry
//!   pass → re-check under the ino's exclusive 4a lease, plus the two
//!   guards C9's era floor cannot supply here (a prior-era inode can be
//!   legitimately hardlinked one microsecond ago): the record witness
//!   `(nlink, ctime)` bracketed around the fresh pass, and the open
//!   cross-volume intent exemption.
//! - **Directories are never a count finding**: their `nlink` counts `.`
//!   and every child's `..`, which are synthesized and never records.
//! - **`nlink == 0` WITHOUT a name stays unclaimed** — that is POSIX
//!   unlinked-but-open / POSIX-15's rename-overwrite orphan, and telling
//!   it from a corpse nobody will FORGET needs the live open-count
//!   registries `FsckCtx` does not carry. `nlink == 0` WITH a name is
//!   unambiguous: no legitimate state has it.
//! - **The block classes stay silent**: a wrong `nlink` does not corrupt
//!   block accounting, so C2/C3/C8 must not fire alongside a C10 finding.
//! - **`fsck_findings` = 0 on healthy volumes**, hardlinks and directory
//!   trees included, across a remount.
//! - **Repair is conservative per direction**: raising a count is safe,
//!   lowering one is not (an undercount would make a live name's inode
//!   reclaimable), a name resolving to nothing can only be removed, and
//!   ambiguous evidence is refused rather than guessed at.

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
use squeezefs::meta_backend::kv::record::{
    inode_key, DentryValue, InodeValue, XattrValue, TREE_INODES, TREE_XATTRS,
};
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

/// Mount-shaped fixture (VL6a's shape, identical to
/// `tests/fsck_c9_tests.rs`): resolved volume records drive
/// `register_backend`, the allocator refcount census is rebuilt from the
/// tree exactly as a mount rebuilds it, and the fsck context points at the
/// live meta + router.
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
        multi_owner: false,
    }
}

fn apply() -> RepairOptions {
    RepairOptions {
        apply: true,
        quarantine_dir: None,
        multi_owner: false,
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

/// A real second name for `ino` (nlink AND the dentry both move — the
/// healthy hardlink shape every C10 arm must discriminate against).
async fn hardlink(fx: &Fx, ino: u64, name: &str) {
    fx.fs
        .link(req(), ino, 1, OsStr::new(name))
        .await
        .unwrap_or_else(|e| panic!("link({name}) failed: {e:?}"));
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

// ---------------------------------------------------------------------------
// Damage fixtures — the field shapes, produced by raw record surgery so no
// product path has to be broken to write the test
// ---------------------------------------------------------------------------

/// The ino's raw inode record, as the census reads it.
async fn read_record(fx: &Fx, ino: u64) -> Option<InodeValue> {
    let (v_idx, local) = fx.meta.route_ino(ino);
    let kv = &fx.meta.volumes[v_idx];
    kv.drain_pending_times_now().await.ok();
    let bytes = kv.trees()[0].lookup(&inode_key(local)).await.ok()??;
    InodeValue::decode(&bytes).ok()
}

/// **Damage: force `nlink`.** The one raw mutation these fixtures need —
/// a link count that disagrees with the names is exactly what a lost
/// count-step commit leaves behind (pre-S3.5 cross-volume `link`/`unlink`,
/// and an S9 plan whose `SetNlink` volume was fenced).
async fn force_nlink(fx: &Fx, ino: u64, nlink: u32) {
    let (v_idx, local) = fx.meta.route_ino(ino);
    let kv = &fx.meta.volumes[v_idx];
    kv.drain_pending_times_now().await.ok();
    let mut val = read_record(fx, ino).await.expect("inode record");
    val.nlink = nlink;
    kv.migration_apply(
        vec![(TREE_INODES, inode_key(local).to_vec(), val.encode())],
        vec![],
    )
    .await
    .expect("force nlink");
}

/// **Damage: drop ONE of the dentry records naming `ino`** (leaving any
/// others, and leaving the inode record and its `nlink` untouched) — the
/// lost-name-step shape.
async fn drop_one_name(fx: &Fx, ino: u64) {
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
                        dentries.delete(k).await.expect("drop one naming dentry");
                        return;
                    }
                }
            }
        }
    }
    panic!("no dentry names ino {ino}");
}

/// **Damage: destroy the inode record, keep the name.** The dentry now
/// resolves to nothing — the shape S3.5's `24ef223c` made *removable* and
/// that nothing ever found.
async fn destroy_record_keep_name(fx: &Fx, ino: u64) {
    let (v_idx, local) = fx.meta.route_ino(ino);
    let kv = &fx.meta.volumes[v_idx];
    kv.drain_pending_times_now().await.ok();
    kv.trees()[0]
        .delete(&inode_key(local))
        .await
        .expect("destroy the inode record");
}

/// Plant an OPEN cross-volume intent whose plan names `ino` — the durable
/// footprint a plan in flight (or severed mid-plan) leaves. The count arms
/// must treat every ino such a plan names as in-flight and exempt it: a
/// multi-commit plan is precisely the window where the count and the name
/// legitimately disagree.
async fn plant_open_intent(fx: &Fx, ino: u64, tx_id: u64) {
    use squeezefs::meta_backend::crossvol_tx::{
        intent_key, intent_name, IntentRecord, XvOp, XvStep,
    };
    let record = IntentRecord {
        tx_id,
        op: XvOp::Link,
        steps: vec![
            XvStep::InsertDentry {
                parent: 1,
                name: "in-flight-plan".to_string(),
                child: ino,
                ft_bits: libc::S_IFREG,
                parent_update: 1,
            },
            XvStep::SetNlink {
                ino,
                pre: 1,
                post: 2,
                ctime: None,
            },
        ],
    };
    let value = XattrValue {
        name: intent_name(tx_id).into_bytes(),
        value: record.encode().expect("encode intent"),
    }
    .encode()
    .expect("encode xattr value");
    let kv = &fx.meta.volumes[0];
    kv.migration_apply(
        vec![(TREE_XATTRS, intent_key(tx_id).to_vec(), value)],
        vec![],
    )
    .await
    .expect("plant the open intent");
    assert!(
        !kv.xv_scan_intents().await.expect("scan intents").is_empty(),
        "the fixture must leave a discoverable open intent"
    );
}

/// How many dentry RECORDS carry `name`. The precise probe these tests
/// need: a dangling name can never be `lookup`ed (the lookup resolves the
/// child ino and then fails to read its record — that IS the damage), so
/// "is the name still there" has to be asked of the dentry tree itself.
async fn name_records(fx: &Fx, name: &str) -> usize {
    use squeezefs::meta_backend::kv::node::key_successor;
    use squeezefs::meta_backend::kv::tree::KEY_SPACE_MAX;
    let mut found = 0;
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
            for (_, v) in &page {
                if let Ok(d) = DentryValue::decode(v) {
                    if d.name == name.as_bytes() {
                        found += 1;
                    }
                }
            }
        }
    }
    found
}

/// Retire the planted intent (the plan completed) — what makes the
/// exemption test non-vacuous.
async fn retire_open_intent(fx: &Fx, tx_id: u64) {
    use squeezefs::meta_backend::crossvol_tx::intent_key;
    let kv = &fx.meta.volumes[0];
    kv.migration_apply(vec![], vec![(TREE_XATTRS, intent_key(tx_id).to_vec())])
        .await
        .expect("retire the intent");
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

fn class_findings<'a>(
    report: &'a FsckReport,
    class: &str,
) -> Vec<&'a squeezefs::fsck::FsckFinding> {
    report
        .findings
        .iter()
        .filter(|f| f.class == class)
        .collect()
}

fn c10(report: &FsckReport) -> Vec<&squeezefs::fsck::FsckFinding> {
    class_findings(report, "C10")
}

fn assert_zero_findings(report: &FsckReport, what: &str) {
    assert!(
        report.findings.is_empty(),
        "{what}: fsck_findings must be 0 on a healthy volume (tripwire), got {:?}",
        report.findings
    );
}

/// The dry-run plan's verb for a class (the §5.6a table verb an operator
/// reads before deciding).
fn planned_verbs(plan: &squeezefs::fsck::RepairReport, class: &str) -> Vec<String> {
    plan.planned
        .iter()
        .filter(|a| a.class == class)
        .map(|a| a.action.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Direction 1 — `nlink` too high (the leak class)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_nlink_above_the_name_count_is_reported_as_the_leak_direction() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    // Healthy neighbours, including a genuine hardlink pair, so the class
    // must discriminate rather than fire on multiplicity.
    for i in 0..6 {
        create_file(&fx, &format!("keep{i}")).await;
    }
    let shared = create_file(&fx, "shared.bin").await;
    hardlink(&fx, shared, "shared.link").await;

    // The damage: two names and nlink 2, then ONE name lost — a
    // cross-volume `link` whose dentry step never committed.
    let damaged = create_file(&fx, "leaked.bin").await;
    striped_burst(&fx, damaged, 2).await;
    hardlink(&fx, damaged, "leaked.link").await;
    assert_eq!(
        read_record(&fx, damaged).await.map(|v| v.nlink),
        Some(2),
        "the fixture starts from a real two-name hardlink"
    );
    drop_one_name(&fx, damaged).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let found = c10(&report);
    assert_eq!(
        found.len(),
        1,
        "exactly one nlink/name disagreement must be reported: {:?}",
        report.findings
    );
    assert!(
        found[0].object.contains(&damaged.to_string()),
        "the finding names the damaged ino {damaged}: {:?}",
        found[0]
    );
    assert!(
        found[0].evidence.contains("exceeds") && found[0].evidence.contains("nlink 2"),
        "the leak direction states the numbers it observed: {:?}",
        found[0]
    );
    assert!(
        !found[0].evidence.contains("DATA-LOSS RISK"),
        "the leak direction must NOT wear the dangerous marker (it is a leak, \
         not loss): {:?}",
        found[0]
    );
    // Requirement: a wrong nlink does not corrupt block accounting, so the
    // block classes must coexist silently.
    assert_eq!(
        report.findings.len(),
        1,
        "no block class may fire on nlink damage: {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Direction 2 — `nlink` below the name count (the dangerous class)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_nlink_below_the_name_count_is_reported_and_distinguished() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    for i in 0..4 {
        create_file(&fx, &format!("keep{i}")).await;
    }
    // Both directions in ONE scan: the report must let an operator tell
    // them apart, because only one of them is a data-loss risk.
    let too_high = create_file(&fx, "high.bin").await;
    hardlink(&fx, too_high, "high.link").await;
    drop_one_name(&fx, too_high).await;

    let too_low = create_file(&fx, "low.bin").await;
    hardlink(&fx, too_low, "low.link").await;
    // The count step was lost, the name step landed: two names, nlink 1.
    force_nlink(&fx, too_low, 1).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let found = c10(&report);
    assert_eq!(
        found.len(),
        2,
        "both directions must be reported: {:?}",
        report.findings
    );
    let low = found
        .iter()
        .find(|f| f.object.contains(&too_low.to_string()))
        .unwrap_or_else(|| panic!("the below-count inode {too_low} is missing: {found:?}"));
    let high = found
        .iter()
        .find(|f| f.object.contains(&too_high.to_string()))
        .unwrap_or_else(|| panic!("the above-count inode {too_high} is missing: {found:?}"));
    assert!(
        low.evidence.contains("DATA-LOSS RISK") && low.evidence.contains("below"),
        "the dangerous direction must say so — this is the one an operator stops \
         and reads: {low:?}"
    );
    assert!(
        high.evidence.contains("exceeds"),
        "the leak direction stays distinguishable: {high:?}"
    );
    // Distinguishable in the PLAN too: raising a count is safe, lowering
    // one is not, so they can never share a verb.
    let plan = run_repair(&fx.ctx(), &report, &dry_run())
        .await
        .expect("plan");
    let mut verbs = planned_verbs(&plan, "C10");
    verbs.sort();
    verbs.dedup();
    assert_eq!(
        verbs.len(),
        2,
        "the two directions must plan DIFFERENT verbs: {:?}",
        plan.planned
    );
    assert!(
        verbs.iter().any(|v| v.contains("raise")) && verbs.iter().any(|v| v.contains("lower")),
        "the verbs must name the direction (raise vs lower): {verbs:?}"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// `nlink == 0` with a live name — unambiguous damage, the priority shape
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_zero_nlink_with_a_live_name_is_the_dangerous_class() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    for i in 0..4 {
        create_file(&fx, &format!("keep{i}")).await;
    }
    let doomed = create_file(&fx, "reclaimable.bin").await;
    striped_burst(&fx, doomed, 2).await;
    // A count of 0 while a path still resolves: ordinary reclaim is
    // entitled to destroy this inode. No legitimate state has this shape.
    force_nlink(&fx, doomed, 0).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let found = c10(&report);
    assert_eq!(
        found.len(),
        1,
        "an nlink-0 inode with a live name must be reported: {:?}",
        report.findings
    );
    assert!(
        found[0].object.contains(&doomed.to_string())
            && found[0].evidence.contains("DATA-LOSS RISK")
            && found[0].evidence.contains("nlink 0"),
        "the finding must name the ino and flag the dangerous class: {:?}",
        found[0]
    );
    // The repair direction: raise the count to the names that exist —
    // dropping names is data loss and is never the repair.
    let plan = run_repair(&fx.ctx(), &report, &dry_run())
        .await
        .expect("plan");
    assert!(
        planned_verbs(&plan, "C10")
            .iter()
            .all(|v| v.contains("raise")),
        "an nlink-0 inode with names is repaired by RAISING the count: {:?}",
        plan.planned
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The repair-order consequence of the same shape: raising the count
// re-attaches blocks the census walked as unreferenced
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_raising_a_zero_count_runs_before_the_block_classes_and_saves_the_blocks() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    let doomed = create_file(&fx, "reclaimable.bin").await;
    striped_burst(&fx, doomed, 2).await;
    let offsets: Vec<u64> = fx
        .fs
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(doomed))
        .await
        .expect("layout")
        .block_map
        .as_deref()
        .map(|m| {
            m.values()
                .map(|k| {
                    fx.fs
                        .router
                        .backend_router
                        .parse_block_key(k)
                        .expect("parse")
                        .1
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(offsets.len(), 2, "the fixture owns two blocks");
    force_nlink(&fx, doomed, 0).await;

    // Since the corpse-census correction (2026-08-23) the census COUNTS a
    // `nlink == 0` inode's layout — a corpse owns its blocks until reclaim
    // — so this shape has no block-plane face at all: the blocks were
    // never "unreferenced", and the ordering hazard this test was born to
    // guard (a C2 repair freeing blocks the C10 raise re-attaches) is now
    // unreachable from it BY CONSTRUCTION. The strictly stronger contract
    // pinned here: zero C2 findings, the raise applies, and the blocks
    // survive untouched.
    let fx_ctx = fx.ctx();
    let report = run_fsck(&fx_ctx, &online_opts()).await.expect("fsck");
    assert_eq!(c10(&report).len(), 1, "{:?}", report.findings);
    assert!(
        class_findings(&report, "C2").is_empty(),
        "a zero-count-with-a-name inode's blocks are OWNED (the corpse-census \
         correction) — the block plane must not read them leaked: {:?}",
        report.findings
    );

    let rep = run_repair(&fx_ctx, &report, &apply()).await.expect("apply");
    assert_eq!(
        rep.applied.iter().filter(|a| a.class == "C10").count(),
        1,
        "the count is raised: applied {:?} / refused {:?}",
        rep.applied,
        rep.refused
    );
    assert_eq!(
        rep.applied.iter().filter(|a| a.class == "C2").count(),
        0,
        "no block may be freed once the inode plane re-attached it: {:?}",
        rep.applied
    );
    let alloc = fx.default_allocator(&recs[0].id);
    for off in &offsets {
        assert!(
            alloc.refcount(*off).is_some(),
            "block {off} must survive: the repair order is what protects it"
        );
    }
    assert_zero_findings(
        &run_fsck(&fx_ctx, &online_opts()).await.expect("fsck"),
        "after raising the count the block plane agrees again",
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// A dentry naming an inode that does not exist
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_dangling_dentry_naming_a_missing_inode_is_reported() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    for i in 0..5 {
        create_file(&fx, &format!("keep{i}")).await;
    }
    let ghost = create_file(&fx, "ghost.bin").await;
    destroy_record_keep_name(&fx, ghost).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let found = c10(&report);
    assert_eq!(
        found.len(),
        1,
        "a name resolving to nothing must be reported: {:?}",
        report.findings
    );
    assert!(
        found[0].evidence.contains("dangling") && found[0].evidence.contains(&ghost.to_string()),
        "the finding names the dentry and the ino it fails to resolve: {:?}",
        found[0]
    );
    assert!(
        found[0].object.contains("ghost.bin"),
        "the object identifies the NAME an operator can see: {:?}",
        found[0]
    );
    let plan = run_repair(&fx.ctx(), &report, &dry_run())
        .await
        .expect("plan");
    assert!(
        planned_verbs(&plan, "C10")
            .iter()
            .any(|v| v.contains("remove-dangling-dentry")),
        "a name that resolves to nothing can only be removed: {:?}",
        plan.planned
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The tripwire: healthy volumes report zero, hardlinks and dirs included
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_healthy_volume_with_hardlinks_and_dirs_reports_zero_findings() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    for i in 0..16 {
        let ino = create_file(&fx, &format!("f{i}")).await;
        if i % 5 == 0 {
            striped_burst(&fx, ino, 2).await;
        }
    }
    // Three names of one inode: nlink 3, three dentry records. The
    // count arms must accept it exactly.
    let triple = create_file(&fx, "triple.bin").await;
    hardlink(&fx, triple, "triple.b").await;
    hardlink(&fx, triple, "triple.c").await;
    assert_eq!(
        read_record(&fx, triple).await.map(|v| v.nlink),
        Some(3),
        "the fixture is a real three-name hardlink"
    );
    // A directory tree: `.` and `..` are synthesized, never records, so a
    // directory's nlink (2 + subdirs) is NOT its number of names — the
    // trap a naive count comparison falls into on every directory.
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
    for n in 0..3 {
        fx.fs
            .mkdir(
                req(),
                sub,
                OsStr::new(&format!("sub{n}")),
                libc::S_IFDIR | 0o755,
                0,
            )
            .await
            .unwrap();
    }
    fx.fs
        .create(req(), sub, OsStr::new("nested"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();
    assert!(
        read_record(&fx, sub).await.map(|v| v.nlink).unwrap_or(0) >= 5,
        "the fixture's directory really does carry nlink = 2 + subdirs"
    );

    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "same-mount healthy population",
    );
    fx.close().await;

    // Every inode is now prior-era — the same population, re-derived from
    // the durable tree.
    let fx = open_fixture(&meta, &recs).await;
    assert_zero_findings(
        &run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck"),
        "post-remount healthy population",
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The POSIX-15 line: `nlink == 0` WITHOUT a name stays unclaimed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_zero_nlink_without_a_name_is_never_claimed() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    for i in 0..4 {
        create_file(&fx, &format!("keep{i}")).await;
    }
    // Unlinked-but-open / POSIX-15's rename-overwrite orphan: nlink 0 and
    // no name. Telling that apart from a corpse nobody will FORGET needs
    // the live open-count registries fsck does not carry, so NEITHER C9
    // (which excludes nlink 0) nor C10 (which requires a live name) may
    // claim it.
    let unlinked = create_file(&fx, "open-then-unlinked.bin").await;
    striped_burst(&fx, unlinked, 1).await;
    force_nlink(&fx, unlinked, 0).await;
    drop_one_name(&fx, unlinked).await;
    fx.close().await;

    // Prior-era too, so no era filter is doing the work.
    let fx = open_fixture(&meta, &recs).await;
    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        c10(&report).is_empty(),
        "an nlink-0 inode with NO name is out of scope by design (the open-count \
         registries are what could judge it): {:?}",
        report.findings
    );
    assert!(
        class_findings(&report, "C9").is_empty(),
        "C9 excludes the nlink-0 shape too — the line must not move: {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The in-flight guard: an open cross-volume intent exempts its inos
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_an_open_cross_volume_intent_exempts_the_inode() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    for i in 0..4 {
        create_file(&fx, &format!("keep{i}")).await;
    }
    // A REAL disagreement (nlink 2, one name) — the exact shape a
    // cross-volume `link` shows while its two commits are in flight.
    let inflight = create_file(&fx, "inflight.bin").await;
    force_nlink(&fx, inflight, 2).await;

    const TX: u64 = 0x0000_0042_dead_beef;
    plant_open_intent(&fx, inflight, TX).await;
    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(
        c10(&report).is_empty(),
        "an ino named by an OPEN cross-volume intent is in flight by definition and \
         must be exempt: {:?}",
        report.findings
    );

    // …and the guard is not vacuous: retire the intent (the plan
    // completed) and the same damage IS reported.
    retire_open_intent(&fx, TX).await;
    let after = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(
        c10(&after).len(),
        1,
        "with no intent open the same disagreement must be reported (the exemption \
         must not be a blanket silence): {:?}",
        after.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Repair: dry run mutates nothing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c10_repair_dry_run_plans_and_mutates_nothing() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    let damaged = create_file(&fx, "damaged.bin").await;
    striped_burst(&fx, damaged, 2).await;
    hardlink(&fx, damaged, "damaged.link").await;
    drop_one_name(&fx, damaged).await;
    let ghost = create_file(&fx, "ghost.bin").await;
    destroy_record_keep_name(&fx, ghost).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(c10(&report).len(), 2, "{:?}", report.findings);

    let plan = run_repair(&fx.ctx(), &report, &dry_run())
        .await
        .expect("plan");
    assert!(
        plan.dry_run && plan.applied.is_empty(),
        "a dry run applies nothing: {plan:?}"
    );
    assert_eq!(
        planned_verbs(&plan, "C10").len(),
        2,
        "both findings plan an action: {:?}",
        plan.planned
    );
    // Nothing moved: the count is still wrong and the ghost name is still
    // there, so a re-scan finds exactly the same two.
    assert_eq!(
        read_record(&fx, damaged).await.map(|v| v.nlink),
        Some(2),
        "dry run must not touch nlink"
    );
    let again = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(c10(&again).len(), 2, "{:?}", again.findings);
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Repair applied, per direction
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c10_repair_applied_sets_nlink_to_the_counted_names() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    // Engage the durable block-reference ledger so the C8 oracle grades
    // the run: an nlink repair must leave block accounting untouched.
    std::env::set_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS", "1");
    format_meta(&meta, &[&oss1]).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS");
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    assert!(
        fx.meta.volumes[0].block_refs_engaged(),
        "the durable ledger must be engaged for this leg"
    );
    // Direction 1: nlink 2, one name ⇒ lower to 1.
    let high = create_file(&fx, "high.bin").await;
    striped_burst(&fx, high, 2).await;
    hardlink(&fx, high, "high.link").await;
    drop_one_name(&fx, high).await;
    // Direction 2: nlink 1, two names ⇒ raise to 2.
    let low = create_file(&fx, "low.bin").await;
    striped_burst(&fx, low, 1).await;
    hardlink(&fx, low, "low.link").await;
    force_nlink(&fx, low, 1).await;
    // Direction 2 (the unambiguous face): nlink 0, one name ⇒ raise to 1.
    let zero = create_file(&fx, "zero.bin").await;
    force_nlink(&fx, zero, 0).await;
    // A name resolving to nothing ⇒ remove the name.
    let ghost = create_file(&fx, "ghost.bin").await;
    destroy_record_keep_name(&fx, ghost).await;

    let alloc = fx.default_allocator(&recs[0].id);
    let high_offsets: Vec<u64> = {
        let meta_l = fx
            .fs
            .router
            .fetch_metadata(&squeezefs::keys::inode_path(high))
            .await
            .expect("layout");
        meta_l
            .block_map
            .as_deref()
            .map(|m| {
                m.values()
                    .map(|k| {
                        fx.fs
                            .router
                            .backend_router
                            .parse_block_key(k)
                            .expect("parse")
                            .1
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(c10(&report).len(), 4, "{:?}", report.findings);

    let rep = run_repair(&fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert_eq!(
        rep.applied.iter().filter(|a| a.class == "C10").count(),
        4,
        "applied: {:?} / refused: {:?}",
        rep.applied,
        rep.refused
    );
    // Quarantine-first: every mutation copied its record bytes aside.
    assert!(
        rep.counters.quarantined_records >= 4 && rep.counters.quarantined_bytes > 0,
        "quarantine-first: {:?}",
        rep.counters
    );

    assert_eq!(
        read_record(&fx, high).await.map(|v| v.nlink),
        Some(1),
        "the leak direction is corrected DOWN to the counted names"
    );
    assert_eq!(
        read_record(&fx, low).await.map(|v| v.nlink),
        Some(2),
        "the dangerous direction is corrected UP to the counted names"
    );
    assert_eq!(
        read_record(&fx, zero).await.map(|v| v.nlink),
        Some(1),
        "an nlink-0 inode with a name is made live again, never unnamed"
    );
    assert_eq!(
        name_records(&fx, "ghost.bin").await,
        0,
        "the name that resolved to nothing is gone from the dentry tree"
    );
    // The blocks of the repaired inode are untouched: this class never
    // moves the block plane.
    for off in &high_offsets {
        assert!(
            alloc.refcount(*off).is_some(),
            "block {off} must survive an nlink repair"
        );
    }
    // And the volume is clean afterwards — no C10, and no block-class
    // drift introduced by the repairs (C2/C3/C6/C8 included).
    let clean = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_zero_findings(&clean, "after the C10 repairs");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Repair refuses where the evidence is ambiguous
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c10_repair_refuses_ambiguous_evidence() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    // (a) A dangling name that named a DIRECTORY: removing it must also
    // decrement the parent's directory nlink, and doing that on a count
    // this very class cannot verify is exactly the ambiguity the posture
    // refuses. Report, never guess.
    fx.fs
        .mkdir(req(), 1, OsStr::new("ghostdir"), libc::S_IFDIR | 0o755, 0)
        .await
        .unwrap();
    let gdir = fx
        .fs
        .lookup(req(), 1, OsStr::new("ghostdir"))
        .await
        .unwrap()
        .attr
        .ino;
    destroy_record_keep_name(&fx, gdir).await;
    // (b) A count disagreement that HEALS between detection and repair:
    // verify-before-repair must refuse it rather than re-apply a stale
    // verdict.
    let healed = create_file(&fx, "healed.bin").await;
    hardlink(&fx, healed, "healed.link").await;
    force_nlink(&fx, healed, 1).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert_eq!(c10(&report).len(), 2, "{:?}", report.findings);
    // The world moves on: the count is corrected by ordinary means.
    force_nlink(&fx, healed, 2).await;

    let rep = run_repair(&fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert_eq!(
        rep.applied.iter().filter(|a| a.class == "C10").count(),
        0,
        "neither ambiguous finding may be acted on: applied {:?}",
        rep.applied
    );
    assert_eq!(
        rep.refused.iter().filter(|a| a.class == "C10").count(),
        2,
        "both must be REFUSED with a reason: {:?}",
        rep.refused
    );
    assert!(
        rep.refused
            .iter()
            .any(|a| a.detail.contains("directory") || a.detail.contains("parent")),
        "the directory-name refusal must say why: {:?}",
        rep.refused
    );
    assert!(
        rep.refused
            .iter()
            .any(|a| a.detail.contains("heal") || a.detail.contains("matches")),
        "the healed finding's refusal must say why: {:?}",
        rep.refused
    );
    // Nothing was destroyed: the ghost directory name is still there for
    // an operator to act on. (Asked of the dentry tree, not of `lookup` —
    // a dangling name never resolves, which is the damage itself.)
    assert_eq!(
        name_records(&fx, "ghostdir").await,
        1,
        "a refused repair must not have removed the name"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Offline sharding: exactly one finding across the merge, no double count
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_sharded_scan_reports_exactly_one_c10_finding_across_the_merge() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    // Enough neighbours (and a healthy hardlink pair) that every shard
    // holds inodes whose NAMES live outside its own ino residue — the
    // sharding trap: a shard that filtered dentries by key instead of by
    // target ino would mis-count every one of them.
    for i in 0..24 {
        create_file(&fx, &format!("f{i}")).await;
    }
    let pair = create_file(&fx, "pair.bin").await;
    hardlink(&fx, pair, "pair.link").await;
    let damaged = create_file(&fx, "damaged.bin").await;
    hardlink(&fx, damaged, "damaged.link").await;
    force_nlink(&fx, damaged, 1).await;
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    let ctx = fx.ctx();
    let full = run_fsck(&ctx, &offline_opts()).await.expect("full scan");
    assert_eq!(c10(&full).len(), 1, "{:?}", full.findings);

    const N: u32 = 3;
    let mut shard_reports = Vec::new();
    for k in 0..N {
        let mut opts = offline_opts();
        opts.shard = Some((k, N));
        let r = run_fsck(&ctx, &opts).await.expect("shard scan");
        assert!(
            c10(&r).len() <= 1,
            "shard {k}/{N} reported outside its own residue: {:?}",
            r.findings
        );
        shard_reports.push(r);
    }
    assert_eq!(
        shard_reports.iter().filter(|r| !c10(r).is_empty()).count(),
        1,
        "exactly one shard owns the damaged ino's residue (no double counting)"
    );
    let merged = merge_reports(&shard_reports);
    assert_eq!(
        c10(&merged).len(),
        1,
        "the union must carry exactly one C10 finding: {:?}",
        merged.findings
    );
    assert_eq!(
        c10(&merged)[0].object,
        c10(&full)[0].object,
        "the sharded union names the same inode as the full scan"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// §10 gauges: the counting extension, the per-direction triage split, the
// guard's engagement, and the repair class
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c10_counters_and_repair_class_gauge_move() {
    use std::sync::atomic::Ordering;
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    // A healthy hardlink so the counting extension has something to count,
    // and one of each direction so the triage gauges can be told apart.
    let pair = create_file(&fx, "pair.bin").await;
    hardlink(&fx, pair, "pair.link").await;
    let high = create_file(&fx, "high.bin").await;
    hardlink(&fx, high, "high.link").await;
    drop_one_name(&fx, high).await;
    let low = create_file(&fx, "low.bin").await;
    hardlink(&fx, low, "low.link").await;
    force_nlink(&fx, low, 1).await;
    let zero = create_file(&fx, "zero.bin").await;
    force_nlink(&fx, zero, 0).await;
    let ghost = create_file(&fx, "ghost.bin").await;
    destroy_record_keep_name(&fx, ghost).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    let c = &report.counters;
    assert!(
        c.nlink_names_counted >= 2,
        "the counting extension must account the multi-named population \
         (the healthy hardlink included): {c:?}"
    );
    assert_eq!(
        (
            c.nlink_mismatch_high,
            c.nlink_mismatch_low,
            c.nlink_zero_named,
            c.dangling_dentries
        ),
        (1, 1, 1, 1),
        "each direction is counted on its own gauge — the last three are the \
         DATA-LOSS direction an operator triages on: {c:?}"
    );
    assert_eq!(c.findings, 4, "{:?}", report.findings);

    let before = squeezefs::fuse_client::METRICS.fsck_repair_class[9].load(Ordering::Relaxed);
    let rep = run_repair(&fx.ctx(), &report, &apply())
        .await
        .expect("apply");
    assert_eq!(
        rep.counters.per_class.get("C10").copied(),
        Some(4),
        "per-class repair accounting: {:?}",
        rep.counters
    );
    assert!(
        squeezefs::fuse_client::METRICS.fsck_repair_class[9].load(Ordering::Relaxed) >= before + 4,
        "fsck_repair_classC10 moved"
    );
    fx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_the_guard_gauge_moves_when_a_guard_clears_a_suspect() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    let inflight = create_file(&fx, "inflight.bin").await;
    force_nlink(&fx, inflight, 2).await;
    plant_open_intent(&fx, inflight, 0x0000_0077_dead_beef).await;

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    assert!(c10(&report).is_empty(), "{:?}", report.findings);
    assert!(
        report.counters.nlink_transient_cleared >= 1,
        "a cleared suspect must be ATTRIBUTED to the guard that cleared it — a shield \
         with no engagement gauge cannot be shown to be non-vacuous: {:?}",
        report.counters
    );
    assert!(
        report.counters.suspects >= 1 && report.counters.suspects_cleared >= 1,
        "the suspect was formed and then cleared (not never nominated): {:?}",
        report.counters
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The derivation's tie test (drift is red)
// ---------------------------------------------------------------------------

#[test]
fn test_c10_count_entry_budget_is_derived_not_tuned() {
    let entries = squeezefs::fsck::c10_count_entry_budget();
    let ino_set = squeezefs::fsck::ino_set_byte_budget();

    // The count maps ride the SAME byte budget as ONE C9 ino set: the only
    // constant is the charged bytes per entry (a `(parent, name)` pair or an
    // `(ino, nlink)` pair with its table overhead), which is an accounting
    // factor, not a tuning knob.
    const CHARGED_BYTES_PER_ENTRY: u64 = 64;
    assert_eq!(
        entries,
        ino_set / CHARGED_BYTES_PER_ENTRY,
        "the C10 count budget must stay derived from the ino-set byte budget; a \
         free-floating entry constant here is a program violation"
    );
    // And it must be able to hold a real hardlink population: at the design
    // cap's own floor (16 MiB of bits) that is a quarter-million names.
    assert!(
        entries >= 250_000,
        "the budget must hold a field-scale multi-named population, got {entries}"
    );
}

// ---------------------------------------------------------------------------
// Links and unlinks racing a scan (the shapes the guards exist for)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_links_and_unlinks_racing_a_scan_produce_no_findings() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    // A populated, healthy, prior-era volume: every inode the scan walks
    // is a candidate by age, so only the count guards can save the
    // link/unlink/rename traffic that lands during the walk.
    let fx = open_fixture(&meta, &recs).await;
    let mut targets = Vec::new();
    for i in 0..32 {
        targets.push(create_file(&fx, &format!("old{i}")).await);
    }
    fx.close().await;

    let fx = open_fixture(&meta, &recs).await;
    let fs = fx.fs.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_writer = stop.clone();
    let churn = tokio::spawn(async move {
        let mut n = 0u32;
        while !stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
            let ino = targets[(n as usize) % targets.len()];
            let name = format!("l{n}");
            // link → rename → unlink: every op that moves a name, and
            // every window where the count and the names disagree.
            if fs.link(req(), ino, 1, OsStr::new(&name)).await.is_ok() {
                let moved = format!("m{n}");
                let _ = fs
                    .rename(req(), 1, OsStr::new(&name), 1, OsStr::new(&moved))
                    .await;
                let _ = fs.unlink(req(), 1, OsStr::new(&moved)).await;
                let _ = fs.unlink(req(), 1, OsStr::new(&name)).await;
            }
            n += 1;
            tokio::task::yield_now().await;
        }
        n
    });

    let report = run_fsck(&fx.ctx(), &online_opts()).await.expect("fsck");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let churned = churn.await.expect("churn task");
    assert!(churned > 0, "the racing churn must have done work");
    assert!(
        c10(&report).is_empty(),
        "link/rename/unlink racing the scan produced C10 findings: {:?}",
        report.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// §5.9.3 / KD-PV-8 — the repair-CONSEQUENCE split under a multi-owner plane
// ---------------------------------------------------------------------------

/// The online posture of a node that owns SOME of a set's volumes
/// (`docs/design-per-volume-claim-admission.md` §5.9.3).
fn multi_owner_opts() -> FsckOptions {
    let mut o = online_opts();
    o.owned_volumes = Some(vec![0]);
    o.multi_owner = true;
    o
}

fn multi_owner_apply() -> RepairOptions {
    RepairOptions {
        apply: true,
        quarantine_dir: None,
        multi_owner: true,
    }
}

/// Contract (KD-PV-8): the SAFE RAISES stay online. Their false-positive
/// source is an OVERCOUNTED reference set — the direction a
/// monotone-behind reader projection errs in — and their FP consequence
/// is a leak the next pass corrects, while their true-positive
/// consequence is restoring an inode a live path still resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c10_low_and_zero_named_raises_still_apply_online() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    let too_low = create_file(&fx, "low.bin").await;
    hardlink(&fx, too_low, "low.link").await;
    force_nlink(&fx, too_low, 1).await;
    let zero_named = create_file(&fx, "zero.bin").await;
    force_nlink(&fx, zero_named, 0).await;

    let report = run_fsck(&fx.ctx(), &multi_owner_opts())
        .await
        .expect("fsck");
    assert_eq!(
        c10(&report).len(),
        2,
        "detection is unchanged under multi-owner: {:?}",
        report.findings
    );
    let rep = run_repair(&fx.ctx(), &report, &multi_owner_apply())
        .await
        .expect("apply");
    assert_eq!(
        rep.counters.refused_multi_owner, 0,
        "a RAISE is never declined for the multi-owner cause: {rep:?}"
    );
    assert_eq!(
        rep.counters.applied, 2,
        "both raises apply online: {:?}",
        rep.refused
    );
    assert_eq!(read_record(&fx, too_low).await.expect("record").nlink, 2);
    assert_eq!(read_record(&fx, zero_named).await.expect("record").nlink, 1);
    fx.close().await;
}

/// Contract (KD-PV-8 / §5.9.1): the DESTRUCTIVE-and-DANGEROUS pair of
/// C10 — `lower-nlink-to-counted-names` (the code's own text: *"the one
/// C10 repair that could make a named inode reclaimable if the count
/// were wrong"*) and `remove-dangling-dentry` (a live name disappears) —
/// go report-only under a multi-owner plane, counted, naming the offline
/// pass. Detection is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c10_lower_and_dangling_removal_refuse_online_under_multi_owner() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    let fx = open_fixture(&meta, &recs).await;
    let too_high = create_file(&fx, "high.bin").await;
    hardlink(&fx, too_high, "high.link").await;
    drop_one_name(&fx, too_high).await;
    let dangling = create_file(&fx, "dangling.bin").await;
    destroy_record_keep_name(&fx, dangling).await;

    let report = run_fsck(&fx.ctx(), &multi_owner_opts())
        .await
        .expect("fsck");
    assert_eq!(
        c10(&report).len(),
        2,
        "both shapes are still DETECTED under multi-owner: {:?}",
        report.findings
    );
    let rep = run_repair(&fx.ctx(), &report, &multi_owner_apply())
        .await
        .expect("repair runs and refuses");
    assert_eq!(rep.counters.applied, 0, "{rep:?}");
    assert_eq!(rep.counters.refused_multi_owner, 2, "{rep:?}");
    for action in &rep.refused {
        let detail = action.detail.to_lowercase();
        assert!(
            detail.contains("offline") && detail.contains("multi-owner"),
            "each refusal names its cause and the pass with full teeth: {action:?}"
        );
    }
    assert_eq!(
        read_record(&fx, too_high).await.expect("record").nlink,
        2,
        "the count is untouched — lowering it is what could strand the inode"
    );
    assert_eq!(
        name_records(&fx, "dangling.bin").await,
        1,
        "the name is untouched — removing it is how a live path disappears"
    );
    fx.close().await;
}
