//! PR B2 — device-overlay **sync reservation + fresh/hole stores**
//! (`docs/design-device-overlay.md` Rev 2 §8 B2). Red-first: compiles
//! against the `squeezefs::device_overlay` wiring and the `overlay_*`
//! metrics, which do not exist until B2 lands.
//!
//! Scope under test (the SAFEST shape only — blocks with NO old
//! binding: fresh allocations and holes, `old_binding_or_hole = Hole`,
//! law 5's gaps are zeros, no displaced free exists):
//!
//! * install/reserve/store/coverage wiring — engagement counted
//!   (`overlay_installs/stores/store_bytes`), publication at coverage
//!   completion, ACK-after-CQE (KD-OV-7);
//! * the fsync sequence: freeze → complete → **seed zeros** → flush →
//!   publish (`overlay_gap_seed_bytes` pays only on partial overlays;
//!   a successful fsync leaves no overlay unpublished —
//!   `overlay_unpublished_at_fsync` stays 0);
//! * the law-5 recycled-content screen: uncovered ranges of an open
//!   overlay NEVER serve the destination's recycled device bytes —
//!   zeros for the fresh/hole shape (pinned against a deliberately
//!   recycled offset);
//! * generic/209 (fresh-file shape): a byte whose write COMPLETED
//!   before a read began must never read stale — reads of open
//!   overlays drain (freeze/complete/seed/publish) rather than
//!   compose in B2 (the §5.2 lock-free composition is PR B3);
//! * **fresh-shape overlays never create a rewrite-shadow interaction**
//!   (KD-OV-12's B2 face: shadow stays default-ON; the five B4
//!   dual-authority hazards cannot arise because the fresh shape has no
//!   old binding — pinned as zero shadow-epoch deltas);
//! * crash recovery (kill-9 face): a dropped session's
//!   allocated-but-unpublished destinations are reclaimed by the
//!   existing mount census arithmetic — the file reads its pre-overlay
//!   image and the offsets re-enter the free supply (KD-OV-14: the
//!   overlay-specific attribution lives HERE, in harness knowledge —
//!   production structurally cannot tell these offsets apart);
//! * ineligible shapes decline structurally (unaligned, mapped block)
//!   and ride the accumulation path byte-exact.
//!
//! Vehicle note: in-process suites carry `WritePayload::Bytes` (no
//! zc-armed transport), so the harness arms the registered test seam
//! `SQUEEZEFS_TEST_OVERLAY_BYTES` — the §4.3 pooled vehicle serves as
//! the store engine, exercising the SAME install/reserve/claim/
//! coverage/publish/drain laws the slot leg runs (the slot leg's
//! engagement is the tcp-devsub rig's job).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::device_overlay::{set_ack_early_for_tests, set_device_overlay_for_tests};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Sandbox block size (the extent_patch_tests convention): every
/// offset/length below speaks in the 4096-byte LBA quantum of the v1
/// aligned-only gate.
const BS: u64 = 64 * 1024;
const PAGE: u64 = 4096;

/// Process-global METRICS deltas: serialize tests.
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

async fn open_fs(
    tag: &str,
    meta_path: &std::path::Path,
    backing_path: &std::path::Path,
    staging: &std::path::Path,
) -> SqueezefsFilesystem {
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing_path.to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let cache = TieredCache::new(
        vec![staging.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = KvMetaBackend::open(meta_path).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    // v3 refcount recovery, exactly like a real remount (main.rs mount
    // path).
    for kv in &routed.volumes {
        ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
            .await
            .expect("v3 refcount recovery");
    }
    fs
}

async fn format_meta(path: &std::path::Path, uuid: [u8; 16]) {
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xDE1_ACE_0FF_5E7,
        uuid,
    })
    .unwrap()
    .build(path, 128 * 1024 * 1024)
    .await
    .unwrap();
}

async fn make(tag: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    set_device_overlay_for_tests(true, true);
    // This suite pins ACK-after-CQE (KD-OV-7): production Bytes overlay
    // now ACKs early by default, which would race the crash-drop and
    // the post-write metric checks.
    set_ack_early_for_tests(false, false);
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    format_meta(m.path(), *b"dev-overlay-b2!!").await;
    let s = tempdir().unwrap();
    let fs = open_fs(tag, m.path(), b.path(), s.path()).await;
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

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
}

async fn unlink(h: &H, name: &str) {
    h.fs.unlink(h.req, 1, OsStr::new(name)).await.unwrap();
}

