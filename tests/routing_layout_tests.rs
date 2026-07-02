//! P0-5: Direct DataRouter layout / RMW regression tests.
//!
//! These define the contract for progressive layout (inline / staged / striped),
//! read-after-write integrity, delete (truncate-to-zero data path), and
//! concurrent same-inode writes under FUSE-like serialization.
//!
//! Requires a live Garnet/Redis instance (skips cleanly if unavailable).
//! Prefer: `cargo test --all-features --test routing_layout_tests -- --test-threads=1`

use bytes::Bytes;
use redis::AsyncCommands;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use squeezefs::{set_fs_prefix, set_write_verification};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

fn redis_url() -> String {
    std::env::var("GARNET_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string())
}

async fn garnet_available() -> bool {
    let Ok(client) = redis::Client::open(redis_url()) else {
        return false;
    };
    client.get_multiplexed_tokio_connection().await.is_ok()
}

/// Isolated router fixture: unique fs_prefix + block volume + temp backing file.
struct LayoutFixture {
    router: DataRouter,
    /// Keep temp dir alive for the backing device + optional staging.
    _temp: TempDir,
    fencing: AtomicU64,
    /// Unique inode base so `metadata:inode_*` keys do not collide across tests
    /// (file meta keys are not namespaced by FS_PREFIX).
    ino_base: u64,
    next_ino: AtomicU64,
}

impl LayoutFixture {
    async fn new(test_id: &str, with_staging: bool) -> Option<Self> {
        if !garnet_available().await {
            println!("Skipping {test_id}: Redis/Garnet not available");
            return None;
        }

        // Isolate Garnet keys for this test (global prefix — run with --test-threads=1).
        // Unique suffix avoids cross-run residue of fencing tokens / meta.
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let prefix = format!("layout_{test_id}_{uniq}");
        // Derive a large unique inode space from the same uniqueness source.
        let ino_base = (uniq as u64).wrapping_mul(1_000_003).wrapping_add(10_000);
        set_fs_prefix(&prefix);
        set_write_verification(false);

        let dlm = DlmClient::new(&redis_url()).ok()?;
        let meta = Arc::new(dlm.meta_client().clone());

        // Best-effort wipe of this volume's allocator keys so free list is clean.
        if let Ok(mut con) = meta.get_connection().await {
            let _: () = redis::cmd("DEL")
                .arg(format!("{prefix}:free_blocks"))
                .arg(format!("{prefix}:highest_block"))
                .arg(format!("{prefix}:block_refcounts"))
                .arg(format!("{prefix}:block_sizes"))
                .query_async(&mut con)
                .await
                .unwrap_or_default();
        }

        let temp = TempDir::new().ok()?;
        let block_path = temp.path().join("backing.img");
        {
            let f = std::fs::File::create(&block_path).ok()?;
            // 64 MiB sparse-ish file for several 4 MiB blocks
            f.set_len(64 * 1024 * 1024).ok()?;
        }
        let nvme = Arc::new(NvmeBlockDev::new(block_path.to_str()?));
        let alloc = Arc::new(
            BlockAllocator::new(meta.clone(), &prefix)
                .await
                .expect("block allocator"),
        );

        let staging_dirs = if with_staging {
            let s = temp.path().join("staging");
            std::fs::create_dir_all(&s).ok()?;
            vec![s]
        } else {
            vec![]
        };

        let cache = TieredCache::new(
            staging_dirs,
            Some("32MB"),
            Some("32MB"),
            Some("64MB"),
            Some("64MB"),
            (*meta).clone(),
            alloc.clone(),
            nvme.clone(),
        )
        .expect("tiered cache");

        let router = DataRouter::new(dlm, cache, alloc, nvme);
        Some(Self {
            router,
            _temp: temp,
            fencing: AtomicU64::new(1),
            ino_base,
            next_ino: AtomicU64::new(0),
        })
    }

    fn next_fence(&self) -> u64 {
        self.fencing.fetch_add(1, Ordering::SeqCst)
    }

    /// Allocate a unique `inode_<n>` path for this fixture run.
    fn path(&self) -> String {
        let n = self.next_ino.fetch_add(1, Ordering::SeqCst);
        format!("inode_{}", self.ino_base.wrapping_add(n))
    }

    async fn layout_type(&self, file_path: &str) -> Option<String> {
        let mut con = self.router.dlm.get_connection().await.ok()?;
        let meta_key = format!("metadata:{file_path}");
        con.hget(&meta_key, "type").await.ok()?
    }

    async fn meta_size(&self, file_path: &str) -> Option<u64> {
        let mut con = self.router.dlm.get_connection().await.ok()?;
        let meta_key = format!("metadata:{file_path}");
        con.hget(&meta_key, "size").await.ok()?
    }
}

// ---------------------------------------------------------------------------
// Happy path: progressive layouts
// ---------------------------------------------------------------------------

/// Micro-file (< 4 KiB) with no staging must use **inline** layout and round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_write_read_inline_layout() {
    let Some(fx) = LayoutFixture::new("inline", false).await else {
        return;
    };
    let path = fx.path();
    let payload = Bytes::from(vec![0xABu8; 512]);

    fx.router
        .write_file(&path, 0, payload.clone(), fx.next_fence())
        .await
        .expect("inline write");

    assert_eq!(
        fx.layout_type(&path).await.as_deref(),
        Some("inline"),
        "sub-4KiB write without staging must be inline"
    );
    assert_eq!(fx.meta_size(&path).await, Some(512));

    let got = fx.router.read_file(&path).await.expect("inline read");
    assert_eq!(got.as_slice(), payload.as_ref());

    let (range, _) = fx
        .router
        .read_file_range_zero_copy(&path, 10, 20)
        .await
        .expect("range read");
    assert_eq!(range.as_ref(), &payload.as_ref()[10..30]);
}

