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

use once_cell::sync::Lazy;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::tempdir;

static TEST_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_distributed_job_execution() {
    let _guard = TEST_MUTEX.lock().unwrap();
    squeezefs::set_write_verification(false);
    squeezefs::nvme_dev::set_simulate_corruption(false);

    let redis_url = "redis://127.0.0.1:6379";
    let fs_name = "test_vol_jobs";
    squeezefs::set_fs_prefix(fs_name);

    // 1. Reset redis key state
    let client = redis::Client::open(redis_url).unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_vol_jobs:active_jobs")
            .arg("test_vol_jobs:free_blocks")
            .arg("test_vol_jobs:highest_block")
            .arg("test_vol_jobs:block_refcounts")
            .arg("test_vol_jobs:block_map:test_map_1")
            .query(&mut conn)
            .unwrap_or_default();
    }

    let dlm = DlmClient::new(redis_url).unwrap();
    let meta_client = Arc::new(dlm.meta_client().clone());
    let block_alloc = Arc::new(
        BlockAllocator::new(meta_client.clone(), fs_name)
            .await
            .unwrap(),
    );

    // Allocate a low-index block hole and a high block to move
    let src_block_idx = 10u64;
    let dest_block_idx = 1u64;
    let chunk_size = 4 * 1024 * 1024;
    let src_offset = src_block_idx * chunk_size;
    let dest_offset = dest_block_idx * chunk_size;

    // Add dest block to free list so it can be specifically allocated (claimed)
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("SADD")
            .arg("test_vol_jobs:free_blocks")
            .arg(dest_block_idx)
            .query(&mut conn)
            .unwrap();
    }

    block_alloc
        .allocate_specific_block(dest_block_idx)
        .await
        .unwrap();

    let temp_dir = tempdir().unwrap();
    let block_file = temp_dir.path().join("mock_block.img");

    // Create mock 100MB sparse file
    let file = std::fs::File::create(&block_file).unwrap();
    file.set_len(100 * 1024 * 1024).unwrap();

    let nvme_dev = Arc::new(NvmeBlockDev::new(block_file.to_str().unwrap()));

    // Write mock data to source block offset
    let mock_data = vec![0xAAu8; 4096];
    nvme_dev.write_block(src_offset, &mock_data).await.unwrap();

    let staging_dir = temp_dir.path().join("staging");
    std::fs::create_dir_all(&staging_dir).unwrap();

    let cache = TieredCache::new(
        vec![staging_dir],
        None,
        None,
        None,
        None,
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();

    let router = Arc::new(DataRouter::new(dlm, cache, block_alloc, nvme_dev));

    // 2. Start background worker with 100% CPU (no throttling for test speed)
    squeezefs::jobs::start_job_worker(router.clone(), fs_name.to_string(), 100);

    // 3. Submit a BlockMove task
    let task = squeezefs::jobs::TaskType::BlockMove {
        map_id: "test_map_1".to_string(),
        idx_str: "0".to_string(),
        src_offset,
        dest_offset,
        len: 4096,
    };

    squeezefs::jobs::submit_and_wait_for_job(redis_url, fs_name, vec![task])
        .await
        .unwrap();

    // 4. Verify data was successfully moved to dest_offset
    let read_back = router
        .nvme_writer
        .read_block(dest_offset, 4096)
        .await
        .unwrap();
    assert_eq!(read_back, mock_data);

    // Verify metadata was updated
    let mut con = router.dlm.get_connection().await.unwrap();
    let block_map_key = format!("test_vol_jobs:block_map:test_map_1");
    let mapped_offset: String = redis::cmd("HGET")
        .arg(&block_map_key)
        .arg("0")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(mapped_offset, dest_offset.to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_write_verification_failure_pauses_job() {
    let _guard = TEST_MUTEX.lock().unwrap();
    let redis_url = "redis://127.0.0.1:6379";
    let fs_name = "test_vol_jobs_fail";
    squeezefs::set_fs_prefix(fs_name);

    // 1. Enable write verification
    squeezefs::set_write_verification(true);

    // Reset redis key state
    let client = redis::Client::open(redis_url).unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_vol_jobs_fail:active_jobs")
            .arg("test_vol_jobs_fail:free_blocks")
            .arg("test_vol_jobs_fail:highest_block")
            .arg("test_vol_jobs_fail:block_refcounts")
            .arg("test_vol_jobs_fail:block_map:test_map_2")
            .query(&mut conn)
            .unwrap_or_default();
    }

    let dlm = DlmClient::new(redis_url).unwrap();
    let meta_client = Arc::new(dlm.meta_client().clone());
    let block_alloc = Arc::new(
        BlockAllocator::new(meta_client.clone(), fs_name)
            .await
            .unwrap(),
    );

    let src_block_idx = 10u64;
    let dest_block_idx = 1u64;
    let chunk_size = 4 * 1024 * 1024;
    let src_offset = src_block_idx * chunk_size;
    let dest_offset = dest_block_idx * chunk_size;

    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("SADD")
            .arg("test_vol_jobs_fail:free_blocks")
            .arg(dest_block_idx)
            .query(&mut conn)
            .unwrap();
    }

    block_alloc
        .allocate_specific_block(dest_block_idx)
        .await
        .unwrap();

    let temp_dir = tempdir().unwrap();
    let block_file = temp_dir.path().join("mock_block.img");

    let file = std::fs::File::create(&block_file).unwrap();
    file.set_len(100 * 1024 * 1024).unwrap();

    let nvme_dev = Arc::new(NvmeBlockDev::new(block_file.to_str().unwrap()));

    // Write mock data to source block offset
    let mock_data = vec![0xAAu8; 4096];
    nvme_dev.write_block(src_offset, &mock_data).await.unwrap();

    let staging_dir = temp_dir.path().join("staging");
    std::fs::create_dir_all(&staging_dir).unwrap();

    let cache = TieredCache::new(
        vec![staging_dir],
        None,
        None,
        None,
        None,
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();

    let router = Arc::new(DataRouter::new(dlm, cache, block_alloc, nvme_dev));

    // Enable simulated write verification mismatch
    squeezefs::nvme_dev::set_simulate_corruption(true);

    // 2. Start background worker
    squeezefs::jobs::start_job_worker(router.clone(), fs_name.to_string(), 100);

    // 3. Submit a BlockMove task
    let task = squeezefs::jobs::TaskType::BlockMove {
        map_id: "test_map_2".to_string(),
        idx_str: "0".to_string(),
        src_offset,
        dest_offset,
        len: 4096,
    };

    // Wait/poll the database keys to verify the job transitions to paused status
    let mut con = router.dlm.get_connection().await.unwrap();
    let job_id = uuid::Uuid::new_v4().to_string();
    let pending_key = format!("{}:jobs:{}:pending", fs_name, job_id);
    let active_set_key = format!("{}:active_jobs", fs_name);
    let paused_key = format!("{}:jobs:{}:paused", fs_name, job_id);

    let task_wrapper = squeezefs::jobs::JobTask {
        task_id: format!("{}_0", job_id),
        job_id: job_id.clone(),
        task_type: task,
    };
    let task_json = serde_json::to_string(&task_wrapper).unwrap();

    // Add to active and pending
    let _: () = redis::cmd("SADD")
        .arg(&pending_key)
        .arg(&task_json)
        .query_async(&mut con)
        .await
        .unwrap();
    let _: () = redis::cmd("SADD")
        .arg(&active_set_key)
        .arg(&job_id)
        .query_async(&mut con)
        .await
        .unwrap();

    // Check if the worker runs, fails the task, and pauses the job
    let mut paused = false;
    for _ in 0..20 {
        let is_paused: Option<String> = redis::cmd("GET")
            .arg(&paused_key)
            .query_async(&mut con)
            .await
            .unwrap_or(None);
        if is_paused.as_deref() == Some("1") {
            paused = true;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    squeezefs::nvme_dev::set_simulate_corruption(false);
    squeezefs::set_write_verification(false);

    assert!(
        paused,
        "Job should be paused globally due to write verification failure"
    );
}
