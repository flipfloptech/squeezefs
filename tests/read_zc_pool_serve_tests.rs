//! The fd-source zc READ serve (R-4, e2e perf audit read board #4/#5,
//! `perf/read-zc-serve`, 2026-09-05 — `SQUEEZEFS_READ_ZC_SERVE`).
//!
//! On a `FUSE_URING_ZERO_COPY` session every out-paged reply body moves
//! into the request's pages through the queue ring — `READ_FIXED(fd →
//! slot)`. The kernel primitive is fd-SOURCED: the direct leg names the
//! device fd, the bounce leg names the per-ent bounce memfd, and there is
//! no io_uring op that copies an anonymous VA into a fixed buffer (the
//! bounce exists because "an anonymous mapping has no fd for the ring ops
//! to name"). So a warm/cold-slice serve on an armed session paid TWO
//! passes: the daemon's copy tier-buffer → bounce, then the kernel's
//! bounce → folios. The lever makes the whole-block READ FILL POOL
//! fd-addressable (one memfd slab, `MAP_SHARED` — the `ZcBounce` law:
//! the SAME bytes reachable by VA and by fd), so a serve whose source
//! `Bytes` lives in that pool hands the transport `(fd, offset)` and the
//! kernel bridges the tier buffer itself into the folios: ONE pass, the
//! daemon copy DELETED. No kernel change — the bridge op is the one the
//! bounce already rides.
//!
//! Contracts (red-first; the knob is read at pool init, so every test in
//! this binary arms it before the first pool touch):
//! 1. **Substrate**: the armed pool exists; a whole-block handout is
//!    fd-addressable (`fd_offset_of`), sub-slices map to `off + delta`,
//!    foreign pointers and slab-crossing ranges do not, and the memfd
//!    reads back what the VA wrote (two-way reachability).
//! 2. **Cold fill slice-out arm**: an UNALIGNED window (the direct leg
//!    declines) on a zc handle returns the pool slice itself — no dest
//!    copy (`read_copy_fill_slice_bytes`/`read_copy_dest_bytes` flat),
//!    `read_zc_pool_serve_bytes` accounts it, the bytes are exact.
//! 3. **Hot arm**: the re-read serves the hot entry by fd — the copy
//!    elided (`read_copy_hot_serve_bytes` flat), the warm subset counts.
//! 4. **Hold arm**: a pool-backed hold entry serves by fd likewise.
//! 5. **Non-pool sources keep the copy path**: an NVMe read-cache
//!    (mmap) entry copies into the dest exactly as before, even armed.
//! 6. **The fast probe (R-2 venue)**: a hot block served by the reap
//!    thread's SYNC ladder answers `FastReadProbe::ServedFd` — the pool
//!    slice + its fd address, no window copy.
//! 7. **Without a zc handle** (an un-armed session's request), every arm
//!    copies into the dest as before — the lever changes armed sessions
//!    only.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::reply::FastReadProbe;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::pool::{read_bounce_pool, zc_fill_fd_offset, zc_fill_pool};
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
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const BS: u64 = 1_048_576;

fn arm_lever() {
    // Read once at pool init: must precede the first pool touch in this
    // process (every test calls this first).
    std::env::set_var("SQUEEZEFS_READ_ZC_SERVE", "1");
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "1048576");
}

