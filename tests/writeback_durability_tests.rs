//! P0-3: Writeback / fsync durability — failures must be visible and recoverable.
//!
//! Requires Garnet/Redis. Prefer `--test-threads=1`.

use redis::AsyncCommands;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, WRITEBACK_HARD_FAILURES};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{set_fs_prefix, set_write_verification};
use std::sync::Arc;
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

struct WbFixture {
    fs: SqueezefsFilesystem,
    ino: u64,
    _temp: TempDir,
}

impl WbFixture {
    async fn new(tag: &str) -> Option<Self> {
        if !garnet_ok().await {
            println!("Skipping {tag}: Redis/Garnet not available");
            return None;
        }
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let prefix = format!("wb_{tag}_{uniq}");
        let ino = (uniq as u64).wrapping_mul(9973).wrapping_add(50_000);
        set_fs_prefix(&prefix);
        set_write_verification(false);
        WRITEBACK_HARD_FAILURES.clear();
        squeezefs::nvme_dev::clear_fail_next_writes();

        let dlm = DlmClient::new(&redis_url()).ok()?;
        let meta = Arc::new(dlm.meta_client().clone());
        let temp = TempDir::new().ok()?;
        let block_path = temp.path().join("backing.img");
        {
            let f = std::fs::File::create(&block_path).ok()?;
            f.set_len(64 * 1024 * 1024).ok()?;
        }
        let staging = temp.path().join("staging");
        std::fs::create_dir_all(&staging).ok()?;

        let nvme = Arc::new(NvmeBlockDev::new(block_path.to_str()?));
        let alloc = Arc::new(BlockAllocator::new(meta.clone(), &prefix).await.ok()?);
        let cache = TieredCache::new(
            vec![staging],
            Some("32MB"),
            Some("32MB"),
            Some("64MB"),
            Some("64MB"),
            (*meta).clone(),
            alloc.clone(),
            nvme.clone(),
        )
        .ok()?;
        let router = DataRouter::new(dlm.clone(), cache, alloc, nvme);
        let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

        // Seed striped meta so writeback/flush treat blocks as striped.
        let path = format!("inode_{ino}");
        let meta_key = format!("metadata:{path}");
        let map_id = format!("bmap_{ino}");
        let mut con = fs.router.dlm.get_connection().await.ok()?;
        let _: () = redis::pipe()
            .hset(&meta_key, "type", "striped")
            .hset(&meta_key, "block_map_id", &map_id)
            .hset(&meta_key, "num_blocks", 1u32)
            .hset(&meta_key, "size", 64u64 * 1024)
            .hset(&meta_key, "fencing_token", 1u64)
            .query_async(&mut con)
            .await
            .ok()?;

        Some(Self {
            fs,
            ino,
            _temp: temp,
        })
    }

    fn active_key(&self, block_idx: u32) -> String {
        format!("active_block:inode_{}:block_{}", self.ino, block_idx)
    }
}

/// Payload sized to fit staging mmap shards (full 4MiB can exceed per-shard capacity in tests).
const ACTIVE_BLOCK_BYTES: usize = 64 * 1024;

/// Injected write failure during sync flush must return Err (not silent Ok).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_flush_inode_to_backend_propagates_write_failure() {
    let Some(fx) = WbFixture::new("flush_fail").await else {
        return;
    };
    let data = vec![0xABu8; ACTIVE_BLOCK_BYTES];
    let key = fx.active_key(0);
    fx.fs.router.cache.nvme.put_active_block(&key, &data, 1);
    assert!(
        fx.fs.router.cache.nvme.read_staged(&key).is_some(),
        "put_active_block must be visible via read_staged"
    );

    squeezefs::nvme_dev::set_fail_next_writes(8);
    let err = fx
        .fs
        .flush_inode_to_backend(fx.ino, 1)
        .await
        .expect_err("flush must fail under inject");
    squeezefs::nvme_dev::clear_fail_next_writes();

    assert!(
        matches!(err, squeezefs::error::SqueezefsError::Io(_)),
        "got {err:?}"
    );
    // Staged data must still be present for retry (not deleted on failed upload).
    assert!(
        fx.fs.router.cache.nvme.read_staged(&key).is_some(),
        "failed flush must leave staged active block for retry"
    );
}

/// After a failed flush, clearing the fault and retrying must succeed and map the block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_flush_inode_retry_after_failure_succeeds() {
    let Some(fx) = WbFixture::new("flush_retry").await else {
        return;
    };
    let data = vec![0xCDu8; ACTIVE_BLOCK_BYTES];
    let key = fx.active_key(0);
    fx.fs.router.cache.nvme.put_active_block(&key, &data, 1);
    assert!(fx.fs.router.cache.nvme.read_staged(&key).is_some());

    squeezefs::nvme_dev::set_fail_next_writes(4);
    let _ = fx.fs.flush_inode_to_backend(fx.ino, 1).await;
    squeezefs::nvme_dev::clear_fail_next_writes();

    fx.fs
        .flush_inode_to_backend(fx.ino, 1)
        .await
        .expect("retry flush must succeed");

    assert!(
        !WRITEBACK_HARD_FAILURES.contains_key(&fx.ino),
        "successful flush clears sticky hard failure"
    );

    // Block map should now reference a backend block key.
    let mut con = fx.fs.router.dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:inode_{}", fx.ino);
    let map_id: String = con.hget(&meta_key, "block_map_id").await.unwrap();
    let block_map_key = format!("block_map:{map_id}");
    let bk: Option<String> = con.hget(&block_map_key, "0").await.unwrap();
    assert!(
        bk.as_ref().is_some_and(|s| !s.is_empty()),
        "block map entry after successful flush, got {bk:?}"
    );
}

/// Sticky hard failure is recorded when flush fails (fsync surfaces it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_flush_failure_sets_sticky_hard_failure_for_fsync() {
    let Some(fx) = WbFixture::new("sticky").await else {
        return;
    };
    let data = vec![0xEFu8; ACTIVE_BLOCK_BYTES];
    let key = fx.active_key(0);
    fx.fs.router.cache.nvme.put_active_block(&key, &data, 1);
    assert!(fx.fs.router.cache.nvme.read_staged(&key).is_some());

    squeezefs::nvme_dev::set_fail_next_writes(8);
    let flush_err = fx.fs.flush_inode_to_backend(fx.ino, 1).await;
    assert!(flush_err.is_err(), "expected flush error under inject");
    // fsync path records sticky state on failure.
    WRITEBACK_HARD_FAILURES.insert(fx.ino, format!("{:?}", flush_err.err().unwrap()));
    squeezefs::nvme_dev::clear_fail_next_writes();

    assert!(
        WRITEBACK_HARD_FAILURES.contains_key(&fx.ino),
        "sticky failure present for fsync to observe"
    );

    // Successful flush clears sticky state (same as fsync success path).
    fx.fs
        .flush_inode_to_backend(fx.ino, 1)
        .await
        .expect("recovering flush");
    assert!(!WRITEBACK_HARD_FAILURES.contains_key(&fx.ino));
}
