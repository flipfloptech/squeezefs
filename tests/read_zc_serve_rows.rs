//! R-4 in-process rows (`.benchmarks/2026-09-05-r4-read-zc-serve.md` §5):
//! the READ copy ledger on a warm 1 MiB loop through the router's hot arm
//! with a zc handle and a dest, printed as one JSON line per row. The
//! lever is read at pool init, so the A/B is two PROCESSES:
//!
//! ```text
//! cargo test --release --test read_zc_serve_rows -- --nocapture                       # A: lever off
//! SQUEEZEFS_READ_ZC_SERVE=1 cargo test --release --test read_zc_serve_rows -- --nocapture  # B: lever on
//! ```
//!
//! Not a perf claim (a file-backed sandbox, the fake zc handle never
//! fetches, the transport bridge is not in the loop): the row prices the
//! DAEMON copy side only — `read_copy_*` vs `read_zc_pool_serve_*` per
//! byte served, and the handler-lane ns/op the elided memcpy returns.
//! The field rows (tcp devsub, sqz kernel, root) are the parent's.

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
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile};

const BS: u64 = 1_048_576;
const BLOCKS: u64 = 8;
const LOOPS: usize = 64;
/// Non-sequential touch order (a sequential sweep classifies as a stream
/// and R1b skips the hot publish — the row must land every block hot),
/// and an UNALIGNED window above the ranged threshold so only the tier
/// arms (and, cold, the fill slice-out) can serve it.
const ORDER: [u64; 8] = [5, 2, 7, 0, 3, 6, 1, 4];
const WIN_OFF: u64 = 1234;
const WIN_LEN: u32 = 384 * 1024;

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
}

impl Drop for AlignedDest {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout above.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

fn zc_handle() -> ZcReadServe {
    ZcReadServe::new(Box::new(|_fd, _off, _len| {
        Box::pin(async { Err(std::io::Error::other("no device fetch on the warm row")) })
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_hot_loop_row() {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "1048576");
    let lever = squeezefs::cache::pool::zc_fill_pool().is_some();
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("zc-rows").await.unwrap());
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
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_5C5C_0003,
            uuid: [0x7C; 16],
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
    let ino = fs
        .create(req, 1, OsStr::new("row.bin"), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino;
    let data = bytes::Bytes::from(vec![0xA5u8; BS as usize]);
    for blk in 0..BLOCKS {
        let w = fs
            .write(req, ino, 0, blk * BS, data.clone(), 0, 0)
            .await
            .unwrap();
        assert_eq!(w.written as u64, BS);
    }
    fs.fsync(req, ino, 0, false).await.unwrap();
    let path = squeezefs::keys::inode_path(ino);
    let map = fs
        .router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default();
    for key in map.values() {
        fs.router.cache.purge_block_key(key);
    }
    let dest = AlignedDest::new(BS as usize);

    // Warm every block through the ordinary ladder (one pooled fill each,
    // deposited in hot probation), then prove them hot with a second pass
    // — the fill counts (dest + fill_slice) stay out of the row's window.
    for pass in 0..2 {
        for &blk in &ORDER {
            let h0 = METRICS.hot_block_hits.load(Ordering::Relaxed);
            let (d, _b) = fs
                .router
                .read_file_range_zero_copy_with_meta(
                    &path,
                    blk * BS + WIN_OFF,
                    WIN_LEN,
                    Some(dest.dest()),
                    ReadClassHint::default(),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(d.len(), WIN_LEN as usize);
            if pass == 1 {
                assert!(
                    METRICS.hot_block_hits.load(Ordering::Relaxed) > h0,
                    "warm-up: block {blk} must be hot before the row"
                );
            }
        }
    }

    let snap = || {
        (
            METRICS.read_copy_dest_bytes.load(Ordering::Relaxed),
            METRICS.read_copy_warm_serve_bytes.load(Ordering::Relaxed),
            METRICS.read_copy_hot_serve_bytes.load(Ordering::Relaxed),
            METRICS.read_zc_pool_serve_bytes.load(Ordering::Relaxed),
            METRICS
                .read_zc_pool_serve_warm_bytes
                .load(Ordering::Relaxed),
            METRICS.hot_block_hits.load(Ordering::Relaxed),
        )
    };
    let s0 = snap();
    let t0 = std::time::Instant::now();
    let mut served = 0u64;
    for i in 0..LOOPS {
        let blk = ORDER[i % BLOCKS as usize];
        let zc = zc_handle();
        let (d, _b) = fs
            .router
            .read_file_range_zero_copy_with_meta(
                &path,
                blk * BS + WIN_OFF,
                WIN_LEN,
                Some(dest.dest()),
                ReadClassHint::default(),
                None,
                Some(&zc),
            )
            .await
            .unwrap();
        served += d.len() as u64;
        drop(d);
    }
    let elapsed = t0.elapsed();
    let s1 = snap();
    let d = (
        s1.0 - s0.0,
        s1.1 - s0.1,
        s1.2 - s0.2,
        s1.3 - s0.3,
        s1.4 - s0.4,
        s1.5 - s0.5,
    );
    // Closure on this shape: every served byte is either a dest copy or an
    // fd-source serve.
    assert_eq!(d.0 + d.3, served, "ledger closure on the warm loop");
    assert_eq!(d.5 as usize, LOOPS, "every loop read was a hot hit");
    println!(
        "R4_ROW {{\"lever\":{lever},\"loops\":{LOOPS},\"served_bytes\":{served},\
         \"read_copy_dest_bytes\":{},\"read_copy_warm_serve_bytes\":{},\
         \"read_copy_hot_serve_bytes\":{},\"read_zc_pool_serve_bytes\":{},\
         \"read_zc_pool_serve_warm_bytes\":{},\"hot_block_hits\":{},\
         \"ns_per_op\":{}}}",
        d.0,
        d.1,
        d.2,
        d.3,
        d.4,
        d.5,
        elapsed.as_nanos() / LOOPS as u128
    );
}
