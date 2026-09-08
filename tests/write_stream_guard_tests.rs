//! W-2 `perf/write-stream-guard` — e2e perf audit ladder row 10 / board
//! item Write #6: "the order-1 write guard serializes a fresh/append
//! stream's blocks even when block locks suffice (P1-8); narrow to
//! meta-prep only on the fresh/append shape".
//!
//! Contracts pinned here (evidence note
//! `.benchmarks/2026-09-05-w2-write-stream-guard.md`):
//!
//! - **The hold instrument** (`write_lock_hold_{shared,metaprep,entire}`,
//!   acquisition → drop per FINAL scope): Σ hold counts ≡ Σ
//!   `write_lock_scope_*` on every schedule, and a stream write's hold
//!   EXCLUDES the data path — made observable with the checkout-stall
//!   seam (`set_test_checkout_stall_ms`: every write parks inside its
//!   `BLOCK_FLUSH_LOCKS`-held window), so a hold that read stall-length
//!   would mean the order-1 guard is held across the block I/O.
//! - **P1-8 on the stream shape**: K concurrent extending writes to K
//!   fresh blocks of ONE striped file run their block windows in
//!   parallel (wall ≪ K × stall), and N files × qd-K streams are
//!   parallel across AND within files with byte-exact content
//!   (`sha256` of every file after a cold read-back).
//! - **The lever (RED against `022f3647`)**: the fresh/append stream —
//!   a cache-resident striped EXTENDING write — dispatches on the inode
//!   READ guard (`write_lock_scope_shared` counts it, and the write
//!   completes while a test-held READ guard on the same ino is alive),
//!   as does a within-EOF hole-fill; the exclusive meta-prep those
//!   shapes used to take serialized a RAM-only snapshot and protected
//!   nothing the read guard does not (every inode-plane mutation the
//!   convoy design's KD-3 named runs AFTER the drop point on both
//!   modes, under (3) + (3.5)).
//! - **The A/B lever**: `SQUEEZEFS_WRITE_GUARD_NARROW=0` restores the
//!   pre-campaign class (extends/holes exclusive, mapped within-EOF
//!   overwrites Shared) with byte-identical results.

use fuse3::raw::prelude::Filesystem;
use sha2::{Digest, Sha256};
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    set_test_checkout_stall_ms, set_write_guard_narrow_for_tests, set_write_shared_for_tests,
    SqueezefsFilesystem, METRICS,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::{tempdir, NamedTempFile};

const BS: u64 = 65536;

/// Suite serializer: the posture overrides, the stall seam and the
/// process-global ledgers are shared state.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII reset of every process-global seam this suite touches.
struct SeamReset;
impl Drop for SeamReset {
    fn drop(&mut self) {
        set_test_checkout_stall_ms(0);
        set_write_shared_for_tests(true);
        set_write_guard_narrow_for_tests(true);
    }
}

async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
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
    .expect("format v3");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3")
}

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make(tag: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    // The accumulation path is the machinery under test (the field's 1 MiB
    // segments are always patch-oversize; the Bytes vehicle never rides
    // the device overlay in-process) — pin both fast paths off so the
    // downscaled BS cannot reroute the shape.
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    b.as_file().set_len(512 * 1024 * 1024).unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
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
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 128 * 1024 * 1024).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = fuse3::raw::Request {
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

async fn write_at(
    fs: &SqueezefsFilesystem,
    req: fuse3::raw::Request,
    ino: u64,
    off: u64,
    data: &[u8],
) {
    let w = fs
        .write(req, ino, 0, off, bytes::Bytes::copy_from_slice(data), 0, 0)
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// The post-crossing field shape: a striped file whose maps are durable,
/// pipeline quiet, both RAM caches warm (the `getattr` is the attr-cache
/// witness a real open/stat leaves behind).
async fn grow_striped(h: &H, ino: u64, blocks: u64) {
    let base = vec![0x11u8; (blocks * BS) as usize];
    write_at(&h.fs, h.req, ino, 0, &base).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "pipeline drains"
    );
    let _ = h.fs.getattr(h.req, ino, None, 0).await.expect("getattr");
    let path = squeezefs::keys::inode_path(ino);
    let m = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(m.file_type, "striped", "fixture must be STRIPED");
}

async fn quiesce(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(60)).await,
        "pipeline drains"
    );
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

