//! Staged-identity read-visibility contract — the fstests generic/074, /127,
//! /616 (+/075 at low rate) TRANSIENT-ZEROS family.
//!
//! Under buffered write+read churn a read must NEVER observe zeros (or an
//! error) for a range whose write was acked and not since overwritten,
//! truncated away, or punched — regardless of which staged-identity
//! transition is in flight: an in-place re-stage of the same `file_id`, a
//! merge-worker promotion to a durable block, a spill, or a layout flip
//! (staged→inline, staged→striped). The zeros-degrade leg
//! (`staged_payload_lost_reads`) is a CRASH-RECOVERY contract for payloads
//! discarded by segment recovery; it must be unreachable for live data.
//!
//! Root causes pinned by these tests (see the fix commits):
//!   RC1 `NvmeShard::reserve_and_write` removed the key's index entry before
//!       copying the replacement (phase 1) and re-inserted it after
//!       (phase 2): every re-stage exposed a "key absent" window the length
//!       of a payload memcpy. A racing read missed the ring, found no
//!       promoted mapping (never-promoted staged files have none), and fell
//!       into the zeros-degrade leg.
//!   RC2 staged→inline publishes the new identity only in RAM
//!       (`layout_dirty`), then releases the ring entry: a reader holding
//!       the pre-transition meta snapshot missed ring + mapping + backend.
//!   RC3 a reader that resolved a promoted `block_map[0]` could fetch the
//!       block after a racing re-stage/transition freed it
//!       (`release_superseded_staged`) — freed/reallocated bytes served
//!       without a binding revalidation.
//!   RC4 dead staging-ring extents (tombstoned old copies, removed entries)
//!       stayed RSS-resident: first-fit walks the segment, every dead
//!       extent's pages remain mapped-dirty (unreclaimable on tmpfs) — the
//!       observed ~24 MB/min churn creep.
//!
//! Fill discipline: writers alternate exactly two fill bytes (0xAA/0xBB), so
//! at ANY instant a correct read of covered data returns one uniform fill
//! from that set. Zeros (or any other byte) = the transient-zeros bug.

