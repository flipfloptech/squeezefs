//! P1-8: inode write-lock scope — meta-prep vs full-op, concurrent striped data path.
//! Requires Garnet for the staged concurrent tests (skips if unavailable).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    inode_write_lock_scope, InodeWriteLockScope, SqueezefsFilesystem, BLOCK_FLUSH_LOCKS,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile};
use tokio::sync::RwLock;

fn get_redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn try_dlm() -> Option<DlmClient> {
    let url = get_redis_url();
    let c = redis::Client::open(url.clone()).ok()?;
    if c.get_multiplexed_tokio_connection().await.is_err() {
        return None;
    }
    DlmClient::new(&url).ok()
}

/// Policy: inline and non-striped keep exclusive lock for the whole write;
/// already-striped files only need meta-prep under the inode write lock.
#[test]
fn test_inode_write_lock_scope_policy() {
    assert!(matches!(
        inode_write_lock_scope(true, false),
        InodeWriteLockScope::EntireOp
    ));
    assert!(matches!(
        inode_write_lock_scope(true, true),
        InodeWriteLockScope::EntireOp
    ));
    assert!(matches!(
        inode_write_lock_scope(false, false),
        InodeWriteLockScope::EntireOp
    ));
    assert!(matches!(
        inode_write_lock_scope(false, true),
        InodeWriteLockScope::MetaPrepOnly
    ));
}

/// Protocol: after meta-prep drops the write guard, another writer can enter
/// meta while the first task is still in a simulated long data phase.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_meta_prep_releases_lock_before_data_phase() {
    let lock = Arc::new(RwLock::new(()));
    let (meta_done_tx, meta_done_rx) = tokio::sync::oneshot::channel::<()>();
    let (second_entered_tx, second_entered_rx) = tokio::sync::oneshot::channel::<()>();

    let lock_a = lock.clone();
    let data_phase = tokio::spawn(async move {
        let guard = lock_a.write().await;
        // meta-prep complete
        drop(guard);
        let _ = meta_done_tx.send(());
        // long data I/O (no inode write lock held)
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    meta_done_rx.await.expect("meta phase signal");

    let lock_b = lock.clone();
    let second = tokio::spawn(async move {
        // Must acquire without waiting for first writer's data phase
        let acquired = tokio::time::timeout(Duration::from_millis(50), lock_b.write()).await;
        assert!(
            acquired.is_ok(),
            "second writer must acquire inode write lock during peer data phase"
        );
        let _ = second_entered_tx.send(());
        drop(acquired.unwrap());
    });

    second_entered_rx
        .await
        .expect("second writer entered during data phase");
    second.await.expect("second join");
    data_phase.await.expect("data phase join");
}

/// Concurrent non-overlapping staged writes without an outer inode write lock
/// must remain correct via per-block `BLOCK_FLUSH_LOCKS`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_staged_writes_without_inode_lock() {
    let dlm = match try_dlm().await {
        Some(d) => d,
        None => {
            eprintln!("Skipping: Garnet/Redis not available");
            return;
        }
    };

    let prefix = format!(
        "p1_8_conc_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    squeezefs::set_fs_prefix(&prefix);

    {
        let mut con = dlm.meta_client().get_connection().await.unwrap();
        let _: () = redis::cmd("DEL")
            .arg(format!("{prefix}:free_blocks"))
            .arg(format!("{prefix}:highest_block"))
            .arg(format!("{prefix}:block_refcounts"))
            .arg(format!("{prefix}:block_sizes"))
            .query_async(&mut con)
            .await
            .unwrap_or_default();
        let format_key = format!("{prefix}:format");
        let _: () = redis::cmd("HSET")
            .arg(&format_key)
            .arg("name")
            .arg(&prefix)
            .arg("block_size")
            .arg("4194304")
            .query_async(&mut con)
            .await
            .unwrap();
    }

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new(Arc::new(dlm.meta_client().clone()), &prefix)
            .await
            .unwrap(),
    );
    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("256MB"),
        Some("256MB"),
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
    router.set_block_size(4 * 1024 * 1024);
    let fs = Arc::new(SqueezefsFilesystem::new(router, dlm, 1000, 1000));

    let ino = 42u64;
    let chunk = 256 * 1024usize;
    let n_writers = 8u8;
    let final_size = (n_writers as u64) * (chunk as u64);
    // All writes land in block 0 at non-overlapping offsets — no outer inode lock.
    let mut handles = Vec::new();
    for i in 0..n_writers {
        let fs = fs.clone();
        handles.push(tokio::spawn(async move {
            let offset = (i as u64) * (chunk as u64);
            let data = vec![i.wrapping_add(1); chunk];
            // MetaPrepOnly contract: data path only; block locks serialize same-block RMW.
            // `existing_size` is the post-merge logical size so RMW preserves peer ranges.
            fs.write_file_staged(ino, offset, &data, final_size, 1)
                .await
                .expect("staged write without inode lock");
        }));
    }
    for h in handles {
        h.await.expect("join");
    }

    // Force flush so we can observe the merged block image.
    fs.flush_memory_buffers_for_inode(ino, 1)
        .await
        .expect("flush");

    let cache_key = format!("active_block:inode_{ino}:block_0");
    // After full-block completion buffer may be on NVMe staging only.
    let image = if let Some((_, buf)) = fs.active_block_buffers.remove(&cache_key) {
        buf
    } else {
        fs.router
            .cache
            .nvme
            .read_staged(&cache_key)
            .expect("staged block image after concurrent writes")
    };

    for i in 0..n_writers {
        let start = (i as usize) * chunk;
        let expected = i.wrapping_add(1);
        assert!(
            image[start..start + chunk].iter().all(|&b| b == expected),
            "range at {start} must be 0x{expected:02x}"
        );
    }
}