fn block_pattern(file_tag: u8, block: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 31 + block * 7) as u8) ^ file_tag | 1)
        .collect()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ledger {
    scope: [u64; 3],
    hold_count: [u64; 3],
    hold_sum_ns: [u64; 3],
    upgrades: u64,
}

fn ledger() -> Ledger {
    Ledger {
        scope: [
            METRICS.write_lock_scope_shared.load(Ordering::Relaxed),
            METRICS.write_lock_scope_metaprep.load(Ordering::Relaxed),
            METRICS.write_lock_scope_entire.load(Ordering::Relaxed),
        ],
        hold_count: [
            METRICS.write_lock_hold_shared.count(),
            METRICS.write_lock_hold_metaprep.count(),
            METRICS.write_lock_hold_entire.count(),
        ],
        hold_sum_ns: [
            METRICS.write_lock_hold_shared.sum_ns(),
            METRICS.write_lock_hold_metaprep.sum_ns(),
            METRICS.write_lock_hold_entire.sum_ns(),
        ],
        upgrades: METRICS
            .write_lock_scope_shared_upgrades
            .load(Ordering::Relaxed),
    }
}

fn delta(a: &Ledger, b: &Ledger) -> Ledger {
    let sub = |x: [u64; 3], y: [u64; 3]| [y[0] - x[0], y[1] - x[1], y[2] - x[2]];
    Ledger {
        scope: sub(a.scope, b.scope),
        hold_count: sub(a.hold_count, b.hold_count),
        hold_sum_ns: sub(a.hold_sum_ns, b.hold_sum_ns),
        upgrades: b.upgrades - a.upgrades,
    }
}

/// Σ hold counts ≡ Σ final-scope counts, class by class.
fn assert_hold_closes(d: &Ledger, what: &str) {
    assert_eq!(
        d.hold_count, d.scope,
        "{what}: the hold ledger must close class-by-class against the \
         final-scope ledger (drop-based recording — every classified exit \
         records exactly once)"
    );
}

const SHARED: usize = 0;
const METAPREP: usize = 1;
const ENTIRE: usize = 2;

// ---------------------------------------------------------------------------
// The hold instrument
// ---------------------------------------------------------------------------

/// A stream write's order-1 hold is meta-prep only: with every write
/// parked `stall` inside its block-guarded window, the hold recorded for
/// the extending write must not contain the stall — and the ledger must
/// close (one hold, one final scope, never EntireOp for the stream shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_write_hold_excludes_the_block_io_window() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    let h = make("wsg_hold").await;
    let ino = create(&h, "hold").await;
    grow_striped(&h, ino, 4).await;

    let stall = Duration::from_millis(300);
    set_test_checkout_stall_ms(stall.as_millis() as u64);
    let l0 = ledger();
    let t0 = Instant::now();
    let data = block_pattern(0xA5, 4, BS as usize);
    write_at(&h.fs, h.req, ino, 4 * BS, &data).await; // extends: block 4 is fresh
    let wall = t0.elapsed();
    set_test_checkout_stall_ms(0);
    let d = delta(&l0, &ledger());

    assert!(
        wall >= stall,
        "the stall seam must have engaged (wall {wall:?} < stall {stall:?}) — \
         otherwise the hold assertion below proves nothing"
    );
    assert_hold_closes(&d, "one extending write");
    assert_eq!(
        d.scope[SHARED] + d.scope[METAPREP],
        1,
        "the stream shape is a drop-before-I/O class (never EntireOp)"
    );
    assert_eq!(d.scope[ENTIRE], 0);
    let hold_ns = d.hold_sum_ns[SHARED] + d.hold_sum_ns[METAPREP];
    assert!(
        hold_ns < (stall / 4).as_nanos() as u64,
        "P1-8: the order-1 hold ({hold_ns} ns) must EXCLUDE the block I/O \
         window ({stall:?}) — a stall-length hold means the inode guard is \
         held across the data path"
    );
    quiesce(&h, ino).await;
    assert_eq!(read_at(&h, ino, 4 * BS, BS as usize).await, data);
}

