//! PR 4 of docs/design-zero-copy-write-path.md (§5.3): complete-block
//! write-through — bypass staging for content-complete blocks.
//!
//! Contract under test:
//!
//! - **Trigger parity**: write-through fires exactly at today's
//!   `is_block_complete` point (`write_end == b_end_offset`) for every entry
//!   kind (Fresh / Seeded / one-shot) and every fill order (sequential,
//!   tail-first, gap, middle-last) — only the *destination* changes: a
//!   content-complete block goes crypto → allocate → DMA → block-map merge
//!   instead of staging-mmap + writeback. Observable as: no staging entry
//!   for the completed block, the mapping present in the authoritative
//!   backend meta immediately after the write acks, and the
//!   `write_through_blocks` / `write_through_bytes` stats moving.
//! - **Uncovered-range semantics under memset elision**: a Fresh
//!   accumulation buffer no longer zero-fills at seed time; its `covered`
//!   interval is pure memset-elision bookkeeping (never trigger input), and
//!   recycled pool bytes never leave the covered range — not to the kernel
//!   (coverage-aware read hit), not to staging / the device (zero-complete
//!   under the block lock at the trigger and at every stage/upload exit).
//! - **One merge discipline**: every striped block-map RMW goes through
//!   `DataRouter::merge_block_mappings` under `INODE_META_LOCKS` —
//!   write-through, the fallback/fsync flush paths, the staging-refusal
//!   escalation, truncate shrink (`TruncateFrom`) *and* grow (degenerate
//!   `Merge(&[])`), fallocate-extend, and defrag `BlockMove` — so the
//!   cross-discipline interleavings (i)-(v) can never lose an update.
//! - **Never-lossy fallback**: a failed write-through (device error via
//!   `FAIL_NEXT_WRITES`) degrades into today's staging path; the write still
//!   acks, the data is readable and durably flushable, and
//!   `write_through_fallbacks` records it.
//! - **Spill discipline**: the RAM-cap spill acquires the victim's
//!   `BLOCK_FLUSH_LOCKS` via `try_lock` (a blocking acquire can self-deadlock
//!   on a shared stripe shard while the inserter holds its own block lock)
//!   and zero-completes Fresh victims before they reach staging.
//!
//! RED against current code: the coverage API (`ActiveBlockBuf::fresh`,
//! `record_write`, `covered_snapshot`, `zero_complete`), the merge primitive
//! (`DataRouter::merge_block_mappings`, `BlockMapOp`, `LayoutFlip`) and the
//! `write_through_*` stats do not exist yet (compile failure); with the API
//! present, the write-through tests fail because completed blocks today go
//! to staging + a writeback queue nobody drains in this harness (no
//! background worker without `init()`), so backend maps stay empty until an
//! explicit teardown flush.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use fuse3::SetAttr;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::active_block::ActiveBlockBuf;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::{SqueezefsFilesystem, BLOCK_FLUSH_LOCKS, METRICS};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

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

const BS: u64 = 4096;

/// The write-through stats are process-global; tests asserting counter
/// deltas serialize (same pattern as `writeback_tests` / `nvme_dev_tests`).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make_with(test_id: &str, write_disk: &str, block_size: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size);
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some(write_disk),
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
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 256 * 1024 * 1024).await,
    ]));
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

async fn make(test_id: &str) -> H {
    make_with(test_id, "64MB", "4096").await
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
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 + seed as u64) % 251) as u8)
        .collect()
}

/// Authoritative backend meta (bypasses the RAM TTL cache).
async fn backend_meta(h: &H, ino: u64) -> squeezefs::routing::CachedMetadata {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&path);
    h.fs.router.fetch_metadata(&path).await.unwrap()
}

/// Make `ino` striped via the fresh-file direct striped route (first big
/// write of a fresh file never stages), returning the content written.
async fn make_striped(h: &H, ino: u64, len: usize, seed: u8) -> Vec<u8> {
    let p = pattern(len, seed);
    write_at(h, ino, 0, &p).await;
    let meta = backend_meta(h, ino).await;
    assert_eq!(meta.file_type, "striped", "premise: file must be striped");
    p
}

fn staged_key(ino: u64, b: u64) -> String {
    squeezefs::keys::active_block(ino, b).to_string()
}

fn wt_blocks() -> u64 {
    METRICS.write_through_blocks.load(Ordering::Relaxed)
}

fn wt_bytes() -> u64 {
    METRICS.write_through_bytes.load(Ordering::Relaxed)
}

