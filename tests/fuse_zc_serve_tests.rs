//! The FUSE-zc direct serve leg (K1 kill, 2026-08-06 — the read
//! program's lever #1, ruling D13): on a `FUSE_URING_ZERO_COPY` session
//! the kernel installs the READ's pages into the transport ring's sparse
//! fixed-buffer slot and skips the COMMIT folio copy — a cold aligned
//! passthrough window can therefore serve by DEVICE DMA STRAIGHT INTO
//! THE CALLER'S PAGES (`READ_FIXED(device → slot)`), deleting the serve
//! pass AND the K1 kernel copy.
//!
//! This suite pins the ROUTER half — eligibility, the fill-discipline
//! validation ladder, the engagement ledger — with an INJECTED fetch
//! primitive (`ZcReadServe::new` takes the fetch fn; the live one is the
//! fuse3 queue ring's `READ_FIXED`, reachable only on the sqz kernel —
//! the field A-B-B-A row is its acceptance venue, gated on exact
//! `fuse3_zc_replies`/`read_zc_serve_bytes` engagement).
//!
//! Contracts (red-first):
//! 1. **Engagement**: a cold, 4 KiB-aligned, passthrough single-block
//!    window with a zc handle serves through the fetch primitive — the
//!    reply body is EMPTY with `served() == Some(len)`,
//!    `read_zc_serve_bytes` accounts every byte, and the fetched device
//!    bytes are the file's bytes. BOTH sub-block and WHOLE-BLOCK shapes
//!    (zc covers the whole-block cohort dest-lease left behind).
//! 2. **Warm serves keep their venue**: a RAM-tier hit with a zc handle
//!    serves bytes (no fetch, `served() == None`, zc ledger flat).
//! 3. **Unaligned windows decline**: the ordinary ladder serves the
//!    exact bytes; zc ledger flat.
//! 4. **A failed fetch falls through**: the ordinary ladder serves the
//!    exact bytes — a refused/broken direct leg must never become a
//!    lost or wrong read.
//!
//! Counter-asserting phases share one test fn (the ledger suites'
//! counter-isolation discipline — the counters are process-global).

use fuse3::raw::prelude::Filesystem;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{ReadClassHint, ZcReadServe};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 1_048_576;
const WIN: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "1048576");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = Some(tempdir().unwrap());
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
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
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_5C5C_0001,
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

    let req = fuse3::raw::Request {
        unique: 2,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 4,
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

/// Deterministic ground-truth byte for global file offset `off`.
fn pat(off: u64) -> u8 {
    ((off % 239) as u8) ^ (((off / 4096) % 13) as u8)
}

async fn write_pattern(h: &H, ino: u64, len: u64) {
    let mut off = 0u64;
    while off < len {
        let chunk = std::cmp::min(BS, len - off) as usize;
        let data: Vec<u8> = (0..chunk as u64).map(|i| pat(off + i)).collect();
        write_at(h, ino, off, &data).await;
        off += chunk as u64;
    }
}

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
    assert!(!map.is_empty(), "fixture must promote to striped");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
}

/// The injected "request pages": a fetch primitive that runs a REAL
/// aligned `pread` on the device fd the router resolved (standing in for
/// the fuse3 ring's `READ_FIXED(device → slot)`) and captures the DMA'd
/// bytes for ground-truth comparison. `fail` makes every fetch error
/// (the broken-leg contract).
/// `(device_offset, fetched bytes)` per fetch.
type CapturedFetches = Vec<(u64, Vec<u8>)>;

struct FakePages {
    captured: Arc<Mutex<CapturedFetches>>,
    fetches: Arc<AtomicU32>,
}

impl FakePages {
    fn new() -> Self {
        Self {
            captured: Arc::new(Mutex::new(Vec::new())),
            fetches: Arc::new(AtomicU32::new(0)),
        }
    }

    fn serve(&self, fail: bool) -> ZcReadServe {
        let captured = Arc::clone(&self.captured);
        let fetches = Arc::clone(&self.fetches);
        ZcReadServe::new(Box::new(move |fd, off, len| {
            let captured = Arc::clone(&captured);
            let fetches = Arc::clone(&fetches);
            Box::pin(async move {
                fetches.fetch_add(1, Ordering::Relaxed);
                if fail {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "injected zc fetch failure",
                    ));
                }
                // O_DIRECT-aligned scratch (the zc read fd carries
                // O_DIRECT exactly like the live device fd).
                let layout =
                    std::alloc::Layout::from_size_align(len as usize, 4096).expect("layout");
                // SAFETY: nonzero aligned allocation, freed below.
                let buf = unsafe { std::alloc::alloc(layout) };
                assert!(!buf.is_null());
                // SAFETY: fd is the router-resolved device read fd; the
                // buffer is `len` writable bytes.
                let n = unsafe { libc::pread(fd, buf.cast(), len as usize, off as libc::off_t) };
                let res = if n == len as isize {
                    // SAFETY: pread initialized exactly n bytes.
                    let v = unsafe { std::slice::from_raw_parts(buf, n as usize) }.to_vec();
                    captured.lock().unwrap().push((off, v));
                    Ok(len)
                } else {
                    Err(std::io::Error::last_os_error())
                };
                // SAFETY: allocated with this layout above.
                unsafe { std::alloc::dealloc(buf, layout) };
                res
            })
        }))
    }
}