/// Closure over a mixed workload: extends, a within-EOF overwrite, a
/// within-EOF hole-fill, and a fresh inline file (EntireOp — whose hold
/// legitimately spans its router commit).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_ledger_closes_class_by_class_against_final_scope() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    let h = make("wsg_closure").await;
    let ino = create(&h, "c").await;
    grow_striped(&h, ino, 2).await;
    // Leave blocks 2..=3 as a hole: extend at block 4.
    write_at(&h.fs, h.req, ino, 4 * BS, &[0x22u8; BS as usize]).await;
    quiesce(&h, ino).await;

    let l0 = ledger();
    for i in 5..8u64 {
        write_at(&h.fs, h.req, ino, i * BS, &[0x33u8; 4096]).await; // extends
    }
    write_at(&h.fs, h.req, ino, BS, &[0x44u8; 4096]).await; // mapped overwrite
    write_at(&h.fs, h.req, ino, 2 * BS, &[0x55u8; 4096]).await; // hole-fill
    let tiny = create(&h, "tiny").await;
    write_at(&h.fs, h.req, tiny, 0, &[0x66u8; 512]).await; // inline: EntireOp
    let d = delta(&l0, &ledger());
    assert_eq!(d.scope.iter().sum::<u64>(), 6, "six classified writes");
    assert_hold_closes(&d, "mixed workload");
    assert_eq!(d.scope[ENTIRE], 1, "the inline write is the one EntireOp");
    assert!(
        d.hold_sum_ns[ENTIRE] > 0,
        "an EntireOp hold spans its commit and records a nonzero hold"
    );
}

// ---------------------------------------------------------------------------
// P1-8 on the stream shape (block locks suffice)
// ---------------------------------------------------------------------------

/// K concurrent extending writes into K fresh blocks of ONE striped file:
/// their block windows overlap (wall ≪ K × stall) — the order-1 guard
/// never fences the data path — and every block reads back exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_stream_writers_to_one_file_overlap_their_block_windows() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    let h = make("wsg_one_file").await;
    let ino = create(&h, "stream").await;
    grow_striped(&h, ino, 2).await;

    const K: u64 = 8;
    let stall = Duration::from_millis(200);
    set_test_checkout_stall_ms(stall.as_millis() as u64);
    let l0 = ledger();
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for k in 0..K {
        let fs = h.fs.clone();
        let req = h.req;
        tasks.push(tokio::spawn(async move {
            let b = 2 + k;
            let data = block_pattern(0x3C, b, BS as usize);
            write_at(&fs, req, ino, b * BS, &data).await;
        }));
    }
    for t in tasks {
        t.await.expect("writer task");
    }
    let wall = t0.elapsed();
    set_test_checkout_stall_ms(0);
    let d = delta(&l0, &ledger());

    assert!(wall >= stall, "the seam engaged (wall {wall:?})");
    assert!(
        wall < stall * 3,
        "P1-8: K = {K} concurrent extending writes must overlap their block \
         windows (wall {wall:?} vs the serialized K × stall = {:?})",
        stall * K as u32
    );
    assert_hold_closes(&d, "K concurrent extends");
    assert_eq!(
        d.scope[SHARED] + d.scope[METAPREP],
        K,
        "every write is a stream-class write"
    );
    let hold_ns = d.hold_sum_ns[SHARED] + d.hold_sum_ns[METAPREP];
    assert!(
        hold_ns < stall.as_nanos() as u64,
        "Σ hold over K writes ({hold_ns} ns) stays below ONE stall — no hold \
         contains a block window"
    );

    quiesce(&h, ino).await;
    purge_tiers(&h, ino).await;
    for k in 0..K {
        let b = 2 + k;
        assert_eq!(
            read_at(&h, ino, b * BS, BS as usize).await,
            block_pattern(0x3C, b, BS as usize),
            "block {b} content"
        );
    }
}

