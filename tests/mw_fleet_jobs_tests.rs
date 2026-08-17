//! MW rung 10c (KD-MW-16) — fleet-parallel maintenance, red-first
//! (`docs/design-mw-fleet-jobs.md`; charter row 10c of
//! `docs/design-full-multi-writer.md`).
//!
//! The three charter gates, pinned in-process (the rig legs
//! `s10c-fsck-scale` / `s10c-kill-shard` are the live venue):
//!
//! - **Gate 1 — exactly-once census coverage**: an N-member fleet fsck
//!   detect pass covers the census exactly once — the merged report's
//!   `inodes_scanned` equals the unsharded baseline's, findings stay 0 on
//!   the healthy fixture at every N, and a seeded in-capacity
//!   referenced-but-untracked violation (the C2 tracked arm, which only
//!   the coordinator's FINALIZE over the fleet-merged census can run)
//!   is found through worker-executed shards.
//! - **Gate 2 — kill-9/lost-shard re-lease, zero double-count**: a worker
//!   that goes silent mid-shard loses its lease (TTL), the shard
//!   re-leases (here: relocal — no other capable worker), its LATE
//!   proposal refuses stale (`job_remote_refused_stale`), and the merged
//!   census still counts every inode exactly once. Read-shard expiry
//!   takes the fencing/notify arm ONLY: no destination quarantine and no
//!   PR preempt (a read worker DMAs nothing; preempting a live host's
//!   registrant key over a lost READ shard would fence its data plane).
//! - **Gate 3 — movers never fight custody**: the §5.7 quiescence probe
//!   defers an ino with a live S9 custody grant
//!   (`job_mover_custody_defers`), and releases the deferral with the
//!   custody authority.
//!
//! Plus the two composition laws: the ZERO-CAPACITY identity (a fleet
//! run with no workers IS the shipped local run — same engine, same
//! counters, zero fleet metrics moved) and the R5-Red worker abandon
//! (a Red member refuses its shard loudly and PROMPTLY — the coordinator
//! re-leases without waiting out the TTL).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::data_grant::{self, AcquireFrame, JoinFrame, WriteCustodyOwner, CUSTODY_SCHEMA};
use squeezefs::dlm::DlmClient;
use squeezefs::fleet_worker;
use squeezefs::fsck::{run as run_fsck, run_fleet, FsckCtx, FsckOptions};
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::job_wire::{
    JobWireConfig, JobWireHost, JobWireWorker, ShardDeviceSeam, WorkerOptions, CAP_FLEET_READ,
};
use squeezefs::jobs::{FleetDispatch, JobFabric, JobType};
use squeezefs::mem_budget::Level;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::Metadata as _;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
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

/// Mount-shaped fixture (the fsck_tests shape): live meta + router +
/// staging, allocator recovery run like a mount.
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

    async fn close(self) {
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = Duration::from_millis(100);
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

/// Striped burst (the fsck_tests helper): force striped layout, write
/// `nblocks` distinct blocks, fsync.
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

/// Populate a small mixed tree; returns the created file count.
async fn seed_tree(fx: &Fx, files: usize) -> usize {
    for i in 0..files {
        let ino = create_file(fx, &format!("f{i}.bin")).await;
        striped_burst(fx, ino, 2).await;
    }
    files
}

/// A fabric with no local pool (the wire/fleet tests drive shards
/// directly through `run_fleet`).
async fn fabric(meta: &Arc<squeezefs::meta_backend::RoutedMetaBackend>) -> Arc<JobFabric> {
    JobFabric::start(meta.clone(), 0, 100, None)
        .await
        .expect("fabric start")
}

fn wire_cfg(ttl_ms: u64, hb_ms: u64) -> JobWireConfig {
    JobWireConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        lease_ttl: Duration::from_millis(ttl_ms),
        heartbeat_interval: Duration::from_millis(hb_ms),
        ..JobWireConfig::default()
    }
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

/// The in-process MEMBER seam: executes fleet census shards over the
/// SHARED fixture context — the membership_sim posture (one process,
/// N logical members). Mirrors `fleet_worker::MountFleetSeam`'s shard
/// law: offline posture, ino-residue shard, staging scanned FULL.
struct MemberSeam {
    ctx: FsckCtx,
    /// `Some(err)` ⇒ refuse every shard (the R5-Red member shape).
    refuse: Option<String>,
}

impl ShardDeviceSeam for MemberSeam {
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
        Err(std::io::Error::other(
            "read-class member seam: no allocator",
        ))
    }
    fn read_source(&self, _key: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("read-class member seam: no device"))
    }
    fn write_block(
        &self,
        _dest: &squeezefs::job_wire::DestTuple,
        _data: &[u8],
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("read-class member seam: no device"))
    }
    fn read_block(&self, _dest: &squeezefs::job_wire::DestTuple) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("read-class member seam: no device"))
    }
    fn run_fleet_shard(
        &self,
        job: &JobType,
        shard_k: u32,
        shard_count: u32,
        throttle_pct: u32,
    ) -> std::io::Result<Vec<u8>> {
        if let Some(why) = &self.refuse {
            return Err(std::io::Error::other(why.clone()));
        }
        let mut o = FsckOptions::offline();
        o.shard = Some((shard_k, shard_count));
        o.staging_full = true;
        o.throttle_pct = throttle_pct;
        if let JobType::Fsck {
            scrub, scrub_only, ..
        } = job
        {
            o.scrub = *scrub;
            o.scrub_only = *scrub_only;
        }
        let report = squeezefs_ipc::sqz_blocking::block_on(run_fsck(&self.ctx, &o))
            .map_err(|e| std::io::Error::other(format!("member shard fsck failed: {e}")))?;
        serde_json::to_vec(&report).map_err(std::io::Error::other)
    }
}

