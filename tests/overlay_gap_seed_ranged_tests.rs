//! W-6 (e2e perf audit write board #10, `docs/design-e2e-perf-audit.md`
//! §3.4 Write #10): the device overlay's settle seeds a partial
//! OVERWRITE record's uncovered ranges from the captured old binding
//! (design-overlay-overwrite §5.8) — and until this campaign it fetched
//! the WHOLE old block image to source them, whatever the gap size: a
//! 4 MiB block with a 64 KiB hole read 4 MiB to seed 64 KiB (the
//! amplification face `overlay_gap_seed_old_bytes` never showed it,
//! because that gauge counts seeded bytes, not device bytes read).
//!
//! **The seed-bytes law** pinned here: on a passthrough volume with an
//! undecorated old binding, sourcing a K-byte uncovered span costs a
//! device read of exactly K bytes (gaps are OVERLAY_PAGE-aligned by
//! construction, so K is already the LBA-rounded length) — the ranged
//! primitive the read path already owns (`read_block_range`), issued
//! per gap. The whole-image read survives only where it is cheaper
//! (Σ gaps ≥ the block window) or unavoidable (a decorated
//! `bk:off:len` old binding). `SQUEEZEFS_GAP_SEED_RANGED=0` is the A/B
//! lever (the shipped whole-image shape, byte-identical).
//!
//! Gauges: `overlay_gap_seed_read_bytes` — device bytes READ to source
//! gap seeds (the amplification numerator: whole window or Σ ranged);
//! `overlay_gap_seed_ranged_bytes` — the seeded old-image bytes that
//! rode the ranged funnel (⊆ `overlay_gap_seed_old_bytes` ⊆
//! `overlay_gap_seed_bytes`).
//!
//! Transformed (compressed/encrypted) volumes need the whole stored
//! image by construction — and the device overlay is passthrough-only
//! by its shape screen, so the ranged funnel is structurally unreachable
//! there: pinned as "no overlay seed, no ranged bytes, data byte-exact"
//! on an lz4 fixture, so a future widening of the overlay screen cannot
//! quietly drag a raw ranged read under a transformed image.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

/// 16 KiB blocks: four OVERLAY_PAGE (4 KiB) pages per block, so partial
/// coverage shapes have one to three gap pages.
const FBS: u64 = 16 * 1024;
const PAGE: u64 = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore default posture on scope exit (the overlay_overwrite_tests
/// knob hygiene).
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::set_rewrite_shadow(true);
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
        squeezefs::device_overlay::clear_device_overlay_for_tests();
    }
}

