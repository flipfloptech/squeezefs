//! Finding 47 — the device overlay's **length floor**
//! (`docs/design-overlay-overwrite.md` §5.1, the §5.8 falsifier arm).
//!
//! The overlay is the ALIGNED SINGLE-BLOCK overwrite/fresh store of the
//! 1 MiB+ segment A-leg (KD-B4). Its shape screen in `write_file_staged`
//! had no minimum length, and the overlay arm runs BEFORE the W2 extent
//! park — so a sub-cap write (≤ `patch_max_bytes()` = block_size/8, the
//! W1 class boundary) that the W1 patch DECLINED at state time (hole /
//! clone-shared / decorated / co-writer-refused / stream-adjacent) never
//! reached W2: it minted a fresh CoW dest block, held it Open, and at
//! settle read the whole old image and seeded the whole complement —
//! the field row `.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md`
//! (6 refused patches → `overlay_gap_seed_old_bytes` = 6 × 4 MiB −
//! 24,576, one 4 KiB write per record). The Random-small-write program
//! (design-random-small-writes §5.2) owns that population: W1 in place
//! when eligible, else the W2 byte-budgeted extent park + amortized fold.
//!
//! The floor is DERIVED, never a constant: overlay-eligible by length
//! ⇔ `len > patch_max_bytes()` — literally the W1 predicate-5 oversize
//! verdict, so the two classes tile the sub-block population exactly
//! (≤ cap ⇒ W1/W2; > cap ⇒ overlay/accumulation). Composition with the
//! `SQUEEZEFS_PATCH_MAX_BYTES=0` A/B lever: cap 0 empties the W1 class,
//! so every length is overlay-eligible — exactly as it makes none
//! patch-eligible.
//!
//! Harness note: the in-process harness has no mount, so it applies the
//! mount's own derivation (`apply_derived_write_knobs` ⇒
//! `set_patch_max_bytes(derived_patch_max_bytes(bs))`) explicitly. The
//! store vehicle is the registered Bytes seam (the device_overlay_tests
//! convention); ACK-early is pinned OFF so metric deltas are settled
//! when `write` returns.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::device_overlay::{
    set_ack_early_for_tests, set_device_overlay_for_tests, set_overlay_overwrite_for_tests,
};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    derived_patch_max_bytes, overlay_length_eligible, patch_max_bytes, set_patch_max_bytes,
    SqueezefsFilesystem, METRICS,
};
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

/// Sandbox block size: 64 KiB ⇒ the derived W1 cap is 8 KiB, so 4 KiB
/// and 8 KiB segments are SUB-CAP and 16 KiB+ segments are the overlay
/// class. Every offset speaks in the 4096-byte LBA quantum.
const BS: u64 = 64 * 1024;
/// A second geometry for the derivation pin: 256 KiB ⇒ cap 32 KiB, so
/// the SAME 16 KiB segment flips from overlay-class to W2-class.
const BS_WIDE: u64 = 256 * 1024;
/// The amplification burst's geometry: 1 MiB ⇒ cap 128 KiB, 25 %
/// escalation edge at 256 KiB — 16 × 4 KiB per block stays an extent
/// overlay, so the fsync fold IS the W2 fill-16 arithmetic.
const BS_MIB: u64 = 1024 * 1024;
const PAGE: u64 = 4096;

/// Process-global METRICS deltas: serialize tests.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore the default posture on scope exit (knob hygiene).
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        set_patch_max_bytes(512 * 1024);
        squeezefs::device_overlay::clear_device_overlay_for_tests();
    }
}

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    bs: u64,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn format_meta(path: &std::path::Path, uuid: [u8; 16]) {
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xF47_0F47_0F47_0F47,
        uuid,
    })
    .unwrap()
    .build(path, 128 * 1024 * 1024)
    .await
    .unwrap();
}

