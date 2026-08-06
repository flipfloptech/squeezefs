//! The fsync-vs-writeback tail-loss bug (P0 data loss, field-characterized
//! `.benchmarks/2026-08-06-fuse-zc-serve.md` §5b; campaign evidence
//! `.benchmarks/2026-08-06-fsync-writeback-tail-loss.md`).
//!
//! THE PROVEN INTERLEAVE (local devsub tape, 9/10 natural corruption on
//! `cp 32MiB f && sync f`):
//!
//! 1. Kernel writeback delivers the tail block's WRITEs; the last merge
//!    completes the coverage union; the detached pipeline upload task is
//!    admitted; the WRITE ACKs with custody parked (writeback-cache law).
//! 2. `close(2)` → RELEASE captures the write-era fencing token, spawns
//!    the background flush with it, then — last close — releases the
//!    cached lease.
//! 3. `sync <file>` → FSYNC → `acquire_write_lease` mints a NEW lease:
//!    the ino's fencing generation advances (process-local rotation).
//! 4. The RELEASE-background flush reaches the still-parked tail block
//!    first (the detached task is pre-DMA), drives the write-through leg
//!    with its now-stale captured token, and the publish merge refuses
//!    `FencingTokenExpired` — whereupon the leg RETIRED the parked
//!    overlay and published NOTHING ("a fenced writer must not publish").
//! 5. The detached task revalidates, finds the entry gone, and per its
//!    contract assumes "the surviving generation is durable (flush leg)"
//!    — frees its orphan and returns. The ONLY copy of the acked bytes
//!    is gone; the block map never names the block; the file's tail
//!    reads back as a hole (zeros), persistently. Every tripwire silent.
//!
//! THE LAW THESE TESTS PIN (FIND-M11-A extended to live RAM custody):
//! within one process, `FencingTokenExpired` from a publish merge can
//! only mean this same daemon re-acquired the ino's lease between token
//! capture and merge revalidation (open/close churn). It is NEVER the
//! cross-mount fence — that is the D0 guard latch, surfacing as
//! `WriterGuardFenced` from `authorize_dma`. The disposition for live
//! parked custody is RE-PRESENT THE CURRENT GENERATION AND RETRY
//! (`writeback_stale_token_retries` — the staged ladder's own transient
//! contract, `flush_one_active_block` doc), never a custody drop.
//! "Stale fencing tokens discard staged work" stays the REMOUNT
//! contract only.
//!
//! RED (2026-08-06, against `58703f20`): all three tests fail — the
//! flush write-through leg and `write_through_complete_block` drop
//! custody on the stale token, and the end-to-end release→fsync shape
//! zeroes the tail block.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    block_lock_acquire, set_test_upload_stall_ms, test_upload_stall_entries, BlockLockSite,
    SqueezefsFilesystem, METRICS,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536;

/// Process-global knobs + METRICS deltas: suite serializes (house
/// pattern, `write_pipeline_tests`).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Reset the upload-stall seam on scope exit (panic hygiene).
struct StallGuard;
impl Drop for StallGuard {
    fn drop(&mut self) {
        set_test_upload_stall_ms(0);
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // Pin the W1 patch path OFF (the downscaled BS would make sub-block
    // segments patch-eligible and bypass the machinery under test).
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
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
        .unwrap()
        .attr
        .ino
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
        .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// A striped fixture: > 1 block so the layout classifies striped, then
/// fsync so the map is published and the pipeline is drained.
async fn striped_fixture(h: &H, name: &str) -> u64 {
    let ino = create(h, name).await;
    let base = pattern(2 * BS as usize, 0x11);
    write_at(h, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "fixture pipeline must drain"
    );
    ino
}

async fn block_map_has(h: &H, ino: u64, b: u32) -> bool {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .ok()
        .and_then(|m| m.block_map.as_ref().map(|bm| bm.contains_key(&b)))
        .unwrap_or(false)
}

async fn eventually(mut cond: impl AsyncFnMut() -> bool, what: &str) {
    for _ in 0..600 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never held within 6s: {what}");
}

/// T1 — the field interleave's loss unit, deterministic: parked
/// coverage-complete custody (its detached upload stalled pre-DMA in
/// the unlocked window — exactly where it was in the tape) flushed by
/// the RELEASE-background pass presenting its CAPTURED, since-rotated
/// token. The flush must CONVERGE — publish the parked bytes under the
/// current generation and return Ok — never retire unpublished custody.
///
/// RED: the write-through leg propagates `FencingTokenExpired`, the
/// block map never names the block, and the readback is zeros — the
/// exact stored corruption of the `cp && sync <file>` field shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_captured_token_flush_converges_and_never_drops_custody() {
    let _s = serial().await;
    let _stall = StallGuard;
    let h = make(*b"fwt-t1-converge!", "fwt_ns_t1").await;
    let ino = striped_fixture(&h, "t1").await;

    // Detached uploads park in their unlocked pre-DMA window: the block
    // lock is FREE while the task is mid-flight (the tape's schedule).
    let entries0 = test_upload_stall_entries();
    set_test_upload_stall_ms(2_000);

    // The tail block: one covering write ACKs with custody parked and
    // the pipeline task admitted.
    let data = pattern(BS as usize, 0x5A);
    write_at(&h, ino, 2 * BS, &data).await;
    eventually(
        async || test_upload_stall_entries() > entries0,
        "the detached upload must be parked in its stall window",
    )
    .await;

    // The RELEASE-background flush's captured token: one lease rotation
    // behind the current generation (the FSYNC acquire that made it
    // stale in the field).
    let current = h.fs.dlm().get_fencing_token_ino(ino);
    assert!(current > 0, "the write path must have acquired a lease");
    let stale = current - 1;

    let res = h.fs.flush_memory_buffers_for_inode(ino, stale).await;
    assert!(
        res.is_ok(),
        "a process-local token rotation must converge (the ino's current \
         generation re-presented), never fail the flush: {res:?}"
    );
    assert!(
        block_map_has(&h, ino, 2).await,
        "the flush must PUBLISH the parked custody — a retire without a \
         publish is the tail-loss bug"
    );

    // Release the stalled task: it revalidates, finds the custody
    // already durable, and no-ops.
    set_test_upload_stall_ms(0);
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the detached task must drain"
    );
    let got = read_at(&h, ino, 2 * BS, BS as usize).await;
    assert_eq!(
        got, data,
        "the tail block must read back the acked bytes — all-zeros here \
         is the stored corruption"
    );
}

