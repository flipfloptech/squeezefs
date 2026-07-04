//! Regression/unit test for hybrid write routing in FUSE write path.
//!
//! Aligned writes (offset and size multiples of block size) on striped files
//! must bypass staging and write directly to the backend. Unaligned writes
//! must go through the staging cache.
//!
//! Requires a live Garnet/Redis instance. Prefer `--test-threads=1`.

use redis::AsyncCommands;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{set_fs_prefix, set_write_verification};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_hybrid_write_routing_paths() {
    let dlm = match try_dlm().await {
        Some(d) => d,
        None => {
            eprintln!("Skipping: Garnet/Redis not available");
            return;
        }
    };

    let prefix = format!(
        "hybrid_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    set_fs_prefix(&prefix);
    set_write_verification(false);

    // Format metadata settings in Redis
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
            .arg("4194304") // 4MB block size
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
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let ino = 101u64;
    let file_path = squeezefs::keys::inode_path(ino);
    let meta_key = squeezefs::keys::metadata_for_inode(ino);
    let attr_key = squeezefs::keys::attr(ino);
    let bmap_id = format!("bmap_{ino}");

    // Seed file metadata: striped layout, size = 4MB, fencing_token = 1
    {
        let mut con = dlm.get_connection().await.unwrap();
        let _: () = redis::pipe()
            .hset(&meta_key, "type", "striped")
            .hset(&meta_key, "block_map_id", &bmap_id)
            .hset(&meta_key, "num_blocks", "1")
            .hset(&meta_key, "size", "4194304")
            .hset(&meta_key, "fencing_token", "1")
            .hset(&attr_key, "size", "4194304")
            .hset(&attr_key, "kind", "1")
            .query_async(&mut con)
            .await
            .unwrap();
    }

    // 1. Perform block-aligned write (size = 4MB, offset = 0)
    // This is block-aligned, so it MUST bypass active_block_buffers / staging cache.
    let payload_4mb = vec![0xAAu8; 4 * 1024 * 1024];
    use fuse3::raw::prelude::Filesystem;

    // Construct dummy Request
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let dummy_req = fuse3::raw::Request {
        unique: 999,
        uid,
        gid,
        pid: 1234,
    };

    let reply = fs
        .write(
            dummy_req,
            ino,
            0,
            0,
            reply_data_slice_helper(&payload_4mb),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(reply.written, 4 * 1024 * 1024);

    // Verify it did not enter staging
    let cache_key = squeezefs::keys::active_block(ino, 0).to_string();
    assert!(!fs.active_block_buffers.contains_key(&cache_key));
    assert!(fs.router.cache.nvme.read_staged(&cache_key).is_none());

    // Verify block map has a backend block key mapped directly
    {
        let mut con = dlm.get_connection().await.unwrap();
        let block_map_key = squeezefs::keys::block_map(&bmap_id);
        let bk: Option<String> = con.hget(&block_map_key, "0").await.unwrap();
        assert!(bk.is_some());
        let block_key = bk.unwrap();
        assert!(!block_key.is_empty());

        // Verify we can read the written data from backend / read_lru
        let read_back = fs.router.read_file(&file_path).await.unwrap();
        assert_eq!(read_back.len(), 4 * 1024 * 1024);
        assert!(read_back.iter().all(|&b| b == 0xAA));
    }

    // 2. Perform an unaligned write (size = 64KB, offset = 0)
    // This is unaligned, so it MUST use write_file_staged and end up in active_block_buffers
    let payload_64kb = vec![0xBBu8; 64 * 1024];
    let reply = fs
        .write(
            dummy_req,
            ino,
            0,
            0,
            reply_data_slice_helper(&payload_64kb),
            0,
            0,
        )
        .await
        .unwrap();
    assert_eq!(reply.written, 64 * 1024);

    // Verify it is staged now
    assert!(fs.active_block_buffers.contains_key(&cache_key));
    let staged_data = fs.active_block_buffers.get(&cache_key).unwrap().clone();
    assert_eq!(staged_data.len(), 4 * 1024 * 1024);
    assert!(staged_data[0..64 * 1024].iter().all(|&b| b == 0xBB));
    // The rest of the block should contain the old 0xAA bytes
    assert!(staged_data[64 * 1024..].iter().all(|&b| b == 0xAA));
}

fn reply_data_slice_helper(data: &[u8]) -> &[u8] {
    data
}
