//! READ fast-dispatch from the reap thread — e2e perf audit **R-2**
//! (read board #2; `.benchmarks/2026-09-03-4k-random-attribution.md`
//! §4/§7: transport ingress `queue_wait` 69 + `dispatch_lag` 89 = 158 µs
//! of a 439 µs kern 4 KiB random read, device inside the op 40 µs).
//!
//! The lever: the fuse3 queue worker runs the filesystem's SYNC try-only
//! probe (`Filesystem::read_fast_probe`) at the delivery CQE. A warm READ
//! is served + committed inline (no channel, no wake, no lane); a miss
//! mints the full handler straight onto a `fuse3-tpc` lane (the inbound
//! queue + session dispatch task skipped).
//!
//! Laws pinned here (the fuse3 side pins the eligibility core and the
//! served-op accounting in its own suite):
//!
//! 1. **Warm ⇒ Served, byte-exact**: an active-buffer-resident read and
//!    a tier-resident read answer `Served` with exactly the bytes
//!    `Filesystem::read` returns (same EOF clamp; EOF is `Served(empty)`).
//! 2. **A tier serve lands in the dest window and aliases it** (the
//!    commit's copy elision keys on pointer equality).
//! 3. **Cold ⇒ Demote**, and the handler it defers to still serves.
//! 4. **The reap thread never blocks**: a writer holding the per-inode
//!    lock is a `Demote` in bounded time (the `try_read()` law), never a
//!    wait — the lock IS the seam.
//! 5. **Virtual inodes demote** (their payloads are regenerated async).
//! 6. **Served-op accounting**: `queue_wait == dispatch_lag == 0` exact
//!    zeros, `transport_total` the arrival→commit span, and the trace
//!    chain `transport_recv ≡ fast_dispatch → reply_commit`.
//!
//! RED against 12f6d3f5 + the R-2 step-1/2 commits: the trait default is
//! `Demote` for every shape (laws 1–2), and no session registers a
//! dispatcher.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::reply::FastReadProbe;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{read_transport_phase_json, SqueezefsFilesystem, STATS_INODE};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::op_trace::{self, Stage};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 512 KiB blocks — the read_serve_phase suite's geometry (hot-tier
/// landing class, ≥ 64 KiB publish branch).
const BS: u64 = 524_288;

/// Process-global instrument families + the trace ring: serialize.
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

async fn make(test_id: &str, uuid: [u8; 16]) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_WINDOW", "0");
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("128MB"),
        Some("128MB"),
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
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap()
        .data
        .to_vec()
}

/// fsync + purge every read-tier retention of every current block key.
async fn make_cold(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 3) % 251) as u8 ^ tag).collect()
}

fn served(p: FastReadProbe) -> bytes::Bytes {
    match p {
        FastReadProbe::Served(b) => b,
        FastReadProbe::Demote => panic!("expected Served, got Demote"),
    }
}

// ---------------------------------------------------------------------------
// Law 1 — warm active-buffer reads serve byte-exact, EOF-clamped
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_active_buffer_read_is_served_byte_exact_and_eof_clamped() {
    let _g = serial().await;
    let h = make("rfd_active", *b"rfd-active-vol!!").await;
    let ino = create(&h, "warm").await;
    let data = pattern(12 * 1024, 0x11);
    write_at(&h, ino, 0, &data).await;

    // The written bytes sit in the active block buffer: a sub-block read
    // is the handler's zero-copy snapshot slice — servable sync.
    let b = served(h.fs.read_fast_probe(ino, 0, 0, 4096, 0, None));
    assert_eq!(&b[..], &data[..4096], "served bytes are the written bytes");
    assert_eq!(
        b.to_vec(),
        read_at(&h, ino, 0, 4096).await,
        "the probe answers exactly what Filesystem::read answers"
    );

    // EOF clamp: a read straddling the file end is shortened exactly as
    // the handler shortens it.
    let b = served(h.fs.read_fast_probe(ino, 0, 8192, 8192, 0, None));
    assert_eq!(&b[..], &data[8192..], "clamped to EOF");
    assert_eq!(b.to_vec(), read_at(&h, ino, 8192, 8192).await);

    // Past EOF: Served(empty), never a Demote (the handler returns empty
    // without touching a device — so must the probe).
    let b = served(h.fs.read_fast_probe(ino, 0, 1 << 20, 4096, 0, None));
    assert!(b.is_empty(), "past-EOF is an empty serve");
}

