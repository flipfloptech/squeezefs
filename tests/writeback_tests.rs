use fuse3::raw::Filesystem;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

use squeezefs::crypto_compress::CryptoCompressState;
use squeezefs::fuse_client::METRICS;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

#[tokio::test]
async fn test_writeback_queue_full_deadlock() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    // Set default block size to 4096, and writeback queue capacity to 5
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    std::env::set_var("SQUEEZEFS_WRITEBACK_QUEUE_CAP", "5");

    let test_id = "writeback_deadlock_test";
    let dlm = DlmClient::new("local").unwrap();

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
        None,
    )
    .await
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
    let meta_backend = open_v3_meta(&meta_path, 256 * 1024 * 1024).await;
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
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");

    let test_id = "inline_overflow_test";
    let dlm = DlmClient::new("local").unwrap();

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
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_backend = open_v3_meta(&meta_path, 256 * 1024 * 1024).await;
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
async fn test_small_block_map_stays_inline() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");

    let test_id = "indirect_map_test";
    let dlm = DlmClient::new("local").unwrap();

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
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc.clone(), nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_path = meta_temp.path().to_path_buf();
    let meta_backend = open_v3_meta(&meta_path, 256 * 1024 * 1024).await;
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

    // Force flush all staged data to block storage, persisting the layout.
    fs.force_flush_all_staged_data().await.unwrap();

    // Remove from cache to force backend read
    let file_path = squeezefs::keys::inode_path(child_ino);
    fs.router.metadata_cache.remove(&child_ino);

    // §5.3 (PR K8): the spill boundary is the per-volume record cap
    // (~60 KiB serialized at the default node size), not an entry count —
    // a 71-entry map (~1.5 KiB) stays INLINE through the FUSE flush path.
    // The indirect mechanism past the cap is pinned by
    // data_path_correctness_tests::test_v3_spill_boundary_roundtrips_both_directions.
    let meta = fs.router.fetch_metadata(&file_path).await.unwrap();
    assert!(meta.block_map.is_some());
    let bm = meta.block_map.as_ref().unwrap();
    assert_eq!(bm.len(), 71);
    let map_id = meta.block_map_id.as_ref().expect("map id");
    assert!(
        !map_id.starts_with("indirect:"),
        "a 71-entry map must stay inline under the record-cap spill rule, got: {}",
        map_id
    );

    // Verify we can read back from block 35 and block 69 correctly
    let read_res_35 = fs.read(req, child_ino, 0, 35 * 4096, 1, 0).await.unwrap();
    assert_eq!(read_res_35.data.as_ref(), &[35]);

    let read_res_69 = fs.read(req, child_ino, 0, 69 * 4096, 1, 0).await.unwrap();
    assert_eq!(read_res_69.data.as_ref(), &[69]);

    // Check used blocks count: 71 data blocks, NO indirect-map block.
    let used_blocks = block_alloc.get_used_blocks() - start_blocks;
    assert_eq!(
        used_blocks, 71,
        "Expected 71 allocated data blocks (inline map needs no indirect block)"
    );

    // Delete/unlink the file
    fs.unlink(req, 1, OsStr::new("indirect.bin")).await.unwrap();

    // Reclaim/delete file blocks explicitly to trigger cleanup
    let mut meta_connection = dlm.meta_client().get_connection().await.unwrap();
    fs.router
        .delete_file(&file_path, &mut meta_connection)
        .await
        .unwrap();

    // Verify all data blocks are freed
    let end_blocks = block_alloc.get_used_blocks();
    assert_eq!(
        end_blocks,
        start_blocks,
        "Expected all blocks to be freed, but {} blocks remain",
        end_blocks - start_blocks
    );
}

