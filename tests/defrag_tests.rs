use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::{DlmClient, MetaClient};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;

#[tokio::test]
async fn test_defragmentation_under_lock() {
    let _ = env_logger::try_init();
    let redis_url = "redis://127.0.0.1:6379";
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Check connection
    if client.get_multiplexed_tokio_connection().await.is_err() {
        println!("Skipping test: Redis/Garnet not available");
        return;
    }

    let fs_name = "test_defrag_vol";

    // Clear out keys
    {
        let mut conn = client.get_connection().unwrap();

        // Scan and delete all metadata:inode_* keys
        let mut cursor: u64 = 0;
        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("metadata:inode_*")
                .arg("COUNT")
                .arg(1000)
                .query(&mut conn)
                .unwrap_or((0, vec![]));
            for key in keys {
                let _: () = redis::cmd("DEL")
                    .arg(&key)
                    .query(&mut conn)
                    .unwrap_or_default();
            }
            cursor = next_cursor;
            if cursor == 0 {
                break;
            }
        }

        // Scan and delete all block_map:* keys
        cursor = 0;
        loop {
            let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg("block_map:*")
                .arg("COUNT")
                .arg(1000)
                .query(&mut conn)
                .unwrap_or((0, vec![]));
            for key in keys {
                let _: () = redis::cmd("DEL")
                    .arg(&key)
                    .query(&mut conn)
                    .unwrap_or_default();
            }
            cursor = next_cursor;
            if cursor == 0 {
                break;
            }
        }

        let _: () = redis::cmd("DEL")
            .arg(format!("{}:free_blocks", fs_name))
            .arg(format!("{}:highest_block", fs_name))
            .arg(format!("{}:block_refcounts", fs_name))
            .arg(format!("{}:block_sizes", fs_name))
            .query(&mut conn)
            .unwrap_or_default();
    }

    let meta = Arc::new(MetaClient::new_single(client));
    let dev_path = "/tmp/squeezefs_test_defrag_dev";
    std::fs::write(dev_path, vec![0u8; 32 * 1024 * 1024]).unwrap(); // 32MB backing file

    let dev = Arc::new(NvmeBlockDev::new(dev_path));
    let alloc = Arc::new(BlockAllocator::new(meta.clone(), fs_name).await.unwrap());

    let dlm = DlmClient::new(redis_url).unwrap();
    let cache = TieredCache::new(
        vec![],
        None,
        None,
        None,
        None,
        (*meta).clone(),
        alloc.clone(),
        dev.clone(),
    )
    .unwrap();
    let router = Arc::new(DataRouter::new(dlm, cache, alloc.clone(), dev.clone()));

    // Start background job worker
    squeezefs::jobs::start_job_worker(router.clone(), fs_name.to_string(), 100);

    // 1. Manually setup metadata for a striped file (inode = 456)
    let file_path = "inode_456";
    let meta_key = format!("metadata:{}", file_path);
    let block_map_id = "defrag_map_999";
    let block_map_key = format!("block_map:{}", block_map_id);

    let mut con = router.dlm.get_connection().await.unwrap();
    let _: () = redis::cmd("DEL")
        .arg(&meta_key)
        .arg(&block_map_key)
        .query_async(&mut con)
        .await
        .unwrap_or_default();

    let _: () = redis::cmd("HSET")
        .arg(&meta_key)
        .arg("type")
        .arg("striped")
        .arg("block_map_id")
        .arg(block_map_id)
        .query_async(&mut con)
        .await
        .unwrap();

    // 2. Allocate low block (index 1 = 4MB), free it to create a hole, then allocate a high block (index 4 = 16MB)
    // Block size is 4MB. Block index 0 is reserved for superblock.

    // Allocate index 1 (hole target)
    let low_offset = alloc.allocate_block().await.unwrap();
    assert_eq!(low_offset, 4 * 1024 * 1024);

    // Allocate index 2 and 3
    let _ = alloc.allocate_block().await.unwrap();
    let _ = alloc.allocate_block().await.unwrap();

    // Allocate index 4 (high block)
    let high_offset = alloc.allocate_block().await.unwrap();
    assert_eq!(high_offset, 16 * 1024 * 1024);

    // Free index 1 to make it a hole
    alloc.free_block(low_offset).await.unwrap();

    // Map the high block in the block map of the file
    let _: () = redis::cmd("HSET")
        .arg(&block_map_key)
        .arg("0")
        .arg(high_offset.to_string())
        .query_async(&mut con)
        .await
        .unwrap();

    // Write some test data into the high block
    let test_data = b"defrag_test_payload_12345";
    router
        .nvme_writer
        .write_block(high_offset, test_data)
        .await
        .unwrap();

    // 3. Run defragmentation
    squeezefs::defrag::run_defragmentation(redis_url, fs_name, dev_path)
        .await
        .unwrap();

    // 4. Verify block was migrated to the low hole (offset 4MB = 4194304)
    let new_offset_str: String = redis::cmd("HGET")
        .arg(&block_map_key)
        .arg("0")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(new_offset_str, "4194304");

    // Verify data remains correct and matches at new offset
    let read_data = router
        .nvme_writer
        .read_block(4 * 1024 * 1024, test_data.len())
        .await
        .unwrap();
    assert_eq!(read_data.as_ref(), test_data);

    let _ = std::fs::remove_file(dev_path);
}