/// Promote `ino` to striped authority (two whole blocks written and
/// fsynced — §7.1: overlays install only AFTER promotion has fully
/// drained and published) and return the striped base image.
async fn promote_striped(h: &H, ino: u64) -> Vec<u8> {
    let base: Vec<u8> = (0..2 * BS).map(|i| (i % 251) as u8).collect();
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    base
}

fn m(v: &squeezefs::fuse_client::Align64<std::sync::atomic::AtomicU64>) -> u64 {
    v.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------
// Engagement + publication
// ---------------------------------------------------------------------

/// The B2 write half: aligned segments into a FRESH block install one
/// overlay, store per segment, publish ONCE at coverage completion, and
/// read back byte-exact durably. The accumulation path is not paid for
/// the overlaid block (no extraction destination, no parked buffer
/// write-through).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_block_segments_ride_the_overlay_and_publish_at_completion() {
    let _g = serial().await;
    let h = make("ovl_engage").await;
    let ino = create(&h, "engage").await;
    promote_striped(&h, ino).await;

    let installs0 = m(&METRICS.overlay_installs);
    let stores0 = m(&METRICS.overlay_stores);
    let bytes0 = m(&METRICS.overlay_store_bytes);
    let pubs0 = m(&METRICS.overlay_publishes);
    let wt0 = m(&METRICS.write_through_blocks);
    let open0 = m(&METRICS.overlay_open);
    let seedb0 = m(&METRICS.overlay_gap_seed_bytes);

    // Block 2 (fresh — beyond the striped image): four aligned 16 KiB
    // segments, in order.
    let seg: Vec<u8> = vec![0xAB; (BS / 4) as usize];
    for i in 0..4u64 {
        write_at(&h, ino, 2 * BS + i * (BS / 4), &seg).await;
    }
    fsync(&h, ino).await;

    assert_eq!(
        m(&METRICS.overlay_installs) - installs0,
        1,
        "one overlay record per (ino, block)"
    );
    assert_eq!(
        m(&METRICS.overlay_stores) - stores0,
        4,
        "one store per segment"
    );
    assert_eq!(
        m(&METRICS.overlay_store_bytes) - bytes0,
        BS,
        "store bytes ≈ user bytes on the eligible shape"
    );
    assert!(
        m(&METRICS.overlay_publishes) - pubs0 >= 1,
        "coverage completion publishes the block"
    );
    assert_eq!(
        m(&METRICS.write_through_blocks) - wt0,
        0,
        "the overlaid block never rides the accumulation write-through"
    );
    assert_eq!(
        m(&METRICS.overlay_gap_seed_bytes) - seedb0,
        0,
        "a full-coverage stream seeds nothing (the B4 falsifier's instrument)"
    );
    assert_eq!(
        m(&METRICS.overlay_open),
        open0,
        "no record survives the fsync"
    );
    assert_eq!(
        m(&METRICS.overlay_unpublished_at_fsync),
        0,
        "§6.2 invariant"
    );

    let got = read_at(&h, ino, 2 * BS, BS as u32).await;
    assert_eq!(got.len(), BS as usize);
    assert!(
        got.iter().all(|&b| b == 0xAB),
        "durable read-back must be byte-exact"
    );
}

/// The fsync sequence on a PARTIAL overlay: freeze → complete → seed
/// ZEROS into the gaps → publish a whole block. Law 5's dual (§6.2
/// step 3): the seed pays exactly the gap bytes, and the published
/// block's gaps read zeros — never recycled device content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_overlay_fsync_seeds_zeros_and_publishes_whole_block() {
    let _g = serial().await;
    let h = make("ovl_seed").await;
    let ino = create(&h, "seed").await;
    promote_striped(&h, ino).await;

    let seeds0 = m(&METRICS.overlay_gap_seeds);
    let seedb0 = m(&METRICS.overlay_gap_seed_bytes);
    let open0 = m(&METRICS.overlay_open);

    // One 16 KiB segment at block 2 + 16 KiB (an interior segment: gaps
    // on BOTH sides).
    let seg: Vec<u8> = vec![0x5A; (BS / 4) as usize];
    write_at(&h, ino, 2 * BS + BS / 4, &seg).await;
    fsync(&h, ino).await;

    assert!(
        m(&METRICS.overlay_gap_seeds) - seeds0 >= 1,
        "a partial overlay pays its gap seed at fsync"
    );
    assert_eq!(
        m(&METRICS.overlay_gap_seed_bytes) - seedb0,
        BS - BS / 4,
        "the seed pays exactly the gap bytes"
    );
    assert_eq!(m(&METRICS.overlay_open), open0);
    assert_eq!(m(&METRICS.overlay_unpublished_at_fsync), 0);

    // POSIX size: the acked end (2·BS + BS/2) — the seeded zeros beyond
    // it are never size-visible (generic/795: size never leads data).
    let sz = h.fs.getattr(h.req, ino, None, 0).await.unwrap().attr.size;
    assert_eq!(sz, 2 * BS + BS / 2, "size floors at the acked end");
    let got = read_at(&h, ino, 2 * BS, (BS / 2) as u32).await;
    assert_eq!(got.len(), (BS / 2) as usize);
    for (i, &b) in got.iter().enumerate() {
        let expect = if (i as u64) >= BS / 4 { 0x5A } else { 0 };
        assert_eq!(
            b, expect,
            "byte {i}: the pre-segment hole must read zeros, the segment its bytes"
        );
    }
}