// ---------------------------------------------------------------------------
// PR 3 (docs/design-zero-copy-write-path.md §5.5): zero-copy staged-block
// flush via a write-only guard-backed DMA source.
//
// Contract under test:
// - `NvmeStaging::staged_dma_source` returns a guard-backed view straight
//   over the staging mmap (zero-copy), 4 KiB-aligned for active blocks, so
//   `write_block_from_staging` takes `write_block`'s `WriteData::Aligned`
//   DMA branch (`nvme_unaligned_write_fallbacks` must not move — the PR 2
//   contract detector; today's flush paths copy to a fresh heap `Bytes`
//   first, which misses the aligned branch in test builds, so the counter
//   assertions are RED until the guard-backed source lands).
// - Normative sequencing: the guard is provably dead when the helper
//   returns — a same-shard write-lock op (`remove_active_block`,
//   `put_active_block`) immediately after the DMA must not self-deadlock.
// - A guard-backed `Bytes` never enters any cache: striped flushes skip the
//   read-LRU put entirely; promotion flushes put a REAL copy (its pointer
//   must lie outside the staging mmap value range).
// - `flush_due_active_blocks_for_inode`'s batch futures resolve carrying
//   keys/sizes only — observable as the fsync batch path passing the same
//   sequencing + aligned-DMA assertions.
// - `--write-verification` read-back runs within the stated guard-hold
//   bound: a sampled flush verifies against the guard-backed source and
//   still releases the shard for same-shard mutations afterwards.
// ---------------------------------------------------------------------------

/// `nvme_unaligned_write_fallbacks` is process-global, so every test in this
/// binary that submits `write_block` traffic serializes against the tests
/// that assert counter deltas (same pattern as `tests/nvme_dev_tests.rs`).
static WRITE_SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    WRITE_SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn unaligned_fallbacks() -> u64 {
    METRICS
        .nvme_unaligned_write_fallbacks
        .load(Ordering::Relaxed)
}

/// Direct staging + block-device sandbox (no FUSE layer): the sharpest view
/// of the §5.5 source/helper contract.
struct StagingSandbox {
    nvme: squeezefs::cache::nvme::NvmeStaging,
    dev: Arc<NvmeBlockDev>,
    _backing: NamedTempFile,
    _staging: TempDir,
}

async fn make_staging_sandbox(test_id: &str) -> StagingSandbox {
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    let dev = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("16MB"),
        Some("16MB"),
        dlm.meta_client().clone(),
        ba,
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    StagingSandbox {
        nvme: cache.nvme.clone(),
        dev,
        _backing: backing,
        _staging: staging,
    }
}

