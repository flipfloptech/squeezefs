//! **Per-volume claim admission — the fsck inode plane under N owners,
//! and the maintenance coordinator** (`docs/design-per-volume-claim-admission.md`
//! §5.8/§5.8.0/§5.8.1/§5.8.2, §5.9, KD-PV-7/8/14/16; PR 6).
//!
//! # The two failures this rung exists to prevent
//!
//! 1. **The mirage** (R3). The inode plane is a ONE-VIEW plane: its
//!    verdicts are census-vs-dentry-pass AGREEMENT, so a pass whose two
//!    halves sit at different instants manufactures the loss-direction
//!    shapes from a healthy tree (the `fix/mw-xv-unlink-c10`
//!    conviction). KD-PV-16 restates the law as *one coherent view per
//!    OWNER over its OWN inos* — an owner reads records it APPENDS to,
//!    plus a cross-owner reference set the freeze law (§5.9.2) makes
//!    unchangeable — and the wire-side predicate (§5.8.2) is what keeps
//!    the restatement from re-admitting the mirage: a proposal is
//!    admitted only when the COORDINATOR's own lease table and its own
//!    `OwnerMap` say the proposer owns the volume the finding is about.
//! 2. **The vacuum** (R17). KD-PV-7 scopes the CANDIDATE set to volumes
//!    the evaluating node owns; composed naively with KD-PV-14's single
//!    coordinator that leaves 1/K of the set unevaluated online — and
//!    `fsck_findings == 0` would pass trivially. Coverage is therefore an
//!    ASSERTION: `fsck_inode_plane_volumes_covered == volume_count`, and
//!    a missing owner shard makes the pass INCOMPLETE, never narrower.
//!
//! # The asymmetry that is load-bearing (§5.8.0)
//!
//! CANDIDATE-scoped, REFERENCED-whole. A name living on a peer's volume
//! can reference an ino this owner is responsible for — and KD-PV-15
//! deliberately creates a whole population of exactly that shape (the K
//! subtree roots, whose dentry lives on the set authority's volume while
//! the ino lives on the assignee's). Scoping BOTH halves would report
//! `C9Unreferenced` for every subtree root on the supported deployment.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{run as run_fsck, run_fleet, FsckCtx, FsckOptions, FsckReport};
use squeezefs::fuse_client::METRICS;
use squeezefs::job_wire::{
    FleetShardSpec, JobWireConfig, JobWireHost, JobWireWorker, ShardDeviceSeam, WorkerOptions,
    CAP_FLEET_READ,
};
use squeezefs::jobs::{FleetDispatch, JobFabric, JobSpec, JobType};
use squeezefs::membership::ClaimSet;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{open_routed_meta_set, Metadata as _, RoutedMetaBackend};
use squeezefs::meta_ship::owners::{self as owners};
use squeezefs::meta_ship::{OwnerMap, PeerOwner};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tempfile::TempDir;

const BLOCK: usize = 4096;
const VOL_LEN: u64 = 256 * 1024 * 1024;

/// This node (the set authority, owner of the slot-0 volume).
const NODE: &str = "node_00000000deadbeef.m00000001";
/// The peers this suite assigns volumes 1.. to.
const PEERS: [&str; 3] = [
    "node_00000000feedface.m00000001",
    "node_00000000c0ffee00.m00000001",
    "node_00000000badc0de0.m00000001",
];

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// The ownership plane is process-global: every armed test disarms
/// however it ends (the `pv_owner_map_tests` precedent).
struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        owners::disarm_ownership();
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

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// A `k`-volume stamped set carrying the claim-set capability (bit 14) —
/// the assignment record's home.
async fn stamped_set(dir: &Path, k: usize) -> Vec<String> {
    let plan = squeezefs::meta_backend::plan_meta_slot_set(k).expect("derived slot plan");
    let mut paths = Vec::new();
    for i in 0..k {
        let p = make_file(dir, &format!("meta{i}"), VOL_LEN);
        format_v3_stamped(&p, VOL_LEN, &opts(), plan.stamps[i].clone())
            .await
            .expect("format stamped meta volume");
        sb::set_claim_set_bit(&p).await.expect("stamp bit 14");
        paths.push(p.display().to_string());
    }
    paths
}

/// Write volume `i`'s durable ownership assignment — the ONE observable
/// fact §5.9.2's freeze precondition rests on (PR 7's verb is what does
/// this in production; here it is the fixture).
async fn assign_owner(path: &str, owner: &str) {
    let be = KvMetaBackend::open(Path::new(path))
        .await
        .expect("assignment open");
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.owner = Some(owner.to_string());
    ClaimSet::store(&be, &set).await.expect("store claim set");
    be.sync_device().await.expect("barrier");
    be.shutdown().await.expect("release");
}

