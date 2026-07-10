//! Regression harness for the WRITTEN-DATA-READS-ZEROS family (format-agnostic,
//! shared data path — reproduces on BOTH v2 and v3): fstests `generic/091`
//! (fsx -Z O_DIRECT) and `generic/075` (buffered fsx on a multi-block file).
//! A completed write MUST stay visible to every subsequent read until it is
//! overwritten, truncated away, or punched. Two independent daemon-side bugs
//! violated that:
//!
//!   1. `extend_file_size` (fallocate mode=0 / ZERO_RANGE grow leg) gated on
//!      the DURABLE inode size — which lags the true size while staged/inline
//!      writes defer their layout+size persist — and then force-set
//!      `size = offset + length` everywhere. An INTERIOR fallocate issued
//!      while the durable size lagged SHRANK the logical size, hiding every
//!      byte beyond `offset + length`: reads returned zeros/EOF for real data
//!      (generic/091, deterministic at fsx op 9621 with `-t 512 -w 512`).
//!   2. truncate-down of a striped file pruned the block map but left the
//!      in-RAM parked `ActiveBlockBuf`s and staged `active_block:` ring
//!      entries of the pruned blocks alive. The stale overlay then (a) served
//!      single-block reads directly, and (b) became the read-modify-write base
//!      of the next partial write to that block — resurrecting pre-truncate
//!      bytes where the model expects zeros (generic/075.2). The straddling
//!      block's tail had the same stale-size trap through the pre-zero gate.
//!
//! Contract pinned here: after any size-changing op sequence, previously
//! written in-bounds data reads back byte-exact, regions exposed as holes read
//! zeros, and the logical size never regresses below live data.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::{
    storage::MetaLvStorage, MetaLvBackend, RoutedMetaBackend, VolumeBackend,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fmt {
    V2,
    V3,
}

/// 64 KiB block size so one harness covers all three layouts:
/// inline (<= 4 KiB), staged (4 KiB .. 64 KiB), striped (> 64 KiB).
const BS: u64 = 65536;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(fmt: Fmt) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "wvis_test")
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
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = match fmt {
        Fmt::V2 => {
            let ms = MetaLvStorage::open(m.path(), 128 * 1024 * 1024).unwrap();
            MetaLvBackend::format_v2_for_tests(&ms, true, true, None)
                .await
                .unwrap();
            Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V2(
                Arc::new(MetaLvBackend::new(ms)),
            )]))
        }
        Fmt::V3 => {
            ImageBuilder::new(BuilderConfig {
                node_size: DEFAULT_NODE_SIZE,
                journal_len_override: None,
                hash_seed: 0xC0FF_EE00_1234_5678,
                uuid: *b"wvis-regress-v3!",
            })
            .unwrap()
            .build(m.path(), 128 * 1024 * 1024)
            .await
            .unwrap();
            let be = KvMetaBackend::open(m.path()).await.unwrap();
            Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(be)]))
        }
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

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

/// Plain `fallocate(mode=0)`: preallocate; extends size only when
/// `offset + length` is beyond EOF. Must NEVER hide existing data.
async fn falloc(h: &H, ino: u64, off: u64, len: u64) {
    h.fs.fallocate(h.req, ino, 0, off, len, 0).await.unwrap();
}

/// Every byte in `[off, off+len)` reads back as `val`. A short (or empty)
/// reply is a failure too: the range is in-bounds live data.
async fn assert_fill(h: &H, ino: u64, off: u64, len: u64, val: u8, tag: &str) {
    let got = read_at(h, ino, off, len as u32).await;
    assert_eq!(
        got.len(),
        len as usize,
        "[{tag}] LOST WRITE: short read of live data at off {off:#x}: got {} want {len}",
        got.len()
    );
    if let Some(pos) = got.iter().position(|&b| b != val) {
        panic!(
            "[{tag}] LOST WRITE: byte at file offset {:#x} = {:#x} (expected {:#x})",
            off + pos as u64,
            got[pos],
            val
        );
    }
}