/// P2-12: BlockMove path must take the inode DLM lease; a held lease blocks a second
/// acquire (simulating live FUSE writers serialized against defrag moves).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_defrag_block_move_contends_for_inode_lease() {
    let redis_url = "redis://127.0.0.1:6379";
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return,
    };
    if client.get_multiplexed_tokio_connection().await.is_err() {
        println!("Skipping: Redis/Garnet not available");
        return;
    }

    let dlm = DlmClient::new(redis_url).unwrap();
    let ino = 9_001_234u64;
    let lock_name = format!("inode_{ino}");

    let held = dlm
        .acquire_lock(&lock_name, None, std::time::Duration::from_secs(30))
        .await
        .expect("hold live-writer lease");

    // Same path BlockMove uses: short retry budget — must fail while FUSE holds lease.
    let contended = dlm
        .acquire_lock_with_retry(&lock_name, None, std::time::Duration::from_secs(1), 3)
        .await;
    assert!(
        contended.is_err(),
        "second acquire must fail while inode lease is held (defrag vs live write)"
    );

    let _ = held.release().await;

    let after = dlm
        .acquire_lock_with_retry(&lock_name, None, std::time::Duration::from_secs(5), 5)
        .await;
    assert!(after.is_ok(), "lease available after live writer releases");
    let _ = after.unwrap().release().await;
}

/// P2-11 pure policy: low-hole selection and high-candidate retention.
#[test]
fn test_defrag_bounded_selection_helpers() {
    use squeezefs::defrag::{insert_high_candidate, select_low_free_holes};
    use std::collections::BTreeMap;

    assert_eq!(select_low_free_holes([0, 8, 3, 1, 2], 3, 2), vec![1, 2]);

    let mut map = BTreeMap::new();
    for off in [100u64, 200, 150, 50, 300] {
        insert_high_candidate(&mut map, off, (1, "m".into(), "0".into()), 100, 2);
    }
    let keys: Vec<_> = map.keys().copied().collect();
    assert_eq!(keys, vec![200, 300]);
}