/// The fsck fixture over a `k`-volume metadata set: a live routed set
/// plus a one-data-volume router (the block plane is silent on these
/// fixtures — the inode plane is what is under test).
struct Fx {
    meta: Arc<RoutedMetaBackend>,
    router: DataRouter,
    staging_path: PathBuf,
    paths: Vec<String>,
    records: Vec<DataVolumeRecord>,
    _staging: TempDir,
    dir: TempDir,
}

async fn open_fixture(dir: TempDir, paths: &[String], records: &[DataVolumeRecord]) -> Fx {
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
    let router = DataRouter::new(dlm, cache, first_alloc, first_dev);
    for rec in records {
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());
    let meta = open_routed_meta_set(paths).await.expect("open the set");
    router.set_meta_backend(meta.clone());
    Fx {
        meta,
        router,
        staging_path,
        paths: paths.to_vec(),
        records: records.to_vec(),
        _staging: staging,
        dir,
    }
}

/// A `k`-volume set with volumes `1..k` assigned to `PEERS[..k-1]` and
/// volume 0 (slot 0 — the SET AUTHORITY's) to this node.
async fn assigned_fixture(k: usize) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), k).await;
    assign_owner(&paths[0], NODE).await;
    for (i, path) in paths.iter().enumerate().skip(1) {
        assign_owner(path, PEERS[i - 1]).await;
    }
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    open_fixture(dir, &paths, &recs).await
}

impl Fx {
    fn ctx(&self) -> FsckCtx {
        FsckCtx {
            meta: self.meta.clone(),
            router: self.router.clone(),
            staging_dirs: vec![self.staging_path.clone()],
            expected_generation: None,
        }
    }

    /// Arm this node's ownership plane: volumes `1..k` belong to their
    /// assigned peers, volume 0 is ours.
    fn arm(&self) -> ArmGuard {
        let foreign: Vec<(usize, PeerOwner)> = (1..self.meta.volumes.len())
            .map(|v| {
                (
                    v,
                    PeerOwner::new(PEERS[v - 1], format!("127.0.0.1:71{v:02}")),
                )
            })
            .collect();
        owners::arm_ownership(OwnerMap::for_volumes(&self.meta, foreign).expect("owner map"));
        ArmGuard
    }

    /// Remount: close the set and re-open it, which is what makes the
    /// seeded damage PRIOR-era — C9's whole zero-FP shield is the writer
    /// era's ino floor, so residue this mount minted is (correctly) never
    /// a candidate for it.
    async fn remount(self) -> Fx {
        let paths = self.paths.clone();
        let records = self.records.clone();
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
        let dir = self.dir;
        drop(self.meta);
        drop(self.router);
        open_fixture(dir, &paths, &records).await
    }