/// N fresh files × qd-K sequential streams (a per-file cursor hands each
/// task the next block — the libaio iodepth shape): parallel across AND
/// within files, and every file's cold read-back hashes to its expected
/// image.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn n_fresh_files_qd_k_streams_are_parallel_and_byte_exact() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    let h = make("wsg_n_files").await;
    const N: u64 = 4;
    const K: u64 = 4;
    const BLOCKS: u64 = 12; // stream blocks per file past the 2-block promotion
    let mut inos = Vec::new();
    for f in 0..N {
        let ino = create(&h, &format!("f{f}")).await;
        // The fresh file's first write rides the layout promotion (the
        // one EntireOp per file the field row shows); the stream is
        // everything after it.
        grow_striped(&h, ino, 2).await;
        inos.push(ino);
    }

    let stall = Duration::from_millis(100);
    set_test_checkout_stall_ms(stall.as_millis() as u64);
    let l0 = ledger();
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for (f, &ino) in inos.iter().enumerate() {
        let cursor = Arc::new(AtomicU64::new(2));
        for _ in 0..K {
            let fs = h.fs.clone();
            let req = h.req;
            let cursor = cursor.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let b = cursor.fetch_add(1, Ordering::Relaxed);
                    if b >= 2 + BLOCKS {
                        break;
                    }
                    let data = block_pattern(f as u8 + 1, b, BS as usize);
                    write_at(&fs, req, ino, b * BS, &data).await;
                }
            }));
        }
    }
    for t in tasks {
        t.await.expect("stream task");
    }
    let wall = t0.elapsed();
    set_test_checkout_stall_ms(0);
    let d = delta(&l0, &ledger());

    // Ideal: each file's K tasks pipeline BLOCKS writes ⇒ BLOCKS/K stall
    // rounds, files in parallel. Fully serialized on order 1 per file:
    // BLOCKS × stall per file; on a global lock: N × BLOCKS × stall.
    let rounds = BLOCKS.div_ceil(K) as u32;
    assert!(
        wall < stall * rounds * 3,
        "N = {N} files × qd {K} streams must run their block windows in \
         parallel: wall {wall:?} vs the per-file-serialized {:?}",
        stall * BLOCKS as u32
    );
    assert_hold_closes(&d, "N × K streams");
    assert_eq!(
        d.scope[SHARED] + d.scope[METAPREP],
        N * BLOCKS,
        "every stream write is a drop-before-I/O class"
    );
    assert_eq!(
        d.scope[ENTIRE], 0,
        "the stream past the promotion never takes EntireOp"
    );
    let hold_ns = d.hold_sum_ns[SHARED] + d.hold_sum_ns[METAPREP];
    assert!(
        hold_ns < stall.as_nanos() as u64,
        "Σ hold over {} stream writes ({hold_ns} ns) stays below ONE stall",
        N * BLOCKS
    );

    // Byte-exactness: durable, cold, hashed per file.
    for (f, &ino) in inos.iter().enumerate() {
        quiesce(&h, ino).await;
        purge_tiers(&h, ino).await;
        let mut expected = vec![0x11u8; (2 * BS) as usize];
        for b in 2..2 + BLOCKS {
            expected.extend(block_pattern(f as u8 + 1, b, BS as usize));
        }
        let mut got = Vec::with_capacity(expected.len());
        for b in 0..2 + BLOCKS {
            got.extend(read_at(&h, ino, b * BS, BS as usize).await);
        }
        assert_eq!(
            sha256(&got),
            sha256(&expected),
            "file {f}: cold read-back sha256 must match the stream's image"
        );
        let attr = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr;
        assert_eq!(attr.size, (2 + BLOCKS) * BS, "file {f}: size");
    }
}

// ---------------------------------------------------------------------------
// The lever: the stream takes the READ guard
// ---------------------------------------------------------------------------

