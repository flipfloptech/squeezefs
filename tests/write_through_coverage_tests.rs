//! RW3b — coverage-based write-through trigger + covered flush-seed elision
//! (PR RW3 fix of docs/design-random-small-writes.md §5.3 / G-RW4; convicted
//! by .benchmarks/2026-07-17-rw3-find-l1a-forensics.md).
//!
//! The convicted mechanism: the kernel legally SPLITS a FUSE WRITE (unaligned
//! O_DIRECT user buffers span max_pages+1 pages) and
//! `FOPEN_PARALLEL_DIRECT_WRITES` legally dispatches the segments
//! concurrently — per-block segments arrive OUT OF ORDER. The old
//! write-through trigger (`write_end == b_end_offset`, fuse_client.rs:4087)
//! keyed on THIS write's end as a proxy for block completeness:
//!
//! - the end-aligned segment arriving EARLY fired the trigger with partial
//!   coverage → an inline 4 MiB seed fetch (`write_path_seed_read_bytes`)
//!   inside a sequential write;
//! - the segment that actually COMPLETED coverage didn't end at b_end → the
//!   trigger never fired → fully-covered buffers parked and drained through
//!   fsync-flush / cap-spill = a 12 MiB/op RMW pipeline (seed read + staging
//!   put + writeback upload) on a pure sequential stream — the FIND-L1-A
//!   convoy fuel (t16 def/mb12 = 0.664× measured).
//!
//! Contract pinned here (the RW3b fix):
//!
//! - **Coverage-based completeness**: the write-through trigger fires exactly
//!   when the ActiveBlockBuf's ACCUMULATED written coverage reaches the whole
//!   block (union of written ranges, overlap-safe, order-blind) — never
//!   because one segment's end coincides with the block end. One trigger per
//!   block per covering stream, any delivery order.
//! - **Item-B deferred-seed law preserved BOTH ways**: partial coverage keeps
//!   deferring (no seed fetch at write time, no write-through), and a
//!   genuinely-partial block MUST still fetch its old-block seed at flush.
//! - **Covered flush-seed elision**: a stream that fully covers its blocks
//!   leaves nothing behind — zero seed reads at write time AND at
//!   fsync-flush, zero staging-put/writeback pipeline for covered blocks.
//! - **Crash shape**: out-of-order parked segments are RAM custody only; a
//!   crash inside the accumulation window leaves the old durable block fully
//!   intact (extends item-B's `crash_inside_window_leaves_old_block_intact`
//!   to the out-of-order case).
//!
//! RED against dev b1d7a28: the out-of-order pins fail on
//! `write_path_seed_read_bytes` / `get_obj` / `flush_seed_read_bytes` /
//! `staging_put_bytes_flush` deltas (the early-fire + parked-straggler
//! pipeline), and the partial pin fails on `write_through_blocks` (the old
//! trigger fires on a partial end-aligned segment).

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
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 65536; // striped at small sizes; keeps ledger deltas exact
const HALF: u64 = BS / 2;

/// The seed/write-through stats are process-global; tests asserting counter
/// deltas serialize (same pattern as `write_through_tests`).
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

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // This suite pins the ACCUMULATION trigger machinery. At this downscaled
    // BS the sub-block segments would be W1 patch-eligible (they are ~1 MiB
    // kernel-split segments in production — always patch-oversize), which
    // would bypass the machinery under test — pin the patch path OFF, as
    // striped_overwrite_lazy_seed_tests does for the same reason.
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), alloc_ns)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
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

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

fn assert_bytes(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: length");
    if let Some(i) = (0..got.len()).find(|&i| got[i] != want[i]) {
        panic!(
            "{what}: first mismatch at {i}: got {:#04x} want {:#04x}",
            got[i], want[i]
        );
    }
}

