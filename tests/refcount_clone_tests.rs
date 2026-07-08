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
use squeezefs::meta_backend::{storage::MetaLvStorage, MetaLvBackend, Metadata};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{CachedMetadata, DataRouter};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

async fn make_router() -> (
    DataRouter,
    Arc<BlockAllocator>,
    Arc<squeezefs::meta_backend::RoutedMetaBackend>,
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
    router.set_meta_backend(routed.clone());
    (router, ba, routed, b, m, s)
}

/// Create a real inode and return its `inode_{ino}` path (layout xattrs
/// need an existing inode slot).
async fn mk_ino(routed: &squeezefs::meta_backend::RoutedMetaBackend, name: &str) -> String {
    let ino = routed
        .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create")
        .ino;
    format!("inode_{ino}")
}

fn striped_meta(block_map: &[(u32, String)], size: u64, dirty: bool) -> CachedMetadata {
    CachedMetadata {
        file_type: "striped".to_string(),
        size,
        block_map: Some(block_map.iter().cloned().collect()),
        layout_dirty: dirty,
        ..Default::default()
    }
}

/// Seed a layout into cache AND backend through the public writeback flow.
async fn seed_meta(router: &DataRouter, path: &str, meta: CachedMetadata) {
    router.metadata_cache.insert(path.to_string(), meta);
    router
        .persist_dirty_layout_if_needed(path, 1)
        .await
        .expect("persist seeded layout");
}

/// Contract 1: increments report whether the reference was taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_increment_reports_refusal() {
    let (router, ba, _routed, _b, _m, _s) = make_router().await;

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
    let (router, _ba, routed, _b, _m, _s) = make_router().await;

    // Source meta (cache AND backend agree) references an untracked offset:
    // every pin attempt refuses, on the snapshot and on the refetched map.
    let src = mk_ino(&routed, "unpinnable_src").await;
    let dest = mk_ino(&routed, "unpinnable_dest").await;
    seed_meta(
        &router,
        &src,
        striped_meta(&[(0, "424242".to_string())], 4096, true),
    )
    .await;

    let res = router.clone_file(&src, &dest, Some(1), Some(1)).await;
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
    let (router, ba, routed, _b, _m, _s) = make_router().await;

    // Authoritative (backend) map references a live allocated block…
    let offset = ba.allocate_block().await.expect("alloc");
    ba.publish_block(offset);
    let src = mk_ino(&routed, "heal_src").await;
    let dest = mk_ino(&routed, "heal_dest").await;
    seed_meta(
        &router,
        &src,
        striped_meta(&[(0, offset.to_string())], 4096, true),
    )
    .await;
    // …while the hot cache holds a stale map with an unpinnable key
    // (clean flag: nothing re-persists the stale form over the good one).
    let stale = striped_meta(&[(0, "424243".to_string())], 4096, false);
    router.metadata_cache.insert(src.clone(), stale);

    router
        .clone_file(&src, &dest, Some(1), Some(1))
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

// ---------------------------------------------------------------------------
// Hole-punch discipline on block free (surfaced by PR 6 of
// docs/design-zero-copy-write-path.md, pre-existing on dev): a freed block's
// hole punch is destructive device I/O and must obey two contracts. Both
// were violated by `BackendRouter::free_block` punching unconditionally
// AFTER the allocator made the offset reallocatable — demonstrated as an
// acked-write-then-device-zeros lost update under free→realloc churn with
// concurrent readers (the pinned striped concurrency test), invisible
// while `write_striped`'s read-LRU put shielded reads from the device.
// ---------------------------------------------------------------------------

/// Contract A: a NON-terminal free (refcount still > 0 — e.g. a clone's
/// shared block) must NOT punch the block's data. Punching a live shared
/// block destroys the surviving clone's bytes on the device.
#[tokio::test]
async fn test_nonterminal_free_must_not_punch_shared_block() {
    let (router, ba, _routed, backing, _m, _s) = make_router().await;

    let offset = ba.allocate_block().await.expect("alloc");
    let key = offset.to_string();
    assert!(
        router.backend_router.increment_refcount(&key),
        "second reference (clone) must pin"
    );

    let pattern: Vec<u8> = (0..8192usize).map(|i| (i % 251) as u8).collect();
    router
        .nvme_writer
        .write_block(offset, bytes::Bytes::from(pattern.clone()))
        .await
        .expect("write");
    ba.publish_block(offset);

    // Drop ONE of the two references through the router (the path that
    // punches). The block is still referenced by the clone.
    router
        .backend_router
        .free_block(&key)
        .await
        .expect("non-terminal free");

    // The surviving reference's data must still be on the device — read the
    // backing file directly so no cache can mask a punch.
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(backing.path()).expect("open backing");
    let mut buf = vec![0u8; 8192];
    f.read_exact_at(&mut buf, offset).expect("pread");
    assert_eq!(
        buf, pattern,
        "non-terminal free punched a still-referenced block's data \
         (clone data destroyed on device)"
    );
}

/// Contract B: a TERMINAL free must punch strictly BEFORE the offset
/// becomes reallocatable. `begin_free` (terminal) retires the offset but
/// must not put it on the free list yet — `allocate_block` cannot return
/// it until `finish_free`. This is the ordering that makes the punch's
/// destructive zeroing race-free against a new owner's DMA: today the
/// offset is handed out first and the freer's late punch zeroes the new
/// owner's acked write (the durable lost-update class).
#[tokio::test]
async fn test_terminal_free_punches_before_offset_is_reallocatable() {
    let (_router, ba, _routed, _b, _m, _s) = make_router().await;

    // Drain any implicit free list first: allocate twice, keep both.
    let a = ba.allocate_block().await.expect("alloc a");
    let b = ba.allocate_block().await.expect("alloc b");
    assert_ne!(a, b);

    // Terminal begin_free retires `a` but must NOT make it reallocatable.
    assert!(
        ba.begin_free(a),
        "single-reference free must report terminal"
    );
    let c = ba.allocate_block().await.expect("alloc c");
    assert_ne!(
        c, a,
        "offset became reallocatable between begin_free and finish_free \
         (a concurrent owner's DMA would race the freer's hole punch)"
    );

    // finish_free publishes it for reuse.
    ba.finish_free(a);
    let d = ba.allocate_block().await.expect("alloc d");
    assert_eq!(d, a, "finish_free must return the offset to the free list");

    // A non-terminal begin_free reports false and releases nothing.
    assert!(
        ba.increment_refcount(b),
        "pin b to two references (count 2)"
    );
    assert!(
        !ba.begin_free(b),
        "non-terminal release must not report terminal"
    );
}
