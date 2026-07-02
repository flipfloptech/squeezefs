use squeezefs::block_allocator::BlockAllocator;
use squeezefs::dlm::MetaClient;
use std::sync::Arc;

// TDD Phase 2: Define tests for Block Allocator

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_allocate_and_free_block() {
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_vol_1:free_blocks")
            .arg("test_vol_1:highest_block")
            .query(&mut conn)
            .unwrap_or_default();
    }
    let meta = Arc::new(MetaClient::new_single(client));

    let allocator = BlockAllocator::new(meta, "test_vol_1")
        .await
        .expect("Failed to create allocator");

    // 2. Allocate a block
    let offset1 = allocator
        .allocate_block()
        .await
        .expect("Failed to allocate block");
    assert_eq!(offset1 % (4 * 1024 * 1024), 0, "Offset must be 4MB aligned");

    let offset2 = allocator
        .allocate_block()
        .await
        .expect("Failed to allocate second block");
    assert_ne!(offset1, offset2, "Offsets must be unique");

    // 3. Free the first block
    allocator
        .free_block(offset1)
        .await
        .expect("Failed to free block");

    // 4. Re-allocate
    let offset3 = allocator
        .allocate_block()
        .await
        .expect("Failed to allocate block after free");
    assert_ne!(
        offset2, offset3,
        "Re-allocated offset must not overlap with active blocks"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_allocations() {
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_vol_concurrent:free_blocks")
            .arg("test_vol_concurrent:highest_block")
            .query(&mut conn)
            .unwrap_or_default();
    }
    let meta = Arc::new(MetaClient::new_single(client));

    let allocator = Arc::new(
        BlockAllocator::new(meta, "test_vol_concurrent")
            .await
            .unwrap(),
    );

    let mut handles = vec![];
    for _ in 0..100 {
        let alloc_clone = allocator.clone();
        handles.push(tokio::spawn(async move {
            alloc_clone
                .allocate_block()
                .await
                .expect("Concurrent allocation failed")
        }));
    }

    let mut offsets = vec![];
    for handle in handles {
        offsets.push(handle.await.unwrap());
    }

    offsets.sort();
    offsets.dedup();
    assert_eq!(
        offsets.len(),
        100,
        "All concurrent allocations must yield unique block offsets"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_interleaved_alloc_free() {
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_vol_interleaved:free_blocks")
            .arg("test_vol_interleaved:highest_block")
            .query(&mut conn)
            .unwrap_or_default();
    }
    let meta = Arc::new(MetaClient::new_single(client));

    let allocator = Arc::new(
        BlockAllocator::new(meta, "test_vol_interleaved")
            .await
            .unwrap(),
    );

    // Allocate 10 blocks
    let mut offsets = vec![];
    for _ in 0..10 {
        offsets.push(allocator.allocate_block().await.unwrap());
    }

    // Free 5 of them concurrently
    let mut free_handles = vec![];
    for &offset in offsets.iter().take(5) {
        let alloc_clone = allocator.clone();
        free_handles.push(tokio::spawn(async move {
            alloc_clone.free_block(offset).await.unwrap()
        }));
    }

    for handle in free_handles {
        handle.await.unwrap();
    }

    // Exhaust the remaining 246 blocks in the current thread's inline reservoir run
    for _ in 0..246 {
        let _ = allocator.allocate_block().await.unwrap();
    }

    // Allocate 5 more, they should reuse the freed ones
    let mut new_offsets = vec![];
    for _ in 0..5 {
        new_offsets.push(allocator.allocate_block().await.unwrap());
    }

    new_offsets.sort();
    let mut freed_offsets = offsets[0..5].to_vec();
    freed_offsets.sort();

    assert_eq!(new_offsets, freed_offsets, "Freed blocks should be reused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_backend_router_routing() {
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_be_routing:free_blocks")
            .arg("test_be_routing:highest_block")
            .arg("test_be_routing:fabrics02:free_blocks")
            .arg("test_be_routing:fabrics02:highest_block")
            .query(&mut conn)
            .unwrap_or_default();
    }
    let meta = Arc::new(MetaClient::new_single(client));

    // Create temporary backing files for test
    let dev0_path = "/tmp/squeezefs_test_be_routing_dev0";
    let dev1_path = "/tmp/squeezefs_test_be_routing_dev1";
    std::fs::write(dev0_path, vec![0u8; 8 * 1024 * 1024]).unwrap();
    std::fs::write(dev1_path, vec![0u8; 8 * 1024 * 1024]).unwrap();

    let dev0 = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(dev0_path));
    let dev1 = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(dev1_path));

    let alloc0 = Arc::new(
        BlockAllocator::new(meta.clone(), "test_be_routing")
            .await
            .unwrap(),
    );
    let alloc1 = Arc::new(
        BlockAllocator::new(meta.clone(), "test_be_routing:fabrics02")
            .await
            .unwrap(),
    );

    let block_size = Arc::new(std::sync::atomic::AtomicU64::new(4 * 1024 * 1024));
    let router = squeezefs::routing::BackendRouter::new(alloc0, dev0, block_size);

    // Verify initial active backend is backend_0
    let (active_id, _, _) = router.get_active_backend().unwrap();
    assert_eq!(active_id, "backend_0");

    // Register supplementary backend fabrics02
    router.backends.insert(
        "fabrics02".to_string(),
        Arc::new(squeezefs::routing::StorageBackend {
            device: dev1,
            block_allocator: alloc1,
        }),
    );

    // Switch active write backend to fabrics02
    router
        .active_write_backend
        .store(std::sync::Arc::new("fabrics02".to_string()));

    // Verify active backend is fabrics02
    let (active_id, active_alloc, active_dev) = router.get_active_backend().unwrap();
    assert_eq!(active_id, "fabrics02");

    // Write a block to the active backend (fabrics02)
    let offset = active_alloc.allocate_block().await.unwrap();
    let stored_key = format!("fabrics02://{}", offset);

    let payload = vec![42u8; 4 * 1024 * 1024];
    active_dev.write_block(offset, &payload).await.unwrap();

    // Read it back via BackendRouter read_block using the key
    let read_payload = router
        .read_block(&stored_key, 4 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(read_payload.as_ref(), payload.as_slice());

    // Free the block via BackendRouter free_block using the key
    router.free_block(&stored_key).await.unwrap();

    // Clean up files
    let _ = std::fs::remove_file(dev0_path);
    let _ = std::fs::remove_file(dev1_path);
}

