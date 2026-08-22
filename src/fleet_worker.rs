//! KD-MW-16 (rung 10c, `docs/design-mw-fleet-jobs.md` §2/§5) — the
//! **mount-side fleet job worker**: a reader/co-writer member that has
//! joined the S6 membership plane serves fleet READ shards (fsck
//! census/scrub residues) to the coordinator over the §5.1.6 wire.
//!
//! **Eligibility is BY MEMBERSHIP; authentication stays storage-trust.**
//! The worker reads the `job:enroll` secret through this mount's OWN
//! meta backend — the access it already holds IS the credential (ruling
//! D2) — and enrolls over the same challenge/HMAC handshake every worker
//! uses; its `worker_id` is the mount's durable KD-MW-2 enrollment id,
//! so the coordinator's shard records name roster identities. The
//! manual `squeezefs job worker` verb survives unchanged for non-mount
//! storage-trust workers.
//!
//! **The R5 composition (design §5)**: a shard runs only while this
//! member's OWN memory budget is below Red — Red at admission refuses
//! the shard, Red mid-walk cancels it, and either way the worker
//! ABANDONS loudly (`ShardAbandon`, `job_fleet_worker_red_aborts`) and
//! never proposes a partial report; the coordinator re-leases the
//! residue promptly.

use crate::fsck::{self, FsckCtx, FsckOptions};
use crate::fuse_client::METRICS;
use crate::job_wire::{
    self, DestTuple, FleetShardSpec, JobWireWorker, ShardDeviceSeam, WorkerOptions, CAP_FLEET_READ,
    HEARTBEAT_INTERVAL,
};
use crate::jobs::JobType;
use crate::mem_budget::Level;
use crate::meta_backend::RoutedMetaBackend;
use squeezefs_ipc::sqz_blocking;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The R5 refusal law, pure: a Red budget refuses fleet-shard work —
/// the member sheds its own load first, and the coordinator re-leases
/// (VL9 pin (e)'s per-client face). Yellow keeps working: shedding is
/// the tiers' business, and a duty-cycled read walk is not what Yellow
/// exists to stop.
pub fn refuse_on_red(level: Level) -> std::io::Result<()> {
    if level == Level::Red {
        return Err(std::io::Error::other(
            "R5 memory budget is Red on this member — fleet shard refused \
             (design-mw-fleet-jobs §5; the coordinator re-leases)",
        ));
    }
    Ok(())
}

/// The mount-side fleet shard seam: executes census/scrub residues over
/// THIS mount's live meta view + router (offline posture — a member's
/// coherent view has nothing of its own in flight; the coordinator's
/// finalize ladder re-verifies against CURRENT state before any
/// verdict), with the member's OWN staging dirs scanned FULL
/// (design §3: staging shards by locality, not residue).
pub struct MountFleetSeam {
    meta: Arc<RoutedMetaBackend>,
    router: crate::routing::DataRouter,
    staging_dirs: Vec<PathBuf>,
    expected_generation: Option<String>,
    /// Injected budget probe (the pin's seam; production =
    /// `mem_budget::level`).
    level: fn() -> Level,
}

impl MountFleetSeam {
    pub fn new(meta: Arc<RoutedMetaBackend>, router: crate::routing::DataRouter) -> Self {
        let staging_dirs = router.cache.nvme.staging_dirs().to_vec();
        let expected_generation = Some(fsck::volume_generation(&meta));
        Self {
            meta,
            router,
            staging_dirs,
            expected_generation,
            level: crate::mem_budget::level,
        }
    }
}

