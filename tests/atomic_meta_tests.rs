//! P0-6: Critical metadata updates must be MULTI/EXEC-atomic.
//!
//! After successful layout commits, related keys must always appear together
//! (never type=striped without block_map, never inline type without payload, etc.).
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

struct MetaFixture {
    router: DataRouter,
    _temp: TempDir,
    next_ino: u64,
}

impl MetaFixture {
    async fn new(tag: &str, with_staging: bool) -> Option<Self> {
        if !garnet_ok().await {
            println!("Skipping {tag}: Garnet unavailable");
            return None;
        }
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let prefix = format!("atm_{tag}_{n}");
        let base_ino = (n as u64).wrapping_mul(13033).wrapping_add(90_000);
        set_fs_prefix(&prefix);
        set_write_verification(false);

        let dlm = DlmClient::new(&redis_url()).ok()?;
        let meta = Arc::new(dlm.meta_client().clone());
        let temp = TempDir::new().ok()?;
        let block_path = temp.path().join("b.img");
        {
            let f = std::fs::File::create(&block_path).ok()?;
            f.set_len(64 * 1024 * 1024).ok()?;
        }
        let staging_dirs = if with_staging {
            let staging = temp.path().join("st");
            std::fs::create_dir_all(&staging).ok()?;
            vec![staging]
        } else {
            vec![]
        };
        let nvme = Arc::new(NvmeBlockDev::new(block_path.to_str()?));
        let alloc = Arc::new(BlockAllocator::new(meta.clone(), &prefix).await.ok()?);
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
        .ok()?;
        let router = DataRouter::new(dlm, cache, alloc, nvme);
        Some(Self {
            router,
            _temp: temp,
            next_ino: base_ino,
        })
    }

    fn path(&mut self) -> String {
        let p = format!("inode_{}", self.next_ino);
        self.next_ino += 1;
        p
    }
}

/// After a successful large write, striped meta fields and block_map must co-exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_striped_meta_and_block_map_consistent() {
    // No staging → stripe threshold is 4 KiB.
    let Some(mut fx) = MetaFixture::new("stripe", false).await else {
        return;
    };
    let path = fx.path();
    let data = Bytes::from(vec![0x5Au8; 8 * 1024]);
    fx.router
        .write_file(&path, 0, data.clone(), 1)
        .await
        .expect("striped write");

    let mut con = fx.router.dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:{path}");
    let ty: Option<String> = con.hget(&meta_key, "type").await.unwrap();
    let map_id: Option<String> = con.hget(&meta_key, "block_map_id").await.unwrap();
    let num_blocks: Option<u32> = con.hget(&meta_key, "num_blocks").await.unwrap();
    let size: Option<u64> = con.hget(&meta_key, "size").await.unwrap();
    let fence: Option<u64> = con.hget(&meta_key, "fencing_token").await.unwrap();

    assert_eq!(ty.as_deref(), Some("striped"));
    let map_id = map_id.expect("block_map_id must be set with type=striped");
    assert!(num_blocks.unwrap_or(0) >= 1);
    assert_eq!(size, Some(data.len() as u64));
    assert_eq!(fence, Some(1));

    let map_key = format!("block_map:{map_id}");
    let bk0: Option<String> = con.hget(&map_key, "0").await.unwrap();
    assert!(
        bk0.as_ref().is_some_and(|s| !s.is_empty()),
        "block_map must have entry 0 when type is striped"
    );

    let got = fx.router.read_file(&path).await.expect("read back");
    assert_eq!(got.as_slice(), data.as_ref());
}