/// Overlay ON (both arms), Bytes vehicle, ACK-after-CQE, and the W1 cap
/// DERIVED from the block size exactly as the mount derives it.
async fn make(tag: &str, bs: u64) -> (H, LeverGuard) {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", bs.to_string());
    set_device_overlay_for_tests(true, true);
    set_overlay_overwrite_for_tests(true);
    set_ack_early_for_tests(false, false);
    squeezefs::routing::set_rewrite_shadow(true);
    set_patch_max_bytes(derived_patch_max_bytes(bs));
    let guard = LeverGuard;

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(512 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let mut uuid = *b"ovl-len-floor-47";
    uuid[15] = (bs / 1024) as u8;
    format_meta(m.path(), uuid).await;
    let s = tempdir().unwrap();

    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(tag).await.unwrap());
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
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
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    for kv in &routed.volumes {
        ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
            .await
            .expect("v3 refcount recovery");
    }
    assert_eq!(
        fs.router.block_size.load(Ordering::Relaxed),
        bs,
        "harness premise: the router runs the requested block size"
    );
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    (
        H {
            fs,
            req,
            bs,
            _b: b,
            _m: m,
            _s: s,
        },
        guard,
    )
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
        .unwrap_or_else(|e| panic!("write ino {ino} off {off}: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at off {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, len: usize) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, len as u32, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off}: {e:?}"))
        .data
        .to_vec()
}

async fn fsync(h: &H, ino: u64) {
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
}

async fn set_size(h: &H, ino: u64, size: u64) {
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
    .expect("setattr size");
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| ((i % 249) as u8) ^ tag | 1).collect()
}

fn m(v: &squeezefs::fuse_client::Align64<std::sync::atomic::AtomicU64>) -> u64 {
    v.load(Ordering::Relaxed)
}

/// A durable striped fixture: `mapped` whole blocks written + fsynced
/// (striped authority — §7.1), then the size EXTENDED to `total` blocks
/// so blocks `mapped..total` are HOLES inside i_size (the
/// `patch_ineligible_unmapped` population, not the extends-the-file
/// oversize class).
async fn sparse_striped(h: &H, name: &str, mapped: u64, total: u64, tag: u8) -> (u64, Vec<u8>) {
    let ino = create(h, name).await;
    let base = pattern((mapped * h.bs) as usize, tag);
    write_at(h, ino, 0, &base).await;
    fsync(h, ino).await;
    if total > mapped {
        set_size(h, ino, total * h.bs).await;
    }
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    assert_eq!(
        meta.block_map.as_ref().map(|bm| bm.len()).unwrap_or(0),
        mapped as usize,
        "fixture premise: exactly the written blocks are mapped"
    );
    assert_eq!(meta.size, total * h.bs, "fixture premise: sparse size");
    (ino, base)
}

/// The routing ledger the floor moves.
#[derive(Clone, Copy, Debug)]
struct Snap {
    overlay_installs: u64,
    overlay_overwrite_installs: u64,
    overlay_stores: u64,
    overlay_store_bytes: u64,
    overlay_gap_seed_bytes: u64,
    overlay_gap_seed_old_bytes: u64,
    overlay_ineligible_sub_cap: u64,
    overlay_open: u64,
    extent_parks: u64,
    fold_passes: u64,
    patch_writes: u64,
    patch_write_bytes: u64,
    patch_ineligible_unmapped: u64,
    patch_ineligible_shared: u64,
    patch_ineligible_adjacent: u64,
    write_through_blocks: u64,
    write_through_bytes: u64,
    durable_upload_bytes: u64,
}

fn snap() -> Snap {
    Snap {
        overlay_installs: m(&METRICS.overlay_installs),
        overlay_overwrite_installs: m(&METRICS.overlay_overwrite_installs),
        overlay_stores: m(&METRICS.overlay_stores),
        overlay_store_bytes: m(&METRICS.overlay_store_bytes),
        overlay_gap_seed_bytes: m(&METRICS.overlay_gap_seed_bytes),
        overlay_gap_seed_old_bytes: m(&METRICS.overlay_gap_seed_old_bytes),
        overlay_ineligible_sub_cap: m(&METRICS.overlay_ineligible_sub_cap),
        overlay_open: m(&METRICS.overlay_open),
        extent_parks: m(&METRICS.extent_parks),
        fold_passes: m(&METRICS.fold_passes),
        patch_writes: m(&METRICS.patch_writes),
        patch_write_bytes: m(&METRICS.patch_write_bytes),
        patch_ineligible_unmapped: m(&METRICS.patch_ineligible_unmapped),
        patch_ineligible_shared: m(&METRICS.patch_ineligible_shared),
        patch_ineligible_adjacent: m(&METRICS.patch_ineligible_adjacent),
        write_through_blocks: m(&METRICS.write_through_blocks),
        write_through_bytes: m(&METRICS.write_through_bytes),
        durable_upload_bytes: m(&METRICS.durable_upload_bytes_writeback)
            + m(&METRICS.durable_upload_bytes_self_flush)
            + m(&METRICS.durable_upload_bytes_escalation),
    }
}

