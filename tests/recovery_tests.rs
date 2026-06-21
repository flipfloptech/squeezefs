use redis::AsyncCommands;
use squeezefs::backend::RustFsClient;
use squeezefs::recovery::recover_staging;
use std::fs;
use tempfile::tempdir;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

#[tokio::test]
async fn test_crash_recovery_flow() {
    let redis_url = get_redis_url();
    let redis_client = match redis::Client::open(redis_url.clone()) {
        Ok(c) => {
            if c.get_multiplexed_tokio_connection().await.is_err() {
                println!("Skipping test: Redis/Garnet not available");
                return;
            }
            c
        }
        Err(_) => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();

    let file_path = "test_recoverable.bin";
    let file_id = "recoverable-uuid-1111";
    let data = vec![8; 2000];
    let fencing_token = 500u64;

    // 1. Manually write the local staging files to simulate a crash before flush
    let data_path = temp_dir.path().join(format!("{}.data", file_id));
    let meta_path = temp_dir.path().join(format!("{}.meta", file_id));

    fs::write(&data_path, &data).unwrap();
    let meta_content = serde_json::json!({
        "file_path": file_path,
        "fencing_token": fencing_token
    });
    fs::write(&meta_path, serde_json::to_vec(&meta_content).unwrap()).unwrap();

    // 2. Set the metadata in Garnet matching the staging ID (simulating active write)
    let mut con = redis_client
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let _: () = redis::pipe()
        .hset(&meta_key, "size", data.len())
        .hset(&meta_key, "type", "staged")
        .hset(&meta_key, "file_id", file_id)
        .hset(&meta_key, "fencing_token", fencing_token)
        .query_async(&mut con)
        .await
        .unwrap();

    let meta_client = squeezefs::dlm::MetaClient::Single(redis_client.clone());
    // 3. Execute recovery
    let recovered = recover_staging(temp_dir.path(), &mock_backend, &meta_client)
        .await
        .expect("Recovery should complete");

    assert_eq!(recovered, 1);

    // 4. Verify local staging files are cleaned up
    assert!(!data_path.exists());
    assert!(!meta_path.exists());

    // 5. Verify Garnet mapping has been recorded
    let mapping_key = format!("mapping:{}", file_id);
    let block: Option<String> = con.hget(&mapping_key, "block").await.unwrap();
    let offset: Option<u64> = con.hget(&mapping_key, "offset").await.unwrap();
    let size: Option<u64> = con.hget(&mapping_key, "size").await.unwrap();

    assert_eq!(block, Some(format!("recovered/blocks/{}", file_id)));
    assert_eq!(offset, Some(0));
    assert_eq!(size, Some(2000));
}

#[tokio::test]
async fn test_stale_write_recovery_discard() {
    let redis_url = get_redis_url();
    let redis_client = match redis::Client::open(redis_url.clone()) {
        Ok(c) => {
            if c.get_multiplexed_tokio_connection().await.is_err() {
                println!("Skipping test: Redis/Garnet not available");
                return;
            }
            c
        }
        Err(_) => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let temp_dir = tempdir().unwrap();
    let mock_backend = RustFsClient::new_mock();

    let file_path = "test_stale.bin";
    let file_id = "stale-uuid-2222";
    let data = vec![9; 1000];
    let fencing_token = 500u64;

    // 1. Manually write the local staging files
    let data_path = temp_dir.path().join(format!("{}.data", file_id));
    let meta_path = temp_dir.path().join(format!("{}.meta", file_id));

    fs::write(&data_path, &data).unwrap();
    let meta_content = serde_json::json!({
        "file_path": file_path,
        "fencing_token": fencing_token
    });
    fs::write(&meta_path, serde_json::to_vec(&meta_content).unwrap()).unwrap();

    // 2. Set the metadata in Garnet pointing to a DIFFERENT file_id (newer write occurred since crash)
    let mut con = redis_client
        .get_multiplexed_tokio_connection()
        .await
        .unwrap();
    let meta_key = format!("metadata:{}", file_path);
    let _: () = redis::pipe()
        .hset(&meta_key, "size", 4000)
        .hset(&meta_key, "type", "staged")
        .hset(&meta_key, "file_id", "newer-uuid-3333") // Different ID!
        .hset(&meta_key, "fencing_token", fencing_token + 1)
        .query_async(&mut con)
        .await
        .unwrap();

    let meta_client = squeezefs::dlm::MetaClient::Single(redis_client.clone());
    // 3. Execute recovery
    let recovered = recover_staging(temp_dir.path(), &mock_backend, &meta_client)
        .await
        .expect("Recovery should complete");

    // Should NOT recover the file because it is stale
    assert_eq!(recovered, 0);

    // 4. Stale local staging files should still be cleaned up to prevent disk leak
    assert!(!data_path.exists());
    assert!(!meta_path.exists());

    // 5. Verify Garnet mapping has NOT been recorded for this stale ID
    let mapping_key = format!("mapping:{}", file_id);
    let block: Option<String> = con.hget(&mapping_key, "block").await.unwrap();
    assert!(block.is_none());
}