struct H {
    fs: SqueezefsFilesystem,
    req: fuse3::raw::Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    arm_lever();
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
            hash_seed: 0xC0FF_EE00_5C5C_0002,
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

fn pat(off: u64) -> u8 {
    ((off % 241) as u8) ^ (((off / 4096) % 11) as u8)
}

async fn write_pattern(h: &H, ino: u64, len: u64) {
    let mut off = 0u64;
    while off < len {
        let chunk = std::cmp::min(BS, len - off) as usize;
        let data: Vec<u8> = (0..chunk as u64).map(|i| pat(off + i)).collect();
        let w =
            h.fs.write(h.req, ino, 0, off, bytes::Bytes::from(data), 0, 0)
                .await
                .unwrap();
        assert_eq!(w.written as usize, chunk);
        off += chunk as u64;
    }
}

async fn make_cold(h: &H, ino: u64) -> Arc<std::collections::HashMap<u32, String>> {
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
    map
}

/// 4 KiB-aligned scratch destination (the bounce-slot stand-in).
struct AlignedDest {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl AlignedDest {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        // SAFETY: non-zero size, valid layout.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Self { ptr, layout }
    }
    fn dest(&self) -> squeezefs::routing::ReadDest {
        // SAFETY: the allocation outlives every read it is handed to.
        unsafe { squeezefs::routing::ReadDest::new(self.ptr as u64, self.layout.size()) }
    }
    fn contains(&self, p: *const u8) -> bool {
        let a = self.ptr as usize;
        let q = p as usize;
        q >= a && q < a + self.layout.size()
    }
}

impl Drop for AlignedDest {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout above.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// A zc handle whose device fetch is never expected on these arms (the
/// direct leg declines unaligned windows and warm blocks): any fetch
/// fails loud.
fn zc_handle() -> ZcReadServe<'static> {
    ZcReadServe::new(Box::new(|_fd, _off, _len| {
        Box::pin(async {
            Err(std::io::Error::other(
                "unexpected device fetch on an fd-source serve arm",
            ))
        })
    }))
}

struct Snap {
    dest: u64,
    warm: u64,
    hot: u64,
    hold: u64,
    cache: u64,
    fill_slice: u64,
    bounce: u64,
    pool: u64,
    pool_warm: u64,
}

fn snap() -> Snap {
    Snap {
        dest: METRICS.read_copy_dest_bytes.load(Ordering::Relaxed),
        warm: METRICS.read_copy_warm_serve_bytes.load(Ordering::Relaxed),
        hot: METRICS.read_copy_hot_serve_bytes.load(Ordering::Relaxed),
        hold: METRICS.read_copy_hold_serve_bytes.load(Ordering::Relaxed),
        cache: METRICS.read_copy_cache_serve_bytes.load(Ordering::Relaxed),
        fill_slice: METRICS.read_copy_fill_slice_bytes.load(Ordering::Relaxed),
        bounce: METRICS.read_copy_bounce_bytes.load(Ordering::Relaxed),
        pool: METRICS.read_zc_pool_serve_bytes.load(Ordering::Relaxed),
        pool_warm: METRICS
            .read_zc_pool_serve_warm_bytes
            .load(Ordering::Relaxed),
    }
}

fn delta(s0: &Snap) -> Snap {
    let s1 = snap();
    Snap {
        dest: s1.dest - s0.dest,
        warm: s1.warm - s0.warm,
        hot: s1.hot - s0.hot,
        hold: s1.hold - s0.hold,
        cache: s1.cache - s0.cache,
        fill_slice: s1.fill_slice - s0.fill_slice,
        bounce: s1.bounce - s0.bounce,
        pool: s1.pool - s0.pool,
        pool_warm: s1.pool_warm - s0.pool_warm,
    }
}

fn pread_all(fd: i32, off: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    // SAFETY: `v` is `len` writable bytes; `fd` is the pool's live memfd.
    let n = unsafe { libc::pread(fd, v.as_mut_ptr().cast(), len, off as libc::off_t) };
    assert_eq!(n, len as isize, "pread on the pool memfd");
    v
}

/// Contract 1: the substrate.
#[test]
fn armed_fill_pool_is_fd_addressable_both_ways() {
    arm_lever();
    let pool = zc_fill_pool().expect("SQUEEZEFS_READ_ZC_SERVE=1 arms the memfd fill pool");
    assert!(
        Arc::ptr_eq(read_bounce_pool(4 * 1024 * 1024), pool),
        "whole-block read fills route to the armed pool"
    );
    assert!(
        !Arc::ptr_eq(read_bounce_pool(4096), pool),
        "sub-block (ranged) fills keep the RANGED pool"
    );
    let (ptr, bytes) = pool.alloc();
    let len = pool.buf_size();
    assert_eq!(bytes.len(), len);
    let (fd, off) = pool
        .fd_offset_of(ptr, len)
        .expect("a pool handout is fd-addressable");
    assert_eq!(
        zc_fill_fd_offset(ptr, len),
        Some((fd, off)),
        "the process-wide probe agrees with the pool's"
    );
    // Sub-slice → offset shifts by the same delta.
    assert_eq!(
        // SAFETY: inside the handout.
        pool.fd_offset_of(unsafe { ptr.add(8192) }, 4096),
        Some((fd, off + 8192))
    );
    // The slab's last page is addressable; a range crossing the slab's
    // end is not (the fd names the slab and nothing past it).
    let (base, span) = pool.slab_range().expect("armed pool is slab-backed");
    let last_page = (base + span - 4096) as *const u8;
    assert_eq!(
        pool.fd_offset_of(last_page, 4096),
        Some((fd, (span - 4096) as u64))
    );
    assert_eq!(pool.fd_offset_of(last_page, 8192), None);
    let foreign = Box::new([0u8; 64]);
    assert_eq!(
        pool.fd_offset_of(foreign.as_ptr(), 64),
        None,
        "a heap pointer is never fd-addressable"
    );
    // Two-way reachability: the VA write is the memfd's content.
    // SAFETY: `ptr` is this test's exclusive handout of `len` bytes.
    unsafe {
        for i in 0..len {
            *ptr.add(i) = pat(i as u64);
        }
    }
    let back = pread_all(fd, off, 16384);
    assert!(
        back.iter().enumerate().all(|(i, &b)| b == pat(i as u64)),
        "pread(memfd) returns the bytes written through the VA"
    );
    drop(bytes);
}

/// Contracts 2–5 + 7 on one fixture (process-global counters).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fd_source_serve_elides_the_copy_on_pool_backed_arms_only() {
    let h = make([0x7A; 16], "zc-pool-a").await;
    let ino = create(&h, "zcp_a.bin").await;
    write_pattern(&h, ino, 8 * BS).await;
    let map = make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let dest = AlignedDest::new(BS as usize);

