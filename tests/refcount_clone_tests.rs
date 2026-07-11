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
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{CachedMetadata, DataRouter, LayoutMetadata};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

/// Format + mount one v3 metadata volume for this harness.
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

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
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, ba.clone(), nvme);

    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
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

// ---------------------------------------------------------------------------
// PR K8 (design-cow-kv-metadata §5.3): lifting the inline-spill ceiling keeps
// large v3 files' block maps INLINE (up to the per-volume record cap) instead
// of forcing them to an indirect block past 32 entries. Clone must keep
// pinning every block of such an inline-mapped large source — the lift must
// not break the all-or-nothing refcount pin.
// ---------------------------------------------------------------------------

/// A router wired to a single-v3-volume routed backend (same shape as
/// [`make_router`], but format v3 so the spill lift applies).
async fn make_router_v3() -> (
    DataRouter,
    Arc<BlockAllocator>,
    Arc<RoutedMetaBackend>,
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
        BlockAllocator::new(dlm.meta_client().clone(), "refcount_clone_v3_test")
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
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, ba.clone(), nvme);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0x5CA1_AB1E_0DD5_9111,
        uuid: *b"k8-clone-lift!!!",
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    router.set_meta_backend(routed.clone());
    (router, ba, routed, b, m, s)
}

/// The spill lift keeps a large v3 file's map inline; `clone_file` must still
/// pin EVERY source block (all-or-nothing) — the lift is refcount-safe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_clone_of_large_inline_v3_file_pins_every_block() {
    let (router, ba, routed, _b, _m, _s) = make_router_v3().await;

    // 40 real blocks — past the pre-K8 32-entry inline ceiling, so the source
    // sits in the lifted inline regime on v3.
    let n = 40usize;
    let mut offsets = Vec::with_capacity(n);
    for _ in 0..n {
        let off = ba.allocate_block().await.expect("alloc");
        ba.publish_block(off);
        offsets.push(off);
    }
    let block_map: Vec<(u32, String)> = offsets
        .iter()
        .enumerate()
        .map(|(i, o)| (i as u32, o.to_string()))
        .collect();

    let src = mk_ino(&routed, "large_inline_src").await;
    let dest = mk_ino(&routed, "large_inline_dest").await;
    let src_ino: u64 = src.strip_prefix("inode_").unwrap().parse().unwrap();
    seed_meta(
        &router,
        &src,
        striped_meta(&block_map, n as u64 * 4096, true),
    )
    .await;

    // The source persisted INLINE on v3 (the whole point of the lift): the
    // block map rides in the layout value under the inline sentinel.
    let bytes = routed
        .getxattr(src_ino, "layout")
        .await
        .unwrap()
        .expect("layout xattr present");
    let layout = bincode::deserialize::<LayoutMetadata>(&bytes).unwrap();
    assert!(
        layout.block_map.is_some()
            && layout
                .block_map_id
                .as_deref()
                .unwrap()
                .starts_with("block_map_"),
        "a large ({n}-entry) v3 source must persist INLINE (id={:?})",
        layout.block_map_id,
    );
    assert_eq!(
        layout.block_map.as_ref().unwrap().len(),
        n,
        "the inline source map holds every block"
    );

    // Clone must pin every block (all-or-nothing).
    router
        .clone_file(&src, &dest, Some(1), Some(1))
        .await
        .expect("clone of an inline-mapped large v3 file must succeed");

    // Each source block is now pinned twice (alloc + clone). Drop the source's
    // reference on each; a further pin must still succeed — an unpinned clone
    // would have left count 1 → the drop would be terminal → refusal here.
    for off in &offsets {
        let key = off.to_string();
        ba.free_block(*off).await.expect("drop source ref");
        assert!(
            router.backend_router.increment_refcount(&key),
            "clone did not pin block {off} (dropping the source ref was terminal)"
        );
    }
}