/// Enroll one in-process member worker with `CAP_FLEET_READ` and serve
/// shards on a spawned task. Returns the task handle (joined by the
/// test).
async fn spawn_member(
    host: &Arc<JobWireHost>,
    meta: &Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    id: &str,
    seam: MemberSeam,
) -> tokio::task::JoinHandle<squeezefs::job_wire::WorkerReport> {
    let secret = squeezefs::job_wire::read_enroll_secret(meta)
        .await
        .expect("job:enroll present");
    let mut opts = WorkerOptions::new(id);
    opts.caps = CAP_FLEET_READ;
    let worker = JobWireWorker::connect(&host.endpoint().to_string(), &secret, opts)
        .await
        .expect("member enrolls (storage trust)");
    tokio::spawn(async move {
        worker
            .run(Arc::new(seam))
            .await
            .expect("worker run returns a report")
    })
}

// ---------------------------------------------------------------------------
// Zero capacity: the fleet run IS the local run
// ---------------------------------------------------------------------------

/// Contract (design §4 step 1): with no fleet dispatch — or a wire host
/// with zero enrolled read-capable workers — `run_fleet` IS the shipped
/// local engine: same findings, same census counters, and no fleet
/// metric moves. A single-writer mount's fsck is literally unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_capacity_fleet_run_is_the_local_run_verbatim() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    seed_tree(&fx, 6).await;

    let baseline = run_fsck(&fx.ctx(), &online_opts()).await.expect("baseline");
    assert!(!baseline.has_findings(), "healthy fixture");

    // No dispatch at all.
    let none = run_fleet(&fx.ctx(), &online_opts(), None, "job-none")
        .await
        .expect("fleet(None)");
    assert_eq!(none.findings, baseline.findings);
    assert_eq!(
        none.counters.inodes_scanned,
        baseline.counters.inodes_scanned
    );

    // A live host with zero enrolled workers.
    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 10_000),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    assert_eq!(host.fleet_read_capacity(), 0, "no members enrolled");
    let dispatched0 = METRICS.job_fleet_shards_dispatched.load(Ordering::Relaxed);
    let idle = run_fleet(
        &fx.ctx(),
        &online_opts(),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-idle",
    )
    .await
    .expect("fleet(0 workers)");
    assert_eq!(idle.findings, baseline.findings);
    assert_eq!(
        idle.counters.inodes_scanned,
        baseline.counters.inodes_scanned
    );
    assert_eq!(
        METRICS.job_fleet_shards_dispatched.load(Ordering::Relaxed),
        dispatched0,
        "zero capacity moves no fleet metric"
    );
    host.shutdown().await;
    fab.shutdown_abrupt().await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Gate 1 — exactly-once coverage with partition accounting
// ---------------------------------------------------------------------------