/// §5.5 core + the same-shard flush-then-remove sequencing test: the DMA
/// source is guard-backed (zero-copy over the staging mmap), aligned for the
/// `WriteData::Aligned` branch, and provably dead when the helper returns so
/// the same shard can be mutated immediately afterwards.
#[tokio::test]
async fn test_staged_dma_source_is_guard_backed_and_flush_then_remove_sequences() {
    let _serial = serial().await;
    let sb = make_staging_sandbox("dma_source_seq").await;
    let key = squeezefs::keys::active_block(42, 0).to_string();
    let payload: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
    assert!(
        sb.nvme.put_active_block(&key, &payload, 7),
        "staging put refused on an empty pool"
    );

    // Zero-copy: the source must expose the exact mmap bytes the read guard
    // exposes — not a heap copy of them.
    let mmap_addr = {
        let guard = sb
            .nvme
            .read_staged_zero_copy(&key)
            .expect("staged entry must be readable");
        guard.as_ptr() as usize
    };
    let source = sb
        .nvme
        .staged_dma_source(&key)
        .expect("staged_dma_source must resolve a staged active block");
    assert_eq!(
        source.as_ref().as_ptr() as usize,
        mmap_addr,
        "StagedDmaSource must be guard-backed (a view over the staging mmap), not a copy"
    );
    assert_eq!(source.len(), payload.len());
    assert!(!source.is_empty());
    // §5.5 aligned-DMA caveat: active-block staging values sit at a 4 KiB
    // boundary and are whole blocks, so the guard-backed source qualifies
    // for the aligned branch by construction.
    assert_eq!(
        source.as_ref().as_ptr() as usize % 4096,
        0,
        "active-block staging value must be 4 KiB-aligned"
    );
    assert_eq!(
        source.len() % 4096,
        0,
        "active-block staging value must be a whole block (4 KiB multiple)"
    );

    // Passthrough DMA straight off the mmap: must take the aligned branch.
    let crypto = CryptoCompressState::new("none".to_string(), "none".to_string(), None);
    let before = unaligned_fallbacks();
    squeezefs::cache::nvme::write_block_from_staging(&crypto, &sb.dev, 0, source)
        .await
        .expect("guard-backed DMA failed");
    assert_eq!(
        unaligned_fallbacks() - before,
        0,
        "guard-backed staged flush must take write_block's zero-copy WriteData::Aligned branch"
    );

    let read_back = sb.dev.read_block(0, payload.len()).await.unwrap();
    assert_eq!(read_back.as_ref(), &payload[..], "DMA content mismatch");

    // Normative sequencing: the guard died inside the helper, so the
    // same-shard WRITE-lock ops must proceed (a still-live guard-backed
    // Bytes here is exactly the read->write self-deadlock §5.5 closes).
    let nvme = sb.nvme.clone();
    let k = key.clone();
    let removed = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || nvme.remove_active_block(&k)),
    )
    .await
    .expect("same-shard remove_active_block after the DMA self-deadlocked (guard still alive)")
    .expect("remove task panicked")
    .expect("staged entry vanished before removal");
    assert_eq!(removed, payload, "removed staged value mismatch");

    let nvme = sb.nvme.clone();
    let k = key.clone();
    let p = payload.clone();
    let admitted = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || nvme.put_active_block(&k, &p, 8)),
    )
    .await
    .expect("same-shard put_active_block after the DMA self-deadlocked (guard still alive)")
    .expect("put task panicked");
    assert!(
        admitted,
        "shard must stay writable after a guard-backed flush"
    );
}

/// Non-passthrough leg of the normative §5.5 sequence: `process_write`
/// consumes the guard-backed bytes into a fresh transform buffer, so the
/// guard is dead BEFORE the DMA — and the shard is mutable right after.
#[tokio::test]
async fn test_write_block_from_staging_transform_leg_drops_guard_before_dma() {
    let _serial = serial().await;
    let sb = make_staging_sandbox("dma_source_lz4").await;
    let key = squeezefs::keys::active_block(43, 0).to_string();
    // Compressible payload: transform output is a fresh (smaller) buffer.
    let payload = vec![0x5Au8; 65536];
    assert!(sb.nvme.put_active_block(&key, &payload, 7));

    let source = sb
        .nvme
        .staged_dma_source(&key)
        .expect("staged_dma_source must resolve a staged active block");
    let crypto = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
    // The transform is deterministic: compute the expected on-device image
    // from an independent copy of the plaintext.
    let expected = crypto
        .process_write(bytes::Bytes::copy_from_slice(&payload))
        .expect("reference transform failed");
    assert!(expected.len() < payload.len(), "payload must compress");

    squeezefs::cache::nvme::write_block_from_staging(&crypto, &sb.dev, 0, source)
        .await
        .expect("transform-leg staged flush failed");

    // Guard died inside process_write (fresh output buffer) — same-shard
    // mutation must proceed.
    let nvme = sb.nvme.clone();
    let k = key.clone();
    let removed = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || nvme.remove_active_block(&k)),
    )
    .await
    .expect("same-shard remove_active_block after transform flush self-deadlocked")
    .expect("remove task panicked")
    .expect("staged entry vanished before removal");
    assert_eq!(removed, payload);

    // The device holds exactly the transform output (the compressed image),
    // byte-for-byte.
    let raw = sb.dev.read_block(0, expected.len()).await.unwrap();
    assert_eq!(
        raw.as_ref(),
        expected.as_ref(),
        "transform DMA image mismatch"
    );
    let plain = crypto
        .process_read(&expected)
        .expect("reference read-back failed");
    assert_eq!(&plain[..], &payload[..], "transform round-trip mismatch");
}

