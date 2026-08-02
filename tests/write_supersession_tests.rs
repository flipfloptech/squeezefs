//! Idea 2 — latest-wins supersession of unpublished in-flight uploads
//! (`docs/design-rewrite-program.md` §4; the rewrite program's
//! overlapping-face loop-rewrite vehicle).
//!
//! Today the pipeline upload holds `BLOCK_FLUSH_LOCKS(ino, b)` across its
//! whole body, so a rewrite of an in-DMA block waits out the fabric RTT
//! and then pays its own full upload — device writes ≡ ops, serialized.
//! The supersession restructure snapshots + stamps the buffer's
//! write-epoch under the lock, runs crypto → allocate → DMA → publish
//! UNLOCKED, then re-acquires and REVALIDATES: epoch unchanged ⇒ merge
//! (as today); superseded / entry-gone ⇒ the stale completion publishes
//! NOTHING — it frees its orphan offset and leaves the parked buffer
//! (the retained dirty authority) to the newest completion's task. The
//! CQE-supersession law (KD-2.4): a superseded DMA's completion must
//! not publish a stale generation — FIND-M11-A's sentence applied
//! intra-mount, under the lock that orders publishes.
//!
//! Schedule driver: `SQUEEZEFS_TEST_UPLOAD_STALL_MS`
//! (`set_test_upload_stall_ms`) stalls the UNLOCKED window between the
//! snapshot and the DMA — the deterministic form of "a rewrite lands
//! while the prior image is in flight" (the
//! `SQUEEZEFS_TEST_WRITE_STALL_MS` seam pattern).
//!
//! Contracts:
//! 1. **Planted-stale-CQE**: a full-block rewrite landing while the
//!    prior upload stalls in flight wins — reads observe ONLY the
//!    newest bytes, exactly one image publishes, the superseded
//!    completion is counted, and its orphan offset is freed (allocator
//!    accounting exact).
//! 2. **Entry-gone skip**: an fsync flush that durably publishes while
//!    the pipeline task stalls makes the resumed task a counted no-op —
//!    it frees its orphan and never re-publishes over the flush.
//! 3. **The in-place lever keeps the serialized arm**: with
//!    `SQUEEZEFS_INPLACE_OVERWRITE=1` the upload holds its lock across
//!    the DMA (the live-offset mutation must stay serialized) — zero
//!    supersessions under the same schedule.
//!
//! RED against `464ea02`: the pipeline upload is lock-serialized end to
//! end — no supersession machinery, no counters, no stall seam.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore default posture on scope exit (knob hygiene). The W1 patch
/// path is disabled for these schedules (`set_patch_max_bytes(0)` — the
/// acceptance A/B lever): a lone whole-block overwrite would otherwise
/// ride the in-place sub-block patch and never enter the pipeline whose
/// supersession these contracts pin.
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_test_upload_stall_ms(0);
        squeezefs::fuse_client::set_inplace_overwrite(false);
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
        squeezefs::routing::set_rewrite_shadow(true);
    }
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make_harness(test_id: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let m = NamedTempFile::new().unwrap();
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
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
            .await
            .expect("open v3 meta volume"),
    ]));
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
        fs: Arc::new(fs),
        req,
        backing,
        _m: m,
        _s: s,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
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