/// Contract (charter gate 1): a 3-shard fleet pass (coordinator + two
/// members) covers the census exactly once — merged `inodes_scanned`
/// equals the unsharded baseline, findings 0 on the healthy fixture —
/// and the engagement ledger closes (`job_fleet_shards_dispatched` ==
/// `job_fleet_shards_completed` == 2, relocal 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_census_covers_exactly_once_with_partition_accounting() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    seed_tree(&fx, 9).await;

    let baseline = run_fsck(&fx.ctx(), &online_opts()).await.expect("baseline");
    assert!(!baseline.has_findings());

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let m1 = spawn_member(
        &host,
        &fx.meta,
        "node-m1",
        MemberSeam {
            ctx: fx.ctx(),
            refuse: None,
        },
    )
    .await;
    let m2 = spawn_member(
        &host,
        &fx.meta,
        "node-m2",
        MemberSeam {
            ctx: fx.ctx(),
            refuse: None,
        },
    )
    .await;
    poll_until("2 read workers enrolled", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 2
    })
    .await;

    let d0 = METRICS.job_fleet_shards_dispatched.load(Ordering::Relaxed);
    let c0 = METRICS.job_fleet_shards_completed.load(Ordering::Relaxed);
    let r0 = METRICS.job_fleet_shards_relocal.load(Ordering::Relaxed);

    let merged = run_fleet(
        &fx.ctx(),
        &online_opts(),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-fleet-1",
    )
    .await
    .expect("fleet run");

    assert!(
        !merged.has_findings(),
        "findings 0 on the healthy fixture at N=3: {:?}",
        merged.findings
    );
    assert_eq!(
        merged.counters.inodes_scanned, baseline.counters.inodes_scanned,
        "the residue partition covers the census EXACTLY once"
    );
    assert_eq!(
        METRICS.job_fleet_shards_dispatched.load(Ordering::Relaxed) - d0,
        2,
        "one shard per enrolled member"
    );
    assert_eq!(
        METRICS.job_fleet_shards_completed.load(Ordering::Relaxed) - c0,
        2,
        "every dispatched shard completed (the engagement ledger closes)"
    );
    assert_eq!(
        METRICS.job_fleet_shards_relocal.load(Ordering::Relaxed) - r0,
        0
    );
    // The finalize re-ran the allocator classes over the MERGED census:
    // the tracked population was checked (nonzero refcounts_checked),
    // which a sharded run alone never does.
    assert!(
        merged.counters.refcounts_checked >= baseline.counters.refcounts_checked,
        "the coordinator finalize covered the allocator classes ({} < {})",
        merged.counters.refcounts_checked,
        baseline.counters.refcounts_checked
    );

    host.shutdown().await;
    let (r1, r2) = (m1.await.expect("m1 joins"), m2.await.expect("m2 joins"));
    assert_eq!(r1.shards_completed + r2.shards_completed, 2);
    fab.shutdown_abrupt().await;
    fx.close().await;
}