macro_rules! delta {
    ($after:expr, $before:expr, $field:ident) => {
        $after.$field - $before.$field
    };
}

/// Data-namespace DEVICE WRITE BYTES a workload paid, composed from the
/// always-on ledger faces (the in-process twin of the rig's
/// `/proc/diskstats` column): overlay stores + gap seeds, W1 patch DMA,
/// write-through, the writeback/self-flush uploads, and the W2 folds
/// (one whole-block upload per pass — `fold_upload_block` carries no
/// byte face of its own).
fn device_write_bytes(after: Snap, before: Snap, bs: u64) -> u64 {
    delta!(after, before, overlay_store_bytes)
        + delta!(after, before, overlay_gap_seed_bytes)
        + delta!(after, before, patch_write_bytes)
        + delta!(after, before, write_through_bytes)
        + delta!(after, before, durable_upload_bytes)
        + delta!(after, before, fold_passes) * bs
}

// ---------------------------------------------------------------------
// (1) The hole shape — `patch_ineligible_unmapped` ⇒ W2, never the overlay
// ---------------------------------------------------------------------

/// A 4 KiB write into a HOLE of a striped file (overlay ON, both arms)
/// must not mint an overlay store: the W1 predicate declines it at state
/// time (`unmapped`), and the ladder falls through to the W2 extent
/// park exactly as before the overlay landed. At fsync the W2 fold
/// publishes ONE whole block with zero gap seeding (a hole seeds
/// nothing — the RW4 hole-soak law).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sub_cap_hole_write_rides_w2_never_the_overlay() {
    let _g = serial().await;
    let (h, _lever) = make("f47_hole", BS).await;
    let (ino, _base) = sparse_striped(&h, "hole.dat", 2, 8, 0x47).await;

    let before = snap();
    let p = pattern(PAGE as usize, 0x11);
    // Block 3 (a hole inside i_size), second page — aligned, sub-cap
    // (4 KiB ≤ the derived 8 KiB cap), non-extending, not stream-adjacent.
    write_at(&h, ino, 3 * BS + PAGE, &p).await;
    let after = snap();

    assert_eq!(
        delta!(after, before, patch_ineligible_unmapped),
        1,
        "the W1 state screen must decline the hole (the ladder order is untouched)"
    );
    assert_eq!(
        delta!(after, before, overlay_installs),
        0,
        "a sub-cap hole write must NOT install an overlay record (finding 47)"
    );
    assert_eq!(
        delta!(after, before, overlay_stores),
        0,
        "no overlay store for a sub-cap segment"
    );
    assert_eq!(
        delta!(after, before, overlay_ineligible_sub_cap),
        1,
        "the floor's ledger names the decline"
    );
    assert_eq!(
        delta!(after, before, extent_parks),
        1,
        "the W2 extent park absorbs the sub-cap hole write"
    );
    assert_eq!(
        after.overlay_open, before.overlay_open,
        "no open record minted"
    );

    fsync(&h, ino).await;
    let settled = snap();
    assert_eq!(
        delta!(settled, before, overlay_gap_seed_bytes),
        0,
        "no overlay ⇒ no gap seeding at the durability boundary"
    );
    assert_eq!(
        delta!(settled, before, fold_passes),
        1,
        "the W2 fold publishes the block once"
    );
    // Byte-exact: the page, zeros around it.
    let got = read_at(&h, ino, 3 * BS, BS as usize).await;
    assert!(
        got[..PAGE as usize].iter().all(|&x| x == 0),
        "hole prefix zeros"
    );
    assert_eq!(
        &got[PAGE as usize..2 * PAGE as usize],
        &p[..],
        "the written page"
    );
    assert!(
        got[2 * PAGE as usize..].iter().all(|&x| x == 0),
        "hole suffix zeros"
    );
}