/// Engagement: a cache-resident striped EXTENDING write (the fresh/append
/// stream) and a within-EOF hole-fill DISPATCH on the shared (read)
/// guard; the mapped overwrite keeps its Shared class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extending_stream_and_hole_fill_dispatch_on_the_shared_guard() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    set_write_shared_for_tests(true);
    set_write_guard_narrow_for_tests(true);
    let h = make("wsg_engage").await;
    let ino = create(&h, "e").await;
    grow_striped(&h, ino, 2).await;
    write_at(&h.fs, h.req, ino, 4 * BS, &[0x22u8; BS as usize]).await; // hole at 2..=3
    quiesce(&h, ino).await;

    let l0 = ledger();
    let ext = block_pattern(0x77, 5, BS as usize);
    write_at(&h.fs, h.req, ino, 5 * BS, &ext).await; // extend
    let hole = block_pattern(0x78, 2, 4096);
    write_at(&h.fs, h.req, ino, 2 * BS, &hole).await; // hole-fill
    let over = block_pattern(0x79, 1, 4096);
    write_at(&h.fs, h.req, ino, BS, &over).await; // mapped overwrite
    let d = delta(&l0, &ledger());
    assert_eq!(
        d.scope[SHARED], 3,
        "extend + hole-fill + mapped overwrite all dispatch on the READ \
         guard under the narrowed class (final-scope ledger)"
    );
    assert_eq!(
        d.scope[METAPREP], 0,
        "no exclusive drop-before-I/O dispatch remains"
    );
    assert_eq!(d.upgrades, 0, "nothing raced: no upgrade");
    assert_hold_closes(&d, "narrowed class");

    quiesce(&h, ino).await;
    purge_tiers(&h, ino).await;
    assert_eq!(read_at(&h, ino, 5 * BS, BS as usize).await, ext);
    assert_eq!(read_at(&h, ino, 2 * BS, 4096).await, hole);
    assert_eq!(read_at(&h, ino, BS, 4096).await, over);
}

/// The mode witness: an extending write COMPLETES while the test holds
/// the ino's READ guard (read-read compatible). Under the pre-campaign
/// exclusive class it would park behind the held reader — the 10 s bound
/// is the loud failure, not a schedule.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extending_write_completes_under_a_held_read_guard() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    set_write_shared_for_tests(true);
    set_write_guard_narrow_for_tests(true);
    let h = make("wsg_mode").await;
    let ino = create(&h, "m").await;
    grow_striped(&h, ino, 2).await;

    let lock = h.fs.active_inode_locks.get_inode_lock(ino);
    let held_read = lock.read().await;
    let fs = h.fs.clone();
    let req = h.req;
    let data = block_pattern(0x5A, 2, BS as usize);
    let d2 = data.clone();
    let w = tokio::spawn(async move { write_at(&fs, req, ino, 2 * BS, &d2).await });
    let done = tokio::time::timeout(Duration::from_secs(10), w).await;
    drop(held_read);
    done.expect(
        "the extending stream write must take the READ guard and complete \
         beside a held reader — parking here means the exclusive meta-prep \
         is back",
    )
    .expect("writer task");
    quiesce(&h, ino).await;
    assert_eq!(read_at(&h, ino, 2 * BS, BS as usize).await, data);
}

/// KD-2's ONE upgrade under the narrowed class: the revalidation's
/// failure mode is the snapshot EVICTION (a reclaim purge / cache drop
/// between the pre-lock probe and the held read guard) — the writer
/// drops its read guard, takes exclusive once, and completes on the
/// fetch-capable path. Deterministic: the test holds the ino's exclusive
/// guard, observes the writer PARKED on it (`SqzRwLock::waiters`), evicts
/// both caches, then releases.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn evicted_snapshot_between_probe_and_guard_upgrades_exactly_once() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    set_write_shared_for_tests(true);
    set_write_guard_narrow_for_tests(true);
    let h = make("wsg_evict").await;
    let ino = create(&h, "ev").await;
    grow_striped(&h, ino, 2).await;

    let lock = h.fs.active_inode_locks.get_inode_lock(ino);
    let held = lock.write().await;
    let l0 = ledger();
    let fs = h.fs.clone();
    let req = h.req;
    let data = block_pattern(0x6B, 2, BS as usize);
    let d2 = data.clone();
    let w = tokio::spawn(async move { write_at(&fs, req, ino, 2 * BS, &d2).await });
    // The writer's pre-lock probe admitted Shared; its read acquisition
    // parks behind our exclusive guard — wait for the park itself.
    let deadline = Instant::now() + Duration::from_secs(10);
    while lock.waiters() == 0 {
        assert!(
            Instant::now() < deadline,
            "the writer never parked on the guard"
        );
        tokio::task::yield_now().await;
    }
    h.fs.router.metadata_cache.invalidate(&ino);
    h.fs.attr_cache.invalidate(&ino);
    drop(held);
    w.await.expect("writer task");

    let d = delta(&l0, &ledger());
    assert_eq!(
        d.upgrades, 1,
        "revalidation misses once and takes the ONE upgrade"
    );
    assert_eq!(
        d.scope[SHARED], 0,
        "an upgraded op never counts a Shared final scope"
    );
    assert_eq!(
        d.scope[METAPREP], 1,
        "it completes on the exclusive drop-before-I/O class"
    );
    assert_hold_closes(&d, "evicted upgrade");
    quiesce(&h, ino).await;
    assert_eq!(read_at(&h, ino, 2 * BS, BS as usize).await, data);
}