/// With staging dirs, mid-size files (4 KiB < size ≤ 4 MiB) must use **staged** layout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_write_read_staged_layout() {
    let Some(fx) = LayoutFixture::new("staged", true).await else {
        return;
    };
    let path = fx.path();
    // 64 KiB: above inline threshold (4 KiB), at or below staged max (4 MiB)
    let payload = Bytes::from(vec![0xCDu8; 64 * 1024]);

    fx.router
        .write_file(&path, 0, payload.clone(), fx.next_fence())
        .await
        .expect("staged write");

    assert_eq!(
        fx.layout_type(&path).await.as_deref(),
        Some("staged"),
        "mid-size write with staging must be staged"
    );

    let got = fx.router.read_file(&path).await.expect("staged read");
    assert_eq!(got.as_slice(), payload.as_ref());
}

/// Without staging, writes above 4 KiB must use **striped** layout and round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_write_read_striped_layout() {
    let Some(fx) = LayoutFixture::new("striped", false).await else {
        return;
    };
    let path = fx.path();
    // 8 KiB forces stripe threshold when no staging (threshold = 4 KiB)
    let payload = Bytes::from(vec![0xEFu8; 8 * 1024]);

    fx.router
        .write_file(&path, 0, payload.clone(), fx.next_fence())
        .await
        .expect("striped write");

    assert_eq!(
        fx.layout_type(&path).await.as_deref(),
        Some("striped"),
        "over-threshold write without staging must be striped"
    );

    let got = fx.router.read_file(&path).await.expect("striped read");
    assert_eq!(got.len(), payload.len());
    assert_eq!(got.as_slice(), payload.as_ref());
}

// ---------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------

/// Growing an inline file past the no-staging threshold must transition to **striped**.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_inline_to_striped_transition() {
    let Some(fx) = LayoutFixture::new("inline2stripe", false).await else {
        return;
    };
    let path = fx.path();

    let small = Bytes::from(vec![0x11u8; 256]);
    fx.router
        .write_file(&path, 0, small.clone(), fx.next_fence())
        .await
        .expect("initial inline");
    assert_eq!(fx.layout_type(&path).await.as_deref(), Some("inline"));

    // Grow past 4 KiB stripe threshold (no staging)
    let large = Bytes::from(vec![0x22u8; 8 * 1024]);
    fx.router
        .write_file(&path, 0, large.clone(), fx.next_fence())
        .await
        .expect("grow to striped");

    assert_eq!(
        fx.layout_type(&path).await.as_deref(),
        Some("striped"),
        "must transition inline → striped when size exceeds threshold"
    );

    let got = fx
        .router
        .read_file(&path)
        .await
        .expect("read after transition");
    assert_eq!(got.as_slice(), large.as_ref());
}