// ---------------------------------------------------------------------
// (2) The clone-shared shape — `patch_ineligible_shared` ⇒ W2 park
// ---------------------------------------------------------------------

/// A 4 KiB write to a CLONE-SHARED block (refcount 2 after a whole-file
/// CFR clone) declines the W1 patch (in-place mutation of a pinned
/// block is corruption) and must ride the W2 park — never the B4
/// overwrite arm, whose settle would read the whole old image and seed
/// the whole complement from it (`overlay_gap_seed_old_bytes`, the
/// field row's exact arithmetic). The clone's snapshot never moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sub_cap_write_to_a_clone_shared_block_rides_w2_never_the_overlay() {
    let _g = serial().await;
    let (h, _lever) = make("f47_shared", BS).await;
    let (src, base) = sparse_striped(&h, "shr_src.dat", 4, 4, 0x15).await;

    let path = squeezefs::keys::inode_path(src);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    let bk0 = meta.block_map.as_ref().unwrap().get(&0).unwrap().clone();
    let off0 = h.fs.router.backend_router.parse_block_offset(&bk0).unwrap();
    let dst = create(&h, "shr_dst.dat").await;
    let copied =
        h.fs.copy_file_range(h.req, src, 0, 0, dst, 0, 0, base.len() as u64, 0)
            .await
            .expect("whole-file CFR clone")
            .copied;
    assert_eq!(copied, base.len() as u64, "clone copies the whole file");
    assert_eq!(
        h.fs.router.backend_router.default_allocator.refcount(off0),
        Some(2),
        "fixture premise: the clone pinned block 0 (refcount 2)"
    );

    let before = snap();
    let p = pattern(PAGE as usize, 0xBD);
    write_at(&h, src, 2 * PAGE, &p).await; // aligned, sub-cap, mapped, SHARED
    let after = snap();

    assert_eq!(
        delta!(after, before, patch_ineligible_shared),
        1,
        "the W1 state screen declines the clone-shared block"
    );
    assert_eq!(
        delta!(after, before, patch_writes),
        0,
        "never in place under a clone"
    );
    assert_eq!(
        delta!(after, before, overlay_overwrite_installs),
        0,
        "a sub-cap write to a shared block must NOT take the B4 overwrite arm"
    );
    assert_eq!(
        delta!(after, before, overlay_installs),
        0,
        "no overlay record"
    );
    assert_eq!(
        delta!(after, before, overlay_ineligible_sub_cap),
        1,
        "the floor's ledger names the decline (the overwrite shape too)"
    );
    assert_eq!(
        delta!(after, before, extent_parks),
        1,
        "the W2 extent park absorbs the patch-ineligible sub-cap write"
    );

    fsync(&h, src).await;
    let settled = snap();
    assert_eq!(
        delta!(settled, before, overlay_gap_seed_old_bytes),
        0,
        "zero old-image gap seeding — the §5.8 falsifier stays silent"
    );
    // The clone's snapshot is untouched; the source carries the write.
    let got_dst = read_at(&h, dst, 0, base.len()).await;
    assert_eq!(got_dst, base, "clone snapshot untouched by the CoW write");
    let mut want_src = base.clone();
    want_src[2 * PAGE as usize..3 * PAGE as usize].copy_from_slice(&p);
    let got_src = read_at(&h, src, 0, base.len()).await;
    assert_eq!(got_src, want_src, "source carries the write");
}

// ---------------------------------------------------------------------
// (3) The A-leg — above-cap aligned overwrites keep the overlay
// ---------------------------------------------------------------------