fn wt_fallbacks() -> u64 {
    METRICS.write_through_fallbacks.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Coverage bookkeeping (§5.3 accumulation state machine, unit level)
// ---------------------------------------------------------------------------

/// Fresh buffers are born uncovered (memset elided); `record_write` extends
/// the covered interval; `zero_complete` zeroes exactly the uncovered
/// complement. The `make_mut` garbage fill simulates recycled pool memory —
/// without zero-complete those bytes would escape as another file's content.
#[tokio::test]
async fn test_covered_interval_tracking_and_zero_complete() {
    let mut buf = ActiveBlockBuf::fresh(4096);
    assert_eq!(buf.covered(), (0, 0), "fresh buffer must be born uncovered");
    assert!(!buf.is_content_valid());

    // Simulate recycled pool garbage: raw mutation without coverage.
    buf.make_mut().fill(0xAB);

    // Sequential-ish fill: [1024, 2048).
    buf.record_write(1024, 2048);
    buf.make_mut()[1024..2048].copy_from_slice(&pattern(1024, 3));
    assert_eq!(buf.covered(), (1024, 2048));
    assert!(!buf.is_content_valid());

    let (snap, covered) = buf.covered_snapshot();
    assert_eq!(covered, (1024, 2048));
    assert_eq!(&snap[1024..2048], &pattern(1024, 3)[..]);

    // Overlapping extension: [1536, 3072) merges into [1024, 3072).
    buf.record_write(1536, 3072);
    assert_eq!(buf.covered(), (1024, 3072));

    // Abutting extensions reach full coverage with zero memset — the
    // elided-bytes stat is the observable.
    let elided_before = METRICS
        .active_block_memset_elided_bytes
        .load(Ordering::Relaxed);
    buf.record_write(3072, 4096);
    assert_eq!(buf.covered(), (1024, 4096));
    assert!(!buf.is_content_valid());
    buf.record_write(0, 1024);
    // Full coverage: [0, 4096).
    assert_eq!(buf.covered(), (0, 4096));
    assert!(buf.is_content_valid());
    assert!(
        METRICS
            .active_block_memset_elided_bytes
            .load(Ordering::Relaxed)
            > elided_before,
        "reaching full coverage by writes alone must record elided memset bytes"
    );

    // Second buffer: zero_complete on a partial fill zeroes the complement.
    let mut buf2 = ActiveBlockBuf::fresh(4096);
    buf2.make_mut().fill(0xCD); // recycled garbage
    buf2.record_write(1024, 2048);
    buf2.make_mut()[1024..2048].copy_from_slice(&pattern(1024, 9));
    buf2.zero_complete();
    assert!(buf2.is_content_valid());
    let s = buf2.snapshot();
    assert!(
        s[..1024].iter().all(|&x| x == 0),
        "uncovered head must be zeroed (recycled bytes must never escape)"
    );
    assert_eq!(&s[1024..2048], &pattern(1024, 9)[..]);
    assert!(
        s[2048..].iter().all(|&x| x == 0),
        "uncovered tail must be zeroed (recycled bytes must never escape)"
    );

    // zero_complete on a content-valid buffer is a no-op (idempotent).
    buf2.zero_complete();
    assert_eq!(&buf2.snapshot()[1024..2048], &pattern(1024, 9)[..]);

    // Seeded buffers are born content-valid (fully covered).
    let seeded = ActiveBlockBuf::seeded(&[7u8; 100], 4096);
    assert!(seeded.is_content_valid());
    assert_eq!(seeded.covered(), (0, 4096));
}

/// Gap write → zero the complement and degrade to fully-initialized
/// (Accumulating_Fresh → Accumulating_Seeded in the §5.3 state machine).
#[tokio::test]
async fn test_gap_write_degrades_to_fully_initialized() {
    let mut buf = ActiveBlockBuf::fresh(4096);
    buf.make_mut().fill(0xEE); // recycled garbage

    buf.record_write(0, 100);
    buf.make_mut()[0..100].fill(1);
    assert_eq!(buf.covered(), (0, 100));

    // Gap write [200, 300): the buffer must become content-valid, with the
    // gap [100, 200) and the tail [300, 4096) zeroed — never 0xEE.
    buf.record_write(200, 300);
    buf.make_mut()[200..300].fill(2);
    assert!(
        buf.is_content_valid(),
        "gap write must degrade the buffer to fully-initialized"
    );
    let s = buf.snapshot();
    assert!(s[0..100].iter().all(|&x| x == 1));
    assert!(
        s[100..200].iter().all(|&x| x == 0),
        "gap must read zeros, not recycled bytes"
    );
    assert!(s[200..300].iter().all(|&x| x == 2));
    assert!(
        s[300..].iter().all(|&x| x == 0),
        "tail must read zeros, not recycled bytes"
    );
}

// ---------------------------------------------------------------------------
// Write-through trigger matrix (FS level)
// ---------------------------------------------------------------------------

/// Sequential complete block: the block-completing write uploads directly —
/// no staging entry, mapping in the authoritative backend map immediately,
/// stats move, and (mirroring flush semantics) the striped block never
/// enters the read LRU.
#[tokio::test]
async fn test_sequential_complete_block_writes_through_without_staging() {
    let _g = serial().await;
    let h = make("wt_seq").await;
    let ino = create(&h, "seq.bin").await;
    let p0 = make_striped(&h, ino, 2 * BS as usize + 1, 1).await;

    let blocks_before = wt_blocks();
    let bytes_before = wt_bytes();

    // Non-aligned overwrite [0, BS+1): block 0 completes at merge time
    // (write_end == b_end), block 1 becomes a partial RAM tail.
    let p1 = pattern(BS as usize + 1, 60);
    write_at(&h, ino, 0, &p1).await;

    assert!(
        h.fs.router
            .cache
            .nvme
            .read_staged(&staged_key(ino, 0))
            .is_none(),
        "content-complete block 0 must bypass staging (write-through)"
    );

    let meta = backend_meta(&h, ino).await;
    let bm = meta.block_map.clone().expect("striped map");
    let k0 = bm
        .get(&0)
        .expect("block 0 mapping must be merged synchronously by write-through");
    assert!(
        h.fs.router.cache.read_lru.get(k0).is_none(),
        "write-through must not put striped blocks into the read LRU"
    );

    assert_eq!(
        wt_blocks() - blocks_before,
        1,
        "exactly one block written through"
    );
    assert_eq!(
        wt_bytes() - bytes_before,
        BS,
        "write_through_bytes accounts the block"
    );

    // Content: p1 over [0, BS+1), p0's tail beyond.
    let mut expected = p1.clone();
    expected.extend_from_slice(&p0[BS as usize + 1..]);
    assert_eq!(read_at(&h, ino, 0, p0.len() as u32).await, expected);
}

/// RMW-seeded complete block: partial overwrite reaching the block end fires
/// the same trigger; the seeded prefix (old bytes) survives byte-exact.
#[tokio::test]
async fn test_rmw_seeded_complete_block_write_through_preserves_prefix() {
    let _g = serial().await;
    let h = make("wt_rmw").await;
    let ino = create(&h, "rmw.bin").await;
    let p0 = make_striped(&h, ino, 2 * BS as usize, 11).await;

    let before = wt_blocks();
    // [100, BS): Seeded entry (existing data), write_end == b_end → trigger.
    let p1 = pattern(BS as usize - 100, 77);
    write_at(&h, ino, 100, &p1).await;

    assert!(
        h.fs.router
            .cache
            .nvme
            .read_staged(&staged_key(ino, 0))
            .is_none(),
        "seeded complete block must write through, not stage"
    );
    assert_eq!(wt_blocks() - before, 1);

    let mut expected = p0.clone();
    expected[100..BS as usize].copy_from_slice(&p1);
    assert_eq!(read_at(&h, ino, 0, p0.len() as u32).await, expected);
}

/// One-shot: a single request covering a whole block (block_size <=
/// max_write shapes) uploads via one severing copy into the pooled buffer.
#[tokio::test]
async fn test_one_shot_full_block_write_through() {
    let _g = serial().await;
    let h = make("wt_oneshot").await;
    let ino = create(&h, "oneshot.bin").await;
    make_striped(&h, ino, BS as usize + 1, 21).await;

    let before = wt_blocks();
    // [BS, 2*BS+1): block 1 is covered whole by one request (Complete_OneShot),
    // block 2 is a 1-byte partial tail.
    let p = pattern(BS as usize + 1, 42);
    write_at(&h, ino, BS, &p).await;

    assert!(
        h.fs.router
            .cache
            .nvme
            .read_staged(&staged_key(ino, 1))
            .is_none(),
        "one-shot complete block must write through"
    );
    assert_eq!(wt_blocks() - before, 1);
    let meta = backend_meta(&h, ino).await;
    assert!(
        meta.block_map.as_ref().and_then(|m| m.get(&1)).is_some(),
        "block 1 mapping must be published"
    );
    assert_eq!(read_at(&h, ino, BS, p.len() as u32).await, p);
}

/// Partial tails never write through — they stay in RAM until fsync drives
/// them to staging/upload (staging's design role is preserved).
#[tokio::test]
async fn test_partial_tail_stays_in_ram_until_fsync() {
    let _g = serial().await;
    let h = make("wt_tail").await;
    let ino = create(&h, "tail.bin").await;
    make_striped(&h, ino, BS as usize + 1, 5).await;

    let before = wt_blocks();
    let p = pattern(512, 90);
    write_at(&h, ino, 2 * BS, &p).await; // partial fresh block 2, mid-file hole

    assert_eq!(wt_blocks(), before, "partial tail must not write through");
    assert!(
        h.fs.router
            .cache
            .nvme
            .read_staged(&staged_key(ino, 2))
            .is_none(),
        "partial tail must stay in RAM, not staging"
    );
    assert_eq!(
        read_at(&h, ino, 2 * BS, 512).await,
        p,
        "RYW from the RAM buffer"
    );

    // fsync moves the tail out of RAM (staging or direct upload — its
    // durable merge is the writeback worker's job in production; the
    // teardown flush stands in for the worker in this harness).
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    h.fs.force_flush_all_staged_data().await.unwrap();
    let meta = backend_meta(&h, ino).await;
    assert!(
        meta.block_map.as_ref().and_then(|m| m.get(&2)).is_some(),
        "flushed partial tail must be merged durably"
    );
    assert_eq!(read_at(&h, ino, 2 * BS, 512).await, p);
}

// ---------------------------------------------------------------------------
// Out-of-order fills: the completion point must match today's byte-for-byte
// ---------------------------------------------------------------------------

/// Tail-first fill: [BS/2, BS) of a fresh block ends at the block end — the
/// trigger fires immediately (as today), zeroing the uncovered head first.
#[tokio::test]
async fn test_tail_first_fill_zeroes_head_and_uploads_at_trigger() {
    let _g = serial().await;
    let h = make("wt_tailfirst").await;
    let ino = create(&h, "tailfirst.bin").await;
    make_striped(&h, ino, BS as usize + 1, 33).await;

    let before = wt_blocks();
    let p = pattern((BS / 2) as usize, 8);
    // Fresh block 3, write [3.5*BS, 4*BS): write_end == b_end → trigger.
    write_at(&h, ino, 3 * BS + BS / 2, &p).await;
    assert_eq!(
        wt_blocks() - before,
        1,
        "tail-first block-end write must upload immediately, exactly as today's staging point"
    );
    assert!(h
        .fs
        .router
        .cache
        .nvme
        .read_staged(&staged_key(ino, 3))
        .is_none());

    // The uncovered head is zeros — on the DEVICE (durable), not just in RAM.
    assert!(
        read_at(&h, ino, 3 * BS, (BS / 2) as u32)
            .await
            .iter()
            .all(|&b| b == 0),
        "uncovered head of a tail-first fill must read zeros"
    );
    assert_eq!(read_at(&h, ino, 3 * BS + BS / 2, (BS / 2) as u32).await, p);
}

/// Gap fill: [0,100) then [200,300) (gap zeroed, buffer stays in RAM — no
/// trigger), then [300, BS) completes the block and uploads. Hole bytes are
/// zeros at every stage.
#[tokio::test]
async fn test_gap_then_completion_matches_todays_bytes() {
    let _g = serial().await;
    let h = make("wt_gap").await;
    let ino = create(&h, "gap.bin").await;
    make_striped(&h, ino, BS as usize + 1, 44).await;

    let before = wt_blocks();
    write_at(&h, ino, 2 * BS, &[1u8; 100]).await;
    write_at(&h, ino, 2 * BS + 200, &[2u8; 100]).await; // gap write
    assert_eq!(wt_blocks(), before, "no trigger before the block end");

    // RYW: the gap [100,200) must read zeros from the RAM buffer.
    let ram = read_at(&h, ino, 2 * BS, 300).await;
    assert!(ram[0..100].iter().all(|&b| b == 1));
    assert!(
        ram[100..200].iter().all(|&b| b == 0),
        "gap bytes must read zeros from RAM (never recycled pool memory)"
    );
    assert!(ram[200..300].iter().all(|&b| b == 2));

    // Complete the block → trigger → upload.
    let tail = pattern(BS as usize - 300, 13);
    write_at(&h, ino, 2 * BS + 300, &tail).await;
    assert_eq!(wt_blocks() - before, 1);

    let durable = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert!(durable[0..100].iter().all(|&b| b == 1));
    assert!(
        durable[100..200].iter().all(|&b| b == 0),
        "gap zeros must be durable"
    );
    assert!(durable[200..300].iter().all(|&b| b == 2));
    assert_eq!(&durable[300..], &tail[..]);
}

/// Middle-last: a tail-first block publishes; the later [0, BS/2) write
/// re-seeds a Seeded entry from the published block via RMW (as today) and
/// completes with old tail + new head.
#[tokio::test]
async fn test_middle_last_after_published_tail_rmw_seeds() {
    let _g = serial().await;
    let h = make("wt_midlast").await;
    let ino = create(&h, "midlast.bin").await;
    make_striped(&h, ino, BS as usize + 1, 50).await;

    // Publish block 2 tail-first.
    let tail = pattern((BS / 2) as usize, 71);
    write_at(&h, ino, 2 * BS + BS / 2, &tail).await;

    // Middle-last head write: [2*BS, 2*BS + BS/2) — partial (no trigger),
    // seeded via RMW from the just-published block.
    let head = pattern((BS / 2) as usize, 99);
    write_at(&h, ino, 2 * BS, &head).await;

    let got = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(&got[..(BS / 2) as usize], &head[..], "new head");
    assert_eq!(
        &got[(BS / 2) as usize..],
        &tail[..],
        "published tail must survive the RMW re-seed"
    );

    // Completing the block again writes through the seeded entry.
    let before = wt_blocks();
    let p2 = pattern((BS / 2) as usize, 111);
    write_at(&h, ino, 2 * BS + BS / 2, &p2).await;
    assert_eq!(wt_blocks() - before, 1);
    let got = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(&got[..(BS / 2) as usize], &head[..]);
    assert_eq!(&got[(BS / 2) as usize..], &p2[..]);
}

// ---------------------------------------------------------------------------
// Sparse-hole zeros (the recycled-pool-memory leak, RAM + durable variants)
// ---------------------------------------------------------------------------

/// Sparse write into a fresh block, then read the hole: zeros, served by the
/// coverage-aware read hit (never recycled pool bytes, never a mutation of
/// the shared buffer).
#[tokio::test]
async fn test_sparse_hole_reads_zeros_from_ram_buffer() {
    let _g = serial().await;
    let h = make("wt_sparse_ram").await;
    let ino = create(&h, "sparse_ram.bin").await;
    make_striped(&h, ino, BS as usize + 1, 3).await;

    // Dirty the pool so recycled buffers carry garbage: several complete
    // blocks pass through pooled buffers and recycle on upload.
    for i in 0..8u64 {
        write_at(
            &h,
            ino,
            (4 + i) * BS + BS / 2,
            &vec![0xAB; (BS / 2) as usize],
        )
        .await;
    }

    // Fresh block 20: sparse mid-block write [1024, 2048) — no trigger,
    // buffer stays in RAM with covered == (1024, 2048).
    let p = pattern(1024, 66);
    write_at(&h, ino, 20 * BS + 1024, &p).await;

    // Hole below the covered range, inside file_size: MUST be zeros.
    let hole = read_at(&h, ino, 20 * BS, 1024).await;
    assert!(
        hole.iter().all(|&b| b == 0),
        "sparse hole served from an uncovered fresh buffer must be zeros \
         (recycled pool memory leaked through the kernel)"
    );
    // A read spanning hole + covered range: zeros then the pattern.
    let span = read_at(&h, ino, 20 * BS, 2048).await;
    assert!(span[..1024].iter().all(|&b| b == 0));
    assert_eq!(&span[1024..], &p[..]);
    // The covered range itself is served exactly.
    assert_eq!(read_at(&h, ino, 20 * BS + 1024, 1024).await, p);
}

/// The durable variant: sparse write → fsync (stage/upload exit must
/// zero-complete under the block lock) → read back through the block device:
/// zeros.
#[tokio::test]
async fn test_sparse_hole_zeros_survive_fsync_to_durable() {
    let _g = serial().await;
    let h = make("wt_sparse_dur").await;
    let ino = create(&h, "sparse_dur.bin").await;
    make_striped(&h, ino, BS as usize + 1, 4).await;

    for i in 0..8u64 {
        write_at(
            &h,
            ino,
            (4 + i) * BS + BS / 2,
            &vec![0xCD; (BS / 2) as usize],
        )
        .await;
    }

    let p = pattern(1024, 87);
    write_at(&h, ino, 30 * BS + 1024, &p).await;
    // fsync zero-completes + stages the partial buffer; the teardown flush
    // stands in for the writeback worker's durable merge in this harness.
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    h.fs.force_flush_all_staged_data().await.unwrap();

    // Purge every RAM tier so the read resolves via the durable block.
    let path = squeezefs::keys::inode_path(ino);
    let meta = backend_meta(&h, ino).await;
    let k = meta
        .block_map
        .as_ref()
        .and_then(|m| m.get(&30))
        .expect("fsync must have made block 30 durable")
        .clone();
    h.fs.router.cache.read_lru.remove(&k);
    h.fs.router.cache.read_lru.remove(&path);
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.nvme.remove_cached_read_block(&k);

    let hole = read_at(&h, ino, 30 * BS, 1024).await;
    assert!(
        hole.iter().all(|&b| b == 0),
        "durable sparse hole must be zeros (staging/upload exit must zero-complete)"
    );
    assert_eq!(read_at(&h, ino, 30 * BS + 1024, 1024).await, p);
}

// ---------------------------------------------------------------------------
// Spill discipline: try_lock, never a blocking acquire; victims zero-complete
// ---------------------------------------------------------------------------

/// The RAM-cap spill runs while the inserter already holds its own block
/// lock — a blocking acquire of a victim's lock can self-deadlock on a
/// shared stripe shard. Hold one victim's lock for the whole storm: every
/// write must still complete (try_lock skips the held victim), and the held
/// victim must never reach staging while locked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_spill_victim_try_lock_contention_no_deadlock() {
    let _g = serial().await;
    let h = make("wt_spill").await;
    let ino = create(&h, "spill.bin").await;
    make_striped(&h, ino, BS as usize + 1, 7).await;

    // Partial write to block 2 → RAM buffer (the pinned victim).
    write_at(&h, ino, 2 * BS, &[9u8; 64]).await;

    // Hold block 2's flush lock (simulating a concurrent flush holder).
    let pinned_lock = BLOCK_FLUSH_LOCKS.get_lock(ino, 2);
    let pinned_guard = pinned_lock.lock().await;

    // Storm: > MAX_ACTIVE_BLOCK_BUFFERS (256) partial buffers, skipping any
    // block whose stripe shard collides with the pinned lock (the storm
    // writes themselves take their own block locks and must not block on
    // ours).
    let mut written = Vec::new();
    let mut b = 4u64;
    while written.len() < 300 {
        if !std::ptr::eq(BLOCK_FLUSH_LOCKS.get_lock(ino, b as u32), pinned_lock) {
            written.push(b);
        }
        b += 1;
    }
    let storm = async {
        for &blk in &written {
            write_at(&h, ino, blk * BS, &[(blk % 250) as u8 + 1; 64]).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), storm)
        .await
        .expect("spill storm deadlocked while a victim's block lock was held");

    assert!(
        h.fs.router
            .cache
            .nvme
            .read_staged(&staged_key(ino, 2))
            .is_none(),
        "spill must skip (try_lock) a victim whose block lock is held"
    );
    drop(pinned_guard);

    // Everything is readable byte-exact: spilled victims (zero-completed to
    // staging) and RAM residents alike, holes read zeros.
    assert_eq!(read_at(&h, ino, 2 * BS, 64).await, vec![9u8; 64]);
    for &blk in written.iter().take(20) {
        let got = read_at(&h, ino, blk * BS, 128).await;
        assert_eq!(
            &got[..64],
            &[(blk % 250) as u8 + 1; 64][..],
            "block {blk} data"
        );
        assert!(
            got[64..].iter().all(|&x| x == 0),
            "block {blk} spill hole must be zeros (victim zero-completes under its lock)"
        );
    }
}