// ---------------------------------------------------------------------
// Read visibility (generic/209 fresh-file shape + law 5)
// ---------------------------------------------------------------------

/// RYW on an OPEN overlay: an ACKed segment reads back immediately
/// (law 1 — an ACKed byte is always resolvable), and the uncovered
/// ranges read ZEROS even when the destination offset carries recycled
/// bytes from a freed block (law 5 — the recycled-content leak, pinned
/// red-first against a deliberately dirtied free list).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_overlay_reads_serve_acked_bytes_and_zero_gaps() {
    let _g = serial().await;
    let h = make("ovl_ryw").await;

    // Dirty the free list: a whole striped file of 0xEE, then unlink it
    // — its device offsets return to the free supply carrying 0xEE.
    let dirty = create(&h, "dirty").await;
    let junk: Vec<u8> = vec![0xEE; 3 * BS as usize];
    write_at(&h, dirty, 0, &junk).await;
    fsync(&h, dirty).await;
    unlink(&h, "dirty").await;

    let ino = create(&h, "ryw").await;
    promote_striped(&h, ino).await;

    let installs0 = m(&METRICS.overlay_installs);

    // One interior segment into fresh block 2; NO fsync.
    let seg: Vec<u8> = vec![0x77; (BS / 4) as usize];
    write_at(&h, ino, 2 * BS + BS / 4, &seg).await;
    assert_eq!(
        m(&METRICS.overlay_installs) - installs0,
        1,
        "the segment must ride the overlay for this pin to mean anything"
    );

    // RYW: the ACKed segment.
    let got = read_at(&h, ino, 2 * BS + BS / 4, (BS / 4) as u32).await;
    assert!(
        got.iter().all(|&b| b == 0x77),
        "an ACKed overlay byte must read back (law 1/4)"
    );
    // Law 5: the gap BEFORE the segment — zeros, never 0xEE.
    let gap = read_at(&h, ino, 2 * BS, (BS / 4) as u32).await;
    assert!(
        gap.iter().all(|&b| b == 0),
        "an uncovered overlay range must read zeros, never the \
         destination's recycled device content (law 5)"
    );
    // Leave the process-global gauges clean for the sibling tests.
    fsync(&h, ino).await;
}

