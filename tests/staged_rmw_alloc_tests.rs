//! The staged-file RMW allocation flood (follow-up C, root-caused with
//! dhat on the aged-daemon storm — `.benchmarks` note carries the
//! profile): every sub-block write/punch/copy_range to a STAGED file
//! rebuilt the whole image through fresh heap (`read_staged` → `Vec`
//! seed, `Bytes::from(existing_data)` assembly) — ~21 GB of 4 MiB-class
//! malloc/free churn in a 30 k-op fsx storm, ~1.5 GiB live at gmax, and
//! the jemalloc retention that churn leaves behind is exactly the
//! intermittent QUICK tier-tail cage kill (anon 8.33 GiB, no VMA over
//! 260 MB). The KV metadata core was a CO-SYMPTOM (it journals each
//! op's size/mtime commit at storm rate) — its allocations measured
//! modest.
//!
//! Contract pinned here: the staged RMW seed/assembly path runs through
//! the bounded, authority-gauged `BUFFER_POOL` — pooled seeds are
//! COUNTED, recycle back to the pool (no per-op malloc/free of the
//! image), and the bytes stay exact.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::pool::BUFFER_POOL;
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
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    // Default 4 MiB blocks: a 2 MiB file stays STAGED (the flood shape).
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
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
        Some("128MB"),
        Some("128MB"),
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
    assert_eq!(w.written as usize, data.len());
}

/// fsx-storm shape: scattered 4 KiB overwrites into a 2 MiB staged file —
/// every op RMW-rebuilds the whole staged image.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_rmw_storm_is_pool_backed_recycling_and_byte_exact() {
    const IMG: usize = 2 * 1024 * 1024;
    const OPS: u64 = 200;
    let h = make(*b"stagedrmw-c-fu01", "srmw_ns_a").await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("srmw"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    // Model of the file for byte-exactness.
    let mut model = vec![0u8; IMG];
    for (i, b) in model.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    write_at(&h, ino, 0, &model).await;
    let path = squeezefs::keys::inode_path(ino);
    assert_eq!(
        h.fs.router.fetch_metadata(&path).await.unwrap().file_type,
        "staged",
        "fixture must exercise the STAGED RMW path"
    );

    let pooled0 = METRICS.staged_rmw_pooled_seeds.load(Ordering::Relaxed);
    let idle0 = BUFFER_POOL.len();

    // W2 (RW4): sub-image overwrites >= 25 % of the image stay on the
    // whole-image RMW path this suite pins (smaller ones ride the staged
    // extent-record RIDER now — its economy is pinned in
    // tests/extent_overlay_tests.rs).
    const SUB: usize = 768 * 1024;
    let mut x = 0x9E37_79B9u64;
    for i in 0..OPS {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let off = (x % (IMG as u64 - SUB as u64)) & !7;
        let val = (i % 251) as u8;
        let buf = vec![val; SUB];
        write_at(&h, ino, off, &buf).await;
        model[off as usize..off as usize + SUB].copy_from_slice(&buf);
    }

    // 1) The seed path is POOLED — counted once per RMW op.
    assert_eq!(
        METRICS.staged_rmw_pooled_seeds.load(Ordering::Relaxed) - pooled0,
        OPS,
        "every staged RMW must seed through the bounded BUFFER_POOL — \
         fresh heap per op is the 21 GB/30k-op churn flood"
    );

    // 2) Recycling: the pooled backings return; the pool's idle count does
    //    not bleed with op count (transient by construction — the staged
    //    arm retains nothing: the ring is authoritative and both LRUs are
    //    removed).
    let idle1 = BUFFER_POOL.len();
    assert!(
        idle0.saturating_sub(idle1) <= 2,
        "pooled RMW images must recycle: idle {idle0} -> {idle1} after \
         {OPS} ops (a per-op bleed is the flood in pool clothes)"
    );

    // 3) Byte-exactness across the whole image after the storm.
    let got =
        h.fs.read(h.req, ino, 0, 0, IMG as u32, 0)
            .await
            .unwrap()
            .data
            .to_vec();
    assert_eq!(got.len(), IMG);
    assert_eq!(got, model, "staged RMW storm must stay byte-exact");
}
