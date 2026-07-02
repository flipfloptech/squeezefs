//! P0-4: Crash consistency — DLM leases, fencing tokens, staging recovery.
//!
//! Requires Garnet/Redis. Prefer `--test-threads=1`.

use bytes::Bytes;
use redis::AsyncCommands;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{set_fs_prefix, set_write_verification};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn garnet_ok() -> bool {
    let Ok(c) = redis::Client::open(redis_url()) else {
        return false;
    };
    c.get_multiplexed_tokio_connection().await.is_ok()
}

fn uniq_tag(tag: &str) -> (String, u64) {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (
        format!("cc_{tag}_{n}"),
        (n as u64).wrapping_mul(7919).wrapping_add(70_000),
    )
}

/// After Redis lock key is deleted (TTL / kill simulation), a second client can
/// acquire a higher fencing token; the first client's lower token must fail writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_expired_lease_allows_new_fence_stale_write_rejected() {
    if !garnet_ok().await {
        println!("Skipping: Garnet unavailable");
        return;
    }
    let (prefix, ino) = uniq_tag("lease_expire");
    set_fs_prefix(&prefix);
    set_write_verification(false);

    let dlm_a = DlmClient::new(&redis_url()).unwrap();
    let dlm_b = DlmClient::new(&redis_url()).unwrap();
    let path = format!("inode_{ino}");

    let lease_a = dlm_a
        .acquire_lock(&path, None, Duration::from_secs(30))
        .await
        .expect("client A acquires");
    let token_a = lease_a.fencing_token();
    assert!(lease_a.is_held().await);

    // Simulate crash: lock key gone without graceful release.
    {
        let mut con = dlm_a.meta_client().get_connection().await.unwrap();
        let _: () = redis::cmd("DEL")
            .arg(lease_a.lock_key())
            .query_async(&mut con)
            .await
            .unwrap();
    }
    assert!(
        !lease_a.is_held().await,
        "after DEL, lease must report not held"
    );

    let lease_b = dlm_b
        .acquire_lock(&path, None, Duration::from_secs(30))
        .await
        .expect("client B acquires after A expired");
    let token_b = lease_b.fencing_token();
    assert!(token_b > token_a, "B must get a newer fencing token");

    // Build a minimal router to exercise write fencing.
    let temp = TempDir::new().unwrap();
    let block_path = temp.path().join("b.img");
    {
        let f = std::fs::File::create(&block_path).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    let meta = Arc::new(dlm_a.meta_client().clone());
    let nvme = Arc::new(NvmeBlockDev::new(block_path.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(meta.clone(), &prefix).await.unwrap());
    let cache = TieredCache::new(
        vec![],
        Some("16MB"),
        Some("16MB"),
        Some("16MB"),
        Some("16MB"),
        (*meta).clone(),
        alloc.clone(),
        nvme.clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm_a.clone(), cache, alloc, nvme);

    // B commits a write with the new token first.
    router
        .write_file(&path, 0, Bytes::from_static(b"from-B"), token_b)
        .await
        .expect("B write");

    // Stale A token must be rejected.
    let err = router
        .write_file(&path, 0, Bytes::from_static(b"from-A-stale"), token_a)
        .await
        .expect_err("stale fence rejected");
    assert!(matches!(
        err,
        squeezefs::error::SqueezefsError::FencingTokenExpired { .. }
    ));

    let _ = lease_b.release().await;
    // lease_a Drop may race; force drop without release ownership.
    drop(lease_a);
}

/// After lock key loss, `is_held` is false and a new acquisition gets a higher fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lease_is_held_false_after_lock_key_deleted_then_reacquire() {
    if !garnet_ok().await {
        println!("Skipping: Garnet unavailable");
        return;
    }
    let (prefix, ino) = uniq_tag("reval");
    set_fs_prefix(&prefix);

    let dlm = DlmClient::new(&redis_url()).unwrap();
    let path = format!("inode_{ino}");
    let lease = dlm
        .acquire_lock(&path, None, Duration::from_secs(30))
        .await
        .unwrap();
    let token1 = lease.fencing_token();
    assert!(lease.is_held().await);

    let mut con = dlm.meta_client().get_connection().await.unwrap();
    let _: () = redis::cmd("DEL")
        .arg(lease.lock_key())
        .query_async(&mut con)
        .await
        .unwrap();
    assert!(
        !lease.is_held().await,
        "lost Redis lock must not report held"
    );

    let lease2 = dlm
        .acquire_lock(&path, None, Duration::from_secs(30))
        .await
        .expect("re-acquire after loss");
    assert!(lease2.fencing_token() > token1);
    let _ = lease2.release().await;
    drop(lease);
}

