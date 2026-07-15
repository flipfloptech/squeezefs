//! PR M5 (design-metadata-throughput §5.2 D2.c): the trailing-GETATTR
//! economy. The kernel invalidates its parent-dir attrs on every
//! unlink/rename (`fuse_dir_changed`) and re-GETATTRs them on the next
//! path walk — traffic the daemon cannot suppress (M2 measured GETATTR
//! 1.17/create and **1.82/unlink at ~45 µs each**, the biggest trailing-op
//! population). What the daemon CAN control is the **cost**: pre-M5 the
//! unlink/rename handlers also invalidated the daemon's own `attr_cache`
//! for the parent (and child), so each forced kernel GETATTR became a
//! contended backend fetch. D2.c refreshes instead: the handler re-seeds
//! the cache from the RAM-authoritative backend (exact values, monotone
//! through the M6 pending-times fold), so the kernel's revalidation is a
//! ~µs cache hit.
//!
//! Contract pinned here (per mutated inode):
//! - post-unlink: parent AND child attrs are PRESENT in the cache and
//!   byte-agree with backend truth (fresh mtime/ctime, decremented nlink);
//! - post-rename: both parents present-and-true;
//! - `fuse_attr_cache_refreshes` counts each refresh (the acceptance
//!   session's live signal);
//! - the values must come from the BACKEND, never a handler-side clock —
//!   pinned by exact equality with a subsequent backend read.
//!
//! Counter deltas are exact under the sanctioned serial gate
//! (`--test-threads=1`).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use fuse3::Timestamp;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "attrref_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_9ABC_DEF0,
            uuid: *b"attr-refresh-v3!",
        })
        .unwrap()
        .build(m.path(), 64 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
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
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

fn refreshes() -> u64 {
    METRICS.fuse_attr_cache_refreshes.load(Ordering::Relaxed)
}

fn ts(ns: u64) -> Timestamp {
    Timestamp::new((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
}

/// Read the RAW cached attr (never through getattr — `get_attr_internal`
/// would repopulate the cache on a miss and mask an invalidate).
fn cached_attr(h: &H, ino: u64) -> Option<fuse3::raw::reply::FileAttr> {
    h.fs.attr_cache.get(&ino).map(|(a, _)| a)
}

async fn backend_inode(h: &H, ino: u64) -> squeezefs::meta_backend::Inode {
    h.fs.meta_backend
        .as_ref()
        .unwrap()
        .getattr(ino)
        .await
        .unwrap()
}

async fn create(h: &H, parent: u64, name: &str) -> u64 {
    let ino =
        h.fs.create(h.req, parent, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    // Close the create handle so unlink -> reclaim lifecycles stay clean.
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();
    ino
}

async fn mkdir(h: &H, name: &str) -> u64 {
    h.fs.mkdir(h.req, 1, OsStr::new(name), 0o755, 0)
        .await
        .unwrap()
        .attr
        .ino
}

/// Post-unlink, the parent's and the (still-alive-until-FORGET) child's
/// attrs are cached FRESH — present, and byte-agreeing with the backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_refreshes_parent_and_child_attr_cache() {
    let h = make().await;
    let dir = mkdir(&h, "d").await;
    let child = create(&h, dir, "victim").await;

    let r0 = refreshes();
    h.fs.unlink(h.req, dir, OsStr::new("victim")).await.unwrap();

    // Parent: present and true (the kernel's forced revalidation GETATTR
    // must be a cache hit serving the post-op times).
    let cached = cached_attr(&h, dir)
        .expect("post-unlink the PARENT attrs must be cached (refreshed, not invalidated)");
    let truth = backend_inode(&h, dir).await;
    assert_eq!(
        cached.mtime,
        ts(truth.mtime),
        "cached parent mtime must be the backend's post-unlink value"
    );
    assert_eq!(
        cached.ctime,
        ts(truth.ctime),
        "cached parent ctime must be the backend's post-unlink value"
    );

    // Child: present and true (nlink dropped; inode alive until FORGET).
    let cached_child = cached_attr(&h, child)
        .expect("post-unlink the CHILD attrs must be cached (refreshed, not invalidated)");
    let child_truth = backend_inode(&h, child).await;
    assert_eq!(cached_child.nlink, child_truth.nlink, "post-unlink nlink");
    assert_eq!(cached_child.ctime, ts(child_truth.ctime));

    assert_eq!(
        refreshes() - r0,
        2,
        "unlink must refresh exactly parent + child"
    );
}

/// Post-rename, both parents' attrs are cached fresh (the M6 +0.21
/// fuse_ops/rename revalidation lands on the cache, not the backend).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_refreshes_both_parents() {
    let h = make().await;
    let src_dir = mkdir(&h, "src").await;
    let dst_dir = mkdir(&h, "dst").await;
    create(&h, src_dir, "mover").await;

    let r0 = refreshes();
    h.fs.rename(
        h.req,
        src_dir,
        OsStr::new("mover"),
        dst_dir,
        OsStr::new("moved"),
    )
    .await
    .unwrap();

    for (dir, tag) in [(src_dir, "source parent"), (dst_dir, "dest parent")] {
        let cached = cached_attr(&h, dir)
            .unwrap_or_else(|| panic!("post-rename the {tag} attrs must be cached"));
        let truth = backend_inode(&h, dir).await;
        assert_eq!(
            cached.mtime,
            ts(truth.mtime),
            "{tag}: cached mtime must be the backend's post-rename value"
        );
    }
    assert_eq!(
        refreshes() - r0,
        2,
        "cross-dir rename with no dest refreshes exactly the two parents"
    );
}

/// Same-dir rename refreshes the one parent once (no double fetch).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_dir_rename_refreshes_parent_once() {
    let h = make().await;
    let dir = mkdir(&h, "one").await;
    create(&h, dir, "a").await;

    let r0 = refreshes();
    h.fs.rename(h.req, dir, OsStr::new("a"), dir, OsStr::new("b"))
        .await
        .unwrap();
    assert_eq!(
        refreshes() - r0,
        1,
        "same-parent rename must refresh the shared parent exactly once"
    );
    assert!(cached_attr(&h, dir).is_some());
}