    // ---- Contract 2: cold fill → the slice-out arm, UNALIGNED window
    // (the direct leg declines) ABOVE the ranged threshold (a sub-256 KiB
    // window would take the §5.6 ranged bounce, whose 64 KiB pool is
    // heap — a different arm); the whole-block fill lands in the armed
    // pool and the slice-out arm hands back its slice.
    let off = 5 * BS + 1234;
    let len = 384 * 1024usize;
    let zc = zc_handle();
    let s0 = snap();
    let data =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                off,
                len as u32,
                Some(dest.dest()),
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("cold unaligned read");
    assert_eq!(data.len(), len);
    assert!(
        data.iter()
            .enumerate()
            .all(|(i, &b)| b == pat(off + i as u64)),
        "contract 2: exact bytes"
    );
    assert!(
        !dest.contains(data.as_ptr()),
        "contract 2: the served Bytes are NOT the dest — the copy was elided"
    );
    let (fd, foff) = zc_fill_fd_offset(data.as_ptr(), data.len())
        .expect("contract 2: the served slice is fd-addressable (the pool fill itself)");
    assert_eq!(
        pread_all(fd, foff, len),
        data.to_vec(),
        "contract 2: the fd address names exactly the served bytes"
    );
    assert_eq!(
        zc.served(),
        None,
        "no direct-leg serve on an unaligned window"
    );
    let d = delta(&s0);
    assert_eq!(d.fill_slice, 0, "contract 2: the slice-out copy is DELETED");
    assert_eq!(d.dest, 0, "contract 2: no dest copy at all");
    assert_eq!(d.bounce, 0, "contract 2: no bounce either");
    assert_eq!(
        d.pool, len as u64,
        "contract 2: read_zc_pool_serve_bytes accounts the fd-source serve"
    );
    assert_eq!(d.pool_warm, 0, "contract 2: a cold serve is not warm");
    drop(data);