/// A durable striped file: 6 blocks written, fsynced (map published), read
/// tiers dropped so the `get_obj` ledger is honest (any RMW seed MUST be a
/// device read).
async fn durable_striped(h: &H, name: &str, tag: u8) -> (u64, Vec<u8>) {
    const LEN: usize = 6 * BS as usize;
    let ino = create(h, name).await;
    let base = pattern(LEN, tag);
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
    purge_tiers(h, ino).await;
    (ino, base)
}

async fn purge_tiers(h: &H, ino: u64) {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.cache.write_lru.remove(&path);
    h.fs.router.cache.read_lru.remove(&path);
    if let Ok(m) = h.fs.router.fetch_metadata(&path).await {
        if let Some(bm) = m.block_map.as_ref() {
            for bk in bm.values() {
                h.fs.router.cache.purge_block_key(bk);
            }
        }
    }
}

/// Counter snapshot for the ledger assertions.
#[derive(Clone, Copy, Debug)]
struct Ledger {
    get_obj: u64,
    write_path_seed: u64,
    flush_seed: u64,
    spill_seed: u64,
    wt_blocks: u64,
    staging_put_flush: u64,
    wb_enq_flush: u64,
    seed_skipped: u64,
}

fn ledger() -> Ledger {
    Ledger {
        get_obj: METRICS.get_obj.load(Ordering::Relaxed),
        write_path_seed: METRICS.write_path_seed_read_bytes.load(Ordering::Relaxed),
        flush_seed: METRICS.flush_seed_read_bytes.load(Ordering::Relaxed),
        spill_seed: METRICS.spill_seed_read_bytes.load(Ordering::Relaxed),
        wt_blocks: METRICS.write_through_blocks.load(Ordering::Relaxed),
        staging_put_flush: METRICS.staging_put_bytes_flush.load(Ordering::Relaxed),
        wb_enq_flush: METRICS.writeback_enqueued_flush.load(Ordering::Relaxed),
        seed_skipped: METRICS.overwrite_seed_skipped.load(Ordering::Relaxed),
    }
}

/// The zero-RMW assertion set for a covering out-of-order overwrite of
/// `blocks` blocks: no device reads, no seed fetches anywhere (write path,
/// flush, spill), exactly one write-through per block, no flush staging
/// pipeline.
fn assert_covering_stream_ledger(before: Ledger, after: Ledger, blocks: u64, what: &str) {
    assert_eq!(
        after.get_obj - before.get_obj,
        0,
        "{what}: a fully-covering stream must not read old blocks from the \
         device (FIND-L1-A: get_obj ≈ the whole dataset read back during a \
         pure sequential write)"
    );
    assert_eq!(
        after.write_path_seed - before.write_path_seed,
        0,
        "{what}: no inline write-path seed fetch (the old trigger's \
         partial-coverage misfire / gap-materialize)"
    );
    assert_eq!(
        after.flush_seed - before.flush_seed,
        0,
        "{what}: no flush-exit seed fetch (covered flush-seed elision — \
         nothing partial may be left parked by a covering stream)"
    );
    assert_eq!(
        after.spill_seed - before.spill_seed,
        0,
        "{what}: no spill-victim seed fetch"
    );
    assert_eq!(
        after.wt_blocks - before.wt_blocks,
        blocks,
        "{what}: write-through must fire EXACTLY once per block, at coverage \
         completion"
    );
    assert_eq!(
        after.staging_put_flush - before.staging_put_flush,
        0,
        "{what}: no fsync-flush staging puts (the parked-straggler 12 MiB/op \
         RMW pipeline must be dead)"
    );
    assert_eq!(
        after.wb_enq_flush - before.wb_enq_flush,
        0,
        "{what}: no fsync-flush writeback enqueues"
    );
}