use bytes::Bytes;
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
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 64 KiB block size: inline (<= 4 KiB), staged (4 KiB .. 64 KiB),
/// striped (> 64 KiB) — all three layouts drivable with small payloads.
const BS: u64 = 65536;
/// Staged-file working size (well inside one block).
const STAGED_LEN: usize = 48 * 1024;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(tag: &str, staging_write_budget: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(staging_write_budget),
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
    let mut uuid = *b"sidv-regress-v3!";
    uuid[..4].copy_from_slice(&tag.as_bytes()[..4]);
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
        h.fs.write(h.req, ino, 0, off, Bytes::copy_from_slice(data), 0, 0)
            .await
            .unwrap();
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn truncate_to(h: &H, ino: u64, size: u64) {
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(size),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

/// One racing reader over `[0, len)` of the file `ino_src` currently points
/// at: every reply must be exactly `len` bytes of ONE fill byte from
/// {0xAA, 0xBB} — never zeros, never short, never torn, never an error.
/// Returns the number of reads performed.
async fn reader_loop(
    router: DataRouter,
    ino_src: Arc<AtomicU64>,
    len: usize,
    stop: Arc<AtomicBool>,
    tag: &'static str,
) -> u64 {
    let mut reads = 0u64;
    while !stop.load(Ordering::Acquire) {
        let ino = ino_src.load(Ordering::Acquire);
        let file_path = squeezefs::keys::inode_path(ino);
        let (data, _backing) = router
            .read_file_range_zero_copy(
                &file_path,
                0,
                len as u32,
                None,
                squeezefs::routing::ReadClassHint::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("[{tag}] read errored during identity churn: {e:?}"));
        if data.len() != len {
            let cache_meta = router.metadata_cache.get(&ino);
            panic!(
                "[{tag}] short read of live staged data: got {} want {len} \
                 (identity transition dropped coverage); cache meta = {:?}",
                data.len(),
                cache_meta.map(|m| (m.file_type, m.size, m.file_id, m.layout_dirty))
            );
        }
        let first = data[0];
        assert!(
            first == 0xAA || first == 0xBB,
            "[{tag}] TRANSIENT ZEROS/STALE: read returned {first:#04x} (legal fills 0xaa/0xbb) — \
             the zeros-degrade leg fired for LIVE data"
        );
        if let Some(pos) = data.iter().position(|&x| x != first) {
            panic!(
                "[{tag}] TORN READ: byte {pos} = {:#04x} differs from reply fill {first:#04x}",
                data[pos]
            );
        }
        reads += 1;
    }
    reads
}

fn rss_kb() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let resident_pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    resident_pages * (unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64) / 1024
}

fn spawn_readers(
    h: &H,
    n: usize,
    ino_src: &Arc<AtomicU64>,
    len: usize,
    stop: &Arc<AtomicBool>,
    tag: &'static str,
) -> Vec<tokio::task::JoinHandle<u64>> {
    (0..n)
        .map(|_| {
            tokio::spawn(reader_loop(
                h.fs.router.clone(),
                ino_src.clone(),
                len,
                stop.clone(),
                tag,
            ))
        })
        .collect()
}

async fn join_readers(readers: Vec<tokio::task::JoinHandle<u64>>) -> u64 {
    let mut total = 0u64;
    for r in readers {
        total += r.await.expect("reader panicked");
    }
    total
}

fn assert_no_lost_reads(lost_before: u64, what: &str) {
    let lost_after = METRICS.staged_payload_lost_reads.load(Ordering::Relaxed);
    assert_eq!(
        lost_after - lost_before,
        0,
        "zeros-degrade leg fired {} time(s) for LIVE data during {what}",
        lost_after - lost_before
    );
}

// ---------------------------------------------------------------------------
// RC1: in-place re-stage storm. Every buffered rewrite of a staged file
// replaces its ring entry under the SAME file_id; a racing read must always
// resolve one intact payload (old or new) — the ring index may never present
// an "absent" window, and the zeros-degrade leg may never fire.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn restage_storm_reads_never_transient_zeros() {
    let h = make("rc1s", "64MB").await;
    let ino = create(&h, "restage_storm").await;
    let lost_before = METRICS.staged_payload_lost_reads.load(Ordering::Relaxed);

    write_at(&h, ino, 0, &vec![0xAAu8; STAGED_LEN]).await;

    let ino_src = Arc::new(AtomicU64::new(ino));
    let stop = Arc::new(AtomicBool::new(false));
    let readers = spawn_readers(&h, 3, &ino_src, STAGED_LEN, &stop, "rc1-restage");

    for round in 0..1500u32 {
        let fill = if round % 2 == 0 { 0xBBu8 } else { 0xAAu8 };
        write_at(&h, ino, 0, &vec![fill; STAGED_LEN]).await;
    }

    stop.store(true, Ordering::Release);
    let total_reads = join_readers(readers).await;
    assert!(
        total_reads > 100,
        "harness self-check: readers must actually race the writer (got {total_reads} reads)"
    );
    assert_no_lost_reads(lost_before, "re-stage churn");
}

// ---------------------------------------------------------------------------
// RC2: layout-transition storm. Each round takes a FRESH file through
// inline→staged→inline→staged→striped (+truncates), while readers race the
// head KiB — which every step rewrites with the round's fill, so it is live
// data under every layout. Every transition publishes its new identity
// before releasing the old one; a racing read must re-resolve the moved
// identity — never zeros, never short.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn layout_transition_storm_reads_never_transient_zeros() {
    let h = make("rc2t", "64MB").await;
    let lost_before = METRICS.staged_payload_lost_reads.load(Ordering::Relaxed);

    let first = create(&h, "transition_0").await;
    write_at(&h, first, 0, &vec![0xAAu8; STAGED_LEN]).await;

    let ino_src = Arc::new(AtomicU64::new(first));
    let stop = Arc::new(AtomicBool::new(false));
    let readers = spawn_readers(&h, 3, &ino_src, 1024, &stop, "rc2-transition");

    for round in 0..80u32 {
        let fill = if round % 2 == 0 { 0xBBu8 } else { 0xAAu8 };
        // Fresh file per round: striped files never return to staged, so the
        // full transition chain needs a new inode each time.
        let ino = create(&h, &format!("transition_{}", round + 1)).await;
        // inline → staged
        write_at(&h, ino, 0, &vec![fill; STAGED_LEN]).await;
        // Point the readers at the new identity only once its head is live.
        ino_src.store(ino, Ordering::Release);
        // staged → staged (re-stage, shrunk by truncate)
        truncate_to(&h, ino, 2048).await;
        // staged → inline (size fits inline again)
        write_at(&h, ino, 0, &vec![fill; 2048]).await;
        // inline → staged (fresh file_id, fresh ring entry)
        write_at(&h, ino, 0, &vec![fill; STAGED_LEN]).await;
        // staged → striped (grow past one block; ring entry released)
        write_at(&h, ino, 0, &vec![fill; 3 * BS as usize]).await;
        // striped truncate + head rewrite (active-block overlay path)
        truncate_to(&h, ino, 2048).await;
        write_at(&h, ino, 0, &vec![fill; 2048]).await;
    }

    stop.store(true, Ordering::Release);
    let total_reads = join_readers(readers).await;
    assert!(
        total_reads > 100,
        "harness self-check: readers must actually race the writer (got {total_reads} reads)"
    );
    assert_no_lost_reads(lost_before, "layout transitions");
}

