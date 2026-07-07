//! Refcount refusal propagation contracts.
//!
//! `increment_refcount` refuses (instead of resurrecting) once a block's
//! count hit its terminal zero — but a refusal the caller ignores is a
//! silent unpinned clone: the clone's block map references an offset the
//! allocator may hand to the next writer, which then reads as foreign
//! bytes. Contracts:
//!
//! 1. Allocator/router increments report whether the reference was taken.
//! 2. `clone_file` of a striped source must be all-or-nothing: if a block
//!    cannot be pinned even after re-reading the authoritative map, the
//!    clone FAILS loudly — never an Ok clone with unpinned blocks.
//! 3. A stale cached map heals: when the authoritative backend map pins
//!    cleanly, the clone succeeds using that map (retry path), and the
//!    pinned block's refcount reflects the clone.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::meta_backend::{storage::MetaLvStorage, Metadata, MetaLvBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{CachedMetadata, DataRouter};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

async fn make_router() -> (
    DataRouter,
    Arc<BlockAllocator>,
    NamedTempFile,
    NamedTempFile,
    tempfile::TempDir,
) {
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "refcount_clone_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm, cache, ba.clone(), nvme);

    let m = NamedTempFile::new().unwrap();
    let ms = MetaLvStorage::open(m.path(), 256 * 1024 * 1024).unwrap();
    MetaLvBackend::format(&ms).await.unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        Arc::new(MetaLvBackend::new(ms)),
    ]));
    router.set_meta_backend(routed);
    (router, ba, b, m, s)
}

fn striped_meta(block_map: &[(u32, String)], size: u64) -> CachedMetadata {
    CachedMetadata {
        file_type: "striped".to_string(),
        size,
        block_map: Some(block_map.iter().cloned().collect()),
        ..Default::default()
    }
}

/// Contract 1: increments report whether the reference was taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_increment_reports_refusal() {
    let (router, ba, _b, _m, _s) = make_router().await;

    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);
    let key = offset.to_string();

    assert!(
        router.backend_router.increment_refcount(&key),
        "live block must pin (count 1 -> 2)"
    );
    // Undo the pin, then free to terminal zero.
    ba.free_block(offset).await.expect("unpin");
    ba.free_block(offset).await.expect("terminal free");

    assert!(
        !router.backend_router.increment_refcount(&key),
        "freed block must refuse the pin"
    );
    assert!(
        !router.backend_router.increment_refcount("999999999"),
        "untracked offset must refuse the pin"
    );
}

/// Contract 2: a striped clone whose source blocks cannot be pinned (even
/// from the authoritative map) fails loudly — never Ok-with-unpinned-blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_clone_fails_loud_when_blocks_unpinnable() {
    let (router, _ba, _b, _m, _s) = make_router().await;

    // Source meta (cache AND backend agree) references an untracked offset:
    // every pin attempt refuses, on the snapshot and on the refetched map.
    let src = "inode_777001";
    let meta = striped_meta(&[(0, "424242".to_string())], 4096);
    router
        .save_metadata_to_backend(777001, &meta, 1)
        .await
        .expect("seed backend meta");
    router.metadata_cache.insert(src.to_string(), meta);

    let res = router
        .clone_file(src, "inode_777002", Some(1), Some(1))
        .await;
    assert!(
        res.is_err(),
        "clone must fail when its source blocks cannot be pinned \
         (an Ok here is an unpinned clone aliasing a reallocatable offset)"
    );
}

/// Contract 3: a stale cached map heals through the authoritative backend
/// map — the clone retries, pins the real block, and succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_clone_retries_via_authoritative_map() {
    let (router, ba, _b, _m, _s) = make_router().await;

    // Authoritative (backend) map references a live allocated block…
    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);
    let src = "inode_777010";
    let good = striped_meta(&[(0, offset.to_string())], 4096);
    router
        .save_metadata_to_backend(777010, &good, 1)
        .await
        .expect("seed backend meta");
    // …while the hot cache holds a stale map with an unpinnable key.
    let stale = striped_meta(&[(0, "424243".to_string())], 4096);
    router.metadata_cache.insert(src.to_string(), stale);

    router
        .clone_file(src, "inode_777011", Some(1), Some(1))
        .await
        .expect("clone must heal via the authoritative map");

    // The clone holds a real pin (count 2 = alloc + clone): dropping the
    // source's reference must not be terminal, so a further pin still
    // succeeds. An unpinned clone would leave count 1 -> terminal free ->
    // refusal here.
    ba.free_block(offset).await.expect("drop source ref");
    assert!(
        router
            .backend_router
            .increment_refcount(&offset.to_string()),
        "clone did not actually pin the block (source drop was terminal)"
    );
}