// ---------------------------------------------------------------------------
// Laws 2 + 3 — cold demotes; a tier-resident block serves INTO the dest
// window and aliases it
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_block_demotes_then_the_tier_resident_block_serves_into_the_dest_window() {
    let _g = serial().await;
    let h = make("rfd_tier", *b"rfd-tier-vol!!!!").await;
    let ino = create(&h, "tier").await;
    let data = pattern(BS as usize, 0x22);
    write_at(&h, ino, 0, &data).await;
    make_cold(&h, ino).await;

    // Cold: no RAM-resident copy anywhere — the probe must demote (a
    // device fetch is the lanes' business).
    assert_eq!(
        h.fs.read_fast_probe(ino, 0, 0, 4096, 0, None),
        FastReadProbe::Demote,
        "a cold block is never served sync"
    );
    // The handler it defers to serves the cold read (and warms a tier).
    assert_eq!(read_at(&h, ino, 0, 128 * 1024).await, &data[..128 * 1024]);

    // Warm now: a DIFFERENT sub-range of the same block serves from the
    // tier ladder (staging / hot / hold / read-cache), landing in the
    // caller's dest window — the served Bytes ALIAS the window.
    let off = 256 * 1024u64;
    let len = 64 * 1024usize;
    let mut dest = vec![0u8; 1 << 20];
    let b = served(h.fs.read_fast_probe(
        ino,
        0,
        off,
        len as u32,
        0,
        Some((dest.as_mut_ptr() as u64, dest.len())),
    ));
    assert_eq!(b.len(), len);
    assert_eq!(
        &b[..],
        &data[off as usize..off as usize + len],
        "tier bytes"
    );
    assert_eq!(
        &dest[..len],
        &data[off as usize..off as usize + len],
        "the serve landed in the dest window"
    );
    assert!(
        std::ptr::eq(b.as_ptr(), dest.as_ptr()),
        "the served Bytes alias the dest window (the commit elides the copy)"
    );
    // The dest window's cap is honored: a window too small for the read
    // is refused — the probe demotes rather than overrun (FUSE-4e).
    let mut tiny = vec![0u8; 4096];
    assert_eq!(
        h.fs.read_fast_probe(
            ino,
            0,
            off,
            len as u32,
            0,
            Some((tiny.as_mut_ptr() as u64, tiny.len()))
        ),
        FastReadProbe::Demote,
        "a dest window smaller than the serve demotes"
    );
}

// ---------------------------------------------------------------------------
// Law 4 — the reap thread never blocks: a held writer lock is a Demote
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_holding_the_inode_lock_demotes_in_bounded_time() {
    let _g = serial().await;
    let h = make("rfd_lock", *b"rfd-lock-vol!!!!").await;
    let ino = create(&h, "locked").await;
    let data = pattern(8192, 0x33);
    write_at(&h, ino, 0, &data).await;
    // Warm and servable…
    assert_eq!(
        &served(h.fs.read_fast_probe(ino, 0, 0, 4096, 0, None))[..],
        &data[..4096]
    );
    // …until a writer holds the per-inode lock: the probe's `try_read()`
    // fails and it demotes IMMEDIATELY (the seam is the lock itself — no
    // test-only hook, and no spin: the reap thread must never wait).
    let guard = h.fs.get_inode_lock_ref(ino).write().await;
    let t0 = std::time::Instant::now();
    let p = h.fs.read_fast_probe(ino, 0, 0, 4096, 0, None);
    let took = t0.elapsed();
    assert_eq!(p, FastReadProbe::Demote, "a held writer lock demotes");
    assert!(
        took < std::time::Duration::from_millis(50),
        "the probe must not wait on the lock (took {took:?})"
    );
    drop(guard);
    assert_eq!(
        &served(h.fs.read_fast_probe(ino, 0, 0, 4096, 0, None))[..],
        &data[..4096],
        "servable again once the writer is gone"
    );
}

