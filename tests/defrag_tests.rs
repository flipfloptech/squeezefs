use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::{DlmClient, MetaClient};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;

#[tokio::test]
async fn test_defragmentation_under_lock() {
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