/// Contracts 1–4 in one fn (process-global counters).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zc_direct_leg_engagement_and_fallbacks() {
    let h = make([0x5C; 16], "zc-serve-a").await;
    let ino = create(&h, "zc_a.bin").await;
    write_pattern(&h, ino, 8 * BS).await;
    make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);

    // ---- Contract 1a: cold sub-block window (WIN at block 0 + WIN). ----
    let pages = FakePages::new();
    let zc = pages.serve(false);
    let zc0 = METRICS.read_zc_serve_bytes.load(Ordering::Relaxed);
    let (data, _backing) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                WIN,
                WIN as u32,
                None,
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("cold sub-block zc read");
    assert!(
        data.is_empty(),
        "a zc-served read replies with an EMPTY body (payload is in the pages)"
    );
    assert_eq!(zc.served(), Some(WIN as u32), "served length latched");
    assert_eq!(
        METRICS.read_zc_serve_bytes.load(Ordering::Relaxed) - zc0,
        WIN,
        "read_zc_serve_bytes accounts every direct-leg byte"
    );
    assert_eq!(
        pages.fetches.load(Ordering::Relaxed),
        1,
        "exactly one fetch"
    );
    {
        let cap = pages.captured.lock().unwrap();
        let (_, bytes) = cap.last().expect("captured fetch");
        assert_eq!(bytes.len(), WIN as usize);
        for (i, b) in bytes.iter().enumerate() {
            assert_eq!(
                *b,
                pat(WIN + i as u64),
                "device byte {} must be the file's byte (passthrough identity)",
                i
            );
        }
    }

    // ---- Contract 1b: whole-block shape (block 1). ----
    let zc = pages.serve(false);
    let zc0 = METRICS.read_zc_serve_bytes.load(Ordering::Relaxed);
    let (data, _backing) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                BS,
                BS as u32,
                None,
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("cold whole-block zc read");
    assert!(data.is_empty(), "whole-block zc serve replies empty");
    assert_eq!(zc.served(), Some(BS as u32));
    assert_eq!(
        METRICS.read_zc_serve_bytes.load(Ordering::Relaxed) - zc0,
        BS,
        "whole-block bytes accounted (the cohort dest-lease left behind)"
    );
    {
        let cap = pages.captured.lock().unwrap();
        let (_, bytes) = cap.last().expect("captured fetch");
        for (i, b) in bytes.iter().enumerate().step_by(4093) {
            assert_eq!(*b, pat(BS + i as u64), "whole-block byte {}", i);
        }
    }

    // ---- Contract 2: warm serves keep their venue. ----
    // Warm block 2 through an ordinary read (fills the RAM tier)…
    let (warm_fill, _b) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                2 * BS,
                BS as u32,
                None,
                ReadClassHint::default(),
                None,
                None,
            )
            .await
            .expect("warm fill read");
    assert_eq!(warm_fill.len(), BS as usize);
    // …then read it again WITH a zc handle: the warm tier must win.
    let zc = pages.serve(false);
    let zc0 = METRICS.read_zc_serve_bytes.load(Ordering::Relaxed);
    let f0 = pages.fetches.load(Ordering::Relaxed);
    let (data, _b) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                2 * BS,
                WIN as u32,
                None,
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("warm read with zc handle");
    assert_eq!(data.len(), WIN as usize, "warm serve returns bytes");
    for (i, b) in data.iter().enumerate().step_by(4099) {
        assert_eq!(*b, pat(2 * BS + i as u64), "warm byte {}", i);
    }
    assert_eq!(zc.served(), None, "warm serve never latches a zc length");
    assert_eq!(
        METRICS.read_zc_serve_bytes.load(Ordering::Relaxed),
        zc0,
        "zc ledger flat on a warm serve"
    );
    assert_eq!(
        pages.fetches.load(Ordering::Relaxed),
        f0,
        "no device fetch on a warm serve"
    );

    // ---- Contract 3: unaligned windows decline (ordinary ladder). ----
    let zc = pages.serve(false);
    let zc0 = METRICS.read_zc_serve_bytes.load(Ordering::Relaxed);
    let off = 3 * BS + 1234;
    let len = 8192u32;
    let (data, _b) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                off,
                len,
                None,
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("unaligned read with zc handle");
    assert_eq!(data.len(), len as usize);
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, pat(off + i as u64), "unaligned byte {}", i);
    }
    assert_eq!(zc.served(), None, "unaligned windows never zc-serve");
    assert_eq!(
        METRICS.read_zc_serve_bytes.load(Ordering::Relaxed),
        zc0,
        "zc ledger flat on the unaligned decline"
    );
}

/// Contract 4: a broken direct leg (every fetch errors) falls through to
/// the ordinary ladder — exact bytes, no zc engagement, never a lost or
/// wrong read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zc_fetch_failure_falls_through_to_the_ordinary_ladder() {
    let h = make([0x5D; 16], "zc-serve-b").await;
    let ino = create(&h, "zc_b.bin").await;
    write_pattern(&h, ino, 8 * BS).await;
    make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);

    let pages = FakePages::new();
    let zc = pages.serve(true);
    let zc0 = METRICS.read_zc_serve_bytes.load(Ordering::Relaxed);
    let (data, _b) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                0,
                WIN as u32,
                None,
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("read with failing zc fetch");
    assert_eq!(data.len(), WIN as usize, "the ordinary ladder served");
    for (i, b) in data.iter().enumerate() {
        assert_eq!(*b, pat(i as u64), "fallback byte {}", i);
    }
    assert_eq!(zc.served(), None, "a failed fetch never latches a serve");
    assert_eq!(
        METRICS.read_zc_serve_bytes.load(Ordering::Relaxed),
        zc0,
        "zc ledger flat when the leg failed"
    );
    assert!(
        pages.fetches.load(Ordering::Relaxed) >= 1,
        "the leg was genuinely attempted"
    );
}
