//! Regression harness for the SPARSE-WRITE-MATERIALIZES-THE-HOLE family — the
//! generic/285 daemon OOM (~108 GB RSS on a 64 KiB write).
//!
//! xfstests generic/285 (`src/seek_sanity_test` tests 10–12, `huge_file_test`)
//! writes 64 KiB at offset 0 and 64 KiB at `filsz - 64 KiB` for filsz = 8 GiB,
//! `alloc_size << 31 + 1 MiB` (~8 TiB) and `alloc_size << 32 + 1 MiB`
//! (~16 TiB). `DataRouter::write_file`'s staged/inline→striped promotion
//! materialized the ENTIRE logical span in RAM
//! (`existing_data.resize(end_offset, 0)`) and then durably wrote every
//! zero-filled block of the hole via `durable_write_stripe_payload` — O(logical
//! size) RAM + device I/O for a small write at a large offset. The ~8 TiB
//! `Vec` zero-fill is the observed ~108 GB RSS → OOM kill → cascading QUICK
//! failures.
//!
//! The pinned contract: **file-content coverage is O(map), never O(logical
//! size)**. A small write at a huge offset must
//!   1. keep the daemon/process allocation delta bounded (peak RSS delta
//!      < 256 MiB here vs +8 GiB per 8 GiB of logical size before the fix),
//!   2. produce a sparse striped layout whose block map carries ONLY the
//!      data-bearing blocks (holes = unmapped indices),
//!   3. read back POSIX-correctly with bounded reads: data at both extents,
//!      zeros in the hole, size = end of the far write,
//!   4. never regress the logical size below a truncate-up hole
//!      (truncate 100 GiB then write 4 KiB at 0 keeps size = 100 GiB), and
//!   5. keep `copy_file_range`'s SOURCE read O(chunk) — it must never
//!      materialize the whole (sparse, huge) source file to serve a 64 KiB
//!      copy.
//!
//! SEEK_HOLE/SEEK_DATA note: SqueezeFS intentionally does NOT implement
//! FUSE_LSEEK. The daemon replies ENOSYS once and the kernel serves
//! SEEK_HOLE/SEEK_DATA from `i_size` (generic_file_llseek: SEEK_HOLE → EOF,
//! SEEK_DATA → offset, ENXIO past EOF) — O(1), POSIX-legal "default behavior"
//! which seek_sanity_test accepts. What that fallback DEPENDS on is exactly
//! what these tests pin: getattr size must be correct for sparse files and
//! serving it must not allocate. (A native lseek reporting layout-granularity
//! holes would make generic/285 FAIL: seek_sanity probes the allocation unit
//! at st_blksize granularity, finer than our 4 MiB blocks.)

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 64 KiB block size so the striped threshold (and thus the generic/285
/// promotion shape) is reachable with small writes: inline (<= 4 KiB),
/// staged (4 KiB .. 64 KiB), striped (> 64 KiB).
const BS: u64 = 65536;
/// The generic/285 huge_file_test scale stand-in: 8 GiB logical (test 10 uses
/// 8 GiB; tests 11/12 use ~8/16 TiB — same O(logical) blowup, bigger).
const HUGE: u64 = 8 * 1024 * 1024 * 1024;
/// The truncate-up shape from the task contract: 100 GiB logical hole.
const TASK_HUGE: u64 = 100 * 1024 * 1024 * 1024;
/// Peak-RSS delta bound for any single sparse-file operation.
const RSS_BOUND_KB: u64 = 256 * 1024; // 256 MiB in kB

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

/// `staging`: true = staged layout available (fstests harness shape);
/// false = cache-less format (RAM tiers + direct block I/O, beyond-inline
/// writes route striped) — both promotion branches must be sparse-bounded.
async fn make(staging: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "sparse_bound_test")
            .await
            .unwrap(),
    );
    let (s, staging_paths) = if staging {
        let s = tempdir().unwrap();
        let p = vec![s.path().to_path_buf()];
        (Some(s), p)
    } else {
        (None, Vec::new())
    };
    let cache = TieredCache::new(
        staging_paths,
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
            hash_seed: 0xC0FF_EE00_5EEC_285A,
            uuid: *b"sparse-bound-v3!",
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

/// Touch every process-wide Lazy I/O pool (BUFFER_POOL / ALIGNED_BUF_POOL /
/// crypto scratch) with a small striped write+read BEFORE measuring: those
/// pools pre-allocate `max(cores*16, 64)` block-sized buffers on first use —
/// a fixed ~1 GiB one-time cost on a 32-core box that the daemon pays at
/// mount, not an O(logical size) signal. Without the warm-up the FIRST
/// device write in the process absorbs the pool init into its RSS delta.
async fn warm_io_pools(h: &H) {
    let ino = create(h, "pool_warmup").await;
    let buf = vec![b'w'; 3 * BS as usize]; // > BS => striped, 3 blocks
    write_at(h, ino, 0, &buf).await;
    let got = read_at(h, ino, 0, 3 * BS as u32).await;
    assert_eq!(got, buf, "warm-up readback");
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
    h.fs.read(h.req, ino, 0, off, size)
        .await
        .unwrap()
        .data
        .to_vec()
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

async fn size_of(h: &H, ino: u64) -> u64 {
    h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr.size
}

/// Peak process RSS (VmHWM) in kB — the daemon-side "did we materialize the
/// hole" signal. HWM never decreases, so each test measures its own delta.
fn vm_hwm_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse()
                .expect("parse VmHWM");
        }
    }
    panic!("VmHWM not found in /proc/self/status");
}