    // ---- Contract 3: the hot arm (block 5 is hot from the fill above).
    let zc = zc_handle();
    let s0 = snap();
    let hot0 = METRICS.hot_block_hits.load(Ordering::Relaxed);
    let data =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                5 * BS,
                (384 * 1024) as u32,
                Some(dest.dest()),
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("hot read");
    assert_eq!(data.len(), 384 * 1024);
    assert!(
        METRICS.hot_block_hits.load(Ordering::Relaxed) > hot0,
        "contract 3: served by the hot tier"
    );
    assert!(
        data.iter()
            .enumerate()
            .step_by(4099)
            .all(|(i, &b)| b == pat(5 * BS + i as u64)),
        "contract 3: exact bytes"
    );
    let (fd, foff) = zc_fill_fd_offset(data.as_ptr(), data.len())
        .expect("contract 3: the hot slice is fd-addressable");
    assert_eq!(pread_all(fd, foff, 4096), data[..4096].to_vec());
    let d = delta(&s0);
    assert_eq!(d.hot, 0, "contract 3: the hot-arm copy is DELETED");
    assert_eq!(d.warm, 0);
    assert_eq!(d.dest, 0);
    assert_eq!(d.pool, 384 * 1024, "contract 3: accounted");
    assert_eq!(
        d.pool_warm,
        384 * 1024,
        "contract 3: ... in the warm subset"
    );
    drop(data);

    // ---- Contract 4: the hold arm — a pool-backed entry planted for a
    // block no tier holds (block 6).
    let k6 = map.get(&6).expect("block 6 mapped").clone();
    let pool = zc_fill_pool().unwrap();
    let (p6, b6) = pool.alloc();
    // SAFETY: exclusive handout of `buf_size` bytes; BS ≤ buf_size.
    unsafe {
        for i in 0..BS as usize {
            *p6.add(i) = pat(6 * BS + i as u64);
        }
    }
    h.fs.router
        .cache
        .read_lane_hold
        .insert(&k6, b6.slice(..BS as usize), 64 * 1024 * 1024);
    let zc = zc_handle();
    let s0 = snap();
    let serves0 = METRICS.read_lane_serves.load(Ordering::Relaxed);
    let data =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                6 * BS + 4096,
                (256 * 1024) as u32,
                Some(dest.dest()),
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("hold read");
    assert_eq!(data.len(), 256 * 1024);
    assert_eq!(
        METRICS.read_lane_serves.load(Ordering::Relaxed) - serves0,
        1,
        "contract 4: served by the hold"
    );
    assert!(
        data.iter()
            .enumerate()
            .step_by(4099)
            .all(|(i, &b)| b == pat(6 * BS + 4096 + i as u64)),
        "contract 4: exact bytes"
    );
    assert!(
        zc_fill_fd_offset(data.as_ptr(), data.len()).is_some(),
        "contract 4: the hold slice is fd-addressable"
    );
    let d = delta(&s0);
    assert_eq!(d.hold, 0, "contract 4: the hold-arm copy is DELETED");
    assert_eq!(d.dest, 0);
    assert_eq!(d.pool, 256 * 1024);
    assert_eq!(d.pool_warm, 256 * 1024);
    drop(data);

    // ---- Contract 5: the NVMe read cache (mmap — not fd-addressable
    // through the pool) keeps the copy path even armed.
    let k7 = map.get(&7).expect("block 7 mapped").clone();
    let block7: Vec<u8> = (0..BS).map(|i| pat(7 * BS + i)).collect();
    h.fs.router
        .cache
        .nvme
        .cache_read_block(&k7, bytes::Bytes::from(block7))
        .expect("disk-tier plant");
    let zc = zc_handle();
    let s0 = snap();
    let data =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                7 * BS,
                (384 * 1024) as u32,
                Some(dest.dest()),
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .expect("cache read");
    assert_eq!(data.len(), 384 * 1024);
    assert!(
        dest.contains(data.as_ptr()),
        "contract 5: a non-pool source copies INTO the dest as before"
    );
    let d = delta(&s0);
    assert_eq!(d.cache, 384 * 1024, "contract 5: the cache copy is counted");
    assert_eq!(d.dest, 384 * 1024);
    assert_eq!(d.pool, 0, "contract 5: no fd-source serve");
    drop(data);