/// Full FS harness used by the flush-path tests (block size 4096, so a
/// 4097-byte write yields one content-complete staged active block plus a
/// partial RAM tail).
struct FlushHarness {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _backing: NamedTempFile,
    _meta: NamedTempFile,
    _staging: TempDir,
}

async fn make_flush_fs(test_id: &str) -> FlushHarness {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    let dlm = DlmClient::new("local").unwrap();

    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let meta = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(meta.path(), 256 * 1024 * 1024).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = fuse3::raw::Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1234,
    };
    FlushHarness {
        fs,
        req,
        _backing: backing,
        _meta: meta,
        _staging: staging,
    }
}

async fn harness_write(h: &FlushHarness, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn harness_read(h: &FlushHarness, ino: u64, off: u64, len: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} failed: {e:?}"))
        .data
        .to_vec()
}

/// Striped flushes (the writeback path AND the fsync batch stage feeding
/// `flush_due_active_blocks_for_inode` / `upload_single_active_block_data`)
/// must DMA the staged bytes without the audit-#8 heap copy — observable as
/// the aligned-branch counter staying flat across the flush window — and
/// must never put a block into the read LRU (§5.5: guard-backed bytes are
/// barred from every cache; the `!is_striped` gate keeps striped flushes
/// put-free).
#[tokio::test]
async fn test_striped_flush_dma_zero_copy_aligned_no_lru_retention() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial().await;
    let h = make_flush_fs("pr3_striped_flush").await;

    let ino =
        h.fs.create(
            h.req,
            1,
            OsStr::new("pr3_striped.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap()
        .attr
        .ino;

    // Make the file striped first: the first big write of a fresh file
    // routes through the router's direct striped path, not staging.
    let p0: Vec<u8> = (0..8193u32).map(|i| (i % 199) as u8).collect();
    harness_write(&h, ino, 0, &p0).await;

    // The staged shape post-PR 4: content-complete blocks write through and
    // bypass staging, so the staged flush leg is reached via the never-lossy
    // fallback — inject one device-write failure so block 0's write-through
    // degrades into the staging put (+ writeback enqueue); block 1 stays
    // partial in RAM. (FAIL_NEXT_WRITES fires before any aligned-branch
    // accounting, so the counter window below stays clean.)
    let p1: Vec<u8> = (0..4097u32).map(|i| ((i % 97) + 60) as u8).collect();
    squeezefs::nvme_dev::set_fail_next_writes(1);
    harness_write(&h, ino, 0, &p1).await;
    squeezefs::nvme_dev::clear_fail_next_writes();
    let cache_key0 = squeezefs::keys::active_block(ino, 0).to_string();
    assert!(
        h.fs.router.cache.nvme.read_staged(&cache_key0).is_some(),
        "premise: block 0 must be staged by the write-through fallback path"
    );

    // Flush window under measurement: spill the partial tail to staging,
    // then flush every staged active block (deterministic: no background
    // writeback worker without init()).
    let fallbacks_before = unaligned_fallbacks();
    h.fs.flush_all_memory_buffers_to_staging().await.unwrap();
    let summary = h.fs.flush_all_staged_blocks_to_backend().await;
    assert_eq!(summary.attempted, 2, "premise: blocks 0 and 1 staged");
    assert_eq!(
        summary.failed, 0,
        "staged flush failures: {:?}",
        summary.error_samples
    );

    // Zero-copy pin (RED pre-PR 3): every flushed block is a whole, 4 KiB-
    // aligned staging-mmap value in passthrough mode, so every flush DMA must
    // take WriteData::Aligned. The heap-copy flush misses the branch in test
    // builds (system allocator) and bumps the PR 2 contract counter instead.
    assert_eq!(
        unaligned_fallbacks() - fallbacks_before,
        0,
        "staged-block flushes must DMA guard-backed aligned staging memory, \
         not a bounced heap copy (audit #8)"
    );

    // Layout + §5.5 cache rule: striped flushes never enter the read LRU
    // (no plaintext block cached for an already-striped file — neither a
    // copy nor, worse, a guard-backed Bytes).
    let file_path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&file_path).await.unwrap();
    assert_eq!(meta.file_type, "striped");
    let bm = meta
        .block_map
        .clone()
        .expect("striped file must have a map");
    for b in [0u32, 1u32] {
        let k = bm
            .get(&b)
            .unwrap_or_else(|| panic!("block {b} mapping missing"))
            .clone();
        assert!(
            h.fs.router.cache.read_lru.get(&k).is_none(),
            "striped flush must not put block {b} ({k}) into the read LRU"
        );
    }

    // Content: p1 over [0..4097), p0's tail beyond.
    let mut expected = p1.clone();
    expected.extend_from_slice(&p0[4097..]);
    assert_eq!(
        harness_read(&h, ino, 0, 8193).await,
        expected,
        "post-flush content mismatch"
    );

    // The staged active-block entries are consumed by the flush.
    for b in [0u64, 1u64] {
        let key = squeezefs::keys::active_block(ino, b).to_string();
        assert!(
            h.fs.router.cache.nvme.read_staged(&key).is_none(),
            "flushed active block {b} must leave the staging ring"
        );
    }
}

