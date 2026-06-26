use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::routing::DataRouter;
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn clean_db() -> Option<redis::aio::MultiplexedConnection> {
    let redis_url = get_redis_url();
    let client = redis::Client::open(redis_url).ok()?;
    let mut con = client.get_multiplexed_tokio_connection().await.ok()?;
    let _: () = redis::cmd("FLUSHALL")
        .query_async(&mut con)
        .await
        .unwrap_or(());
    Some(con)
}

#[tokio::test]
async fn test_df_overall_and_file_reports() {
    let mut con = match clean_db().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Garnet/Redis not available");
            return;
        }
    };

    let redis_url = get_redis_url();
    let fs_name = "df_test_vol";

    // 1. Format the volume with lz4 compression enabled
    squeezefs::fuse_client::format_volume(
        &redis_url,
        fs_name,
        1024 * 1024,       // 1MB block size
        500 * 1024 * 1024, // 500MB capacity
        10000,
        "lz4", // lz4 compression
        "none",
        None,
        Some("64MB"),
        Some("256MB"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let dlm = DlmClient::new(&redis_url).unwrap();
    let backend = RustFsClient::new_mock();
    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        None,
        None,
        None,
        None,
        backend.clone(),
        dlm.meta_client().clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm, backend, cache);

    // Write different files to test size tracking:
    // A. Inline file (10KB of repetitive data -> compresses extremely well)
    let inline_data = vec![65; 10240];
    router
        .write_file("inline_file.bin", 0, &inline_data, 201)
        .await
        .unwrap();

    // B. Staged file (100KB of repetitive data)
    let staged_data = vec![66; 102400];
    router
        .write_file("staged_file.bin", 0, &staged_data, 202)
        .await
        .unwrap();

    // C. Striped file (5MB of repetitive data -> will cross the 1MB block size boundary, resulting in multiple blocks)
    let striped_data = vec![67; 5 * 1024 * 1024];
    router
        .write_file("striped_file.bin", 0, &striped_data, 203)
        .await
        .unwrap();

    // Let staged file merge-flusher run
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 2. Validate block sizes are registered in Garnet
    let block_sizes: HashMap<String, String> = con.hgetall("squeezefs:block_sizes").await.unwrap();
    assert!(
        !block_sizes.is_empty(),
        "block sizes should be registered in squeezefs:block_sizes"
    );

    // Print all block sizes for debug
    for (bk, size_info) in &block_sizes {
        println!("Registered block: {} -> {}", bk, size_info);
    }

    // 3. Run Cli commands as child processes
    // A. Overall df command
    let output_overall = Command::new("./target/debug/squeezefs")
        .arg("-g")
        .arg(&redis_url)
        .arg("df")
        .output()
        .expect("Failed to execute squeezefs df");

    assert!(output_overall.status.success());
    let stdout_overall = String::from_utf8_lossy(&output_overall.stdout);
    println!("--- STDOUT OVERALL ---");
    println!("{}", stdout_overall);
    assert!(stdout_overall.contains("SqueezeFS Filesystem Space Usage Summary:"));
    assert!(stdout_overall.contains("Capacity:"));
    assert!(stdout_overall.contains("Logical File Size:"));
    assert!(stdout_overall.contains("Physical S3 Size:"));
    assert!(stdout_overall.contains("Compression Ratio:"));

    // B. Inline file df command
    let output_inline = Command::new("./target/debug/squeezefs")
        .arg("-g")
        .arg(&redis_url)
        .arg("df")
        .arg("inline_file.bin")
        .output()
        .expect("Failed to execute squeezefs df inline_file.bin");

    assert!(output_inline.status.success());
    let stdout_inline = String::from_utf8_lossy(&output_inline.stdout);
    println!("--- STDOUT INLINE ---");
    println!("{}", stdout_inline);
    assert!(stdout_inline.contains("File Space Usage Details for: inline_file.bin"));
    assert!(stdout_inline.contains("Layout Type:  inline"));
    assert!(stdout_inline.contains("inline_data:inline_file.bin"));

    // C. Staged file df command
    let output_staged = Command::new("./target/debug/squeezefs")
        .arg("-g")
        .arg(&redis_url)
        .arg("df")
        .arg("staged_file.bin")
        .output()
        .expect("Failed to execute squeezefs df staged_file.bin");

    assert!(output_staged.status.success());
    let stdout_staged = String::from_utf8_lossy(&output_staged.stdout);
    println!("--- STDOUT STAGED ---");
    println!("{}", stdout_staged);
    assert!(stdout_staged.contains("File Space Usage Details for: staged_file.bin"));
    assert!(stdout_staged.contains("Layout Type:  staged"));

    // D. Striped file df command
    let output_striped = Command::new("./target/debug/squeezefs")
        .arg("-g")
        .arg(&redis_url)
        .arg("df")
        .arg("striped_file.bin")
        .output()
        .expect("Failed to execute squeezefs df striped_file.bin");

    assert!(output_striped.status.success());
    let stdout_striped = String::from_utf8_lossy(&output_striped.stdout);
    println!("--- STDOUT STRIPED ---");
    println!("{}", stdout_striped);
    assert!(stdout_striped.contains("File Space Usage Details for: striped_file.bin"));
    assert!(stdout_striped.contains("Layout Type:  striped"));
}
