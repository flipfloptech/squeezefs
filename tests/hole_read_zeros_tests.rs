//! Regression harness for the HOLES-READ-STALE-BYTES family (format-agnostic,
//! shared data path — reproduces on BOTH v2 and v3). A POSIX hole — any region
//! never written, punched, or exposed by extending a file past its data — MUST
//! read back as zeros. SqueezeFS returned stale/prior block contents for
//! several hole-creating paths:
//!
//!   1. `fallocate(FALLOC_FL_PUNCH_HOLE)` acked the punch but never zeroed or
//!      unmapped the range — a later read returned the pre-punch bytes. This is
//!      the `mkfs.xfs` BLKDISCARD-then-trust-zeroing corruption (LTP `writev03`
//!      xfs leg → `XFS: failed to locate log tail`). See the "writev03 BROKEN"
//!      section of `.benchmarks/2026-07-09-kv-v3-gates.md`.
//!   2. `truncate`-down → re-extend of a striped file left the surviving
//!      partial block's tail mapped with stale bytes — the re-exposed region
//!      read stale instead of zeros (the residual `generic/616` mismatch left
//!      after the copy_file_range crawl fix, commit 97e2ed4).
//!
//! The unifying contract these tests pin, for PUNCH / TRUNCATE-EXTEND /
//! FALLOCATE-EXTEND / sparse-write, across inline / staged / striped layouts,
//! on BOTH v2 and v3: **an unwritten / punched / extended region reads zeros,
//! adjacent real data is preserved, and the file size follows POSIX.**

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

/// 64 KiB block size so one harness covers all three layouts:
/// inline (<= 4 KiB), staged (4 KiB .. 64 KiB), striped (> 64 KiB).
const BS: u64 = 65536;
const POISON: u8 = 0xAB;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "hole_test")
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
            uuid: *b"hole-regress-v3!",
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