/// 1. THE headline pin — kernel-split segments with the END-ALIGNED segment
/// arriving FIRST (the reorder `FOPEN_PARALLEL_DIRECT_WRITES` legally
/// produces): every block fully covered out of order must write through
/// exactly once with ZERO seed reads at write time and at flush.
///
/// RED on dev: the end-aligned segment fires the old trigger with partial
/// coverage (inline seed fetch), the tail-filling segment parks and drains
/// through the fsync-flush seed+staging+writeback pipeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ooo_end_first_segments_write_through_without_seed_reads() {
    let _g = serial().await;
    let h = make(*b"rw3b-ooo-endfrst", "rw3b_ns_a").await;
    let (ino, _) = durable_striped(&h, "endfirst", 0x00).await;
    const LEN: usize = 6 * BS as usize;

    let over = pattern(LEN, 0x5A);
    let before = ledger();
    for b in 0..6u64 {
        let s = b * BS;
        // End-aligned segment FIRST, tail-filling segment second.
        write_at(
            &h,
            ino,
            s + HALF,
            &over[(s + HALF) as usize..(s + BS) as usize],
        )
        .await;
        write_at(&h, ino, s, &over[s as usize..(s + HALF) as usize]).await;
    }
    fsync(&h, ino).await;
    let after = ledger();
    assert_covering_stream_ledger(before, after, 6, "end-first segments");
    assert_eq!(
        after.seed_skipped - before.seed_skipped,
        6,
        "every overwritten block's deferred seed must be SKIPPED (fully \
         covered before any exit)"
    );

    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, LEN).await;
    assert_bytes(&got, &over, "end-first out-of-order overwrite");
}

/// 2. The in-order control (both-orders clause): tail-filling segment first,
/// end-aligned second — today's good path must stay byte-identical and
/// seed-free. Green before AND after the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_order_segments_write_through_without_seed_reads() {
    let _g = serial().await;
    let h = make(*b"rw3b-inorder-ctl", "rw3b_ns_b").await;
    let (ino, _) = durable_striped(&h, "inorder", 0x00).await;
    const LEN: usize = 6 * BS as usize;

    let over = pattern(LEN, 0x3C);
    let before = ledger();
    for b in 0..6u64 {
        let s = b * BS;
        write_at(&h, ino, s, &over[s as usize..(s + HALF) as usize]).await;
        write_at(
            &h,
            ino,
            s + HALF,
            &over[(s + HALF) as usize..(s + BS) as usize],
        )
        .await;
    }
    fsync(&h, ino).await;
    let after = ledger();
    assert_covering_stream_ledger(before, after, 6, "in-order segments");

    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, LEN).await;
    assert_bytes(&got, &over, "in-order overwrite");
}

/// 3. Three-segment shuffles: the trigger is order-blind — including the
/// shape that creates a DISJOINT covered run (segment lands beyond a gap)
/// which the completing segment later bridges.
///
/// RED on dev: shuffle (mid, end, head) fires the old trigger at the end
/// segment (partial → inline seed fetch), strands the head segment; shuffle
/// (end, head, mid) additionally routes the head segment as a GAP write
/// (inline gap-materialize seed fetch).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ooo_three_segment_shuffles_single_write_through() {
    let _g = serial().await;
    let h = make(*b"rw3b-3segshuffle", "rw3b_ns_c").await;
    let (ino, _) = durable_striped(&h, "shuffle", 0x00).await;
    const LEN: usize = 6 * BS as usize;
    let t1 = BS / 4; // segment cuts
    let t2 = 3 * BS / 4;

    let over = pattern(LEN, 0x77);
    let seg = |b: u64, from: u64, to: u64| {
        over[(b * BS + from) as usize..(b * BS + to) as usize].to_vec()
    };

    let before = ledger();
    for b in 0..6u64 {
        let s = b * BS;
        match b % 2 {
            // (mid, end, head)
            0 => {
                write_at(&h, ino, s + t1, &seg(b, t1, t2)).await;
                write_at(&h, ino, s + t2, &seg(b, t2, BS)).await;
                write_at(&h, ino, s, &seg(b, 0, t1)).await;
            }
            // (end, head, mid) — head lands DISJOINT from the parked end run
            _ => {
                write_at(&h, ino, s + t2, &seg(b, t2, BS)).await;
                write_at(&h, ino, s, &seg(b, 0, t1)).await;
                write_at(&h, ino, s + t1, &seg(b, t1, t2)).await;
            }
        }
    }
    fsync(&h, ino).await;
    let after = ledger();
    assert_covering_stream_ledger(before, after, 6, "3-segment shuffles");

    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, LEN).await;
    assert_bytes(&got, &over, "3-segment shuffled overwrite");
}

