use squeezefs::block_allocator::BlockAllocator;
use squeezefs::dlm::MetaClient;
use std::sync::Arc;

// TDD Phase 2: Define tests for Block Allocator

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_allocate_and_free_block() {
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let meta = Arc::new(MetaClient::Single(client));
    
    let allocator = BlockAllocator::new(meta, "test_vol_1").await.expect("Failed to create allocator");

    // 2. Allocate a block
    let offset1 = allocator.allocate_block().await.expect("Failed to allocate block");
    assert_eq!(offset1 % (4 * 1024 * 1024), 0, "Offset must be 4MB aligned");

    let offset2 = allocator.allocate_block().await.expect("Failed to allocate second block");
    assert_ne!(offset1, offset2, "Offsets must be unique");

    // 3. Free the first block
    allocator.free_block(offset1).await.expect("Failed to free block");

    // 4. Re-allocate
    let offset3 = allocator.allocate_block().await.expect("Failed to allocate block after free");
    assert_ne!(offset2, offset3, "Re-allocated offset must not overlap with active blocks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_allocations() {
    let client = redis::Client::open("redis://127.0.0.1:6379").unwrap();
    let meta = Arc::new(MetaClient::Single(client));
    
    let allocator = Arc::new(BlockAllocator::new(meta, "test_vol_concurrent").await.unwrap());

    let mut handles = vec![];
    for _ in 0..100 {
        let alloc_clone = allocator.clone();
        handles.push(tokio::spawn(async move {
            alloc_clone.allocate_block().await.expect("Concurrent allocation failed")
        }));
    }

    let mut offsets = vec![];
    for handle in handles {
        offsets.push(handle.await.unwrap());
    }

    offsets.sort();
    offsets.dedup();
    assert_eq!(offsets.len(), 100, "All concurrent allocations must yield unique block offsets");
}