    async fn close(self) {
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

/// Online options with a short settle window (the whole ladder still
/// runs; only the wall clock shrinks).
fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = Duration::from_millis(100);
    o
}

/// The posture of an OWNER evaluating the plane over `owned`.
fn owner_opts(owned: &[usize]) -> FsckOptions {
    let mut o = online_opts();
    o.owned_volumes = Some(owned.to_vec());
    o.multi_owner = true;
    o
}

/// A directory under `parent` whose ino homes on `want_vol` — the KD-PV-15
/// subtree-root shape when `parent` lives on another volume (the dentry
/// stays with the parent; the inode goes to the assignee).
async fn mkdir_on(meta: &RoutedMetaBackend, parent: u64, want_vol: usize, tag: &str) -> u64 {
    for i in 0..128 {
        let ino = meta
            .create(parent, &format!("{tag}_{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir")
            .ino;
        if meta.route_ino(ino).0 == want_vol {
            return ino;
        }
    }
    panic!("directory striping never placed an inode on volume {want_vol}");
}

async fn mkfile_on(meta: &RoutedMetaBackend, parent: u64, want_vol: usize, tag: &str) -> u64 {
    for i in 0..128 {
        let ino = meta
            .create(parent, &format!("{tag}_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        if meta.route_ino(ino).0 == want_vol {
            return ino;
        }
    }
    panic!("file striping never placed an inode on volume {want_vol}");
}

/// Drop the dentry record that NAMES `ino`, leaving its inode behind —
/// the pre-S3.5 crash residue verbatim (the C9 damage fixture).
async fn orphan_inode(meta: &RoutedMetaBackend, ino: u64) {
    use squeezefs::meta_backend::kv::node::key_successor;
    use squeezefs::meta_backend::kv::record::DentryValue;
    use squeezefs::meta_backend::kv::tree::KEY_SPACE_MAX;
    for kv in &meta.volumes {
        let dentries = kv.flat_trees()[1].clone();
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

fn class_findings<'a>(r: &'a FsckReport, class: &str) -> Vec<&'a squeezefs::fsck::FsckFinding> {
    r.findings.iter().filter(|f| f.class == class).collect()
}

// ---------------------------------------------------------------------------
// §5.8.0 — CANDIDATE-scoped, REFERENCED-whole
// ---------------------------------------------------------------------------

/// Contract (§5.8.0, Issue 28 — the cheapest possible insurance on the
/// population KD-PV-15 introduces): a subtree root whose ONLY name lives
/// on the set authority's volume is never a C9 candidate on its owner's
/// shard. Scoping the referenced set the way the candidate set is scoped
/// — the natural misreading — produces K guaranteed false findings on
/// the supported deployment, forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verb_minted_subtree_root_is_never_a_c9_candidate_on_its_owners_shard() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    // The verb's act — performed BEFORE the plane arms, exactly as
    // KD-PV-15 specifies (the offline verb mints each owner's subtree
    // root on the volume it assigns; M2 then pins every descendant to
    // its parent's owner, which is why the roots must pre-exist).
    let root = mkdir_on(&fx.meta, 1, 1, "peer_subtree").await;
    assert_eq!(fx.meta.route_ino(root).0, 1);
    let child = mkfile_on(&fx.meta, root, 1, "under_root").await;
    assert_eq!(fx.meta.route_ino(child).0, 1);
    let _arm = fx.arm();

    let report = run_fsck(&fx.ctx(), &owner_opts(&[1]))
        .await
        .expect("the owner's shard runs");
    assert!(
        class_findings(&report, "C9").is_empty(),
        "the subtree root's only name is CROSS-OWNER: the referenced set must come from a \
         whole-set dentry pass, never from the owned volumes alone — {:?}",
        report.findings
    );
    assert!(
        report.findings.is_empty(),
        "a healthy assigned tree produces no inode-plane finding on its owner's shard: {:?}",
        report.findings
    );
    fx.close().await;
}

/// Contract (§5.8.0): the owner shard's dentry pass walks EVERY volume,
/// not only its own — the engagement half of the asymmetry, measured on
/// the pass's own gauge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_shards_dentry_pass_covers_every_volume_not_only_its_own() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    // Names on BOTH volumes (seeded before the plane arms — see the
    // subtree-root contract): two directories under root (volume 0's
    // dentry tree) each holding a file whose dentry lands on its own
    // parent's volume.
    let d0 = mkdir_on(&fx.meta, 1, 0, "local_dir").await;
    let d1 = mkdir_on(&fx.meta, 1, 1, "peer_dir").await;
    mkfile_on(&fx.meta, d0, 0, "in_local").await;
    mkfile_on(&fx.meta, d1, 1, "in_peer").await;
    let _arm = fx.arm();

    let whole = run_fsck(&fx.ctx(), &online_opts())
        .await
        .expect("the unscoped whole-set pass");
    let owner = run_fsck(&fx.ctx(), &owner_opts(&[1]))
        .await
        .expect("the owner's shard");
    assert_eq!(
        owner.counters.dentry_refs_indexed, whole.counters.dentry_refs_indexed,
        "the CANDIDATE set is owned-only; the REFERENCED set is whole-set — the shard pays a \
         full dentry scan of the set (the honest cost §5.8.0 states)"
    );
    assert!(
        owner.counters.dentry_refs_indexed > 0,
        "a pass that indexed nothing proves nothing"
    );
    fx.close().await;
}

/// Contract (KD-PV-7): the CANDIDATE set is scoped — residue on a volume
/// this node does not append to is reported by that volume's owner, and
/// the scoping is counted rather than silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_c9_candidate_on_a_peer_owned_volume_is_left_to_its_owner() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    let victim = mkfile_on(&fx.meta, 1, 1, "peer_orphan").await;
    orphan_inode(&fx.meta, victim).await;
    let fx = fx.remount().await;
    let _arm = fx.arm();

    // Volume 0's owner sees the damage on volume 1 and declines it.
    let not_mine = run_fsck(&fx.ctx(), &owner_opts(&[0]))
        .await
        .expect("the set authority's shard");
    assert!(
        class_findings(&not_mine, "C9").is_empty(),
        "a peer's volume is not this node's candidate set (its era floor there is a snapshot \
         of a cursor another node advances): {:?}",
        not_mine.findings
    );
    assert!(
        not_mine.counters.inode_plane_foreign_scoped >= 1,
        "the scoping is COUNTED, never silent — the division of labour must be visible"
    );

