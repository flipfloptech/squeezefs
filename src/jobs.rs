//! The **durable job fabric** (PR VL2, design-volume-lifecycle §5.1).
//!
//! Coordinator-side core: durable schema-versioned job records on
//! ino 1 (`job:` xattrs — v3 whole-tx atomicity + torn-write immunity
//! for free, offline-probe visible, behind the FUSE reserved-namespace
//! screen), a local worker pool with the percentage **duty-cycle
//! throttle** (KD-3, live-retunable), pause/resume/cancel control, a
//! bounded checkpoint cadence, and **crash-resume by plan
//! regeneration** (KD-6: a restarted fabric re-scans the records and
//! re-plans; the progress record is advisory, never correctness-
//! bearing).
//!
//! v1.1 job types land incrementally: VL2 ships the fabric itself with
//! [`JobType::Noop`] (the fabric's own test/soak vehicle); VL4+ add
//! the movers (evacuate/rebalance), VL6 fsck shards, VL7 defrag. The
//! §5.1.6 remote-worker wire is PR VL2b.

use crate::meta_backend::{Metadata, RoutedMetaBackend};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;

/// Reserved xattr prefix for fabric records on ino 1 (§5.1.2; the FUSE
/// layer screens it — `src/fuse_client.rs` `reserved_xattr_name`).
pub const JOB_XATTR_PREFIX: &str = "job:";

/// Root ino: job records live on the volume root (the same home as the
/// format-config record).
const ROOT_INO: u64 = 1;

/// Checkpoint cadence (§5.1.2): every N completed tasks or every 5 s,
/// whichever first — bounded re-work on crash.
const CHECKPOINT_TASKS: u64 = 256;
const CHECKPOINT_SECS: u64 = 5;

/// Job kinds. `Noop` is the fabric's own test/soak vehicle (sanctioned
/// by the design's G-VL-7 instrument: "crash-resume with a no-op test
/// task type") — each task sleeps `task_ms`, making duty cycle and
/// progress directly measurable with zero I/O.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    Noop { tasks: u64, task_ms: u64 },
}

impl JobType {
    fn tasks_total(&self) -> u64 {
        match self {
            JobType::Noop { tasks, .. } => *tasks,
        }
    }
}

/// Job lifecycle states. Terminal = `Completed`/`Cancelled`/`Failed`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Cancelled,
    Completed,
    Failed,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Cancelled | JobState::Completed | JobState::Failed
        )
    }
}

/// The durable `job:{id}` record (schema v1). One JSON value, well
/// under the xattr cap; large plans shard into `job:{id}:shard:{k}`
/// records (the movers' shape — VL4+).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobRecord {
    pub schema: u32,
    pub job_id: String,
    pub job_type: JobType,
    pub state: JobState,
    pub throttle_pct: u32,
    pub created_by: String,
    pub created_ts: u64,
    /// Advisory progress (checkpointed; correctness is plan
    /// regeneration, KD-6).
    pub tasks_done: u64,
    pub tasks_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Submission parameters.
#[derive(Clone, Debug)]
pub struct JobSpec {
    pub job_type: JobType,
    /// Duty-cycle percentage (KD-3); `0`/`≥100` = unthrottled.
    pub throttle_pct: u32,
}

/// A point-in-time status view (live map when running, durable record
/// otherwise).
#[derive(Clone, Debug)]
pub struct JobStatus {
    pub job_id: String,
    pub state: JobState,
    pub tasks_done: u64,
    pub tasks_total: u64,
    pub throttle_pct: u32,
}

