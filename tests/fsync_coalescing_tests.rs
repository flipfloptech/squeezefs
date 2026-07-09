//! Integration guard for Step B: concurrent FUSE `fsync`s route through the
//! per-volume group-commit coalescer.
//!
//! Each `fsync` records one barrier *request* (`meta_sync_requests`); the
//! coalescer collapses concurrent requests into shared `fdatasync`s
//! (`meta_device_syncs`). This test pins the wiring — every fsync requests a
//! barrier and the number of real barriers is bounded by `1..=N`. The exact
//! coalescing ratio is environment-dependent (device fsync latency), so the
//! deterministic reduction is proven in the coalescer unit tests; here we prove
//! the fsync path actually goes through it.

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
        f.set_len(128 * 1024 * 1024).unwrap();
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
    MetaLvBackend::format_v2_for_tests(&meta_storage, true, true, None)
        .await
        .unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_fsync_routes_through_coalescer() {
    let h = make_fs("fsync_coalescing").await;
    let n: usize = 64;

    // Create N inline files, each with a 4 KiB payload.
    let mut inos = Vec::with_capacity(n);
    for i in 0..n {
        let ino =
            h.fs.create(
                h.req,
                1,
                OsStr::new(&format!("coalesce_{i}.bin")),
                libc::S_IFREG | 0o644,
                0,
            )
            .await
            .unwrap()
            .attr
            .ino;
        let payload = vec![0x33u8; 4096];
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
        inos.push(ino);
    }

    let req_before = METRICS.meta_sync_requests.load(Ordering::Relaxed);
    let sync_before = METRICS.meta_device_syncs.load(Ordering::Relaxed);

    // Fire all fsyncs at once, aligned on a barrier to maximize overlap.
    let start = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for &ino in &inos {
        let fs = h.fs.clone();
        let req = h.req;
        let start = start.clone();
        handles.push(tokio::spawn(async move {
            start.wait().await;
            fs.fsync(req, ino, 0, false).await
        }));
    }
    for hdl in handles {
        hdl.await.unwrap().unwrap();
    }

    let req_delta = METRICS.meta_sync_requests.load(Ordering::Relaxed) - req_before;
    let sync_delta = METRICS.meta_device_syncs.load(Ordering::Relaxed) - sync_before;

    // Wiring: every fsync issued exactly one barrier request through the coalescer.
    assert_eq!(
        req_delta, n as u64,
        "each concurrent fsync must request exactly one barrier"
    );
    // The coalescer performed between 1 and N real barriers (never more than requests).
    assert!(
        sync_delta >= 1 && sync_delta <= n as u64,
        "coalesced barriers {sync_delta} must be within 1..={n}"
    );
    eprintln!("coalescing: {req_delta} fsync requests -> {sync_delta} real fdatasync barriers");
}
