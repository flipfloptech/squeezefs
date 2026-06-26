/*
 * SqueezeFS, Copyright 2026 Juicedata, Inc.
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

use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::fuse_client::format_volume_ext;
use std::fs;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

#[tokio::test]
async fn test_format_wipes_all_data() {
    let redis_url = get_redis_url();
    let client = match redis::Client::open(redis_url.clone()) {
        Ok(c) => c,
        Err(_) => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };
    let mut con = match client.get_multiplexed_tokio_connection().await {
        Ok(c) => c,
        Err(_) => {
            println!("Skipping test: Redis/Garnet connection failed");
            return;
        }
    };

    // 1. Prepare Redis keys that should be wiped
    let _: () = con
        .set("squeezefs:test_dummy_key", "dummy_val")
        .await
        .unwrap();
    assert_eq!(
        con.get::<_, Option<String>>("squeezefs:test_dummy_key")
            .await
            .unwrap(),
        Some("dummy_val".to_string())
    );

    // 2. Prepare local cache/staging directory and files
    let temp_dir = tempdir().unwrap();
    let staging_path = temp_dir.path().to_path_buf();

    let cache_seg_dir = staging_path.join("cache_segment");
    let staging_seg_dir = staging_path.join("staging_segment");
    fs::create_dir_all(&cache_seg_dir).unwrap();
    fs::create_dir_all(&staging_seg_dir).unwrap();

    let dummy_block_file = cache_seg_dir.join("block_123");
    let dummy_staging_file = staging_seg_dir.join("staging_456");
    let dummy_gds_file = staging_path.join("file_789.gds_cache");

    fs::write(&dummy_block_file, "cached block data").unwrap();
    fs::write(&dummy_staging_file, "staged write data").unwrap();
    fs::write(&dummy_gds_file, "gds data").unwrap();

    assert!(dummy_block_file.exists());
    assert!(dummy_staging_file.exists());
    assert!(dummy_gds_file.exists());

    // 3. Prepare S3 data
    let s3_endpoint = "http://127.0.0.1:9000".to_string();
    let s3_bucket = format!("format-wipe-test-{}", uuid::Uuid::new_v4());

    // We instantiate a real client and initialize the bucket
    let s3_client = RustFsClient::new_with_local_ips(
        Vec::new(),
        Some(s3_endpoint.clone()),
        Some("admin".to_string()),
        Some("password".to_string()),
        Some(s3_bucket.clone()),
    )
    .await;

    // Put some test object
    let test_key = "test_block_key";
    s3_client
        .put_object(test_key, bytes::Bytes::from("test data"), 1)
        .await
        .unwrap();

    // Verify it is there
    let fetched = s3_client.get_object(test_key).await;
    assert!(fetched.is_ok());
    assert_eq!(fetched.unwrap(), b"test data");

    // Also populate the bucket in squeezefs:backends so formatting detects it and wipes it
    let backend_info = serde_json::json!({
        "endpoint": s3_endpoint,
        "access_key": "admin",
        "secret_key": "password",
        "bucket": s3_bucket,
    });
    let _: () = con
        .hset("squeezefs:backends", "backend_0", backend_info.to_string())
        .await
        .unwrap();

    // Populate disk_cache_paths in squeezefs:format so format knows to wipe it
    let _: () = con
        .hset(
            "squeezefs:format",
            "disk_cache_paths",
            staging_path.to_string_lossy().to_string(),
        )
        .await
        .unwrap();

    // 4. Perform Full Format (quick = false)
    let res = format_volume_ext(
        &redis_url,
        "format_wipe_test_volume",
        4 * 1024 * 1024,
        100 * 1024 * 1024,
        1000,
        "none",
        "none",
        None,
        None,
        None,
        Some(std::slice::from_ref(&staging_path)),
        Some(&s3_endpoint),
        Some("admin"),
        Some("password"),
        Some(&s3_bucket),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        false, // quick = false (wipe object storage)
    )
    .await;

    assert!(res.is_ok());

    // 5. Verify local files are wiped
    assert!(
        !dummy_block_file.exists(),
        "Local block cache file should have been deleted"
    );
    assert!(
        !dummy_staging_file.exists(),
        "Local staged write file should have been deleted"
    );
    assert!(
        !dummy_gds_file.exists(),
        "Local GDS cache file should have been deleted"
    );

    // Check that the directories were re-created empty
    assert!(cache_seg_dir.exists() || staging_path.exists());

    // 6. Verify Redis/Garnet database is flushed
    let keys: Vec<String> = con.keys("*").await.unwrap();
    // Only format-related metadata keys generated by the new format should exist
    for key in &keys {
        assert!(
            key.starts_with("squeezefs:format")
                || key == "squeezefs:used_inodes"
                || key == "squeezefs:inode_counter"
                || key == "squeezefs:backends",
            "Stale Redis key found: {}",
            key
        );
    }
    assert_eq!(
        con.get::<_, Option<String>>("squeezefs:test_dummy_key")
            .await
            .unwrap(),
        None,
        "User-defined dummy key should be deleted"
    );

    // 7. Verify S3 objects in bucket are deleted
    let fetched_after_format = s3_client.get_object(test_key).await;
    assert!(
        fetched_after_format.is_err(),
        "S3 object should have been deleted during formatting"
    );
}
