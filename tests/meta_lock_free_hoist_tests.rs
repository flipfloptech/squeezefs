//! RES-1 (pre-RC engineering spec §7): `INODE_META_LOCKS` (P1-9 level
//! 3.5) must never be held across a terminal `free_block`.
//!
//! `BackendRouter::free_block` is not a bounded operation. Since the
//! park-don't-spill fix (write-wall iteration 1) an at-cap enqueue
//! **parks** — asynchronously, but for up to
//! `SQUEEZEFS_RECLAIM_CAP_PARK_MS` (default 1000) **per key**. Every
//! layout-commit path that frees its displaced keys *inside* the
//! `INODE_META_LOCKS` critical section therefore holds one of 4096
//! stripes for `keys × cap_park_ms`: 64 displaced keys under reclaim
//! pressure is up to 64 SECONDS of a per-inode lock that FUSE ops, the
//! writeback ladder, and the read-path refill all serialize on.
//!
//! The correct pattern is already in the tree in the two busiest places
//! — `write_striped` and `truncate_layout`'s striped arm collect the
//! displaced keys under the lock (through the §5.3 merge primitive) and
//! free them **after** the guard drops. The §5.2 law is "free only after
//! the new layout is published", and *after the guard* is strictly later
//! than *under the guard*, so the deferral costs no correctness.
//!
//! Contracts pinned here:
//!
//! 1. **`close_rewrite_epoch`** (the Idea-1 swap, whose `displaced` pop
//!    loop is the worst amplifier in the tree) releases the meta lock
//!    before its parked-key frees.
//! 2. **The staged-truncate commit** does the same for the durable copy
//!    it prunes.
//!
//! Both are observed as a happens-before, never a timeout: the probe is
//! released only once `block_free_reclaim_cap_parks` proves the freeing
//! task is inside the park, and the assertion is that the probe finishes
//! FIRST. Under the bug the probe cannot even start until the park
//! expires, so the completion order inverts deterministically.
//!
//! RED against dev 7d1ec2e1: `close_rewrite_epoch` frees inside
//! `_map_guard`'s scope (`while let Some(k) = epoch.displaced.pop()`) and
//! the staged-truncate commit frees inside `_meta_guard`'s.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::routing::DataRouter;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

/// Park bound for the at-cap enqueue: long enough that the probe cannot
/// win by luck, short enough that the RED run still terminates.
const CAP_PARK_MS: u64 = 1500;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Env-knob guard: set (or clear) for one test, restore on drop.
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prev }
    }
    fn clear(key: &'static str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// The reclaim knobs that make the park DETERMINISTIC: a one-entry
/// deferred-space cap, a worker whose accumulation window will not expire
/// inside the test, and a bounded park.
fn park_knobs() -> Vec<EnvGuard> {
    vec![
        EnvGuard::set("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", "1"),
        // The worker sleeps this accumulation window before its first
        // `take_batch`, so the queue stays at cap for the whole test and
        // the next free genuinely parks.
        EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "600000"),
        EnvGuard::set("SQUEEZEFS_RECLAIM_CAP_PARK_MS", &CAP_PARK_MS.to_string()),
        // File-backed harness: keep the elision arm off so terminal
        // frees really enter the queue.
        EnvGuard::set("SQUEEZEFS_DISCARD_ELISION", "0"),
    ]
}

/// The packing seam returns to the knob on drop.
struct PackLeverGuard;
impl Drop for PackLeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::test_set_small_file_packing(None);
    }
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    _backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make_harness(test_id: &str, staging_write_budget: &str) -> H {
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("16MB"),
        Some(staging_write_budget),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    squeezefs::meta_backend::kv::builder::format_v3(
        m.path(),
        128 * 1024 * 1024,
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
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs: Arc::new(fs),
        req,
        _backing: backing,
        _m: m,
        _s: s,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8 | 1)
        .collect()
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write");
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(
        h.req,
        1,
        std::ffi::OsStr::new(name),
        libc::S_IFREG | 0o644,
        0,
    )
    .await
    .expect("create")
    .attr
    .ino
}

async fn quiesce(h: &H) {
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
}

fn cap_parks() -> u64 {
    METRICS.block_free_reclaim_cap_parks.load(Ordering::Relaxed)
}

/// Put ONE entry in the reclaim queue so the next terminal free hits the
/// (one-entry) deferred-space cap and parks. Uses a block allocated and
/// freed straight through the router — no file, no layout.
async fn fill_reclaim_queue_to_cap(h: &H) {
    let (be_id, alloc, _w) =
        h.fs.router
            .backend_router
            .get_active_backend()
            .expect("active backend");
    let off = alloc.allocate_block().await.expect("allocate filler block");
    alloc.publish_block(off);
    let key = h.fs.router.backend_router.persist_block_key(&be_id, off);
    h.fs.router
        .backend_router
        .free_block(&key)
        .await
        .expect("free filler block");
}

