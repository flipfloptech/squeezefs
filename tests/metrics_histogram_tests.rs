//! Integration tests for write path metrics histograms.
//!
//! Run with: `cargo test --all-features --test metrics_histogram_tests -- --test-threads=1`

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{set_fs_prefix, set_write_verification};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile};

fn redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn garnet_available() -> bool {
    let Ok(client) = redis::Client::open(redis_url()) else {
        return false;
    };
    client.get_multiplexed_tokio_connection().await.is_ok()
}

#[tokio::test]
async fn test_metrics_write_path_histograms() {
    if !garnet_available().await {
        println!("Skipping metrics_histogram_tests: Redis/Garnet not available");
        return;
    }

    let test_id = "metrics_histogram_test";
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prefix = format!("{}_{}", test_id, uniq);
    set_fs_prefix(&prefix);
    set_write_verification(false);

    let dlm = DlmClient::new(&redis_url()).unwrap();

    // Format metadata settings in Redis
    {
        let mut con = dlm.meta_client().get_connection().await.unwrap();
        let format_key = format!("{prefix}:format");
        let _: () = redis::cmd("HSET")
            .arg(&format_key)
            .arg("name")
            .arg(&prefix)
            .arg("block_size")
            .arg("4194304") // 4MB block size
            .query_async(&mut con)
            .await
            .unwrap();
    }

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new(Arc::new(dlm.meta_client().clone()), &prefix)
            .await
            .unwrap(),
    );
    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();
    let router = DataRouter::new(
        dlm.clone(),
        cache.clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    );
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    // Record some mock measurements into METRICS histograms directly to verify bucketing logic
    METRICS.write_lock_wait.record(Duration::from_micros(0)); // <=1us
    METRICS.write_lock_wait.record(Duration::from_micros(2)); // <=2us
    METRICS.write_lock_wait.record(Duration::from_micros(3)); // <=4us
    METRICS.write_lock_wait.record(Duration::from_micros(100)); // <=128us
    METRICS.write_lock_wait.record(Duration::from_millis(50)); // <=64ms
    METRICS.write_lock_wait.record(Duration::from_secs(20)); // >16s

    METRICS.writeback_queue_depth.record(0);
    METRICS.writeback_queue_depth.record(1);
    METRICS.writeback_queue_depth.record(4);
    METRICS.writeback_queue_depth.record(5000);

    // Call generate_stats_json to get the JSON output
    let stats_json_str = fs.generate_stats_json().await;
    let stats: serde_json::Value = serde_json::from_str(&stats_json_str).unwrap();

    let metrics = stats.get("metrics").unwrap();

    // Verify write_lock_wait histogram counts in stats JSON
    let w_lock = metrics.get("write_lock_wait").unwrap();
    assert_eq!(w_lock.get("<=1us").unwrap().as_u64().unwrap(), 1);
    assert_eq!(w_lock.get("<=2us").unwrap().as_u64().unwrap(), 1);
    assert_eq!(w_lock.get("<=4us").unwrap().as_u64().unwrap(), 1);
    assert_eq!(w_lock.get("<=128us").unwrap().as_u64().unwrap(), 1);
    assert_eq!(w_lock.get("<=64ms").unwrap().as_u64().unwrap(), 1);
    assert_eq!(w_lock.get(">16s").unwrap().as_u64().unwrap(), 1);

    // Verify writeback_queue_depth histogram counts in stats JSON
    let q_depth = metrics.get("writeback_queue_depth").unwrap();
    assert_eq!(q_depth.get("0").unwrap().as_u64().unwrap(), 1);
    assert_eq!(q_depth.get("1").unwrap().as_u64().unwrap(), 1);
    assert_eq!(q_depth.get("<=4").unwrap().as_u64().unwrap(), 1);
    assert_eq!(q_depth.get(">4096").unwrap().as_u64().unwrap(), 1);

    // Verify other histograms exist
    assert!(metrics.get("block_lock_wait").is_some());
    assert!(metrics.get("lease_lock_wait").is_some());
    assert!(metrics.get("dlm_acquire_time").is_some());
}
