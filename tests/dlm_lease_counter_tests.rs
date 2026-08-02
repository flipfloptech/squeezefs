//! DLM S0 — honest `lease_acquire_ok` / `lease_acquire_fail` counters
//! (spec §6.1 last row: both are rendered on the stats inode and were
//! never incremented anywhere — permanently 0, which poisoned the
//! 2026-07-14 lease-batching demotion's cited evidence).
//!
//! Contract: the counters gauge REAL cluster-lease acquisitions at the
//! one real acquire site, `get_or_acquire_lease` (fuse_client) — the
//! slow path that actually calls `DlmClient::acquire_lock`:
//!
//! - a write that acquires the ino's lease increments `lease_acquire_ok`
//!   exactly once (the cached-lease hot path stays counter-silent: it
//!   acquires nothing);
//! - an acquisition that FAILS (the lock is held by another client past
//!   the wait budget) increments `lease_acquire_fail` and surfaces the
//!   error.
//!
//! RED against dev @ 402ca77: neither counter moves.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    dlm: DlmClient,
    req: Request,
    _meta: NamedTempFile,
    _backing: NamedTempFile,
    _staging: tempfile::TempDir,
}

async fn make(ns: &str, uuid: [u8; 16]) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let meta = NamedTempFile::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let staging = tempdir().unwrap();
    meta.as_file().set_len(96 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_5EA5_E001,
        uuid,
    })
    .unwrap()
    .build(meta.path(), 96 * 1024 * 1024)
    .await
    .unwrap();

    let dlm = DlmClient::new("local").unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), ns)
            .await
            .unwrap(),
    );
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    router.set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
        "lz4".to_string(),
        "none".to_string(),
        None,
    ));
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = KvMetaBackend::open(meta.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs,
        dlm,
        req,
        _meta: meta,
        _backing: backing,
        _staging: staging,
    }
}

/// A leased write moves `lease_acquire_ok` exactly once per episode; the
/// cached-lease hot path acquires nothing and stays counter-silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leased_write_counts_one_acquire_ok() {
    let _g = serial().await;
    let h = make("lease_ctr_ok", *b"lease-counter-01").await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("a.bin"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    let ok_before = METRICS.lease_acquire_ok.load(Ordering::Relaxed);
    let fail_before = METRICS.lease_acquire_fail.load(Ordering::Relaxed);
    for i in 0..4u64 {
        let w =
            h.fs.write(
                h.req,
                ino,
                0,
                i * 4096,
                bytes::Bytes::from(vec![0x5A; 4096]),
                0,
                0,
            )
            .await
            .unwrap();
        assert_eq!(w.written, 4096);
    }
    let ok_after = METRICS.lease_acquire_ok.load(Ordering::Relaxed);
    let fail_after = METRICS.lease_acquire_fail.load(Ordering::Relaxed);

    assert_eq!(
        ok_after - ok_before,
        1,
        "one open-for-write episode = ONE real lease acquisition; the \
         cached-lease hot path must stay counter-silent (spec §6.1: the \
         counters were permanently 0 — never incremented anywhere)"
    );
    assert_eq!(
        fail_after - fail_before,
        0,
        "no failed acquisition happened"
    );
}

/// A blocked acquisition (lock held by another client past the wait
/// budget) surfaces the error AND counts `lease_acquire_fail`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_acquire_counts_fail() {
    let _g = serial().await;
    let h = make("lease_ctr_fail", *b"lease-counter-02").await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("b.bin"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    // A second client holds the ino's whole-file lock; the write path's
    // 5 s wait budget then fails the acquisition loudly.
    let contender = DlmClient::new("local").unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let held = contender
        .acquire_lock(&path, None, std::time::Duration::from_secs(5))
        .await
        .expect("contender acquire");

    let fail_before = METRICS.lease_acquire_fail.load(Ordering::Relaxed);
    let res =
        h.fs.write(h.req, ino, 0, 0, bytes::Bytes::from(vec![0xA5; 512]), 0, 0)
            .await;
    let fail_after = METRICS.lease_acquire_fail.load(Ordering::Relaxed);

    assert!(
        res.is_err(),
        "a write that cannot acquire its lease must fail loudly"
    );
    assert!(
        fail_after > fail_before,
        "the failed acquisition must be counter-visible \
         (lease_acquire_fail: {} -> {})",
        fail_before,
        fail_after
    );
    held.release().await.expect("contender release");
}