/// The win must not regress: an aligned overwrite LONGER than the W1
/// cap (16 KiB and a whole 64 KiB block here; the 1 MiB+ kernel
/// segments in the field) of a mapped striped passthrough block still
/// installs the B4 overwrite overlay — the floor only ever declines the
/// sub-cap population.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn above_cap_aligned_overwrite_still_takes_the_overlay() {
    let _g = serial().await;
    let (h, _lever) = make("f47_aleg", BS).await;
    let (ino, base) = sparse_striped(&h, "aleg.dat", 3, 3, 0x33).await;
    assert!(
        derived_patch_max_bytes(BS) < 16 * 1024,
        "premise: 16 KiB is ABOVE the derived cap at this geometry"
    );

    let before = snap();
    // 16 KiB (> 8 KiB cap) into mapped block 1, aligned, non-adjacent.
    let seg16 = pattern(16 * 1024, 0x44);
    write_at(&h, ino, BS + 2 * PAGE, &seg16).await;
    // A whole-block aligned overwrite of mapped block 0 — the exact
    // field A-leg shape at this block size.
    let seg_full = pattern(BS as usize, 0x55);
    write_at(&h, ino, 0, &seg_full).await;
    let after = snap();

    assert_eq!(
        delta!(after, before, overlay_overwrite_installs),
        2,
        "both above-cap aligned overwrites install the B4 overwrite overlay"
    );
    assert_eq!(
        delta!(after, before, overlay_stores),
        2,
        "one store per segment"
    );
    assert_eq!(
        delta!(after, before, extent_parks),
        0,
        "the overlay class never parks extents"
    );
    assert_eq!(
        delta!(after, before, overlay_ineligible_sub_cap),
        0,
        "the floor stays silent above the cap"
    );

    fsync(&h, ino).await;
    let mut want = base.clone();
    want[..BS as usize].copy_from_slice(&seg_full);
    want[(BS + 2 * PAGE) as usize..(BS + 2 * PAGE) as usize + 16 * 1024].copy_from_slice(&seg16);
    let got = read_at(&h, ino, 0, base.len()).await;
    assert_eq!(got, want, "durable byte-exactness across both arms");
}

// ---------------------------------------------------------------------
// (4) The floor is DERIVED — it moves with the block size
// ---------------------------------------------------------------------

/// The SAME 16 KiB aligned hole write is overlay-class on a 64 KiB
/// block (cap 8 KiB) and W2-class on a 256 KiB block (cap 32 KiB): the
/// boundary is `derived_patch_max_bytes(block_size)` = block_size/8,
/// never a free-floating byte constant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_floor_tracks_the_block_size() {
    let _g = serial().await;
    assert_eq!(derived_patch_max_bytes(BS), BS / 8);
    assert_eq!(derived_patch_max_bytes(BS_WIDE), BS_WIDE / 8);
    assert_eq!(
        derived_patch_max_bytes(4 * 1024 * 1024),
        512 * 1024,
        "shipped geometry"
    );
    let seg = pattern(16 * 1024, 0x66);

    {
        let (h, _lever) = make("f47_bs64", BS).await;
        let (ino, _) = sparse_striped(&h, "bs64.dat", 2, 6, 0x61).await;
        let before = snap();
        write_at(&h, ino, 4 * BS, &seg).await;
        let after = snap();
        assert_eq!(
            delta!(after, before, overlay_installs),
            1,
            "16 KiB > the 8 KiB cap at 64 KiB blocks ⇒ overlay"
        );
        assert_eq!(delta!(after, before, extent_parks), 0);
        fsync(&h, ino).await;
        assert_eq!(read_at(&h, ino, 4 * BS, seg.len()).await, seg);
    }
    {
        let (h, _lever) = make("f47_bs256", BS_WIDE).await;
        let (ino, _) = sparse_striped(&h, "bs256.dat", 2, 6, 0x62).await;
        let before = snap();
        write_at(&h, ino, 4 * BS_WIDE, &seg).await;
        let after = snap();
        assert_eq!(
            delta!(after, before, overlay_installs),
            0,
            "16 KiB ≤ the 32 KiB cap at 256 KiB blocks ⇒ sub-cap ⇒ never the overlay"
        );
        assert_eq!(
            delta!(after, before, extent_parks),
            1,
            "…and the W2 park owns it"
        );
        fsync(&h, ino).await;
        assert_eq!(read_at(&h, ino, 4 * BS_WIDE, seg.len()).await, seg);
    }
}

// ---------------------------------------------------------------------
// (5) The A/B lever composition — cap 0 ⇒ every length is overlay-eligible
// ---------------------------------------------------------------------

