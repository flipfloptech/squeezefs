/*
 * Squeezefs, Copyright 2026 Juicedata, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::error::{Result, SqueezefsError};
use crate::routing::DataRouter;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum TaskType {
    BlockMove {
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

/// Submit a list of tasks as a distributed job and block/poll until they are all completed.
pub async fn submit_and_wait_for_job(
    redis_url: &str,
    fs_name: &str,
    tasks: Vec<TaskType>,
) -> Result<()> {
    let dlm = crate::dlm::DlmClient::new(redis_url)?;
    let mut con = dlm.get_connection().await?;

    let job_id = uuid::Uuid::new_v4().to_string();
    let pending_key = format!("{}:jobs:{}:pending", fs_name, job_id);
    let completed_key = format!("{}:jobs:{}:completed", fs_name, job_id);
    let active_set_key = format!("{}:active_jobs", fs_name);

    let total_tasks = tasks.len();
    if total_tasks == 0 {
        return Ok(());
    }

    println!("Registering job {} with {} tasks...", job_id, total_tasks);

    // Push tasks to pending set
    let mut pipe = redis::pipe();
    for (i, t_type) in tasks.into_iter().enumerate() {
        let task = JobTask {
            task_id: format!("{}_{}", job_id, i),
            job_id: job_id.clone(),
            task_type: t_type,
        };
        let task_json = serde_json::to_string(&task)
            .map_err(|e| SqueezefsError::InvalidOperation(e.to_string()))?;
        pipe.sadd(&pending_key, task_json);
    }
    pipe.sadd(&active_set_key, &job_id);
    let _: () = pipe.query_async(&mut con).await?;

    // Progress bar or status polling loop
    let pb = indicatif::ProgressBar::new(total_tasks as u64);
    pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} tasks completed ({eta}) {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    let paused_key = format!("{}:jobs:{}:paused", fs_name, job_id);

    loop {
        // Check if the job has been paused globally
        let paused_str: Option<String> = con.get(&paused_key).await.unwrap_or(None);
        if paused_str.as_deref() == Some("1") {
            pb.set_message(" - PAUSED (Write Verification Failure)");
            sleep(Duration::from_millis(1000)).await;
            continue;
        } else {
            pb.set_message("");
        }

        // Query pending and completed counts
        let pending_count: u64 = con.scard(&pending_key).await.unwrap_or(0);
        let completed_count: u64 = con.scard(&completed_key).await.unwrap_or(0);

        pb.set_position(completed_count);

        if pending_count == 0 && completed_count >= total_tasks as u64 {
            break;
        }

        // Check if any clients are executing or if there is progress
        sleep(Duration::from_millis(500)).await;
    }

    pb.finish_with_message("Distributed job completed successfully");

    // Cleanup keys
    let mut cleanup_pipe = redis::pipe();
    cleanup_pipe.srem(&active_set_key, &job_id);
    cleanup_pipe.del(&pending_key);
    cleanup_pipe.del(&completed_key);
    cleanup_pipe.del(&paused_key);
    let _: () = cleanup_pipe.query_async(&mut con).await?;

    Ok(())
}

/// Spawns a background loop that monitors active jobs and executes pending tasks.
pub fn start_job_worker(
    router: Arc<DataRouter>,
    fs_name: String,
    cpu_limit_pct: u32, // CPU limit percentage (1 to 100)
) {
    tokio::spawn(async move {
        let cpu_limit = cpu_limit_pct.clamp(1, 100);

        loop {
            if let Err(e) = run_worker_cycle(&router, &fs_name, cpu_limit).await {
                log::debug!("Job worker cycle error: {:?}", e);
            }
            sleep(Duration::from_millis(500)).await;
        }
    });
}

async fn run_worker_cycle(router: &DataRouter, fs_name: &str, cpu_limit: u32) -> Result<()> {
    let mut con = router.dlm.get_connection().await?;
    let active_set_key = format!("{}:active_jobs", fs_name);

    // Get list of active jobs
    let active_jobs: Vec<String> = con.smembers(&active_set_key).await?;
    for job_id in active_jobs {
        let pending_key = format!("{}:jobs:{}:pending", fs_name, job_id);
        let completed_key = format!("{}:jobs:{}:completed", fs_name, job_id);
        let paused_key = format!("{}:jobs:{}:paused", fs_name, job_id);

        // Check if job is paused
        let paused_str: Option<String> = con.get(&paused_key).await?;
        if paused_str.as_deref() == Some("1") {
            continue;
        }

        // Atomic SPOP to grab a task
        let task_json_opt: Option<String> = con.spop(&pending_key).await?;
        if let Some(task_json) = task_json_opt {
            let task: JobTask = match serde_json::from_str(&task_json) {
                Ok(t) => t,
                Err(_) => continue,
            };

            let start_time = Instant::now();

            // Execute task
            let execute_result = execute_task(router, &task.task_type).await;

            let elapsed = start_time.elapsed();

            match execute_result {
                Ok(()) => {
                    // Register completed task
                    let _: () = con.sadd(&completed_key, task_json).await?;
                }
                Err(e) => {
                    log::error!("Task {} failed: {:?}", task.task_id, e);
                    let err_msg = e.to_string();
                    if err_msg.contains("Write verification failed") {
                        log::warn!(
                            "CRITICAL: Pausing job {} due to write verification failure!",
                            task.job_id
                        );
                        let _: () = con.set(&paused_key, "1").await?;
                    }
                    // Push back to pending
                    let _: () = con.sadd(&pending_key, task_json).await?;
                }
            }

            // Duty-cycle CPU throttling
            if cpu_limit < 100 {
                // sleep_duration = elapsed * (100 - L) / L
                let multiplier = (100 - cpu_limit) as f64 / cpu_limit as f64;
                let sleep_duration = elapsed.mul_f64(multiplier);
                if sleep_duration > Duration::from_millis(1) {
                    sleep(sleep_duration).await;
                }
            }
        }
    }

    Ok(())
}

async fn execute_task(router: &DataRouter, task_type: &TaskType) -> Result<()> {
    match task_type {
        TaskType::BlockMove {
            map_id,
            idx_str,
            src_offset,
            dest_offset,
            len,
        } => {
            // Read block from primary device
            let data = router.nvme_writer.read_block(*src_offset, *len).await?;

            // Write block to destination offset
            router.nvme_writer.write_block(*dest_offset, &data).await?;

            // Atomically update block_map metadata in database
            let mut con = router.dlm.get_connection().await?;
            let key = format!("{}:block_map:{}", crate::fs_prefix(), map_id);
            let _: () = con.hset(&key, idx_str, dest_offset.to_string()).await?;

            // Free the old high block
            router.block_allocator.free_block(*src_offset).await?;
        }
    }
    Ok(())
}