/// Poll until the freeing task is observably INSIDE the at-cap park — a
/// counter transition is the synchronization, never a fixed sleep.
async fn await_park(base: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while cap_parks() == base {
        assert!(
            std::time::Instant::now() < deadline,
            "the free path never parked at the reclaim cap — the test's \
             premise (a bounded-but-long free) did not hold"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

/// Completion-order ticket: "who finished first" as a total order, never
/// a timeout.
#[derive(Default)]
struct Order {
    seq: AtomicU64,
}

impl Order {
    fn tick(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Contract 1 — the rewrite-epoch swap frees AFTER its meta lock drops.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewrite_epoch_close_frees_after_the_meta_lock_drops() {
    const FBS: u64 = 4096;
    let _g = serial().await;
    let _bs = EnvGuard::set("SQUEEZEFS_DEFAULT_BLOCK_SIZE", &FBS.to_string());
    let _knobs = park_knobs();
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    let h = make_harness("res1_epoch_close", "64MB").await;

    let ino = create(&h, "f1").await;
    write_at(&h, ino, 0, &pattern(3 * FBS as usize, 1)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    quiesce(&h).await;
    h.fs.router.metadata_cache.remove(&ino);
    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");

    // Partial rewrite: two displaced A keys park in the epoch (nothing
    // is freed yet — the §5.2 deferred-free law).
    write_at(&h, ino, 0, &pattern(2 * FBS as usize, 7)).await;
    quiesce(&h).await;

    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let order = Arc::new(Order::default());
    let base = cap_parks();

    let closer = {
        let fs = h.fs.clone();
        let order = order.clone();
        tokio::spawn(async move {
            let r = fs.router.close_rewrite_epoch(ino, token).await;
            (order.tick(), r)
        })
    };

    // The close is now inside the at-cap park for one of its displaced
    // keys. If the meta lock is still held, this probe cannot run.
    await_park(base).await;
    let probe =
        h.fs.router
            .grow_layout_size(ino, 0, token)
            .await
            .map(|()| order.tick())
            .expect("the meta-lock probe must not error");

    let (closed_at, res) = closer.await.expect("closer task");
    res.expect("the epoch close must succeed");
    assert!(
        probe < closed_at,
        "RES-1: an INODE_META_LOCKS op on ino {ino} had to wait for a \
         parked free to finish (probe ticket {probe} vs close ticket \
         {closed_at}) — the epoch close holds the level-3.5 stripe across \
         free_block, which parks up to {CAP_PARK_MS} ms PER KEY"
    );
}

// ---------------------------------------------------------------------------
// Contract 2 — the staged-truncate commit frees after its guard drops.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_truncate_commit_frees_after_the_meta_lock_drops() {
    // > the 1 MiB ring's high water (768 KiB) ⇒ the merge worker
    // promotes the blob to a durable `block_map[0]` (the same fixture
    // `staged_truncate_stale_tests` leg B uses).
    const A_LEN: usize = 800 * 1024;
    let _g = serial().await;
    let _bs = EnvGuard::clear("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let _knobs = park_knobs();
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(false);
    // The ONE-BLOCK promotion arm (`SQUEEZEFS_SMALL_FILE_PACKING=0`, the A/B
    // control since PK7's flip): this contract's premise is a TERMINAL
    // free the truncate commit issues — the whole promoted block entering
    // the reclaim queue and parking at its cap. A packed tenant's truncate
    // is a nonterminal reference release (the block stays held by the open
    // pack's pin), so nothing would ever park; the packed arm's RES-1 shape
    // is `pack_tenant_ops_tests`'.
    squeezefs::routing::test_set_small_file_packing(Some(false));
    let _pack = PackLeverGuard;
    let h = make_harness("res1_staged_truncate", "1MB").await;

    let ino = create(&h, "s1").await;
    write_at(&h, ino, 0, &pattern(A_LEN, 3)).await;
    let path = squeezefs::keys::inode_path(ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "staged", "fixture premise: staged layout");

    // Wait for the promotion to publish its durable copy — the thing the
    // truncate commit must prune and free.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let promoted =
            h.fs.router
                .metadata_cache
                .get(&ino)
                .and_then(|m| m.block_map.as_ref().and_then(|bm| bm.get(&0).cloned()))
                .is_some();
        if promoted {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture premise: the staged blob must promote to a durable block"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Hold the reclaim queue at its one-entry cap so the truncate's own
    // free parks.
    fill_reclaim_queue_to_cap(&h).await;

    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let order = Arc::new(Order::default());
    let base = cap_parks();

    let truncator = {
        let fs = h.fs.clone();
        let order = order.clone();
        tokio::spawn(async move {
            let r = fs.router.truncate_layout(ino, 0, token).await;
            (order.tick(), r)
        })
    };

    await_park(base).await;
    let probe =
        h.fs.router
            .grow_layout_size(ino, 0, token)
            .await
            .map(|()| order.tick())
            .expect("the meta-lock probe must not error");

    let (done_at, res) = truncator.await.expect("truncator task");
    res.expect("truncate must succeed");
    assert!(
        probe < done_at,
        "RES-1: an INODE_META_LOCKS op on ino {ino} had to wait for the \
         staged-truncate commit's parked free (probe ticket {probe} vs \
         commit ticket {done_at}) — the commit frees inside _meta_guard"
    );
}