    // Volume 1's owner reports it.
    let mine = run_fsck(&fx.ctx(), &owner_opts(&[1]))
        .await
        .expect("the owner's shard");
    assert_eq!(
        class_findings(&mine, "C9").len(),
        1,
        "the volume's OWNER reports its residue: {:?}",
        mine.findings
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// §5.9.2 — the freeze precondition, self-certifying
// ---------------------------------------------------------------------------

/// Contract (§5.9.2): the online plane rests on every peer-owned volume's
/// projection showing its own `owner` field — one durable, observable
/// fact. A projection that predates its assignment records NO VERDICT
/// (the existing incomplete-pass law), never a verdict from a set that
/// may still be growing cross-owner names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_volume_projection_predating_its_assignment_records_no_verdict() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let paths = stamped_set(dir.path(), 2).await;
    // Volume 0 is assigned; volume 1 is NOT — the pre-assignment shape.
    assign_owner(&paths[0], NODE).await;
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(dir, &paths, &recs).await;
    let victim = mkfile_on(&fx.meta, 1, 0, "local_orphan").await;
    orphan_inode(&fx.meta, victim).await;
    let fx = fx.remount().await;
    let _arm = fx.arm();

    let report = run_fsck(&fx.ctx(), &owner_opts(&[0]))
        .await
        .expect("the pass runs");
    assert!(
        report.findings.is_empty(),
        "an unassigned peer volume means the cross-owner reference set is not yet frozen: the \
         plane records NO verdict rather than one taken over a moving set: {:?}",
        report.findings
    );
    assert_eq!(
        report.counters.inode_plane_volumes_covered, 0,
        "no verdict ⇒ no coverage claimed (a pass that covers nothing must never read as \
         covering something)"
    );

    // The same damage, once the assignment exists.
    fx.close().await;
}

// ---------------------------------------------------------------------------
// §5.9.3 — the repair-consequence split
// ---------------------------------------------------------------------------

/// Contract (KD-PV-8): coverage is asserted on a healthy multi-owner
/// tree — the findings half of PR 6's gate cannot pass at 1/K coverage
/// because the coverage half is checked beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_owner_online_pass_over_a_healthy_tree_produces_zero_inode_plane_findings() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    let d1 = mkdir_on(&fx.meta, 1, 1, "peer_subtree").await;
    for i in 0..4 {
        mkfile_on(&fx.meta, d1, 1, &format!("peer_f{i}")).await;
        mkfile_on(&fx.meta, 1, 0, &format!("local_f{i}")).await;
    }
    let _arm = fx.arm();

