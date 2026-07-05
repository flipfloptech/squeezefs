use crate::error::{Result, SqueezefsError};
use crate::meta_backend::Metadata;
use crate::routing::DataRouter;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum TaskType {
    BlockMove {
        ino: u64,
        map_id: String,
        idx_str: String,
        src_offset: u64,
        dest_offset: u64,
        len: usize,
    },
}

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

static WORKER_NOTIFY: Lazy<tokio::sync::Notify> = Lazy::new(|| tokio::sync::Notify::new());

pub async fn submit_and_wait_for_job(
    _redis_url: &str,
    _fs_name: &str,
    tasks: Vec<TaskType>,
) -> Result<()> {
    let job_id = uuid::Uuid::new_v4().to_string();
    let mut job_tasks = Vec::new();
    for (idx, task) in tasks.into_iter().enumerate() {
        job_tasks.push(JobTask {
            task_id: format!("{}_{}", job_id, idx),
            job_id: job_id.clone(),
            task_type: task,
        });
    }

    let notify = Arc::new(tokio::sync::Notify::new());
    {
        let mut state = IN_MEMORY_JOBS.lock();
        state.active_jobs.insert(job_id.clone());
        state.pending_tasks.insert(job_id.clone(), job_tasks);
        state.job_notifiers.insert(job_id.clone(), notify.clone());
    }
    WORKER_NOTIFY.notify_waiters();

    loop {
        let n = {
            let state = IN_MEMORY_JOBS.lock();
            if state.paused_jobs.contains(&job_id) {
                return Err(SqueezefsError::InvalidOperation("Job paused".to_string()));
            }
            if !state.pending_tasks.contains_key(&job_id) {
                break;
            }
            state.job_notifiers.get(&job_id).cloned()
        };
        if let Some(notifier) = n {
            notifier.notified().await;
        } else {
            break;
        }
    }
    Ok(())
}

pub fn start_job_worker(router: Arc<DataRouter>, _fs_name: String, cpu_limit_pct: u32) {
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
                    let task = tasks.remove(0);
                    let is_empty = tasks.is_empty();
                    let task_wrapper = task.clone();
                    if is_empty {
                        state.pending_tasks.remove(&job_id);
                        state.active_jobs.remove(&job_id);
                        if let Some(n) = state.job_notifiers.remove(&job_id) {
                            n.notify_waiters();
                        }
                    }
                    Some(task_wrapper)
                } else {
                    None
                }
            };

            if let Some(task_wrapper) = task_to_run {
                let start = std::time::Instant::now();
                match task_wrapper.task_type {
                    TaskType::BlockMove {
                        ino: _,
                        map_id,
                        idx_str,
                        src_offset,
                        dest_offset,
                        len,
                    } => {
                        if let Ok(data) = router.nvme_writer.read_block(src_offset, len).await {
                            match router.nvme_writer.write_block(dest_offset, data).await {
                                Ok(_) => {
                                    if let Some(backend) = router.meta_backend.get() {
                                        if let Ok(Some(bytes)) = backend
                                            .getxattr(map_id.parse().unwrap_or(0), "layout")
                                            .await
                                        {
                                            if let Ok(mut layout) = serde_json::from_slice::<
                                                crate::routing::LayoutMetadata,
                                            >(
                                                bytes.as_slice()
                                            ) {
                                                if let Some(ref mut bm) = layout.block_map {
                                                    let idx: u32 = idx_str.parse().unwrap_or(0);
                                                    bm.insert(idx, dest_offset.to_string());
                                                    if let Ok(updated_bytes) =
                                                        serde_json::to_vec(&layout)
                                                    {
                                                        let _ = backend
                                                            .setxattr(
                                                                map_id.parse().unwrap_or(0),
                                                                "layout",
                                                                &updated_bytes,
                                                            )
                                                            .await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::error!(
                                        "Job worker: BlockMove failed: {:?}. Pausing job.",
                                        e
                                    );
                                    let mut state = IN_MEMORY_JOBS.lock();
                                    state.paused_jobs.insert(task_wrapper.job_id.clone());
                                    if let Some(n) = state.job_notifiers.get(&task_wrapper.job_id) {
                                        n.notify_waiters();
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(delay) = job_throttle_sleep(start.elapsed(), cpu_limit_pct) {
                    tokio::time::sleep(delay).await;
                }
            } else {
                WORKER_NOTIFY.notified().await;
            }
        }
    });
}

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