// ---------------------------------------------------------------------------
// RC3: promotion/release churn under capacity pressure. A tiny staging
// budget keeps the merge worker promoting resident entries while writers
// re-stage them (releasing the promoted durable copies). Readers race the
// promote→remove and re-stage→release windows; a read that resolved a
// promoted mapping must revalidate the binding before serving.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn promotion_pressure_storm_reads_never_transient_zeros() {
    let h = make("rc3p", "384KB").await;
    let lost_before = METRICS.staged_payload_lost_reads.load(Ordering::Relaxed);

    let mut inos = Vec::new();
    for i in 0..6 {
        let ino = create(&h, &format!("promo_{i}")).await;
        write_at(&h, ino, 0, &vec![0xAAu8; STAGED_LEN]).await;
        inos.push(ino);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for &ino in &inos {
        let ino_src = Arc::new(AtomicU64::new(ino));
        readers.extend(spawn_readers(
            &h,
            1,
            &ino_src,
            STAGED_LEN,
            &stop,
            "rc3-promotion",
        ));
    }

    for round in 0..250u32 {
        let fill = if round % 2 == 0 { 0xBBu8 } else { 0xAAu8 };
        for &ino in &inos {
            write_at(&h, ino, 0, &vec![fill; STAGED_LEN]).await;
        }
        // Give the merge worker poll points to interleave promotions.
        tokio::task::yield_now().await;
    }

    stop.store(true, Ordering::Release);
    let total_reads = join_readers(readers).await;
    assert!(
        total_reads > 100,
        "harness self-check: readers must actually race the writer (got {total_reads} reads)"
    );
    assert_no_lost_reads(lost_before, "promotion churn");
}

// ---------------------------------------------------------------------------
// RC1 at the ring level: a same-key replace must be ATOMIC for readers —
// `get` returns the old intact payload or the new intact payload, never
// None and never a torn value, across the whole replace.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ring_same_key_replace_is_atomic_for_readers() {
    use squeezefs::tiering::nvme::NvmeCache;

    let dir = tempdir().unwrap();
    let cache = Arc::new(NvmeCache::new(&[dir.path()], &[64 * 1024 * 1024], 1).unwrap());
    let key = Bytes::from_static(b"the-one-staged-file-id");
    let payload_len = 192 * 1024;

    // Seed round 0.
    let meta = vec![7u8; 32];
    assert!(cache.reserve_and_write(key.clone(), 32, &meta, &vec![0xAAu8; payload_len], None));

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..3 {
        let cache = cache.clone();
        let key = key.clone();
        let stop = stop.clone();
        readers.push(tokio::task::spawn_blocking(move || {
            let mut hits = 0u64;
            let mut copy = vec![0u8; 0];
            while !stop.load(Ordering::Acquire) {
                {
                    let guard = cache.get(&key).unwrap_or_else(|| {
                        panic!(
                            "RING IDENTITY GAP: get() returned None mid-replace \
                             (index entry absent while a same-key replacement was in flight)"
                        )
                    });
                    let val = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
                    copy.clear();
                    copy.extend_from_slice(val);
                    // Guard drops here: production holds are bounded (§5.5) —
                    // scanning outside the guard keeps writers unstarved.
                }
                // Value layout here: 8B meta_len + 32B meta + payload.
                let payload = &copy[40..];
                let first = payload[0];
                assert!(
                    first == 0xAA || first == 0xBB,
                    "ring served foreign bytes {first:#04x}"
                );
                assert!(
                    payload.iter().all(|&x| x == first),
                    "ring served a TORN payload"
                );
                hits += 1;
                std::thread::yield_now();
            }
            hits
        }));
    }

    let writer = {
        let cache = cache.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            for round in 1..1200u32 {
                let fill = if round % 2 == 0 { 0xAAu8 } else { 0xBBu8 };
                let meta = vec![7u8; 32];
                assert!(
                    cache.reserve_and_write(key.clone(), 32, &meta, &vec![fill; payload_len], None),
                    "replace refused with ample free space at round {round}"
                );
            }
        })
    };
    writer.await.unwrap();
    stop.store(true, Ordering::Release);
    let mut total = 0u64;
    for r in readers {
        total += r.await.expect("ring reader panicked");
    }
    assert!(total > 100, "harness self-check: readers raced ({total})");
}

