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
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// 64 KiB block size so one harness covers all three layouts:
/// inline (<= 4 KiB), staged (4 KiB .. 64 KiB), striped (> 64 KiB).
const BS: u64 = 65536;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    /// The data allocator handle (round-4 pins: capacity clamps drive the
    /// deterministic ENOSPC shapes).
    ba: Arc<BlockAllocator>,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make() -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("wvis_test").await.unwrap());
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
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
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
        ba,
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
        // Round 5: the discriminator travels IN the panic (a writer-task
        // death must be self-attributing on any exit path/stream).
        .unwrap_or_else(|e| {
            panic!(
                "write at off {off} (ino {ino}) failed: {e:?}\n{}",
                storm_stats_line()
            )
        });
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
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

async fn falloc_interior_stale_durable_size(full: u64, tag: &str) {
    let h = make().await;
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
async fn falloc_interior_stale_size_inline_v3() {
    falloc_interior_stale_durable_size(4096, "inline-v3").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_staged_v3() {
    falloc_interior_stale_durable_size(48_000, "staged-v3").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falloc_interior_stale_size_striped_v3() {
    falloc_interior_stale_durable_size(4 * BS, "striped-v3").await;
}

// ---------------------------------------------------------------------------
// 2. generic/075.2 shape: truncate-down of a striped file must invalidate the
//    parked active-block overlays of every pruned block. A parked partial
//    buffer (RMW-seeded with pre-truncate block content) must neither serve
//    reads nor seed the next partial write after the block became a hole.
// ---------------------------------------------------------------------------

async fn truncate_drops_parked_active_blocks() {
    let tag = "parked-truncate".to_string();
    let h = make().await;
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
async fn truncate_drops_parked_active_blocks_v3() {
    truncate_drops_parked_active_blocks().await;
}

// ---------------------------------------------------------------------------
// 3. Straddling-block tail under a lagging durable size: a truncate whose
//    new_size sits inside a block must zero that block's surviving tail even
//    when the stale durable size claims the file is smaller than new_size
//    (the pre-zero gate must use the freshest size, not the durable one).
// ---------------------------------------------------------------------------

async fn truncate_straddle_tail_stale_durable_size() {
    let tag = "straddle-stale-size".to_string();
    let h = make().await;
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
async fn truncate_straddle_tail_stale_durable_size_v3() {
    truncate_straddle_tail_stale_durable_size().await;
}

// ---------------------------------------------------------------------------
// 4. copy_file_range must see parked active-block overlays on BOTH sides:
//    the source read is router-level (block map + read tiers) and missed the
//    FUSE-layer parked buffer of a partial striped write (copying stale
//    zeros/pre-write bytes — the residual generic/075.2 lost-copy), and the
//    striped destination write bypassed the parked overlay so the copied
//    bytes were shadowed on the next read/flush.
// ---------------------------------------------------------------------------

async fn cfr(h: &H, ino: u64, off_in: u64, ino_out: u64, off_out: u64, len: u64) -> u64 {
    h.fs.copy_file_range(h.req, ino, 0, off_in, ino_out, 0, off_out, len, 0)
        .await
        .unwrap()
        .copied
}

async fn copy_file_range_sees_parked_overlays() {
    let tag = "cfr-parked".to_string();
    let h = make().await;
    let ino = create(&h, "cfrsrc").await;

    // Striped file covering blocks 0..2.
    write_at(&h, ino, 0, &vec![0x11u8; (3 * BS) as usize]).await;

    // Parked partial writes: 0x77 in block 0 (the copy SOURCE), 0x22 in
    // block 1 (a parked overlay the copy DESTINATION lands next to).
    write_at(&h, ino, 0x5000, &vec![0x77u8; 0x800]).await;
    write_at(&h, ino, BS + 0x1000, &vec![0x22u8; 0x800]).await;

    // Same-file copy: parked-source bytes into the parked-dest block.
    let copied = cfr(&h, ino, 0x5000, ino, BS + 0x1400, 0x400).await;
    assert_eq!(copied, 0x400, "[{tag}] short copy");

    // The copied range must carry the parked 0x77 bytes...
    assert_fill(&h, ino, BS + 0x1400, 0x400, 0x77, &tag).await;
    // ...the parked destination neighbors survive...
    assert_fill(&h, ino, BS + 0x1000, 0x400, 0x22, &tag).await;
    // ...and the source itself is untouched.
    assert_fill(&h, ino, 0x5000, 0x800, 0x77, &tag).await;
    assert_fill(&h, ino, 0, 0x5000, 0x11, &tag).await;

    // Nothing degrades across the durable flush cycle.
    fsync(&h, ino).await;
    assert_fill(&h, ino, BS + 0x1400, 0x400, 0x77, &tag).await;
    assert_fill(&h, ino, BS + 0x1000, 0x400, 0x22, &tag).await;
    assert_fill(&h, ino, BS + 0x1800, 0x800, 0x11, &tag).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_file_range_sees_parked_overlays_v3() {
    copy_file_range_sees_parked_overlays().await;
}

// ---------------------------------------------------------------------------
// 5. Multi-block zero-copy reads must WRITE every byte of the caller's dest
//    buffer. The FUSE-over-io_uring payload buffer is reused across requests:
//    a dest region left untouched — a hole block, or the tail past a short
//    tier copy — replays the PREVIOUS reply's bytes to the kernel (transient
//    stale read; the on-disk content stays correct, which is exactly the
//    generic/075.2 empty-good/bad-diff failure shape).
// ---------------------------------------------------------------------------

async fn multiblock_read_zeroes_hole_into_reused_dest() {
    let tag = "multiblock-hole-dest".to_string();
    let h = make().await;
    let ino = create(&h, "mbhole").await;
    let file_path = format!("inode_{ino}");

    // Striped file, then punch block 1 whole: a REAL unmapped hole block
    // (a promotion-time zero block would be mapped and mask the bug).
    write_at(&h, ino, 0, &vec![0x11u8; (2 * BS) as usize]).await;
    write_at(&h, ino, 2 * BS, &vec![0x33u8; BS as usize]).await;
    h.fs.fallocate(
        h.req,
        ino,
        0,
        BS,
        BS,
        (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
    )
    .await
    .unwrap();
    fsync(&h, ino).await;

    // Simulate the reused uring payload buffer: poisoned with a previous
    // reply's bytes.
    let read_off = BS / 2;
    let read_len = (2 * BS) as usize; // spans blocks 0(tail), 1(hole), 2(head)
    let mut dest = vec![0xAAu8; read_len];
    let (data, _backing) = h
        .fs
        .router
        .read_file_range_zero_copy(
            &file_path,
            read_off,
            read_len as u32,
            // SAFETY: `dest` outlives the read and the window is
            // exclusively this test's (the FUSE-4e dest contract).
            Some(unsafe { squeezefs::routing::ReadDest::new(dest.as_mut_ptr() as u64, read_len) }),
            squeezefs::routing::ReadClassHint::default(),
        )
        .await
        .unwrap();
    assert_eq!(data.len(), read_len, "[{tag}] short multi-block read");

    let expect_at = |off: u64| -> u8 {
        if off < BS {
            0x11
        } else if off < 2 * BS {
            0 // hole
        } else {
            0x33
        }
    };
    for (i, &got) in dest.iter().enumerate() {
        let off = read_off + i as u64;
        let want = expect_at(off);
        assert_eq!(
            got, want,
            "[{tag}] dest byte at file offset {off:#x} = {got:#x}, want {want:#x} \
             (0xAA = recycled previous-reply bytes leaked through)"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiblock_read_zeroes_hole_into_reused_dest_v3() {
    multiblock_read_zeroes_hole_into_reused_dest().await;
}

// ---------------------------------------------------------------------------
// 6. Layout-prune epoch: a delayed block-map merge (writeback worker / batch
//    flush) whose content was captured BEFORE a truncate/punch pruned the map
//    must be refused — merging it would re-insert the pruned block with
//    pre-prune content (the generic/075.2 flush-vs-truncate resurrection
//    race, observed live as a post-truncate `Merge` re-adding the dead
//    block). Pins the primitive: prune ops bump the epoch; an epoch-guarded
//    merge with a stale capture returns None and applies nothing.
// ---------------------------------------------------------------------------

async fn prune_epoch_refuses_stale_delayed_merge() {
    let tag = "prune-epoch".to_string();
    let h = make().await;
    let ino = create(&h, "epoch").await;
    let file_path = format!("inode_{ino}");

    // Striped file: blocks 0..2 mapped.
    write_at(&h, ino, 0, &vec![0x11u8; (3 * BS) as usize]).await;
    fsync(&h, ino).await;
    let token = h.fs.dlm().get_fencing_token_ino(ino);

    // A delayed flush captures its content (and the epoch) here...
    let captured = squeezefs::routing::layout_prune_epoch(ino);

    // ...then a truncate prunes blocks 1..2 (bumps the epoch)...
    truncate_to(&h, ino, BS / 2).await;
    assert_ne!(
        squeezefs::routing::layout_prune_epoch(ino),
        captured,
        "[{tag}] truncate must bump the layout-prune epoch"
    );

    // ...and the delayed merge must now be refused wholesale.
    let refused =
        h.fs.router
            .merge_block_mappings_if_epoch(
                ino,
                squeezefs::routing::BlockMapOp::Merge(&[(2u32, "999999".to_string())]),
                0,
                squeezefs::routing::LayoutFlip::ToStripedKeepStagedIdentity,
                token,
                Some(captured),
            )
            .await
            .unwrap();
    assert!(
        refused.is_none(),
        "[{tag}] stale-epoch merge must be refused, got {refused:?}"
    );

    // The pruned region stays a hole (re-expose it first).
    write_at(&h, ino, 2 * BS + BS / 2, &[0x33u8; 16]).await;
    assert_hole(&h, ino, BS, BS, &tag).await;

    // A fresh capture merges normally (sanity: the guard refuses only
    // genuinely stale captures).
    let fresh = squeezefs::routing::layout_prune_epoch(ino);
    let applied =
        h.fs.router
            .merge_block_mappings_if_epoch(
                ino,
                squeezefs::routing::BlockMapOp::Merge(&[]),
                0,
                squeezefs::routing::LayoutFlip::KeepLayout,
                token,
                Some(fresh),
            )
            .await
            .unwrap();
    assert!(applied.is_some(), "[{tag}] fresh-epoch merge must apply");
    let _ = file_path;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_epoch_refuses_stale_delayed_merge_v3() {
    prune_epoch_refuses_stale_delayed_merge().await;
}

// ---------------------------------------------------------------------------
// 7. delete_file must remove the staged `active_block:` entries under the
//    canonical key (`active_block:{path}:block_{b}`); the malformed key it
//    used (`active_block:{path}:{b}`) left stale staged overlays alive to
//    shadow a reused inode's reads.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_file_purges_staged_active_blocks() {
    let h = make().await;
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

    h.fs.router
        .delete_file(&squeezefs::keys::inode_path(ino).to_string())
        .await
        .unwrap();

    assert!(
        h.fs.router.cache.nvme.read_staged(&key).is_none(),
        "delete_file left a stale staged active_block entry alive (wrong key format?)"
    );
}

// ---------------------------------------------------------------------------
// 8. PR M5 (design-metadata-throughput §5.1 D1.d): clean-handle
//    FLUSH/RELEASE fast path. A handle nothing ever dirtied (no write /
//    truncate / fallocate / copy_file_range) must FLUSH and RELEASE without
//    lease acquisition or buffer scans — and, load-bearing for the write
//    path: a handle that WAS dirtied must keep today's full FLUSH/RELEASE
//    behavior bit-for-bit (data visible after flush, durable after fsync).
//    Observability contract: the fast path (and only the fast path) counts
//    `fuse_flush_clean_fastpath` / `fuse_release_clean_fastpath`.
//
//    NOTE: the counter-delta assertions read process-global METRICS and are
//    exact under the sanctioned gate (`--test-threads=1`, per AGENTS.md);
//    parallel in-binary runs can interleave other tests' flushes into the
//    deltas (same posture as the writeback_retry_exhaustions tests).
// ---------------------------------------------------------------------------

fn flush_fast_count() -> u64 {
    squeezefs::fuse_client::METRICS
        .fuse_flush_clean_fastpath
        .load(std::sync::atomic::Ordering::Relaxed)
}

fn release_fast_count() -> u64 {
    squeezefs::fuse_client::METRICS
        .fuse_release_clean_fastpath
        .load(std::sync::atomic::Ordering::Relaxed)
}

async fn flush(h: &H, ino: u64) {
    h.fs.flush(h.req, ino, ino, 0).await.unwrap();
}

/// D2.a under writeback cache (kernel-verified on 7.1.3: uapi fuse.h —
/// "FOPEN_NOFLUSH: don't flush data cache on close (unless
/// FUSE_WRITEBACK_CACHE)", and the smoke probe measured FLUSH still
/// arriving 1/close with the bit advertised): the kernel's honored
/// elision switch under wb-cache is the ENOSYS reply, which latches
/// `fc->no_flush` — the kernel keeps flushing dirty pages at close
/// (`write_inode_now` runs BEFORE the latch check) but stops sending the
/// FLUSH round trip. A CLEAN handle's flush must therefore reply ENOSYS
/// (and count the fast path).
async fn flush_clean(h: &H, ino: u64) {
    let err =
        h.fs.flush(h.req, ino, ino, 0)
            .await
            .expect_err("clean-handle FLUSH must reply ENOSYS (latches kernel no_flush)");
    assert_eq!(libc::c_int::from(err), -libc::ENOSYS);
}

async fn release(h: &H, ino: u64) {
    h.fs.release(h.req, ino, ino, 0, 0, false).await.unwrap();
}

async fn open(h: &H, ino: u64) {
    let _ =
        h.fs.open(h.req, ino, libc::O_RDONLY as u32, 0)
            .await
            .unwrap();
}

/// Never-dirtied handle: FLUSH and RELEASE take the fast path (counters
/// move), and the release still tears down open-count bookkeeping so
/// inode reclaim keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_handle_flush_release_take_fast_path() {
    let h = make().await;
    // create() leaves the handle open and CLEAN (empty file, no writes).
    let ino = create(&h, "clean_fast").await;

    let f0 = flush_fast_count();
    flush_clean(&h, ino).await;
    assert_eq!(
        flush_fast_count() - f0,
        1,
        "FLUSH on a never-dirtied handle must take the D1.d fast path"
    );

    let r0 = release_fast_count();
    release(&h, ino).await;
    assert_eq!(
        release_fast_count() - r0,
        1,
        "RELEASE on a never-dirtied handle must take the D1.d fast path"
    );
    assert!(
        !h.fs.is_open(ino),
        "fast-path release must still decrement the open count (reclaim lifecycle)"
    );

    // Reopen for read only: still clean, still fast.
    open(&h, ino).await;
    let f1 = flush_fast_count();
    let r1 = release_fast_count();
    flush_clean(&h, ino).await;
    release(&h, ino).await;
    assert_eq!(flush_fast_count() - f1, 1, "read-only reopen stays clean");
    assert_eq!(release_fast_count() - r1, 1, "read-only reopen stays clean");
}

/// Dirty-handle FLUSH/RELEASE behavior is UNCHANGED (the D2.a semantics
/// review's pin): full path runs (no fast-path counts), written data is
/// visible after flush and survives fsync.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dirty_handle_flush_release_behavior_unchanged() {
    let h = make().await;
    let ino = create(&h, "dirty_full").await;

    write_at(&h, ino, 0, &vec![0xAAu8; (BS + 17) as usize]).await;

    let f0 = flush_fast_count();
    flush(&h, ino).await;
    assert_eq!(
        flush_fast_count() - f0,
        0,
        "FLUSH on a dirtied handle must run the FULL flush path, never the fast path"
    );
    assert_fill(&h, ino, 0, BS + 17, 0xAA, "dirty-flush").await;

    let r0 = release_fast_count();
    release(&h, ino).await;
    assert_eq!(
        release_fast_count() - r0,
        0,
        "RELEASE on a dirtied handle must run the FULL release path (background flush spawn)"
    );

    // Durability unaffected: reopen, fsync, read back.
    open(&h, ino).await;
    fsync(&h, ino).await;
    assert_fill(&h, ino, 0, BS + 17, 0xAA, "dirty-fsync").await;
    release(&h, ino).await;
}

/// Every data-mutating op class must set the dirty bit: truncate (setattr
/// size), fallocate, copy_file_range destination.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncate_fallocate_copy_range_mark_handle_dirty() {
    let h = make().await;

    // setattr(size) — truncate dirties.
    let a = create(&h, "dirty_trunc").await;
    truncate_to(&h, a, 4096).await;
    let f0 = flush_fast_count();
    flush(&h, a).await;
    assert_eq!(flush_fast_count() - f0, 0, "truncate must dirty the handle");

    // fallocate dirties.
    let b = create(&h, "dirty_falloc").await;
    falloc(&h, b, 0, 8192).await;
    let f1 = flush_fast_count();
    flush(&h, b).await;
    assert_eq!(
        flush_fast_count() - f1,
        0,
        "fallocate must dirty the handle"
    );

    // copy_file_range dirties the DESTINATION only.
    let src = create(&h, "cfr_src").await;
    write_at(&h, src, 0, &[0x5Au8; 4096]).await;
    flush(&h, src).await;
    let dst = create(&h, "cfr_dst").await;
    let copied =
        h.fs.copy_file_range(h.req, src, src, 0, dst, dst, 0, 4096, 0)
            .await
            .unwrap();
    assert_eq!(copied.copied, 4096, "copy_file_range short copy");
    let f2 = flush_fast_count();
    flush(&h, dst).await;
    assert_eq!(
        flush_fast_count() - f2,
        0,
        "copy_file_range must dirty the destination handle"
    );
}

/// The dirty bit is per open-generation: after the last close of a
/// dirtied inode, a fresh reopen that never writes is clean again (its
/// unflushed state was already handed to the background path at the dirty
/// close — nothing new to flush).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_after_dirty_close_is_clean_again() {
    let h = make().await;
    let ino = create(&h, "reopen_clean").await;
    write_at(&h, ino, 0, &[0x11u8; 1024]).await;
    release(&h, ino).await; // dirty close: full path, schedules bg flush

    open(&h, ino).await; // open count 0 -> 1 resets the dirty bit
    let f0 = flush_fast_count();
    let r0 = release_fast_count();
    flush_clean(&h, ino).await;
    release(&h, ino).await;
    assert_eq!(
        flush_fast_count() - f0,
        1,
        "clean reopen after dirty close must be fast again"
    );
    assert_eq!(
        release_fast_count() - r0,
        1,
        "clean reopen after dirty close must be fast again"
    );
    // And the data written before the dirty close is still there.
    assert_fill(&h, ino, 0, 1024, 0x11, "reopen-clean").await;
}

/// While ANY handle of the inode is dirty, a second handle's close must
/// NOT take the fast path (the dirty bit is per-inode: conservative for
/// multi-handle opens, exact for the create-storm shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_clean_handle_of_dirty_inode_stays_on_full_path() {
    let h = make().await;
    let ino = create(&h, "two_handles").await; // handle 1 (clean)
    open(&h, ino).await; // handle 2 (clean)
    write_at(&h, ino, 0, &[0x22u8; 512]).await; // dirties the inode

    let f0 = flush_fast_count();
    let r0 = release_fast_count();
    flush(&h, ino).await; // "handle 2" flush
    release(&h, ino).await; // "handle 2" close — inode still open once
    assert_eq!(
        flush_fast_count() - f0,
        0,
        "dirty inode: no flush may take the fast path while the dirty state is live"
    );
    assert_eq!(
        release_fast_count() - r0,
        0,
        "dirty inode: no release may take the fast path while the dirty state is live"
    );
    assert!(h.fs.is_open(ino), "one handle must remain open");
    release(&h, ino).await;
}

/// fstests generic/209 repro-port (VL10 release gate,
/// `aio-dio-invalidate-readahead`): read-after-ACKED-write freshness
/// under a sequential overwrite storm crossing the staged→striped
/// promotion boundary. The writer overwrites the whole file page by
/// page with the pass number; a concurrent reader may read ANY range
/// whose writes already COMPLETED and must never see the previous
/// pass's byte. The mount-level failure ("reader found old byte") sat
/// deterministically just past the first block boundary — 3/3 with and
/// without the kernel writeback cache, so the staleness is served by
/// the daemon, not kernel readahead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_overwrites_never_serve_the_previous_pass() {
    let h = Arc::new(make().await);
    let ino = create(&h, "gen209").await;

    const PAGE: u64 = 4096;
    const FILE: u64 = 2 * BS; // two blocks — the promotion boundary lives inside
    const PASSES: u8 = 12;

    // Pass 0 lays the file down NON-ZERO (value 100): a served stale
    // pass-0 byte reads 100, a zero-filled overlay complement reads 0 —
    // the two failure classes disambiguate on sight.
    for off in (0..FILE).step_by(PAGE as usize) {
        write_at(&h, ino, off, &[100u8; PAGE as usize]).await;
    }

    // (pass, completed_end): every byte in [0, completed_end) carries
    // `pass`; everything at/after carries `pass - 1`.
    let (tx, rx) = tokio::sync::watch::channel((0u8, FILE));

    let writer = {
        let h = h.clone();
        tokio::spawn(async move {
            for pass in 1..=PASSES {
                let _ = tx.send((pass, 0));
                let buf = vec![pass; PAGE as usize];
                for off in (0..FILE).step_by(PAGE as usize) {
                    write_at(&h, ino, off, &buf).await;
                    let _ = tx.send((pass, off + PAGE));
                    if (off / PAGE).is_multiple_of(8) {
                        tokio::task::yield_now().await;
                    }
                }
            }
            drop(tx);
        })
    };

    let reader = {
        let h = h.clone();
        let mut rx = rx.clone();
        tokio::spawn(async move {
            loop {
                let (pass, end) = *rx.borrow_and_update();
                if pass >= 1 && end >= PAGE {
                    // Read a window straddling the block boundary when
                    // covered, else the freshest completed page.
                    let want = if end > BS + PAGE { BS - PAGE } else { 0 };
                    let off = want.min(end - PAGE);
                    let len = (end - off).min(4 * PAGE) as u32;
                    // Round-6 forensics: snapshot the read-arm counters so
                    // a failure names the SERVE ARM the failing read took
                    // (deltas travel in the panic — the round-5 mandate).
                    let probe0 = read_probe_snapshot();
                    let got = read_at(&h, ino, off, len).await;
                    // Every byte in a COMPLETED range must be >= pass
                    // (a racing next-pass byte is legal; pass-1 is not).
                    let (cur_pass, cur_end) = *rx.borrow();
                    if cur_pass == pass {
                        for (i, &b) in got.iter().enumerate() {
                            let pos = off + i as u64;
                            if pos < cur_end.min(end) && b < pass {
                                // The generic/209 contract: a byte whose
                                // write COMPLETED before this read began
                                // must never read the previous pass. (The
                                // convicted windows, forensically
                                // attributed during VL10: the write path's
                                // remove->mutate->reinsert overlay checkout,
                                // and the multi-block/fallthrough reads
                                // that never composed RAM overlays over the
                                // base tiers — both transient, self-healing
                                // serves of one-write-behind acked bytes.)
                                //
                                // Round-6 forensics (the 2026-08-19 flake
                                // conviction): discriminate a TRANSIENT
                                // race window (the next read heals) from a
                                // STICKY poisoned serve, and carry the
                                // write-vehicle counters inline — the
                                // round-5 mandate (panic-inline, immune to
                                // exit-path stream loss).
                                let mut heal = String::new();
                                for attempt in 0..3u32 {
                                    tokio::task::yield_now().await;
                                    let again = read_at(&h, ino, off, len).await;
                                    let cur = again.get(i).copied();
                                    heal.push_str(&format!(" reread{attempt}={cur:?}"));
                                    if cur.is_some_and(|c| c >= pass) {
                                        break;
                                    }
                                }
                                panic!(
                                    "READER FOUND OLD BYTE {b} at pos {pos} \
                                     (pass {pass}, completed_end {end}) — \
                                     generic/209;{heal}\nREAD-ARM {}\n{}",
                                    read_probe_delta(&probe0),
                                    storm_stats_line()
                                );
                            }
                        }
                    }
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
    };

    writer.await.unwrap();
    reader.await.unwrap();

    // Post-storm: the final pass is fully durable-visible everywhere.
    let final_bytes = read_at(&h, ino, 0, FILE as u32).await;
    for (i, &b) in final_bytes.iter().enumerate() {
        assert_eq!(b, PASSES, "post-storm byte {i} must carry the last pass");
    }
}

/// Deterministic discriminator for the generic/209 staleness: does a
/// SERIALIZED sequential overwrite revert its neighbor (a write-path
/// seed-source bug), or is the stale byte only served transiently
/// (a read-path race)?
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serialized_overwrite_never_reverts_neighbors() {
    let h = make().await;
    let ino = create(&h, "gen209det").await;
    const PAGE: usize = 4096;

    // Pass 0: zeros across 6 pages (staged regime).
    for p in 0..6u64 {
        write_at(&h, ino, p * PAGE as u64, &[0u8; PAGE]).await;
    }
    // Pass 1: page-by-page value 1, verifying after EVERY write that all
    // previously written pass-1 pages still read 1.
    for p in 0..6u64 {
        write_at(&h, ino, p * PAGE as u64, &[1u8; PAGE]).await;
        let got = read_at(&h, ino, 0, ((p + 1) * PAGE as u64) as u32).await;
        for (i, &b) in got.iter().enumerate() {
            assert_eq!(
                b, 1,
                "byte at {i} reverted to the previous pass after the \
                 SERIALIZED write of page {p} — write-path seed source bug"
            );
        }
    }
}

/// Deterministic repro of the generic/209 flake's convicted window
/// (2026-08-19 conviction — the ~5/10 storm flake, "OLD BYTE at pos 0,
/// completed_end 4096"): a W1 in-place patch is the ONE content
/// mutation that moves neither the block-map binding nor any key the
/// single-flight registry is indexed by, and the PERF-11 deferred tier
/// publish OWNS the flight guard — so the registry entry (and the
/// flight's STORED `FillResult`) outlives the patch's tier purge. A
/// read that begins strictly AFTER the patch's ACK misses every purged
/// tier, joins the held flight, and is served the pre-patch snapshot
/// with the fill-TIME `serve_valid=true` verdict — which the binding
/// recheck cannot catch (the binding is unchanged by construction).
///
/// Protocol (every step's engagement asserted):
///   1. striped 2-block file, settled (fsync);
///   2. read block 0 — fill #1 records the ghost (first touch);
///   3. patch #1 (0x22 at page 0) — purges tiers;
///   4. hold the tier publish open (the §5.2 named seam — the PERF-11
///      guard rides it, so the NEXT fill's registry entry stays live);
///   5. read block 0 — fill #2 (ghost hit ⇒ deferred publish ⇒ HELD
///      flight) serves the post-patch-#1 image;
///   6. patch #2 (0x33 at page 0) — ACK;
///   7. read block 0 AFTER the ACK: every byte of page 0 must read
///      0x33. Pre-fix this read is served fill #2's stored snapshot
///      (0x22) with a stale-but-true verdict — the storm's OLD BYTE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patched_block_never_serves_a_held_flight_snapshot() {
    use squeezefs::routing::TEST_TIER_PUBLISH_DELAY_MS;
    use std::sync::atomic::Ordering as O;

    // Seam guard: restore both seams on every exit path (panic included).
    struct SeamReset;
    impl Drop for SeamReset {
        fn drop(&mut self) {
            TEST_TIER_PUBLISH_DELAY_MS.store(0, O::Relaxed);
            squeezefs::device_overlay::clear_device_overlay_for_tests();
        }
    }
    let _reset = SeamReset;
    // The convicted window needs no device overlay (the A/B matrix:
    // DEVICE_OVERLAY=0 still fails 9/100) — pin it off so the repro's
    // write vehicles are exactly {W1 patch, write-through}.
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);

    let h = make().await;
    let ino = create(&h, "flight209").await;
    const PAGE: usize = 4096;

    // 1. One whole-file write → striped write-through both blocks; fsync
    //    settles writeback so both mappings are whole-block and no
    //    RAM/staged overlay is live (the patch predicates' premise).
    write_at(&h, ino, 0, &vec![0x11u8; (2 * BS) as usize]).await;
    fsync(&h, ino).await;

    // 2. Fill #1: first-touch miss records the R1b ghost for block 0's
    //    key (second-touch admission is the shipped default — the next
    //    miss of the SAME key takes the deferred-publish arm).
    let got = read_at(&h, ino, 0, BS as u32).await;
    assert!(
        got.iter().all(|&b| b == 0x11),
        "settled base must read 0x11"
    );

    // Setup helper: a page-0 write that must ride the W1 patch (the
    // window is only reachable through the in-place vehicle). Engagement
    // is checked on the process-global ledger, so under DEFAULT-
    // PARALLELISM a foreign suite's load can classify one attempt away
    // from the ladder (stale attr-arm heuristics, cache churn) — settle
    // (fsync drains any accumulation vehicle the attempt landed on;
    // content is value-idempotent) and retry, loud on exhaustion.
    // Single-threaded this engages on the first try (0/300 retries
    // across the conviction brackets).
    async fn patch_engaged(h: &H, ino: u64, val: u8) {
        use std::sync::atomic::Ordering as O;
        let m = &squeezefs::fuse_client::METRICS;
        for _ in 0..25 {
            let before = m.patch_writes.load(O::Relaxed);
            write_at(h, ino, 0, &[val; 4096]).await;
            if m.patch_writes.load(O::Relaxed) > before {
                return;
            }
            fsync(h, ino).await;
            tokio::task::yield_now().await;
        }
        panic!(
            "W1 patch never engaged for the held-flight repro setup\n{}",
            storm_stats_line()
        );
    }

    // 3. Patch #1: page 0 := 0x22 (aligned, non-adjacent, sub-cap,
    //    non-extending ⇒ the W1 in-place DMA; purges block 0's tiers).
    patch_engaged(&h, ino, 0x22).await;

    // 4+5. Hold the tier publish open, then fill #2: the ghost hit
    //    routes this fill's publish through the deferred arm, whose
    //    closure owns the single-flight guard — the registry entry now
    //    stays live for the whole held window.
    TEST_TIER_PUBLISH_DELAY_MS.store(2_000, O::Relaxed);
    let got = read_at(&h, ino, 0, BS as u32).await;
    assert!(
        got[..PAGE].iter().all(|&b| b == 0x22),
        "fill #2 must serve the post-patch-#1 page"
    );

    // 6. Patch #2: page 0 := 0x33, ACKed. The purge sweeps every tier —
    //    but the held flight's stored FillResult is not a tier.
    patch_engaged(&h, ino, 0x33).await;

    // 7. The contract: this read BEGINS after patch #2's ACK, so every
    //    byte of page 0 must read 0x33 — the stored flight snapshot
    //    (0x22, verdict computed before the patch) is not servable.
    let got = read_at(&h, ino, 0, BS as u32).await;
    if let Some(pos) = got[..PAGE].iter().position(|&b| b != 0x33) {
        panic!(
            "READER FOUND OLD BYTE {:#x} at pos {pos} after the patch's ACK \
             — the held single-flight snapshot served a pre-patch image \
             (generic/209 convicted window)\n{}",
            got[pos],
            storm_stats_line()
        );
    }
    assert!(
        got[PAGE..].iter().all(|&b| b == 0x11),
        "bytes beyond the patched page stay at the base pattern"
    );
}

/// fstests generic/795 repro-port (VL10 release gate, the cat-race
/// face): one stable source pattern; a writer loop that unlinks,
/// re-creates, and SEQUENTIALLY rewrites the copy (the `cat orig >
/// fsv` shape — the file walks inline → staged → striped as it grows);
/// concurrent readers compare the copy against the pattern. A reader
/// may legally see a SHORT file (EOF — the copy is mid-flight), but
/// every byte it DOES get inside the visible size must equal the
/// pattern: 795's readers filter EOF and still caught wrong bytes
/// (zeros / foreign) at stable offsets, self-healing on remount —
/// a daemon-side transient wrong serve.
/// Per-read serve-arm probe (round-6 forensics): the counters whose DELTA
/// across one read names which arm served it.
const READ_PROBE_NAMES: [&str; 12] = [
    "overlay_read_serves",
    "overlay_read_gap_serves",
    "overlay_read_drains",
    "overlay_window_escalations",
    "stale_binding_rebinds",
    "singleflight_waiter_result_serves",
    "cache_hits",
    "cache_misses",
    "ranged_reads",
    "hot_block_hits",
    "read_lane_serves",
    "overwrite_seed_materialized",
];

fn read_probe_snapshot() -> [u64; 12] {
    use std::sync::atomic::Ordering as O;
    let m = &squeezefs::fuse_client::METRICS;
    [
        m.overlay_read_serves.load(O::Relaxed),
        m.overlay_read_gap_serves.load(O::Relaxed),
        m.overlay_read_drains.load(O::Relaxed),
        m.overlay_window_escalations.load(O::Relaxed),
        m.stale_binding_rebinds.load(O::Relaxed),
        m.singleflight_waiter_result_serves.load(O::Relaxed),
        m.cache_hits.load(O::Relaxed),
        m.cache_misses.load(O::Relaxed),
        m.ranged_reads.load(O::Relaxed),
        m.hot_block_hits.load(O::Relaxed),
        m.read_lane_serves.load(O::Relaxed),
        m.overwrite_seed_materialized.load(O::Relaxed),
    ]
}

fn read_probe_delta(before: &[u64; 12]) -> String {
    let now = read_probe_snapshot();
    READ_PROBE_NAMES
        .iter()
        .zip(now.iter().zip(before.iter()))
        .map(|(n, (a, b))| format!("{n}={}", a - b))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One greppable line of the discriminator counters ("STORM-STATS …").
/// Round-5 mandate: the round-4 falsification tape STILL carried no
/// gauge line (whatever the exit path did to the drop guard's stream),
/// so the counters now travel INLINE in every panic message — immune to
/// stream/exit-path issues — and the drop guard prints the same line as
/// the belt.
fn storm_stats_line() -> String {
    use std::sync::atomic::Ordering as O;
    let m = &squeezefs::fuse_client::METRICS;
    format!(
        "STORM-STATS overlay_window_escalations={} stale_binding_escalations={} \
         overlay_read_drains={} overlay_installs={} overlay_publishes={} \
         overlay_epoch_feeds={} overlay_feed_fallbacks={} overlay_fence_drops={} \
         overlay_claim_conflicts={} overlay_ack_early_lost={} \
         overlay_enospc_declines={} invariant_tripwires={} \
         write_through_fallbacks={} writeback_orphan_discards={} \
         writeback_stale_token_retries={} stale_binding_rebinds={} \
         patch_writes={} patch_ineligible_shared={} patch_ineligible_overlay={} \
         extent_parks={} write_block_revisits={} write_through_blocks={} \
         hot_block_hits={} read_lane_serves={} singleflight_waiter_result_serves={} \
         read_tier_admissions={}",
        m.overlay_window_escalations.load(O::Relaxed),
        m.stale_binding_escalations.load(O::Relaxed),
        m.overlay_read_drains.load(O::Relaxed),
        m.overlay_installs.load(O::Relaxed),
        m.overlay_publishes.load(O::Relaxed),
        m.overlay_epoch_feeds.load(O::Relaxed),
        m.overlay_feed_fallbacks.load(O::Relaxed),
        m.overlay_fence_drops.load(O::Relaxed),
        m.overlay_claim_conflicts.load(O::Relaxed),
        m.overlay_ack_early_lost.load(O::Relaxed),
        m.overlay_enospc_declines.load(O::Relaxed),
        m.invariant_tripwires.load(O::Relaxed),
        m.write_through_fallbacks.load(O::Relaxed),
        m.writeback_orphan_discards.load(O::Relaxed),
        m.writeback_stale_token_retries.load(O::Relaxed),
        m.stale_binding_rebinds.load(O::Relaxed),
        m.patch_writes.load(O::Relaxed),
        m.patch_ineligible_shared.load(O::Relaxed),
        m.patch_ineligible_overlay.load(O::Relaxed),
        m.extent_parks.load(O::Relaxed),
        m.write_block_revisits.load(O::Relaxed),
        m.write_through_blocks.load(O::Relaxed),
        m.hot_block_hits.load(O::Relaxed),
        m.read_lane_serves.load(O::Relaxed),
        m.singleflight_waiter_result_serves.load(O::Relaxed),
        m.read_tier_admissions.load(O::Relaxed),
    )
}

/// Drop-guard stats dump (the belt; the panic-inline lines are the
/// braces). Drop runs on unwind, so one guard at test start covers all
/// paths that unwind the MAIN test task.
struct StormStatsDump;
impl Drop for StormStatsDump {
    fn drop(&mut self) {
        eprintln!("{}", storm_stats_line());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sequential_recopy_readers_never_see_foreign_bytes() {
    let _stats = StormStatsDump;
    let h = Arc::new(make().await);
    const LEN: usize = 10 * BS as usize; // 10 blocks: crosses all layouts
    let pattern: Arc<Vec<u8>> = Arc::new((0..LEN).map(|i| (i % 251) as u8 ^ 0x5A).collect());

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ino_cell = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // The writer's ACK watermark for the CURRENT incarnation: bytes
    // [0, watermark) have completed their write_at when read.
    let acked = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // TEST-3: the soak is bounded by COMPLETED WORK, not by a wall-clock
    // sleep. The retired `sleep(8 s)` bought whatever coverage the box
    // happened to deliver — many incarnations on an idle machine, possibly
    // a handful under `--test-threads=1` load — which is both slow and the
    // exact load-dependence the house rule forbids.
    let cycles = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Writer: rm + create + sequential 8 KiB rewrite, forever.
    let mut writer = {
        let h = h.clone();
        let pattern = pattern.clone();
        let stop = stop.clone();
        let ino_cell = ino_cell.clone();
        let acked = acked.clone();
        let cycles = cycles.clone();
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let name = "fsv";
                let _ = h.fs.unlink(h.req, 1, OsStr::new(name)).await;
                let ino = create(&h, name).await;
                acked.store(0, std::sync::atomic::Ordering::Release);
                ino_cell.store(ino, std::sync::atomic::Ordering::Release);
                for off in (0..LEN).step_by(8192) {
                    let end = (off + 8192).min(LEN);
                    write_at(&h, ino, off as u64, &pattern[off..end]).await;
                    acked.store(end as u64, std::sync::atomic::Ordering::Release);
                    if off % 65536 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                cycles.fetch_add(1, std::sync::atomic::Ordering::Release);
                tokio::task::yield_now().await;
            }
        })
    };

    // Readers: random windows of the CURRENT incarnation; short reads are
    // legal, wrong bytes never.
    let mut readers = Vec::new();
    for r in 0..4u64 {
        let h = h.clone();
        let pattern = pattern.clone();
        let stop = stop.clone();
        let ino_cell = ino_cell.clone();
        let acked = acked.clone();
        let reads = reads.clone();
        readers.push(tokio::spawn(async move {
            let mut i = 0u64;
            let mut served = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                i += 1;
                let ino = ino_cell.load(std::sync::atomic::Ordering::Acquire);
                if ino == 0 {
                    tokio::task::yield_now().await;
                    continue;
                }
                let off = ((i * 7919 + r * 13007) % LEN as u64) & !4095;
                let len = 16384u32;
                let reply = match h.fs.read(h.req, ino, 0, off, len, 0).await {
                    Ok(rep) => rep,
                    Err(_) => continue, // unlinked under us — legal
                };
                let got = reply.data.as_ref();
                // The incarnation may have moved (unlink+recreate): only
                // judge bytes when the ino is STILL current after the read.
                if ino_cell.load(std::sync::atomic::Ordering::Acquire) != ino {
                    continue;
                }
                let mut first_bad = None;
                let mut bad = 0usize;
                for (j, &b) in got.iter().enumerate() {
                    let pos = off as usize + j;
                    if pos < LEN && b != pattern[pos] {
                        if first_bad.is_none() {
                            first_bad = Some(pos);
                        }
                        bad += 1;
                    }
                }
                let watermark = acked.load(std::sync::atomic::Ordering::Acquire);
                if let Some(pos) = first_bad {
                    let j0 = pos - off as usize;
                    let dump: Vec<String> = got[j0..(j0 + 32).min(got.len())]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    let wdump: Vec<String> = pattern[pos..(pos + 32).min(LEN)]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    panic!(
                        "foreign bytes served: incarnation {ino} reply off {off} len {} — \
                         {bad} wrong bytes starting at {pos} (ack watermark {watermark}: \
                         {}); got[{j0}..]={} want={} (generic/795)\n{}",
                        got.len(),
                        if (pos as u64) < watermark {
                            "ACKED bytes lost from visibility"
                        } else {
                            "SIZE LED DATA (unacked range readable)"
                        },
                        dump.join(""),
                        wdump.join(""),
                        storm_stats_line()
                    );
                }
                served += got.len() as u64;
                reads.fetch_add(1, std::sync::atomic::Ordering::Release);
                if i.is_multiple_of(64) {
                    tokio::task::yield_now().await;
                }
            }
            served
        }));
    }

    // Work-bounded soak (TEST-3): stop once the storm has actually
    // delivered its coverage — WRITER_CYCLES full unlink→create→rewrite
    // incarnations (the layout walk inline → staged → striped, once per
    // cycle) with READER_OPS concurrent reads observed against them. The
    // deadline is a failure bound, not the schedule: a box that cannot
    // reach the coverage says so instead of silently testing less.
    const WRITER_CYCLES: u64 = 12;
    const READER_OPS: u64 = 2_000;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let (c, r) = (
            cycles.load(std::sync::atomic::Ordering::Acquire),
            reads.load(std::sync::atomic::Ordering::Acquire),
        );
        if c >= WRITER_CYCLES && r >= READER_OPS {
            break;
        }
        // WRITER-PROGRESS hard gate (round 4): a dead writer task (its
        // write_at unwrapped an error) must fail NOW with its panic, not
        // sit out the deadline as a coverage shortfall — the round-3
        // falsification tape burned 115 s waiting on a writer that died
        // at incarnation 10.
        if writer.is_finished() && c < WRITER_CYCLES {
            let res = (&mut writer).await;
            panic!(
                "writer task died/exited early at {c}/{WRITER_CYCLES} \
                 incarnations — the storm's coverage is a hard gate \
                 (join: {res:?})\n{}",
                storm_stats_line()
            );
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "generic/795 storm never reached its coverage: {c}/{WRITER_CYCLES} \
                 writer incarnations, {r}/{READER_OPS} reader ops\n{}",
                storm_stats_line()
            );
        }
        // FORCED-FAILURE DEMONSTRATION lever (round-5 mandate: at least
        // one acceptance log must PROVE the discriminator prints on a
        // failure path): `STORM_FORCE_FAIL=1` fails the storm here, at
        // coverage-poll cadence, through the same panic-inline vehicle.
        if std::env::var("STORM_FORCE_FAIL").is_ok() && r > 0 {
            panic!(
                "STORM_FORCE_FAIL demonstration panic ({c}/{WRITER_CYCLES} \
                 incarnations, {r} reader ops)\n{}",
                storm_stats_line()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.await.unwrap();
    let mut total = 0u64;
    for t in readers {
        total += t.await.unwrap();
    }
    assert!(total > 0, "readers must have served bytes");
}

/// fstests generic/795 repro-port, the OPEN/RECLAIM race face (VL10
/// release gate): `rm` of an unwatched file admits its reclaim; a racing
/// OPEN that lands after the admission must NOT be granted a handle onto
/// the inode being destroyed. Pre-fix, the admission checked the open
/// count BEFORE claiming the in-flight slot, so an OPEN arriving in the
/// gap succeeded and then read a destroyed inode: the daemon's size view
/// collapsed to 0/NotFound, reads went empty, and the kernel
/// zero-extended them to its cached i_size — full-length foreign ZEROS
/// through a legally-open fd (the generic/795 `cmp` mismatch, sticky in
/// the page cache until drop_caches). Contract pinned here: whenever the
/// open HANDLER succeeds, every subsequent read through that handle
/// serves the file's real bytes — never empty, never an error, never
/// zeros; an ENOENT open (lost the race to rm) is the one legal refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn open_racing_reclaim_never_reads_a_destroyed_ino() {
    let h = Arc::new(make().await);
    // 10 blocks: the destroy walks a real block map, keeping the
    // admission->destroy window wide enough to race deterministically.
    let payload: Vec<u8> = (0..10 * BS as usize)
        .map(|i| (i % 251) as u8 ^ 0xA7)
        .collect();

    for round in 0..150u32 {
        let name = format!("hs{round}");
        let ino = create(&h, &name).await;
        write_at(&h, ino, 0, &payload).await;
        // Unlink: nlink -> 0, destroy deferred to the reclaim machinery.
        h.fs.unlink(h.req, 1, OsStr::new(&name)).await.unwrap();

        // Race: the reclaim drive (the release/forget worker's unit of
        // work) vs a LOOP of OPEN + read + release on the same ino for the
        // reclaim's whole duration — every granted handle must serve the
        // real bytes.
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reclaimer = {
            let h = h.clone();
            let done = done.clone();
            tokio::spawn(async move {
                // Let the opener loop get airborne so the admission's
                // open-count re-check races live open/release churn.
                tokio::task::yield_now().await;
                h.fs.reclaim_orphaned_batch(vec![ino]).await;
                done.store(true, std::sync::atomic::Ordering::Release);
            })
        };
        let opener = {
            let h = h.clone();
            let payload = payload.clone();
            let done = done.clone();
            tokio::spawn(async move {
                let mut granted = 0u32;
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    match h.fs.open(h.req, ino, libc::O_RDONLY as u32, 0).await {
                        Ok(reply) => {
                            granted += 1;
                            // Handle granted: the inode must stay fully
                            // readable for the handle's lifetime — read a
                            // window in each block.
                            for b in 0..10u64 {
                                let off = b * BS;
                                let got =
                                    h.fs.read(h.req, ino, reply.fh, off, 4096, 0)
                                        .await
                                        .unwrap_or_else(|e| {
                                            panic!(
                                                "round {round}: read at {off} through a GRANTED \
                                             handle failed {e:?} (open won the race, the \
                                             inode must be alive)"
                                            )
                                        });
                                assert_eq!(
                                    got.data.as_ref(),
                                    &payload[off as usize..off as usize + 4096],
                                    "round {round}: granted handle served wrong/empty bytes \
                                     at {off} (destroyed-under-fd — generic/795)"
                                );
                            }
                            let _ = h.fs.release(h.req, ino, reply.fh, 0, 0, false).await;
                        }
                        Err(e) => {
                            // Lost the race to rm: ENOENT is the one legal refusal.
                            assert_eq!(
                                e,
                                fuse3::Errno::from(libc::ENOENT),
                                "round {round}: open refused with the wrong errno"
                            );
                            break;
                        }
                    }
                    tokio::task::yield_now().await;
                }
                granted
            })
        };
        let (r1, r2) = tokio::join!(reclaimer, opener);
        r1.unwrap();
        r2.unwrap();
    }
}

/// fstests generic/795 — the WHOLE-FILE CLONE face (the last face of the
/// gate's fix ladder, tape-proven): `copy_file_range`'s off 0→0,
/// full-length, empty-dest fast path takes `DataRouter::clone_file`, which
/// cloned the striped source's DURABLE-ONLY block map by refcount. A
/// source whose tail (or any) block's acked bytes still sat in PARKED /
/// STAGED custody (the normal state right after a buffered copy — the
/// write-through merges complete blocks, the partial tail parks) handed
/// the clone a full-size layout with a HOLE where the parked block
/// belongs: every read of the clone served ZEROS there, durably, while
/// the source read fine (its overlay serves) — `cmp orig fsv differ` at
/// exactly the first parked-block boundary, persisting for the clone's
/// whole life. The clone must carry the source's COMPLETE acked custody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whole_file_clone_carries_parked_source_custody() {
    let tag = "clone-parked-src".to_string();
    let h = make().await;
    let src = create(&h, "clsrc").await;

    // Striped source: blocks 0,1 complete (the promotion merges them
    // durably), then a SEPARATE partial tail write — the striped write
    // path parks block 2's acked bytes in the RAM overlay (partial
    // coverage never write-throughs; the durable map stays {0,1}). This
    // is the exact custody state a buffered `cp`-style copy leaves right
    // after its tail write.
    let len = 2 * BS + BS / 2;
    write_at(&h, src, 0, &vec![0x5Cu8; (2 * BS) as usize]).await;
    write_at(&h, src, 2 * BS, &vec![0x5Cu8; (BS / 2) as usize]).await;

    // Move block 2's custody one station down the chain: RAM overlay →
    // staged `active_block:` sibling (what any concurrent reader's
    // multi-block flush does), with the durable merge still QUEUED — the
    // state a busy mount is in almost all the time. The clone's
    // source-freeze must drain THIS station too; pre-fix it scanned RAM
    // overlays only and cloned a map with block 2 missing.
    let tok = h.fs.router.dlm.get_fencing_token_ino(src);
    h.fs.flush_memory_buffers_for_inode(src, tok).await.unwrap();

    // Empty destination + the exact fast-path shape: off 0 -> 0, full
    // length, dest_size 0.
    let dst = create(&h, "cldst").await;
    let copied = cfr(&h, src, 0, dst, 0, len).await;
    assert_eq!(copied, len, "[{tag}] short clone");

    // Every byte of the clone must be the source's acked bytes — the
    // parked tail block included. Pre-fix: [2*BS, 2.5*BS) read ZEROS.
    assert_fill(&h, dst, 0, len, 0x5C, &tag).await;

    // And it stays true across the durable flush cycle on BOTH files.
    fsync(&h, src).await;
    fsync(&h, dst).await;
    assert_fill(&h, dst, 0, len, 0x5C, &tag).await;
    assert_fill(&h, src, 0, len, 0x5C, &tag).await;
}

/// generic/795, the DETERMINISTIC face of the recopy storm above (found
/// by the 2026-08-09 gate under load: 7/20 storm failures overlay-ON,
/// 0/20 overlay-OFF): the overlay compose serve
/// (`try_serve_overlay_read`) clamped its reply to BLOCK bounds only —
/// never to file size — and the read handler consults it BEFORE its
/// size prelude, so a read reaching past EOF inside an overlay-open
/// block returned zero padding AS FILE CONTENT (a full-length reply
/// where the contract demands a short one). Both storm damage labels
/// collapse to this hole ("SIZE LED DATA" verbatim; "ACKED bytes lost"
/// is the same over-serve read against a watermark that advanced before
/// the panic formatted it). Size may never lead data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_compose_read_clamps_to_eof() {
    let h = make().await;
    // Pin the overlay ON explicitly (its integration-binary default):
    // this contract exists to hold ON THE OVERLAY PATH; the storm's
    // overlay-OFF A/B is what proved the accumulation path clean.
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);

    use std::sync::atomic::Ordering as AtomOrd;
    let installs0 = squeezefs::fuse_client::METRICS
        .overlay_installs
        .load(AtomOrd::Relaxed);
    let ino = create(&h, "ovl_eof").await;
    // The storm's own growth shape: sequential 8 KiB chunks walking the
    // file through its layouts — stop mid-block so the file ends with a
    // partial tail block. 6 blocks + one 8 KiB chunk.
    const CHUNK: usize = 8192;
    let tail_block_start = 6 * BS;
    let eof = tail_block_start + CHUNK as u64;
    let pattern: Vec<u8> = (0..eof as usize).map(|i| (i % 251) as u8 ^ 0x5A).collect();
    for off in (0..eof).step_by(CHUNK) {
        let end = ((off as usize) + CHUNK).min(eof as usize);
        write_at(&h, ino, off, &pattern[off as usize..end]).await;
    }

    // A read spanning past EOF must be SHORT — never zero-padded to the
    // requested length. Assert at the tail (the overlay-open block) and,
    // for completeness, at every block's start (path-blind contract).
    for b in 0..=6u64 {
        let off = b * BS;
        let want = ((eof - off) as usize).min(16384);
        let got = read_at(&h, ino, off, 16384).await;
        assert_eq!(
            got.len(),
            want,
            "block {b}: read [{off}, {off}+16384) against eof {eof} must serve \
             exactly {want} bytes — a longer reply is zero padding past EOF \
             served as file content (size led data, generic/795)"
        );
        assert_eq!(
            &got[..],
            &pattern[off as usize..off as usize + want],
            "block {b}: served bytes must be the acked pattern"
        );
    }

    // Wholly-past-EOF reads inside the tail block are EMPTY.
    let got = read_at(&h, ino, eof + 4096, 4096).await;
    assert!(
        got.is_empty(),
        "a read wholly past EOF must be empty, got {} bytes",
        got.len()
    );

    // Engagement: the tail block must actually have exercised the
    // overlay (otherwise this pins nothing — the storm's A/B law).
    assert!(
        squeezefs::fuse_client::METRICS
            .overlay_installs
            .load(AtomOrd::Relaxed)
            > installs0,
        "the fixture never engaged the device overlay — the contract \
         above ran against the accumulation path only"
    );
}

/// generic/795 recopy storm — the SECOND deterministic face (conviction
/// 2026-08-15, tape-attributed at dev tip 9d5ed2d4; survives the two
/// 2026-08-14 `stress_recycled_keys_v3` fixes): the B4 device overlay is
/// a FOURTH custody station — a block's acked, size-published bytes can
/// live at an overlay record's unpublished device dest — and the
/// multi-block read protocol could not see it. An overlay INSTALL +
/// store + ACK landing after the reader's entry drain (and before its
/// size snapshot) is invisible to all three loop defenses: the pre/post
/// parked-run captures probe RAM overlays only, and the custody
/// fingerprint hashes map keys + custody epochs, neither of which an
/// OPEN overlay record moves. The base read then serves the durable
/// world — the old binding for an overwrite record (correct to the old
/// image's length, ZEROS beyond: the storm's exact damage shape) or
/// hole-zeros for a fresh record — inside a reply whose length the
/// already-published size justified. Both storm labels ("ACKED bytes
/// lost from visibility", "SIZE LED DATA") are this one hole. The
/// read-window stall seam selects the schedule deterministically; load
/// only ever selected it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_installed_inside_read_window_never_hides_acked_bytes() {
    use std::sync::atomic::Ordering as AtomOrd;
    let h = Arc::new(make().await);
    // Pin the overlay ON explicitly (its integration-binary default):
    // this contract exists to hold ON THE OVERLAY PATH.
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);

    let ino = create(&h, "ovl_window").await;
    let pattern: Vec<u8> = (0..3 * BS as usize)
        .map(|i| (i % 251) as u8 ^ 0x5A)
        .collect();
    // Blocks 0-1 land and publish durably (fsync drains every overlay and
    // parked buffer): block 2 starts FRESH in every custody station.
    write_at(&h, ino, 0, &pattern[..2 * BS as usize]).await;
    fsync(&h, ino).await;

    // Reader: a multi-block window spanning blocks 1-2, parked INSIDE the
    // read prelude — strictly after its entry overlay drain, before its
    // size snapshot. This is the storm's window, held open.
    let entries0 = squeezefs::fuse_client::test_read_window_stall_entries();
    squeezefs::fuse_client::set_test_read_window_stall(true);
    let reader = {
        let h = h.clone();
        tokio::spawn(async move { h.fs.read(h.req, ino, 0, 2 * BS - 8192, 16384, 0).await })
    };
    let t0 = std::time::Instant::now();
    while squeezefs::fuse_client::test_read_window_stall_entries() == entries0 {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(30),
            "reader never reached the stall window"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    // Writer: the first write to block 2 — the fresh-shape device-overlay
    // store (striped, 4 KiB-aligned, single-block, no custody anywhere).
    // Its ACK publishes size = 2*BS + 8192 while the reader sits inside
    // its window and the bytes sit at the overlay's unpublished dest.
    let installs0 = squeezefs::fuse_client::METRICS
        .overlay_installs
        .load(AtomOrd::Relaxed);
    write_at(
        &h,
        ino,
        2 * BS,
        &pattern[2 * BS as usize..2 * BS as usize + 8192],
    )
    .await;
    assert!(
        squeezefs::fuse_client::METRICS
            .overlay_installs
            .load(AtomOrd::Relaxed)
            > installs0,
        "the racing write never engaged the device overlay — this pins \
         nothing (engagement law)"
    );

    // Release the reader; it resumes at its size snapshot (139264 — the
    // ACK's postlude published it) and composes its reply.
    let esc0 = squeezefs::fuse_client::METRICS
        .overlay_window_escalations
        .load(AtomOrd::Relaxed);
    squeezefs::fuse_client::set_test_read_window_stall(false);
    let reply = reader.await.unwrap().expect("read failed");
    let got = reply.data.as_ref();

    // Engagement (the round-3 architectural law): a live record inside
    // the window is a VALIDATION FAILURE, and every validation failure
    // re-serves through the serialized settle arm — correctness by lock
    // order, not probe completeness.
    assert!(
        squeezefs::fuse_client::METRICS
            .overlay_window_escalations
            .load(AtomOrd::Relaxed)
            > esc0,
        "the read never escalated to the serialized window serve \
         (overlay_window_escalations flat) — the lock-order arm did not \
         engage, so this pin proves nothing"
    );

    // The reply spans [2*BS-8192, 2*BS+8192): the block-1 tail and the
    // block-2 head. The head's bytes were ACKED and size-published
    // strictly before the serve — zeros there are the storm's failure
    // verbatim ("ACKED bytes lost from visibility", generic/795).
    assert_eq!(
        got.len(),
        16384,
        "size 2*BS+8192 was published before the serve; the reply must \
         cover the full window"
    );
    let base = (2 * BS - 8192) as usize;
    for (i, &b) in got.iter().enumerate() {
        assert_eq!(
            b,
            pattern[base + i],
            "byte at file offset {} is wrong (block-2 head = the overlay's \
             acked bytes; zeros here = the open overlay record was \
             invisible to the read window — generic/795)",
            base + i
        );
    }
}

/// generic/795 recopy storm — the THIRD deterministic face (the
/// 2026-08-15 re-falsification, arm 2 of 2): the read loop's two
/// post-read authority reads ran in the INVERTED order relative to the
/// custody transfer they validate. Every settle publishes its
/// destination strictly BEFORE retiring its source (map move `M`, then
/// record retire `R`, `M < R`); the loop read the custody FINGERPRINT
/// first (`T_fp`) and the device-overlay REGISTRY second (`T_probe`,
/// `T_fp < T_probe`). A whole settle landing inside the gap —
/// `T_fp < M < R < T_probe` — is invisible to both: the fingerprint
/// compared two pre-`M` snapshots (match) and the probe found the
/// record already retired (empty), so the loop served the base compose
/// that had resolved the PRE-move map — old-binding bytes / hole-zeros
/// for acked, size-published data (the falsification tape's exact
/// signature at tests line 1092). Probing in the REVERSE order of the
/// publish makes the miss require `R < T_probe < T_fp < M`, i.e.
/// `R < M` — a contradiction — closing the window STRUCTURALLY. The
/// validate-stall seam sits between the two reads and holds a forced
/// settle inside the gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_inside_the_validate_gap_never_hides_acked_bytes() {
    use std::sync::atomic::Ordering as AtomOrd;
    let h = Arc::new(make().await);
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);

    let ino = create(&h, "ovl_vgap").await;
    let pattern: Vec<u8> = (0..3 * BS as usize)
        .map(|i| (i % 251) as u8 ^ 0x5A)
        .collect();
    write_at(&h, ino, 0, &pattern[..2 * BS as usize]).await;
    fsync(&h, ino).await;

    // Reader parks in the PRELUDE window first (so the racing write's
    // overlay install is invisible to its entry drain and size snapshot
    // comes after the ACK — the pin-2 fixture, reused).
    let wentries0 = squeezefs::fuse_client::test_read_window_stall_entries();
    squeezefs::fuse_client::set_test_read_window_stall(true);
    let reader = {
        let h = h.clone();
        tokio::spawn(async move { h.fs.read(h.req, ino, 0, 2 * BS - 8192, 16384, 0).await })
    };
    let t0 = std::time::Instant::now();
    while squeezefs::fuse_client::test_read_window_stall_entries() == wentries0 {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(30),
            "reader never reached the prelude stall window"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    // The racing write: fresh-shape overlay store on block 2, ACK +
    // size publish while the reader is parked pre-snapshot.
    let installs0 = squeezefs::fuse_client::METRICS
        .overlay_installs
        .load(AtomOrd::Relaxed);
    write_at(
        &h,
        ino,
        2 * BS,
        &pattern[2 * BS as usize..2 * BS as usize + 8192],
    )
    .await;
    assert!(
        squeezefs::fuse_client::METRICS
            .overlay_installs
            .load(AtomOrd::Relaxed)
            > installs0,
        "the racing write never engaged the device overlay — this pins \
         nothing (engagement law)"
    );

    // Move the reader from the prelude window into the VALIDATE gap
    // (between its two post-read authority reads), then land the whole
    // settle — publish + retire — inside that gap.
    let ventries0 = squeezefs::fuse_client::test_read_validate_stall_entries();
    squeezefs::fuse_client::set_test_read_validate_stall(true);
    squeezefs::fuse_client::set_test_read_window_stall(false);
    let t1 = std::time::Instant::now();
    while squeezefs::fuse_client::test_read_validate_stall_entries() == ventries0 {
        assert!(
            t1.elapsed() < std::time::Duration::from_secs(30),
            "reader never reached the validate gap"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    // The forced settle: freeze -> seed -> publish (map moves) -> retire,
    // all while the reader sits between its two validation reads.
    h.fs.test_settle_overlay_block(ino, 2, false)
        .await
        .expect("forced settle failed");
    let esc0 = squeezefs::fuse_client::METRICS
        .overlay_window_escalations
        .load(AtomOrd::Relaxed);
    squeezefs::fuse_client::set_test_read_validate_stall(false);

    let reply = reader.await.unwrap().expect("read failed");
    let got = reply.data.as_ref();

    // Engagement (the round-3 architectural law): the probe captured the
    // live record before the gap, so the validation fails and the serve
    // must ride the serialized settle arm.
    assert!(
        squeezefs::fuse_client::METRICS
            .overlay_window_escalations
            .load(AtomOrd::Relaxed)
            > esc0,
        "the read never escalated to the serialized window serve \
         (overlay_window_escalations flat) — the lock-order arm did not \
         engage, so this pin proves nothing"
    );
    assert_eq!(
        got.len(),
        16384,
        "size 2*BS+8192 was published before the serve; the reply must \
         cover the full window"
    );
    let base = (2 * BS - 8192) as usize;
    for (i, &b) in got.iter().enumerate() {
        assert_eq!(
            b,
            pattern[base + i],
            "byte at file offset {} is wrong: the settle that landed inside \
             the validate gap was invisible to both post-read authority \
             reads (fingerprint pre-publish, probe post-retire) — the \
             inverted-probe-order arm of generic/795",
            base + i
        );
    }
}

/// generic/795 recopy storm — the SETTLE-WEDGE face (the 2026-08-15
/// re-falsification, arm 1 of 2, tape: `ovl-settle-oldread-FAIL ...
/// err=UnexpectedEof "short read: kernel returned 8192 of 65536"` in a
/// forever loop, every write to the block EIO, the storm's coverage
/// deadline tripped): an overwrite overlay record whose captured old
/// binding is a RAW short image RESIDENT AT THE BACKING'S TAIL (the
/// staged->striped conversion publishes the partial tail block as a raw
/// key with an image shorter than the block — the storm's block-1 shape
/// on every cycle) cannot settle: the gap-seed's old-image fetch reads
/// the whole device window, the file-backed substrate answers a SHORT
/// read past its tail, the exact-length contract fails it loud, and the
/// record wedges Frozen — every write to the block settles-and-fails
/// (EIO) forever, every read-path drain likewise. The old image's
/// missing tail is the never-written device region — holes both
/// consumers already seed as zeros — so the honest disposition is the
/// readable PREFIX, not an error. Forced here by truncating the backing
/// to the image's end (the exact field shape: the allocation frontier
/// sits beyond the backing tail after churn).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tail_resident_short_old_image_never_wedges_the_settle() {
    let h = Arc::new(make().await);
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);

    let ino = create(&h, "ovl_shortold").await;
    let pattern: Vec<u8> = (0..(BS + 8192) as usize)
        .map(|i| (i % 251) as u8 ^ 0x5A)
        .collect();
    // ONE write past the stripe threshold: the staged->striped conversion
    // publishes block 0 full and block 1 as the PARTIAL tail — a raw key
    // with an 8192-byte stored image (the storm's per-cycle shape).
    write_at(&h, ino, 0, &pattern).await;
    fsync(&h, ino).await;
    let b1_off = {
        let m =
            h.fs.router
                .metadata_cache
                .get(&ino)
                .expect("striped meta present");
        assert_eq!(m.file_type, "striped", "fixture premise: striped layout");
        let k = m
            .block_map
            .as_ref()
            .and_then(|bm| bm.get(&1))
            .expect("block 1 mapped")
            .clone();
        let off: u64 = k.parse().unwrap_or_else(|_| {
            panic!(
                "fixture premise moved: block 1's mapping is no longer a RAW \
                 key ({k}) — re-derive the tail-resident short-image shape"
            )
        });
        off
    };

    // Overwrite overlay on block 1 with gaps on both sides of one page:
    // the settle owes the gaps the OLD image's bytes. Installed BEFORE
    // the truncation below — the seam's store write extends the backing,
    // and the field shape this pins has the OLD image at the tail with
    // the record's dest on a RECYCLED offset elsewhere.
    h.fs.test_install_overwrite_overlay(ino, 1, 8192, &pattern[..4096])
        .await
        .expect("overlay install");

    // Truncate the backing to the OLD image's end: block 1's raw key now
    // names a TAIL-RESIDENT short image — the settle's whole-window
    // old-image read shorts (the exact field shape once the allocation
    // frontier sits beyond the backing tail; in the field the dest is a
    // recycled low offset, here the truncation stands in for that — the
    // seam-stored covered page is sacrificed, so only the gap-seeded old
    // bytes are asserted below).
    h._b.as_file()
        .set_len(b1_off + 8192)
        .expect("backing truncate");

    // Pre-fix: Err(UnexpectedEof "short read ...") forever — the record
    // wedges Frozen and every write to the block EIOs (the storm's
    // coverage-deadline face). Post-fix: the old-image fetch serves the
    // readable PREFIX (its missing tail is the never-written device
    // region = holes = zeros) and the settle publishes.
    h.fs.test_settle_overlay_block(ino, 1, false)
        .await
        .expect("settle of a tail-resident short old image must not wedge");

    // The old image's acked bytes survive the settle verbatim (served
    // from the published dest's gap-seeded [0, 8192) range).
    let got = read_at(&h, ino, BS, 8192).await;
    assert_eq!(got.len(), 8192, "block-1 tail readable");
    assert_eq!(
        &got[..],
        &pattern[BS as usize..(BS + 8192) as usize],
        "block-1's acked bytes must survive the settle of a tail-resident \
         short old image (generic/795 — the settle-wedge face)"
    );
}

