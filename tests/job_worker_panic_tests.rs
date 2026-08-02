//! RES-7 (pre-RC engineering spec §7): a panicking job must not take the
//! worker with it, and must never strand its claim.
//!
//! `worker_loop` called `run_job(...).await` and then cleared
//! `ctl.claimed`. An unwind skipped the clear AND killed the worker task
//! — whose `JoinHandle` sits in `JobFabric.handles` and is never
//! observed, so the loss was silent. Because `claimed` is the VL9 pin
//! (a) mover-serialization token, the stranded flag then blocked EVERY
//! mover-class job whose volume scope intersects the dead one's,
//! FOREVER: a drain submitted after the panic sits `Queued`, logging
//! "queued behind a running mover" about a mover that no longer exists.
//!
//! The tree already has the shape this needs — the KV commit conveyor's
//! release-on-unwind `PassSentinel` and the publish conveyor's
//! `PassGuard` (spec §8: "`#[must_use]` `Admission` plus release-on-
//! unwind `PassSentinel` close the budget-leak and wedge classes on
//! every traced panic path"). The job fabric is the one detached
//! executor that never got it.
//!
//! Contracts pinned here:
//!
//! 1. **The claim is released on unwind** — a mover-class job submitted
//!    after a panicking mover's death runs instead of queueing forever.
//! 2. **The worker survives** — the pool does not shrink by one task per
//!    panic; the next job still runs.
//! 3. **Counted and loud** — `job_worker_panics` (0 on healthy mounts)
//!    and a durable `Failed` record with the panic as its error, rather
//!    than a job wedged `Running` forever.
//!
//! RED against dev 7d1ec2e1: the panic escapes `worker_loop`, the worker
//! task dies, `claimed` stays `true`, the record stays `Running`, and
//! `job_worker_panics` does not exist.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::jobs::{JobFabric, JobSpec, JobState, JobType};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile};

struct Fx {
    meta: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    _fs: Arc<SqueezefsFilesystem>,
    _meta_file: NamedTempFile,
    _backing: NamedTempFile,
    _staging: tempfile::TempDir,
}

async fn fixture(tag: &str) -> Fx {
    let dlm = DlmClient::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(tag).await.expect("allocator"));
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, alloc, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let meta_file = NamedTempFile::new().unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        meta_file.path(),
        256 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(b"{\"name\":\"jobpanic\"}".to_vec()),
        },
    )
    .await
    .expect("format v3 meta volume");
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta_file.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    Fx {
        meta: routed,
        _fs: Arc::new(fs),
        _meta_file: meta_file,
        _backing: backing,
        _staging: staging,
    }
}

/// Disarm the panic seam whatever the test does.
struct SeamGuard;
impl Drop for SeamGuard {
    fn drop(&mut self) {
        squeezefs::jobs::set_test_job_panic_after(0);
    }
}

fn worker_panics() -> u64 {
    METRICS.job_worker_panics.load(Ordering::Relaxed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panicking_job_releases_its_claim_counts_and_keeps_the_worker() {
    let _seam = SeamGuard;
    let fx = fixture("fab-panic").await;
    // ONE worker: if the panic kills it, the pool is empty and nothing
    // else can ever run — the field shape (the loss is silent because
    // the JoinHandle is never observed).
    let fab = JobFabric::start(fx.meta.clone(), 1, 100, None)
        .await
        .expect("fabric start");

    let before = worker_panics();
    squeezefs::jobs::set_test_job_panic_after(1);
    let doomed = fab
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 8,
                task_ms: 0,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");

    // Contract 3: the job reaches a TERMINAL state (Failed) rather than
    // wedging Running forever, and the panic is counted.
    fab.wait_terminal(&doomed, Duration::from_secs(20))
        .await
        .expect(
            "RES-7: a panicking job must reach a terminal state — an unwound \
             worker leaves it Running forever",
        );
    let status = fab
        .status(&doomed)
        .await
        .expect("status")
        .expect("known job");
    assert_eq!(
        status.state,
        JobState::Failed,
        "a panicking job fails loudly, it does not silently vanish"
    );
    assert_eq!(
        worker_panics() - before,
        1,
        "RES-7: every worker unwind must be counted (job_worker_panics)"
    );

    // Contract 2: the pool survived — a second job still runs on it.
    squeezefs::jobs::set_test_job_panic_after(0);
    let follow = fab
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 4,
                task_ms: 0,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit follow-up");
    fab.wait_terminal(&follow, Duration::from_secs(20))
        .await
        .expect("RES-7: the panic must not remove a worker from the pool");
    assert_eq!(
        fab.status(&follow)
            .await
            .expect("status")
            .expect("known job")
            .state,
        JobState::Completed
    );
}

/// Contract 1 — the stranded `claimed` flag is the wedge: by the VL9
/// pin-(a) mover serialization it blocks EVERY intersecting mover-class
/// job on that volume scope, permanently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panicking_mover_does_not_wedge_the_volume_scope_forever() {
    let _seam = SeamGuard;
    let fx = fixture("fab-panic-scope").await;
    // Two workers so the follow-up mover has somewhere to run even if
    // the panicking one's worker were lost: this test isolates the
    // CLAIM leak from the worker loss.
    let fab = JobFabric::start(fx.meta.clone(), 2, 100, None)
        .await
        .expect("fabric start");

    squeezefs::jobs::set_test_job_panic_after(1);
    let doomed = fab
        .submit(JobSpec {
            job_type: JobType::Noop {
                tasks: 8,
                task_ms: 0,
            },
            throttle_pct: 100,
        })
        .await
        .expect("submit");
    fab.wait_terminal(&doomed, Duration::from_secs(20))
        .await
        .expect("the panicking job must go terminal");
    squeezefs::jobs::set_test_job_panic_after(0);

    // The dead job's ctl must no longer hold its claim. A live claim is
    // only observable through its effect: a mover-class job whose scope
    // intersects it stays Queued forever. Rebalance's scope intersects
    // every volume, which is exactly the blast radius the spec names.
    let rebalance = fab
        .submit(JobSpec {
            job_type: JobType::Rebalance,
            throttle_pct: 100,
        })
        .await
        .expect("submit rebalance");
    fab.wait_terminal(&rebalance, Duration::from_secs(20))
        .await
        .expect(
            "RES-7: a mover-class job must not queue behind a claim the \
             panicking worker never released",
        );
}