// ---------------------------------------------------------------------------
// Striped-tier sibling (the generic/074 fstest.3 shape): partial writes to a
// STRIPED file check the RAM active-block overlay out of the shared map for
// the whole RMW (and the one-authority ring-sibling removal ran BEFORE the
// merge landed), while capacity pressure (tiny staging budget) stretches and
// multiplies the windows with spills/refusals. A read racing those windows
// missed every overlay and served the durable map's PREVIOUS-round block (or
// a hole): stale fills / zeros for acked data. Readers here must only ever
// see the current or previous round's fill.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn striped_pressure_storm_reads_never_stale_or_zeros() {
    let h = make("rc5s", "2MB").await;
    let flen = 3 * BS as usize; // striped: 3 blocks of 64 KiB

    let mut inos = Vec::new();
    for i in 0..3 {
        let ino = create(&h, &format!("striped_{i}")).await;
        write_at(&h, ino, 0, &vec![0xAAu8; flen]).await;
        inos.push(ino);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for &ino in &inos {
        for off in [0u64, BS + 512, 2 * BS + 4096] {
            let router = h.fs.router.clone();
            let stop = stop.clone();
            readers.push(tokio::spawn(async move {
                let file_path = squeezefs::keys::inode_path(ino);
                let mut reads = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let (data, _b) = router
                        .read_file_range_zero_copy(
                            &file_path,
                            off,
                            1024,
                            None,
                            squeezefs::routing::ReadClassHint::default(),
                        )
                        .await
                        .unwrap_or_else(|e| panic!("[rc5-striped] read errored: {e:?}"));
                    assert_eq!(data.len(), 1024, "[rc5-striped] short read of live data");
                    let first = data[0];
                    assert!(
                        first == 0xAA || first == 0xBB,
                        "[rc5-striped] STALE/ZEROS: {first:#04x} at off {off} \
                         (legal fills 0xaa/0xbb) — overlay window served the durable map"
                    );
                    assert!(
                        data.iter().all(|&x| x == first),
                        "[rc5-striped] torn 1 KiB read"
                    );
                    reads += 1;
                }
                reads
            }));
        }
    }

    for round in 0..12u32 {
        let fill = if round % 2 == 0 { 0xBBu8 } else { 0xAAu8 };
        for &ino in &inos {
            // Full rewrite (complete blocks -> write-through under pressure).
            write_at(&h, ino, 0, &vec![fill; flen]).await;
            // Partial-block RMW churn (checkout windows) across all blocks.
            for off in [512u64, BS - 512, BS + 512, 2 * BS + 512, flen as u64 - 1024] {
                write_at(&h, ino, off, &vec![fill; 1024]).await;
            }
            // fsync drains buffers through the flush path (its own windows).
            h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        }
    }

    stop.store(true, Ordering::Release);
    let total_reads = join_readers(readers).await;
    assert!(
        total_reads > 100,
        "harness self-check: readers must actually race the writer (got {total_reads} reads)"
    );
}

