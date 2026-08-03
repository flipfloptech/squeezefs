//! FUSE-3k ⊕ POSIX-14 — the daemon keeps a LOOKUP refcount, and a
//! forget's `nlookup` is honored (pre-rc-engineering-spec §4 FUSE-3k, §5
//! POSIX-14).
//!
//! The kernel's contract (fs/fuse/dir.c, readdir.c, inode.c) is that it
//! takes one reference per entry reply it instantiates and returns them in
//! bulk: `fuse_evict_inode` sends ONE forget carrying the inode's whole
//! accumulated `nlookup`, while `fuse_force_forget` returns a single
//! reference without evicting anything. The daemon kept no count at all —
//! `BATCH_FORGET` discarded the per-entry `nlookup` outright — so every
//! forget was one unconditional eviction: the `fuse_force_forget(1)` shape
//! (a readdirplus entry the kernel failed to link, a revalidate that
//! dropped its ref) threw away attr-cache/side-map state the kernel still
//! references and queued a reclaim for an inode it still holds.
//!
//! Contracts pinned here:
//!
//! 1. Every entry reply the daemon sends counts ONE lookup reference
//!    (create counts; each LOOKUP counts).
//! 2. A forget subtracts `nlookup` and evicts ONLY at zero — a partial
//!    return keeps the inode's daemon-side state.
//! 3. `BATCH_FORGET` carries per-entry `nlookup` (it is the drop_caches /
//!    memory-pressure path; discarding the counts made it the loudest
//!    version of the same bug).
//! 4. An ino the daemon never counted still evicts on its first forget —
//!    the pre-3k behavior stays the fallback, so a miscount can only ever
//!    cost cache retention, never correctness.
//! 5. POSIX-14: a lost RELEASE leaves `open_count` nonzero forever. At the
//!    final forget the kernel has certified it holds no reference (which
//!    requires every `struct file` closed), so a nonzero count there is a
//!    provably stale veto on reclaim: it is DETECTED and counted on the
//!    must-stay-0 `open_count_stranded` tripwire. The veto itself stands —
//!    an unlinked-open file's data is never destroyed on the strength of a
//!    count we already know is wrong (the generic/795 lesson).

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
use tempfile::{tempdir, NamedTempFile};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn harness(tag: &str, uuid: [u8; 16]) -> H {
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let alloc = Arc::new(BlockAllocator::new(tag).await.expect("allocator"));
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("32MB"),
        Some("32MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .expect("cache");
    let router = DataRouter::new(dlm.clone(), cache, alloc, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(48 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0x3B3B_0000_1111_2222,
            uuid,
        })
        .unwrap()
        .build(m.path(), 48 * 1024 * 1024)
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
        ..Default::default()
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .expect("create")
        .attr
        .ino
}

async fn lookup(h: &H, name: &str) -> u64 {
    h.fs.lookup(h.req, 1, OsStr::new(name))
        .await
        .expect("lookup")
        .attr
        .ino
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_replies_count_and_a_partial_forget_does_not_evict() {
    let h = harness("fuse3k_partial", *b"fuse3k-partial-1").await;
    let ino = create(&h, "f").await;
    assert_eq!(
        h.fs.lookup_refs(ino),
        1,
        "an entry reply (create) is one kernel lookup reference"
    );
    for _ in 0..3 {
        assert_eq!(lookup(&h, "f").await, ino);
    }
    assert_eq!(
        h.fs.lookup_refs(ino),
        4,
        "every LOOKUP reply the daemon sends is another kernel reference"
    );

    // The `fuse_force_forget`-shaped partial return: 2 of 4.
    h.fs.forget(h.req, ino, 2).await;
    assert_eq!(h.fs.lookup_refs(ino), 2, "nlookup must be SUBTRACTED");
    assert!(
        h.fs.attr_cache_holds(ino),
        "FUSE-3k: a partial forget must not evict daemon state the kernel \
         still references"
    );

    // The eviction forget: the remaining 2.
    h.fs.forget(h.req, ino, 2).await;
    assert_eq!(h.fs.lookup_refs(ino), 0);
    assert!(
        !h.fs.attr_cache_holds(ino),
        "the final forget evicts exactly as before"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_forget_carries_per_entry_nlookup() {
    let h = harness("fuse3k_batch", *b"fuse3k-batch-001").await;
    let a = create(&h, "a").await;
    let b = create(&h, "b").await;
    assert_eq!(lookup(&h, "a").await, a);
    assert_eq!(lookup(&h, "a").await, a);
    // a: 3 refs, b: 1 ref.
    assert_eq!((h.fs.lookup_refs(a), h.fs.lookup_refs(b)), (3, 1));

    // One batch, honest per-entry counts: `a` keeps 1, `b` reaches 0.
    h.fs.batch_forget(h.req, &[(a, 2), (b, 1)]).await;
    assert_eq!(h.fs.lookup_refs(a), 1, "BATCH_FORGET must subtract 2 from a");
    assert!(
        h.fs.attr_cache_holds(a),
        "FUSE-3k: BATCH_FORGET discarded nlookup, so a partially-returned \
         inode was evicted anyway"
    );
    assert!(!h.fs.attr_cache_holds(b), "b's last reference returned");

    h.fs.batch_forget(h.req, &[(a, 1)]).await;
    assert!(!h.fs.attr_cache_holds(a), "a's last reference returned");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncounted_inode_still_evicts_on_its_first_forget() {
    let h = harness("fuse3k_unknown", *b"fuse3k-unknown-1").await;
    let ino = create(&h, "f").await;
    // Simulate an ino with no tracked references (a count this mount never
    // established — e.g. state carried by a kernel that outlived a
    // remount): the pre-3k behavior must remain the fallback.
    h.fs.forget(h.req, ino, 1).await;
    assert_eq!(h.fs.lookup_refs(ino), 0);
    h.fs.getattr(h.req, ino, None, 0).await.expect("getattr");
    assert!(h.fs.attr_cache_holds(ino));
    h.fs.forget(h.req, ino, 1).await;
    assert!(
        !h.fs.attr_cache_holds(ino),
        "an untracked ino evicts on its first forget — a miscount may cost \
         cache retention, never correctness"
    );
}

/// POSIX-14: the stranded-open-count detector.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_release_leaves_a_stranded_open_count_that_is_detected() {
    let h = harness("posix14_strand", *b"posix14-strand-1").await;
    let ino = create(&h, "f").await;
    // An OPEN whose RELEASE never arrives (a panicked handler task: FUSE-2
    // synthesizes the reply so the kernel is not left waiting, but the
    // daemon's `remove_open` never ran).
    h.fs.open(h.req, ino, libc::O_RDWR as u32)
        .await
        .expect("open");
    assert!(h.fs.is_open(ino), "fixture premise: the count is up");

    let before = METRICS.open_count_stranded.load(Ordering::Relaxed);
    // The kernel's final forget: it holds no reference to this inode, which
    // it could not say while any file on it was open.
    h.fs.forget(h.req, ino, h.fs.lookup_refs(ino)).await;
    assert_eq!(
        METRICS.open_count_stranded.load(Ordering::Relaxed),
        before + 1,
        "POSIX-14: a nonzero open count at the final forget is a lost \
         RELEASE and must be detected (open_count_stranded)"
    );
    assert!(
        h.fs.is_open(ino),
        "data safety first: the count is NOT zeroed on the strength of an \
         accounting we already know is wrong — an unlinked-open file's data \
         is never destroyed by a reclaim we cannot justify (generic/795)"
    );
}