// ---------------------------------------------------------------------------
// Never-lossy fallback + fencing
// ---------------------------------------------------------------------------

/// A device-error write-through degrades into the staging path: the write
/// still acks, the bytes stay readable, the fallback is counted, and a
/// teardown flush makes them durable.
#[tokio::test]
async fn test_write_through_fallback_never_lossy_on_device_error() {
    let _g = serial().await;
    let h = make("wt_fallback").await;
    let ino = create(&h, "fallback.bin").await;
    let p0 = make_striped(&h, ino, 2 * BS as usize + 1, 14).await;

    let fallbacks_before = wt_fallbacks();
    squeezefs::nvme_dev::set_fail_next_writes(1);
    let p1 = pattern(BS as usize + 1, 120);
    write_at(&h, ino, 0, &p1).await; // block 0 completes; its DMA fails once
    squeezefs::nvme_dev::clear_fail_next_writes();

    assert!(
        wt_fallbacks() > fallbacks_before,
        "device-error write-through must fall back (and be counted)"
    );
    // Never lossy: the block landed in staging (or RAM) and reads exactly.
    let mut expected = p1.clone();
    expected.extend_from_slice(&p0[BS as usize + 1..]);
    assert_eq!(read_at(&h, ino, 0, p0.len() as u32).await, expected);

    // Teardown flush drains the fallback durably.
    h.fs.flush_all_memory_buffers_to_staging().await.unwrap();
    let summary = h.fs.flush_all_staged_blocks_to_backend().await;
    assert_eq!(
        summary.failed, 0,
        "fallback flush failures: {:?}",
        summary.error_samples
    );
    let meta = backend_meta(&h, ino).await;
    assert!(meta.block_map.as_ref().and_then(|m| m.get(&0)).is_some());
    assert_eq!(read_at(&h, ino, 0, p0.len() as u32).await, expected);
}