/// Growing a staged file past 4 MiB must transition to **striped**.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_staged_to_striped_transition() {
    let Some(fx) = LayoutFixture::new("stage2stripe", true).await else {
        return;
    };
    let path = fx.path();

    let mid = Bytes::from(vec![0x33u8; 128 * 1024]);
    fx.router
        .write_file(&path, 0, mid.clone(), fx.next_fence())
        .await
        .expect("initial staged");
    assert_eq!(fx.layout_type(&path).await.as_deref(), Some("staged"));

    // Cross 4 MiB staged max → striped
    let big = Bytes::from(vec![0x44u8; 5 * 1024 * 1024]);
    fx.router
        .write_file(&path, 0, big.clone(), fx.next_fence())
        .await
        .expect("grow to striped");

    assert_eq!(
        fx.layout_type(&path).await.as_deref(),
        Some("striped"),
        "must transition staged → striped when size exceeds 4 MiB"
    );

    let got = fx
        .router
        .read_file(&path)
        .await
        .expect("read after staged→striped");
    assert_eq!(got.len(), big.len());
    assert_eq!(got.as_slice(), big.as_ref());
}

// ---------------------------------------------------------------------------
// RMW (read-modify-write) on striped
// ---------------------------------------------------------------------------

/// Partial overwrite of a striped file must preserve surrounding bytes (POSIX RMW).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_striped_rmw_middle_range() {
    let Some(fx) = LayoutFixture::new("rmw", false).await else {
        return;
    };
    let path = fx.path();

    // Two full 4 MiB logical blocks of deterministic pattern
    let block = 4 * 1024 * 1024usize;
    let mut full = vec![0u8; block * 2];
    for (i, b) in full.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    fx.router
        .write_file(&path, 0, Bytes::from(full.clone()), fx.next_fence())
        .await
        .expect("initial striped write");
    assert_eq!(fx.layout_type(&path).await.as_deref(), Some("striped"));

    // Overwrite 1 KiB in the middle of the first block
    let offset = 1024u64;
    let patch = Bytes::from(vec![0xFFu8; 1024]);
    fx.router
        .write_file(&path, offset, patch.clone(), fx.next_fence())
        .await
        .expect("RMW patch");

    let got = fx.router.read_file(&path).await.expect("read after RMW");
    assert_eq!(got.len(), full.len());

    // Before patch
    assert_eq!(&got[..offset as usize], &full[..offset as usize]);
    // Patch
    assert_eq!(
        &got[offset as usize..offset as usize + 1024],
        patch.as_ref()
    );
    // After patch
    assert_eq!(
        &got[offset as usize + 1024..],
        &full[offset as usize + 1024..]
    );
}

/// RMW that spans a block boundary must update both blocks correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_striped_rmw_cross_block_boundary() {
    let Some(fx) = LayoutFixture::new("rmw_xblock", false).await else {
        return;
    };
    let path = fx.path();
    let block = 4 * 1024 * 1024usize;
    let full = vec![0x55u8; block * 2];
    fx.router
        .write_file(&path, 0, Bytes::from(full.clone()), fx.next_fence())
        .await
        .expect("initial");

    // 512 bytes before boundary + 512 after
    let offset = (block - 512) as u64;
    let patch = Bytes::from(vec![0xAAu8; 1024]);
    fx.router
        .write_file(&path, offset, patch.clone(), fx.next_fence())
        .await
        .expect("cross-block RMW");

    let got = fx.router.read_file(&path).await.expect("read");
    assert_eq!(
        &got[offset as usize..offset as usize + 1024],
        patch.as_ref()
    );
    assert_eq!(&got[..offset as usize], &full[..offset as usize]);
    assert_eq!(
        &got[offset as usize + 1024..],
        &full[offset as usize + 1024..]
    );
}

// ---------------------------------------------------------------------------
// Delete / truncate-to-zero data path (router side of FUSE setattr size=0)
// ---------------------------------------------------------------------------