    for owned in [vec![0usize], vec![1usize]] {
        let report = run_fsck(&fx.ctx(), &owner_opts(&owned))
            .await
            .expect("the owner's shard");
        assert!(
            report.findings.is_empty(),
            "the mirage: a healthy tree must produce no inode-plane finding on owner {owned:?}: \
             {:?}",
            report.findings
        );
        assert_eq!(
            report.inode_plane_covered, owned,
            "an owner's shard covers exactly the volumes it appends to"
        );
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// KD-PV-16 — the owner-shard fan-out over the existing job wire
// ---------------------------------------------------------------------------

/// The in-process OWNER seam: a peer authority serving the coordinator's
/// inode-plane shard over the volumes IT owns, plus census residues like
/// any read-capable member.
struct OwnerSeam {
    ctx: FsckCtx,
    /// The volumes this simulated node appends to (production reads its
    /// OWN process's `OwnerMap`; in-process every "node" shares one, so
    /// the seam carries the posture).
    owned: Vec<usize>,
    /// `Some` ⇒ propose this report verbatim for an inode-plane shard —
    /// the fabricated-proposal arms (a member's mirage, a foreign
    /// volume's verdict, a payload that asserts its own authority).
    fabricate: Option<FsckReport>,
}

impl ShardDeviceSeam for OwnerSeam {
    fn plan_blocks(&self, _job: &JobType) -> usize {
        0
    }
    fn block_len(&self) -> usize {
        0
    }
    fn allocate(&self, n: usize) -> std::io::Result<Vec<squeezefs::job_wire::DestTuple>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        Err(std::io::Error::other("read-class seam: no allocator"))
    }
    fn read_source(&self, _key: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("read-class seam: no device"))
    }
    fn write_block(
        &self,
        _dest: &squeezefs::job_wire::DestTuple,
        _data: &[u8],
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("read-class seam: no device"))
    }
    fn read_block(&self, _dest: &squeezefs::job_wire::DestTuple) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("read-class seam: no device"))
    }
    fn run_fleet_shard(&self, job: &JobType, spec: FleetShardSpec) -> std::io::Result<Vec<u8>> {
        if let Some(fake) = &self.fabricate {
            // The fabricated proposal keeps the shard IDENTITY the wire
            // asked for (a census residue is labelled, a plane shard is
            // not) — anything else is refused as a lost shard before the
            // admission predicate is ever consulted, which would test the
            // wrong thing.
            let mut fake = fake.clone();
            fake.shard = (!spec.inode_plane).then(|| format!("{}/{}", spec.k, spec.n));
            return serde_json::to_vec(&fake).map_err(std::io::Error::other);
        }
        let mut o = FsckOptions::offline();
        o.settle = Duration::from_millis(10);
        o.throttle_pct = spec.throttle_pct;
        if spec.inode_plane {
            // The KD-PV-16 owner shard: the plane, over the volumes this
            // node appends to, on its own coherent view.
            o.inode_plane = true;
            o.inode_plane_only = true;
            o.multi_owner = true;
            o.owned_volumes = Some(self.owned.clone());
        } else {
            o.shard = Some((spec.k, spec.n));
            o.staging_full = true;
            o.inode_plane = false;
        }
        if let JobType::Fsck {
            scrub, scrub_only, ..
        } = job
        {
            o.scrub = *scrub;
            o.scrub_only = *scrub_only;
        }
        let report = squeezefs_ipc::sqz_blocking::block_on(run_fsck(&self.ctx, &o))
            .map_err(|e| std::io::Error::other(format!("owner shard fsck failed: {e}")))?;
        serde_json::to_vec(&report).map_err(std::io::Error::other)
    }
}

fn wire_cfg(ttl_ms: u64, hb_ms: u64) -> JobWireConfig {
    JobWireConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        lease_ttl: Duration::from_millis(ttl_ms),
        heartbeat_interval: Duration::from_millis(hb_ms),
        ..JobWireConfig::default()
    }
}

async fn fabric(meta: &Arc<RoutedMetaBackend>) -> Arc<JobFabric> {
    JobFabric::start(meta.clone(), 0, 100, None)
        .await
        .expect("fabric start")
}

async fn poll_until(what: &str, deadline: Duration, mut f: impl FnMut() -> bool) {
    let start = tokio::time::Instant::now();
    loop {
        if f() {
            return;
        }
        assert!(
            start.elapsed() < deadline,
            "timed out after {deadline:?} waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_owner(
    host: &Arc<JobWireHost>,
    meta: &Arc<RoutedMetaBackend>,
    id: &str,
    seam: OwnerSeam,
) -> tokio::task::JoinHandle<squeezefs::job_wire::WorkerReport> {
    let secret = squeezefs::job_wire::read_enroll_secret(meta)
        .await
        .expect("job:enroll present");
    let mut opts = WorkerOptions::new(id);
    opts.caps = CAP_FLEET_READ;
    let worker = JobWireWorker::connect(&host.endpoint().to_string(), &secret, opts)
        .await
        .expect("the owner enrolls (storage trust)");
    tokio::spawn(async move { worker.run(Arc::new(seam)).await.expect("worker report") })
}

/// Contract (KD-PV-16 + Issue 25): the union of the coordinator's own
/// plane and the admitted owner shards covers EVERY volume's inode
/// plane — asserted, not claimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_union_of_online_owner_shards_covers_every_volumes_inode_plane_at_k4() {
    let _serial = serial().await;
    let fx = assigned_fixture(4).await;
    for v in 0..4 {
        let d = mkdir_on(&fx.meta, 1, v, &format!("sub{v}")).await;
        mkfile_on(&fx.meta, d, v, &format!("f{v}")).await;
    }
    let _arm = fx.arm();

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let mut workers = Vec::new();
    for (i, peer) in PEERS.iter().enumerate() {
        workers.push(
            spawn_owner(
                &host,
                &fx.meta,
                peer,
                OwnerSeam {
                    ctx: fx.ctx(),
                    owned: vec![i + 1],
                    fabricate: None,
                },
            )
            .await,
        );
    }
    poll_until("3 owner workers enrolled", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 3
    })
    .await;

    let merged = run_fleet(
        &fx.ctx(),
        &owner_opts(&[0]),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-pv-k4",
    )
    .await
    .expect("fleet run");

    assert!(
        merged.findings.is_empty(),
        "healthy K=4 fleet: {:?}",
        merged.findings
    );
    assert_eq!(
        merged.counters.inode_plane_volumes_covered,
        fx.meta.volumes.len() as u64,
        "the gate is findings==0 AND full coverage — the coverage half is what stops the \
         findings half from passing trivially at 1/K (covered: {:?})",
        merged.inode_plane_covered
    );
    assert!(
        merged.counters.inode_plane_proposals_admitted >= 3,
        "coverage that closes without an admitted owner proposal came from nowhere"
    );
    assert_eq!(
        merged.counters.inode_plane_proposals_stripped, 0,
        "a homogeneous fleet strips nothing — growth means a non-owner is proposing the plane"
    );

    host.shutdown().await;
    for w in workers {
        let _ = w.await;
    }
    fab.shutdown_abrupt().await;
    fx.close().await;
}