/// KD-2/KD-7 rails survive the widening: a cold-cache extend routes
/// exclusive at admission (no Shared, no upgrade).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_cache_extend_still_routes_exclusive() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    set_write_shared_for_tests(true);
    set_write_guard_narrow_for_tests(true);
    let h = make("wsg_cold").await;
    let ino = create(&h, "k").await;
    grow_striped(&h, ino, 2).await;
    h.fs.router.metadata_cache.invalidate(&ino);
    h.fs.attr_cache.invalidate(&ino);

    let l0 = ledger();
    write_at(&h.fs, h.req, ino, 2 * BS, &[0x66u8; 4096]).await;
    let d = delta(&l0, &ledger());
    assert_eq!(
        d.scope[SHARED], 0,
        "a cache miss never dispatches Shared (KD-2)"
    );
    assert_eq!(d.upgrades, 0, "miss routes exclusive at ADMISSION");
    assert_eq!(
        d.scope[METAPREP], 1,
        "the extend runs the exclusive drop-before-I/O class"
    );
    assert_hold_closes(&d, "cold extend");
}

/// The same-binary A/B lever: `SQUEEZEFS_WRITE_GUARD_NARROW=0` restores
/// the pre-campaign class — extends and hole-fills exclusive, the mapped
/// within-EOF overwrite still Shared — with byte-identical results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn narrow_off_restores_the_pre_campaign_class() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    set_write_shared_for_tests(true);
    set_write_guard_narrow_for_tests(false);
    let h = make("wsg_ab").await;
    let ino = create(&h, "ab").await;
    grow_striped(&h, ino, 2).await;
    write_at(&h.fs, h.req, ino, 4 * BS, &[0x22u8; BS as usize]).await;
    quiesce(&h, ino).await;

    let l0 = ledger();
    let ext = block_pattern(0x87, 5, BS as usize);
    write_at(&h.fs, h.req, ino, 5 * BS, &ext).await; // extend
    let hole = block_pattern(0x88, 2, 4096);
    write_at(&h.fs, h.req, ino, 2 * BS, &hole).await; // hole-fill
    let over = block_pattern(0x89, 1, 4096);
    write_at(&h.fs, h.req, ino, BS, &over).await; // mapped overwrite
    let d = delta(&l0, &ledger());
    assert_eq!(
        d.scope[METAPREP], 2,
        "narrow off: extend + hole-fill take the exclusive class"
    );
    assert_eq!(
        d.scope[SHARED], 1,
        "narrow off: the mapped overwrite keeps the v1 Shared class"
    );
    assert_hold_closes(&d, "narrow off");

    quiesce(&h, ino).await;
    purge_tiers(&h, ino).await;
    assert_eq!(read_at(&h, ino, 5 * BS, BS as usize).await, ext);
    assert_eq!(read_at(&h, ino, 2 * BS, 4096).await, hole);
    assert_eq!(read_at(&h, ino, BS, 4096).await, over);
}

// ---------------------------------------------------------------------------
// In-process rows (release build; run explicitly):
//   cargo test --release --test write_stream_guard_tests rows_ -- --ignored --nocapture
// ---------------------------------------------------------------------------

/// Bucket snapshot of a histogram (per-row tails need a delta, the
/// histograms are process-cumulative).
fn buckets(h: &squeezefs::fuse_client::ShardedLatencyHistogram) -> Vec<u64> {
    h.buckets().to_vec()
}

/// Highest power-of-two µs bucket that gained samples between two
/// snapshots (the tail reading the note quotes beside the exact mean).
fn max_bucket_delta(a: &[u64], b: &[u64]) -> &'static str {
    let mut last = "0";
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if y > x {
            last = squeezefs::latency_core::LATENCY_BUCKET_LABELS[i];
        }
    }
    last
}

struct WaitHold {
    wait_sh: (u64, u64),
    wait_ex: (u64, u64),
    hold: [(u64, u64); 3],
    hold_buckets: [Vec<u64>; 2],
}