/// `delete_file` must remove layout metadata so a subsequent write can start clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_file_after_striped_then_rewrite() {
    let Some(fx) = LayoutFixture::new("delete", false).await else {
        return;
    };
    let path = fx.path();
    let payload = Bytes::from(vec![0x77u8; 16 * 1024]);
    fx.router
        .write_file(&path, 0, payload.clone(), fx.next_fence())
        .await
        .expect("write striped");
    assert_eq!(fx.layout_type(&path).await.as_deref(), Some("striped"));

    {
        let mut con = fx
            .router
            .dlm
            .get_connection()
            .await
            .expect("conn for delete");
        fx.router
            .delete_file(&path, &mut con)
            .await
            .expect("delete_file");
        // Mirror FUSE truncate-to-zero meta reset (setattr path).
        let meta_key = format!("metadata:{path}");
        let _: () = redis::pipe()
            .hset(&meta_key, "type", "inline")
            .hset(&meta_key, "size", 0u64)
            .query_async(&mut con)
            .await
            .expect("reset meta after delete");
        fx.router.metadata_cache.invalidate(&path);
        fx.router.cache.write_lru.remove(&path);
        fx.router.cache.read_lru.remove(&path);
    }

    // Fresh small write after truncate-to-zero path must be inline again.
    let again = Bytes::from_static(b"after-delete");
    fx.router
        .write_file(&path, 0, again.clone(), fx.next_fence())
        .await
        .expect("rewrite after delete");

    assert_eq!(
        fx.layout_type(&path).await.as_deref(),
        Some("inline"),
        "after delete + meta reset, small write should be inline"
    );
    let got = fx.router.read_file(&path).await.expect("read rewrite");
    assert_eq!(got.as_slice(), again.as_ref());
}

// ---------------------------------------------------------------------------
// Concurrent same-inode writes (FUSE-serialized model)
// ---------------------------------------------------------------------------

/// Concurrent non-overlapping writes to the same inode, serialized like FUSE
/// inode write locks, must leave a consistent final image.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_same_inode_writes_serialized() {
    let Some(fx) = LayoutFixture::new("conc_ser", false).await else {
        return;
    };
    let path = fx.path();
    let block = 4 * 1024 * 1024usize;

    // Establish striped layout with two zero blocks
    let zeros = Bytes::from(vec![0u8; block * 2]);
    fx.router
        .write_file(&path, 0, zeros, fx.next_fence())
        .await
        .expect("establish striped");
    assert_eq!(fx.layout_type(&path).await.as_deref(), Some("striped"));

    let router = fx.router.clone();
    let fence = Arc::new(AtomicU64::new(fx.next_fence()));
    // Simulates FUSE `active_inode_locks` write serialization
    let inode_lock = Arc::new(tokio::sync::Mutex::new(()));

    let mut handles = Vec::new();
    for i in 0..8u8 {
        let router = router.clone();
        let fence = fence.clone();
        let inode_lock = inode_lock.clone();
        let path = path.clone();
        handles.push(tokio::spawn(async move {
            let _g = inode_lock.lock().await;
            let offset = (i as u64) * 4096;
            let data = Bytes::from(vec![i.wrapping_add(1); 4096]);
            let token = fence.fetch_add(1, Ordering::SeqCst);
            router
                .write_file(&path, offset, data, token)
                .await
                .expect("serialized concurrent write");
        }));
    }
    for h in handles {
        h.await.expect("join write task");
    }

    let got = fx.router.read_file(&path).await.expect("final read");
    assert_eq!(got.len(), block * 2);
    for i in 0..8u8 {
        let offset = (i as usize) * 4096;
        let expected = i.wrapping_add(1);
        assert!(
            got[offset..offset + 4096].iter().all(|&b| b == expected),
            "range starting at {offset} must be solid 0x{expected:02x}"
        );
    }
}

/// Stale fencing token must still be rejected after a successful write
/// (layout path must honor DLM fencing — complements dlm_tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_layout_write_rejects_stale_fencing_token() {
    let Some(fx) = LayoutFixture::new("fence", false).await else {
        return;
    };
    let path = fx.path();
    fx.router
        .write_file(&path, 0, Bytes::from_static(b"ok"), 10)
        .await
        .expect("write with token 10");

    let err = fx
        .router
        .write_file(&path, 0, Bytes::from_static(b"bad"), 5)
        .await
        .expect_err("stale token must fail");
    assert!(
        matches!(
            err,
            squeezefs::error::SqueezefsError::FencingTokenExpired { .. }
        ),
        "got {err:?}"
    );
}