/// Promotion flushes (`!is_striped`) DO seed the read LRU — but with a REAL
/// copy, never the guard-backed staging bytes (an LRU entry has unbounded
/// lifetime; holding the shard read lock through it would block every
/// writer/evictor on that shard). Pointer-range check + a same-key re-stage
/// prove the LRU entry is detached from the staging mmap.
#[tokio::test]
async fn test_promotion_flush_lru_entry_is_real_copy_not_guard_backed() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial().await;
    let h = make_flush_fs("pr3_promotion_flush").await;

    let ino =
        h.fs.create(
            h.req,
            1,
            OsStr::new("pr3_promo.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap()
        .attr
        .ino;

    // The `!is_striped` promotion flush fires when staged active blocks
    // exist while the inode's meta is not (yet) striped — the teardown /
    // meta-flip-race shape. Construct it directly: a staged active block for
    // a still-inline inode, flushed through the public teardown API (which
    // fetches meta per key and passes is_striped = false).
    let p1: Vec<u8> = (0..4096u32).map(|i| (i % 211) as u8).collect();
    let cache_key0 = squeezefs::keys::active_block(ino, 0).to_string();
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    assert!(
        h.fs.router
            .cache
            .nvme
            .put_active_block(&cache_key0, &p1, token),
        "staging put refused on an empty pool"
    );

    // Capture the staging-mmap value range of the staged block BEFORE the
    // flush: any cache entry pointing into this range after the flush is a
    // retained guard — the §5.5 violation.
    let staged_range = {
        let guard =
            h.fs.router
                .cache
                .nvme
                .read_staged_zero_copy(&cache_key0)
                .expect("block 0 must be staged before the flush");
        (guard.as_ptr() as usize, guard.len())
    };

    // Flush of a not-yet-striped inode: the promotion path (LRU seed).
    let summary = h.fs.flush_all_staged_blocks_to_backend().await;
    assert_eq!(summary.attempted, 1, "premise: exactly one staged block");
    assert_eq!(
        summary.failed, 0,
        "promotion flush failures: {:?}",
        summary.error_samples
    );

    let file_path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&file_path).await.unwrap();
    let bm = meta
        .block_map
        .clone()
        .expect("block map missing after flush");
    let k0 = bm.get(&0).expect("block 0 mapping missing").clone();

    let cached =
        h.fs.router
            .cache
            .read_lru
            .get(&k0)
            .expect("promotion flush must seed the read LRU with block 0");
    assert_eq!(cached.as_ref(), &p1[..], "LRU copy content mismatch");
    let cached_addr = cached.as_ptr() as usize;
    let (start, len) = staged_range;
    assert!(
        cached_addr < start || cached_addr >= start + len,
        "read-LRU entry for {k0} aliases the staging mmap \
         (guard-backed Bytes entered a cache — §5.5 violation)"
    );

    // With the LRU entry still alive, the SAME staging key (same shard by
    // construction) must accept a write-lock op promptly: if the LRU held
    // the guard, this would stall until eviction.
    let nvme = h.fs.router.cache.nvme.clone();
    let key0 = squeezefs::keys::active_block(ino, 0).to_string();
    let payload = vec![0xEEu8; 4096];
    let admitted = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || nvme.put_active_block(&key0, &payload, 99)),
    )
    .await
    .expect("same-shard put stalled while a flushed block sat in the read LRU")
    .expect("put task panicked");
    assert!(admitted, "re-stage after promotion flush refused");
}