/// Every byte in `[off, off+len)` reads back zero (a hole). Short reads are
/// acceptable only if everything returned is zero.
async fn assert_hole(h: &H, ino: u64, off: u64, len: u64, tag: &str) {
    let got = read_at(h, ino, off, len as u32).await;
    if let Some(pos) = got.iter().position(|&b| b != 0) {
        panic!(
            "[{tag}] STALE RESURRECTION: byte at file offset {:#x} = {:#x} (expected 0); \
             {} of {} returned bytes non-zero",
            off + pos as u64,
            got[pos],
            got.iter().filter(|&&b| b != 0).count(),
            got.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 1. generic/091 shape: fallocate(mode=0) with a lagging durable size must not
//    shrink the logical size / hide written data. The durable inode size lags
//    because staged/inline writes defer their layout+size persist; the last
//    durably-persisted size is the truncate's. Scaled from the deterministic
//    fsx repro (ops 9603..9621): truncate down, two hole-extending writes,
//    interior fallocate, read the second write back.
// ---------------------------------------------------------------------------

async fn falloc_interior_stale_durable_size(fmt: Fmt, full: u64, tag: &str) {
    let h = make(fmt).await;
    let ino = create(&h, tag).await;

    // Grow to `full`, then truncate down: the truncate persists the small
    // durable size.
    write_at(&h, ino, 0, &vec![0x11u8; full as usize]).await;
    let small = full / 4;
    truncate_to(&h, ino, small).await;

    // Two hole-extending writes; their new size stays RAM-cached (deferred
    // layout persist) so the durable inode size still reads `small`.
    let w1_off = full / 2;
    let w1_len = full / 8;
    write_at(&h, ino, w1_off, &vec![0x22u8; w1_len as usize]).await;
    let w2_off = full - full / 8;
    let w2_len = full / 8;
    write_at(&h, ino, w2_off, &vec![0x33u8; w2_len as usize]).await;
    let true_size = w2_off + w2_len;

    // Interior preallocate: inside the true size, beyond the stale durable
    // size. POSIX: no size change, no data change.
    falloc(&h, ino, small + full / 16, full / 8).await;

    assert_eq!(
        size_of(&h, ino).await,
        true_size,
        "[{tag}] fallocate(mode=0) interior must not change the file size"
    );
    assert_fill(&h, ino, w2_off, w2_len, 0x33, tag).await;
    assert_fill(&h, ino, w1_off, w1_len, 0x22, tag).await;
    // The hole between the truncate point and the first write stays zeros.
    assert_hole(&h, ino, small, full / 16, tag).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_inline_v2() {
    falloc_interior_stale_durable_size(Fmt::V2, 4096, "inline-v2").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_inline_v3() {
    falloc_interior_stale_durable_size(Fmt::V3, 4096, "inline-v3").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_staged_v2() {
    falloc_interior_stale_durable_size(Fmt::V2, 48_000, "staged-v2").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_staged_v3() {
    falloc_interior_stale_durable_size(Fmt::V3, 48_000, "staged-v3").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_striped_v2() {
    falloc_interior_stale_durable_size(Fmt::V2, 4 * BS, "striped-v2").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_striped_v3() {
    falloc_interior_stale_durable_size(Fmt::V3, 4 * BS, "striped-v3").await;
}

// ---------------------------------------------------------------------------
// 2. generic/075.2 shape: truncate-down of a striped file must invalidate the
//    parked active-block overlays of every pruned block. A parked partial
//    buffer (RMW-seeded with pre-truncate block content) must neither serve
//    reads nor seed the next partial write after the block became a hole.
// ---------------------------------------------------------------------------

async fn truncate_drops_parked_active_blocks(fmt: Fmt) {
    let tag = format!("{fmt:?}/parked-truncate");
    let h = make(fmt).await;
    let ino = create(&h, "parked").await;

    // Striped file covering blocks 0..2 (3 * BS).
    write_at(&h, ino, 0, &vec![0x11u8; (3 * BS) as usize]).await;

    // Partial write into block 1: parks an ActiveBlockBuf seeded with the
    // 0x11 device content, overlaid with 0x22.
    write_at(&h, ino, BS + 0x1000, &vec![0x22u8; 0x800]).await;

    // Truncate into block 0: blocks 1 and 2 are gone; their overlays must die.
    truncate_to(&h, ino, BS / 2).await;

    // Hole-extend with a partial write into block 2: block 1 is now a hole.
    write_at(&h, ino, 2 * BS + 0x100, &vec![0x33u8; 0x200]).await;

    // Every pre-truncate byte of block 1 must read zeros — both the range the
    // parked buffer overlaid (0x22) and its RMW-seeded remainder (0x11).
    assert_hole(&h, ino, BS, BS, &tag).await;
    assert_fill(&h, ino, 2 * BS + 0x100, 0x200, 0x33, &tag).await;
    assert_fill(&h, ino, 0, BS / 2, 0x11, &tag).await;

    // A partial write into the holed block must RMW against zeros, not the
    // stale parked buffer.
    write_at(&h, ino, BS + 0x4000, &vec![0x44u8; 0x100]).await;
    assert_fill(&h, ino, BS + 0x4000, 0x100, 0x44, &tag).await;
    assert_hole(&h, ino, BS, 0x4000, &tag).await;
    assert_hole(&h, ino, BS + 0x4100, BS - 0x4100, &tag).await;

    // And nothing resurrects across a durable flush cycle.
    fsync(&h, ino).await;
    assert_hole(&h, ino, BS, 0x4000, &tag).await;
    assert_fill(&h, ino, BS + 0x4000, 0x100, 0x44, &tag).await;
    assert_hole(&h, ino, BS + 0x4100, BS - 0x4100, &tag).await;
    assert_fill(&h, ino, 0, BS / 2, 0x11, &tag).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_drops_parked_active_blocks_v2() {
    truncate_drops_parked_active_blocks(Fmt::V2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_drops_parked_active_blocks_v3() {
    truncate_drops_parked_active_blocks(Fmt::V3).await;
}

// ---------------------------------------------------------------------------
// 3. Straddling-block tail under a lagging durable size: a truncate whose
//    new_size sits inside a block must zero that block's surviving tail even
//    when the stale durable size claims the file is smaller than new_size
//    (the pre-zero gate must use the freshest size, not the durable one).
// ---------------------------------------------------------------------------

async fn truncate_straddle_tail_stale_durable_size(fmt: Fmt) {
    let tag = format!("{fmt:?}/straddle-stale-size");
    let h = make(fmt).await;
    let ino = create(&h, "straddle").await;

    // Grow striped, then truncate tiny: durable size = 0x800. (Complete-block
    // write-through persists size as a side effect, so the lag below needs
    // parked-partial writes only.)
    write_at(&h, ino, 0, &vec![0x11u8; (3 * BS) as usize]).await;
    truncate_to(&h, ino, 0x800).await;

    // Re-extend with PARTIAL (parked, never block-completing) writes into
    // blocks 1 and 2: the true size lives only in RAM caches; the durable
    // inode size still reads 0x800.
    write_at(&h, ino, BS + 0x300, &vec![0x22u8; 0x3d00]).await;
    write_at(&h, ino, 2 * BS + 0x100, &vec![0x22u8; 0x200]).await;

    // Truncate into block 1. Old gate compared new_size against the stale
    // durable 0x800, classified this real shrink as a grow, and skipped
    // zeroing block 1's surviving tail [new_size, 2*BS) — leaving the parked
    // buffer's stale 0x22 alive.
    let new_size = BS + 0x100;
    truncate_to(&h, ino, new_size).await;

    // Expose the tail again and verify it is a hole, not stale 0x22.
    write_at(&h, ino, 2 * BS + 0x300, &vec![0x33u8; 0x100]).await;
    assert_hole(&h, ino, new_size, 2 * BS - new_size, &tag).await;
    assert_fill(&h, ino, 2 * BS + 0x300, 0x100, 0x33, &tag).await;
    // And the durable flush cycle must not resurrect it either.
    fsync(&h, ino).await;
    assert_hole(&h, ino, new_size, 2 * BS - new_size, &tag).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_straddle_tail_stale_durable_size_v2() {
    truncate_straddle_tail_stale_durable_size(Fmt::V2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_straddle_tail_stale_durable_size_v3() {
    truncate_straddle_tail_stale_durable_size(Fmt::V3).await;
}

// ---------------------------------------------------------------------------
// 4. delete_file must remove the staged `active_block:` entries under the
//    canonical key (`active_block:{path}:block_{b}`); the malformed key it
//    used (`active_block:{path}:{b}`) left stale staged overlays alive to
//    shadow a reused inode's reads.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_file_purges_staged_active_blocks() {
    let h = make(Fmt::V3).await;
    let ino = create(&h, "del").await;

    write_at(&h, ino, 0, &vec![0x55u8; (2 * BS) as usize]).await;

    // Simulate a spilled/parked staged active block for block 1 (the
    // write-through fallback and RAM-cap spill paths create exactly this).
    let key = squeezefs::keys::active_block(ino, 1).to_string();
    assert!(
        h.fs.router
            .cache
            .nvme
            .put_active_block(&key, &vec![0x66u8; BS as usize], 1),
        "staging must admit the active block"
    );
    assert!(h.fs.router.cache.nvme.read_staged(&key).is_some());

    let mut con = h.fs.router.dlm.get_connection().await.unwrap();
    h.fs.router
        .delete_file(&squeezefs::keys::inode_path(ino).to_string(), &mut con)
        .await
        .unwrap();

    assert!(
        h.fs.router.cache.nvme.read_staged(&key).is_none(),
        "delete_file left a stale staged active_block entry alive (wrong key format?)"
    );
}
