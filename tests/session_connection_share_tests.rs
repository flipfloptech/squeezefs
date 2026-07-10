//! Regression guard for the shared `session_connection` cell.
//!
//! The FUSE daemon publishes the live `FuseConnection` on
//! `SqueezefsFilesystem::session_connection` *after* `session.mount(fs.clone(),
//! …)` has already consumed a clone (`start_mount` in `src/fuse_client.rs`).
//! The request handlers run on that clone. If the clone gets its own private
//! `ArcSwap` cell, the post-mount `store` is invisible to the handler, so
//! `get_payload_buffer` (the read zero-copy payload destination) silently
//! returns `None` forever — every buffered read falls back to an extra copy.
//!
//! The cell must therefore be shared (`Arc<ArcSwap<…>>`) across clones. This
//! test mirrors the exact mount ordering: clone first, publish on the original
//! afterwards, and assert the clone observes the published value.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Guards keep the backing/staging temp files alive for the fs under test.
struct MinFs {
    fs: SqueezefsFilesystem,
    _backing: NamedTempFile,
    _staging: TempDir,
}

async fn make_min_fs(test_id: &str) -> MinFs {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "4096");
    let dlm = DlmClient::new("local").unwrap();

    let backing_temp = NamedTempFile::new().unwrap();
    std::fs::File::create(backing_temp.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
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
        None,
    )
    .await
    .unwrap();

    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    MinFs {
        fs,
        _backing: backing_temp,
        _staging: temp_staging,
    }
}

/// The mounted request handler must observe the `session_connection` published
/// after `mount` consumed a clone. Reproduces the exact `start_mount` ordering:
/// clone → publish on the original → the clone reads it back.
///
/// RED before the fix (each clone owns a private `ArcSwap`, so the store is
/// invisible to the handler clone); GREEN once the cell is a shared
/// `Arc<ArcSwap<…>>`.
#[tokio::test]
async fn test_clone_observes_post_mount_session_connection() {
    let h = make_min_fs("sesconn_share").await;

    // `session.mount(fs.clone(), …)` — the handlers run on this clone.
    let fs_handler = h.fs.clone();

    // `start_mount` publishes the live connection on the ORIGINAL fs *after*
    // mount returns. We can't cheaply build a real `FuseConnection`, but the
    // sharing invariant is independent of the payload: publish a sentinel and
    // require the handler clone to observe the very same cell contents.
    let sentinel: Arc<Option<Arc<fuse3::raw::connection::FuseConnection>>> = Arc::new(None);
    h.fs.session_connection.store(sentinel.clone());

    let observed = fs_handler.session_connection.load_full();
    assert!(
        Arc::ptr_eq(&sentinel, &observed),
        "the mounted handler's clone did not observe the session_connection \
         published after mount — the cell is not shared across clones, so the \
         read zero-copy payload dest (get_payload_buffer) is permanently None"
    );
}

/// A store on the original is observed by *every* live clone, and clones made
/// after the store observe it too — i.e. one shared cell, no per-clone split.
#[tokio::test]
async fn test_session_connection_shared_both_clone_orderings() {
    let h = make_min_fs("sesconn_share2").await;

    let before = h.fs.clone(); // cloned before publish (the mount case)
    let sentinel: Arc<Option<Arc<fuse3::raw::connection::FuseConnection>>> = Arc::new(None);
    h.fs.session_connection.store(sentinel.clone());
    let after = h.fs.clone(); // cloned after publish

    assert!(
        Arc::ptr_eq(&sentinel, &before.session_connection.load_full()),
        "clone made before publish must share the cell and see the store"
    );
    assert!(
        Arc::ptr_eq(&sentinel, &after.session_connection.load_full()),
        "clone made after publish must share the cell and see the store"
    );
}