/// `--write-verification` read-back is part of the stated §5.5 guard-hold
/// bound: a sampled flush verifies the DMA against the caller's (guard-
/// backed) bytes and must still take the aligned branch, round-trip
/// byte-exact, and release the shard afterwards.
#[tokio::test]
async fn test_flush_write_verification_readback_runs_within_guard_hold() {
    let _ = env_logger::builder().is_test(true).try_init();
    let _serial = serial().await;
    let h = make_flush_fs("pr3_verified_flush").await;

    let ino =
        h.fs.create(
            h.req,
            1,
            OsStr::new("pr3_verify.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap()
        .attr
        .ino;

    // Striped file with a staged complete block 0 (via the write-through
    // fallback, post-PR 4) + partial RAM block 1 (same staged shape as the
    // striped-flush test).
    let p0: Vec<u8> = (0..8193u32).map(|i| (i % 223) as u8).collect();
    harness_write(&h, ino, 0, &p0).await;
    let p1: Vec<u8> = (0..4097u32).map(|i| ((i % 113) + 5) as u8).collect();
    squeezefs::nvme_dev::set_fail_next_writes(1);
    harness_write(&h, ino, 0, &p1).await;
    squeezefs::nvme_dev::clear_fail_next_writes();

    // Every flush write from here runs the sampled read-back verify against
    // the caller's (guard-backed) payload.
    squeezefs::set_write_verification(true);
    squeezefs::set_write_verification_sample_rate(1);

    h.fs.flush_all_memory_buffers_to_staging().await.unwrap();
    let fallbacks_before = unaligned_fallbacks();
    let summary = h.fs.flush_all_staged_blocks_to_backend().await;
    squeezefs::set_write_verification(false);
    assert_eq!(summary.attempted, 2, "premise: blocks 0 and 1 staged");
    assert_eq!(
        summary.failed, 0,
        "verified flush failures: {:?}",
        summary.error_samples
    );

    // RED pre-PR 3 for the same reason as the striped test: the flush DMA
    // must come straight from aligned staging memory even when write_block
    // keeps the payload alive for the sampled read-back verify.
    assert_eq!(
        unaligned_fallbacks() - fallbacks_before,
        0,
        "verified staged flush must still take the aligned zero-copy DMA branch"
    );

    let mut expected = p1.clone();
    expected.extend_from_slice(&p0[4097..]);
    assert_eq!(
        harness_read(&h, ino, 0, 8193).await,
        expected,
        "verified flush content mismatch"
    );

    // Guard released after write+verify: the staging entries are gone and
    // the shard accepts writes.
    for b in 0..2u64 {
        let key = squeezefs::keys::active_block(ino, b).to_string();
        assert!(
            h.fs.router.cache.nvme.read_staged(&key).is_none(),
            "verified flush left block {b} in the staging ring"
        );
    }
    let nvme = h.fs.router.cache.nvme.clone();
    let key = squeezefs::keys::active_block(ino, 0).to_string();
    let payload = vec![0x77u8; 4096];
    let admitted = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || nvme.put_active_block(&key, &payload, 100)),
    )
    .await
    .expect("shard write stalled after a verified flush (guard leaked past verify)")
    .expect("put task panicked");
    assert!(admitted);
}

