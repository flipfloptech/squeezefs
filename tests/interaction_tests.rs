//! PR VL9 — cross-feature interaction policy pins, red-first
//! (docs/design-volume-lifecycle.md §PR VL9; AGENTS TDD posture):
//!
//! - **Mover-class serialization (pin a)**: mover-class jobs
//!   (`EvacuateVolume` / `Rebalance` / `DefragData`) whose volume scope
//!   intersects a RUNNING mover **serialize via the fabric** — the later
//!   job stays `Queued` (loud log + `job_serialized_waits`, counted once
//!   per deferral episode) and is claimed when the running mover
//!   finishes. KD-6 idempotence is what makes queueing safe. Whole-set
//!   movers (`Rebalance`, `DefragData { volume_id: None }`) intersect
//!   everything; per-volume movers intersect only the same volume.
//! - **Disjoint scopes proceed (pin c, different volumes)**: a defrag
//!   scoped to another volume runs to terminal WHILE a drain holds its
//!   victim — no serialization for disjoint per-volume scopes.
//! - **Draining-volume defrag refuses loud (pin c, same volume, named)**:
//!   `DefragData { volume_id: Some(draining) }` fails loudly at plan
//!   time (the placement-eligibility refusal) — never a silent no-op.
//! - **R5 Red job pause (pin e)**: driving the mem budget to Red with
//!   the `job_copy_buffers` component charged pauses every RUNNING job
//!   loudly (`job_paused_mem_pressure`, durable `paused` record); the
//!   pause is deliberately NOT self-resuming — `job resume` is the
//!   operator's call once pressure clears, and it works.
//! - **Drain-during-fsck (pin b)**: fsck is NOT a mover — it runs to
//!   `Completed` with ZERO findings WHILE a drain is parked mid-move
//!   (the G-VL-5(a) drain-concurrent adversary, deterministic at cargo
//!   scale via the pre-publish park).
//! - **Meta-slot-migrate + data-drain (pin d)**: `MigrateMetaSlot` has
//!   no mover scope — it converges WHILE a drain is parked; both jobs
//!   complete, byte identity and global-ino stability hold across the
//!   pair (the cutover gate parks meta ops, never fabric workers).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::jobs::{JobFabric, JobSpec, JobState, JobType, MoverCtx, JOB_XATTR_PREFIX};
use squeezefs::mem_budget::{Component, Level, MemBudget};
use squeezefs::meta_backend::Metadata;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig, VOL_STATE_DRAINING, VOL_STATE_RETIRED};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
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

/// Format a stamped `--meta-slots` set the way `format` does (config
/// xattr on member 0 only) — the pin (d) fixture shape.
async fn format_stamped_metas(metas: &[PathBuf], width: u32, data_lvs: &[&Path]) {
    let cfg = base_format_config(data_lvs);
    let plan = squeezefs::meta_backend::plan_meta_slot_set_with_width(metas.len(), width)
        .expect("plan admits");
    for (i, m) in metas.iter().enumerate() {
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            m,
            256 * 1024 * 1024,
            &squeezefs::meta_backend::kv::builder::FormatV3Options {
                node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
                journal_len_override: None,
                force: true,
                full_wipe: false,
                format_config_xattr: (i == 0).then(|| serde_json::to_vec(&cfg).unwrap()),
            },
            plan.stamps[i].clone(),
        )
        .await
        .expect("format stamped meta volume");
    }
}

/// Mount-shaped fixture (the VL4 drain-suite shape): resolved volume
/// records drive `register_backend`, the first record's device/allocator
/// are the router's default slot, and the job fabric runs with the mover
/// context wired.
struct Fx {
    fs: Arc<SqueezefsFilesystem>,
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    fabric: Arc<JobFabric>,
    staging_path: PathBuf,
    _staging: TempDir,
}

async fn open_fixture(meta: &Path, records: &[DataVolumeRecord]) -> Fx {
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fixture_with_meta(routed, records).await
}

/// The stamped-set variant (pin d): opens the routed W-slot set the way
/// a mount does.
async fn open_fixture_stamped(metas: &[PathBuf], records: &[DataVolumeRecord]) -> Fx {
    let uris: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .expect("open stamped meta set");
    fixture_with_meta(routed, records).await
}

async fn fixture_with_meta(
    routed: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    records: &[DataVolumeRecord],
) -> Fx {
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

    Fx {
        fs,
        meta: routed,
        fabric,
        staging_path,
        _staging: staging,
    }
}