fn wait_hold() -> WaitHold {
    WaitHold {
        hold_buckets: [
            buckets(&METRICS.write_lock_hold_shared),
            buckets(&METRICS.write_lock_hold_metaprep),
        ],
        wait_sh: (
            METRICS.write_lock_wait_shared.count(),
            METRICS.write_lock_wait_shared.sum_ns(),
        ),
        wait_ex: (
            METRICS.write_lock_wait_exclusive.count(),
            METRICS.write_lock_wait_exclusive.sum_ns(),
        ),
        hold: [
            (
                METRICS.write_lock_hold_shared.count(),
                METRICS.write_lock_hold_shared.sum_ns(),
            ),
            (
                METRICS.write_lock_hold_metaprep.count(),
                METRICS.write_lock_hold_metaprep.sum_ns(),
            ),
            (
                METRICS.write_lock_hold_entire.count(),
                METRICS.write_lock_hold_entire.sum_ns(),
            ),
        ],
    }
}

fn mean_ns(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    let c = b.0 - a.0;
    (c, (b.1 - a.1).checked_div(c).unwrap_or(0))
}

/// One row: `files` striped files × `qd` cursor-driven stream tasks each,
/// `blocks` blocks per file written as `segs` sub-block segments (the
/// field's 1 MiB-into-4 MiB shape at BS/4), under the given posture.
async fn stream_row(h: &H, label: &str, narrow: bool, files: u64, qd: u64, blocks: u64, segs: u64) {
    set_write_guard_narrow_for_tests(narrow);
    let mut inos = Vec::new();
    for f in 0..files {
        let ino = create(h, &format!("{label}_{f}")).await;
        grow_striped(h, ino, 2).await;
        inos.push(ino);
    }
    let seg_len = BS / segs;
    let a = wait_hold();
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for &ino in &inos {
        let cursor = Arc::new(AtomicU64::new(2 * segs));
        for _ in 0..qd {
            let fs = h.fs.clone();
            let req = h.req;
            let cursor = cursor.clone();
            tasks.push(tokio::spawn(async move {
                let data = vec![0x5Cu8; seg_len as usize];
                loop {
                    let s = cursor.fetch_add(1, Ordering::Relaxed);
                    if s >= (2 + blocks) * segs {
                        break;
                    }
                    write_at(&fs, req, ino, s * seg_len, &data).await;
                }
            }));
        }
    }
    for t in tasks {
        t.await.expect("stream task");
    }
    let wall = t0.elapsed();
    let b = wait_hold();
    let writes = files * blocks * segs;
    let (wsh_n, wsh_mean) = mean_ns(a.wait_sh, b.wait_sh);
    let (wex_n, wex_mean) = mean_ns(a.wait_ex, b.wait_ex);
    let holds: Vec<(u64, u64)> = (0..3).map(|i| mean_ns(a.hold[i], b.hold[i])).collect();
    let bytes = writes * seg_len;
    eprintln!(
        "| {label} | narrow={} | {files}×qd{qd}, {blocks} blk × {segs} seg | {writes} | {:.1} ms | {:.2} GiB/s | wait_sh n={wsh_n} mean={wsh_mean} ns | wait_ex n={wex_n} mean={wex_mean} ns | hold_sh n={} mean={} ns (max {}) | hold_mp n={} mean={} ns (max {}) | hold_en n={} mean={} ns |",
        narrow as u8,
        wall.as_secs_f64() * 1e3,
        bytes as f64 / wall.as_secs_f64() / (1u64 << 30) as f64,
        holds[0].0,
        holds[0].1,
        max_bucket_delta(&a.hold_buckets[0], &b.hold_buckets[0]),
        holds[1].0,
        holds[1].1,
        max_bucket_delta(&a.hold_buckets[1], &b.hold_buckets[1]),
        holds[2].0,
        holds[2].1,
    );
    for &ino in &inos {
        quiesce(h, ino).await;
    }
}