/// `SQUEEZEFS_PATCH_MAX_BYTES=0` empties the W1 class (nothing is
/// patch-eligible); the floor composes with it by emptying the sub-cap
/// class too: a 4 KiB hole write is overlay-eligible BY LENGTH and
/// installs a (fresh) overlay record. The lever keeps meaning "no W1",
/// never "no overlay".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_cap_zero_makes_every_length_overlay_eligible() {
    let _g = serial().await;
    let (h, _lever) = make("f47_cap0", BS).await;
    let (ino, _) = sparse_striped(&h, "cap0.dat", 2, 6, 0x70).await;

    // The predicate itself: `len > cap`, the W1 oversize verdict — the
    // classes tile at the cap (≤ cap ⇒ W1/W2, > cap ⇒ overlay) — and
    // cap 0 admits every length.
    let cap = patch_max_bytes();
    assert_eq!(cap, derived_patch_max_bytes(BS), "harness derived the cap");
    assert!(!overlay_length_eligible(PAGE));
    assert!(
        !overlay_length_eligible(cap),
        "len == cap is W1's (predicate 5 admits it)"
    );
    assert!(overlay_length_eligible(cap + PAGE));
    set_patch_max_bytes(0);
    assert!(
        overlay_length_eligible(PAGE),
        "cap 0: every length is overlay-eligible"
    );

    let before = snap();
    let p = pattern(PAGE as usize, 0x71);
    write_at(&h, ino, 3 * BS + PAGE, &p).await;
    let after = snap();

    assert_eq!(
        delta!(after, before, patch_writes) + delta!(after, before, patch_ineligible_unmapped),
        0,
        "cap 0: the W1 ladder is silent (no patch, no ledger)"
    );
    assert_eq!(
        delta!(after, before, overlay_installs),
        1,
        "cap 0: a 4 KiB hole write is overlay-eligible by length"
    );
    assert_eq!(
        delta!(after, before, overlay_ineligible_sub_cap),
        0,
        "cap 0: the floor's ledger is silent too (no sub-cap class exists)"
    );
    assert_eq!(delta!(after, before, extent_parks), 0, "…and never parks");
    fsync(&h, ino).await;
    assert_eq!(read_at(&h, ino, 3 * BS + PAGE, PAGE as usize).await, p);
}

// ---------------------------------------------------------------------
// (6) The amplification contract — a rand-4k burst into holes
// ---------------------------------------------------------------------

/// 64 random-order 4 KiB writes into 4 hole blocks (16 per block —
/// `fold_fill` = 16, the W2 law's gauge) of a sparse striped file on a
/// 1 MiB block (cap 128 KiB; 16 pages = 64 KiB stays under the 25 %
/// escalation edge): the floor sends EVERY op down the W2 ladder (zero
/// overlay records minted — no per-touched-block dest pinned across the
/// burst), and at fsync the folds pay ONE whole-block upload per touched
/// block: device write bytes = 4 × 1 MiB ÷ 256 KiB = 16× ≤ 30× (the
/// closing law's fill-16 arithmetic), never the 1,024× the per-op
/// arithmetic of a fill-1 settle reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rand_4k_burst_into_holes_rides_w2_amortized() {
    let _g = serial().await;
    let (h, _lever) = make("f47_burst", BS_MIB).await;
    // Blocks 0–1 mapped, 2–5 holes (the burst's targets), 6–7 tail holes.
    let (ino, _) = sparse_striped(&h, "burst.dat", 2, 8, 0x80).await;

    // One op per (block, page): 4 blocks × 16 pages, visited in a fixed
    // permutation (stride 37 is coprime with 64) whose consecutive ops
    // always change block — never a stream-adjacent pair (predicate 6
    // must not be what routes these).
    let ops: Vec<(u64, u64)> = (0..64u64).map(|i| (2 + i % 4, i / 4)).collect();
    let order: Vec<usize> = (0..64).map(|i| (i * 37 + 3) % 64).collect();
    let user_bytes = 64 * PAGE;

    let before = snap();
    let mut want: Vec<Vec<u8>> = vec![vec![0u8; BS_MIB as usize]; 4];
    for &i in &order {
        let (blk, page) = ops[i];
        let p = pattern(PAGE as usize, (0x90 + i) as u8);
        write_at(&h, ino, blk * BS_MIB + page * PAGE, &p).await;
        want[(blk - 2) as usize][(page * PAGE) as usize..((page + 1) * PAGE) as usize]
            .copy_from_slice(&p);
    }
    let burst = snap();
    assert_eq!(
        delta!(burst, before, overlay_installs),
        0,
        "no overlay record may be minted for a sub-cap burst"
    );
    assert_eq!(
        burst.overlay_open, before.overlay_open,
        "no dest pinned per touched block"
    );
    assert_eq!(
        delta!(burst, before, overlay_ineligible_sub_cap),
        64,
        "the floor engaged on every op of the burst"
    );
    assert_eq!(
        delta!(burst, before, extent_parks),
        64,
        "every op parks a ~4 KiB extent (W2)"
    );
    assert_eq!(
        delta!(burst, before, patch_ineligible_adjacent),
        0,
        "the permutation never produced a stream-adjacent pair"
    );

    fsync(&h, ino).await;
    let settled = snap();
    let dev_w = device_write_bytes(settled, before, BS_MIB);
    let amp = dev_w as f64 / user_bytes as f64;
    assert!(
        amp <= 30.0,
        "device write bytes {dev_w} ÷ user bytes {user_bytes} = {amp:.1}× exceeds the W2 fold law"
    );
    assert_eq!(
        delta!(settled, before, fold_passes),
        4,
        "one fold per touched block (fill 16)"
    );
    assert_eq!(
        delta!(settled, before, overlay_gap_seed_bytes),
        0,
        "no overlay gap seeds"
    );
    for (k, w) in want.iter().enumerate() {
        let got = read_at(&h, ino, (2 + k as u64) * BS_MIB, BS_MIB as usize).await;
        assert!(got == *w, "block {} byte-exact after the fold", 2 + k);
    }
}

