//! The maintenance-job worker skeleton (percentage duty-cycle throttle
//! + pause-aware in-process queue).
//!
//! VL1 (design-volume-lifecycle §5.0) deleted the dead `BlockMove`
//! task type and its never-called `submit_and_wait_for_job` front end
//! (git history keeps the shape). What survives is exactly what PR VL2
//! extends into the durable distributed job fabric: the throttle law
//! (`job_throttle_sleep`) and the mount-spawned worker loop with its
//! pause/notify queue discipline. Until VL2 lands, [`TaskType`] is
//! uninhabited — no producer exists, and the worker parks.

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// Maintenance task kinds. Uninhabited in VL1 (the fake `BlockMove`
/// was deleted); VL2's fabric populates it (evacuate / rebalance /
/// fsck / defrag shards — design-volume-lifecycle §5.1).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum TaskType {}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobTask {
    pub task_id: String,
    pub job_id: String,
    pub task_type: TaskType,
}

struct InMemoryJobs {
    active_jobs: HashSet<String>,
    pending_tasks: HashMap<String, Vec<JobTask>>,
    paused_jobs: HashSet<String>,
    job_notifiers: HashMap<String, Arc<tokio::sync::Notify>>,
}

static IN_MEMORY_JOBS: Lazy<Mutex<InMemoryJobs>> = Lazy::new(|| {
    Mutex::new(InMemoryJobs {
        active_jobs: HashSet::new(),
        pending_tasks: HashMap::new(),
        paused_jobs: HashSet::new(),
        job_notifiers: HashMap::new(),
    })
});

static WORKER_NOTIFY: Lazy<tokio::sync::Notify> = Lazy::new(tokio::sync::Notify::new);

/// The mount-spawned maintenance worker (`--job-cpu-limit` wires
/// `cpu_limit_pct`). Drains the pause-aware queue under the duty-cycle
/// throttle; parks when idle.
pub fn start_job_worker(cpu_limit_pct: u32) {
    tokio::spawn(async move {
        loop {
            let task_to_run = {
                let mut state = IN_MEMORY_JOBS.lock();
                let mut chosen_job = None;
                for (job_id, tasks) in state.pending_tasks.iter() {
                    if !state.paused_jobs.contains(job_id) && !tasks.is_empty() {
                        chosen_job = Some(job_id.clone());
                        break;
                    }
                }
                if let Some(job_id) = chosen_job {
                    let tasks = state.pending_tasks.get_mut(&job_id).unwrap();
                    let _task = tasks.remove(0);
                    let is_empty = tasks.is_empty();
                    if is_empty {
                        state.pending_tasks.remove(&job_id);
                        state.active_jobs.remove(&job_id);
                        if let Some(n) = state.job_notifiers.remove(&job_id) {
                            n.notify_waiters();
                        }
                    }
                    Some(_task)
                } else {
                    None
                }
            };

            if let Some(task_wrapper) = task_to_run {
                let start = std::time::Instant::now();
                run_task(task_wrapper);
                if let Some(delay) = job_throttle_sleep(start.elapsed(), cpu_limit_pct) {
                    tokio::time::sleep(delay).await;
                }
            } else {
                WORKER_NOTIFY.notified().await;
            }
        }
    });
}

/// Execute one task. [`TaskType`] is uninhabited until VL2, so this is
/// statically unreachable-by-construction (no task can be built) — the
/// seam where VL2's shard execution lands.
fn run_task(task: JobTask) {
    match task.task_type {}
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