/// The concurrency contract's shape as a row: K concurrent extends into K
/// fresh blocks of one ino under a `stall` block-window park — wall vs
/// K × stall, and the hold sum vs one stall, per posture.
async fn stalled_row(h: &H, label: &str, narrow: bool, k: u64, stall: Duration) {
    set_write_guard_narrow_for_tests(narrow);
    let ino = create(h, label).await;
    grow_striped(h, ino, 2).await;
    set_test_checkout_stall_ms(stall.as_millis() as u64);
    let a = wait_hold();
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..k {
        let fs = h.fs.clone();
        let req = h.req;
        tasks.push(tokio::spawn(async move {
            let b = 2 + i;
            write_at(&fs, req, ino, b * BS, &block_pattern(0x3C, b, BS as usize)).await;
        }));
    }
    for t in tasks {
        t.await.expect("writer");
    }
    let wall = t0.elapsed();
    set_test_checkout_stall_ms(0);
    let b = wait_hold();
    let hold_sum_ns: u64 = (0..3).map(|i| b.hold[i].1 - a.hold[i].1).sum();
    let (wsh_n, wsh_mean) = mean_ns(a.wait_sh, b.wait_sh);
    let (wex_n, wex_mean) = mean_ns(a.wait_ex, b.wait_ex);
    eprintln!(
        "| {label} | narrow={} | K={k} extends, stall {stall:?} | wall {:.1} ms (K×stall = {:.0} ms) | Σ hold {} µs | wait_sh n={wsh_n} mean={wsh_mean} ns | wait_ex n={wex_n} mean={wex_mean} ns |",
        narrow as u8,
        wall.as_secs_f64() * 1e3,
        stall.as_secs_f64() * 1e3 * k as f64,
        hold_sum_ns / 1000,
    );
    quiesce(h, ino).await;
}

/// The in-process rows behind the evidence note's tables: the convoy
/// shape (one ino, deep qd) and the field shape (many inos, qd 16), each
/// A-B-B-A across the posture within one process.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
#[ignore = "release-build row printer (see the module comment above)"]
async fn rows_in_process_release() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    set_write_shared_for_tests(true);
    let h = make("wsg_rows").await;
    eprintln!("| contract row | posture | shape | wall | Σ hold | wait shared | wait exclusive |");
    for (i, narrow) in [false, true, true, false].into_iter().enumerate() {
        stalled_row(
            &h,
            &format!("k8_stall200_{i}"),
            narrow,
            8,
            Duration::from_millis(200),
        )
        .await;
    }
    eprintln!("| row | posture | shape | writes | wall | GiB/s | wait shared | wait exclusive | hold shared | hold metaprep | hold entire |");
    // A-B-B-A: one ino, qd 16, 512 blocks × 4 segments.
    for (i, narrow) in [false, true, true, false].into_iter().enumerate() {
        stream_row(&h, &format!("one_ino_qd16_{i}"), narrow, 1, 16, 512, 4).await;
    }
    // A-B-B-A: 24 inos × qd 16, 32 blocks × 4 segments each.
    for (i, narrow) in [false, true, true, false].into_iter().enumerate() {
        stream_row(&h, &format!("24_inos_qd16_{i}"), narrow, 24, 16, 32, 4).await;
    }
    // A-B-B-A: one ino, qd 64 (the deep convoy), 512 blocks × 4 segments.
    for (i, narrow) in [false, true, true, false].into_iter().enumerate() {
        stream_row(&h, &format!("one_ino_qd64_{i}"), narrow, 1, 64, 512, 4).await;
    }
}

/// `SQUEEZEFS_WRITE_SHARED=0` dominates: no Shared dispatch at all, the
/// narrow posture notwithstanding.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_shared_off_dominates_the_narrow_posture() {
    let _s = SERIAL.lock().await;
    let _reset = SeamReset;
    set_write_shared_for_tests(false);
    set_write_guard_narrow_for_tests(true);
    let h = make("wsg_off").await;
    let ino = create(&h, "off").await;
    grow_striped(&h, ino, 2).await;

    let l0 = ledger();
    write_at(&h.fs, h.req, ino, 2 * BS, &[0x99u8; 4096]).await; // extend
    write_at(&h.fs, h.req, ino, BS, &[0x9Au8; 4096]).await; // mapped overwrite
    let d = delta(&l0, &ledger());
    assert_eq!(d.scope[SHARED], 0, "posture OFF: no Shared dispatch");
    assert_eq!(d.scope[METAPREP], 2);
    assert_hold_closes(&d, "shared off");
}