/// Contract (Issue 25): an owner that does not report makes the pass
/// INCOMPLETE — never silently narrower.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_owner_shard_makes_the_pass_incomplete_not_narrower() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    mkfile_on(&fx.meta, 1, 1, "peer_file").await;
    let _arm = fx.arm();

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    // No owner enrolls: the peer's plane has nobody to evaluate it.
    let merged = run_fleet(
        &fx.ctx(),
        &owner_opts(&[0]),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-pv-missing",
    )
    .await
    .expect("fleet run");
    assert_eq!(
        merged.counters.inode_plane_volumes_covered, 1,
        "the coordinator's own volume is covered and the peer's is not: {:?}",
        merged.inode_plane_covered
    );
    assert!(
        merged.counters.inode_plane_volumes_covered < fx.meta.volumes.len() as u64,
        "a pass that reached 1/K of the plane must SAY so"
    );
    host.shutdown().await;
    fab.shutdown_abrupt().await;
    fx.close().await;
}

/// Contract (§5.8.2): an owner shard's findings MERGE and move the
/// coordinator's inode-plane counters — the admitted path, end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_shards_inode_plane_findings_merge_and_move_the_coordinators_counters() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    let victim = mkfile_on(&fx.meta, 1, 1, "peer_orphan").await;
    orphan_inode(&fx.meta, victim).await;
    let fx = fx.remount().await;
    let _arm = fx.arm();

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let w = spawn_owner(
        &host,
        &fx.meta,
        PEERS[0],
        OwnerSeam {
            ctx: fx.ctx(),
            owned: vec![1],
            fabricate: None,
        },
    )
    .await;
    poll_until("the owner enrolls", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 1
    })
    .await;

    let before = METRICS.fsck_dentry_refs_indexed.load(Ordering::Relaxed);
    let merged = run_fleet(
        &fx.ctx(),
        &owner_opts(&[0]),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-pv-admit",
    )
    .await
    .expect("fleet run");

    assert_eq!(
        class_findings(&merged, "C9").len(),
        1,
        "the peer's residue reaches the coordinator's report through its OWNER: {:?}",
        merged.findings
    );
    assert!(
        merged.counters.inode_plane_proposals_admitted >= 1,
        "the admission is counted"
    );
    assert!(
        METRICS.fsck_dentry_refs_indexed.load(Ordering::Relaxed) > before,
        "the admitted shard's counters reach the coordinator's gauges"
    );
    host.shutdown().await;
    let _ = w.await;
    fab.shutdown_abrupt().await;
    fx.close().await;
}

/// Contract (§5.8.2 — the `fix/mw-xv-unlink-c10` mirage's own regression
/// test): a MEMBER's inode-plane proposal is still stripped, loudly, and
/// never moves the coordinator's stop-and-read tripwires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_shards_inode_plane_findings_are_still_stripped_loudly() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    mkfile_on(&fx.meta, 1, 1, "peer_file").await;
    let _arm = fx.arm();

    // A member of no consequence: an id the coordinator's OwnerMap does
    // not name (a reader, a co-writer, or an older binary).
    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let mirage = mirage_report(&fx).await;
    let w = spawn_owner(
        &host,
        &fx.meta,
        "node_0000000000000bad.m00000001",
        OwnerSeam {
            ctx: fx.ctx(),
            owned: vec![1],
            fabricate: Some(mirage),
        },
    )
    .await;
    poll_until("the member enrolls", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 1
    })
    .await;

    let zero_named = METRICS.fsck_nlink_zero_named.load(Ordering::Relaxed);
    let merged = run_fleet(
        &fx.ctx(),
        &owner_opts(&[0]),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-pv-mirage",
    )
    .await
    .expect("fleet run");

    assert!(
        merged.findings.is_empty(),
        "a non-owner's inode-plane verdict is inadmissible by construction: {:?}",
        merged.findings
    );
    assert!(
        merged.counters.inode_plane_proposals_stripped >= 1,
        "the strip is COUNTED — the gauge is what says the mirage path is live"
    );
    assert_eq!(
        merged.counters.inode_plane_proposals_admitted, 0,
        "nothing from a non-owner is ever admitted"
    );
    assert_eq!(
        METRICS.fsck_nlink_zero_named.load(Ordering::Relaxed),
        zero_named,
        "a stripped proposal must not move the coordinator's stop-and-read tripwires"
    );
    host.shutdown().await;
    let _ = w.await;
    fab.shutdown_abrupt().await;
    fx.close().await;
}