/// Fencing expiry mid-stream: a completing write under a stale lease fails
/// loudly (EIO), invalidates the local lease, and the next write re-acquires
/// and succeeds — no torn state.
#[tokio::test]
async fn test_fencing_expiry_fails_write_and_recovers() {
    let _g = serial().await;
    let h = make("wt_fencing").await;
    let ino = create(&h, "fencing.bin").await;
    make_striped(&h, ino, BS as usize + 1, 17).await;

    // Bump the fencing token past the write path's cached lease. A range
    // lock takes a distinct lock key (the whole-file lease stays held by the
    // write path) while sharing the file's fencing generator.
    let path = squeezefs::keys::inode_path(ino);
    let lease =
        h.fs.dlm()
            .acquire_lock(&path, Some((0, 1)), Duration::from_secs(5))
            .await
            .unwrap();
    drop(lease);

    let res =
        h.fs.write(
            h.req,
            ino,
            0,
            0,
            bytes::Bytes::from(pattern(BS as usize, 1)),
            0,
            0,
        )
        .await;
    assert!(
        res.is_err(),
        "a stale-token completing write must fail loudly"
    );

    // Recovery: the stale lease was invalidated; a retry re-acquires.
    let p = pattern(BS as usize + 1, 2);
    write_at(&h, ino, 0, &p).await;
    assert_eq!(read_at(&h, ino, 0, p.len() as u32).await, p);
}