// ---------------------------------------------------------------------------
// In-flight placements are untouchable: a replace copies its payload OUTSIDE
// the shard lock, so a concurrent different-key placement (wrap-around /
// punched-hole reuse) that ignores the not-yet-indexed extent rewrites those
// bytes before the index insert — the first key then serves the second key's
// whole payload (observed live: a staged file reading back another file's
// fill under capacity churn). Two writers churn disjoint keys through a
// shard small enough to wrap constantly; every key must read back exactly
// its own last-written fill.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ring_concurrent_placements_never_overlap() {
    use squeezefs::tiering::nvme::NvmeCache;

    let dir = tempdir().unwrap();
    // 2 MiB shard, 2 writers x 4 keys x 100 KiB payloads (+ replace
    // headroom): placements constantly wrap and reuse punched holes.
    let cache = Arc::new(NvmeCache::new(&[dir.path()], &[2 * 1024 * 1024], 1).unwrap());
    let payload_len = 100 * 1024;

    let mut writers = Vec::new();
    for w in 0..2u8 {
        let cache = cache.clone();
        writers.push(tokio::task::spawn_blocking(move || {
            let mut last_fill = [0u8; 4];
            for round in 0..400u32 {
                for k in 0..4u8 {
                    let key = Bytes::from(format!("w{w}-key{k}"));
                    // Promotion-shaped churn: removing an entry punches a
                    // hole BEHIND the cursor, which first-fit then offers to
                    // the next placement — the geometry that overlapped an
                    // in-flight copy.
                    if round % 3 == 2 && k % 2 == (w % 2) {
                        if cache.remove(&key).is_some() {
                            last_fill[k as usize] = 0;
                        }
                        continue;
                    }
                    let fill = 1 + ((round as u8) % 100) + w * 100 + k;
                    let meta = vec![7u8; 32];
                    // Refusal is legal backpressure (both writers mid-flight
                    // can exceed the shard); the key keeps its old payload.
                    if cache.reserve_and_write(key, 32, &meta, &vec![fill; payload_len], None) {
                        last_fill[k as usize] = fill;
                    }
                }
            }
            last_fill
        }));
    }
    let mut finals = Vec::new();
    for w in writers {
        finals.push(w.await.expect("ring writer panicked"));
    }

    for (w, fills) in finals.iter().enumerate() {
        for (k, &fill) in fills.iter().enumerate() {
            if fill == 0 {
                continue; // every stage refused (cannot happen, but be safe)
            }
            let key = Bytes::from(format!("w{w}-key{k}"));
            let copy = {
                let guard = cache
                    .get(&key)
                    .unwrap_or_else(|| panic!("w{w}-key{k} lost from the ring"));
                guard.guard.mmap[guard.offset..guard.offset + guard.len].to_vec()
            };
            let payload = &copy[40..];
            assert_eq!(payload.len(), payload_len, "w{w}-key{k} wrong length");
            if let Some(pos) = payload.iter().position(|&b| b != fill) {
                panic!(
                    "CROSS-KEY CLOBBER: w{w}-key{k} byte {pos:#x} = {:#04x}, want fill {fill:#04x} \
                     (a concurrent placement overwrote an in-flight copy)",
                    payload[pos]
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RC4: dead ring extents must not retain RSS. Re-staging one key walks the
// segment first-fit; every superseded copy is dead the moment the replace
// lands and its pages must be returned to the OS (punched/madvised), or a
// long churn drags resident memory toward the whole segment size
// (unreclaimable when staging sits on tmpfs — the observed ~24 MB/min creep).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_ring_extents_do_not_retain_rss() {
    use squeezefs::tiering::nvme::NvmeCache;

    let dir = tempdir().unwrap();
    let seg_size = 512 * 1024 * 1024usize;
    let cache = Arc::new(NvmeCache::new(&[dir.path()], &[seg_size], 1).unwrap());
    let key = Bytes::from_static(b"rss-churn-file-id");
    let payload_len = 240 * 1024;
    let meta = vec![7u8; 32];

    assert!(cache.reserve_and_write(key.clone(), 32, &meta, &vec![0x11u8; payload_len], None));
    let baseline_kb = rss_kb();

    // ~2000 replaces * ~244 KiB ≈ 480 MiB of first-fit traversal — almost the
    // whole segment. Live data is ONE 240 KiB payload throughout.
    let churn = {
        let cache = cache.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            for round in 0..2000u32 {
                let fill = (round % 251) as u8;
                let meta = vec![7u8; 32];
                assert!(
                    cache.reserve_and_write(key.clone(), 32, &meta, &vec![fill; payload_len], None),
                    "replace refused at round {round}"
                );
            }
        })
    };
    churn.await.unwrap();

    let after_kb = rss_kb();
    let drift_mb = after_kb.saturating_sub(baseline_kb) / 1024;
    assert!(
        drift_mb < 64,
        "staging churn retained {drift_mb} MiB of dead ring extents \
         (live payload is 240 KiB; superseded copies must be reclaimed)"
    );

    // Reclaim must not have harmed the live entry.
    let guard = cache.get(&key).expect("live entry must survive reclaim");
    let val = &guard.guard.mmap[guard.offset..guard.offset + guard.len];
    let payload = &val[40..];
    assert_eq!(payload.len(), payload_len);
    let expect = ((2000u32 - 1) % 251) as u8;
    assert!(payload.iter().all(|&x| x == expect), "live payload damaged");
}

// ---------------------------------------------------------------------------
// OQ-5 (lost-wakeup wedge, 2026-07-30): the warm all-RAM read serve had ZERO
// tokio coop-budget leaves — a task looping warm reads never ended its poll,
// so tokio's unstealable LIFO slot starved any task woken from inside that
// loop (the storm test's times-drain task, woken by a DLM guard drop, was
// scheduled into the reading worker's LIFO slot and never polled again; its
// fully-assigned RwLock permits died with it and the staged→striped
// promotion parked forever on a permit-less stripe semaphore).
//
// Contract pinned here: `read_file_range_zero_copy` consumes coop budget, so
// a warm-read loop yields to peer tasks on the SAME worker within one budget
// window (≤ 128 iterations), even when every serve is Ready-immediately.
// Deterministic single-threaded shape: without the budget leaf, task A's
// poll never ends and B never runs (A completes all 100k iterations); with
// it, A yields within ~128 iterations and observes B's flag.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "current_thread")]
async fn warm_read_loop_yields_to_peer_tasks_on_one_worker() {
    let h = make("oq5c", "64MB").await;
    let ino = create(&h, "warm_read_coop").await;
    write_at(&h, ino, 0, &vec![0xAAu8; STAGED_LEN]).await;

    // Warm the serve once: the loop below must be all-RAM Ready-immediate.
    let file_path = squeezefs::keys::inode_path(ino);
    let (data, _b) =
        h.fs.router
            .read_file_range_zero_copy(
                &file_path,
                0,
                1024,
                None,
                squeezefs::routing::ReadClassHint::default(),
            )
            .await
            .expect("warmup read");
    assert_eq!(data.len(), 1024);

    let flag = Arc::new(AtomicBool::new(false));

    // A: spawned FIRST — the current-thread scheduler polls it before B.
    let reader = {
        let router = h.fs.router.clone();
        let file_path = file_path.clone();
        let flag = flag.clone();
        tokio::spawn(async move {
            let mut iters = 0u32;
            while iters < 100_000 {
                let (data, _b) = router
                    .read_file_range_zero_copy(
                        &file_path,
                        0,
                        1024,
                        None,
                        squeezefs::routing::ReadClassHint::default(),
                    )
                    .await
                    .expect("warm read");
                assert_eq!(data.len(), 1024);
                iters += 1;
                if flag.load(Ordering::Acquire) {
                    break;
                }
            }
            iters
        })
    };
    // B: can only run if A's poll ends while A still loops.
    let peer = {
        let flag = flag.clone();
        tokio::spawn(async move {
            flag.store(true, Ordering::Release);
        })
    };

    let iters = reader.await.expect("reader task");
    peer.await.expect("peer task");
    assert!(
        iters < 100_000,
        "warm read loop starved a peer task on the same worker for 100k \
         iterations: the read serve consumed no coop budget (OQ-5 lost-wakeup \
         wedge shape — the LIFO-slot victim never runs)"
    );
}