// ---------------------------------------------------------------------
// Fat #2 — small-bs SEQUENTIAL segments accumulate (request-size face)
// ---------------------------------------------------------------------

/// Stream-adjacent sub-cap segments (4 KiB × 16 filling one fresh block
/// — the small-bs sequential O_DIRECT shape) are `patch_ineligible_
/// adjacent` and must accumulate in the `ActiveBlockBuf` into ONE
/// whole-block write-through — the pre-overlay, request-size-
/// preserving behavior — instead of one overlay store per op (the
/// wareq-sz collapse to bs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn small_bs_sequential_segments_accumulate_into_one_write_through() {
    let _g = serial().await;
    let (h, _lever) = make("f47_seq", BS).await;
    let (ino, _) = sparse_striped(&h, "seq.dat", 2, 4, 0xA0).await;

    let before = snap();
    let mut want = vec![0u8; BS as usize];
    for i in 0..(BS / PAGE) {
        let p = pattern(PAGE as usize, (0xA1 + i) as u8);
        write_at(&h, ino, 2 * BS + i * PAGE, &p).await;
        want[(i * PAGE) as usize..((i + 1) * PAGE) as usize].copy_from_slice(&p);
    }
    // The complete-block write-through rides the detached write
    // pipeline (its ACK detaches from the DMA): drain it before reading
    // the ledger — no fsync yet, the write-through is the point.
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "pipeline must drain"
    );
    let after = snap();

    assert_eq!(
        delta!(after, before, overlay_stores),
        0,
        "sequential sub-cap segments never ride the overlay per op"
    );
    assert_eq!(delta!(after, before, overlay_installs), 0);
    assert_eq!(
        delta!(after, before, overlay_ineligible_sub_cap),
        BS / PAGE,
        "the floor engaged on every segment"
    );
    assert_eq!(
        delta!(after, before, write_through_blocks),
        1,
        "the coverage union completes the block ⇒ ONE whole-block write-through"
    );
    // Only the stream's FIRST segment may park (its adjacency is
    // unknowable — predicate 6 needs a previous end); the second
    // segment escalates that extent overlay in place and the stream
    // keeps the whole-block economy (W2 §5.2).
    assert!(
        delta!(after, before, extent_parks) <= 1,
        "stream-adjacent segments never park per op (got {})",
        delta!(after, before, extent_parks)
    );
    assert!(
        delta!(after, before, patch_ineligible_adjacent) >= (BS / PAGE) - 1,
        "predicate 6 classified the stream"
    );

    fsync(&h, ino).await;
    assert_eq!(read_at(&h, ino, 2 * BS, BS as usize).await, want);
}
