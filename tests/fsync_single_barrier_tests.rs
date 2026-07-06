//! Durability-path guard: a single FUSE `fsync` must issue exactly ONE
//! meta-volume barrier (`sync_device_for_ino` -> `fdatasync`).
//!
//! Before the fix, `fsync` synced twice: once inside `flush_inode_to_backend`
//! and again in the handler with nothing written between them. This test pins
//! the barrier count via `METRICS.meta_device_syncs` so the redundant sync
//! cannot creep back in.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct Harness {
    fs: SqueezefsFilesystem,
    req: Request,
    _backing: NamedTempFile,
    _meta: NamedTempFile,
    _staging: TempDir,
}

async fn make_fs(test_id: &str) -> Harness {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    let dlm = DlmClient::new("local").unwrap();

    let backing_temp = NamedTempFile::new().unwrap();
    {
        let f = std::fs::File::create(backing_temp.path()).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
    }
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing_temp.path().to_str().unwrap()));

    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );

    let temp_staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![temp_staging.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let meta_temp = NamedTempFile::new().unwrap();
    let meta_storage = MetaLvStorage::open(meta_temp.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&meta_storage).await.unwrap();
    let meta_backend = Arc::new(MetaLvBackend::new(meta_storage));
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        meta_backend,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1234,
    };

    Harness {
        fs,
        req,
        _backing: backing_temp,
        _meta: meta_temp,
        _staging: temp_staging,
    }
}

/// A single fsync of a freshly-written inline file issues exactly one meta
/// device barrier — not two.
#[tokio::test]
async fn test_fsync_issues_single_meta_barrier() {
    let h = make_fs("fsync_single_barrier").await;

    let ino =
        h.fs.create(
            h.req,
            1,
            OsStr::new("barrier.bin"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap()
        .attr
        .ino;

    let payload = vec![0x5Au8; 4096];
    h.fs.write(
        h.req,
        ino,
        0,
        0,
        bytes::Bytes::copy_from_slice(&payload),
        0,
        0,
    )
    .await
    .unwrap();

    // Count meta-volume barriers attributable to this one fsync.
    let before = METRICS.meta_device_syncs.load(Ordering::Relaxed);
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let after = METRICS.meta_device_syncs.load(Ordering::Relaxed);

    assert_eq!(
        after - before,
        1,
        "fsync issued {} meta-volume barriers; expected exactly 1 (no redundant fdatasync)",
        after - before
    );
}