/// Contract (gate 1's teeth): an in-capacity referenced-but-untracked
/// block — the C2 tracked arm only the coordinator's FINALIZE over the
/// fleet-merged full census can adjudicate — is found by the fleet run,
/// exactly as the unsharded baseline finds it, WITH worker shards
/// engaged (the reference that convicts it may arrive via a member's
/// residue).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_shard_census_feeds_the_coordinator_allocator_classes() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    seed_tree(&fx, 6).await;

    // Seed the violation on EVERY residue of a 3-way partition: three
    // files, each referencing a distinct phantom in-capacity offset, so
    // at least two of the three convicting references travel through
    // WORKER shards whatever the ino residues are.
    let chunk = fx.fs.router.backend_router.default_allocator.chunk_size();
    for (i, idx) in [400u64, 401, 402].iter().enumerate() {
        let ino = create_file(&fx, &format!("phantom{i}.bin")).await;
        striped_burst(&fx, ino, 2).await;
        let off = idx * chunk;
        let key = fx
            .fs
            .router
            .backend_router
            .persist_block_key(&recs[0].id, off);
        let token = fx.fs.dlm().get_fencing_token_ino(ino);
        let entries = [(300u32, key)];
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
    }

    let baseline = run_fsck(&fx.ctx(), &online_opts()).await.expect("baseline");
    let baseline_lost: Vec<_> = baseline
        .findings
        .iter()
        .filter(|f| f.class == "C2" && f.evidence.contains("not allocator-tracked"))
        .collect();
    assert_eq!(
        baseline_lost.len(),
        3,
        "the unsharded engine convicts all three phantoms: {:?}",
        baseline.findings
    );

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let m1 = spawn_member(
        &host,
        &fx.meta,
        "node-m1",
        MemberSeam {
            ctx: fx.ctx(),
            refuse: None,
        },
    )
    .await;
    let m2 = spawn_member(
        &host,
        &fx.meta,
        "node-m2",
        MemberSeam {
            ctx: fx.ctx(),
            refuse: None,
        },
    )
    .await;
    poll_until("2 read workers enrolled", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 2
    })
    .await;

    let merged = run_fleet(
        &fx.ctx(),
        &online_opts(),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-fleet-2",
    )
    .await
    .expect("fleet run");
    let merged_lost: Vec<_> = merged
        .findings
        .iter()
        .filter(|f| f.class == "C2" && f.evidence.contains("not allocator-tracked"))
        .collect();
    assert_eq!(
        merged_lost.len(),
        3,
        "the fleet pass convicts the same three phantoms (worker census reaches the \
         coordinator's allocator classes): {:?}",
        merged.findings
    );

    host.shutdown().await;
    let _ = m1.await.expect("m1 joins");
    let _ = m2.await.expect("m2 joins");
    fab.shutdown_abrupt().await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Gate 2 — lost shard re-leases; the late proposal refuses stale
// ---------------------------------------------------------------------------

/// Contract (charter gate 2, the in-process face — the rig's
/// `s10c-kill-shard` is the real kill-9): a member that stops
/// heartbeating and parks before submission loses its lease at the TTL;
/// the shard re-leases (relocal — no other capable worker); its LATE
/// proposal refuses stale (`job_remote_refused_stale`); the merged
/// census counts every inode exactly once; and read-shard expiry never
/// quarantines destinations and never PR-preempts the holder's host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_read_shard_re_leases_and_the_late_proposal_refuses_stale() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    seed_tree(&fx, 6).await;
    let baseline = run_fsck(&fx.ctx(), &online_opts()).await.expect("baseline");

    let fab = fabric(&fx.meta).await;
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(900, 300),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");

    let secret = squeezefs::job_wire::read_enroll_secret(&fx.meta)
        .await
        .expect("job:enroll present");
    let mut opts = WorkerOptions::new("node-zombie");
    opts.caps = CAP_FLEET_READ;
    // The zombie shape: no heartbeats (partition model) + park before
    // submission (the window the fencing check exists for).
    opts.heartbeats
        .store(false, std::sync::atomic::Ordering::SeqCst);
    opts.hold_submission
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let worker = JobWireWorker::connect(&host.endpoint().to_string(), &secret, opts.clone())
        .await
        .expect("zombie member enrolls");
    let seam = MemberSeam {
        ctx: fx.ctx(),
        refuse: None,
    };
    let wtask = tokio::spawn(async move {
        worker
            .run(Arc::new(seam))
            .await
            .expect("worker run returns")
    });
    poll_until("zombie enrolled", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 1
    })
    .await;

    let stale0 = METRICS.job_remote_refused_stale.load(Ordering::Relaxed);
    let preempt0 = METRICS.job_remote_pr_preempts.load(Ordering::Relaxed);
    let quarantine0 = METRICS
        .job_remote_quarantined_destinations
        .load(Ordering::Relaxed);
    let relocal0 = METRICS.job_fleet_shards_relocal.load(Ordering::Relaxed);
    let expiry0 = METRICS.job_remote_lease_expiries.load(Ordering::Relaxed);

    let merged = run_fleet(
        &fx.ctx(),
        &online_opts(),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-fleet-3",
    )
    .await
    .expect("fleet run completes despite the zombie");

    assert!(!merged.has_findings(), "{:?}", merged.findings);
    assert_eq!(
        merged.counters.inodes_scanned, baseline.counters.inodes_scanned,
        "zero double-count: the re-leased residue lands exactly once"
    );
    assert!(
        METRICS.job_remote_lease_expiries.load(Ordering::Relaxed) > expiry0,
        "the zombie's lease expired"
    );
    assert!(
        METRICS.job_fleet_shards_relocal.load(Ordering::Relaxed) > relocal0,
        "the lost residue re-leased (relocal — no other capable worker)"
    );
    assert_eq!(
        METRICS.job_remote_pr_preempts.load(Ordering::Relaxed),
        preempt0,
        "read-shard expiry NEVER PR-preempts the holder's host"
    );
    assert_eq!(
        METRICS
            .job_remote_quarantined_destinations
            .load(Ordering::Relaxed),
        quarantine0,
        "read shards have no destinations to quarantine"
    );

    // Release the zombie: its LATE proposal must refuse stale, loudly.
    opts.hold_submission
        .store(false, std::sync::atomic::Ordering::SeqCst);
    poll_until(
        "late proposal refused stale",
        Duration::from_secs(5),
        || METRICS.job_remote_refused_stale.load(Ordering::Relaxed) > stale0,
    )
    .await;

    host.shutdown().await;
    let report = wtask.await.expect("worker joins");
    assert_eq!(
        report.submissions_refused, 1,
        "the zombie observed its own refusal"
    );
    fab.shutdown_abrupt().await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The R5 composition — a Red member abandons PROMPTLY, coordinator re-leases