/// The merge primitive re-validates fencing at commit.
#[tokio::test]
async fn test_merge_primitive_rejects_stale_fencing_token() {
    let _g = serial().await;
    let h = make("wt_merge_fencing").await;
    let ino = create(&h, "mf.bin").await;
    make_striped(&h, ino, BS as usize + 1, 19).await;

    let path = squeezefs::keys::inode_path(ino);
    let lease =
        h.fs.dlm()
            .acquire_lock(&path, Some((0, 1)), Duration::from_secs(5))
            .await
            .unwrap();
    let stale = lease.fencing_token() - 1;
    let res =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[]),
                0,
                LayoutFlip::KeepLayout,
                stale,
            )
            .await;
    assert!(
        matches!(res, Err(SqueezefsError::FencingTokenExpired { .. })),
        "stale token must be rejected by the primitive, got {res:?}"
    );
}

// ---------------------------------------------------------------------------
// The merge primitive: displaced keys, TruncateFrom, degenerate size-only
// ---------------------------------------------------------------------------

/// Merge returns exactly the keys displaced from the CURRENT map (never a
/// caller snapshot), already purged from the RAM read tier.
#[tokio::test]
async fn test_merge_primitive_returns_displaced_keys_and_purges_tiers() {
    let _g = serial().await;
    let h = make("wt_merge_displaced").await;
    let ino = create(&h, "md.bin").await;
    make_striped(&h, ino, BS as usize + 1, 23).await;
    let token = h.fs.dlm().get_fencing_token_ino(ino);

    let meta = backend_meta(&h, ino).await;
    let old_k0 = meta.block_map.as_ref().unwrap().get(&0).unwrap().clone();
    h.fs.router
        .cache
        .read_lru
        .put(&old_k0, bytes::Bytes::from_static(b"x"));

    let entries = vec![(0u32, "999888".to_string())];
    let displaced =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&entries),
                0,
                LayoutFlip::ToStripedKeepStagedIdentity,
                token,
            )
            .await
            .unwrap();
    assert_eq!(
        displaced,
        vec![old_k0.clone()],
        "displaced-from-current key"
    );
    assert!(
        h.fs.router.cache.read_lru.get(&old_k0).is_none(),
        "displaced key must be purged from the read tier"
    );

    // Re-merging the same key displaces nothing.
    let displaced2 =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&entries),
                0,
                LayoutFlip::ToStripedKeepStagedIdentity,
                token,
            )
            .await
            .unwrap();
    assert!(
        displaced2.is_empty(),
        "same-key merge must displace nothing"
    );

    let meta = backend_meta(&h, ino).await;
    assert_eq!(meta.block_map.as_ref().unwrap().get(&0).unwrap(), "999888");
}

/// TruncateFrom removes exactly the blocks at/after the cut, returns their
/// keys for the caller's post-publish free, and sets the size exactly.
#[tokio::test]
async fn test_merge_primitive_truncate_from_removes_and_returns_keys() {
    let _g = serial().await;
    let h = make("wt_merge_trunc").await;
    let ino = create(&h, "mt.bin").await;
    make_striped(&h, ino, 4 * BS as usize, 29).await;
    let token = h.fs.dlm().get_fencing_token_ino(ino);

    let meta = backend_meta(&h, ino).await;
    let bm = meta.block_map.clone().unwrap();
    assert!(bm.len() >= 4, "premise: 4 blocks mapped, got {bm:?}");
    let mut expect_removed: Vec<String> = [2u32, 3u32]
        .iter()
        .map(|b| bm.get(b).unwrap().clone())
        .collect();
    expect_removed.sort();

    let mut removed =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::TruncateFrom { new_size: 2 * BS },
                2 * BS,
                LayoutFlip::KeepLayout,
                token,
            )
            .await
            .unwrap();
    removed.sort();
    assert_eq!(
        removed, expect_removed,
        "removed keys must come back as the free list"
    );

    let meta = backend_meta(&h, ino).await;
    assert_eq!(meta.size, 2 * BS, "TruncateFrom sets the size exactly");
    let bm = meta.block_map.clone().unwrap();
    assert!(!bm.contains_key(&2) && !bm.contains_key(&3));
    assert!(bm.contains_key(&0) && bm.contains_key(&1));
}

/// The degenerate size-only save (`Merge(&[])`) re-reads the CURRENT map
/// under the lock: it can never rewrite the block map, and size only floors
/// upward.
#[tokio::test]
async fn test_merge_primitive_degenerate_size_only_floors_never_shrinks() {
    let _g = serial().await;
    let h = make("wt_merge_degen").await;
    let ino = create(&h, "dg.bin").await;
    make_striped(&h, ino, 2 * BS as usize, 31).await;
    let token = h.fs.dlm().get_fencing_token_ino(ino);

    let before = backend_meta(&h, ino).await;
    let bm_before = before.block_map.clone().unwrap();

    // Grow.
    let displaced =
        h.fs.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[]),
                10 * BS,
                LayoutFlip::KeepLayout,
                token,
            )
            .await
            .unwrap();
    assert!(displaced.is_empty());
    let meta = backend_meta(&h, ino).await;
    assert_eq!(meta.size, 10 * BS);
    assert_eq!(
        meta.block_map.clone().unwrap(),
        bm_before,
        "map must be untouched"
    );

    // A lower min_size never shrinks.
    h.fs.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[]),
            BS,
            LayoutFlip::KeepLayout,
            token,
        )
        .await
        .unwrap();
    let meta = backend_meta(&h, ino).await;
    assert_eq!(
        meta.size,
        10 * BS,
        "degenerate merge must never shrink the size"
    );
}

// ---------------------------------------------------------------------------
// Cross-discipline interleavings (i)-(v): one merge discipline, no lost updates
// ---------------------------------------------------------------------------