/// 4. Item-B law pinned BOTH ways: a lone end-aligned segment (partial
/// coverage) must NOT fire write-through and must NOT fetch any seed at
/// write time; the flush of that genuinely-partial block MUST fetch its
/// old-block seed and preserve the uncovered old bytes exactly.
///
/// RED on dev: the old trigger fires on the end-aligned partial segment
/// (write_through_blocks +1, inline seed fetch at write time).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_end_aligned_segment_defers_then_flush_seeds() {
    let _g = serial().await;
    let h = make(*b"rw3b-partial-law", "rw3b_ns_d").await;
    let (ino, base) = durable_striped(&h, "partial", 0x00).await;

    let patch = pattern(HALF as usize, 0xE1);
    let before = ledger();
    write_at(&h, ino, 2 * BS + HALF, &patch).await;
    let mid = ledger();
    assert_eq!(
        mid.wt_blocks - before.wt_blocks,
        0,
        "a partial end-aligned segment must NOT fire write-through \
         (coverage-based trigger, not write_end == b_end)"
    );
    assert_eq!(
        mid.write_path_seed - before.write_path_seed,
        0,
        "partial coverage keeps DEFERRING — nothing fetches a seed at write \
         time (item-B law)"
    );
    assert_eq!(
        mid.get_obj - before.get_obj,
        0,
        "no device read at write time for a partial segment"
    );

    fsync(&h, ino).await;
    let after = ledger();
    assert!(
        after.get_obj - mid.get_obj >= 1,
        "the flush of a genuinely-partial block MUST fetch its old-block \
         seed (item-B law, the other way)"
    );
    assert!(
        after.flush_seed - mid.flush_seed >= BS,
        "the flush seed fetch must be accounted (flush_seed_read_bytes)"
    );

    let mut want = base.clone();
    want[(2 * BS + HALF) as usize..(2 * BS + BS) as usize].copy_from_slice(&patch);
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, want.len()).await;
    assert_bytes(&got, &want, "partial end-aligned segment (old head kept)");
}

/// 5. Overlap/rewrite inside the accumulation: overlapping out-of-order
/// ranges keep the coverage union exact (no double-fire, no false-complete)
/// and the content byte-exact (later write wins in the overlap).
///
/// RED on dev: the mid-stream segment ending at b_end fires the old trigger
/// with partial coverage (inline seed fetch, early upload).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_rewrites_inside_accumulation_stay_exact() {
    let _g = serial().await;
    let h = make(*b"rw3b-overlap-rwr", "rw3b_ns_e").await;
    let (ino, _) = durable_striped(&h, "overlap", 0x00).await;
    let qtr = BS / 4;

    let w1 = pattern(qtr as usize, 0x11); // [QTR, HALF)
    let w2 = pattern(HALF as usize, 0x22); // [HALF, BS)
    let w3 = pattern((qtr + 1024) as usize, 0x33); // [0, QTR+1024) — overlaps w1
    let before = ledger();
    write_at(&h, ino, 3 * BS + qtr, &w1).await;
    write_at(&h, ino, 3 * BS + HALF, &w2).await;
    let mid = ledger();
    assert_eq!(
        mid.wt_blocks - before.wt_blocks,
        0,
        "coverage [QTR, BS) is partial — the end-aligned merge must not fire"
    );
    write_at(&h, ino, 3 * BS, &w3).await; // completes the union
    fsync(&h, ino).await;
    let after = ledger();
    assert_covering_stream_ledger(before, after, 1, "overlapping rewrite");

    let mut want_block = vec![0u8; BS as usize];
    want_block[qtr as usize..HALF as usize].copy_from_slice(&w1);
    want_block[HALF as usize..].copy_from_slice(&w2);
    want_block[..w3.len()].copy_from_slice(&w3); // later write wins
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 3 * BS, BS as usize).await;
    assert_bytes(&got, &want_block, "overlapping out-of-order rewrite");
}

