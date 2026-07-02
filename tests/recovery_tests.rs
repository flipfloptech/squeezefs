use redis::AsyncCommands;
use squeezefs::dlm::DlmClient;
use std::sync::Arc;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn get_client() -> Option<DlmClient> {
    let url = get_redis_url();
    let c = redis::Client::open(url.clone()).ok()?;
    if c.get_multiplexed_tokio_connection().await.is_err() {
        return None;
    }
    DlmClient::new(&url).ok()
}

async fn clear_redis_keys(con: &mut squeezefs::dlm::MetaConnection) {
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg("*")
        .query_async(con)
        .await
        .unwrap_or_default();
    if !keys.is_empty() {
        let mut pipe = redis::pipe();
        for k in &keys {
            pipe.del(k);
        }
        let _: () = pipe.query_async(con).await.unwrap_or_default();
    }
}

#[tokio::test]
async fn test_fsck_and_recovery_flow() {
    let dlm = match get_client().await {
        Some(c) => c,
        None => {
            println!("Skipping test: Redis/Garnet not available");
            return;
        }
    };

    let mut con = dlm.meta_client().get_connection().await.unwrap();

    // ==========================================
    // PHASE 1: FSCK Audit Tests
    // ==========================================
    clear_redis_keys(&mut con).await;

    let fs_name = "test_fsck_rec";
    squeezefs::set_fs_prefix(fs_name);

    // Initialize valid filesystem state
    let root_attr_key = format!("{}:attr:1", fs_name);
    let _: () = redis::cmd("HSET")
        .arg(&root_attr_key)
        .arg("ino")
        .arg("1")
        .query_async(&mut con)
        .await
        .unwrap();

    // Format fields
    let format_key = format!("{}:format", fs_name);
    let _: () = redis::cmd("HSET")
        .arg(&format_key)
        .arg("name")
        .arg(fs_name)
        .arg("capacity")
        .arg("1073741824")
        .query_async(&mut con)
        .await
        .unwrap();

    // Set highest_block = 2
    let max_block_key = format!("{}:highest_block", fs_name);
    let _: () = redis::cmd("SET")
        .arg(&max_block_key)
        .arg("2")
        .query_async(&mut con)
        .await
        .unwrap();

    // Register active backend
    let backends_key = format!("{}:backends", fs_name);
    let _: () = redis::cmd("HSET")
        .arg(&backends_key)
        .arg("backend_0")
        .arg("{\"backing_dev\":\"/dev/null\"}")
        .query_async(&mut con)
        .await
        .unwrap();

    // Create a valid striped file (inode 2)
    let meta_key = "metadata:inode_2";
    let _: () = redis::cmd("HSET")
        .arg(meta_key)
        .arg("type")
        .arg("striped")
        .arg("block_map_id")
        .arg("bmap2")
        .arg("fencing_token")
        .arg("10")
        .query_async(&mut con)
        .await
        .unwrap();

    let block_map_key = "block_map:bmap2";
    let _: () = redis::cmd("HSET")
        .arg(block_map_key)
        .arg("0")
        .arg("backend_0://4194304") // block index 1
        .query_async(&mut con)
        .await
        .unwrap();

    // Set block refcount in Redis to 1
    let refcounts_key = format!("{}:block_refcounts", fs_name);
    let _: () = redis::cmd("HSET")
        .arg(&refcounts_key)
        .arg("backend_0://4194304")
        .arg("1")
        .query_async(&mut con)
        .await
        .unwrap();

    // Set block index 2 to be in free blocks set
    let free_blocks_key = format!("{}:free_blocks", fs_name);
    let _: () = redis::cmd("SADD")
        .arg(&free_blocks_key)
        .arg("2")
        .query_async(&mut con)
        .await
        .unwrap();

    // Run FSCK on clean state. This is consistent.
    let issues = squeezefs::config_ops::run_metadata_fsck(&get_redis_url(), fs_name)
        .await
        .unwrap();
    assert!(
        issues.is_empty(),
        "Expected no issues on clean state, but got: {:?}",
        issues
    );

    // Inject consistency issues to test FSCK validation
    // Test Case A: Dangling Block (Reference a free block)
    let _: () = redis::cmd("HSET")
        .arg(block_map_key)
        .arg("1")
        .arg("backend_0://8388608") // block index 2 (marked as free!)
        .query_async(&mut con)
        .await
        .unwrap();

    // Test Case B: Reference Count Mismatch
    let _: () = redis::cmd("HSET")
        .arg(&refcounts_key)
        .arg("backend_0://4194304")
        .arg("2")
        .query_async(&mut con)
        .await
        .unwrap();

    // Test Case C: Out of Bounds Block (Exceeds highest_block)
    // We will set highest_block = 4 on Case D, so index 5 exceeds it.
    let _: () = redis::cmd("HSET")
        .arg(block_map_key)
        .arg("2")
        .arg("backend_0://20971520") // block index 5 (exceeds highest_block=4)
        .query_async(&mut con)
        .await
        .unwrap();

    // Test Case D: Leaked Block
    let _: () = redis::cmd("SET")
        .arg(&max_block_key)
        .arg("4")
        .query_async(&mut con)
        .await
        .unwrap();

    // Run FSCK to verify all injected issues are detected
    let issues = squeezefs::config_ops::run_metadata_fsck(&get_redis_url(), fs_name)
        .await
        .unwrap();
    println!("FSCK Injected Issues: {:#?}", issues);

    assert!(issues.iter().any(|i| i.contains("exceeds highest_block")));
    assert!(issues.iter().any(|i| i.contains("marked as FREE")));
    assert!(issues
        .iter()
        .any(|i| i.contains("Reference count mismatch")));
    assert!(issues.iter().any(|i| i.contains("Leaked block detected")));

    // ==========================================
    // PHASE 2: Crash Staging Recovery & Fencing
    // ==========================================
    clear_redis_keys(&mut con).await;

    let fs_name_rec = "test_rec_fencing";
    squeezefs::set_fs_prefix(fs_name_rec);

    let temp_dir = tempfile::tempdir().unwrap();
    let staging_path = temp_dir.path().to_path_buf();
    let staging_segment_dir = staging_path.join("staging_segment");
    std::fs::create_dir_all(&staging_segment_dir).unwrap();

    // Set up format fields with write_disk_limit to align shard count to 1
    let format_key_rec = format!("{}:format", fs_name_rec);
    let _: () = redis::cmd("HSET")
        .arg(&format_key_rec)
        .arg("name")
        .arg(fs_name_rec)
        .arg("write_disk_limit")
        .arg("1048576")
        .query_async(&mut con)
        .await
        .unwrap();

    // Set up mock metadata in database
    // Inode 10: database has fencing_token = 5. Staged block has fencing_token = 8 (Valid / New).
    let _: () = redis::cmd("HSET")
        .arg("metadata:inode_10")
        .arg("type")
        .arg("striped")
        .arg("fencing_token")
        .arg("5")
        .query_async(&mut con)
        .await
        .unwrap();

    // Inode 11: database has fencing_token = 12. Staged block has fencing_token = 8 (Stale / Expired).
    let _: () = redis::cmd("HSET")
        .arg("metadata:inode_11")
        .arg("type")
        .arg("striped")
        .arg("fencing_token")
        .arg("12")
        .query_async(&mut con)
        .await
        .unwrap();

    // Write mock staged active blocks to NvmeCache (binary StagedMetadata + 4KiB header pad)
    let cache =
        squeezefs::tiering::nvme::NvmeCache::new(&[&staging_segment_dir], &[100 * 1024 * 1024], 1)
            .unwrap();

    let create_active_payload = |f_token: u64, file_path: &str| -> bytes::Bytes {
        let meta = squeezefs::cache::nvme::StagedMetadata {
            fencing_token: f_token,
            original_size: 16,
            file_path: file_path.to_string(),
        };
        let meta_bytes = meta.serialize();
        let meta_len = meta_bytes.len() as u64;
        let mut buf = Vec::new();
        buf.extend_from_slice(&meta_len.to_be_bytes());
        buf.extend_from_slice(&meta_bytes);
        buf.resize(4096, 0); // active_block header pad (must match put_active_block)
        buf.extend_from_slice(&[9u8; 16]); // 16 bytes of data
        bytes::Bytes::from(buf)
    };

    // Active block for Inode 10: fencing token = 8 (should be recovered)
    let k10 = bytes::Bytes::from("active_block:inode_10:block_0");
    let v10 = create_active_payload(8, "inode_10");
    cache.put(k10, v10);

    // Active block for Inode 11: fencing token = 8 (should be discarded because db has 12)
    let k11 = bytes::Bytes::from("active_block:inode_11:block_0");
    let v11 = create_active_payload(8, "inode_11");
    cache.put(k11, v11);

    // Write incomplete block to test parser resiliency
    let k_bad = bytes::Bytes::from("active_block:inode_10:block_1");
    let v_bad = bytes::Bytes::from(vec![0u8; 4]); // truncated data
    cache.put(k_bad, v_bad);

    // Instantiate mock BlockAllocator and NvmeBlockDev
    let block_allocator = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(
            Arc::new(dlm.meta_client().clone()),
            fs_name_rec,
        )
        .await
        .unwrap(),
    );

    let temp_backing_file = tempfile::NamedTempFile::new().unwrap();
    let nvme_writer = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        temp_backing_file.path().to_str().unwrap(),
    ));

    // Drop the cache instance to sync mappings to segment files
    drop(cache);

    // Run recovery
    let recovered = squeezefs::recovery::recover_staging(
        &staging_path,
        dlm.meta_client(),
        &block_allocator,
        &nvme_writer,
    )
    .await
    .unwrap();

    assert_eq!(
        recovered, 1,
        "Only 1 file (inode 10) should have been recovered"
    );

    // Verify inode 10 block_map was updated
    let bmap_opt: Option<String> = con.hget("metadata:inode_10", "block_map_id").await.unwrap();
    assert!(bmap_opt.is_some());
    let bmap_id = bmap_opt.unwrap();
    let block_val: Option<String> = con
        .hget(format!("block_map:{}", bmap_id), "0")
        .await
        .unwrap();
    assert!(block_val.is_some());
    assert!(block_val.unwrap().starts_with("backend_0://"));

    // Verify inode 11 block_map was NOT updated
    let bmap_opt11: Option<String> = con.hget("metadata:inode_11", "block_map_id").await.unwrap();
    assert!(bmap_opt11.is_none());

    // Clean up database at the end
    clear_redis_keys(&mut con).await;
}