impl ShardDeviceSeam for MountFleetSeam {
    fn plan_blocks(&self, _job: &JobType) -> usize {
        0
    }
    fn block_len(&self) -> usize {
        0
    }
    fn allocate(&self, n: usize) -> std::io::Result<Vec<DestTuple>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        Err(std::io::Error::other(
            "the mount fleet seam is read-class: it never allocates destinations",
        ))
    }
    fn read_source(&self, _key: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "the mount fleet seam is read-class: no mover source reads",
        ))
    }
    fn write_block(&self, _dest: &DestTuple, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "the mount fleet seam is read-class: it never writes device blocks",
        ))
    }
    fn read_block(&self, _dest: &DestTuple) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other(
            "the mount fleet seam is read-class: no verify-reads happen here",
        ))
    }

    fn run_fleet_shard(&self, job: &JobType, spec: FleetShardSpec) -> std::io::Result<Vec<u8>> {
        // R5 at admission: Red refuses before any work (counted).
        if let Err(e) = refuse_on_red((self.level)()) {
            METRICS
                .job_fleet_worker_red_aborts
                .fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        let mut opts = FsckOptions::offline();
        opts.throttle_pct = spec.throttle_pct;
        if spec.inode_plane {
            // **KD-PV-16's OWNER shard**: the coordinator asked THIS node
            // for the inode plane over the volumes it appends to. The
            // posture is derived from this process's OWN ownership map —
            // never from the frame, which says only *that* the plane was
            // asked for, not which volumes this node owns.
            let owned: Vec<usize> = match crate::meta_ship::owners::owner_map()
                .filter(|m| m.multi_owner())
            {
                Some(map) => (0..map.volume_count())
                    .filter(|v| map.owner_of_volume(*v).is_none())
                    .collect(),
                None => {
                    // The coordinator believes this node owns volumes and
                    // this node's own plane says it owns none: refuse
                    // loud (the shard re-leases / its volumes read as
                    // uncovered) rather than answer about a set it has no
                    // authority over.
                    return Err(std::io::Error::other(
                        "an inode-plane shard was assigned to a node with no armed multi-owner \
                         ownership map — refused (KD-PV-16: only an owner may judge its own \
                         inos, and this node owns nothing here)",
                    ));
                }
            };
            opts.inode_plane = true;
            opts.inode_plane_only = true;
            opts.multi_owner = true;
            opts.owned_volumes = Some(owned);
            let ctx = FsckCtx {
                meta: self.meta.clone(),
                router: self.router.clone(),
                staging_dirs: Vec::new(), // the plane touches no staging
                expected_generation: self.expected_generation.clone(),
            };
            let report = sqz_blocking::block_on(fsck::run(&ctx, &opts))
                .map_err(|e| std::io::Error::other(format!("owner inode-plane shard: {e}")))?;
            return serde_json::to_vec(&report).map_err(std::io::Error::other);
        }
        opts.shard = Some((spec.k, spec.n));
        opts.staging_full = true;
        // The one-view law (`FsckOptions::inode_plane`): this member's
        // coherent view is staleness-bounded (S5) and its per-volume
        // checkpoint projections sit at different instants mid-churn, so
        // C9/C10 verdicts taken here manufacture the count/name loss
        // shapes from a healthy tree. A CENSUS residue shard therefore
        // never judges the plane whatever this node owns — it contributes
        // the census/refs the block-plane finalize needs and nothing else.
        opts.inode_plane = false;
        if let JobType::Fsck {
            scrub, scrub_only, ..
        } = job
        {
            opts.scrub = *scrub;
            opts.scrub_only = *scrub_only;
        }
        let ctx = FsckCtx {
            meta: self.meta.clone(),
            router: self.router.clone(),
            staging_dirs: self.staging_dirs.clone(),
            expected_generation: self.expected_generation.clone(),
        };
        // R5 mid-walk: a watcher thread flips the engine's cooperative
        // cancel on Red; a cancelled shard NEVER proposes (a partial
        // census would under-count — the exactly-once law).
        let red_hit = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = {
            let cancel = opts.cancel.clone();
            let red_hit = Arc::clone(&red_hit);
            let stop = Arc::clone(&stop);
            let level = self.level;
            std::thread::Builder::new()
                .name("sqz-fleet-r5".to_string())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        if level() == Level::Red {
                            red_hit.store(true, Ordering::Release);
                            cancel.store(true, Ordering::SeqCst);
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                })
                .map_err(std::io::Error::other)?
        };
        let report = sqz_blocking::block_on(fsck::run(&ctx, &opts));
        stop.store(true, Ordering::Release);
        let _ = watcher.join();
        if red_hit.load(Ordering::Acquire) {
            METRICS
                .job_fleet_worker_red_aborts
                .fetch_add(1, Ordering::Relaxed);
            return Err(std::io::Error::other(
                "R5 memory budget went Red mid-shard on this member — cancelled and \
                 abandoned (never a partial proposal)",
            ));
        }
        let report =
            report.map_err(|e| std::io::Error::other(format!("member shard fsck failed: {e}")))?;
        serde_json::to_vec(&report).map_err(std::io::Error::other)
    }
}