#[tokio::test]
async fn test_pipelined_block_free() {
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_vol_pipelining:free_blocks")
            .arg("test_vol_pipelining:highest_block")
            .query(&mut conn)
            .unwrap_or_default();
    }
    let meta = Arc::new(MetaClient::new_single(client));

    let allocator = BlockAllocator::new(meta, "test_vol_pipelining")
        .await
        .expect("Failed to create allocator");

    // Allocate 3 blocks
    let offset1 = allocator.allocate_block().await.unwrap();
    let offset2 = allocator.allocate_block().await.unwrap();
    let offset3 = allocator.allocate_block().await.unwrap();

    // Call pipelined free_blocks
    allocator
        .free_blocks(&[offset1, offset2, offset3])
        .await
        .expect("Failed batch free");

    // Verify they are added to the free blocks set
    let client2 = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let mut conn = client2.get_connection().unwrap();
    let free_count: u64 = redis::cmd("SCARD")
        .arg("test_vol_pipelining:free_blocks")
        .query(&mut conn)
        .unwrap();
    assert_eq!(free_count, 3);
}

#[tokio::test]
async fn test_delete_file_with_fallback() {
    squeezefs::set_fs_prefix("test_del_fallback");
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    {
        let mut conn = client.get_connection().unwrap();
        let _: () = redis::cmd("DEL")
            .arg("test_del_fallback:free_blocks")
            .arg("test_del_fallback:highest_block")
            .arg("test_del_fallback:block_refcounts")
            .arg("test_del_fallback:block_sizes")
            .query(&mut conn)
            .unwrap_or_default();
    }
    let meta = Arc::new(MetaClient::new_single(client));

    let dev0_path = "/tmp/squeezefs_test_del_fallback_dev0";
    std::fs::write(dev0_path, vec![0u8; 8 * 1024 * 1024]).unwrap();

    let dev0 = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(dev0_path));
    let alloc0 = Arc::new(
        BlockAllocator::new(meta.clone(), "test_del_fallback")
            .await
            .unwrap(),
    );

    let _block_size = Arc::new(std::sync::atomic::AtomicU64::new(4 * 1024 * 1024));
    let dlm = squeezefs::dlm::DlmClient::new("redis://127.0.0.1:6379").unwrap();
    let cache = squeezefs::cache::TieredCache::new(
        vec![],
        None,
        None,
        None,
        None,
        dlm.meta_client().clone(),
        alloc0.clone(),
        dev0.clone(),
    )
    .unwrap();
    let router = squeezefs::routing::DataRouter::new(dlm, cache, alloc0, dev0);

    let mut con = router.dlm.get_connection().await.unwrap();
    let file_path = "test_del_file";
    let meta_key = format!("metadata:{}", file_path);
    let block_map_id = "test_map_123";
    let block_map_key = format!("block_map:{}", block_map_id);

    let _: () = redis::cmd("HSET")
        .arg(&meta_key)
        .arg("type")
        .arg("striped")
        .arg("block_map_id")
        .arg(block_map_id)
        .query_async(&mut con)
        .await
        .unwrap();

    let _: () = redis::cmd("HSET")
        .arg(&block_map_key)
        .arg("0")
        .arg("backend_0://0")
        .query_async(&mut con)
        .await
        .unwrap();

    let refcounts_key = "test_del_fallback:block_refcounts";
    let sizes_key = "test_del_fallback:block_sizes";
    let _: () = redis::cmd("HSET")
        .arg(refcounts_key)
        .arg("backend_0://0")
        .arg("1")
        .query_async(&mut con)
        .await
        .unwrap();
    let _: () = redis::cmd("HSET")
        .arg(sizes_key)
        .arg("backend_0://0")
        .arg("4194304")
        .query_async(&mut con)
        .await
        .unwrap();

    // Call delete_file.
    router.delete_file(file_path, &mut con).await.unwrap();

    // Verify metadata and mappings were deleted
    let size_exists: bool = redis::cmd("HEXISTS")
        .arg(sizes_key)
        .arg("backend_0://0")
        .query_async(&mut con)
        .await
        .unwrap();
    assert!(!size_exists);

    let map_exists: bool = redis::cmd("EXISTS")
        .arg(&block_map_key)
        .query_async(&mut con)
        .await
        .unwrap();
    assert!(!map_exists);

    let _ = std::fs::remove_file(dev0_path);
}
