//! Terminal-free device reclaim economy — the shim-write-amplification fix
//! (`.benchmarks/2026-07-27-shim-write-amplification.md`).
//!
//! Field capture (6-node cluster, relaxed seq-write 1 MiB shim row): device
//! 6.6 GB/s at 98% util serving user 3.6 GB/s = **1.85× write
//! amplification**. Reproduced at exactly **2.000×** on the nvmet-tcp rig
//! (`tests/write_amp_rig.sh`), kernel AND shim paths alike, with the extra
//! device bytes arithmetically equal to `freed blocks × block_size`:
//! `BackendRouter::free_block`'s terminal-release `fallocate(PUNCH_HOLE)`
//! on a RAW BLOCK DEVICE is `blkdev_issue_zeroout` — a full block of
//! Write-Zeroes WRITE bandwidth per freed block, so every steady-state
//! overwrite/delete stream pays ~+1.0× device writes. (The RW3b coverage
//! union itself held: even 64 KiB out-of-order chunk streams produced
//! exactly one write-through per block — pinned next door in
//! `write_through_coverage_tests`.)
//!
//! Contract pinned here:
//!
//! 1. **Reclaim-op classification**: a terminal free reclaims a REGULAR
//!    FILE backing with `PUNCH_HOLE` (host-FS metadata — the original
//!    sparse-backing ENOSPC motivation, no I/O bandwidth) and a BLOCK
//!    DEVICE with `BLKDISCARD` (NVMe Deallocate — a range command with no
//!    data payload, not write-bandwidth-accounted). **Never Write Zeroes.**
//! 2. **Counted reclaims**: `block_free_file_punches`/`block_free_discards`
//!    (+ byte forms) account every terminal reclaim — the live-mount face
//!    of the rig's diskstats instrument. `block_free_reclaim_skipped`
//!    counts refused/unsupported reclaims (never retried as zeroout).
//! 3. **Terminal-only**: non-terminal (clone-shared) frees reclaim nothing
//!    and count nothing (extends `refcount_clone_tests` contract A).
//! 4. **Displacement rides the seam**: a striped overwrite's displaced-key
//!    frees are counted reclaims — the ledger a field row reconciles
//!    against.
//!
//! RED against dev e314bec: `free_reclaim_op` does not exist; the free path
//! punches unconditionally and counts nothing.
//!
//! Async block-reclaim amendment (perf/async-block-reclaim,
//! `tests/async_block_reclaim_tests.rs`): the reclaim now runs on the
//! background reclaimer, not inside `free_block` — the counting contracts
//! here are unchanged but assert after `reclaim_drain()` (same ledger,
//! background venue).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::METRICS;
use squeezefs::routing::{free_reclaim_op, DataRouter, FreeReclaimOp};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

/// METRICS is process-global; counter-delta tests serialize (same pattern
/// as `write_through_tests`).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

// ---------------------------------------------------------------------------
// Contract 1 — classification (pure; mode bits from MetadataExt::mode()).
// ---------------------------------------------------------------------------

#[test]
fn classify_block_device_discards_never_zero_writes() {
    assert!(
        matches!(
            free_reclaim_op(libc::S_IFBLK | 0o600),
            FreeReclaimOp::BdevDiscard
        ),
        "a raw-namespace terminal free must deallocate (BLKDISCARD), never \
         PUNCH_HOLE (= blkdev_issue_zeroout = a full block of Write-Zeroes \
         bandwidth — the 1.85×/2.000× amplification mechanism)"
    );
}

#[test]
fn classify_regular_file_punches() {
    assert!(
        matches!(
            free_reclaim_op(libc::S_IFREG | 0o644),
            FreeReclaimOp::FilePunch
        ),
        "file-backed volumes keep PUNCH_HOLE: sparse-backing space reclaim \
         is host-FS metadata, not device I/O"
    );
}

#[test]
fn classify_anything_else_skips() {
    // A char device / fifo / unknown mode must neither punch nor discard —
    // and must never degrade into a zeroing write.
    assert!(matches!(
        free_reclaim_op(libc::S_IFCHR | 0o600),
        FreeReclaimOp::Skip
    ));
    assert!(matches!(free_reclaim_op(0), FreeReclaimOp::Skip));
}

