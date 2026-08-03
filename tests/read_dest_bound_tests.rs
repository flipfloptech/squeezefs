//! FUSE-4e ⊕ MEM-4 — the zero-copy READ destination is a WINDOW, not a
//! bare pointer (pre-rc-engineering-spec §4 FUSE-4e, §2 MEM-4).
//!
//! `get_payload_buffer` has always returned `(ptr, len)` and the read
//! handler threw the length away:
//!
//! ```ignore
//! .and_then(|conn| conn.get_payload_buffer(_req.slot))
//! .map(|(ptr, _sz)| ptr)
//! ```
//!
//! Every serve leg below then wrote `data.len()` bytes at that pointer,
//! and `RangedDest.cap` was set from the REQUESTED slice length rather
//! than the destination's real size. The only thing standing between a
//! serve and a heap overrun was the kernel honoring the `max_pages` it was
//! told at INIT — an unchecked cross-ABI invariant on the daemon's hottest
//! write-into-caller-memory path.
//!
//! Pinned here:
//! 1. `ReadDest` bounds every serve: a serve longer than the window is
//!    REFUSED (counted), and the request is still served correctly through
//!    the copy path — a bound may not become a lost read.
//! 2. Canary bytes past the window survive a read whose length exceeds it
//!    (the release-build overrun this item names).
//! 3. MEM-4: `RangedDest` / `ReadDest` cannot be built by safe code — the
//!    constructors carry the pointer-validity contract as `unsafe fn`.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{DataRouter, ReadClassHint, ReadDest};
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 524_288;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("dest_bound_ns").await.unwrap());
    let s = Some(tempdir().unwrap());
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
        Some("64MB"),
        Some("64MB"),
        Some("64MB"),
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
    m.as_file().set_len(64 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xDEAD_BEEF_0000_4E4E,
            uuid: *b"fuse4e-dest-bd-1",
        })
        .unwrap()
        .build(m.path(), 64 * 1024 * 1024)
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

fn pat(off: u64) -> u8 {
    ((off % 251) as u8) ^ (((off / 4096) % 7) as u8)
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_pattern(h: &H, ino: u64, len: u64) {
    let mut off = 0u64;
    while off < len {
        let chunk = std::cmp::min(BS, len - off) as usize;
        let data: Vec<u8> = (0..chunk as u64).map(|i| pat(off + i)).collect();
        let w =
            h.fs.write(
                h.req,
                ino,
                0,
                off,
                bytes::Bytes::copy_from_slice(&data),
                0,
                0,
            )
            .await
            .unwrap();
        assert_eq!(w.written as usize, chunk);
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

/// A page-aligned destination allocation with a canary tail: the read is
/// only ever allowed to touch `[0, window)`.
struct Dest {
    ptr: *mut u8,
    layout: std::alloc::Layout,
    total: usize,
}

impl Dest {
    fn new(total: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(total, 4096).unwrap();
        // SAFETY: nonzero size, valid alignment.
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null());
        // SAFETY: `total` bytes of our own fresh allocation.
        unsafe { std::ptr::write_bytes(ptr, 0xA5, total) };
        Dest { ptr, layout, total }
    }
    fn addr(&self) -> u64 {
        self.ptr as u64
    }
    fn canary_intact(&self, window: usize) -> bool {
        // SAFETY: reading our own allocation, in bounds.
        let all = unsafe { std::slice::from_raw_parts(self.ptr, self.total) };
        all[window..].iter().all(|&b| b == 0xA5)
    }
}

impl Drop for Dest {
    fn drop(&mut self) {
        // SAFETY: same ptr/layout pair as the allocation.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// FUSE-4e: a serve longer than the destination window must be refused
/// (counted) — and served correctly anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_serve_longer_than_the_dest_window_is_refused_not_overrun() {
    let h = make().await;
    let ino = create(&h, "dest_bound").await;
    write_pattern(&h, ino, 4 * BS).await;
    make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);

    // A 64 KiB request against a destination window of 8 KiB — the shape a
    // kernel that ignored `max_pages` would produce.
    const REQ: usize = 64 * 1024;
    const WINDOW: usize = 8 * 1024;
    let dest = Dest::new(REQ + 4096);
    let before = METRICS.read_dest_overruns.load(Ordering::Relaxed);
    // SAFETY: `dest` outlives the read; the window is exclusively ours.
    let rd = unsafe { ReadDest::new(dest.addr(), WINDOW) };
    let (data, _backing) = h
        .fs
        .router
        .read_file_range_zero_copy(&path, BS, REQ as u32, Some(rd), ReadClassHint::default())
        .await
        .expect("a bounded destination must never fail the read");

    assert_eq!(data.len(), REQ, "the read is served in full");
    assert!(
        data.iter()
            .enumerate()
            .all(|(i, &b)| b == pat(BS + i as u64)),
        "a refused destination must still serve the right bytes"
    );
    assert!(
        dest.canary_intact(WINDOW),
        "the serve wrote past the destination window — FUSE-4e's overrun"
    );
    assert!(
        METRICS.read_dest_overruns.load(Ordering::Relaxed) > before,
        "a refused destination must be counted (read_dest_overruns)"
    );
}

/// The ordinary case is unchanged: a window that COVERS the request serves
/// zero-copy into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_covering_window_still_serves_into_the_destination() {
    let h = make().await;
    let ino = create(&h, "dest_ok").await;
    write_pattern(&h, ino, 4 * BS).await;
    make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);

    const REQ: usize = 32 * 1024;
    let dest = Dest::new(REQ + 4096);
    let before = METRICS.read_dest_overruns.load(Ordering::Relaxed);
    // SAFETY: `dest` outlives the read; the window is exclusively ours.
    let rd = unsafe { ReadDest::new(dest.addr(), REQ) };
    let (data, _backing) = h
        .fs
        .router
        .read_file_range_zero_copy(&path, 2 * BS, REQ as u32, Some(rd), ReadClassHint::default())
        .await
        .expect("read");
    assert_eq!(data.len(), REQ);
    assert!(
        data.iter()
            .enumerate()
            .all(|(i, &b)| b == pat(2 * BS + i as u64)),
        "content"
    );
    // SAFETY: reading the window we just had served into.
    let served = unsafe { std::slice::from_raw_parts(dest.ptr, REQ) };
    assert!(
        served
            .iter()
            .enumerate()
            .all(|(i, &b)| b == pat(2 * BS + i as u64)),
        "a covering window must receive the served bytes (zero-copy leg)"
    );
    assert!(
        dest.canary_intact(REQ),
        "even a covering window must not be written past"
    );
    assert_eq!(
        METRICS.read_dest_overruns.load(Ordering::Relaxed),
        before,
        "a covering window is not a refusal"
    );
}

/// FUSE-4e: the window bound is a pure decision — pinned directly so the
/// refusal is not only reachable through a fixture.
#[test]
fn the_window_refuses_exactly_what_exceeds_it() {
    let mut buf = [0u8; 64];
    // SAFETY: `buf` outlives `d`; the window is exclusively ours.
    let d = unsafe { ReadDest::new(buf.as_mut_ptr() as u64, 64) };
    assert_eq!(d.cap(), 64);
    assert!(d.checked_ptr(0).is_some(), "an empty serve fits");
    assert!(d.checked_ptr(64).is_some(), "an exact-fit serve fits");
    let before = METRICS.read_dest_overruns.load(Ordering::Relaxed);
    assert!(d.checked_ptr(65).is_none(), "one byte over must refuse");
    assert_eq!(
        METRICS.read_dest_overruns.load(Ordering::Relaxed),
        before + 1,
        "every refusal is counted"
    );
}