    // ---- Contract 7: no zc handle (an un-armed session) ⇒ the hot arm
    // copies into the dest exactly as before, lever notwithstanding.
    let s0 = snap();
    let data =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                5 * BS,
                (384 * 1024) as u32,
                Some(dest.dest()),
                ReadClassHint::default(),
                None,
                None,
            )
            .await
            .expect("un-armed hot read");
    assert_eq!(data.len(), 384 * 1024);
    assert!(
        dest.contains(data.as_ptr()),
        "contract 7: without a zc handle the serve lands in the dest"
    );
    let d = delta(&s0);
    assert_eq!(
        d.hot,
        384 * 1024,
        "contract 7: the copy is paid and counted"
    );
    assert_eq!(d.pool, 0, "contract 7: the lever never engages off-session");
    drop(data);
}

/// Contract 6: the R-2 fast probe hands back the fd-addressable slice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fast_probe_answers_served_fd_for_pool_backed_hot_blocks() {
    let h = make([0x7B; 16], "zc-pool-b").await;
    let ino = create(&h, "zcp_b.bin").await;
    write_pattern(&h, ino, 4 * BS).await;
    make_cold(&h, ino).await;
    let path = squeezefs::keys::inode_path(ino);
    let dest = AlignedDest::new(BS as usize);

    // Warm block 2 through an ordinary (un-armed) read.
    let fill =
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
            .expect("warm fill");
    assert_eq!(fill.len(), BS as usize);
    drop(fill);

    // The armed reply window: `dest` stands in for the bounce slot.
    let part = 512 * 1024usize;
    let s0 = snap();
    let probe = h.fs.read_fast_probe(
        ino,
        0,
        2 * BS,
        part as u32,
        0,
        Some((dest.dest().addr(), dest.dest().cap())),
    );
    let (body, fd, foff) = match probe {
        FastReadProbe::ServedFd { body, fd, off } => (body, fd, off),
        other => panic!("contract 6: expected ServedFd, got {other:?}"),
    };
    assert_eq!(body.len(), part);
    assert!(
        body.iter()
            .enumerate()
            .step_by(4093)
            .all(|(i, &b)| b == pat(2 * BS + i as u64)),
        "contract 6: exact bytes"
    );
    assert!(!dest.contains(body.as_ptr()), "contract 6: no window copy");
    assert_eq!(
        zc_fill_fd_offset(body.as_ptr(), body.len()),
        Some((fd, foff)),
        "contract 6: the fd address IS the body's pool address"
    );
    assert_eq!(pread_all(fd, foff, 4096), body[..4096].to_vec());
    let d = delta(&s0);
    assert_eq!(d.hot, 0, "contract 6: the fast-probe hot copy is DELETED");
    assert_eq!(d.dest, 0);
    assert_eq!(d.pool, part as u64);
    assert_eq!(d.pool_warm, part as u64);

    // A probe WITHOUT a window (the in-process heap stand-in) keeps the
    // copy path: nothing to hand an fd address to.
    let s0 = snap();
    match h.fs.read_fast_probe(ino, 0, 2 * BS, 4096, 0, None) {
        FastReadProbe::Served(b) => assert_eq!(b.len(), 4096),
        other => panic!("heap-sink probe must serve bytes (got {other:?})"),
    }
    let d = delta(&s0);
    assert_eq!(d.pool, 0, "no fd-source serve without a reply window");
    assert_eq!(d.bounce, 4096, "the heap copy is a counted bounce");
}