/// Live per-job control block: the workers' and control verbs' shared
/// truth between checkpoints. `pub(crate)` since PR VL2b: the §5.1.6
/// wire's coordinator is the second population driving the same claims
/// and completions (KD-1: one protocol, two transports).
pub(crate) struct JobCtl {
    pub(crate) job_type: JobType,
    paused: AtomicBool,
    cancelled: AtomicBool,
    /// Live-retunable duty cycle (workers re-read per task).
    pub(crate) throttle: AtomicU32,
    /// Tasks completed (live; checkpointed on cadence).
    pub(crate) done: AtomicU64,
    pub(crate) tasks_total: u64,
    /// One worker owns a job at a time in VL2 (shard-level parallelism
    /// arrives with the real movers' multi-shard plans).
    claimed: AtomicBool,
    state: parking_lot::Mutex<JobState>,
    terminal: Notify,
}

impl JobCtl {
    pub(crate) fn state(&self) -> JobState {
        *self.state.lock()
    }
    fn set_state(&self, s: JobState) {
        *self.state.lock() = s;
        if s.is_terminal() {
            self.terminal.notify_waiters();
        }
    }
}

/// The coordinator-side fabric. One per mounted daemon (or per offline
/// D0-guarded coordinator process).
pub struct JobFabric {
    meta: Arc<RoutedMetaBackend>,
    jobs: parking_lot::Mutex<HashMap<String, Arc<JobCtl>>>,
    default_throttle: u32,
    work: Notify,
    shutdown: AtomicBool,
    handles: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl JobFabric {
    /// Start the fabric: adopt durable non-terminal records (crash-
    /// resume by plan regeneration), then spawn `workers` local pool
    /// tasks. `default_throttle` backs specs submitted with `0`… no —
    /// spec percentages pass through verbatim; this is the mount's
    /// `--job-cpu-limit` default applied when a job is submitted
    /// without an explicit throttle by higher layers.
    pub async fn start(
        meta: Arc<RoutedMetaBackend>,
        workers: usize,
        default_throttle: u32,
    ) -> crate::error::Result<Arc<Self>> {
        let fabric = Arc::new(Self {
            meta,
            jobs: parking_lot::Mutex::new(HashMap::new()),
            default_throttle,
            work: Notify::new(),
            shutdown: AtomicBool::new(false),
            handles: parking_lot::Mutex::new(Vec::new()),
        });

        // Crash-resume (KD-6): every durable non-terminal record is
        // adopted. Running/Queued re-queue (the plan regenerates from
        // the record); Paused stays paused (operator intent survives
        // the crash).
        for rec in Self::list_records(&fabric.meta).await? {
            if rec.state.is_terminal() {
                continue;
            }
            log::info!(
                "job fabric: adopting durable job {} (state {:?}, {}/{} tasks)",
                rec.job_id,
                rec.state,
                rec.tasks_done,
                rec.tasks_total
            );
            let adopted_state = match rec.state {
                JobState::Paused => JobState::Paused,
                _ => JobState::Queued,
            };
            let ctl = Arc::new(JobCtl {
                job_type: rec.job_type.clone(),
                paused: AtomicBool::new(adopted_state == JobState::Paused),
                cancelled: AtomicBool::new(false),
                throttle: AtomicU32::new(rec.throttle_pct),
                // Plan regeneration, not trust: Noop's "current state"
                // is the checkpointed cursor (advisory), and the plan
                // is completed idempotently from there. Real movers
                // re-census instead (VL4).
                done: AtomicU64::new(rec.tasks_done),
                tasks_total: rec.tasks_total,
                claimed: AtomicBool::new(false),
                state: parking_lot::Mutex::new(adopted_state),
                terminal: Notify::new(),
            });
            fabric.jobs.lock().insert(rec.job_id.clone(), ctl);
        }

        // `workers == 0` is the remote-only posture (no local pool —
        // every shard rides the §5.1.6 wire; test/soak shape). Mounts
        // always pass the clamped L4-style pool size.
        for idx in 0..workers {
            let f = Arc::clone(&fabric);
            let h = tokio::spawn(async move { f.worker_loop(idx).await });
            fabric.handles.lock().push(h);
        }
        fabric.work.notify_waiters();
        Ok(fabric)
    }

    /// Submit a job: durable record first (whole-tx), then enqueue.
    pub async fn submit(&self, spec: JobSpec) -> crate::error::Result<String> {
        let job_id = uuid::Uuid::new_v4().to_string();
        let throttle = if spec.throttle_pct == 0 {
            self.default_throttle
        } else {
            spec.throttle_pct
        };
        let rec = JobRecord {
            schema: 1,
            job_id: job_id.clone(),
            job_type: spec.job_type.clone(),
            state: JobState::Queued,
            throttle_pct: throttle,
            created_by: format!("{}:{}", hostname_lossy(), std::process::id()),
            created_ts: unix_ts(),
            tasks_done: 0,
            tasks_total: spec.job_type.tasks_total(),
            error: None,
        };
        self.persist(&rec).await?;
        let ctl = Arc::new(JobCtl {
            job_type: rec.job_type.clone(),
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            throttle: AtomicU32::new(throttle),
            done: AtomicU64::new(0),
            tasks_total: rec.tasks_total,
            claimed: AtomicBool::new(false),
            state: parking_lot::Mutex::new(JobState::Queued),
            terminal: Notify::new(),
        });
        self.jobs.lock().insert(job_id.clone(), ctl);
        crate::fuse_client::METRICS
            .job_submitted
            .fetch_add(1, Ordering::Relaxed);
        self.work.notify_waiters();
        Ok(job_id)
    }

    /// Point-in-time status: live map first, durable record otherwise
    /// (the offline-probe shape).
    pub async fn status(&self, job_id: &str) -> crate::error::Result<Option<JobStatus>> {
        if let Some(ctl) = self.jobs.lock().get(job_id).cloned() {
            return Ok(Some(JobStatus {
                job_id: job_id.to_string(),
                state: ctl.state(),
                tasks_done: ctl.done.load(Ordering::Relaxed),
                tasks_total: ctl.tasks_total,
                throttle_pct: ctl.throttle.load(Ordering::Relaxed),
            }));
        }
        Ok(Self::read_record(&self.meta, job_id)
            .await?
            .map(|r| JobStatus {
                job_id: r.job_id,
                state: r.state,
                tasks_done: r.tasks_done,
                tasks_total: r.tasks_total,
                throttle_pct: r.throttle_pct,
            }))
    }

    /// Pause: workers stop pulling tasks after the in-flight one; the
    /// state change is durable.
    pub async fn pause(&self, job_id: &str) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.paused.store(true, Ordering::SeqCst);
        if !ctl.state().is_terminal() {
            ctl.set_state(JobState::Paused);
        }
        self.checkpoint(job_id, &ctl).await
    }