/// (i) Write-through on block B racing a FAIL_NEXT_WRITES-forced fallback's
/// writeback flush of block A on the same file: both mappings must survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_interleave_write_through_vs_fallback_writeback() {
    let _g = serial().await;
    let h = make("wt_il_fallback").await;

    for iter in 0..6u64 {
        let ino = create(&h, &format!("il_fb_{iter}.bin")).await;
        make_striped(&h, ino, BS as usize + 1, iter as u8).await;

        // Force block 2's write-through to fail → staging fallback.
        squeezefs::nvme_dev::set_fail_next_writes(1);
        let pa = pattern(BS as usize, (iter + 1) as u8);
        write_at(&h, ino, 2 * BS + 1, &pa[1..]).await; // completes block 2 via [1..BS)
        squeezefs::nvme_dev::clear_fail_next_writes();

        // Race: write-through of block 4 vs the fallback's flush of block 2.
        let pb = pattern(BS as usize, (iter + 2) as u8);
        let write_b = async {
            write_at(&h, ino, 4 * BS + 1, &pb[1..]).await;
        };
        let flush_a = h.fs.flush_all_staged_blocks_to_backend();
        let (_, summary) = tokio::join!(write_b, flush_a);
        assert_eq!(
            summary.failed, 0,
            "iter {iter}: {:?}",
            summary.error_samples
        );

        let meta = backend_meta(&h, ino).await;
        let bm = meta.block_map.clone().unwrap_or_default();
        assert!(
            bm.contains_key(&2),
            "iter {iter}: fallback block 2 mapping lost (torn merge): {bm:?}"
        );
        assert!(
            bm.contains_key(&4),
            "iter {iter}: write-through block 4 mapping lost (torn merge): {bm:?}"
        );
        assert_eq!(
            &read_at(&h, ino, 2 * BS + 1, (BS - 1) as u32).await[..],
            &pa[1..]
        );
        assert_eq!(
            &read_at(&h, ino, 4 * BS + 1, (BS - 1) as u32).await[..],
            &pb[1..]
        );
    }
}

/// (ii) Write-through racing the staging-refusal durable escalation
/// (`upload_active_block_bytes` via the fsync path) on the same file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_interleave_write_through_vs_staging_refusal_escalation() {
    let _g = serial().await;
    // Tiny write-staging budget so `put_active_block` refuses.
    let h = make_with("wt_il_refusal", "1MB", "4096").await;

    // Fill the staging segment with junk entries that stay live.
    let mut filled = 0u64;
    loop {
        let key = format!("active_block:inode_999999:block_{filled}");
        if !h
            .fs
            .router
            .cache
            .nvme
            .put_active_block(&key, &[0x55u8; 4096], 1)
        {
            break;
        }
        filled += 1;
        assert!(filled < 100_000, "staging never refused — premise broken");
    }
    assert!(filled > 0, "premise: staging admitted at least one block");

    for iter in 0..6u64 {
        let ino = create(&h, &format!("il_rf_{iter}.bin")).await;
        make_striped(&h, ino, BS as usize + 1, iter as u8).await;

        // Partial block 2 in RAM; fsync-path staging will be REFUSED and must
        // escalate to a durable upload.
        let pa = pattern(1000, (iter + 3) as u8);
        write_at(&h, ino, 2 * BS, &pa).await;

        let token = h.fs.dlm().get_fencing_token_ino(ino);
        let pb = pattern(BS as usize, (iter + 4) as u8);
        let write_b = async {
            write_at(&h, ino, 4 * BS + 1, &pb[1..]).await;
        };
        let escalate_a = h.fs.flush_memory_buffers_for_inode(ino, token);
        let (_, esc) = tokio::join!(write_b, escalate_a);
        esc.unwrap_or_else(|e| panic!("iter {iter}: escalation failed: {e:?}"));

        let meta = backend_meta(&h, ino).await;
        let bm = meta.block_map.clone().unwrap_or_default();
        assert!(
            bm.contains_key(&2),
            "iter {iter}: escalated block 2 mapping lost: {bm:?}"
        );
        assert!(
            bm.contains_key(&4),
            "iter {iter}: write-through block 4 mapping lost: {bm:?}"
        );
        assert_eq!(read_at(&h, ino, 2 * BS, 1000).await, pa);
        assert_eq!(
            &read_at(&h, ino, 4 * BS + 1, (BS - 1) as u32).await[..],
            &pb[1..]
        );
    }
}