/// generic/209, fresh-file shape: a sequential fresh-file writer races
/// a reader; every byte whose write COMPLETED before the read began
/// must never read stale (zeros where data was ACKed, or a previous
/// value). The overlay path must hold the same contract the
/// accumulation path holds today.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_stream_storm_never_serves_stale_bytes() {
    let _g = serial().await;
    let h = Arc::new(make("ovl_storm").await);
    let ino = create(&h, "storm").await;
    promote_striped(&h, ino).await;

    let installs0 = m(&METRICS.overlay_installs);

    const BLOCKS: u64 = 6; // blocks 2..8 — fresh territory
    let (tx, rx) = tokio::sync::watch::channel(0u64); // completed end (abs)

    let writer = {
        let h = h.clone();
        tokio::spawn(async move {
            for p in 0..(BLOCKS * BS / PAGE) {
                let off = 2 * BS + p * PAGE;
                let val = (p % 199 + 1) as u8;
                write_at(&h, ino, off, &vec![val; PAGE as usize]).await;
                let _ = tx.send(off + PAGE);
            }
            drop(tx);
        })
    };

    let reader = {
        let h = h.clone();
        let mut rx = rx.clone();
        tokio::spawn(async move {
            loop {
                let end = *rx.borrow_and_update();
                if end > 2 * BS {
                    let off = 2 * BS + ((end - 2 * BS) / 2 / PAGE) * PAGE;
                    let got = read_at(&h, ino, off, PAGE as u32).await;
                    let p = (off - 2 * BS) / PAGE;
                    let want = (p % 199 + 1) as u8;
                    for (i, &b) in got.iter().enumerate() {
                        assert_eq!(
                            b, want,
                            "STALE BYTE at {off}+{i}: completed write must be visible \
                             (generic/209, fresh-file shape)"
                        );
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
    fsync(&h, ino).await;
    assert!(
        m(&METRICS.overlay_installs) - installs0 >= 1,
        "the storm must have exercised the overlay path"
    );
    // Post-storm durable audit.
    for p in 0..(BLOCKS * BS / PAGE) {
        let off = 2 * BS + p * PAGE;
        let want = (p % 199 + 1) as u8;
        let got = read_at(&h, ino, off, PAGE as u32).await;
        assert!(
            got.iter().all(|&b| b == want),
            "post-storm byte at {off} must carry its pass value"
        );
    }
}

// ---------------------------------------------------------------------
// Rewrite-shadow coexistence (KD-OV-12's B2 face)
// ---------------------------------------------------------------------

/// Fresh-shape overlays must never create a shadow-epoch interaction:
/// rewrite shadow stays default-ON, and the overlay's publish rides the
/// direct merge — zero epoch opens, zero swaps, zero shadow records
/// across the overlay window (the five B4 dual-authority hazards are
/// structurally unreachable without an old binding — assert it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_overlay_never_creates_a_shadow_epoch_interaction() {
    let _g = serial().await;
    assert!(
        squeezefs::routing::rewrite_shadow_enabled(),
        "the pin only means something with shadow default-ON"
    );
    let h = make("ovl_shadow").await;
    let ino = create(&h, "shadow").await;
    promote_striped(&h, ino).await;
    // Quiesce the fixture: fsync above closed any promotion-era epoch.
    let swaps0 = m(&METRICS.rewrite_shadow_swaps);
    let installs0 = m(&METRICS.overlay_installs);

    let seg: Vec<u8> = vec![0x33; (BS / 4) as usize];
    for i in 0..4u64 {
        write_at(&h, ino, 2 * BS + i * (BS / 4), &seg).await;
    }
    fsync(&h, ino).await;

    assert_eq!(
        m(&METRICS.overlay_installs) - installs0,
        1,
        "the window must have ridden the overlay"
    );
    assert_eq!(
        m(&METRICS.rewrite_shadow_swaps) - swaps0,
        0,
        "a fresh-shape overlay publish must never ride the rewrite shadow"
    );
    assert_eq!(
        m(&METRICS.rewrite_shadow_open_epochs),
        0,
        "no epoch may remain open after the overlay window"
    );
}

// ---------------------------------------------------------------------
// Crash recovery (the kill-9 face — volatile overlay, §6.1/§6.3 W1)
// ---------------------------------------------------------------------

/// A session dropped with OPEN overlays (kill-9 equivalent: no drain,
/// no publish) recovers by the existing census arithmetic: the file
/// reads its pre-overlay image, and the allocated-but-unpublished
/// destination offsets re-enter the free supply (a fresh allocation
/// reuses them). Harness-attributed per KD-OV-14 — production cannot
/// tell an overlay's destination from any other unpublished offset.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_without_drain_reclaims_unpublished_offsets() {
    let _g = serial().await;
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "65536");
    set_device_overlay_for_tests(true, true);
    set_ack_early_for_tests(false, false);
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m_file = NamedTempFile::new().unwrap();
    m_file.as_file().set_len(128 * 1024 * 1024).unwrap();
    format_meta(m_file.path(), *b"dev-overlay-b2c!").await;
    let s = tempdir().unwrap();
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };

    let base;
    let ino;
    {
        // Session 1: striped base + an UNDRAINED overlay segment.
        let fs = open_fs("ovl_crash1", m_file.path(), b.path(), s.path()).await;
        let h = H {
            fs,
            req,
            _b: b,
            _m: m_file,
            _s: s,
        };
        ino = create(&h, "crash").await;
        base = promote_striped(&h, ino).await;
        let installs0 = m(&METRICS.overlay_installs);
        let seg: Vec<u8> = vec![0xC4; (BS / 4) as usize];
        write_at(&h, ino, 2 * BS, &seg).await;
        assert_eq!(
            m(&METRICS.overlay_installs) - installs0,
            1,
            "the segment must have opened an overlay for this test to bite"
        );
        // Kill-9: drop everything with the record OPEN. Reclaim the
        // tempfiles from the harness first so the paths survive.
        let H { fs, _b, _m, _s, .. } = h;
        drop(fs);

        // Session 2 over the same volumes: the census walk (remount).
        let fs2 = open_fs("ovl_crash2", _m.path(), _b.path(), _s.path()).await;
        let h2 = H {
            fs: fs2,
            req,
            _b,
            _m,
            _s,
        };

        // The pre-overlay image is intact; the un-fsynced overlay bytes
        // are lost (the stated writeback-class contract, §6.1).
        let got = read_at(&h2, ino, 0, (2 * BS) as u32).await;
        assert_eq!(got, base, "the durable pre-overlay image must survive");
        let sz = h2.fs.getattr(req, ino, None, 0).await.unwrap().attr.size;
        assert_eq!(sz, 2 * BS, "un-fsynced overlay size never became durable");

        // The overlay's destination offset (block idx 2 — the third
        // allocation on a fresh store) is reclaimable: the recovered
        // allocator hands it to the next writer.
        let (_, ba2, _) = h2
            .fs
            .router
            .backend_router
            .get_active_backend()
            .expect("active backend");
        let reused = ba2.allocate_block().await.expect("post-recovery allocate");
        assert_eq!(
            reused,
            2 * ba2.chunk_size(),
            "the unpublished overlay destination must re-enter the free supply"
        );
    }
}

// ---------------------------------------------------------------------
// Structural refusals (§7 v1 hard gates, the B2 subset)
// ---------------------------------------------------------------------

/// Ineligible shapes never install a record and ride the accumulation
/// path byte-exact: an unaligned segment (v1 gate row 3) and an
/// overwrite of an already-mapped block (fresh/hole only in B2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ineligible_shapes_decline_and_stay_byte_exact() {
    let _g = serial().await;
    let h = make("ovl_gate").await;
    let ino = create(&h, "gate").await;
    promote_striped(&h, ino).await;

    let installs0 = m(&METRICS.overlay_installs);

    // Unaligned length into a fresh block: declines (accumulation).
    let odd: Vec<u8> = vec![0x11; 5000];
    write_at(&h, ino, 3 * BS, &odd).await;
    // Aligned overwrite of a MAPPED block (block 0): declines in B2.
    let over: Vec<u8> = vec![0x22; PAGE as usize];
    write_at(&h, ino, 0, &over).await;
    assert_eq!(
        m(&METRICS.overlay_installs) - installs0,
        0,
        "ineligible shapes must not install overlay records"
    );

    fsync(&h, ino).await;
    let got = read_at(&h, ino, 3 * BS, 5000).await;
    assert!(got.iter().all(|&b| b == 0x11), "unaligned write byte-exact");
    let got = read_at(&h, ino, 0, PAGE as u32).await;
    assert!(got.iter().all(|&b| b == 0x22), "overwrite byte-exact");
}

/// Truncate on an ino with an OPEN overlay (a shape-change op, §7 row
/// 6): the overlay is drained or superseded — never a stale serve, no
/// stranded record, and re-extension reads zeros.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_drains_open_overlays() {
    let _g = serial().await;
    let h = make("ovl_trunc").await;
    let ino = create(&h, "trunc").await;
    promote_striped(&h, ino).await;
    let open0 = m(&METRICS.overlay_open);

    let seg: Vec<u8> = vec![0x99; (BS / 4) as usize];
    write_at(&h, ino, 2 * BS, &seg).await;

    // Truncate back into the striped base — the overlaid block dies.
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(BS),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        m(&METRICS.overlay_open),
        open0,
        "no overlay record may survive a truncate of its ino"
    );

    // Re-extend across the dead overlay's range: zeros, never 0x99.
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(3 * BS),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let got = read_at(&h, ino, 2 * BS, (BS / 4) as u32).await;
    assert!(
        got.iter().all(|&b| b == 0),
        "a truncated-away overlay must never resurface"
    );
    fsync(&h, ino).await;
}