// ---------------------------------------------------------------------------
// Contracts 2–4 — the router free path, file-backed harness.
// ---------------------------------------------------------------------------

async fn make_router() -> (
    DataRouter,
    Arc<BlockAllocator>,
    NamedTempFile,
    tempfile::TempDir,
) {
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new("block_free_reclaim_test")
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
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, ba.clone(), nvme);
    (router, ba, b, s)
}

fn punches() -> u64 {
    METRICS.block_free_file_punches.load(Ordering::Relaxed)
}
fn punch_bytes() -> u64 {
    METRICS.block_free_punch_bytes.load(Ordering::Relaxed)
}
fn discards() -> u64 {
    METRICS.block_free_discards.load(Ordering::Relaxed)
}
fn skipped() -> u64 {
    METRICS.block_free_reclaim_skipped.load(Ordering::Relaxed)
}

/// Contract 2: a terminal free on a file backing is ONE counted punch —
/// and the backing range actually deallocates (the original ENOSPC
/// motivation stays alive). Since the async block-reclaim fix the punch
/// is QUEUED off the write path (`tests/async_block_reclaim_tests.rs`);
/// the ledger asserts after a drain — same counts, background venue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_free_on_file_backing_is_one_counted_punch() {
    let _g = serial().await;
    let (router, ba, backing, _s) = make_router().await;

    let offset = ba.allocate_block().await.expect("alloc");
    let key = offset.to_string();
    let pattern: Vec<u8> = (0..8192usize).map(|i| (i % 251) as u8).collect();
    router
        .nvme_writer
        .write_block(offset, bytes::Bytes::from(pattern))
        .await
        .expect("write");
    ba.publish_block(offset);

    let (p0, pb0, d0, s0) = (punches(), punch_bytes(), discards(), skipped());
    router
        .backend_router
        .free_block(&key)
        .await
        .expect("terminal free");
    router.backend_router.reclaim_drain().await;

    assert_eq!(punches() - p0, 1, "terminal file-backed free = one punch");
    let bs = router.block_size.load(Ordering::Relaxed);
    assert_eq!(punch_bytes() - pb0, bs, "punch bytes = block size");
    assert_eq!(discards() - d0, 0, "no discard on a file backing");
    assert_eq!(skipped() - s0, 0, "nothing skipped");

    // The punched range reads zeros from the backing (deallocated, and a
    // reused offset can never serve the dead incarnation's bytes).
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(backing.path()).expect("open backing");
    let mut buf = vec![0u8; 8192];
    f.read_exact_at(&mut buf, offset).expect("pread");
    assert!(
        buf.iter().all(|&x| x == 0),
        "file-backed punch must still deallocate the freed range"
    );
}

/// Contract 3: a NON-terminal free (clone-shared) reclaims nothing and
/// counts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nonterminal_free_reclaims_and_counts_nothing() {
    let _g = serial().await;
    let (router, ba, _backing, _s) = make_router().await;

    let offset = ba.allocate_block().await.expect("alloc");
    let key = offset.to_string();
    ba.publish_block(offset);
    assert!(router.backend_router.increment_refcount(&key), "clone pin");

    let (p0, d0, s0) = (punches(), discards(), skipped());
    router
        .backend_router
        .free_block(&key)
        .await
        .expect("non-terminal free");
    assert_eq!(punches() - p0, 0, "non-terminal free must not punch");
    assert_eq!(discards() - d0, 0, "non-terminal free must not discard");
    assert_eq!(skipped() - s0, 0, "non-terminal free is not a skip");
}

/// Contract 4: batched frees (`free_blocks` — the unlink/truncate shape)
/// count one reclaim per terminal key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batched_frees_count_one_reclaim_per_key() {
    let _g = serial().await;
    let (router, ba, _backing, _s) = make_router().await;

    let mut keys = Vec::new();
    for _ in 0..4 {
        let offset = ba.allocate_block().await.expect("alloc");
        ba.publish_block(offset);
        keys.push(offset.to_string());
    }
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();

    let p0 = punches();
    router
        .backend_router
        .free_blocks(&key_refs)
        .await
        .expect("batched free");
    router.backend_router.reclaim_drain().await;
    assert_eq!(
        punches() - p0,
        4,
        "every terminal free in the batch is a counted reclaim — the \
         ledger a field row's device-byte delta reconciles against"
    );
}