/// A fabricated proposal carrying one finding per dangerous class, with
/// the counters a mirage would move — and a `shard` label plus counters
/// that ASSERT an authority the payload does not have.
async fn mirage_report(fx: &Fx) -> FsckReport {
    use squeezefs::fsck::{FindingId, FsckCounters, FsckFinding};
    let ino_on_1 = fx.meta.volumes.len(); // any ino; the identity is what matters
    let counters = FsckCounters {
        nlink_zero_named: 3,
        dangling_dentries: 2,
        findings: 2,
        inode_plane_volumes_covered: 4,
        ..Default::default()
    };
    FsckReport {
        schema: squeezefs::fsck::FSCK_REPORT_SCHEMA,
        mode: "offline".to_string(),
        shard: None,
        findings: vec![
            FsckFinding {
                class: "C9".to_string(),
                object: format!("ino {ino_on_1}"),
                evidence: "fabricated".to_string(),
                identity: Some(FindingId::C9Unreferenced {
                    ino: peer_ino(fx, 1),
                }),
            },
            FsckFinding {
                class: "C10".to_string(),
                object: format!("ino {ino_on_1}"),
                evidence: "fabricated".to_string(),
                identity: Some(FindingId::C10ZeroNlinkNamed {
                    ino: peer_ino(fx, 1),
                }),
            },
        ],
        counters,
        partial: None,
        repair: None,
        inode_plane_covered: vec![0, 1],
        findings_elided: 0,
    }
}

/// Some global ino that routes to `vol` (arithmetic only — no record has
/// to exist for an admission decision to be taken about it).
fn peer_ino(fx: &Fx, vol: usize) -> u64 {
    for ino in 2..100_000u64 {
        if fx.meta.route_ino(ino).0 == vol {
            return ino;
        }
    }
    panic!("no ino routes to volume {vol}");
}

/// Contract (§5.8.2): an OWNER's finding about a volume it does not own
/// is stripped — the predicate derives the volume from the coordinator's
/// own `route_ino`, never from the shard's word.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_shards_finding_about_a_volume_it_does_not_own_is_stripped() {
    let _serial = serial().await;
    let fx = assigned_fixture(3).await;
    let _arm = fx.arm();

    // PEERS[0] owns volume 1; its proposal is about an ino on volume 2.
    let mut fake = mirage_report(&fx).await;
    let foreign = peer_ino(&fx, 2);
    fake.findings[0].identity = Some(squeezefs::fsck::FindingId::C9Unreferenced { ino: foreign });
    fake.findings.truncate(1);

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let w = spawn_owner(
        &host,
        &fx.meta,
        PEERS[0],
        OwnerSeam {
            ctx: fx.ctx(),
            owned: vec![1],
            fabricate: Some(fake),
        },
    )
    .await;
    poll_until("the owner enrolls", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 1
    })
    .await;

    let merged = run_fleet(
        &fx.ctx(),
        &owner_opts(&[0]),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-pv-foreign",
    )
    .await
    .expect("fleet run");
    assert!(
        merged.findings.is_empty(),
        "a verdict about volume 2 from volume 1's owner is inadmissible: {:?}",
        merged.findings
    );
    assert!(merged.counters.inode_plane_proposals_stripped >= 1);
    host.shutdown().await;
    let _ = w.await;
    fab.shutdown_abrupt().await;
    fx.close().await;
}