#[tokio::test]
async fn test_defragmentation_single_file_only() {
    let _ = env_logger::try_init();
    let redis_url = "redis://127.0.0.1:6379";
    let client = match redis::Client::open(redis_url) {
        Ok(c) => c,
        Err(_) => return,
    };
    if client.get_multiplexed_tokio_connection().await.is_err() {
        println!("Skipping: Redis/Garnet not available");
        return;
    }

    let fs_name = "test_defrag_vol_single";
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg(format!("{}:free_blocks", fs_name))
            .arg(format!("{}:highest_block", fs_name))
            .arg(format!("{}:block_refcounts", fs_name))
            .arg(format!("{}:block_sizes", fs_name))
            .query(&mut conn)
            .unwrap_or_default();
    }

    let meta = Arc::new(MetaClient::new_single(client));
    let dev_path = "/tmp/squeezefs_test_defrag_single_dev";
    std::fs::write(dev_path, vec![0u8; 32 * 1024 * 1024]).unwrap();

    let dev = Arc::new(NvmeBlockDev::new(dev_path));
    let alloc = Arc::new(BlockAllocator::new(meta.clone(), fs_name).await.unwrap());
    let dlm = DlmClient::new(redis_url).unwrap();
    let cache = TieredCache::new(
        vec![],
        None,
        None,
        None,
        None,
        (*meta).clone(),
        alloc.clone(),
        dev.clone(),
    )
    .unwrap();
    let router = Arc::new(DataRouter::new(dlm, cache, alloc.clone(), dev.clone()));

    squeezefs::jobs::start_job_worker(router.clone(), fs_name.to_string(), 100);

    let ino_target = 111u64;
    let ino_nontarget = 222u64;

    let target_meta_key = format!("metadata:inode_{ino_target}");
    let nontarget_meta_key = format!("metadata:inode_{ino_nontarget}");

    let map_target = "defrag_map_target";
    let map_nontarget = "defrag_map_nontarget";

    let block_map_key_target = format!("block_map:{}", map_target);
    let block_map_key_nontarget = format!("block_map:{}", map_nontarget);

    let mut con = router.dlm.get_connection().await.unwrap();
    let _: () = redis::cmd("DEL")
        .arg(&target_meta_key)
        .arg(&nontarget_meta_key)
        .arg(&block_map_key_target)
        .arg(&block_map_key_nontarget)
        .query_async(&mut con)
        .await
        .unwrap_or_default();

    let _: () = redis::cmd("HSET")
        .arg(&target_meta_key)
        .arg("type")
        .arg("striped")
        .arg("block_map_id")
        .arg(map_target)
        .query_async(&mut con)
        .await
        .unwrap();

    let _: () = redis::cmd("HSET")
        .arg(&nontarget_meta_key)
        .arg("type")
        .arg("striped")
        .arg("block_map_id")
        .arg(map_nontarget)
        .query_async(&mut con)
        .await
        .unwrap();

    // Allocate 1st block = index 1 (4MB) -> this will be our hole
    let low_offset = alloc.allocate_block().await.unwrap();
    assert_eq!(low_offset, 4 * 1024 * 1024);

    // Allocate index 2 and 3
    let _ = alloc.allocate_block().await.unwrap();
    let _ = alloc.allocate_block().await.unwrap();

    // Allocate index 4 (16MB) -> high block for target file
    let high_offset_target = alloc.allocate_block().await.unwrap();
    assert_eq!(high_offset_target, 16 * 1024 * 1024);

    // Allocate index 5 (20MB) -> high block for non-target file
    let high_offset_nontarget = alloc.allocate_block().await.unwrap();
    assert_eq!(high_offset_nontarget, 20 * 1024 * 1024);

    // Free low block (offset 4MB) to make it a hole
    alloc.free_block(low_offset).await.unwrap();

    // Map target file's block to high_offset_target
    let _: () = redis::cmd("HSET")
        .arg(&block_map_key_target)
        .arg("0")
        .arg(high_offset_target.to_string())
        .query_async(&mut con)
        .await
        .unwrap();

    // Map non-target file's block to high_offset_nontarget
    let _: () = redis::cmd("HSET")
        .arg(&block_map_key_nontarget)
        .arg("0")
        .arg(high_offset_nontarget.to_string())
        .query_async(&mut con)
        .await
        .unwrap();

    // Write payloads to NVMe
    let payload_target = b"target_payload_data_999";
    let payload_nontarget = b"nontarget_payload_data_888";
    router
        .nvme_writer
        .write_block(high_offset_target, payload_target)
        .await
        .unwrap();
    router
        .nvme_writer
        .write_block(high_offset_nontarget, payload_nontarget)
        .await
        .unwrap();

    // Run defragmentation targeting ONLY the target file (ino_target)
    let opts = squeezefs::defrag::DefragOptions {
        target_inode: Some(ino_target),
        ..Default::default()
    };

    squeezefs::defrag::run_defragmentation_with_options(redis_url, fs_name, dev_path, opts)
        .await
        .unwrap();

    // Verify target file's block was migrated to the low hole (offset 4MB = 4194304)
    let target_new_offset: String = redis::cmd("HGET")
        .arg(&block_map_key_target)
        .arg("0")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(target_new_offset, "4194304");

    // Verify non-target file's block was NOT migrated (still at 20MB = 20971520)
    let nontarget_offset: String = redis::cmd("HGET")
        .arg(&block_map_key_nontarget)
        .arg("0")
        .query_async(&mut con)
        .await
        .unwrap();
    assert_eq!(nontarget_offset, "20971520");

    // Verify target data is correctly read from 4MB
    let read_target = router
        .nvme_writer
        .read_block(4 * 1024 * 1024, payload_target.len())
        .await
        .unwrap();
    assert_eq!(read_target.as_ref(), payload_target);

    let _ = std::fs::remove_file(dev_path);
}