async fn punch(h: &H, ino: u64, off: u64, len: u64) {
    h.fs.fallocate(
        h.req,
        ino,
        0,
        off,
        len,
        (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
    )
    .await
    .unwrap();
}

/// Plain `fallocate(mode=0)` — allocate and extend the file size to
/// `offset + length` if beyond EOF. The grown region is a hole.
async fn fallocate_extend(h: &H, ino: u64, off: u64, len: u64) {
    h.fs.fallocate(h.req, ino, 0, off, len, 0).await.unwrap();
}

/// Every byte in `[off, off+len)` reads back as zero (a hole). A short read is
/// acceptable ONLY if what is returned is all zeros — the kernel zero-fills the
/// tail below i_size — but any non-zero (stale) byte fails.
async fn assert_hole(h: &H, ino: u64, off: u64, len: u64, tag: &str) {
    let got = read_at(h, ino, off, len as u32).await;
    if let Some(pos) = got.iter().position(|&b| b != 0) {
        panic!(
            "[{tag}] HOLE READ STALE: byte at file offset {} = {:#x} (expected 0); \
             read {} of {} requested bytes, {} non-zero",
            off + pos as u64,
            got[pos],
            got.len(),
            len,
            got.iter().filter(|&&b| b != 0).count()
        );
    }
}

/// Every byte in `[off, off+len)` reads back as `val` (preserved real data).
async fn assert_fill(h: &H, ino: u64, off: u64, len: u64, val: u8, tag: &str) {
    let got = read_at(h, ino, off, len as u32).await;
    assert_eq!(
        got.len(),
        len as usize,
        "[{tag}] short read of live data at off {off}: got {} want {len}",
        got.len()
    );
    if let Some(pos) = got.iter().position(|&b| b != val) {
        panic!(
            "[{tag}] LIVE DATA CORRUPTED: byte at file offset {} = {:#x} (expected {:#x})",
            off + pos as u64,
            got[pos],
            val
        );
    }
}

/// Sizes hitting each layout at BS = 64 KiB: inline, staged, striped.
const LAYOUTS: [(usize, &str); 3] = [(2048, "inline"), (40_000, "staged"), (200_000, "striped")];

// ---------------------------------------------------------------------------
// 1. PUNCH_HOLE reads zeros (all layouts) — the writev03 / mkfs.xfs bug.
// ---------------------------------------------------------------------------

async fn punch_reads_zeros() {
    for (idx, (size, lname)) in LAYOUTS.iter().enumerate() {
        let h = make().await;
        let size = *size as u64;
        let ino = create(&h, &format!("punch{idx}")).await;
        write_at(&h, ino, 0, &vec![POISON; size as usize]).await;

        // Punch an interior sub-range, leaving a head and a tail of real data.
        let p_off = size / 4;
        let p_len = size / 2;
        punch(&h, ino, p_off, p_len).await;

        let tag = format!("{lname} punch");
        // Size is unchanged (PUNCH_HOLE implies KEEP_SIZE).
        assert_eq!(
            size_of(&h, ino).await,
            size,
            "[{tag}] size changed by punch"
        );
        // Punched interior reads zeros; head and tail keep POISON.
        assert_fill(&h, ino, 0, p_off, POISON, &tag).await;
        assert_hole(&h, ino, p_off, p_len, &tag).await;
        assert_fill(&h, ino, p_off + p_len, size - (p_off + p_len), POISON, &tag).await;
    }
}

#[tokio::test]
async fn punch_reads_zeros_v3() {
    punch_reads_zeros().await;
}

/// A block-aligned whole-block striped punch (the mkfs.xfs BLKDISCARD shape):
/// the entire middle block is punched and must read zeros; the neighbours stay.
async fn punch_whole_block_striped() {
    let h = make().await;
    let size = 4 * BS; // 4 full blocks
    let ino = create(&h, "punchwb").await;
    write_at(&h, ino, 0, &vec![POISON; size as usize]).await;

    // Punch block index 1 and 2 exactly (block-aligned).
    punch(&h, ino, BS, 2 * BS).await;

    let tag = "striped whole-block punch".to_string();
    assert_eq!(size_of(&h, ino).await, size, "[{tag}] size changed");
    assert_fill(&h, ino, 0, BS, POISON, &tag).await;
    assert_hole(&h, ino, BS, 2 * BS, &tag).await;
    assert_fill(&h, ino, 3 * BS, BS, POISON, &tag).await;
}

#[tokio::test]
async fn punch_whole_block_striped_v3() {
    punch_whole_block_striped().await;
}

// ---------------------------------------------------------------------------
// 2. truncate-down → re-extend reads zeros (all layouts) — the generic/616
//    residual: a striped file's surviving partial block kept a stale tail.
// ---------------------------------------------------------------------------

async fn truncate_extend_reads_zeros() {
    for (idx, (size, lname)) in LAYOUTS.iter().enumerate() {
        let h = make().await;
        let size = *size as u64;
        let ino = create(&h, &format!("trunc{idx}")).await;
        write_at(&h, ino, 0, &vec![POISON; size as usize]).await;

        // Shrink to a non-block-aligned midpoint, then re-extend to the
        // original size. [small, size) must now be a hole.
        let small = size / 3;
        truncate_to(&h, ino, small).await;
        truncate_to(&h, ino, size).await;

        let tag = format!("{lname} truncate-extend");
        assert_eq!(
            size_of(&h, ino).await,
            size,
            "[{tag}] size wrong after re-extend"
        );
        assert_fill(&h, ino, 0, small, POISON, &tag).await;
        assert_hole(&h, ino, small, size - small, &tag).await;
    }
}

#[tokio::test]
async fn truncate_extend_reads_zeros_v3() {
    truncate_extend_reads_zeros().await;
}

// ---------------------------------------------------------------------------
// 3. fallocate-extend (grow) reads zeros (all layouts).
// ---------------------------------------------------------------------------

async fn fallocate_extend_reads_zeros() {
    for (idx, (size, lname)) in LAYOUTS.iter().enumerate() {
        let h = make().await;
        let size = *size as u64;
        let ino = create(&h, &format!("falloc{idx}")).await;
        write_at(&h, ino, 0, &vec![POISON; size as usize]).await;

        // Extend past EOF by more than a block so the grown region spans fresh
        // (unmapped) blocks as well as the tail of the last data block.
        let extra = 100_000u64;
        fallocate_extend(&h, ino, size, extra).await;

        let tag = format!("{lname} fallocate-extend");
        assert_eq!(size_of(&h, ino).await, size + extra, "[{tag}] size wrong");
        assert_fill(&h, ino, 0, size, POISON, &tag).await;
        assert_hole(&h, ino, size, extra, &tag).await;
    }
}

#[tokio::test]
async fn fallocate_extend_reads_zeros_v3() {
    fallocate_extend_reads_zeros().await;
}

// ---------------------------------------------------------------------------
// 4. sparse write past a truncated hole (the mmap-write-then-read-other-page
//    shape): truncate to 0, write one interior region, leaving earlier blocks
//    as holes that must read zeros.
// ---------------------------------------------------------------------------

async fn sparse_write_hole_reads_zeros() {
    let h = make().await;
    let ino = create(&h, "sparse").await;
    // Seed then discard, so the freed blocks carry POISON on the device.
    write_at(&h, ino, 0, &vec![POISON; (4 * BS) as usize]).await;
    truncate_to(&h, ino, 0).await;

    // Sparse write one page inside block 2, leaving blocks 0 and 1 as holes.
    let page = 4096u64;
    let woff = 2 * BS + 8192;
    write_at(&h, ino, woff, &vec![0xCD; page as usize]).await;

    let tag = "striped sparse-write".to_string();
    assert_eq!(size_of(&h, ino).await, woff + page, "[{tag}] size wrong");
    // Earlier blocks are holes even though the same offsets once held POISON.
    assert_hole(&h, ino, 0, 2 * BS, &tag).await;
    // The written page is intact; its block's head is a hole.
    assert_hole(&h, ino, 2 * BS, 8192, &tag).await;
    assert_fill(&h, ino, woff, page, 0xCD, &tag).await;
}

#[tokio::test]
async fn sparse_write_hole_reads_zeros_v3() {
    sparse_write_hole_reads_zeros().await;
}

// ---------------------------------------------------------------------------
// 5. Punched striped blocks freed to the allocator must never read back
//    through a reused offset — the incarnation / block-reuse guarantee.
// ---------------------------------------------------------------------------

async fn punch_then_reuse_reads_zeros() {
    let h = make().await;
    let size = 4 * BS;
    let a = create(&h, "reuse_a").await;
    write_at(&h, a, 0, &vec![POISON; size as usize]).await;

    // Punch the whole middle (blocks 1 and 2), returning those offsets to the
    // allocator's free list.
    punch(&h, a, BS, 2 * BS).await;

    // A second file reuses the just-freed offsets with a distinct pattern.
    let b = create(&h, "reuse_b").await;
    write_at(&h, b, 0, &vec![0xCD; (2 * BS) as usize]).await;

    let tag = "striped punch-then-reuse".to_string();
    // File A's punched range still reads zeros, never B's 0xCD or the old POISON.
    assert_fill(&h, a, 0, BS, POISON, &tag).await;
    assert_hole(&h, a, BS, 2 * BS, &tag).await;
    assert_fill(&h, a, 3 * BS, BS, POISON, &tag).await;
    // File B is intact.
    assert_fill(&h, b, 0, 2 * BS, 0xCD, &tag).await;
}

#[tokio::test]
async fn punch_then_reuse_reads_zeros_v3() {
    punch_then_reuse_reads_zeros().await;
}

// ---------------------------------------------------------------------------
// 6. Writeback-vs-hole race (the durable `generic/616` residual). A
//    `truncate`-DOWN must remove ALL data beyond `new_size` even when the
//    daemon's view of the *current* size is STALE and SMALLER than the file's
//    true physical extent.
//
//    Root cause (confirmed by daemon tracing under `fsx -S 0 -U` == generic/616
//    on a striped/staged file): `DataRouter::truncate_layout` decided
//    grow-vs-shrink from `fetch_metadata().size`, which lags the true extent
//    when the hot layout-cache entry was evicted and the backend layout size
//    persist was deferred (staged/dirty layouts persist only on fsync; the
//    striped variant lags in a narrower window). With the stale size
//    `old < new_size < physical`, the shrink was misclassified as a GROW and
//    the code SKIPPED truncating the staged blob / pruning the striped block
//    map. The stale bytes in `[new_size, physical)` survived and resurfaced as
//    non-zero data the moment the file was re-extended over them — a durable
//    corruption (survives daemon SIGKILL + remount), not a page-cache artifact.
//
//    This test injects that exact precondition deterministically: grow the
//    file, then rewrite the hot cache entry with a stale-small `size` (the
//    state `fetch_metadata` returns after eviction + a lagging backend
//    persist), then truncate DOWN into `(stale, physical)`, re-extend, and
//    require the re-exposed region to read zeros. Format-agnostic (v2 + v3),
//    all three layouts.
// ---------------------------------------------------------------------------

async fn truncate_down_stale_size_reads_zeros(
    stale: u64,
    small_present: u64,
    mid: u64,
    big: u64,
    lname: &str,
) {
    let h = make().await;
    let ino = create(&h, "wbrace").await;

    // Grow the file to `big` with a recognizable pattern everywhere.
    write_at(&h, ino, 0, &vec![POISON; big as usize]).await;

    // Simulate the confirmed corruption precondition: the hot layout-cache
    // entry now reports a size (`stale`) SMALLER than the true physical extent
    // (`big`) while the layout (staged file_id / striped block map / inline
    // data_key) still describes all `big` bytes — exactly what
    // `fetch_metadata` returns after the fresh entry is evicted and the
    // backend layout size persist has lagged behind the last writes.
    let file_path = squeezefs::keys::inode_path(ino);
    let mut m =
        h.fs.router
            .fetch_metadata(&file_path)
            .await
            .expect("fetch meta");
    assert_eq!(m.size, big, "[v3/{lname}] setup: physical size");
    m.size = stale;
    m.cached_at = std::time::Instant::now();
    h.fs.router.metadata_cache.insert(file_path.clone(), m);

    // Truncate DOWN to `mid`, with `stale < mid < big`. The buggy path treats
    // this as a grow (mid >= stale) and never removes the bytes in [mid, big).
    truncate_to(&h, ino, mid).await;

    // Re-extend past the truncated region so [mid, big) is now a POSIX hole.
    truncate_to(&h, ino, big).await;

    let tag = format!("{lname} truncate-down-stale-size");
    assert_eq!(size_of(&h, ino).await, big, "[{tag}] size after re-extend");
    // The re-exposed hole MUST read zeros — never the stale POISON.
    assert_hole(&h, ino, mid, big - mid, &tag).await;
    // Data that survived the truncate (below `small_present`) is preserved.
    if small_present > 0 {
        assert_fill(&h, ino, 0, small_present, POISON, &tag).await;
    }

    // DURABILITY: drop the hot layout cache so the next read must resolve the
    // layout from the persisted backend (and the data from the durable staging
    // blob / striped blocks), never a RAM snapshot. This proves the truncate
    // removed the stale bytes from the DURABLE store, not just the page/RAM
    // cache — the property that made the original bug survive daemon SIGKILL +
    // remount. `truncate_layout` already drops read_lru/write_lru, so evicting
    // metadata_cache forces the whole read through the durable path.
    h.fs.router.metadata_cache.invalidate(&file_path);
    let dtag = format!("{tag} (durable re-read)");
    assert_hole(&h, ino, mid, big - mid, &dtag).await;
    if small_present > 0 {
        assert_fill(&h, ino, 0, small_present, POISON, &dtag).await;
    }
}

async fn truncate_down_stale_size_all_layouts() {
    // inline (<= 4 KiB): stale/mid/big all inline-sized.
    truncate_down_stale_size_reads_zeros(512, 1024, 2048, 3500, "inline").await;
    // staged (4 KiB .. 64 KiB): all <= BS so the file stays a single blob.
    truncate_down_stale_size_reads_zeros(8_000, 12_000, 24_000, 50_000, "staged").await;
    // striped (> 64 KiB): mid is block-aligned (2 * BS) so only whole-block
    // removal is exercised; blocks 2..=3 must be dropped by the truncate.
    truncate_down_stale_size_reads_zeros(80_000, 60_000, 2 * BS, 250_000, "striped").await;
}

#[tokio::test]
async fn truncate_down_stale_size_reads_zeros_v3() {
    truncate_down_stale_size_all_layouts().await;
}