impl Fx {
    async fn close(self) {
        self.fabric.shutdown_abrupt().await;
        for vol in &self.meta.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
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
/// fsync. Returns the expected content.
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
        let data = vec![(b as u8) ^ 0x5C; BLOCK];
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

async fn read_back(fx: &Fx, ino: u64, nblocks: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(nblocks * BLOCK);
    for b in 0..nblocks {
        let reply = fx
            .fs
            .read(req(), ino, 0, (b * BLOCK) as u64, BLOCK as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("read of block {b} failed: {e:?}"));
        out.extend_from_slice(&reply.data);
    }
    out
}

/// Distinct victim base offsets referenced by the ino's durable block
/// map (the drain census the mover must clear).
async fn victim_blocks_of(fx: &Fx, ino: u64, victim: &str) -> Vec<(u32, String)> {
    let meta = fx
        .fs
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("layout");
    let mut out = Vec::new();
    if let Some(map) = meta.block_map.as_deref() {
        for (&b, mapping) in map {
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
            let (be, _off) = fx
                .fs
                .router
                .backend_router
                .parse_block_key(&clean)
                .expect("parse");
            if be == victim {
                out.push((b, mapping.clone()));
            }
        }
    }
    out
}

async fn job_state(fx: &Fx, job_id: &str) -> JobState {
    fx.fabric
        .status(job_id)
        .await
        .expect("status")
        .expect("known job")
        .state
}

async fn durable_record(fx: &Fx, job_id: &str) -> serde_json::Value {
    let raw = fx
        .meta
        .getxattr(1, &format!("{JOB_XATTR_PREFIX}{job_id}"))
        .await
        .expect("backend read")
        .expect("durable job record");
    serde_json::from_slice(&raw).expect("job record is JSON")
}

// ---------------------------------------------------------------------------
// Pin (a): same/intersecting mover scopes SERIALIZE via the fabric —
// queued loud, run after (KD-6 makes queueing safe)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intersecting_scope_movers_serialize_queued_loud_then_run() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const NBLOCKS: usize = 24;
    let ino = create_file(&fx, "burst.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS).await;
    assert!(
        !victim_blocks_of(&fx, ino, "oss2").await.is_empty(),
        "placement must have spread blocks onto oss2"
    );

    // Park the drain at its first pre-publish window: the drain is then
    // provably RUNNING (claimed, mid-move) while the whole-set movers
    // are submitted.
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = std::sync::atomic::AtomicBool::new(false);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(60));
        }
    }));

    let evac_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");
    hit_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the drain must reach its parked publish window");
    assert_eq!(job_state(&fx, &evac_id).await, JobState::Running);

    // Both whole-set movers intersect the drain's scope: they must stay
    // Queued while the drain runs, each counted ONCE.
    let waits_before = METRICS.job_serialized_waits.load(Ordering::Relaxed);
    let defrag_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData { volume_id: None },
            throttle_pct: 100,
        })
        .await
        .expect("submit defrag");
    let rebalance_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::Rebalance,
            throttle_pct: 100,
        })
        .await
        .expect("submit rebalance");

    // Give the second worker several claim-poll cycles: without the
    // serialization pin it would claim a whole-set mover immediately.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        job_state(&fx, &defrag_id).await,
        JobState::Queued,
        "a whole-set defrag must serialize behind the running drain (pin a)"
    );
    assert_eq!(
        job_state(&fx, &rebalance_id).await,
        JobState::Queued,
        "a rebalance must serialize behind the running drain (pin a)"
    );
    let waits_after = METRICS.job_serialized_waits.load(Ordering::Relaxed);
    assert!(
        waits_after >= waits_before + 2,
        "each deferred mover counts job_serialized_waits once \
         ({waits_before} -> {waits_after})"
    );

    // Release the drain: it converges; the deferred movers then run to
    // terminal (queueing, never starvation — KD-6).
    let _ = go_tx.send(());
    squeezefs::jobs::clear_evacuate_pre_publish_hook();
    let end = fx
        .fabric
        .wait_terminal(&evac_id, Duration::from_secs(180))
        .await
        .expect("drain terminal");
    assert_eq!(end, JobState::Completed, "the drain must converge");
    for (label, id) in [("defrag", &defrag_id), ("rebalance", &rebalance_id)] {
        let end = fx
            .fabric
            .wait_terminal(id, Duration::from_secs(120))
            .await
            .unwrap_or_else(|e| panic!("{label} must run after the drain: {e:?}"));
        assert_eq!(
            end,
            JobState::Completed,
            "{label} must complete once the drain released its scope"
        );
    }

    assert_eq!(
        fx.fs.router.backend_router.volume_state("oss2").as_deref(),
        Some(VOL_STATE_RETIRED),
        "the drain retired its victim"
    );
    assert_eq!(
        read_back(&fx, ino, NBLOCKS).await,
        expected,
        "byte identity across the serialized mover sequence"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Pin (c), different volumes: DISJOINT per-volume scopes run concurrently
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disjoint_scope_movers_run_concurrently() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    let oss3 = make_file(dir.path(), "oss3", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2, &oss3]).await;
    let recs = base_format_config(&[&oss1, &oss2, &oss3]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const NBLOCKS: usize = 24;
    let ino = create_file(&fx, "burst.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS).await;
    let on_victim = victim_blocks_of(&fx, ino, "oss3").await;
    assert!(
        !on_victim.is_empty(),
        "placement must have spread blocks onto oss3"
    );

    // Park the drain of oss3 at one SPECIFIC victim block's publish so
    // a concurrent defrag (which may legitimately touch other blocks of
    // the same file) never trips the park itself.
    let park_block = on_victim[0].0;
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = std::sync::atomic::AtomicBool::new(false);
    let target = (ino, park_block);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |h_ino, h_block| {
        if (h_ino, h_block) == target && !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(60));
        }
    }));

    let evac_id = fx
        .fs
        .admin_remove_data_volume("oss3", 100)
        .await
        .expect("remove-data admits");
    hit_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the drain must reach its parked publish window");

    // A defrag scoped to a DIFFERENT volume proceeds while the drain
    // holds oss3 (disjoint per-volume scopes never serialize).
    let defrag_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData {
                volume_id: Some("oss2".to_string()),
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit defrag oss2");
    let end = fx
        .fabric
        .wait_terminal(&defrag_id, Duration::from_secs(60))
        .await
        .expect("a disjoint-scope defrag must run WHILE the drain is parked");
    assert_eq!(end, JobState::Completed, "defrag(oss2) completes mid-drain");
    assert_eq!(
        job_state(&fx, &evac_id).await,
        JobState::Running,
        "the drain must still be running (parked) — concurrency proven"
    );

    let _ = go_tx.send(());
    squeezefs::jobs::clear_evacuate_pre_publish_hook();
    let end = fx
        .fabric
        .wait_terminal(&evac_id, Duration::from_secs(180))
        .await
        .expect("drain terminal");
    assert_eq!(end, JobState::Completed, "the drain must converge");
    assert_eq!(
        read_back(&fx, ino, NBLOCKS).await,
        expected,
        "byte identity across the concurrent mover pair"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Pin (c), same volume, named: defrag NAMING a draining volume fails loud
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn defrag_naming_a_draining_volume_fails_loud() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // Draining state with NO running evacuation job (offline/adopted
    // shape): no serialization conflict exists, so the named defrag is
    // claimed immediately — and must refuse loudly at plan time.
    fx.fs
        .router
        .backend_router
        .set_volume_state("oss2", VOL_STATE_DRAINING)
        .expect("flip to draining");

    let defrag_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::DefragData {
                volume_id: Some("oss2".to_string()),
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    let end = fx
        .fabric
        .wait_terminal(&defrag_id, Duration::from_secs(60))
        .await
        .expect("terminal");
    assert_eq!(
        end,
        JobState::Failed,
        "defrag naming a draining volume must fail loud, not no-op"
    );
    let rec = durable_record(&fx, &defrag_id).await;
    let err = rec["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("placement-eligible"),
        "the durable error must name the eligibility refusal: {rec}"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Pin (e): R5 Red with job_copy_buffers charged pauses running jobs
// loudly; NOT self-resuming; operator resume recovers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn red_pressure_pauses_jobs_loudly_and_operator_resume_recovers() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_meta(&meta, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    // A long-running job to pause mid-flight.
    let job_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 20_000,
                task_ms: 2,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while job_state(&fx, &job_id).await != JobState::Running {
        assert!(
            tokio::time::Instant::now() < deadline,
            "job must reach Running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Shed at/under target is a no-op (the gauge guard): nothing pauses.
    let paused_before = METRICS.job_paused_mem_pressure.load(Ordering::Relaxed);
    fx.fabric.shed_to(u64::MAX);
    assert_eq!(
        job_state(&fx, &job_id).await,
        JobState::Running,
        "shed with gauge <= target must not pause anything"
    );

    // The mount wiring, mirrored on a PRIVATE budget instance: the
    // `job_copy_buffers` component (floor 0, weight 1) gauges
    // METRICS.job_copy_buffer_bytes and sheds through the fabric.
    let mb = MemBudget::new_for_test();
    let shed_fabric = fx.fabric.clone();
    mb.register(Component::new(
        "job_copy_buffers",
        0,
        1,
        Arc::new(|| METRICS.job_copy_buffer_bytes.load(Ordering::Relaxed)),
        Arc::new(move |target| shed_fabric.shed_to(target)),
    ));

    // Charge the copy-window gauge past the Red edge and tick: pressure
    // = 990/1000 = 99 % >= RED_ENTER (95 %) => Red => shed => pause.
    const CHARGE: u64 = 990;
    METRICS
        .job_copy_buffer_bytes
        .fetch_add(CHARGE, Ordering::Relaxed);
    mb.tick_inner(1000, 0, 0);
    assert_eq!(mb.level(), Level::Red, "the charged tick must enter Red");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while job_state(&fx, &job_id).await != JobState::Paused {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Red shed must pause the running job (job_paused_mem_pressure)"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        METRICS.job_paused_mem_pressure.load(Ordering::Relaxed) > paused_before,
        "the pause must be counted (loud)"
    );
    // Durable: the worker checkpoints the paused state.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let rec = durable_record(&fx, &job_id).await;
        if rec["state"] == "paused" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the paused state must reach the durable record: {rec}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Pressure clears (uncharge + a quiet tick => Green) — the job must
    // NOT self-resume: `job resume` is the operator's call.
    METRICS
        .job_copy_buffer_bytes
        .fetch_sub(CHARGE, Ordering::Relaxed);
    mb.tick_inner(1000, 0, 0);
    assert_eq!(mb.level(), Level::Green, "pressure receded to Green");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        job_state(&fx, &job_id).await,
        JobState::Paused,
        "mem-pressure pauses are deliberately NOT self-resuming"
    );

    // Operator resume works: the job advances again.
    fx.fabric.resume(&job_id).await.expect("resume");
    let before = fx.fabric.status(&job_id).await.unwrap().unwrap().tasks_done;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let st = fx.fabric.status(&job_id).await.unwrap().unwrap();
        if st.state == JobState::Running && st.tasks_done > before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resumed job must advance again ({before} -> {})",
            st.tasks_done
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    fx.fabric.cancel(&job_id).await.expect("cancel");
    fx.fabric
        .wait_terminal(&job_id, Duration::from_secs(10))
        .await
        .expect("terminal after cancel");
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Pin (b): drain-during-fsck — BOTH proceed (fsck is not a mover, no
// serialization), fsck completes with ZERO findings while the mover's
// pre-publish state is live (the G-VL-5(a) drain-concurrent adversary
// at cargo scale, deterministic via the parked publish window)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_completes_clean_while_a_drain_is_parked_mid_move() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_meta(&meta, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture(&meta, &recs).await;

    const NBLOCKS: usize = 24;
    let ino = create_file(&fx, "burst.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS).await;
    assert!(
        !victim_blocks_of(&fx, ino, "oss2").await.is_empty(),
        "placement must have spread blocks onto oss2"
    );

    // The fabric-run fsck scans with expected_generation =
    // volume_generation (the mount posture) — bind the fixture staging
    // dir the way a mount does so C5 sees the mounted generation.
    squeezefs::cache::nvme::write_staging_generation_marker(
        &fx.staging_path,
        &squeezefs::fsck::volume_generation(&fx.meta),
    )
    .await
    .expect("stamp staging generation");

    // Park the drain mid-move: fsck then runs against the live mover
    // state (pre-publish copy done, publish pending — the in-flight
    // destination is exactly what the C2/C3 machinery must not flag).
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = std::sync::atomic::AtomicBool::new(false);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(120));
        }
    }));

    let evac_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");
    hit_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the drain must reach its parked publish window");
    assert_eq!(job_state(&fx, &evac_id).await, JobState::Running);

    // fsck is NOT a mover: it must be claimed and run to completion
    // WHILE the drain holds oss2 — and report zero findings.
    let fsck_id = fx
        .fabric
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
        .expect("submit fsck");
    let end = fx
        .fabric
        .wait_terminal(&fsck_id, Duration::from_secs(120))
        .await
        .expect("fsck must run WHILE the drain is parked (no serialization)");
    assert_eq!(end, JobState::Completed, "drain-concurrent fsck completes");
    assert_eq!(
        job_state(&fx, &evac_id).await,
        JobState::Running,
        "the drain must still be running (parked) — concurrency proven"
    );

    // FP = 0: the persisted report carries zero findings and a real scan.
    let report_raw = fx
        .meta
        .getxattr(1, &format!("{JOB_XATTR_PREFIX}{fsck_id}:report"))
        .await
        .expect("backend read")
        .expect("fsck report persisted");
    let report: serde_json::Value = serde_json::from_slice(&report_raw).expect("report is JSON");
    assert_eq!(
        report["findings"].as_array().map(Vec::len),
        Some(0),
        "drain-concurrent fsck must report ZERO findings (G-VL-5(a)): {report}"
    );
    assert!(
        report["counters"]["inodes_scanned"].as_u64().unwrap_or(0) > 0,
        "engagement: the scan must have walked inodes: {report}"
    );

    // Release the drain: it converges; byte identity holds.
    let _ = go_tx.send(());
    squeezefs::jobs::clear_evacuate_pre_publish_hook();
    let end = fx
        .fabric
        .wait_terminal(&evac_id, Duration::from_secs(180))
        .await
        .expect("drain terminal");
    assert_eq!(end, JobState::Completed, "the drain must converge");
    assert_eq!(
        fx.fs.router.backend_router.volume_state("oss2").as_deref(),
        Some(VOL_STATE_RETIRED),
        "the drain retired its victim"
    );
    assert_eq!(
        read_back(&fx, ino, NBLOCKS).await,
        expected,
        "byte identity across the drain-concurrent fsck"
    );
    fx.close().await;
}

// ---------------------------------------------------------------------------
// Pin (d): meta-slot-migrate + data-drain concurrently — BOTH converge
// (MigrateMetaSlot is not a mover-class job: no scope conflict; the
// cutover gate parks meta ops, never fabric workers)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_migration_and_data_drain_converge_concurrently() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let m1 = make_file(dir.path(), "meta1", 256 * 1024 * 1024);
    let m2 = make_file(dir.path(), "meta2", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    let oss2 = make_file(dir.path(), "oss2", 4 << 30);
    format_stamped_metas(&[m1.clone(), m2.clone()], 4, &[&oss1, &oss2]).await;
    let recs = base_format_config(&[&oss1, &oss2]).resolved_data_volumes();
    let fx = open_fixture_stamped(&[m1, m2], &recs).await;

    const NBLOCKS: usize = 24;
    let ino = create_file(&fx, "burst.bin").await;
    let expected = striped_burst(&fx, ino, NBLOCKS).await;
    assert!(
        !victim_blocks_of(&fx, ino, "oss2").await.is_empty(),
        "placement must have spread blocks onto oss2"
    );
    // Populate slot-1 keyspace beyond the root records (ino 1 % 4 = 1 —
    // the root dir itself rides the migrating slot).
    let mut aux = Vec::new();
    for i in 0..8 {
        let name = format!("aux{i}.txt");
        let a = create_file(&fx, &name).await;
        aux.push((name, a));
    }

    // Park the drain mid-move, then run the slot migration to completion
    // WHILE the drain provably holds its claim.
    let (hit_tx, hit_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    let fired = std::sync::atomic::AtomicBool::new(false);
    squeezefs::jobs::set_evacuate_pre_publish_hook(Arc::new(move |_ino, _b| {
        if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let _ = hit_tx.send(());
            let _ = go_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(120));
        }
    }));

    let evac_id = fx
        .fs
        .admin_remove_data_volume("oss2", 100)
        .await
        .expect("remove-data admits");
    hit_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the drain must reach its parked publish window");
    assert_eq!(job_state(&fx, &evac_id).await, JobState::Running);

    // Slot 1 natively lives on volume 1 (slot_map = s % n): migrate it
    // to volume 0 while the drain runs. Both jobs must converge.
    let mig_id = fx
        .fabric
        .submit(JobSpec {
            job_type: JobType::MigrateMetaSlot {
                slot: 1,
                target_volume: 0,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit migrate-meta-slot");
    let end = fx
        .fabric
        .wait_terminal(&mig_id, Duration::from_secs(180))
        .await
        .expect("slot migration must run WHILE the drain is parked");
    assert_eq!(
        end,
        JobState::Completed,
        "the slot migration must converge mid-drain"
    );
    assert_eq!(
        job_state(&fx, &evac_id).await,
        JobState::Running,
        "the drain must still be running (parked) — concurrency proven"
    );

    // Release the drain: it converges too.
    let _ = go_tx.send(());
    squeezefs::jobs::clear_evacuate_pre_publish_hook();
    let end = fx
        .fabric
        .wait_terminal(&evac_id, Duration::from_secs(180))
        .await
        .expect("drain terminal");
    assert_eq!(end, JobState::Completed, "the drain must converge");
    assert_eq!(
        fx.fs.router.backend_router.volume_state("oss2").as_deref(),
        Some(VOL_STATE_RETIRED),
        "the drain retired its victim"
    );

    // Byte identity + name→ino stability across BOTH concurrent ops.
    assert_eq!(
        read_back(&fx, ino, NBLOCKS).await,
        expected,
        "byte identity across the migrate+drain pair"
    );
    for (name, a) in &aux {
        let got = fx
            .fs
            .lookup(req(), 1, OsStr::new(name))
            .await
            .unwrap_or_else(|e| panic!("lookup {name} after migrate+drain: {e:?}"))
            .attr
            .ino;
        assert_eq!(got, *a, "global ino of {name} must be eternally stable");
    }
    fx.close().await;
}

// ---------------------------------------------------------------------------
// fsck × meta-add: the census walk over a GUEST-ONLY member (a volume
// added by `volume add-meta` — no legacy keyspace) must not panic on the
// member's raw bootstrap records, and must report zero findings.
// (Caught by the canonical lifecycle soak, iteration 9: `fsck --offline`
// after add-meta panicked "raw local ino on a volume with no legacy
// keyspace (routing bug)" — the walk fed the guest-only member's local
// control record into make_global_ino.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_census_survives_a_guest_only_meta_member() {
    let _serial = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let m1 = make_file(dir.path(), "meta1", 256 * 1024 * 1024);
    let m2 = make_file(dir.path(), "meta2", 256 * 1024 * 1024);
    let oss1 = make_file(dir.path(), "oss1", 4 << 30);
    format_stamped_metas(&[m1.clone(), m2.clone()], 8, &[&oss1]).await;
    let recs = base_format_config(&[&oss1]).resolved_data_volumes();

    // Seed a dataset on the 2-member set, then close it cleanly.
    {
        let fx = open_fixture_stamped(&[m1.clone(), m2.clone()], &recs).await;
        let ino = create_file(&fx, "burst.bin").await;
        striped_burst(&fx, ino, 8).await;
        for i in 0..6 {
            create_file(&fx, &format!("aux{i}.txt")).await;
        }
        fx.close().await;
    }

    // The offline add-meta: the third member takes 2 slots as a GUEST —
    // it has NO legacy keyspace (its only raw records are bootstrap
    // control records).
    let m3 = make_file(dir.path(), "meta3", 256 * 1024 * 1024);
    let uris: Vec<String> = [&m1, &m2].iter().map(|p| p.display().to_string()).collect();
    let taken = squeezefs::config_ops::add_meta_volume(
        &uris,
        &m3.display().to_string(),
        &squeezefs::config_ops::TakeSlots::Count(2),
    )
    .await
    .expect("add-meta converges");
    assert_eq!(taken.len(), 2, "the new member hosts 2 guest slots");

    // Reopen the 3-member set and run the fsck census (the soak's
    // step-9 posture): it must complete without panicking on the
    // guest-only member and report zero findings.
    let fx = open_fixture_stamped(&[m1, m2, m3], &recs).await;
    let ctx = squeezefs::fsck::FsckCtx {
        meta: fx.meta.clone(),
        router: fx.fs.router.clone(),
        staging_dirs: vec![fx.staging_path.clone()],
        expected_generation: None,
    };
    let mut opts = squeezefs::fsck::FsckOptions::online();
    opts.settle = Duration::from_millis(100);
    let report = squeezefs::fsck::run(&ctx, &opts)
        .await
        .expect("fsck census must survive a guest-only member (no panic, no error)");
    assert!(
        report.findings.is_empty(),
        "healthy 3-member set must report zero findings: {:?}",
        report.findings
    );
    assert!(
        report.counters.inodes_scanned > 0,
        "engagement: the census walked the real inodes"
    );
    fx.close().await;
}