/// Number of mapped (data-bearing) blocks in the file's layout — the O(map)
/// signal. Unmapped indices are holes.
async fn mapped_blocks(h: &H, ino: u64) -> usize {
    let meta =
        h.fs.router
            .fetch_metadata(&format!("inode_{ino}"))
            .await
            .expect("fetch_metadata");
    meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0)
}

async fn assert_zeros(h: &H, ino: u64, off: u64, len: u32, tag: &str) {
    let got = read_at(h, ino, off, len).await;
    if let Some(pos) = got.iter().position(|&b| b != 0) {
        panic!(
            "[{tag}] hole read non-zero: byte at file offset {} = {:#x}",
            off + pos as u64,
            got[pos]
        );
    }
}

/// The exact generic/285 `huge_file_test` shape (test 10, scaled block size):
/// 64 KiB of data at 0, 64 KiB of data ending at filsz = 8 GiB, HUGE hole
/// between. Must be O(map): bounded RSS, sparse block map, POSIX reads.
#[tokio::test(flavor = "multi_thread")]
async fn huge_sparse_far_write_is_bounded_and_omap() {
    let h = make(true).await;
    warm_io_pools(&h).await;
    let ino = create(&h, "seek_sanity_285").await;
    let buf = vec![b'a'; BS as usize];

    write_at(&h, ino, 0, &buf).await; // staged 64 KiB
    let hwm_before = vm_hwm_kb();
    write_at(&h, ino, HUGE - BS, &buf).await; // the promotion write
    let hwm_delta = vm_hwm_kb() - hwm_before;

    assert!(
        hwm_delta < RSS_BOUND_KB,
        "sparse far write materialized the hole: peak RSS delta {} kB >= bound {} kB \
         (O(logical size) allocation — the generic/285 OOM)",
        hwm_delta,
        RSS_BOUND_KB
    );

    assert_eq!(
        size_of(&h, ino).await,
        HUGE,
        "size must be end of far write"
    );

    // O(map): only the two data-bearing 64 KiB blocks may be mapped — the
    // ~131 070 hole blocks must NOT exist (neither in RAM nor on device).
    let mapped = mapped_blocks(&h, ino).await;
    assert!(
        mapped <= 4,
        "hole blocks were materialized into the block map: {mapped} mapped blocks \
         for 128 KiB of data in an 8 GiB file"
    );

    // POSIX-correct bounded reads: data at both extents, zeros in the hole.
    assert_eq!(
        read_at(&h, ino, 0, BS as u32).await,
        buf,
        "data at offset 0 lost"
    );
    assert_eq!(
        read_at(&h, ino, HUGE - BS, BS as u32).await,
        buf,
        "data at far offset lost"
    );
    assert_zeros(&h, ino, HUGE / 2, 4096, "mid-hole").await;
    assert_zeros(&h, ino, BS, 4096, "post-data hole").await;
    assert_zeros(&h, ino, HUGE - BS - 4096, 4096, "pre-far-data hole").await;
}

/// Unaligned far write straddling a block boundary: both partial blocks carry
/// the right bytes, the intra-block gaps read zeros, and the map stays sparse.
#[tokio::test(flavor = "multi_thread")]
async fn sparse_far_write_unaligned_straddle_correct() {
    let h = make(true).await;
    warm_io_pools(&h).await;
    let ino = create(&h, "straddle_285").await;
    let head = vec![b'h'; 8192];
    write_at(&h, ino, 0, &head).await; // staged 8 KiB

    // 64 KiB write starting 32 KiB into block 131071 — covers the tail half
    // of block 131071 and the head half of block 131072.
    let off = HUGE - BS - 32768;
    let far = vec![b'f'; BS as usize];
    let hwm_before = vm_hwm_kb();
    write_at(&h, ino, off, &far).await;
    let hwm_delta = vm_hwm_kb() - hwm_before;
    assert!(
        hwm_delta < RSS_BOUND_KB,
        "unaligned sparse far write materialized the hole: delta {hwm_delta} kB"
    );

    assert_eq!(size_of(&h, ino).await, off + BS);
    let mapped = mapped_blocks(&h, ino).await;
    assert!(mapped <= 4, "straddle write over-mapped: {mapped} blocks");

    assert_eq!(read_at(&h, ino, 0, 8192).await, head, "head data lost");
    assert_eq!(
        read_at(&h, ino, off, BS as u32).await,
        far,
        "straddling far data lost"
    );
    // Zeros: tail of block 0 beyond the 8 KiB head data.
    assert_zeros(&h, ino, 8192, 4096, "block0 tail").await;
    // Zeros: the gap inside block 131071 before the far write starts.
    assert_zeros(&h, ino, off - 4096, 4096, "straddle-block gap").await;
}