// ---------------------------------------------------------------------------

/// Contract (design §5): a member whose budget is Red refuses its shard
/// loudly (`ShardAbandon`) instead of executing it — and the coordinator
/// re-leases WITHOUT waiting out the TTL (the abandon is the prompt form
/// of the expiry law). The pure refusal law is also pinned directly:
/// `fleet_worker::refuse_on_red` refuses Red and admits Green/Yellow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn red_member_abandons_promptly_and_the_shard_re_leases() {
    let _serial = serial().await;
    assert!(fleet_worker::refuse_on_red(Level::Red).is_err());
    assert!(fleet_worker::refuse_on_red(Level::Green).is_ok());
    assert!(fleet_worker::refuse_on_red(Level::Yellow).is_ok());

    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    seed_tree(&fx, 6).await;
    let baseline = run_fsck(&fx.ctx(), &online_opts()).await.expect("baseline");

    let fab = fabric(&fx.meta).await;
    // A LONG TTL: completing promptly proves the abandon path, not the
    // sweeper.
    let host = JobWireHost::start(
        fab.clone(),
        wire_cfg(30_000, 500),
        Arc::new(squeezefs::job_wire::NoopDeviceSeam),
    )
    .await
    .expect("host start");
    let m1 = spawn_member(
        &host,
        &fx.meta,
        "node-red",
        MemberSeam {
            ctx: fx.ctx(),
            refuse: Some("R5 memory budget is Red — shard refused (design §5)".to_string()),
        },
    )
    .await;
    poll_until("red member enrolled", Duration::from_secs(5), || {
        host.fleet_read_capacity() == 1
    })
    .await;

    let relocal0 = METRICS.job_fleet_shards_relocal.load(Ordering::Relaxed);
    let t0 = std::time::Instant::now();
    let merged = run_fleet(
        &fx.ctx(),
        &online_opts(),
        Some(host.clone() as Arc<dyn FleetDispatch>),
        "job-fleet-4",
    )
    .await
    .expect("fleet run completes despite the Red member");
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "the abandon re-leased PROMPTLY (elapsed {:?} must be far below the 30 s TTL)",
        t0.elapsed()
    );
    assert!(!merged.has_findings(), "{:?}", merged.findings);
    assert_eq!(
        merged.counters.inodes_scanned,
        baseline.counters.inodes_scanned
    );
    assert!(
        METRICS.job_fleet_shards_relocal.load(Ordering::Relaxed) > relocal0,
        "the refused residue ran locally"
    );

    host.shutdown().await;
    let report = m1.await.expect("worker joins");
    assert_eq!(report.shards_aborted, 1, "the member counted its refusal");
    fab.shutdown_abrupt().await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// The persisted report survives the fleet's mapping identities
// ---------------------------------------------------------------------------