#[tokio::test]
async fn test_block_allocator_recovery() {
    let _serial = serial().await;
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
        None,
    )
    .await
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
    let meta_backend = open_v3_meta(&meta_path, 256 * 1024 * 1024).await;
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
    let summary = fs.flush_all_staged_blocks_to_backend().await;
    assert_eq!(
        summary.failed, 0,
        "staged flush failures: {:?}",
        summary.error_samples
    );

    // Verify current block allocator has 5 allocated blocks
    assert_eq!(block_alloc.get_used_blocks(), 5);

    // Create a brand new BlockAllocator simulating mount restart
    let new_allocator = BlockAllocator::new(dlm.meta_client().clone(), volume_id)
        .await
        .unwrap();
    assert_eq!(new_allocator.get_used_blocks(), 0);

    // Run recovery (the live-inode-tree walk).
    new_allocator
        .recover_active_blocks_v3(&meta_backend, &fs.router.backend_router)
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

/// Superseded-token writeback units must ADOPT the current fencing epoch and
/// merge — never spin forever with a dead token and never leak a published
/// block per attempt (the generic/074-shape strand: release() spawns the
/// background flush with token T and drops the lease; the next open bumps to
/// T+1..T+n; every queued unit's merge then fails FencingTokenExpired while
/// its staged bytes — the ONLY durable-path copy of ACKED data — sit
/// stranded, and each retry's published-then-unmerged block leaks device
/// space).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stale_token_writeback_adopts_current_epoch_no_leak() {
    let _serial = serial().await;
    let _ = env_logger::builder().is_test(true).try_init();
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    std::env::remove_var("SQUEEZEFS_WRITEBACK_QUEUE_CAP");

    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "stale_token_wb")
            .await
            .unwrap(),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let meta_temp = NamedTempFile::new().unwrap();
    let meta_backend = open_v3_meta(meta_temp.path(), 256 * 1024 * 1024).await;
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = fuse3::raw::Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };

    let ino = fs
        .create(
            req,
            1,
            OsStr::new("stale_tok.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap()
        .attr
        .ino;

    // Striped file: two full 4 KiB blocks through the write path (write-
    // through stages + enqueues writeback units carrying the CURRENT token).
    let data = vec![0xAAu8; 8192 + 1];
    fs.write(req, ino, 0, 0, bytes::Bytes::copy_from_slice(&data), 0, 0)
        .await
        .unwrap();

    // Supersede the token HARD: burn 40 lease epochs (each acquire bumps the
    // fencing counter), exactly the release/reopen churn shape.
    let path = squeezefs::keys::inode_path(ino);
    fs.invalidate_local_lease(ino);
    for _ in 0..40 {
        let l = dlm
            .acquire_lock(&path, None, Duration::from_secs(5))
            .await
            .unwrap();
        drop(l);
    }

    // fsync drives the staged blocks durable through the flush units. With
    // the dead-token strand, this either errors or leaves the map short and
    // leaks one published block per retry.
    fs.fsync(req, ino, 0, false).await.expect("fsync clean");

    // Every staged block must have merged into the durable map.
    let meta = fs.router.fetch_metadata(&path).await.unwrap();
    let mapped = meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0);
    assert!(
        mapped >= 2,
        "staged blocks stranded by dead-token writeback: only {mapped} of >=2 merged"
    );

    // No per-retry leak: used blocks == mapped blocks (each block exactly one
    // device allocation; nothing published-but-unmerged left behind).
    let used = ba.get_used_blocks();
    assert!(
        used <= mapped as u64 + 1,
        "leaked published-but-unmerged blocks: used={used} mapped={mapped}"
    );
}