/// (iii) Write-through racing a concurrent truncate SHRINK on the same file:
/// the surviving map/size must be consistent under either winner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_interleave_write_through_vs_truncate_shrink() {
    let _g = serial().await;
    let h = make("wt_il_shrink").await;

    for iter in 0..10u64 {
        let ino = create(&h, &format!("il_sh_{iter}.bin")).await;
        make_striped(&h, ino, 2 * BS as usize, iter as u8).await;

        let pb = pattern(BS as usize, (iter + 5) as u8);
        let write_b = async {
            write_at(&h, ino, 3 * BS + 1, &pb[1..]).await; // completes block 3
        };
        let shrink = async {
            h.fs.setattr(
                h.req,
                ino,
                None,
                SetAttr {
                    size: Some(2 * BS),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        };
        tokio::join!(write_b, shrink);

        let meta = backend_meta(&h, ino).await;
        let bm = meta.block_map.clone().unwrap_or_default();
        // Both orders are POSIX-legal; torn merges are not:
        if meta.size <= 2 * BS {
            assert!(
                !bm.contains_key(&3),
                "iter {iter}: truncate won (size {}), block 3 mapping resurrected: {bm:?}",
                meta.size
            );
        } else {
            assert!(
                meta.size >= 4 * BS,
                "iter {iter}: write won but size reverted to {}",
                meta.size
            );
            assert!(
                bm.contains_key(&3),
                "iter {iter}: write won (size {}) but block 3 mapping lost: {bm:?}",
                meta.size
            );
            assert_eq!(
                &read_at(&h, ino, 3 * BS + 1, (BS - 1) as u32).await[..],
                &pb[1..]
            );
        }
        // Blocks 0/1 are below every cut and must survive in all orders.
        assert!(
            bm.contains_key(&0) && bm.contains_key(&1),
            "iter {iter}: pre-existing mappings lost: {bm:?}"
        );
    }
}

/// (iv) Write-through racing a concurrent truncate GROW: the grow is a
/// stale-snapshot whole-meta save today — unified merging must keep both the
/// grown size (monotone) and the racing block mapping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_interleave_write_through_vs_truncate_grow() {
    let _g = serial().await;
    let h = make("wt_il_grow").await;

    for iter in 0..10u64 {
        let ino = create(&h, &format!("il_gr_{iter}.bin")).await;
        make_striped(&h, ino, 2 * BS as usize, iter as u8).await;

        let pb = pattern(BS as usize, (iter + 6) as u8);
        let write_b = async {
            write_at(&h, ino, 3 * BS + 1, &pb[1..]).await; // completes block 3
        };
        let grow = async {
            h.fs.setattr(
                h.req,
                ino,
                None,
                SetAttr {
                    size: Some(10 * BS),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        };
        tokio::join!(write_b, grow);

        let meta = backend_meta(&h, ino).await;
        let bm = meta.block_map.clone().unwrap_or_default();
        assert!(
            meta.size >= 10 * BS,
            "iter {iter}: grown size lost (size monotone violated): {}",
            meta.size
        );
        assert!(
            bm.contains_key(&3),
            "iter {iter}: write-through mapping dropped by the grow save: {bm:?}"
        );
        assert_eq!(
            &read_at(&h, ino, 3 * BS + 1, (BS - 1) as u32).await[..],
            &pb[1..]
        );
    }
}

/// (v) Write-through racing a concurrent fallocate-extend (the same
/// stale-snapshot save shape, today under no lock at all).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_interleave_write_through_vs_fallocate_extend() {
    let _g = serial().await;
    let h = make("wt_il_falloc").await;

    for iter in 0..10u64 {
        let ino = create(&h, &format!("il_fa_{iter}.bin")).await;
        make_striped(&h, ino, 2 * BS as usize, iter as u8).await;

        let pb = pattern(BS as usize, (iter + 7) as u8);
        let write_b = async {
            write_at(&h, ino, 3 * BS + 1, &pb[1..]).await; // completes block 3
        };
        let extend = async {
            h.fs.fallocate(h.req, ino, 0, 0, 12 * BS, 0).await.unwrap();
        };
        tokio::join!(write_b, extend);

        let meta = backend_meta(&h, ino).await;
        let bm = meta.block_map.clone().unwrap_or_default();
        assert!(
            meta.size >= 12 * BS,
            "iter {iter}: fallocate size lost (monotone violated): {}",
            meta.size
        );
        assert!(
            bm.contains_key(&3),
            "iter {iter}: write-through mapping dropped by the fallocate save: {bm:?}"
        );
        assert_eq!(
            &read_at(&h, ino, 3 * BS + 1, (BS - 1) as u32).await[..],
            &pb[1..]
        );
    }
}

// ---------------------------------------------------------------------------
// Staged-identity + defrag conversion + RYW
// ---------------------------------------------------------------------------

/// Staged-identity regression (pins the LayoutFlip policy): promotion of a
/// staged file releases its ring entry exactly once, and a later fsync-path
/// flush (KeepStagedIdentity) neither strands nor double-releases it — the
/// staging budget gauge settles at 0 and the layout keeps a clean identity.
///
/// Uses a 64 KiB block size: the staged window is (MAX_INLINE_SIZE,
/// block_size], which is EMPTY at this suite's default 4 KiB shape.
#[tokio::test]
async fn test_staged_identity_promotion_fsync_ring_entry_exact() {
    let _g = serial().await;
    let h = make_with("wt_staged_id", "64MB", "65536").await;
    let ino = create(&h, "staged_id.bin").await;

    // Staged layout first (> inline, <= 64 KiB block, staging dirs present).
    // Read the RAM entry directly: a staged layout is RAM-only until fsync
    // (layout_dirty) — evicting it here would orphan the staged identity.
    let p0 = pattern(8000, 41);
    write_at(&h, ino, 0, &p0).await;
    let path = squeezefs::keys::inode_path(ino);
    let ram =
        h.fs.router
            .metadata_cache
            .get(&path)
            .expect("staged RAM meta present");
    assert_eq!(ram.file_type, "staged", "premise: staged layout");
    let file_id = ram.file_id.clone().expect("staged file_id");
    assert!(
        h.fs.router.cache.nvme.read_staged(&file_id).is_some(),
        "premise: ring entry resident"
    );

    // Grow past block_size → striped transition releases the ring entry.
    let p1 = pattern(60000, 43);
    write_at(&h, ino, 8000, &p1).await;
    assert!(
        h.fs.router.cache.nvme.read_staged(&file_id).is_none(),
        "transition must release the superseded ring entry"
    );

    // fsync-path flush of post-transition active blocks must keep the
    // staged-identity fields untouched (no resurrected file_id) and the
    // budget exact.
    write_at(&h, ino, 68000, &pattern(500, 45)).await; // partial active block
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    h.fs.force_flush_all_staged_data().await.unwrap();

    let meta = backend_meta(&h, ino).await;
    assert_eq!(meta.file_type, "striped");
    assert!(
        meta.file_id.is_none(),
        "fsync flush must not resurrect a staged identity (file_id = {:?})",
        meta.file_id
    );
    assert_eq!(
        h.fs.router
            .cache
            .nvme
            .current_staged_write_bytes
            .load(Ordering::SeqCst),
        0,
        "staging budget must settle at 0 (no strand, no double release)"
    );

    let mut expected = p0.clone();
    expected.extend_from_slice(&p1);
    expected.extend_from_slice(&pattern(500, 45));
    assert_eq!(read_at(&h, ino, 0, expected.len() as u32).await, expected);
}

/// Defrag BlockMove merges through the primitive: serialized, fencing-
/// revalidated, and RAM-coherent (the raw-xattr bypass skipped all three —
/// the stale metadata_cache assertion is RED against it).
#[tokio::test]
async fn test_defrag_block_move_merges_through_primitive() {
    let _g = serial().await;
    let h = make("wt_defrag").await;
    let ino = create(&h, "defrag.bin").await;
    let p0 = make_striped(&h, ino, 2 * BS as usize, 47).await;

    let meta = backend_meta(&h, ino).await;
    let bm = meta.block_map.clone().unwrap();
    let src_key = bm.get(&0).unwrap().clone();
    let src_offset: u64 = src_key.parse().expect("plain offset key");

    // Destination: a fresh allocation the move copies into.
    let (_be, allocator, _writer) = h.fs.router.backend_router.get_active_backend().unwrap();
    let dest_offset = allocator.allocate_block().await.unwrap();
    allocator.publish_block(dest_offset);

    squeezefs::jobs::start_job_worker(Arc::new(h.fs.router.clone()), "t".into(), 100);
    squeezefs::jobs::submit_and_wait_for_job(
        "local",
        "t",
        vec![squeezefs::jobs::TaskType::BlockMove {
            ino,
            map_id: ino.to_string(),
            idx_str: "0".to_string(),
            src_offset,
            dest_offset,
            len: BS as usize,
        }],
    )
    .await
    .unwrap();

    // `submit_and_wait_for_job` acks at dequeue; poll (bounded) for the
    // worker's merge to land.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let meta = backend_meta(&h, ino).await;
        if meta.block_map.as_ref().and_then(|m| m.get(&0)) == Some(&dest_offset.to_string()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "BlockMove never merged the destination mapping: {:?}",
            meta.block_map
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // RAM cache is coherent with the merge (the raw-xattr path left it stale).
    let path = squeezefs::keys::inode_path(ino);
    let ram =
        h.fs.router
            .metadata_cache
            .get(&path)
            .expect("RAM meta present");
    assert_eq!(
        ram.block_map.as_ref().and_then(|m| m.get(&0)),
        Some(&dest_offset.to_string()),
        "BlockMove must publish RAM + backend coherently (one merge discipline)"
    );
    // Content survives the move.
    h.fs.router.cache.read_lru.remove(&src_key);
    h.fs.router.cache.read_lru.remove(&dest_offset.to_string());
    assert_eq!(read_at(&h, ino, 0, BS as u32).await, &p0[..BS as usize]);
}

/// Design OQ 6 resolved — defrag `BlockMove` source-slot free discipline:
/// the source mapping the merge displaces is FREED by the worker, but only
/// AFTER `merge_block_mappings` has published the new map (durable +
/// RAM-coherent, tiers purged), through `BackendRouter::free_block`'s
/// `begin_free` → punch-on-terminal → `finish_free` split — so the offset
/// returns to the allocator's free list (the leak this pins RED: today the
/// worker frees nothing) and the destructive punch strictly happens-before
/// any new owner's DMA at the reused offset.
#[tokio::test]
async fn test_defrag_block_move_frees_displaced_source_after_merge() {
    let _g = serial().await;
    let h = make("wt_defrag_free").await;
    let ino = create(&h, "defrag_free.bin").await;
    let p0 = make_striped(&h, ino, 2 * BS as usize, 53).await;

    let meta = backend_meta(&h, ino).await;
    let bm = meta.block_map.clone().unwrap();
    let src_key = bm.get(&0).unwrap().clone();
    let src_offset: u64 = src_key.parse().expect("plain offset key");

    let (_be, allocator, _writer) = h.fs.router.backend_router.get_active_backend().unwrap();
    let src_idx = src_offset / allocator.chunk_size();
    assert!(
        !allocator
            .get_free_blocks()
            .await
            .unwrap()
            .contains(&src_idx),
        "test precondition: source offset is live (not free-listed)"
    );
    let dest_offset = allocator.allocate_block().await.unwrap();
    allocator.publish_block(dest_offset);

    squeezefs::jobs::start_job_worker(Arc::new(h.fs.router.clone()), "t".into(), 100);
    squeezefs::jobs::submit_and_wait_for_job(
        "local",
        "t",
        vec![squeezefs::jobs::TaskType::BlockMove {
            ino,
            map_id: ino.to_string(),
            idx_str: "0".to_string(),
            src_offset,
            dest_offset,
            len: BS as usize,
        }],
    )
    .await
    .unwrap();

    // `submit_and_wait_for_job` acks at dequeue; poll (bounded) for the
    // worker's merge AND the displaced source's return to the free list.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let meta = backend_meta(&h, ino).await;
        let merged =
            meta.block_map.as_ref().and_then(|m| m.get(&0)) == Some(&dest_offset.to_string());
        let freed = allocator
            .get_free_blocks()
            .await
            .unwrap()
            .contains(&src_idx);
        if merged && freed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "BlockMove never freed the displaced source slot (merged: {merged}, \
             source free-listed: {freed}) — the source must follow the \
             displaced-key free discipline after the merge publishes"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Content still served from the destination after the source is gone.
    h.fs.router.cache.read_lru.remove(&src_key);
    h.fs.router.cache.read_lru.remove(&dest_offset.to_string());
    assert_eq!(read_at(&h, ino, 0, BS as u32).await, &p0[..BS as usize]);
}

/// Design OQ 6, clone-sharing half: a BlockMove whose displaced source is
/// still referenced by another holder (clone-shared refcount) must release
/// its reference WITHOUT punching the device bytes or free-listing the
/// offset — `begin_free`'s non-terminal contract (f0ca977). The surviving
/// referent's bytes stay intact on the device.
#[tokio::test]
async fn test_defrag_block_move_clone_shared_source_not_freed() {
    let _g = serial().await;
    let h = make("wt_defrag_clone").await;
    let ino = create(&h, "defrag_clone.bin").await;
    let p0 = make_striped(&h, ino, 2 * BS as usize, 59).await;

    let meta = backend_meta(&h, ino).await;
    let bm = meta.block_map.clone().unwrap();
    let src0_key = bm.get(&0).unwrap().clone();
    let src0_offset: u64 = src0_key.parse().expect("plain offset key");
    let src1_key = bm.get(&1).unwrap().clone();
    let src1_offset: u64 = src1_key.parse().expect("plain offset key");

    let (_be, allocator, writer) = h.fs.router.backend_router.get_active_backend().unwrap();
    let src0_idx = src0_offset / allocator.chunk_size();
    let src1_idx = src1_offset / allocator.chunk_size();

    // Ground truth for the punch check: the raw device bytes at the shared
    // source, captured before the move (cache-independent).
    let src0_device_before = writer.read_block(src0_offset, BS as usize).await.unwrap();

    // Simulate clone sharing: a second live reference on block 0's source.
    assert!(
        h.fs.router.backend_router.increment_refcount(&src0_key),
        "test precondition: clone reference taken on the source block"
    );

    let dest0 = allocator.allocate_block().await.unwrap();
    allocator.publish_block(dest0);
    let dest1 = allocator.allocate_block().await.unwrap();
    allocator.publish_block(dest1);

    // Two serialized moves: when the SECOND task's (sole-referent) source
    // hits the free list, the first task's free discipline has provably
    // completed — the worker is serial.
    squeezefs::jobs::start_job_worker(Arc::new(h.fs.router.clone()), "t".into(), 100);
    squeezefs::jobs::submit_and_wait_for_job(
        "local",
        "t",
        vec![
            squeezefs::jobs::TaskType::BlockMove {
                ino,
                map_id: ino.to_string(),
                idx_str: "0".to_string(),
                src_offset: src0_offset,
                dest_offset: dest0,
                len: BS as usize,
            },
            squeezefs::jobs::TaskType::BlockMove {
                ino,
                map_id: ino.to_string(),
                idx_str: "1".to_string(),
                src_offset: src1_offset,
                dest_offset: dest1,
                len: BS as usize,
            },
        ],
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if allocator
            .get_free_blocks()
            .await
            .unwrap()
            .contains(&src1_idx)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "second BlockMove never freed its sole-referent source — the \
             displaced-source free discipline did not run"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The clone-shared source was released but NOT reclaimed: not on the
    // free list (its offset must never be handed to a new owner while the
    // clone lives) …
    assert!(
        !allocator
            .get_free_blocks()
            .await
            .unwrap()
            .contains(&src0_idx),
        "clone-shared source was free-listed on a NON-terminal release — \
         a new owner's allocation would alias the surviving clone's block"
    );
    // … and NOT punched: the surviving referent's device bytes are intact.
    let src0_device_after = writer.read_block(src0_offset, BS as usize).await.unwrap();
    assert_eq!(
        &src0_device_after[..],
        &src0_device_before[..],
        "clone-shared source bytes changed on the device — a non-terminal \
         free must never punch a still-referenced block"
    );

    // Both mappings live on the destinations; content byte-exact.
    let meta = backend_meta(&h, ino).await;
    let bm = meta.block_map.clone().unwrap();
    assert_eq!(bm.get(&0), Some(&dest0.to_string()), "block 0 moved");
    assert_eq!(bm.get(&1), Some(&dest1.to_string()), "block 1 moved");
    for k in [&src0_key, &src1_key, &dest0.to_string(), &dest1.to_string()] {
        h.fs.router.cache.read_lru.remove(k);
    }
    assert_eq!(read_at(&h, ino, 0, 2 * BS as u32).await, p0);
}

/// Read-your-own-writes at ack: after a completing (write-through) request
/// returns, an immediate read observes the new bytes across the whole block.
#[tokio::test]
async fn test_ryw_after_write_through_ack() {
    let _g = serial().await;
    let h = make("wt_ryw").await;
    let ino = create(&h, "ryw.bin").await;
    make_striped(&h, ino, BS as usize + 1, 51).await;

    for round in 0..4u64 {
        let b = 2 + round;
        let p = pattern(BS as usize, (round + 60) as u8);
        write_at(&h, ino, b * BS + 1, &p[1..]).await; // completes block b (seeded head byte 0)
        let got = read_at(&h, ino, b * BS, BS as u32).await;
        assert_eq!(got[0], 0, "round {round}: unwritten head byte");
        assert_eq!(
            &got[1..],
            &p[1..],
            "round {round}: RYW after write-through ack"
        );
    }
}