/// Repro-port of the rig-found persist failure: the fleet's shard
/// reports carry the mapping-identity `partial` census (merge-input
/// data), and a 512-block corpus's REPORT blew the 64 KiB xattr cap —
/// so NOTHING persisted and the CLI read "no report". The persisted
/// `job:{id}:report` must exist, decode, and carry NO partial census
/// (findings + counters are the record).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_job_report_persists_without_the_partial_census() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    seed_tree(&fx, 4).await;

    let mover = squeezefs::jobs::MoverCtx::router_only(fx.fs.router.clone());
    let fab = JobFabric::start(fx.meta.clone(), 1, 100, Some(mover))
        .await
        .expect("fabric start");
    let job_id = fab
        .submit(squeezefs::jobs::JobSpec {
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
        .expect("submit fsck job");
    let state = fab
        .wait_terminal(&job_id, Duration::from_secs(60))
        .await
        .expect("job reaches terminal state");
    assert_eq!(state, squeezefs::jobs::JobState::Completed);

    let raw = fx
        .meta
        .getxattr(1, &format!("job:{job_id}:report"))
        .await
        .expect("meta read")
        .expect("the report xattr persists (the rig-found regression)");
    let report: squeezefs::fsck::FsckReport =
        serde_json::from_slice(&raw).expect("the persisted report decodes");
    assert!(
        report.partial.is_none(),
        "the partial census is merge-input data and never persists"
    );
    // This pin's subject is the PERSIST, not cleanliness: the fabric
    // path stamps expected_generation, and the fixture's tempdir
    // staging carries no marker, so a C5 finding here is fixture-shape
    // (the healthy-volume findings-0 law is the other pins' business).
    assert!(report.counters.inodes_scanned > 0);
    fab.shutdown_abrupt().await;
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Gate 3 — movers never fight custody
// ---------------------------------------------------------------------------

/// Contract (charter gate 3): an ino with a LIVE S9 custody grant is NOT
/// quiescent — the §5.7 mover probe defers (`job_mover_custody_defers`),
/// and the deferral lifts when the custody authority no longer holds a
/// grant over the ino.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custody_grant_defers_the_mover_probe() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;
    // The granted ino is SYNTHETIC (a co-writer's file this authority
    // process never opened): the authority's arbiter shares the
    // process-local lock table, so an ino the fixture itself holds a
    // write lease on would CONFLICT at grant time — which is itself
    // correct behavior (custody arbitration working), but not this
    // pin's subject. The probe consults the grant table by ino number;
    // existence is immaterial to the deferral law.
    let ino: u64 = 777_777;

    // Arm a custody authority and grant a (simulated) co-writer
    // whole-inode custody of `ino`.
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(60_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("valid clocks");
    let owner = WriteCustodyOwner::arm(
        "owner-fleet",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("authority arms");
    let lease = owner
        .join(&JoinFrame {
            schema: CUSTODY_SCHEMA,
            client: "cw-1".to_string(),
            pr_key: 0,
            prior_epoch: None,
        })
        .expect("co-writer joins");
    owner
        .grant(&AcquireFrame {
            schema: CUSTODY_SCHEMA,
            client: "cw-1".to_string(),
            lease_epoch: lease.epoch,
            ino,
            span: None,
            concurrent_write: false,
            wait_ms: 500,
        })
        .await
        .expect("whole-inode custody granted");

    struct OwnerGuard;
    impl Drop for OwnerGuard {
        fn drop(&mut self) {
            data_grant::uninstall_custody_owner();
        }
    }
    let _guard = OwnerGuard;
    data_grant::install_custody_owner(Arc::clone(&owner));

    let probe = fx.fs.mover_quiesce_probe();
    // Block index 99 was never written: no active buffer, no staged
    // custody — the pre-existing layers all admit it, isolating the
    // custody arm.
    let defers0 = METRICS.job_mover_custody_defers.load(Ordering::Relaxed);
    assert!(
        !probe(ino, 99),
        "an ino under live S9 custody is NOT quiescent — the mover defers"
    );
    assert!(
        METRICS.job_mover_custody_defers.load(Ordering::Relaxed) > defers0,
        "the deferral is counted (job_mover_custody_defers)"
    );

    // A DIFFERENT ino with no grant stays quiescent while the authority
    // is installed — the custody arm never over-defers.
    let free_ino = create_file(&fx, "free.bin").await;
    assert!(
        probe(free_ino, 99),
        "an ungranted ino stays quiescent under an installed authority"
    );

    // Authority uninstalled (unmount/disarm shape): the deferral lifts
    // and the probe returns to the shipped layers.
    drop(_guard);
    assert!(
        probe(ino, 99),
        "with no custody authority installed the probe returns to the shipped layers"
    );
    fx.close().await;
}