/// recover_staging discards staged data with fencing_token < Garnet metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_recover_staging_discards_stale_fence() {
    if !garnet_ok().await {
        println!("Skipping: Garnet unavailable");
        return;
    }
    let (prefix, ino) = uniq_tag("rec_stale");
    set_fs_prefix(&prefix);
    set_write_verification(false);

    let dlm = DlmClient::new(&redis_url()).unwrap();
    let temp = TempDir::new().unwrap();
    let block_path = temp.path().join("b.img");
    {
        let f = std::fs::File::create(&block_path).unwrap();
        f.set_len(32 * 1024 * 1024).unwrap();
    }
    let staging_root = temp.path().join("staging_root");
    let segment = staging_root.join("staging_segment");
    std::fs::create_dir_all(&segment).unwrap();

    let meta = Arc::new(dlm.meta_client().clone());
    let nvme = Arc::new(NvmeBlockDev::new(block_path.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(meta.clone(), &prefix).await.unwrap());
    let cache = TieredCache::new(
        vec![staging_root.clone()],
        Some("16MB"),
        Some("16MB"),
        Some("32MB"),
        Some("32MB"),
        (*meta).clone(),
        alloc.clone(),
        nvme.clone(),
    )
    .unwrap();

    let path = format!("inode_{ino}");
    let file_id = format!("fid_{ino}_stale");
    let payload = vec![0x11u8; 4096];

    // Stage with old fencing token 1.
    cache
        .nvme
        .stage_write(&path, &file_id, &payload, 1)
        .await
        .expect("stage_write");

    // Garnet says staged but with higher fence (another client already won).
    let mut con = meta.get_connection().await.unwrap();
    let meta_key = format!("metadata:{path}");
    let _: () = redis::pipe()
        .hset(&meta_key, "type", "staged")
        .hset(&meta_key, "file_id", &file_id)
        .hset(&meta_key, "fencing_token", 99u64)
        .hset(&meta_key, "size", payload.len() as u64)
        .query_async(&mut con)
        .await
        .unwrap();

    // Release live mmap so recover can open segment files (simulates remount).
    drop(cache);

    let n = squeezefs::recovery::recover_staging(&staging_root, meta.as_ref(), &alloc, &nvme)
        .await
        .expect("recover_staging");
    assert_eq!(n, 0, "stale fencing must not recover (got {n} recoveries)");
}

/// recover_staging commits staged-but-uncommitted data when fence matches meta.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_recover_staging_commits_matching_uncommitted() {
    if !garnet_ok().await {
        println!("Skipping: Garnet unavailable");
        return;
    }
    let (prefix, ino) = uniq_tag("rec_ok");
    set_fs_prefix(&prefix);
    set_write_verification(false);

    let dlm = DlmClient::new(&redis_url()).unwrap();
    let temp = TempDir::new().unwrap();
    let block_path = temp.path().join("b.img");
    {
        let f = std::fs::File::create(&block_path).unwrap();
        f.set_len(32 * 1024 * 1024).unwrap();
    }
    let staging_root = temp.path().join("staging_root");
    let segment = staging_root.join("staging_segment");
    std::fs::create_dir_all(&segment).unwrap();

    let meta = Arc::new(dlm.meta_client().clone());
    let nvme = Arc::new(NvmeBlockDev::new(block_path.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(meta.clone(), &prefix).await.unwrap());

    let path = format!("inode_{ino}");
    let file_id = format!("fid_{ino}_ok");
    let payload = vec![0xABu8; 8192];
    let fence = 7u64;

    // Write binary staged blob with a 1-shard cache matching recover_staging's
    // layout when write_disk_limit < 10MB.
    {
        let cache =
            squeezefs::tiering::nvme::NvmeCache::new(&[&segment], &[8 * 1024 * 1024], 1).unwrap();
        let meta_hdr = squeezefs::cache::nvme::StagedMetadata {
            fencing_token: fence,
            original_size: payload.len() as u64,
            file_path: path.clone(),
        };
        let meta_bytes = meta_hdr.serialize();
        let meta_len = meta_bytes.len() as u64;
        let mut buf = Vec::new();
        buf.extend_from_slice(&meta_len.to_be_bytes());
        buf.extend_from_slice(&meta_bytes);
        buf.extend_from_slice(&payload);
        cache.put(bytes::Bytes::from(file_id.clone()), bytes::Bytes::from(buf));
        drop(cache);
    }

    let mut con = meta.get_connection().await.unwrap();
    let format_key = format!("{prefix}:format");
    let _: () = redis::cmd("HSET")
        .arg(&format_key)
        .arg("write_disk_limit")
        .arg("8MB")
        .query_async(&mut con)
        .await
        .unwrap();
    let meta_key = format!("metadata:{path}");
    let _: () = redis::pipe()
        .hset(&meta_key, "type", "staged")
        .hset(&meta_key, "file_id", &file_id)
        .hset(&meta_key, "fencing_token", fence)
        .hset(&meta_key, "size", payload.len() as u64)
        .query_async(&mut con)
        .await
        .unwrap();

    let n = squeezefs::recovery::recover_staging(&staging_root, meta.as_ref(), &alloc, &nvme)
        .await
        .expect("recover_staging");
    assert_eq!(n, 1, "matching staged entry must recover");

    let mapping_key = format!("mapping:{file_id}");
    let block: Option<String> = con.hget(&mapping_key, "block").await.unwrap();
    assert!(
        block.as_ref().is_some_and(|s| !s.is_empty()),
        "recovery must write mapping, got {block:?}"
    );
}