async fn create_file(h: &H, name: &str) -> u64 {
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

/// Wait (bounded) until `n` pipeline tasks have entered the armed stall
/// window — the deterministic sequencing observable (never a sleep-sync:
/// the seam itself is the schedule, this just confirms arrival).
async fn await_stall_entries(base: u64, n: u64) {
    for _ in 0..1000 {
        if squeezefs::fuse_client::test_upload_stall_entries() - base >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("stalled tasks never entered the seam window");
}

fn supersessions() -> u64 {
    METRICS.write_pipeline_supersessions.load(Ordering::Relaxed)
}
fn superseded_bytes() -> u64 {
    METRICS
        .write_pipeline_superseded_bytes
        .load(Ordering::Relaxed)
}

async fn quiesce(h: &H) {
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
}

// ---------------------------------------------------------------------------
// Contract 1 — planted-stale-CQE: the newest image wins, exactly once.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_inflight_upload_never_publishes_and_newest_bytes_win() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    // Durable-publish machinery venue: the rewrite-shadow epoch (Idea 1)
    // parks displaced keys and defers the durable rebind — pinned off
    // here (tests/rewrite_shadow_tests.rs owns the epoch venue; the
    // supersession law itself is layout-agnostic).
    squeezefs::routing::set_rewrite_shadow(false);
    let h = make_harness("supersede_planted_stale").await;
    let ino = create_file(&h, "f1").await;
    let blocks = 2u64;
    let len = (blocks * FBS) as usize;

    // Seed a striped file (v0), drained + durable.
    write_at(&h, ino, 0, &pattern(len, 1)).await;
    quiesce(&h).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync v0");
    {
        let path = squeezefs::keys::inode_path(ino);
        h.fs.router.metadata_cache.remove(&ino);
        let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
        assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    }

    // Stall the unlocked upload window; land v1 on BLOCK 0 ONLY (one
    // pipeline task — the schedule must not depend on cross-task
    // interleaving), WAIT until it parks in the window, then land v2 —
    // v2's merge bumps the parked entry's write epoch while v1's image
    // is in flight.
    squeezefs::fuse_client::set_test_upload_stall_ms(400);
    let e0 = squeezefs::fuse_client::test_upload_stall_entries();
    let (s0, sb0, wt0) = (
        supersessions(),
        superseded_bytes(),
        METRICS.write_through_blocks.load(Ordering::Relaxed),
    );
    let v1 = pattern(FBS as usize, 2);
    let v2 = pattern(FBS as usize, 3);
    write_at(&h, ino, 0, &v1).await;
    await_stall_entries(e0, 1).await;
    write_at(&h, ino, 0, &v2).await;
    quiesce(&h).await;
    squeezefs::fuse_client::set_test_upload_stall_ms(0);

    assert!(
        supersessions() - s0 >= 1,
        "every stale in-flight completion must be counted superseded \
         (write_pipeline_supersessions is the latest-wins engagement \
         instrument): got {} [stall_entries={} wt_blocks={} patch_writes={} \
         staging_wt_fallback={} fallbacks={}]",
        supersessions() - s0,
        squeezefs::fuse_client::test_upload_stall_entries() - e0,
        METRICS.write_through_blocks.load(Ordering::Relaxed) - wt0,
        METRICS.patch_writes.load(Ordering::Relaxed),
        METRICS
            .staging_put_bytes_wt_fallback
            .load(Ordering::Relaxed),
        METRICS.write_through_fallbacks.load(Ordering::Relaxed),
    );
    assert!(
        superseded_bytes() - sb0 >= FBS,
        "superseded bytes account the elided stale publish"
    );

    // Exactly one image published per unique block for the two
    // overlapping rewrites.
    assert_eq!(
        METRICS.write_through_blocks.load(Ordering::Relaxed) - wt0,
        1,
        "latest-wins: ONE durable publish for two overlapping rewrites \
         of the block (device writes ≈ unique blocks, not ops)"
    );

    // Reads observe only the newest bytes on block 0; block 1 keeps v0.
    let got =
        h.fs.read(h.req, ino, 0, 0, FBS as u32, 0)
            .await
            .expect("read")
            .data
            .to_vec();
    assert_eq!(got, v2, "the newest image wins");

    // The newest bytes are on the device at block 0's mapped offset.
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    use std::os::unix::fs::FileExt;
    let dev = std::fs::File::open(h.backing.path()).expect("open backing");
    let mut buf = vec![0u8; FBS as usize];
    let key = meta
        .block_map
        .as_ref()
        .and_then(|m| m.get(&0).cloned())
        .expect("mapped");
    let (_, off) = h.fs.router.backend_router.parse_block_key(&key).unwrap();
    dev.read_exact_at(&mut buf, off).expect("pread device");
    assert_eq!(buf, v2, "device bytes are the newest image");

    // No leak: the superseded uploads' orphan offsets were freed. After
    // draining the reclaim queue the allocator tracks exactly `blocks`
    // live blocks for this file.
    h.fs.router.backend_router.reclaim_drain().await;
    assert_eq!(
        h.fs.router.block_allocator.get_used_blocks(),
        blocks,
        "orphan accounting exact: v0's displaced blocks and the \
         superseded images' offsets are all freed"
    );
}

// ---------------------------------------------------------------------------
// Contract 2 — entry-gone skip: a durable flush wins over a stalled task.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flush_that_wins_makes_the_stalled_task_a_counted_noop() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    // Durable-publish machinery venue: the rewrite-shadow epoch (Idea 1)
    // parks displaced keys and defers the durable rebind — pinned off
    // here (tests/rewrite_shadow_tests.rs owns the epoch venue; the
    // supersession law itself is layout-agnostic).
    squeezefs::routing::set_rewrite_shadow(false);
    let h = make_harness("supersede_flush_wins").await;
    let ino = create_file(&h, "f1").await;

    // Striped 2-block fixture; the rewrite targets block 0 only (one
    // pipeline task — see contract 1's schedule note).
    let blocks = 2u64;
    let len = (blocks * FBS) as usize;
    write_at(&h, ino, 0, &pattern(len, 1)).await;
    quiesce(&h).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync v0");

    squeezefs::fuse_client::set_test_upload_stall_ms(400);
    let e0 = squeezefs::fuse_client::test_upload_stall_entries();
    let s0 = supersessions();
    let v1 = pattern(FBS as usize, 9);
    write_at(&h, ino, 0, &v1).await;
    await_stall_entries(e0, 1).await;
    // fsync's flush leg takes the block lock the stalled task dropped,
    // uploads v1 durably and retires the entry; the resumed task must
    // observe entry-gone and free its orphan without re-publishing.
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    quiesce(&h).await;
    squeezefs::fuse_client::set_test_upload_stall_ms(0);

    assert!(
        supersessions() - s0 >= 1,
        "the entry-gone arm counts as a supersession (the flush leg's \
         durable publish is the surviving generation): got {}",
        supersessions() - s0
    );
    let got =
        h.fs.read(h.req, ino, 0, 0, FBS as u32, 0)
            .await
            .expect("read")
            .data
            .to_vec();
    assert_eq!(got, v1, "the flushed bytes serve");

    h.fs.router.backend_router.reclaim_drain().await;
    assert_eq!(
        h.fs.router.block_allocator.get_used_blocks(),
        blocks,
        "the stalled task's orphan offset was freed"
    );
}