/// generic/795 round-4 falsification, arm 1 (the dead-writer tape:
/// `write_at ... Errno(5)` at incarnation 10, then the coverage
/// deadline): the write path's one-authority screen/steal arms
/// propagated a TRANSIENT settle failure — device backpressure, a
/// transient refusal, any of the classes a 120 s slow-box storm selects
/// — straight to the WRITE as EIO. The law (the rebind-starvation law
/// verbatim, applied to the settle unit): transient settle outcomes
/// CONVERGE by bounded retry inside the arm
/// (`settle_overlay_block_converged`); only a failure that survives the
/// budget stays loud. Forced here with the settle transient-failure
/// seam: a Frozen record on the write's own block, the next TWO settle
/// attempts injected to fail — the write must still ACK and the acked
/// bytes must serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_meeting_a_transiently_failing_settle_converges_never_eio() {
    let h = Arc::new(make().await);
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);
    // Patch OFF (the §6 A/B lever): the W1 in-place patch would absorb
    // this pin's block-1 overwrite before it ever reaches the overlay
    // screen — the settle-meeting shape under pin needs the overlay
    // path. Restored below (knob pins must never leak).
    squeezefs::fuse_client::set_patch_max_bytes(0);

    let ino = create(&h, "ovl_transient").await;
    let pattern: Vec<u8> = (0..3 * BS as usize)
        .map(|i| (i % 251) as u8 ^ 0x5A)
        .collect();
    write_at(&h, ino, 0, &pattern[..2 * BS as usize]).await;
    fsync(&h, ino).await;

    // A FROZEN record on block 2 (the seam installs + a manual freeze via
    // the settle seam's own vehicle): install an overwrite record on the
    // MAPPED block 1, then freeze it by injecting a failure into a first
    // settle attempt — the record survives Frozen (the never-lossy error
    // arm), which is exactly the state the writer's steal arm meets.
    h.fs.test_install_overwrite_overlay(ino, 1, 8192, &pattern[..4096])
        .await
        .expect("overlay install");
    squeezefs::fuse_client::set_test_settle_transient_failures(1);
    assert!(
        h.fs.test_settle_overlay_block(ino, 1, false).await.is_err(),
        "fixture: the injected settle failure must surface (record Frozen)"
    );

    // The write to block 1: its screen meets the FROZEN record and must
    // settle it before accumulation. Inject failures into the next SIX
    // settle attempts — past the round-4 bounded budget (4), the round-5
    // falsification's lesson: a write parked on a Frozen record's settle
    // is the writeback ladder's shape and retries FOREVER on transients
    // (never-lossy), so ANY finite injected run must converge. Pre-round-4
    // the first failure EIO'd the write; at round 4's budget the fifth
    // did — the dead-writer tape, twice.
    squeezefs::fuse_client::set_test_settle_transient_failures(6);
    write_at(&h, ino, BS + 8192, &pattern[..8192]).await;
    squeezefs::fuse_client::set_test_settle_transient_failures(0);

    // The acked bytes serve, and the pre-existing record's custody was
    // published by the converged settle (block 1 readable end to end).
    let got = read_at(&h, ino, BS + 8192, 8192).await;
    assert_eq!(
        &got[..],
        &pattern[..8192],
        "the write that converged past the transient settle failures must \
         serve its acked bytes (generic/795 round 4 — the dead-writer law)"
    );
    // The pre-existing block-1 custody survived the churn too.
    let got = read_at(&h, ino, BS, 8192).await;
    assert_eq!(
        &got[..],
        &pattern[BS as usize..BS as usize + 8192],
        "block-1's prior acked bytes must survive the converged settle"
    );
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
}

