use fuse3::raw::Filesystem;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

fn redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

#[tokio::test]
async fn test_writeback_queue_full_deadlock() {
    let _ = env_logger::builder().is_test(true).try_init();
    // Set default block size to 4096, and writeback queue capacity to 5
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    std::env::set_var("SQUEEZEFS_WRITEBACK_QUEUE_CAP", "5");

    let test_id = "writeback_deadlock_test";
    let dlm = DlmClient::new(&redis_url())
        .unwrap_or_else(|_| DlmClient::new("redis://127.0.0.1:6379").unwrap());

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));

    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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

    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_storage = MetaLvStorage::open(&meta_path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&meta_storage).await.unwrap();
    let meta_backend = Arc::new(MetaLvBackend::new(meta_storage));
    let routed_meta_backend = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed_meta_backend.clone());
    fs.meta_backend = Some(routed_meta_backend);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = fuse3::raw::Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
    };

    // Create file
    let create_res = fs
        .create(
            req,
            1,
            OsStr::new("deadlock_test.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap();
    let child_ino = create_res.attr.ino;

    // 1. Initial write of 4097 bytes to force transition to "striped" without xattr overflow
    let init_data = vec![0u8; 4097];
    fs.write(
        req,
        child_ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&init_data),
        0,
        0,
    )
    .await
    .unwrap();

    // 2. Write 12 chunks of 2048 bytes starting at offset 8192.
    // Every second write completes a block, enqueuing a writeback request.
    // At the 6th completed block write, the queue (cap 5) will be full, triggering synchronous flush.
    let write_fut = async {
        let chunk_size = 2048usize;
        let data = vec![0u8; chunk_size];
        for i in 0..12 {
            let offset = 8192 + (i * chunk_size) as u64;
            if let Err(e) = fs
                .write(
                    req,
                    child_ino,
                    0,
                    offset,
                    bytes::Bytes::copy_from_slice(&data),
                    0,
                    0,
                )
                .await
            {
                panic!("Failed at chunk {} (offset {}): {:?}", i, offset, e);
            }
        }
    };

    let timeout_res = tokio::time::timeout(std::time::Duration::from_secs(3), write_fut).await;
    assert!(
        timeout_res.is_ok(),
        "Test deadlocked: writeback queue-full synchronous flush caused a hang!"
    );
}

#[tokio::test]
async fn test_inline_file_layout_overflow() {
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");

    let test_id = "inline_overflow_test";
    let dlm = DlmClient::new(&redis_url())
        .unwrap_or_else(|_| DlmClient::new("redis://127.0.0.1:6379").unwrap());

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));

    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("32MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_storage = MetaLvStorage::open(&meta_path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&meta_storage).await.unwrap();
    let meta_backend = Arc::new(MetaLvBackend::new(meta_storage));
    let routed_meta_backend = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed_meta_backend.clone());
    fs.meta_backend = Some(routed_meta_backend);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = fuse3::raw::Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
    };

    let create_res = fs
        .create(
            req,
            1,
            OsStr::new("overflow_test.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap();
    let child_ino = create_res.attr.ino;

    // Write a 450-byte block. Under JSON xattr encoding, this will exceed 1024 bytes and fail.
    // Under binary layout encoding, it should succeed since it fits within the xattr.
    let data = vec![0u8; 450];
    fs.write(
        req,
        child_ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&data),
        0,
        0,
    )
    .await
    .expect("Writing 450 bytes inline should succeed");
}