/// T2 — the write-through unit's disposition (the detached pipeline's
/// thin read→merge race face): `write_through_complete_block` presented
/// a token the process has since rotated must RETRY with the current
/// generation and publish — custody is the newest bytes in existence
/// and may never be dropped on this error class. Engagement is counted
/// on `writeback_stale_token_retries` (the staged ladder's transient
/// contract, one law).
///
/// RED: the unit retires the overlay, publishes nothing, and propagates
/// `FencingTokenExpired` (the pre-fix pinned behavior).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_token_write_through_unit_converges_and_publishes() {
    let _s = serial().await;
    let _stall = StallGuard;
    let h = make(*b"fwt-t2-unitretry", "fwt_ns_t2").await;
    let ino = striped_fixture(&h, "t2").await;

    let entries0 = test_upload_stall_entries();
    set_test_upload_stall_ms(2_000);
    let data = pattern(BS as usize, 0x77);
    write_at(&h, ino, 3 * BS, &data).await;
    eventually(
        async || test_upload_stall_entries() > entries0,
        "the detached upload must be parked in its stall window",
    )
    .await;

    let cache_key = squeezefs::keys::active_block(ino, 3).to_string();
    let current = h.fs.dlm().get_fencing_token_ino(ino);
    assert!(current > 0, "the write path must have acquired a lease");
    let retries0 = METRICS
        .writeback_stale_token_retries
        .load(Ordering::Relaxed);

    let guard = block_lock_acquire(ino, 3, BlockLockSite::PipelineUpload).await;
    let res =
        h.fs.write_through_complete_block(ino, 3, &cache_key, current - 1, guard)
            .await;
    assert!(
        res.is_ok(),
        "a stale presented token on live parked custody must converge by \
         re-presenting the current generation, not drop custody: {res:?}"
    );
    assert!(
        block_map_has(&h, ino, 3).await,
        "the converged write-through must have published"
    );
    assert!(
        METRICS
            .writeback_stale_token_retries
            .load(Ordering::Relaxed)
            > retries0,
        "the retry must be counted on writeback_stale_token_retries \
         (one transient-fencing law across staged and RAM custody)"
    );

    set_test_upload_stall_ms(0);
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the detached task must drain"
    );
    let got = read_at(&h, ino, 3 * BS, BS as usize).await;
    assert_eq!(got, data, "read-back after the converged publish");
}

/// T3 — the end-to-end field shape through the REAL handlers, iterated:
/// write → RELEASE (spawns the background flush with its captured
/// token, drops the lease) → immediate FSYNC (a fresh lease: the
/// rotation) → drain → every byte reads back. The schedule the loop
/// explores is exactly the `cp <f> mnt/f && sync mnt/f` race (5/10
/// corrupt in the field, 9/10 on the local devsub); post-fix EVERY
/// schedule converges, so the loop is deterministic-green forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_then_fsync_never_zeroes_the_tail_block() {
    let _s = serial().await;
    let _stall = StallGuard;
    let h = make(*b"fwt-t3-e2e-shape", "fwt_ns_t3").await;

    const BLOCKS: u64 = 4;
    for round in 0..20u64 {
        let ino = create(&h, &format!("t3-{round}")).await;
        let data = pattern((BLOCKS * BS) as usize, (round % 200) as u8);
        // The detached uploads sit pre-DMA (kernel-writeback in flight)
        // while close-then-sync races them — the tape's schedule.
        set_test_upload_stall_ms(150);
        for b in 0..BLOCKS {
            write_at(&h, ino, b * BS, &data[(b * BS) as usize..((b + 1) * BS) as usize]).await;
        }
        // close(2): RELEASE spawns the background flush with the
        // write-era token, then releases the lease.
        h.fs.release(h.req, ino, 0, 0, 0, true).await.unwrap();
        // sync <file>: FSYNC re-acquires — the rotation that makes the
        // background flush's captured token stale mid-pass.
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        set_test_upload_stall_ms(0);
        assert!(
            h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
            "round {round}: pipeline must drain"
        );
        // Let the release-background flush finish too (it holds no
        // custody the reads below cannot see; bounded settle).
        let h_ref = &h;
        eventually(
            async || {
                let got = read_at(h_ref, ino, 0, (BLOCKS * BS) as usize).await;
                got == data
            },
            "round: every acked byte must read back (an all-zeros tail \
             block here is the field corruption)",
        )
        .await;
        // The durable map must name every block (the stored state, not
        // a RAM overlay serving over a hole).
        for b in 0..BLOCKS as u32 {
            assert!(
                block_map_has(&h, ino, b).await,
                "round {round}: block {b} must be published durably"
            );
        }
    }
}