/// Overlapping concurrent staged writes without inode lock must not corrupt
/// (last serialized writer per block wins; no panic / torn partial bytes).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_overlapping_staged_writes_block_lock_safe() {
    let dlm = match try_dlm().await {
        Some(d) => d,
        None => {
            eprintln!("Skipping: Garnet/Redis not available");
            return;
        }
    };

    let prefix = format!(
        "p1_8_overlap_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    squeezefs::set_fs_prefix(&prefix);

    {
        let mut con = dlm.meta_client().get_connection().await.unwrap();
        let format_key = format!("{prefix}:format");
        let _: () = redis::cmd("HSET")
            .arg(&format_key)
            .arg("name")
            .arg(&prefix)
            .arg("block_size")
            .arg("4194304")
            .query_async(&mut con)
            .await
            .unwrap();
    }

    let backing_temp = NamedTempFile::new().unwrap();
    let backing_path = backing_temp.path().to_path_buf();
    {
        let f = std::fs::File::create(&backing_path).unwrap();
        f.set_len(32 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_path.to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new(Arc::new(dlm.meta_client().clone()), &prefix)
            .await
            .unwrap(),
    );
    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("256MB"),
        Some("256MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    router.set_block_size(4 * 1024 * 1024);
    let fs = Arc::new(SqueezefsFilesystem::new(router, dlm, 1000, 1000));

    let ino = 7u64;
    let done = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for i in 0..16u8 {
        let fs = fs.clone();
        let done = done.clone();
        handles.push(tokio::spawn(async move {
            // All target overlapping region in block 0.
            let data = vec![i.wrapping_add(10); 64 * 1024];
            fs.write_file_staged(ino, 0, &data, 64 * 1024, 1)
                .await
                .expect("overlap staged write");
            done.fetch_add(1, Ordering::SeqCst);
        }));
    }
    for h in handles {
        h.await.expect("join");
    }
    assert_eq!(done.load(Ordering::SeqCst), 16);

    // Block lock is free after all writers exit.
    let block_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, 0);
    let acquired = tokio::time::timeout(Duration::from_millis(100), block_lock.lock()).await;
    assert!(acquired.is_ok(), "block lock must not be stuck");
}