/// generic/795 round-4 falsification, arm 1's WRITE face by class: the
/// fresh-shape device-overlay mint kept KD-B4-8's LOUD path on
/// StorageFull, converting TRANSIENT space pressure (reclaim lag under
/// settle/publish churn — the storm's steady state on a slow box) into
/// a hard write EIO, while every other write shape rides the
/// never-lossy accumulation ladder under the same pressure. Declining
/// IS the parked supply: the write must ACK through accumulation and
/// the decline must be counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_overlay_mint_under_space_pressure_declines_never_eio() {
    use std::sync::atomic::Ordering as AtomOrd;
    let h = Arc::new(make().await);
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);

    let ino = create(&h, "ovl_enospc").await;
    let pattern: Vec<u8> = (0..2 * BS as usize + 8192)
        .map(|i| (i % 251) as u8 ^ 0x5A)
        .collect();
    write_at(&h, ino, 0, &pattern[..2 * BS as usize]).await;
    fsync(&h, ino).await;

    // Clamp the allocator to ONE chunk — a capacity every fresh mint
    // already exceeds (blocks 0-1 + their promo dests sit above it) —
    // then DRAIN the free list (recycled offsets satisfy an allocate
    // regardless of the clamp): the next allocate is StorageFull — the
    // transient-pressure shape, held deterministically.
    h.ba.set_capacity_bytes(h.ba.chunk_size());
    let mut drained = 0u32;
    while h.ba.allocate_block().await.is_ok() {
        drained += 1;
        assert!(drained < 10_000, "free list never drained under the clamp");
    }

    // The write that walks the fresh-shape overlay screen (striped file,
    // aligned, single-block, block 2 unmapped, no custody anywhere).
    // Pre-fix: the mint's StorageFull surfaced as write EIO (the storm's
    // dead-writer tape). Post-fix: the mint DECLINES (counted) and the
    // write ACKs through the accumulation park, which allocates nothing.
    let declines0 = squeezefs::fuse_client::METRICS
        .overlay_enospc_declines
        .load(AtomOrd::Relaxed);
    write_at(&h, ino, 2 * BS, &pattern[2 * BS as usize..]).await;
    assert!(
        squeezefs::fuse_client::METRICS
            .overlay_enospc_declines
            .load(AtomOrd::Relaxed)
            > declines0,
        "the fresh-shape mint never took the ENOSPC decline — either the \
         clamp missed the window or the loud path is back (engagement law)"
    );

    // The acked bytes serve from the parked custody (no allocation was
    // needed and none may have happened).
    let got = read_at(&h, ino, 2 * BS, 8192).await;
    assert_eq!(
        &got[..],
        &pattern[2 * BS as usize..],
        "acked bytes must serve from the accumulation park"
    );

    // Unclamp so teardown flushes cleanly.
    h.ba.set_capacity_bytes(0);
}