/// Contract (§5.8.2): the predicate never reads the shard's own claim —
/// a payload that declares broader coverage than the coordinator's map
/// grants it moves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_admission_predicate_never_reads_the_shards_own_claim() {
    let _serial = serial().await;
    let fx = assigned_fixture(3).await;
    let _arm = fx.arm();

    // PEERS[0] owns volume 1 alone, and says it covered the whole set.
    let mut fake = mirage_report(&fx).await;
    fake.findings.clear();
    fake.inode_plane_covered = vec![0, 1, 2];
    fake.counters.inode_plane_volumes_covered = 3;

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let w = spawn_owner(
        &host,
        &fx.meta,
        PEERS[0],
        OwnerSeam {
            ctx: fx.ctx(),
            owned: vec![1],
            fabricate: Some(fake),
        },
    )
    .await;
    poll_until("the owner enrolls", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 1
    })
    .await;

    let merged = run_fleet(
        &fx.ctx(),
        &owner_opts(&[0]),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-pv-claim",
    )
    .await
    .expect("fleet run");
    let covered = merged.inode_plane_covered.clone();
    assert!(
        covered.contains(&0) && covered.contains(&1) && !covered.contains(&2),
        "coverage is the INTERSECTION of what a shard reports with what the coordinator's own \
         map says that worker owns — a declaration can only ever narrow: {covered:?}"
    );
    assert!(
        merged.counters.inode_plane_volumes_covered < fx.meta.volumes.len() as u64,
        "volume 2's owner never reported, so the pass stays INCOMPLETE whatever a peer claims"
    );
    host.shutdown().await;
    let _ = w.await;
    fab.shutdown_abrupt().await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// KD-PV-14 — the maintenance coordinator
// ---------------------------------------------------------------------------

/// Contract (KD-PV-14): the coordinator is the owner of the slot-0
/// volume. On a K-node fleet exactly one node answers yes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exactly_one_node_coordinates_on_a_k_node_fleet() {
    let _serial = serial().await;
    let fx = assigned_fixture(3).await;

    // This node owns the slot-0 volume: it coordinates.
    {
        let _arm = fx.arm();
        assert!(
            squeezefs::jobs::maintenance_coordinator_refusal().is_none(),
            "the owner of the slot-0 volume IS the maintenance coordinator (D20)"
        );
    }
    // The same set seen from a node that owns only volume 1: slot 0 is a
    // peer's, so this node refuses to coordinate.
    {
        let foreign: Vec<(usize, PeerOwner)> = vec![
            (0, PeerOwner::new(NODE, "127.0.0.1:7000")),
            (2, PeerOwner::new(PEERS[1], "127.0.0.1:7102")),
        ];
        owners::arm_ownership(OwnerMap::for_volumes(&fx.meta, foreign).expect("owner map"));
        let _arm = ArmGuard;
        let refusal = squeezefs::jobs::maintenance_coordinator_refusal()
            .expect("a non-set-authority refuses to coordinate");
        assert!(
            refusal.contains(NODE),
            "the refusal NAMES the set authority so the operator knows where to go: {refusal}"
        );
        assert!(
            refusal.contains("127.0.0.1:7000"),
            "…and its endpoint: {refusal}"
        );
    }
    fx.close().await;
}

/// Contract (KD-PV-14, scoped in rev 3): the refusal covers
/// COORDINATOR-CLASS acts — minting a job record — and says so; it never
/// touches an owner's participation as a detection SHARD.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_set_authority_refuses_to_coordinate_naming_the_set_authority() {
    let _serial = serial().await;
    let fx = assigned_fixture(2).await;
    let fab = fabric(&fx.meta).await;

    // Solo (nothing armed): submission is the shipped path, unchanged.
    let id = fab
        .submit(JobSpec {
            job_type: JobType::Fsck {
                scrub: false,
                scrub_only: false,
                repair: false,
                apply: false,
                quarantine_dir: None,
            },
            throttle_pct: 100,
        })
        .await
        .expect("an unarmed mount submits exactly as it always has");
    fab.cancel(&id).await.expect("cancel");

    let foreign: Vec<(usize, PeerOwner)> = vec![(0, PeerOwner::new(PEERS[0], "127.0.0.1:7101"))];
    owners::arm_ownership(OwnerMap::for_volumes(&fx.meta, foreign).expect("owner map"));
    let _arm = ArmGuard;
    let err = fab
        .submit(JobSpec {
            job_type: JobType::Fsck {
                scrub: false,
                scrub_only: false,
                repair: false,
                apply: false,
                quarantine_dir: None,
            },
            throttle_pct: 100,
        })
        .await
        .expect_err("a second coordinator over one set is refused");
    let text = format!("{err}");
    assert!(
        text.contains(PEERS[0]) && text.contains("shard"),
        "the refusal names the set authority AND says which of the two acts the operator hit \
         (a coordinator-class act, never an owner's shard participation): {text}"
    );
    fab.shutdown_abrupt().await;
    fx.close().await;
}