/// Suite posture: patch path off, shadow on, product overlay stores
/// disabled (the seam is the only overlay source), the ranged seed as
/// the caller says.
fn levers(ranged: bool) -> LeverGuard {
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(true);
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    squeezefs::device_overlay::set_ack_early_for_tests(false, false);
    squeezefs::device_overlay::set_gap_seed_ranged_for_tests(ranged);
    LeverGuard
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    _backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

async fn make_harness(test_id: &str, compressed: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    if compressed {
        router.set_crypto(squeezefs::crypto_compress::CryptoCompressState::new(
            "lz4".to_string(),
            "none".to_string(),
            None,
        ));
    }
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    squeezefs::meta_backend::kv::builder::format_v3(
        m.path(),
        128 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
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
        fs: Arc::new(fs),
        req,
        _backing: backing,
        _m: m,
        _s: s,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
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
        .unwrap_or_else(|e| panic!("write off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .expect("read")
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
}

async fn quiesce(h: &H) {
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
}

/// Striped fixture with caller-provided content, drained + fsync'd, the
/// RAM layout refetched so the seam sees the durable map.
async fn striped_fixture_with(h: &H, name: &str, content: &[u8]) -> u64 {
    assert_eq!(content.len() as u64 % FBS, 0, "whole blocks only");
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new(name),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    write_at(h, ino, 0, content).await;
    fsync(h, ino).await;
    quiesce(h).await;
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    ino
}

fn m64(v: &squeezefs::fuse_client::Align64<std::sync::atomic::AtomicU64>) -> u64 {
    v.load(Ordering::Relaxed)
}

/// The gap-seed ledger snapshot every pin differences.
#[derive(Clone, Copy)]
struct Seeds {
    seeds: u64,
    bytes: u64,
    old_bytes: u64,
    ranged_bytes: u64,
    read_bytes: u64,
    write_path_seed_read_bytes: u64,
}

fn seeds() -> Seeds {
    Seeds {
        seeds: m64(&METRICS.overlay_gap_seeds),
        bytes: m64(&METRICS.overlay_gap_seed_bytes),
        old_bytes: m64(&METRICS.overlay_gap_seed_old_bytes),
        ranged_bytes: m64(&METRICS.overlay_gap_seed_ranged_bytes),
        read_bytes: m64(&METRICS.overlay_gap_seed_read_bytes),
        write_path_seed_read_bytes: m64(&METRICS.write_path_seed_read_bytes),
    }
}

/// Install a partial overwrite record covering `[rel, rel+len)` of
/// block 0 with `fill` bytes and settle it at fsync; return the
/// ledger delta and the durable block-0 image.
async fn overwrite_then_settle(h: &H, ino: u64, rel: u64, len: u64, fill: u8) -> (Seeds, Vec<u8>) {
    let before = seeds();
    let seg = vec![fill; len as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, rel as usize, &seg)
        .await
        .expect("seam install (partial overwrite)");
    fsync(h, ino).await;
    let after = seeds();
    h.fs.router.metadata_cache.remove(&ino);
    let got = read_at(h, ino, 0, FBS as usize).await;
    (
        Seeds {
            seeds: after.seeds - before.seeds,
            bytes: after.bytes - before.bytes,
            old_bytes: after.old_bytes - before.old_bytes,
            ranged_bytes: after.ranged_bytes - before.ranged_bytes,
            read_bytes: after.read_bytes - before.read_bytes,
            write_path_seed_read_bytes: after.write_path_seed_read_bytes
                - before.write_path_seed_read_bytes,
        },
        got,
    )
}

/// The old⊕new image law, byte-exact: covered range = fill, the rest =
/// the old image.
fn assert_old_xor_new(got: &[u8], old: &[u8], rel: usize, len: usize, fill: u8) {
    assert_eq!(
        &got[..rel],
        &old[..rel],
        "pre-segment gap serves the old bytes"
    );
    assert!(
        got[rel..rel + len].iter().all(|&x| x == fill),
        "the covered segment serves the new bytes"
    );
    assert_eq!(
        &got[rel + len..],
        &old[rel + len..FBS as usize],
        "post-segment gap serves the old bytes"
    );
}

// ---------------------------------------------------------------------------
// The seed-bytes law — one trailing gap page.
// ---------------------------------------------------------------------------

/// A record covering three of four pages leaves ONE 4 KiB gap: the
/// ranged seed reads exactly those 4 KiB (not the 16 KiB block), the
/// ranged face accounts the whole old-sourced seed, and the durable
/// image is old⊕new byte-exact. The write-path seed tripwire stays 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ranged_gap_seed_reads_only_the_uncovered_span_on_passthrough() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("w6_gap_ranged_one", false).await;
    let old = pattern((2 * FBS) as usize, 61);
    let ino = striped_fixture_with(&h, "f1", &old).await;

    let (d, got) = overwrite_then_settle(&h, ino, 0, 3 * PAGE, 0x5A).await;
    assert_eq!(d.seeds, 1, "one gap ⇒ one seed write");
    assert_eq!(d.bytes, PAGE, "the seed writes exactly the gap");
    assert_eq!(d.old_bytes, PAGE, "the gap is sourced from the old binding");
    assert_eq!(
        d.ranged_bytes, PAGE,
        "the seed-bytes law: a K-byte uncovered span rides the ranged funnel for K bytes"
    );
    assert_eq!(
        d.read_bytes, PAGE,
        "device bytes READ to source the seed = the gap, never the whole {FBS}-byte block"
    );
    assert_eq!(
        d.write_path_seed_read_bytes, 0,
        "the settle seed never counts in the write-path seed tripwire"
    );
    assert_old_xor_new(&got, &old, 0, (3 * PAGE) as usize, 0x5A);
}

// ---------------------------------------------------------------------------
// The A/B lever — SQUEEZEFS_GAP_SEED_RANGED=0 is the shipped whole read.
// ---------------------------------------------------------------------------

/// With the lever OFF the same shape reads the WHOLE old image (the
/// pre-campaign posture, byte-identical) and the ranged face stays 0 —
/// the seeded bytes and the durable image are unchanged either way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gap_seed_lever_off_reads_the_whole_old_image() {
    let _g = serial().await;
    let _l = levers(false);
    let h = make_harness("w6_gap_whole", false).await;
    let old = pattern((2 * FBS) as usize, 62);
    let ino = striped_fixture_with(&h, "f1", &old).await;

    let (d, got) = overwrite_then_settle(&h, ino, 0, 3 * PAGE, 0x6B).await;
    assert_eq!(d.seeds, 1);
    assert_eq!(d.bytes, PAGE);
    assert_eq!(d.old_bytes, PAGE, "seeded bytes are lever-independent");
    assert_eq!(
        d.ranged_bytes, 0,
        "lever off ⇒ nothing rides the ranged funnel"
    );
    assert_eq!(
        d.read_bytes, FBS,
        "lever off ⇒ the shipped whole-image read (the A/B control's shape)"
    );
    assert_old_xor_new(&got, &old, 0, (3 * PAGE) as usize, 0x6B);
}

// ---------------------------------------------------------------------------
// Two gaps around one covered page — one ranged read per gap.
// ---------------------------------------------------------------------------

/// Coverage of page 1 alone leaves a leading 4 KiB gap and a trailing
/// 8 KiB gap: two seed writes, Σ 12 KiB read (< the 16 KiB block, so
/// the ranged form still wins and is taken), image byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_gaps_seed_ranged_each_and_compose_exactly() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("w6_gap_two", false).await;
    let old = pattern((2 * FBS) as usize, 63);
    let ino = striped_fixture_with(&h, "f1", &old).await;

    let (d, got) = overwrite_then_settle(&h, ino, PAGE, PAGE, 0x7C).await;
    assert_eq!(d.seeds, 2, "two gaps ⇒ two seed writes");
    assert_eq!(d.bytes, 3 * PAGE);
    assert_eq!(d.old_bytes, 3 * PAGE);
    assert_eq!(d.ranged_bytes, 3 * PAGE, "both gaps rode the ranged funnel");
    assert_eq!(
        d.read_bytes,
        3 * PAGE,
        "Σ ranged reads = Σ gaps, strictly below the whole-block window"
    );
    assert_old_xor_new(&got, &old, PAGE as usize, PAGE as usize, 0x7C);
}

// ---------------------------------------------------------------------------
// Transformed volumes: no overlay seed at all, ranged face stays 0.
// ---------------------------------------------------------------------------

/// On an lz4 volume the device overlay never engages (its shape screen
/// is passthrough-only), so a partial overwrite rides the accumulation
/// path, whose flush re-derives the WHOLE stored image by construction
/// (a compressed image has no addressable sub-range). Pinned: zero
/// overlay gap seeds, zero ranged bytes, zero device bytes on the seed
/// ledger — and the rewritten block reads back old⊕new exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transformed_volume_never_reaches_the_ranged_seed() {
    let _g = serial().await;
    let _l = levers(true);
    // Product overlay stores ON here: the point is that the passthrough
    // screen — not the suite's posture — keeps a transformed volume off
    // the overlay.
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(true);
    let h = make_harness("w6_gap_transformed", true).await;
    let old = pattern((2 * FBS) as usize, 64);
    let ino = striped_fixture_with(&h, "f1", &old).await;

    let before = seeds();
    let seg = vec![0x8Du8; PAGE as usize];
    write_at(&h, ino, PAGE, &seg).await;
    fsync(&h, ino).await;
    quiesce(&h).await;
    let after = seeds();
    assert_eq!(
        after.seeds - before.seeds,
        0,
        "no overlay record on a transformed volume"
    );
    assert_eq!(after.ranged_bytes - before.ranged_bytes, 0);
    assert_eq!(after.read_bytes - before.read_bytes, 0);

    h.fs.router.metadata_cache.remove(&ino);
    let got = read_at(&h, ino, 0, FBS as usize).await;
    assert_old_xor_new(&got, &old, PAGE as usize, PAGE as usize, 0x8D);
}