/// generic/795 round-4 falsification, arm 2 (writer starvation): the
/// round-3 escalation SETTLED every live record it met — an OPEN record
/// is the writer's live streaming vehicle, so each reader escalation
/// forced the writer's next chunk to re-install a fresh record (a fresh
/// dest mint), and reader pressure turned that into a settle/re-install
/// feedback storm (the round-3 failure-rate jump and the 10/12
/// dead-writer run). Writer priority WITHIN the lock-order table: an
/// Open record is COMPOSED under the held block guard (claims awaited,
/// covered pages read from the dest — race-free by the guard), never
/// settled; only Frozen/terminal records (a publish already owed)
/// settle. Pinned as: the window-stall schedule's escalated serve
/// leaves the writer's record OPEN and publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn escalation_composes_an_open_record_never_settles_it() {
    use std::sync::atomic::Ordering as AtomOrd;
    let h = Arc::new(make().await);
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);

    let ino = create(&h, "ovl_open_keep").await;
    let pattern: Vec<u8> = (0..3 * BS as usize)
        .map(|i| (i % 251) as u8 ^ 0x5A)
        .collect();
    write_at(&h, ino, 0, &pattern[..2 * BS as usize]).await;
    fsync(&h, ino).await;

    // The pin-2 window schedule: reader parked pre-snapshot, the racing
    // write ACKs into a fresh OPEN overlay record on block 2, reader
    // resumes and must escalate (the record is a validation failure).
    let wentries0 = squeezefs::fuse_client::test_read_window_stall_entries();
    squeezefs::fuse_client::set_test_read_window_stall(true);
    let reader = {
        let h = h.clone();
        tokio::spawn(async move { h.fs.read(h.req, ino, 0, 2 * BS - 8192, 16384, 0).await })
    };
    let t0 = std::time::Instant::now();
    while squeezefs::fuse_client::test_read_window_stall_entries() == wentries0 {
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(30),
            "reader never reached the stall window"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    write_at(
        &h,
        ino,
        2 * BS,
        &pattern[2 * BS as usize..2 * BS as usize + 8192],
    )
    .await;

    let esc0 = squeezefs::fuse_client::METRICS
        .overlay_window_escalations
        .load(AtomOrd::Relaxed);
    let publishes0 = squeezefs::fuse_client::METRICS
        .overlay_publishes
        .load(AtomOrd::Relaxed);
    let feeds0 = squeezefs::fuse_client::METRICS
        .overlay_epoch_feeds
        .load(AtomOrd::Relaxed);
    let open0 = squeezefs::fuse_client::METRICS
        .overlay_open
        .load(AtomOrd::Relaxed);
    squeezefs::fuse_client::set_test_read_window_stall(false);
    let reply = reader.await.unwrap().expect("read failed");
    let got = reply.data.as_ref();

    // Escalation engaged AND the serve is byte-exact...
    assert!(
        squeezefs::fuse_client::METRICS
            .overlay_window_escalations
            .load(AtomOrd::Relaxed)
            > esc0,
        "the read never escalated (engagement law)"
    );
    let base = (2 * BS - 8192) as usize;
    assert_eq!(got.len(), 16384, "size was published before the serve");
    for (i, &byte) in got.iter().enumerate() {
        assert_eq!(
            byte,
            pattern[base + i],
            "byte at file offset {} is wrong under the escalated OPEN-record \
             compose",
            base + i
        );
    }
    // ...and the WRITER'S RECORD SURVIVED: no settle, no publish, no
    // feed, record still open (writer priority — the round-4 law).
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .overlay_publishes
            .load(AtomOrd::Relaxed),
        publishes0,
        "the escalated serve PUBLISHED the writer's open record — the \
         settle/re-install feedback storm is back"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .overlay_epoch_feeds
            .load(AtomOrd::Relaxed),
        feeds0,
        "the escalated serve FED the writer's open record — the \
         settle/re-install feedback storm is back"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .overlay_open
            .load(AtomOrd::Relaxed),
        open0,
        "the writer's open record must survive the escalated serve"
    );
}