/// 6. Fresh-create out-of-order segments (the bench r1 face): a NEW file
/// written per-block end-segment-first must not conjure an RMW pipeline out
/// of its own earlier merges — zero device reads, zero flush pipeline, one
/// write-through per block.
///
/// RED on dev: the early trigger uploads a zero-headed block, the
/// tail-filling straggler then checks out a DEFERRED buffer against the
/// just-published mapping and drains through the flush seed+staging
/// pipeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ooo_fresh_create_segments_no_rmw_pipeline() {
    let _g = serial().await;
    let h = make(*b"rw3b-fresh-ooo-1", "rw3b_ns_f").await;
    const LEN: usize = 6 * BS as usize;
    let ino = create(&h, "freshooo").await;
    // Make the file striped first (fresh-file direct striped route), then
    // measure a fresh REGION written out of order: blocks 2..6 are written
    // end-segment-first while still unmapped (the create stream stops at
    // block 2).
    let head = pattern(2 * BS as usize, 0x0F);
    write_at(&h, ino, 0, &head).await;
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");

    let over = pattern(LEN, 0x9C);
    let before = ledger();
    for b in 2..6u64 {
        let s = b * BS;
        write_at(
            &h,
            ino,
            s + HALF,
            &over[(s + HALF) as usize..(s + BS) as usize],
        )
        .await;
        write_at(&h, ino, s, &over[s as usize..(s + HALF) as usize]).await;
    }
    fsync(&h, ino).await;
    let after = ledger();
    assert_covering_stream_ledger(before, after, 4, "fresh-create ooo segments");

    let mut want = head.clone();
    want.extend_from_slice(&over[2 * BS as usize..]);
    purge_tiers(&h, ino).await;
    let got = read_at(&h, ino, 0, LEN).await;
    assert_bytes(&got, &want, "fresh-create out-of-order fill");
}

/// 7. Reads during an out-of-order window: the covered runs serve the new
/// bytes, the gap between them serves the OLD bytes (deferred → the reader
/// pays the materialize), and a fresh file's gap serves zeros — the
/// overlay-never-invisible law extended to disjoint coverage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_during_ooo_window_serve_exact_bytes() {
    let _g = serial().await;
    let h = make(*b"rw3b-ooo-reads-1", "rw3b_ns_g").await;
    let (ino, base) = durable_striped(&h, "ooreads", 0x00).await;
    let qtr = BS / 4;

    // Overwrite file: two DISJOINT runs in block 1: [0, QTR) and [3QTR, BS).
    let r1 = pattern(qtr as usize, 0xA1);
    let r2 = pattern(qtr as usize, 0xB2);
    write_at(&h, ino, BS, &r1).await;
    write_at(&h, ino, BS + 3 * qtr, &r2).await;

    let mut want_block = base[BS as usize..2 * BS as usize].to_vec();
    want_block[..qtr as usize].copy_from_slice(&r1);
    want_block[3 * qtr as usize..].copy_from_slice(&r2);

    // Straddling read across run|gap|run: runs serve new bytes, the gap
    // serves the OLD bytes (reader materializes the deferred seed).
    let got = read_at(&h, ino, BS, BS as usize).await;
    assert_bytes(&got, &want_block, "whole-block read across disjoint runs");

    // Fresh-file variant: gap between disjoint runs of an UNMAPPED block
    // reads zeros (recycled pool bytes must never escape).
    let ino2 = create(&h, "ooreads_fresh").await;
    let big = pattern(BS as usize + 1, 0x0D);
    write_at(&h, ino2, 0, &big).await; // striped route
    let f1 = pattern(qtr as usize, 0xC3);
    let f2 = pattern(qtr as usize, 0xD4);
    write_at(&h, ino2, 2 * BS, &f1).await;
    write_at(&h, ino2, 2 * BS + 3 * qtr, &f2).await;
    let got = read_at(&h, ino2, 2 * BS, BS as usize).await;
    assert_bytes(&got[..qtr as usize], &f1, "fresh run 1");
    assert!(
        got[qtr as usize..3 * qtr as usize].iter().all(|&x| x == 0),
        "fresh gap between disjoint covered runs must read zeros"
    );
    assert_bytes(&got[3 * qtr as usize..], &f2, "fresh run 2");
}