/// The task-contract shape: truncate to 100 GiB (a pure hole), then write
/// 4 KiB at offset 0. Size must STAY 100 GiB (no size regression through the
/// small write), allocation must stay bounded, reads POSIX-correct.
#[tokio::test(flavor = "multi_thread")]
async fn truncate_up_then_small_write_preserves_size_bounded() {
    let h = make(true).await;
    warm_io_pools(&h).await;
    let ino = create(&h, "trunc_100g").await;

    let hwm_before = vm_hwm_kb();
    truncate_to(&h, ino, TASK_HUGE).await;
    assert_eq!(size_of(&h, ino).await, TASK_HUGE, "truncate-up size");

    let data = vec![b'd'; 4096];
    write_at(&h, ino, 0, &data).await;
    let hwm_delta = vm_hwm_kb() - hwm_before;
    assert!(
        hwm_delta < RSS_BOUND_KB,
        "truncate-up + small write materialized the hole: delta {hwm_delta} kB"
    );

    // The write must not shrink the file below the truncate-up hole.
    assert_eq!(
        size_of(&h, ino).await,
        TASK_HUGE,
        "small write at 0 regressed the 100 GiB truncate-up size"
    );

    let mapped = mapped_blocks(&h, ino).await;
    assert!(
        mapped <= 2,
        "truncate-up hole was materialized into the map: {mapped} blocks"
    );

    assert_eq!(read_at(&h, ino, 0, 4096).await, data, "written data lost");
    assert_zeros(&h, ino, TASK_HUGE / 2, 4096, "100GiB-hole mid").await;
    assert_zeros(&h, ino, TASK_HUGE - 4096, 4096, "100GiB-hole tail").await;
}

/// copy_file_range from a huge sparse source must be O(chunk), never a
/// whole-file materialization of the source (8 GiB here; ~16 TiB in
/// generic/285's shapes). Pins the SOURCE side of the same class.
#[tokio::test(flavor = "multi_thread")]
async fn copy_file_range_source_read_is_bounded() {
    let h = make(true).await;
    warm_io_pools(&h).await;
    let src = create(&h, "cfr_sparse_src").await;
    let dst = create(&h, "cfr_dst").await;

    // Build a huge sparse STRIPED source without the write-promotion path:
    // 128 KiB of data (striped, 2 blocks), then truncate UP to 8 GiB.
    let data = vec![b's'; 2 * BS as usize];
    write_at(&h, src, 0, &data).await;
    truncate_to(&h, src, HUGE).await;
    assert_eq!(size_of(&h, src).await, HUGE);

    let hwm_before = vm_hwm_kb();
    let copied =
        h.fs.copy_file_range(h.req, src, 0, 0, dst, 0, 0, BS, 0)
            .await
            .expect("copy_file_range")
            .copied;
    let hwm_delta = vm_hwm_kb() - hwm_before;

    assert!(
        hwm_delta < RSS_BOUND_KB,
        "copy_file_range materialized the whole 8 GiB sparse source for a \
         64 KiB copy: delta {hwm_delta} kB"
    );
    assert_eq!(copied, BS, "copy length");
    assert_eq!(
        read_at(&h, dst, 0, BS as u32).await,
        &data[..BS as usize],
        "copied bytes wrong"
    );
}

/// Cache-less filesystems (format without --disk-cache-paths) promote
/// beyond-inline writes straight to striped — that branch must be equally
/// sparse-bounded.
#[tokio::test(flavor = "multi_thread")]
async fn cacheless_far_write_is_bounded_and_omap() {
    let h = make(false).await;
    warm_io_pools(&h).await;
    let ino = create(&h, "cacheless_285").await;
    let head = vec![b'i'; 2048]; // stays inline
    write_at(&h, ino, 0, &head).await;

    let far = vec![b'F'; 4096];
    let hwm_before = vm_hwm_kb();
    write_at(&h, ino, HUGE - 4096, &far).await;
    let hwm_delta = vm_hwm_kb() - hwm_before;
    assert!(
        hwm_delta < RSS_BOUND_KB,
        "cache-less sparse far write materialized the hole: delta {hwm_delta} kB"
    );

    assert_eq!(size_of(&h, ino).await, HUGE);
    let mapped = mapped_blocks(&h, ino).await;
    assert!(
        mapped <= 3,
        "cache-less far write over-mapped: {mapped} blocks"
    );

    assert_eq!(read_at(&h, ino, 0, 2048).await, head, "inline head lost");
    assert_eq!(
        read_at(&h, ino, HUGE - 4096, 4096).await,
        far,
        "far data lost"
    );
    assert_zeros(&h, ino, HUGE / 2, 4096, "cache-less hole").await;
}