// ---------------------------------------------------------------------------
// Law 5 — virtual inodes demote
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn virtual_inodes_demote() {
    let _g = serial().await;
    let h = make("rfd_virt", *b"rfd-virt-vol!!!!").await;
    assert_eq!(
        h.fs.read_fast_probe(STATS_INODE, 0, 0, 4096, 0, None),
        FastReadProbe::Demote,
        "the stats inode's payload is regenerated async — never sync-served"
    );
}

// ---------------------------------------------------------------------------
// Law 6 — served-op accounting: exact zero ingress + the trace chain
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn served_op_records_zero_ingress_and_the_fast_dispatch_trace_chain() {
    use fuse3::raw::connection::fuse_over_uring::fast_dispatch;
    let _g = serial().await;
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    op_trace::disarm();
    let _ = op_trace::drain();
    op_trace::arm_for_tests(1);

    let words = |phase: &str| -> (u64, u64) {
        let fam = read_transport_phase_json();
        let hh = &fam[phase];
        (
            hh["count"].as_u64().unwrap(),
            hh["sum_ns"].as_u64().unwrap(),
        )
    };
    let (q0, d0, t0) = (
        words("queue_wait"),
        words("dispatch_lag"),
        words("transport_total"),
    );
    let s0 = fuse3::fast_dispatch_serves();

    // A served READ: arrival == fast-dispatch instant; committed 9 µs later.
    let unique = 0x5EED_0000_0000_0042u64;
    let arrived = fuse3::raw::connection::fuse_over_uring::fast_dispatch::now_ns();
    fast_dispatch::record_served(unique, arrived, arrived + 9_000);

    assert_eq!(fuse3::fast_dispatch_serves() - s0, 1, "one serve counted");
    let (q1, d1, t1) = (
        words("queue_wait"),
        words("dispatch_lag"),
        words("transport_total"),
    );
    assert_eq!((q1.0 - q0.0, q1.1 - q0.1), (1, 0), "queue_wait: exact zero");
    assert_eq!(
        (d1.0 - d0.0, d1.1 - d0.1),
        (1, 0),
        "dispatch_lag: exact zero"
    );
    assert_eq!(
        (t1.0 - t0.0, t1.1 - t0.1),
        (1, 9_000),
        "transport_total: the arrival→commit span"
    );

    let samples = op_trace::drain();
    op_trace::disarm();
    let mine: Vec<(Stage, u64)> = samples
        .iter()
        .filter(|s| s.op_id == unique)
        .map(|s| (Stage::from_u16(s.stage).expect("known stage"), s.mono_ns))
        .collect();
    let stage_of = |st: Stage| {
        mine.iter()
            .find(|(s, _)| *s == st)
            .map(|(_, ns)| *ns)
            .unwrap_or_else(|| panic!("served op must stamp {st:?}; got {mine:?}"))
    };
    let recv = stage_of(Stage::TransportRecv);
    let fast = stage_of(Stage::FastDispatch);
    let commit = stage_of(Stage::ReplyCommit);
    assert_eq!(
        recv, fast,
        "fast_dispatch is stamped AT the arrival instant"
    );
    assert_eq!(commit - recv, 9_000, "reply_commit closes the span");
    assert!(
        !mine
            .iter()
            .any(|(s, _)| matches!(s, Stage::Dispatch | Stage::HandlerEntry)),
        "a served op never stamps dispatch/handler_entry (it paid neither)"
    );
    assert_eq!(Stage::FastDispatch.name(), "fast_dispatch");
    assert_eq!(Stage::from_u16(6), Some(Stage::FastDispatch), "wire id 6");
}