// ---------------------------------------------------------------------------
// Contract 3 — the in-place lever keeps the serialized arm.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inplace_lever_keeps_the_serialized_upload() {
    let _g = serial().await;
    let _l = LeverGuard;
    squeezefs::fuse_client::set_inplace_overwrite(true);
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(false);
    let h = make_harness("supersede_inplace_serialized").await;
    let ino = create_file(&h, "f1").await;

    let blocks = 2u64;
    let len = (blocks * FBS) as usize;
    write_at(&h, ino, 0, &pattern(len, 1)).await;
    quiesce(&h).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync v0");

    squeezefs::fuse_client::set_test_upload_stall_ms(300);
    let e0 = squeezefs::fuse_client::test_upload_stall_entries();
    let s0 = supersessions();
    let v2 = pattern(FBS as usize, 4);
    write_at(&h, ino, 0, &pattern(FBS as usize, 3)).await;
    write_at(&h, ino, 0, &v2).await;
    quiesce(&h).await;
    squeezefs::fuse_client::set_test_upload_stall_ms(0);

    assert_eq!(
        supersessions() - s0,
        0,
        "SQUEEZEFS_INPLACE_OVERWRITE=1 keeps the under-lock serialized \
         upload (a live-offset in-place DMA must never run unlocked — \
         KD-2.3); zero supersessions under the same schedule"
    );
    assert_eq!(
        squeezefs::fuse_client::test_upload_stall_entries() - e0,
        0,
        "the serialized arm never enters the supersession stall window"
    );
    let got =
        h.fs.read(h.req, ino, 0, 0, FBS as u32, 0)
            .await
            .expect("read")
            .data
            .to_vec();
    assert_eq!(got, v2, "serialized last-writer-wins still holds");
    let _ = len;
}