/// 8. Crash inside an out-of-order accumulation window (extends item-B's
/// `crash_inside_window_leaves_old_block_intact`): unfsynced out-of-order
/// segments — including the END-ALIGNED one the old trigger would have
/// half-uploaded — are RAM custody only; after a crash the old durable
/// blocks are fully intact (no zeros, no partial merge, no early upload).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_with_ooo_parked_segments_leaves_old_blocks_intact() {
    let _g = serial().await;
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let meta = NamedTempFile::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let staging = tempdir().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid: *b"rw3b-crash-ooo-1",
    })
    .unwrap()
    .build(meta.path(), 128 * 1024 * 1024)
    .await
    .unwrap();

    async fn session(
        tag: &str,
        meta_path: &std::path::Path,
        backing_path: &std::path::Path,
        staging: &std::path::Path,
    ) -> H {
        let dlm = DlmClient::new("local").unwrap();
        let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
            backing_path.to_str().unwrap(),
        ));
        let ba = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), tag)
                .await
                .unwrap(),
        );
        let cache = TieredCache::new(
            vec![staging.to_path_buf()],
            Some("64MB"),
            Some("64MB"),
            Some("16MB"),
            Some("64MB"),
            dlm.meta_client().clone(),
            ba.clone(),
            nvme.clone(),
            None,
        )
        .await
        .unwrap();
        let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
        let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
        let be = KvMetaBackend::open(meta_path).await.unwrap();
        let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
        fs.router.set_meta_backend(routed.clone());
        fs.meta_backend = Some(routed);
        let req = Request {
            unique: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            pid: 1,
        };
        let b = NamedTempFile::new().unwrap();
        let m = NamedTempFile::new().unwrap();
        let s = tempdir().unwrap();
        H {
            fs,
            req,
            _b: b,
            _m: m,
            _s: s,
        }
    }

    let (ino, base) = {
        let h = session("rw3b_ns_h1", meta.path(), backing.path(), staging.path()).await;
        let (ino, base) = durable_striped(&h, "crashooo", 0x00).await;
        // Unfsynced out-of-order accumulation: the END-ALIGNED segment of
        // block 2 (the shape the old trigger would have uploaded early,
        // durably changing the block pre-crash) + an interior segment of
        // block 4. Coverage stays partial in both → both park in RAM only.
        write_at(&h, ino, 2 * BS + HALF, &pattern(HALF as usize, 0xE8)).await;
        write_at(&h, ino, 4 * BS + 8 * 1024, &pattern(4 * 1024, 0xE9)).await;
        (ino, base)
        // Session dropped: kill -9 equivalent for all RAM state.
    };

    let h2 = session("rw3b_ns_h1", meta.path(), backing.path(), staging.path()).await;
    let got = read_at(&h2, ino, 0, base.len()).await;
    assert_bytes(
        &got,
        &base,
        "post-crash read: old durable blocks fully intact (out-of-order \
         parked segments were RAM custody only — never half-uploaded)",
    );
}