/// The armed mount-side worker: one named OS thread that discovers the
/// coordinator, enrolls, serves shards, and re-dials on disconnect —
/// until [`FleetWorkerArm::disarm`] (unmount) stops it. Structured
/// teardown: the stop latch plus a socket nudge wake the serve loop out
/// of its parked frame wait, and the thread is JOINED (no leaks).
pub struct FleetWorkerArm {
    stop: Arc<AtomicBool>,
    nudge: Arc<parking_lot::Mutex<Option<std::net::TcpStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FleetWorkerArm {
    /// Stop and join the worker (unmount teardown).
    pub fn disarm(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(n) = self.nudge.lock().take() {
            let _ = n.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for FleetWorkerArm {
    fn drop(&mut self) {
        self.disarm();
    }
}

/// Spawn the fleet worker for a member mount (design §2). Reconnects
/// ride the wire's own heartbeat grain (`HEARTBEAT_INTERVAL / 2` between
/// attempts — the discovery cadence is a property of the wire, never a
/// free-floating constant). Quiet when no coordinator publishes an
/// endpoint yet (a reader can mount before any writer).
pub fn spawn_fleet_worker(
    meta: Arc<RoutedMetaBackend>,
    router: crate::routing::DataRouter,
    worker_id: String,
) -> std::io::Result<FleetWorkerArm> {
    let stop = Arc::new(AtomicBool::new(false));
    let nudge = Arc::new(parking_lot::Mutex::new(None::<std::net::TcpStream>));
    let seam: Arc<dyn ShardDeviceSeam> = Arc::new(MountFleetSeam::new(meta.clone(), router));
    let t_stop = Arc::clone(&stop);
    let t_nudge = Arc::clone(&nudge);
    let thread = std::thread::Builder::new()
        .name("sqz-fleet-wrk".to_string())
        .spawn(move || {
            let backoff = HEARTBEAT_INTERVAL / 2;
            let mut announced_waiting = false;
            loop {
                if t_stop.load(Ordering::SeqCst) {
                    return;
                }
                let dial = sqz_blocking::block_on(async {
                    let endpoint = job_wire::discover_endpoint(&meta).await?;
                    let secret = job_wire::read_enroll_secret(&meta).await.ok()?;
                    let mut opts = WorkerOptions::new(&worker_id);
                    opts.caps = CAP_FLEET_READ;
                    Some((
                        endpoint.clone(),
                        JobWireWorker::connect(&endpoint, &secret, opts).await,
                    ))
                });
                match dial {
                    Some((endpoint, Ok(worker))) => {
                        announced_waiting = false;
                        *t_nudge.lock() = worker.teardown_nudge();
                        log::info!(
                            "fleet worker: member '{worker_id}' enrolled at coordinator \
                             {endpoint} (KD-MW-16 fleet read shards)"
                        );
                        match sqz_blocking::block_on(worker.run(Arc::clone(&seam))) {
                            Ok(report) => log::info!(
                                "fleet worker: coordinator connection closed — shards \
                                 completed {}, refused {}, abandoned {}",
                                report.shards_completed,
                                report.submissions_refused,
                                report.shards_aborted
                            ),
                            Err(e) => log::warn!("fleet worker: serve loop ended: {e}"),
                        }
                        *t_nudge.lock() = None;
                    }
                    Some((endpoint, Err(e))) => {
                        log::warn!(
                            "fleet worker: enrollment at {endpoint} failed ({e}) — retrying \
                             on the wire's heartbeat grain"
                        );
                    }
                    None => {
                        if !announced_waiting {
                            log::info!(
                                "fleet worker: no coordinator endpoint published yet — \
                                 member '{worker_id}' will enroll when one appears"
                            );
                            announced_waiting = true;
                        }
                    }
                }
                // Backoff in bounded slices so disarm is prompt.
                let mut waited = Duration::ZERO;
                while waited < backoff {
                    if t_stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let slice = (backoff - waited).min(Duration::from_millis(100));
                    std::thread::sleep(slice);
                    waited += slice;
                }
            }
        })
        .map_err(std::io::Error::other)?;
    Ok(FleetWorkerArm {
        stop,
        nudge,
        thread: Some(thread),
    })
}