#[tokio::test]
async fn test_indirect_block_map() {
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");

    let test_id = "indirect_map_test";
    let dlm = DlmClient::new(&redis_url())
        .unwrap_or_else(|_| DlmClient::new("redis://127.0.0.1:6379").unwrap());

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));

    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("32MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc.clone(), nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_storage = MetaLvStorage::open(&meta_path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&meta_storage).await.unwrap();
    let meta_backend = Arc::new(MetaLvBackend::new(meta_storage));
    let routed_meta_backend = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed_meta_backend.clone());
    fs.meta_backend = Some(routed_meta_backend);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = fuse3::raw::Request {
        unique: 1,
        uid,
        gid,
        pid: 1234,
    };

    let create_res = fs
        .create(req, 1, OsStr::new("indirect.bin"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap();
    let child_ino = create_res.attr.ino;

    // Start with 0 allocated blocks
    let start_blocks = block_alloc.get_used_blocks();

    // 1. Initial write of 4097 bytes to force transition to "striped"
    let init_data = vec![0u8; 4097];
    fs.write(
        req,
        child_ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&init_data),
        0,
        0,
    )
    .await
    .unwrap();

    // 2. Write 70 distinct blocks starting from block 1 (offset 4096) to block 70 (offset 70 * 4096)
    // Writing 1 byte at each offset will allocate a block (since it is striped write).
    for i in 1..=70 {
        let offset = i * 4096;
        let data = vec![(i % 256) as u8];
        fs.write(
            req,
            child_ino,
            0,
            offset,
            bytes::Bytes::copy_from_slice(&data),
            0,
            0,
        )
        .await
        .unwrap();
    }

    // Force flush all staged data to block storage, which will write the indirect map
    fs.force_flush_all_staged_data().await.unwrap();

    // Remove from cache to force backend read
    let file_path = squeezefs::keys::inode_path(child_ino);
    fs.router.metadata_cache.remove(&file_path);

    let meta = fs.router.fetch_metadata(&file_path).await.unwrap();
    assert!(meta.block_map.is_some());
    let bm = meta.block_map.as_ref().unwrap();
    assert_eq!(bm.len(), 71);
    assert!(meta.block_map_id.is_some());
    let map_id = meta.block_map_id.as_ref().unwrap();
    assert!(
        map_id.starts_with("indirect:"),
        "Expected map_id to start with 'indirect:', got: {}",
        map_id
    );

    // Verify we can read back from block 35 and block 69 correctly
    let read_res_35 = fs.read(req, child_ino, 0, 35 * 4096, 1).await.unwrap();
    assert_eq!(read_res_35.data.as_ref(), &[35]);

    let read_res_69 = fs.read(req, child_ino, 0, 69 * 4096, 1).await.unwrap();
    assert_eq!(read_res_69.data.as_ref(), &[69]);

    // Check used blocks count. It should be 71 data blocks + 1 block for indirect map.
    let used_blocks = block_alloc.get_used_blocks() - start_blocks;
    assert_eq!(
        used_blocks, 72,
        "Expected 72 allocated blocks (71 data + 1 indirect map)"
    );

    // Delete/unlink the file
    fs.unlink(req, 1, OsStr::new("indirect.bin")).await.unwrap();

    // Reclaim/delete file blocks explicitly to trigger cleanup
    let mut meta_connection = dlm.meta_client().get_connection().await.unwrap();
    fs.router
        .delete_file(&file_path, &mut meta_connection)
        .await
        .unwrap();

    // Verify all blocks, including the indirect block, are freed
    let end_blocks = block_alloc.get_used_blocks();
    assert_eq!(
        end_blocks,
        start_blocks,
        "Expected all blocks to be freed, but {} blocks remain",
        end_blocks - start_blocks
    );
}

#[tokio::test]
async fn test_block_allocator_recovery() {
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");

    let dlm = DlmClient::new("local").unwrap();
    let volume_id = "backend_0";
    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), volume_id)
            .await
            .unwrap(),
    );
    let nvme_temp = NamedTempFile::new().unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(nvme_temp.path().to_str().unwrap()));

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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

    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_storage = MetaLvStorage::open(&meta_path, 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&meta_storage).await.unwrap();
    let meta_backend = Arc::new(MetaLvBackend::new(meta_storage.clone()));
    let routed_meta_backend = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend.clone(),
    ]));
    fs.router.set_meta_backend(routed_meta_backend.clone());
    fs.meta_backend = Some(routed_meta_backend);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let req = fuse3::raw::Request {
        unique: 1,
        uid,
        gid,
        pid: 0,
    };

    // Create file
    let create_res = fs
        .create(
            req,
            1,
            OsStr::new("recovery_test.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap();
    let child_ino = create_res.attr.ino;

    // Write 5 blocks of data (offset from 0 to 4 * 4096)
    let data = vec![0u8; 4096];
    for i in 0..5 {
        let offset = i * 4096;
        fs.write(
            req,
            child_ino,
            0,
            offset,
            bytes::Bytes::copy_from_slice(&data),
            0,
            0,
        )
        .await
        .unwrap();
    }

    // Force flush all staged data to block storage
    fs.flush_all_memory_buffers_to_staging().await.unwrap();
    fs.flush_all_staged_blocks_to_backend().await.unwrap();

    // Verify current block allocator has 5 allocated blocks
    assert_eq!(block_alloc.get_used_blocks(), 5);

    // Create a brand new BlockAllocator simulating mount restart
    let new_allocator = BlockAllocator::new(dlm.meta_client().clone(), volume_id)
        .await
        .unwrap();
    assert_eq!(new_allocator.get_used_blocks(), 0);

    // Run recovery
    new_allocator
        .recover_active_blocks(&meta_storage, &fs.router.backend_router)
        .await
        .unwrap();

    // Check if new allocator successfully recovered the 5 blocks
    assert_eq!(new_allocator.get_used_blocks(), 5);

    // Verify the recovered free list contains block indices that do NOT overlap with recovered ones
    let free_blocks = new_allocator.get_free_blocks().await.unwrap();
    for i in 0..5 {
        assert!(
            !free_blocks.contains(&i),
            "Block {} should not be in the free list",
            i
        );
    }
}