/// Inline type and inline_data payload must co-exist after a small write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_inline_meta_and_payload_consistent() {
    let Some(mut fx) = MetaFixture::new("inline", false).await else {
        return;
    };
    let path = fx.path();
    let data = Bytes::from_static(b"tiny-inline-payload");
    fx.router
        .write_file(&path, 0, data.clone(), 3)
        .await
        .expect("inline write");

    let mut con = fx.router.dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:{path}");
    let ty: Option<String> = con.hget(&meta_key, "type").await.unwrap();
    let fence: Option<u64> = con.hget(&meta_key, "fencing_token").await.unwrap();
    let size: Option<u64> = con.hget(&meta_key, "size").await.unwrap();
    assert_eq!(ty.as_deref(), Some("inline"));
    assert_eq!(fence, Some(3));
    assert_eq!(size, Some(data.len() as u64));

    let inline_key = format!("inline_data:{path}");
    let raw: Option<Vec<u8>> = con.get(&inline_key).await.unwrap();
    assert!(
        raw.as_ref().is_some_and(|v| !v.is_empty()),
        "inline type must co-exist with inline_data payload"
    );
    let got = fx.router.read_file(&path).await.expect("read");
    assert_eq!(got.as_slice(), data.as_ref());
}

/// Staged type must co-exist with file_id (and fencing) after mid-size write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_staged_meta_and_file_id_consistent() {
    let Some(mut fx) = MetaFixture::new("staged", true).await else {
        return;
    };
    let path = fx.path();
    let data = Bytes::from(vec![0xCCu8; 64 * 1024]);
    fx.router
        .write_file(&path, 0, data.clone(), 5)
        .await
        .expect("staged write");

    let mut con = fx.router.dlm.get_connection().await.unwrap();
    let meta_key = format!("metadata:{path}");
    let ty: Option<String> = con.hget(&meta_key, "type").await.unwrap();
    let file_id: Option<String> = con.hget(&meta_key, "file_id").await.unwrap();
    let fence: Option<u64> = con.hget(&meta_key, "fencing_token").await.unwrap();
    let size: Option<u64> = con.hget(&meta_key, "size").await.unwrap();

    assert_eq!(ty.as_deref(), Some("staged"));
    assert!(
        file_id.as_ref().is_some_and(|s| !s.is_empty()),
        "staged type must co-exist with file_id"
    );
    assert_eq!(fence, Some(5));
    assert_eq!(size, Some(data.len() as u64));
    let got = fx.router.read_file(&path).await.expect("read");
    assert_eq!(got.as_slice(), data.as_ref());
}

/// Lock acquire must not burn a fencing token when the lock is already held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lock_acquire_failure_does_not_burn_fencing_token() {
    if !garnet_ok().await {
        println!("Skipping: Garnet unavailable");
        return;
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prefix = format!("atm_lock_{n}");
    set_fs_prefix(&prefix);

    let dlm_a = DlmClient::new(&redis_url()).unwrap();
    let dlm_b = DlmClient::new(&redis_url()).unwrap();
    let path = format!("inode_{}", (n as u64) % 1_000_000 + 1);

    let lease_a = dlm_a
        .acquire_lock(&path, None, Duration::from_secs(30))
        .await
        .expect("A acquires");
    let token_a = lease_a.fencing_token();

    // Failed acquire must not advance the generator.
    let fail = dlm_b
        .acquire_lock(&path, None, Duration::from_secs(30))
        .await;
    assert!(fail.is_err(), "B must fail while A holds lock");

    let mut con = dlm_a.meta_client().get_connection().await.unwrap();
    let gen_key = format!("fencing_generator:{path}");
    let gen: u64 = redis::cmd("GET")
        .arg(&gen_key)
        .query_async(&mut con)
        .await
        .unwrap_or(0);
    assert_eq!(
        gen, token_a,
        "failed lock must not INCR fencing_generator (gen={gen}, token_a={token_a})"
    );

    let _ = lease_a.release().await;
    let lease_b = dlm_b
        .acquire_lock(&path, None, Duration::from_secs(30))
        .await
        .expect("B acquires after release");
    assert_eq!(lease_b.fencing_token(), token_a + 1);
    let _ = lease_b.release().await;
}