    /// Resume a paused job.
    pub async fn resume(&self, job_id: &str) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.paused.store(false, Ordering::SeqCst);
        if ctl.state() == JobState::Paused {
            ctl.set_state(JobState::Queued);
        }
        self.checkpoint(job_id, &ctl).await?;
        self.work.notify_waiters();
        Ok(())
    }

    /// Cancel: terminal, durable.
    pub async fn cancel(&self, job_id: &str) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.cancelled.store(true, Ordering::SeqCst);
        ctl.paused.store(false, Ordering::SeqCst); // unpark a paused job so it terminates
        if !ctl.state().is_terminal() {
            ctl.set_state(JobState::Cancelled);
        }
        crate::fuse_client::METRICS
            .job_cancelled
            .fetch_add(1, Ordering::Relaxed);
        self.checkpoint(job_id, &ctl).await?;
        self.work.notify_waiters();
        Ok(())
    }

    /// Live rethrottle (KD-3): workers re-read per task; durable.
    pub async fn throttle(&self, job_id: &str, pct: u32) -> crate::error::Result<()> {
        let ctl = self.require(job_id)?;
        ctl.throttle.store(pct, Ordering::SeqCst);
        self.checkpoint(job_id, &ctl).await
    }

    /// Await a terminal state (test/CLI convenience; polls the live
    /// notify with a deadline).
    pub async fn wait_terminal(
        &self,
        job_id: &str,
        timeout: Duration,
    ) -> crate::error::Result<JobState> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let Some(st) = self.status(job_id).await? else {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "unknown job {job_id}"
                )));
            };
            if st.state.is_terminal() {
                return Ok(st.state);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "job {job_id} not terminal within {timeout:?}"
                )));
            }
            // Bounded poll slice; the terminal notify shortens the tail.
            let notified = async {
                let ctl = self.jobs.lock().get(job_id).cloned();
                match ctl {
                    Some(c) => c.terminal.notified().await,
                    None => std::future::pending().await,
                }
            };
            let _ = tokio::time::timeout(Duration::from_millis(50), notified).await;
        }
    }

    /// "Crash" shutdown for soak/tests: abort workers mid-task, persist
    /// NOTHING — durable records stay non-terminal exactly as a kill-9
    /// leaves them.
    pub async fn shutdown_abrupt(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let handles: Vec<_> = std::mem::take(&mut *self.handles.lock());
        for h in handles {
            h.abort();
            let _ = h.await;
        }
    }

    /// Offline-probe-shaped list: decode every top-level `job:{id}`
    /// record on ino 1 (shard/progress sub-records are skipped by the
    /// single-segment filter).
    pub async fn list_records(
        meta: &Arc<RoutedMetaBackend>,
    ) -> crate::error::Result<Vec<JobRecord>> {
        let mut out = Vec::new();
        for name in meta.listxattr(ROOT_INO).await? {
            let Some(rest) = name.strip_prefix(JOB_XATTR_PREFIX) else {
                continue;
            };
            if rest.contains(':') {
                continue; // shard/progress sub-record
            }
            if let Some(bytes) = meta.getxattr(ROOT_INO, &name).await? {
                match serde_json::from_slice::<JobRecord>(&bytes) {
                    Ok(rec) if rec.schema == 1 => out.push(rec),
                    Ok(rec) => log::warn!(
                        "job fabric: skipping job {} with unknown schema {}",
                        rec.job_id,
                        rec.schema
                    ),
                    Err(e) => log::warn!("job fabric: undecodable record {name}: {e}"),
                }
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------

    /// The fabric's meta handle (admin sink / offline probes reuse it).
    pub fn meta_handle(&self) -> &Arc<RoutedMetaBackend> {
        &self.meta
    }

    /// R5 shed hook for the `job_copy_buffers` component (§5.1.5): under
    /// memory pressure, pause every running job — loud, durable via the
    /// workers' pause checkpoints, and deliberately NOT self-resuming
    /// (`job resume` is the operator's call once pressure clears). The
    /// gauge is worker copy-buffer bytes (0 until the VL4 movers charge
    /// it), so this fires only when real buffers exist.
    pub fn shed_to(&self, target: u64) {
        let gauge = crate::fuse_client::METRICS
            .job_copy_buffer_bytes
            .load(Ordering::Relaxed);
        if gauge <= target {
            return;
        }
        for (id, ctl) in self.jobs.lock().iter() {
            if ctl.state() == JobState::Running {
                ctl.paused.store(true, Ordering::SeqCst);
                crate::fuse_client::METRICS
                    .job_paused_mem_pressure
                    .fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "job fabric: paused job {id} under memory pressure \
                     (job_copy_buffers {gauge} > target {target})"
                );
            }
        }
    }

    fn require(&self, job_id: &str) -> crate::error::Result<Arc<JobCtl>> {
        self.jobs.lock().get(job_id).cloned().ok_or_else(|| {
            crate::error::SqueezefsError::InvalidOperation(format!("unknown job {job_id}"))
        })
    }

    async fn read_record(
        meta: &Arc<RoutedMetaBackend>,
        job_id: &str,
    ) -> crate::error::Result<Option<JobRecord>> {
        let name = format!("{JOB_XATTR_PREFIX}{job_id}");
        match meta.getxattr(ROOT_INO, &name).await? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            None => Ok(None),
        }
    }

    async fn persist(&self, rec: &JobRecord) -> crate::error::Result<()> {
        let name = format!("{JOB_XATTR_PREFIX}{}", rec.job_id);
        let bytes = serde_json::to_vec(rec).map_err(|e| {
            crate::error::SqueezefsError::InvalidOperation(format!("job record encode: {e}"))
        })?;
        self.meta.setxattr(ROOT_INO, &name, &bytes).await?;
        crate::fuse_client::METRICS
            .job_checkpoint_writes
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Write the current live state into the durable record.
    async fn checkpoint(&self, job_id: &str, ctl: &JobCtl) -> crate::error::Result<()> {
        self.checkpoint_as(job_id, ctl, ctl.state()).await
    }

    /// Checkpoint with an explicit state — the terminal path persists
    /// BEFORE the live state flips (durable-then-visible: a waiter woken
    /// by the terminal notify must find the terminal record on disk).
    async fn checkpoint_as(
        &self,
        job_id: &str,
        ctl: &JobCtl,
        state: JobState,
    ) -> crate::error::Result<()> {
        let Some(mut rec) = Self::read_record(&self.meta, job_id).await? else {
            return Ok(()); // record vanished (foreign cleanup) — advisory only
        };
        rec.state = state;
        rec.tasks_done = ctl.done.load(Ordering::Relaxed);
        rec.throttle_pct = ctl.throttle.load(Ordering::Relaxed);
        self.persist(&rec).await
    }

    /// Claim the next runnable job (Queued, unclaimed). `pub(crate)`:
    /// the §5.1.6 wire dispatcher claims through the same gate as the
    /// local pool — one claim law for both populations (KD-1).
    pub(crate) fn claim_next(&self) -> Option<(String, Arc<JobCtl>)> {
        let jobs = self.jobs.lock();
        for (id, ctl) in jobs.iter() {
            if ctl.state() == JobState::Queued
                && !ctl.paused.load(Ordering::SeqCst)
                && ctl
                    .claimed
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return Some((id.clone(), Arc::clone(ctl)));
            }
        }
        None
    }

    /// Mark a remotely-executed job Running (durable-then-visible, the
    /// same order the local pool uses) — called by the wire dispatcher
    /// right after a shard is assigned.
    pub(crate) async fn remote_running(&self, job_id: &str, ctl: &Arc<JobCtl>) {
        ctl.set_state(JobState::Running);
        let _ = self.checkpoint(job_id, ctl).await;
    }

    /// A verified remote submission completes the job: tasks are
    /// accounted, the durable record flips terminal BEFORE the live
    /// state (the run_job durable-then-visible law), and the terminal
    /// notify fires. A job that went terminal meanwhile (cancel) is
    /// left alone — the submission's effects were already refused or
    /// are contractually moot for Noop shards.
    pub(crate) async fn remote_complete(&self, job_id: &str, ctl: &Arc<JobCtl>) {
        if ctl.state().is_terminal() {
            return;
        }
        let executed = ctl
            .tasks_total
            .saturating_sub(ctl.done.load(Ordering::Relaxed));
        ctl.done.store(ctl.tasks_total, Ordering::Relaxed);
        crate::fuse_client::METRICS
            .job_tasks_done
            .fetch_add(executed, Ordering::Relaxed);
        let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
        crate::fuse_client::METRICS
            .job_completed
            .fetch_add(1, Ordering::Relaxed);
        ctl.set_state(JobState::Completed);
    }

    /// Return an expired remote shard's job to the queue (lease-expiry
    /// reassignment, §5.1.6): any population — local pool or another
    /// remote worker — may claim it again; the wire's bumped
    /// shard_fencing is what keeps the old holder's late submission out.
    pub(crate) async fn requeue_remote(&self, job_id: &str, ctl: &Arc<JobCtl>) {
        if !ctl.state().is_terminal() {
            ctl.set_state(JobState::Queued);
            let _ = self.checkpoint(job_id, ctl).await;
        }
        ctl.claimed.store(false, Ordering::SeqCst);
        self.work.notify_waiters();
    }

    /// One pending-work wake for the wire dispatcher (the same notify
    /// the local pool parks on).
    pub(crate) fn work_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.work.notified()
    }

    async fn worker_loop(self: Arc<Self>, idx: usize) {
        log::debug!("job fabric: worker {idx} up");
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let Some((job_id, ctl)) = self.claim_next() else {
                // Park until submitted/resumed work (bounded so a lost
                // notify cannot strand the pool).
                let _ =
                    tokio::time::timeout(Duration::from_millis(500), self.work.notified()).await;
                continue;
            };
            ctl.set_state(JobState::Running);
            let _ = self.checkpoint(&job_id, &ctl).await;
            self.run_job(&job_id, &ctl).await;
            ctl.claimed.store(false, Ordering::SeqCst);
        }
    }

    /// Execute one job until terminal/paused. Duty-cycle throttle after
    /// every task (KD-3, live-re-read); checkpoint on the §5.1.2
    /// cadence.
    async fn run_job(&self, job_id: &str, ctl: &JobCtl) {
        let mut since_checkpoint = 0u64;
        let mut last_checkpoint = tokio::time::Instant::now();
        loop {
            if ctl.cancelled.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Cancelled).await;
                ctl.set_state(JobState::Cancelled);
                return;
            }
            if ctl.paused.load(Ordering::SeqCst) {
                let _ = self.checkpoint_as(job_id, ctl, JobState::Paused).await;
                ctl.set_state(JobState::Paused);
                return;
            }
            let done = ctl.done.load(Ordering::Relaxed);
            if done >= ctl.tasks_total {
                // Durable-then-visible: the record flips terminal on
                // disk before any waiter can observe it live.
                let _ = self.checkpoint_as(job_id, ctl, JobState::Completed).await;
                crate::fuse_client::METRICS
                    .job_completed
                    .fetch_add(1, Ordering::Relaxed);
                ctl.set_state(JobState::Completed);
                return;
            }

            let start = tokio::time::Instant::now();
            match &ctl.job_type {
                JobType::Noop { task_ms, .. } => {
                    tokio::time::sleep(Duration::from_millis(*task_ms)).await;
                }
            }
            ctl.done.fetch_add(1, Ordering::Relaxed);
            crate::fuse_client::METRICS
                .job_tasks_done
                .fetch_add(1, Ordering::Relaxed);
            since_checkpoint += 1;

            if since_checkpoint >= CHECKPOINT_TASKS
                || last_checkpoint.elapsed() >= Duration::from_secs(CHECKPOINT_SECS)
            {
                let _ = self.checkpoint(job_id, ctl).await;
                since_checkpoint = 0;
                last_checkpoint = tokio::time::Instant::now();
            }

            // KD-3: duty-cycle throttle, live re-read per task.
            let pct = ctl.throttle.load(Ordering::Relaxed);
            if let Some(delay) = job_throttle_sleep(start.elapsed(), pct) {
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn hostname_lossy() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// The percentage duty-cycle throttle law (KD-3): after a task that ran
/// for `elapsed`, sleep `elapsed × (100 − pct) / pct` so task-active
/// time ≈ `pct` of wall time. `0` and `≥ 100` mean unthrottled.
pub fn job_throttle_sleep(elapsed: Duration, cpu_limit_pct: u32) -> Option<Duration> {
    if cpu_limit_pct >= 100 || cpu_limit_pct == 0 {
        None
    } else {
        let factor = (100 - cpu_limit_pct) as f64 / cpu_limit_pct as f64;
        let sleep_dur = elapsed.mul_f64(factor);
        if sleep_dur < Duration::from_millis(1) {
            None
        } else {
            Some(sleep_dur)
        }
    }
}
