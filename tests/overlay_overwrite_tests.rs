//! PR B4b — the device-overlay OVERWRITE arm's **coexistence arm (a)**
//! (`docs/design-overlay-overwrite.md` rev 4, §5.4/§5.4a): a completed
//! overwrite overlay FEEDS `rewrite_shadow_record` — the dest key
//! becomes the epoch's B key and the EPOCH is the one pending-binding
//! authority (publication, displaced park, deferred free, fencing,
//! crash windows — KD-B4-1). Red-first: this suite compiles against
//! the B4b seams (`test_install_overwrite_overlay`,
//! `test_settle_overlay_block`) and the `overlay_epoch_feeds` /
//! `overlay_feed_fallbacks` counters, none of which exist until B4b
//! lands — compile-red IS the red state.
//!
//! **Venue adjudication (B4a flagged it; decided here):** the
//! mapped-decline gates stay CLOSED until B4c-ii, so suites must
//! install overwrite records directly — but `DeviceOverlayRegistry::
//! install` and the `MintedBlockGuard` half of its signature are
//! `pub(crate)`, unreachable from integration tests, while the hazard
//! pins need sandbox meta volumes, fsck runs and journal-entry counts
//! (integration scale, the `rewrite_shadow_tests` harness class). So
//! the seam is a `#[doc(hidden)] pub` pair on `SqueezefsFilesystem`
//! (the `routing::set_test_zc_slot_wrap` house precedent), documented
//! on the items as "B4b test seam — narrowed at B4c-ii".
//!
//! The FIVE dual-authority hazards, each pinned red-first with the
//! shadow lever default-ON (the design's falsifier: a hazard pin that
//! needs a coordination patch instead of the structural discharge
//! means arm (a) is not actually one authority — STOP):
//!
//! 1. **hazard1** — one displaced-old queue: only the epoch parks.
//! 2. **hazard2** — no double free: kill the free twice, fsck C2/C3 clean.
//! 3. **hazard3** — no KD-1.11 resurrection through the KD-1.9
//!    refetch-compose after the feed.
//! 4. **hazard4** — ONE fsync authority: journal-entry-count equality
//!    vs the un-fed (accumulation) control — the law-7 pin.
//! 5. **hazard5** — zero durable-ref drift (the C8 oracle,
//!    `SQUEEZEFS_TEST_STAMP_BLOCK_REFS=1`).
//!
//! Plus: the `SQUEEZEFS_REWRITE_SHADOW=0` degenerate's
//! displaced-free-after-publish pin; the §5.4 venue law
//! (`close_never_runs_under_block_guard` — the settle returns a
//! close-owed HINT and never closes itself; the guard-dropping caller
//! acts); the R7 ENOSPC convergence (the epoch close recycles the
//! displaced supply, so a bounded store sustains an overwrite loop);
//! and the fsync ordering pin (drain-feeds-before-the-one-close).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fsck::{run as run_fsck, FsckCtx, FsckOptions};
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use squeezefs::meta_backend::Metadata;
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

// 16 KiB blocks (B4c-i): partial-coverage shapes need multi-page blocks
// (coverage/claims are 4 KiB-page-granular); every B4b pin speaks in FBS
// multiples so the bump is shape-neutral. Allocator capacity stays in
// CHUNK_SIZE granules (one chunk per FS block) — unchanged.
const FBS: u64 = 16 * 1024;
/// The v1 aligned-segment quantum (OVERLAY_PAGE).
const PAGE: u64 = 4096;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Restore default posture on scope exit (knob hygiene — the
/// rewrite_shadow_tests pattern).
struct LeverGuard;
impl Drop for LeverGuard {
    fn drop(&mut self) {
        squeezefs::routing::set_rewrite_shadow(true);
        squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
        squeezefs::device_overlay::clear_device_overlay_for_tests();
        squeezefs::block_reclaim::set_elision_class_all(false);
    }
}

/// Suite posture: patch path off (overwrites ride write-through),
/// shadow per-test, and the DEVICE OVERLAY DISABLED for product
/// writes — the seam is the ONLY overlay source, so fixtures and the
/// hazard4 accumulation control stay deterministic (the mapped-decline
/// gates are closed until B4c-ii anyway; this pins the posture).
fn levers(shadow: bool) -> LeverGuard {
    squeezefs::fuse_client::set_patch_max_bytes(0);
    squeezefs::routing::set_rewrite_shadow(shadow);
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    squeezefs::device_overlay::set_ack_early_for_tests(false, false);
    LeverGuard
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    routed: Arc<squeezefs::meta_backend::RoutedMetaBackend>,
    req: Request,
    backing: NamedTempFile,
    m: NamedTempFile,
    staging_path: std::path::PathBuf,
    _s: tempfile::TempDir,
}

async fn make_harness_capped(test_id: &str, capacity_blocks: Option<u64>) -> H {
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let m = NamedTempFile::new().unwrap();
    make_harness_on(test_id, backing, m, true, capacity_blocks).await
}

/// The remount-capable constructor (the rewrite_shadow_tests kill-9
/// pattern): `format = false` re-opens EXISTING volumes and runs the
/// recovery walk — the crash-matrix session-2 shape.
async fn make_harness_on(
    test_id: &str,
    backing: NamedTempFile,
    m: NamedTempFile,
    format: bool,
    capacity_blocks: Option<u64>,
) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    if let Some(blocks) = capacity_blocks {
        ba.set_capacity_bytes(blocks * ba.chunk_size());
    }
    let s = tempdir().unwrap();
    let staging_path = s.path().to_path_buf();
    let cache = TieredCache::new(
        vec![staging_path.clone()],
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
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    if format {
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
    }
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![kv]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed.clone());
    if !format {
        // Remount shape: rebuild the allocator population from durable
        // maps (the recovery walk — what reclaims unpublished dests).
        for kv in &routed.volumes {
            ba.recover_active_blocks_v3(kv, &fs.router.backend_router)
                .await
                .expect("allocator recovery");
        }
    }

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs: Arc::new(fs),
        routed,
        req,
        backing,
        m,
        staging_path,
        _s: s,
    }
}

async fn make_harness(test_id: &str) -> H {
    make_harness_capped(test_id, None).await
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

/// Fresh striped fixture of `blocks` full blocks, drained + fsync'd
/// (the rewrite_shadow_tests fixture, verbatim premise checks).
async fn striped_fixture(h: &H, name: &str, blocks: u64, seed: u8) -> u64 {
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
    write_at(h, ino, 0, &pattern((blocks * FBS) as usize, seed)).await;
    fsync(h, ino).await;
    quiesce(h).await;
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.expect("meta");
    assert_eq!(meta.file_type, "striped", "fixture premise: striped");
    assert_eq!(
        meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0),
        blocks as usize,
        "fixture premise: every block mapped"
    );
    ino
}

/// [`striped_fixture`] with caller-provided content (the B4c-i pins
/// compare gap serves against the EXACT old image).
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

/// The DURABLE layout's block map (bypasses the RAM cache — the
/// swap-boundary instrument).
async fn durable_block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let bytes =
        h.fs.meta_backend
            .as_ref()
            .expect("backend")
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout exists");
    let layout: squeezefs::layout_wire::LayoutMetadata = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).expect("json layout")
    } else {
        bincode::deserialize(&bytes).expect("bincode layout")
    };
    layout.block_map.unwrap_or_default()
}

/// The RAM-authoritative map (what reads resolve through).
async fn ram_block_map(h: &H, ino: u64) -> std::collections::HashMap<u32, String> {
    let path = squeezefs::keys::inode_path(ino);
    (*h.fs
        .router
        .fetch_metadata(&path)
        .await
        .expect("meta")
        .block_map
        .expect("mapped"))
    .clone()
}

fn m64(v: &squeezefs::fuse_client::Align64<std::sync::atomic::AtomicU64>) -> u64 {
    v.load(Ordering::Relaxed)
}
fn feeds() -> u64 {
    m64(&METRICS.overlay_epoch_feeds)
}
fn fallbacks() -> u64 {
    m64(&METRICS.overlay_feed_fallbacks)
}
fn publishes() -> u64 {
    m64(&METRICS.overlay_publishes)
}
fn swaps() -> u64 {
    m64(&METRICS.rewrite_shadow_swaps)
}
fn open_epochs() -> u64 {
    m64(&METRICS.rewrite_shadow_open_epochs)
}
fn parked_bytes() -> u64 {
    m64(&METRICS.rewrite_shadow_parked_bytes)
}
fn terminal_frees() -> u64 {
    m64(&METRICS.block_free_reclaim_queued) + m64(&METRICS.block_free_reclaim_elided)
}
fn journal_entries() -> u64 {
    META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

/// Fast-settle online fsck options (the fsck_c9_tests pattern).
fn online_opts() -> FsckOptions {
    let mut o = FsckOptions::online();
    o.settle = std::time::Duration::from_millis(100);
    o
}

fn fsck_ctx(h: &H) -> FsckCtx {
    FsckCtx {
        meta: h.routed.clone(),
        router: h.fs.router.clone(),
        staging_dirs: vec![h.staging_path.clone()],
        expected_generation: None,
    }
}

// ---------------------------------------------------------------------------
// Hazard 1 — one displaced-old queue: only the epoch ever parks.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hazard1_single_displaced_park() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4b_hazard1").await;
    let ino = striped_fixture(&h, "f1", 2, 1).await;
    let map_a = ram_block_map(&h, ino).await;

    let (pb0, f0, fe0, ob0, open0) = (
        parked_bytes(),
        terminal_frees(),
        feeds(),
        open_epochs(),
        m64(&METRICS.overlay_open),
    );

    // Overwrite block 0 through the seam (whole block covered), then
    // settle: the §5.4 feed.
    let v1 = pattern(FBS as usize, 7);
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &v1)
        .await
        .expect("seam install");
    let close_owed =
        h.fs.test_settle_overlay_block(ino, 0, false)
            .await
            .expect("settle");
    assert!(
        !close_owed,
        "one fed block of a two-block file is not epoch coverage"
    );

    assert_eq!(feeds() - fe0, 1, "the settle FED the epoch (arm (a))");
    assert_eq!(open_epochs() - ob0, 1, "the feed opened/joined ONE epoch");
    assert_eq!(
        parked_bytes() - pb0,
        FBS,
        "hazard 1: EXACTLY one displaced-old park — the epoch's; a second \
         queue (an overlay-side park) would double this"
    );
    assert_eq!(
        terminal_frees() - f0,
        0,
        "the displaced key is PARKED, never freed before the durable swap \
         (§5.2 deferred-free law)"
    );
    assert_eq!(
        m64(&METRICS.overlay_open),
        open0,
        "the fed record retired (the Fed terminal)"
    );

    // RYW mid-epoch: the RAM map serves the dest.
    assert_eq!(
        read_at(&h, ino, 0, FBS as usize).await,
        v1,
        "RYW after feed"
    );

    // The close is the ONE authority: durable swap, then the single free.
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    h.fs.router
        .close_rewrite_epoch(ino, token)
        .await
        .expect("close");
    assert_eq!(terminal_frees() - f0, 1, "exactly ONE free after the swap");
    assert_eq!(parked_bytes() - pb0, 0, "park gauge returns");
    let durable = durable_block_map(&h, ino).await;
    assert_ne!(durable.get(&0), map_a.get(&0), "block 0 swapped durably");
    assert_eq!(durable.get(&1), map_a.get(&1), "block 1 untouched");
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v1, "durable read");
}

// ---------------------------------------------------------------------------
// Hazard 2 — no double free: kill the free twice; fsck C2/C3 clean.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hazard2_no_double_free() {
    let _g = serial().await;
    let _l = levers(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let h = make_harness("b4b_hazard2").await;
    let ino = striped_fixture(&h, "f1", 2, 2).await;

    let f0 = terminal_frees();
    let v1 = pattern(FBS as usize, 9);
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &v1)
        .await
        .expect("seam install");
    // Free path 1 candidate: the overlay teardown (the Fed disposition
    // must disarm WITHOUT freeing — B4a's pin, re-proven here in vivo).
    h.fs.test_settle_overlay_block(ino, 0, false)
        .await
        .expect("settle (feed + teardown)");
    assert_eq!(terminal_frees() - f0, 0, "teardown freed nothing");

    // Free path 2: the epoch close — run it TWICE (kill the free twice).
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    h.fs.router
        .close_rewrite_epoch(ino, token)
        .await
        .expect("close 1");
    assert_eq!(terminal_frees() - f0, 1, "the close frees exactly once");
    h.fs.router
        .close_rewrite_epoch(ino, token)
        .await
        .expect("close 2 (no epoch — must be a no-op)");
    // And a redundant drain of the (gone) record: a third no-op.
    h.fs.test_settle_overlay_block(ino, 0, false)
        .await
        .expect("settle of a retired record is a no-op");
    assert_eq!(
        terminal_frees() - f0,
        1,
        "hazard 2: killing the free twice must not free twice"
    );

    // fsck: no C2 (leaked/lost block), no C3 (refcount drift).
    fsync(&h, ino).await;
    quiesce(&h).await;
    let report = run_fsck(&fsck_ctx(&h), &online_opts())
        .await
        .expect("fsck run");
    assert!(
        report.findings.is_empty(),
        "hazard 2: fsck must be C2/C3-clean after feed+close (+ replayed \
         frees): {:?}",
        report.findings
    );
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v1);
}

// ---------------------------------------------------------------------------
// Hazard 3 — no KD-1.11 resurrection via the KD-1.9 refetch-compose.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hazard3_no_kd111_resurrection() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4b_hazard3").await;
    let ino = striped_fixture(&h, "f1", 2, 3).await;
    let map_a = ram_block_map(&h, ino).await;

    let v1 = pattern(FBS as usize, 11);
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &v1)
        .await
        .expect("seam install");
    h.fs.test_settle_overlay_block(ino, 0, false)
        .await
        .expect("settle (feed)");

    // The KD-1.9 refetch-compose: evict the RAM entry mid-epoch — the
    // fed binding must survive the backend refetch (the feed IS a
    // shadow record; there is no second map to resurrect from).
    h.fs.router.metadata_cache.remove(&ino);
    let ram = ram_block_map(&h, ino).await;
    assert_ne!(
        ram.get(&0),
        map_a.get(&0),
        "hazard 3: the refetch-compose must serve the FED binding, not \
         resurrect the displaced key"
    );
    assert_eq!(
        read_at(&h, ino, 0, FBS as usize).await,
        v1,
        "reads after eviction observe the fed binding (KD-1.9)"
    );

    // Close, then a SECOND overwrite cycle over the swapped map — the
    // freed key must never resurface through any compose.
    fsync(&h, ino).await;
    let v2 = pattern(FBS as usize, 13);
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &v2)
        .await
        .expect("second seam install");
    h.fs.test_settle_overlay_block(ino, 0, false)
        .await
        .expect("second settle (feed)");
    h.fs.router.metadata_cache.remove(&ino);
    assert_eq!(
        read_at(&h, ino, 0, FBS as usize).await,
        v2,
        "cycle 2: refetch-compose stays poison-free"
    );
    fsync(&h, ino).await;
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v2, "durable");
}

// ---------------------------------------------------------------------------
// Hazard 4 — ONE fsync authority: journal-entry equality vs the
// un-fed (accumulation) control — the law-7 pin.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hazard4_one_fsync_authority() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4b_hazard4").await;

    // The un-fed control must be SHAPE-IDENTICAL minus the feed, so
    // both legs ride the SAME vehicle (the seam) and only the shadow
    // lever differs: a FUSE-write control carries its own write-side
    // echo/attr commit, which is today's accumulation shape, not the
    // feed's doing (measured: real-write control = fed leg + 1, the
    // write's own traffic — the INVERSE of the hazard direction).

    // Leg A (overlay-FED): the fsync window contains the feed (RAM-only
    // — ZERO entries) + the one epoch-close save.
    let ino_a = striped_fixture(&h, "fa", 2, 4).await;
    let va = pattern(FBS as usize, 15);
    h.fs.test_install_overwrite_overlay(ino_a, 0, 0, &va)
        .await
        .expect("seam install");
    quiesce(&h).await;
    let ja0 = journal_entries();
    fsync(&h, ino_a).await;
    let ja = journal_entries() - ja0;

    // Leg B (the UN-FED control): the same seam overwrite with the
    // shadow lever OFF — the fsync window contains the degenerate's
    // ONE durable merge and a no-op close.
    squeezefs::routing::set_rewrite_shadow(false);
    let ino_b = striped_fixture(&h, "fb", 2, 5).await;
    let vb = pattern(FBS as usize, 17);
    h.fs.test_install_overwrite_overlay(ino_b, 0, 0, &vb)
        .await
        .expect("seam install (control)");
    quiesce(&h).await;
    let jb0 = journal_entries();
    fsync(&h, ino_b).await;
    let jb = journal_entries() - jb0;
    squeezefs::routing::set_rewrite_shadow(true);

    assert_eq!(
        ja, jb,
        "hazard 4: the overlay feed must add ZERO journal entries over \
         the un-fed control — one fsync authority, one save, the ref \
         deltas riding the SAME tx (law 7)"
    );
    assert!(ja >= 1, "premise: the window contains the one publish save");
    assert_eq!(read_at(&h, ino_a, 0, FBS as usize).await, va);
    assert_eq!(read_at(&h, ino_b, 0, FBS as usize).await, vb);
}

// ---------------------------------------------------------------------------
// Hazard 5 — zero durable-ref drift (the C8 oracle).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hazard5_ref_drift_zero() {
    let _g = serial().await;
    let _l = levers(true);
    // The C8 oracle seam: stamp incompat bit 8 at format so the durable
    // ledger grades this suite's publishes (never set in production).
    std::env::set_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS", "1");
    let h = make_harness("b4b_hazard5").await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS");
    let ino = striped_fixture(&h, "f1", 2, 6).await;
    let fb0 = fallbacks();

    // Two full overwrite-feed-close cycles on block 0.
    for seed in [19u8, 21u8] {
        let v = pattern(FBS as usize, seed);
        h.fs.test_install_overwrite_overlay(ino, 0, 0, &v)
            .await
            .expect("seam install");
        h.fs.test_settle_overlay_block(ino, 0, false)
            .await
            .expect("settle (feed)");
        fsync(&h, ino).await;
        assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v);
    }
    quiesce(&h).await;

    let drift =
        h.fs.router
            .backend_router
            .verify_durable_block_refs(&h.routed)
            .await
            .expect("C8 oracle pass");
    assert!(
        drift.is_empty(),
        "hazard 5: durable-vs-derived block-reference drift after \
         overlay-fed publishes must be ZERO (the feed notes Delete-old + \
         Put-new exactly once into the publish tx): {drift:?}"
    );
    assert_eq!(
        fallbacks() - fb0,
        0,
        "no fallbacks with the shadow lever ON"
    );
}

// ---------------------------------------------------------------------------
// The shadow-off degenerate: durable merge + displaced-free-after-publish.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shadow_off_degenerate_frees_displaced_only_after_durable_publish() {
    let _g = serial().await;
    let _l = levers(false); // SQUEEZEFS_REWRITE_SHADOW=0 — arm (b) as the degenerate
    let h = make_harness("b4b_degenerate").await;
    let ino = striped_fixture(&h, "f1", 2, 7).await;
    let map_a = ram_block_map(&h, ino).await;

    let (fe0, fb0, p0, s0, f0, rw0) = (
        feeds(),
        fallbacks(),
        publishes(),
        swaps(),
        terminal_frees(),
        m64(&METRICS.rewrite_blocks),
    );
    let v1 = pattern(FBS as usize, 23);
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &v1)
        .await
        .expect("seam install");
    assert_eq!(
        terminal_frees() - f0,
        0,
        "premise: nothing freed before the settle"
    );
    let close_owed =
        h.fs.test_settle_overlay_block(ino, 0, false)
            .await
            .expect("settle (degenerate durable merge)");
    assert!(!close_owed, "the durable-merge arm never owes a close");

    assert_eq!(feeds() - fe0, 0, "lever off ⇒ no feed");
    assert_eq!(
        fallbacks() - fb0,
        1,
        "the degenerate is COUNTED (overlay_feed_fallbacks — the A/B face)"
    );
    assert_eq!(publishes() - p0, 1, "one durable overlay publish");
    assert_eq!(swaps() - s0, 0, "no epoch, no swap");
    assert_eq!(
        rw0 + 1,
        m64(&METRICS.rewrite_blocks),
        "SLO attribution is vehicle-blind on the degenerate too (§8.2)"
    );

    // The publish is durable IMMEDIATELY (no epoch to wait on) and the
    // displaced key freed strictly AFTER it — both visible here.
    let durable = durable_block_map(&h, ino).await;
    assert_ne!(
        durable.get(&0),
        map_a.get(&0),
        "the durable map names the dest right after the settle"
    );
    assert_eq!(
        terminal_frees() - f0,
        1,
        "the displaced key freed exactly once, after the publish (the \
         upload path's non-shadow order)"
    );
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v1);
}

// ---------------------------------------------------------------------------
// The §5.4 venue law: the settle NEVER closes; the verdict travels.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_never_runs_under_block_guard() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4b_venue").await;

    // Half 1 — the raw verdict: feed both blocks of a 2-block file; the
    // second feed completes epoch coverage (close OWED), yet the settle
    // returns with the epoch STILL OPEN — the close belongs to the
    // guard-dropping caller (the `:14798` posture; the close is the
    // "worst park amplifier in the tree" and must never run under (3)).
    let ino = striped_fixture(&h, "f1", 2, 8).await;
    let s0 = swaps();
    let v = pattern((2 * FBS) as usize, 25);
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &v[..FBS as usize])
        .await
        .expect("install b0");
    let owed0 =
        h.fs.test_settle_overlay_block(ino, 0, false)
            .await
            .expect("settle b0");
    assert!(!owed0, "half coverage owes no close");
    h.fs.test_install_overwrite_overlay(ino, 1, 0, &v[FBS as usize..])
        .await
        .expect("install b1");
    let owed1 =
        h.fs.test_settle_overlay_block(ino, 1, false)
            .await
            .expect("settle b1");
    assert!(owed1, "full epoch coverage owes the close (KD-1.6)");
    assert_eq!(
        open_epochs(),
        1,
        "VENUE LAW: the settle returned with the epoch OPEN — it never \
         ran the close itself (under the caller's block guard)"
    );
    assert_eq!(swaps() - s0, 0, "no swap under the guard");
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    h.fs.router
        .close_rewrite_epoch(ino, token)
        .await
        .expect("the caller's close");
    assert_eq!(swaps() - s0, 1, "the hint-acting caller closed once");
    assert_eq!(read_at(&h, ino, 0, (2 * FBS) as usize).await, v);

    // Half 2 — an ACTING venue end-to-end: the read-path drain
    // (`drain_device_overlay_block`) settles under the guard, DROPS it,
    // then acts on the hint — a multi-block read of open overlays must
    // leave the epoch CLOSED with no fsync anywhere.
    let ino2 = striped_fixture(&h, "f2", 2, 9).await;
    let s1 = swaps();
    let v2 = pattern((2 * FBS) as usize, 27);
    h.fs.test_install_overwrite_overlay(ino2, 0, 0, &v2[..FBS as usize])
        .await
        .expect("install f2 b0");
    h.fs.test_install_overwrite_overlay(ino2, 1, 0, &v2[FBS as usize..])
        .await
        .expect("install f2 b1");
    let got = read_at(&h, ino2, 0, (2 * FBS) as usize).await;
    assert_eq!(got, v2, "the drain-serve is byte-exact");
    assert_eq!(
        swaps() - s1,
        1,
        "the read-drain venue ACTED on the close-owed hint after dropping \
         the guard (no fsync ran)"
    );
    assert_eq!(open_epochs(), 0, "no epoch left open");
    assert_eq!(m64(&METRICS.overlay_open), 0, "no record left open");
}

// ---------------------------------------------------------------------------
// R7 — ENOSPC convergence: the close recycles the displaced supply.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enospc_overwrite_loop_converges_via_epoch_close() {
    let _g = serial().await;
    let _l = levers(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    // Capacity 3 blocks: the 2-block fixture leaves ONE spare — every
    // overwrite cycle must mint from it and every close must return the
    // displaced block to the supply, or the loop starves (R7: on
    // overwrite rows the parked displaced set IS the free supply).
    let h = make_harness_capped("b4b_enospc", Some(3)).await;
    let ino = striped_fixture(&h, "f1", 2, 10).await;

    let f0 = terminal_frees();
    let mut last = Vec::new();
    for i in 0..4u8 {
        let v = pattern(FBS as usize, 29 + i);
        h.fs.test_install_overwrite_overlay(ino, 0, 0, &v)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "cycle {i}: the overwrite loop starved (spurious ENOSPC — \
                     the epoch close is not recycling displaced supply): {e:?}"
                )
            });
        h.fs.test_settle_overlay_block(ino, 0, false)
            .await
            .expect("settle (feed)");
        let token = h.fs.dlm().get_fencing_token_ino(ino);
        h.fs.router
            .close_rewrite_epoch(ino, token)
            .await
            .expect("close");
        last = v;
    }
    assert_eq!(
        terminal_frees() - f0,
        4,
        "every cycle freed exactly its displaced block"
    );
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, last, "converged");
}

// ---------------------------------------------------------------------------
// fsync ordering: the drain FEEDS, then the ONE close publishes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_drains_feeds_before_the_one_close() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4b_fsync").await;
    let ino = striped_fixture(&h, "f1", 2, 12).await;
    let map_a = ram_block_map(&h, ino).await;

    let (fe0, s0, p0, u0, f0) = (
        feeds(),
        swaps(),
        publishes(),
        m64(&METRICS.overlay_unpublished_at_fsync),
        terminal_frees(),
    );
    let v1 = pattern(FBS as usize, 31);
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &v1)
        .await
        .expect("seam install");

    // ONE fsync: drain (barrier=true) FEEDS the epoch, then the leg's
    // close publishes — strictly after the data barrier (O2).
    fsync(&h, ino).await;

    assert_eq!(feeds() - fe0, 1, "the fsync drain fed the record");
    assert_eq!(swaps() - s0, 1, "ONE close, ONE save (the one authority)");
    assert_eq!(
        publishes() - p0,
        0,
        "no overlay-owned durable publish on the fed path"
    );
    assert_eq!(
        m64(&METRICS.overlay_unpublished_at_fsync) - u0,
        0,
        "§6.2: a successful fsync leaves no overlay unpublished"
    );
    assert_eq!(terminal_frees() - f0, 1, "displaced freed after the swap");
    let durable = durable_block_map(&h, ino).await;
    assert_ne!(durable.get(&0), map_a.get(&0), "durable map = dest");
    assert_eq!(durable.get(&1), map_a.get(&1), "block 1 untouched");
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v1);
    assert_eq!(open_epochs(), 0);
    assert_eq!(m64(&METRICS.overlay_open), 0);
}

// ═══════════════════════════════════════════════════════════════════════
// PR B4c-i — read composition, the class-split foreign-merge hook, and
// the W1 clause (design-overlay-overwrite rev 4 §5.5/§5.6/§5.7/§5.8,
// KD-OV-13 live, KD-B4-11). Red-first: this section references the
// B4c-i counters (`overlay_gap_seed_old_bytes`, `overlay_read_gap_*`,
// `overlay_mover_skips`, `overlay_superseded_by_merge`,
// `patch_ineligible_device_overlay`) and `squeezefs::fsck::
// flip_mapping_damaged` — none exist/are visible until B4c-i lands.
// The gates stay CLOSED: every overwrite record enters through the B4b
// seam.
// ═══════════════════════════════════════════════════════════════════════

/// A `PayloadSink` over a locked Vec (the IPC arena-window stand-in for
/// the §5.5.1 sync-leg probes).
struct VecSink(std::sync::Mutex<Vec<u8>>);
impl VecSink {
    fn new(len: usize) -> Self {
        Self(std::sync::Mutex::new(vec![0u8; len]))
    }
}
impl squeezefs::PayloadSink for VecSink {
    fn write_at(&self, off: usize, bytes: &[u8]) {
        let mut v = self.0.lock().unwrap();
        let end = (off + bytes.len()).min(v.len());
        if off < end {
            v[off..end].copy_from_slice(&bytes[..end - off]);
        }
    }
    fn zero_at(&self, off: usize, len: usize) {
        let mut v = self.0.lock().unwrap();
        let end = (off + len).min(v.len());
        if off < end {
            v[off..end].fill(0);
        }
    }
}

fn trips() -> u64 {
    m64(&METRICS.invariant_tripwires)
}
fn superseded_by_merge() -> u64 {
    m64(&METRICS.overlay_superseded_by_merge)
}
fn mover_skips() -> u64 {
    m64(&METRICS.overlay_mover_skips)
}

// ---------------------------------------------------------------------------
// Issue-1 — the settle's OWN publish is provenance-exempt (KD-B4-11).
// ---------------------------------------------------------------------------

/// The hook must never fire on a healthy overlay publish: the FRESH arm
/// (product writes, overlay enabled) and the SHADOW-OFF DEGENERATE
/// (seam overwrite via the durable merge) both publish with
/// `invariant_tripwires` delta 0, `overlay_superseded_by_merge` delta 0,
/// the dest never freed, and the map resolving to the dest post-settle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_publish_never_trips_the_foreign_merge_hook() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4c_issue1").await;

    // Leg 1 — the FRESH arm: product writes ride the overlay.
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);
    let ino = striped_fixture(&h, "f1", 2, 40).await;
    let (t0, s0) = (trips(), superseded_by_merge());
    let v = pattern(FBS as usize, 41);
    write_at(&h, ino, 2 * FBS, &v).await; // fresh block 2
    fsync(&h, ino).await;
    assert_eq!(
        trips() - t0,
        0,
        "Issue-1: the fresh arm's own publish tripped the foreign-merge hook"
    );
    assert_eq!(
        superseded_by_merge() - s0,
        0,
        "no containment on the fresh arm"
    );
    assert_eq!(
        read_at(&h, ino, 2 * FBS, FBS as usize).await,
        v,
        "the dest was published (never freed) — fresh arm"
    );
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);

    // Leg 2 — the shadow-off degenerate: the durable merge is the
    // settle's own op and must be provenance-exempt too.
    squeezefs::routing::set_rewrite_shadow(false);
    let ino2 = striped_fixture(&h, "f2", 2, 42).await;
    let map_a = ram_block_map(&h, ino2).await;
    let (t1, s1, p0) = (trips(), superseded_by_merge(), publishes());
    let v2 = pattern(FBS as usize, 43);
    h.fs.test_install_overwrite_overlay(ino2, 0, 0, &v2)
        .await
        .expect("seam install");
    h.fs.test_settle_overlay_block(ino2, 0, false)
        .await
        .expect("settle (degenerate)");
    squeezefs::routing::set_rewrite_shadow(true);
    assert_eq!(
        trips() - t1,
        0,
        "Issue-1: the degenerate's own durable merge tripped the hook — \
         the containment supersede would free a durably-published dest \
         (the belt manufacturing KD-1.11)"
    );
    assert_eq!(superseded_by_merge() - s1, 0);
    assert_eq!(publishes() - p0, 1, "the degenerate published");
    let durable = durable_block_map(&h, ino2).await;
    assert_ne!(
        durable.get(&0),
        map_a.get(&0),
        "the map resolves to the dest post-settle"
    );
    assert_eq!(read_at(&h, ino2, 0, FBS as usize).await, v2);
}

// ---------------------------------------------------------------------------
// Law 5, overwrite edition + §5.6(1) — gaps compose from the OLD binding.
// ---------------------------------------------------------------------------

/// An open PARTIAL overwrite overlay serves covered pages from the dest
/// and uncovered gaps from the OLD BINDING — never the destination's
/// recycled device content (the free list is deliberately dirtied) and
/// never zeros (the old image is real durable data).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recycled_content_never_served_on_mapped_shape() {
    let _g = serial().await;
    let _l = levers(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let h = make_harness("b4c_law5").await;

    // Dirty the free supply: freed offsets carry 0xEE.
    let dirty =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new("dirty"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .unwrap()
        .attr
        .ino;
    write_at(&h, dirty, 0, &vec![0xEE; 3 * FBS as usize]).await;
    fsync(&h, dirty).await;
    h.fs.unlink(h.req, 1, std::ffi::OsStr::new("dirty"))
        .await
        .unwrap();

    let old = pattern((2 * FBS) as usize, 44);
    let ino = striped_fixture_with(&h, "f1", &old).await;

    // Interior segment: one PAGE of 0x77 at rel PAGE — gaps on BOTH sides.
    let (gs0, gb0) = (
        m64(&METRICS.overlay_read_gap_serves),
        m64(&METRICS.overlay_read_gap_bytes),
    );
    let seg = vec![0x77u8; PAGE as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, PAGE as usize, &seg)
        .await
        .expect("seam install (partial)");

    let got = read_at(&h, ino, 0, FBS as usize).await;
    assert_eq!(got.len(), FBS as usize);
    assert_eq!(
        &got[..PAGE as usize],
        &old[..PAGE as usize],
        "law 5 (overwrite edition): the PRE-segment gap must serve the \
         OLD BINDING's bytes — never zeros, never recycled dest content"
    );
    assert!(
        got[PAGE as usize..2 * PAGE as usize]
            .iter()
            .all(|&x| x == 0x77),
        "the covered page serves the dest"
    );
    assert_eq!(
        &got[2 * PAGE as usize..],
        &old[2 * PAGE as usize..FBS as usize],
        "the POST-segment gap serves the old binding too"
    );
    assert!(
        m64(&METRICS.overlay_read_gap_serves) > gs0,
        "gap-compose engagement counted"
    );
    assert_eq!(
        m64(&METRICS.overlay_read_gap_bytes) - gb0,
        FBS - PAGE,
        "gap bytes account exactly the uncovered range"
    );
    fsync(&h, ino).await;
}

// ---------------------------------------------------------------------------
// §5.8 — settle gap seeding from the OLD binding (fresh keeps zeros).
// ---------------------------------------------------------------------------

/// A PARTIAL overwrite overlay at a durability boundary seeds its
/// uncovered ranges from the OLD BINDING (counted
/// `overlay_gap_seed_old_bytes`, a subset of `overlay_gap_seed_bytes`) —
/// the durable post-feed image is old⊕new, never old⊕zeros (the B4b
/// zeros-seed would have DESTROYED the un-overwritten ranges).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_overwrite_settle_seeds_old_binding_bytes() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4c_seed").await;
    let old = pattern((2 * FBS) as usize, 45);
    let ino = striped_fixture_with(&h, "f1", &old).await;

    let (so0, sb0, fe0, wp0) = (
        m64(&METRICS.overlay_gap_seed_old_bytes),
        m64(&METRICS.overlay_gap_seed_bytes),
        feeds(),
        m64(&METRICS.write_path_seed_read_bytes),
    );
    let seg = vec![0x5Au8; PAGE as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, PAGE as usize, &seg)
        .await
        .expect("seam install (partial)");
    fsync(&h, ino).await;

    assert_eq!(feeds() - fe0, 1, "the partial overlay fed at fsync");
    assert_eq!(
        m64(&METRICS.overlay_gap_seed_old_bytes) - so0,
        FBS - PAGE,
        "§5.8: the seed pays exactly the gap bytes FROM THE OLD BINDING"
    );
    assert_eq!(
        m64(&METRICS.overlay_gap_seed_bytes) - sb0,
        FBS - PAGE,
        "the old-bytes face is a subset of the total seed face"
    );
    assert_eq!(
        m64(&METRICS.write_path_seed_read_bytes) - wp0,
        0,
        "the settle seed NEVER counts in the write-path seed tripwire (§11)"
    );

    // Durable image: old ⊕ new, byte-exact.
    h.fs.router.metadata_cache.remove(&ino);
    let got = read_at(&h, ino, 0, FBS as usize).await;
    assert_eq!(&got[..PAGE as usize], &old[..PAGE as usize]);
    assert!(got[PAGE as usize..2 * PAGE as usize]
        .iter()
        .all(|&x| x == 0x5A));
    assert_eq!(
        &got[2 * PAGE as usize..],
        &old[2 * PAGE as usize..FBS as usize]
    );

    // The FRESH shape keeps zeros (law 5): a partial fresh overlay's
    // fsync seeds zeros, not old (there is no old).
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);
    let (so1, sb1) = (
        m64(&METRICS.overlay_gap_seed_old_bytes),
        m64(&METRICS.overlay_gap_seed_bytes),
    );
    write_at(&h, ino, 2 * FBS + PAGE, &seg).await; // fresh block 2, interior
    fsync(&h, ino).await;
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    assert_eq!(
        m64(&METRICS.overlay_gap_seed_old_bytes) - so1,
        0,
        "fresh/hole records keep the zeros seed"
    );
    assert!(m64(&METRICS.overlay_gap_seed_bytes) - sb1 >= PAGE);
}

// ---------------------------------------------------------------------------
// R1 — warm tiers never serve stale under an open overwrite overlay
// (KD-OV-13 live: one case per fast arm on a tier-hot fixture).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_tier_never_serves_stale_under_open_overlay() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4c_warm").await;
    let old = pattern((2 * FBS) as usize, 46);
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let old_key = ram_block_map(&h, ino)
        .await
        .get(&0)
        .cloned()
        .expect("mapped");

    // Tier-hot fixture: the OLD key's bytes planted in the R4 hot tier
    // AND the read-lane hold (the stale bytes are RIGHT THERE — §5.6(2)).
    let old_block = bytes::Bytes::copy_from_slice(&old[..FBS as usize]);
    h.fs.router.cache.hot_block.put(&old_key, old_block.clone());
    h.fs.router
        .cache
        .read_lane_hold
        .insert(&old_key, old_block.clone(), u64::MAX);

    // Warm the attr cache (the IPC probe's size authority) with a real
    // read BEFORE the overlay exists.
    let _ = read_at(&h, ino, FBS, PAGE as usize).await;

    // The open overwrite overlay: whole block 0 rewritten to 0x9A.
    let newv = vec![0x9Au8; FBS as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &newv)
        .await
        .expect("seam install");

    // Arm 1 — the HANDLER ladder (hot-block fast hit + read-lane hold
    // serve sit inside it): the compose-first order at the read top is
    // the screen — an ACKed overlay byte must read back 0x9A, never the
    // planted hot/hold OLD bytes.
    let got = read_at(&h, ino, 0, FBS as usize).await;
    assert!(
        got.iter().all(|&x| x == 0x9A),
        "R1: the handler served stale warm-tier bytes under an open \
         overwrite overlay"
    );

    // Arm 2 — the IPC §5.5.1 SYNC fast path (staging/hot/hold/read-cache
    // legs behind `ipc_read_probe_locked`): with a live record on the
    // block it must DEMOTE (Miss → the handoff runs the composed path),
    // never serve the planted tiers.
    let sink = VecSink::new(PAGE as usize);
    let (probe, _) = h.fs.ipc_read_probe_locked(ino, 0, PAGE as u32, &sink);
    assert!(
        matches!(probe, squeezefs::fuse_client::IpcReadProbe::Miss),
        "R1: the IPC sync fast path must probe-or-demote under a live \
         overlay (KD-OV-13) — it answered {probe:?} with stale tiers hot"
    );

    // Arm 3 — the IPC direct-drive read probe (the B2 screen, re-pinned
    // on the WARM MAPPED population): Overlay-class refusal.
    let dd = h.fs.ipc_direct_read_probe(ino, 0, PAGE as u32, 0);
    assert!(
        matches!(
            dd,
            Err(squeezefs::fuse_client::IpcDirectIneligible::Overlay)
        ),
        "R1: the direct-drive probe must refuse Overlay-class under a \
         live record"
    );

    // Arm 4 — the dest-lease gate is the compose-first HANDLER order
    // (in-process reads carry no transport dest window, so the lease
    // hint is structurally false here; the parent §5.3 v1 gate = reads
    // reach the ladder only overlay-free, which arm 1 just proved).
    // Settle; the feed purges the displaced key's tiers (KD-B4-6).
    h.fs.test_settle_overlay_block(ino, 0, false)
        .await
        .expect("settle (feed)");
    assert!(
        h.fs.router.cache.hot_block.get(&old_key).is_none(),
        "KD-B4-6: the displaced key's hot entry purges at the feed"
    );
    let got = read_at(&h, ino, 0, FBS as usize).await;
    assert!(got.iter().all(|&x| x == 0x9A), "post-feed read exact");
    fsync(&h, ino).await;
}

// ---------------------------------------------------------------------------
// R3 — W1 clause 8: patch bytes are never lost to an overlay publish.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_bytes_never_lost_to_overlay_publish() {
    let _g = serial().await;
    let _l = levers(true);
    // The patch path is ON for this pin (levers() disables it).
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    let h = make_harness("b4c_patch").await;

    // Order A: overlay first, patch-shaped write second — clause 8
    // declines the patch (own bucket), the ladder settles the overlay
    // (one-authority screen) and the write proceeds on the published
    // dest: BOTH writes' bytes survive.
    let old = pattern((2 * FBS) as usize, 47);
    let ino = striped_fixture_with(&h, "fa", &old).await;
    let newv = vec![0xB1u8; FBS as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &newv)
        .await
        .expect("seam install");
    let c0 = m64(&METRICS.patch_ineligible_device_overlay);
    let patch = vec![0xC2u8; PAGE as usize];
    write_at(&h, ino, PAGE, &patch).await; // patch-shaped: aligned sub-block on a mapped striped block
    assert!(
        m64(&METRICS.patch_ineligible_device_overlay) > c0,
        "clause 8: a live device-overlay record must decline the patch \
         into its OWN ledger bucket"
    );
    fsync(&h, ino).await;
    quiesce(&h).await;
    let got = read_at(&h, ino, 0, FBS as usize).await;
    assert!(
        got[..PAGE as usize].iter().all(|&x| x == 0xB1),
        "order A: overlay bytes before the patch window survive"
    );
    assert!(
        got[PAGE as usize..2 * PAGE as usize]
            .iter()
            .all(|&x| x == 0xC2),
        "order A: the PATCH bytes survive the overlay publication (R3 — \
         the lost-update shape)"
    );
    assert!(
        got[2 * PAGE as usize..].iter().all(|&x| x == 0xB1),
        "order A: overlay bytes after the patch window survive"
    );

    // Order B: patch first (no overlay — the patch engages in place),
    // THEN a partial overwrite overlay: the §5.1 capture is CURRENT, so
    // gap serves include the PATCHED bytes.
    let old_b = pattern((2 * FBS) as usize, 48);
    let ino2 = striped_fixture_with(&h, "fb", &old_b).await;
    let pw0 = METRICS.patch_writes.load(Ordering::Relaxed);
    let patch2 = vec![0xC3u8; PAGE as usize];
    write_at(&h, ino2, 0, &patch2).await;
    quiesce(&h).await;
    assert!(
        METRICS.patch_writes.load(Ordering::Relaxed) > pw0,
        "order B premise: the first write PATCHED in place (no overlay yet)"
    );
    let seg = vec![0xD4u8; PAGE as usize];
    h.fs.test_install_overwrite_overlay(ino2, 0, 2 * PAGE as usize, &seg)
        .await
        .expect("seam install (partial, after patch)");
    let got = read_at(&h, ino2, 0, FBS as usize).await;
    assert!(
        got[..PAGE as usize].iter().all(|&x| x == 0xC3),
        "order B: the gap serve includes the PATCHED old bytes (§5.1 \
         capture currency)"
    );
    assert_eq!(
        &got[PAGE as usize..2 * PAGE as usize],
        &old_b[PAGE as usize..2 * PAGE as usize],
        "order B: unpatched gap serves the old image"
    );
    assert!(
        got[2 * PAGE as usize..3 * PAGE as usize]
            .iter()
            .all(|&x| x == 0xD4),
        "order B: the covered page serves the dest"
    );
    fsync(&h, ino2).await;
}

// ---------------------------------------------------------------------------
// R4 — the class-split hook: MergeExpected skips, discarding ops
// supersede, the mover probe defers, fsck's flip refuses.
// ---------------------------------------------------------------------------

/// `MergeExpected` under a live overlay SKIP-APPLIES (never supersedes
/// — a content-preserving mover carries no new bytes; superseding
/// would drop ACK-early acked custody without a fence): the mover's
/// quiesce probe defers first (layer 1), the primitive belt skips for
/// guard-less callers (layer 2), acked bytes survive publication, and
/// the mover converges by re-plan once the record fed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mover_merge_skips_open_overlay() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4c_mover").await;
    let old = pattern((2 * FBS) as usize, 49);
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let old_key = ram_block_map(&h, ino)
        .await
        .get(&0)
        .cloned()
        .expect("mapped");

    let newv = vec![0xA7u8; FBS as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &newv)
        .await
        .expect("seam install (ACK-early acked custody)");

    // Layer 1 — the mover quiesce probe: a live overlay block is NOT
    // quiescent (the entry defers; counted).
    let probe = h.fs.mover_quiesce_probe();
    let sk0 = mover_skips();
    assert!(
        !probe(ino, 0),
        "layer 1: the quiesce probe must defer a block with a live \
         device-overlay record"
    );
    assert!(mover_skips() > sk0, "counted at the probe layer");
    assert!(probe(ino, 1), "sibling blocks stay quiescent");

    // Layer 2 — the primitive belt (the guard-less MergeExpected shape):
    // the entry SKIPS (no displacement, map unchanged, record alive).
    let sk1 = mover_skips();
    let moved_key = format!("{old_key}#moved-elsewhere");
    let entries = [(0u32, old_key.clone(), moved_key.clone())];
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let displaced =
        h.fs.router
            .merge_block_mappings(
                ino,
                squeezefs::routing::BlockMapOp::MergeExpected(&entries),
                0,
                squeezefs::routing::LayoutFlip::KeepLayout,
                token,
            )
            .await
            .expect("MergeExpected under a live overlay must not error");
    assert!(
        !displaced.iter().any(|d| d == &old_key),
        "layer 2: the belt must SKIP-APPLY the overlaid index (a \
         supersede here would drop acked custody — the never-lossy law)"
    );
    assert!(mover_skips() > sk1, "counted at the belt layer too");
    assert_eq!(
        ram_block_map(&h, ino).await.get(&0),
        Some(&old_key),
        "the map is untouched by the skipped entry"
    );
    // The acked bytes survive: still served (compose), then published.
    assert!(
        read_at(&h, ino, 0, FBS as usize)
            .await
            .iter()
            .all(|&x| x == 0xA7),
        "acked bytes survive the mover pass"
    );
    fsync(&h, ino).await;
    assert!(
        read_at(&h, ino, 0, FBS as usize)
            .await
            .iter()
            .all(|&x| x == 0xA7),
        "acked bytes survive publication (the dest key won the map)"
    );

    // Convergence by re-plan: with the record fed and the map now
    // naming the dest, a re-captured MergeExpected APPLIES (the
    // ordinary expected-mismatch/skip machinery has nothing to skip).
    assert!(probe(ino, 0), "post-feed the block is quiescent again");
    let dest_key = ram_block_map(&h, ino).await.get(&0).cloned().expect("dest");
    assert_ne!(dest_key, old_key);
    fsync(&h, ino).await;
}

/// The layer-2 belt's ONE real guard-less client: fsck's
/// `flip_mapping_damaged` on an overlaid index returns `Ok(false)` —
/// the repair REFUSES the action rather than reporting success (a block
/// under a live overlay is about to be displaced anyway).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_damaged_flip_refuses_under_live_overlay() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4c_fsckflip").await;
    let old = pattern((2 * FBS) as usize, 50);
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let old_key = ram_block_map(&h, ino)
        .await
        .get(&0)
        .cloned()
        .expect("mapped");

    let newv = vec![0xE5u8; FBS as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &newv)
        .await
        .expect("seam install");

    let flipped = squeezefs::fsck::flip_mapping_damaged(&fsck_ctx(&h), ino, 0, &old_key)
        .await
        .expect("the flip itself must not error");
    assert!(
        !flipped,
        "fsck's damaged-flip under a live overlay must REFUSE (Ok(false) \
         — its own supersession-safety arm), never report success"
    );
    assert_eq!(
        ram_block_map(&h, ino).await.get(&0),
        Some(&old_key),
        "the mapping is untouched by the refused flip"
    );
    fsync(&h, ino).await;
    assert!(
        read_at(&h, ino, 0, FBS as usize)
            .await
            .iter()
            .all(|&x| x == 0xE5),
        "the acked overlay bytes published normally after the refusal"
    );
}

/// The truncate-belt shape: a DISCARDING op (`RemoveBlocks` /
/// `TruncateFrom`) reaching the primitive UN-DRAINED marks the record
/// Superseded (its data is being discarded — newest-wins), the caller's
/// detached retire converges `overlay_open` → 0, and no foreign-merge
/// tripwire fires (the discarding class is legitimate, not a bug).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discarding_op_undrained_supersedes_and_retires_detached() {
    let _g = serial().await;
    let _l = levers(true);
    let h = make_harness("b4c_truncbelt").await;
    let old = pattern((2 * FBS) as usize, 51);
    let ino = striped_fixture_with(&h, "f1", &old).await;

    let (t0, s0, open0) = (trips(), superseded_by_merge(), m64(&METRICS.overlay_open));
    let newv = vec![0xF2u8; FBS as usize];
    h.fs.test_install_overwrite_overlay(ino, 0, 0, &newv)
        .await
        .expect("seam install");
    assert_eq!(m64(&METRICS.overlay_open), open0 + 1);

    // The belt: the discarding primitive DIRECTLY (bypassing the entry
    // drains — the un-drained path the belt exists for).
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let idxs = [0u32];
    h.fs.router
        .merge_block_mappings(
            ino,
            squeezefs::routing::BlockMapOp::RemoveBlocks(&idxs),
            0,
            squeezefs::routing::LayoutFlip::KeepLayout,
            token,
        )
        .await
        .expect("the discarding op proceeds");

    assert!(
        superseded_by_merge() > s0,
        "the discarding-class belt marks Superseded (counted)"
    );
    assert_eq!(
        trips() - t0,
        0,
        "a discarding op is LEGITIMATE — never the foreign-merge tripwire"
    );
    // The caller's detached retire converges the gauge (bounded wait —
    // the retire is a spawned task).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while m64(&METRICS.overlay_open) > open0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        m64(&METRICS.overlay_open),
        open0,
        "the detached retire must converge overlay_open (a stranded \
         hook-marked record degrades any_open_fast mount-wide)"
    );
}

// ---------------------------------------------------------------------------
// §5.6(3) — compose vs close storm with the reclaim path hot.
// ---------------------------------------------------------------------------

/// Readers composing old-binding gaps race the settle/feed/close with
/// instant-reclaim frees: every read returns a CONSISTENT image (old
/// bytes in gaps, new in covered — before or after the publish), never
/// recycled content, never EIO for legal churn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compose_vs_close_storm_with_hot_reclaim() {
    let _g = serial().await;
    let _l = levers(true);
    squeezefs::block_reclaim::set_elision_class_all(true);
    let h = Arc::new(make_harness("b4c_storm").await);

    for round in 0..4u8 {
        let old = pattern((2 * FBS) as usize, 60 + round);
        let ino = striped_fixture_with(&h, &format!("f{round}"), &old).await;
        let seg = vec![0x40 + round; PAGE as usize];
        h.fs.test_install_overwrite_overlay(ino, 0, PAGE as usize, &seg)
            .await
            .expect("seam install (partial)");

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let h = h.clone();
                let old = old.clone();
                let stop = stop.clone();
                let want = 0x40 + round;
                tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        let got = read_at(&h, ino, 0, FBS as usize).await;
                        assert_eq!(got.len(), FBS as usize);
                        assert_eq!(
                            &got[..PAGE as usize],
                            &old[..PAGE as usize],
                            "storm: gap must read the old image (compose \
                             or post-publish), never recycled/zeros"
                        );
                        assert!(
                            got[PAGE as usize..2 * PAGE as usize]
                                .iter()
                                .all(|&x| x == want),
                            "storm: the covered page must read the dest"
                        );
                        assert_eq!(
                            &got[2 * PAGE as usize..],
                            &old[2 * PAGE as usize..FBS as usize]
                        );
                    }
                })
            })
            .collect();

        // Let the readers spin over the OPEN record, then settle (feed),
        // close (frees the displaced old key into the hot reclaim), and
        // keep reading across the freed-offset window.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        h.fs.test_settle_overlay_block(ino, 0, false)
            .await
            .expect("settle (feed)");
        let token = h.fs.dlm().get_fencing_token_ino(ino);
        h.fs.router
            .close_rewrite_epoch(ino, token)
            .await
            .expect("close");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            r.await.expect("reader task");
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// PR B4c-ii — the overwrite arm LIVE (design-overlay-overwrite rev 4
// §5.1 the ONE-3.5-section capture+install, §5.3 the two-half crash
// matrix, KD-B4-8/9). Red-first: this section references
// `SQUEEZEFS_OVERLAY_OVERWRITE` / `SQUEEZEFS_OVERLAY_CLOSE_BARRIER`
// (registry + tri-state readers + test pins), the §5.1/§11 counters
// (`overlay_overwrite_installs/_bytes`, `overlay_ineligible_shadow_bound`,
// `overlay_enospc_declines`) and the capture-stall seam — none exist
// until B4c-ii lands. The kill-9 half pins OW-1..OW-7 EXACTLY; kill-9
// greens never adjudicate OW-8 (§5.3) — OW-8 is the TEST-1 power-cut
// pin plus its OQ-5 green twin.
// ═══════════════════════════════════════════════════════════════════════

/// Bounded condition wait (the overlay_ack_early_tests helper): the
/// detached publish/feed is asynchronous by design.
async fn eventually(mut f: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !f() {
        assert!(
            std::time::Instant::now() < deadline,
            "eventually timed out: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Kill-9 teardown: EXPLICITLY drop every harness field (empirically,
/// dropping through a `..` rest-pattern left `routed` pinned past the
/// same-process flock-teardown bound on this suite's shape — the
/// explicit destructure releases within ms), returning the two backing
/// files for the session-2 remount.
fn dismantle(h: H) -> (NamedTempFile, NamedTempFile) {
    let H {
        fs,
        routed,
        backing,
        m,
        staging_path,
        _s,
        req,
    } = h;
    drop(routed);
    drop(staging_path);
    drop(_s);
    let _ = req;
    drop(fs);
    (backing, m)
}

/// The live-arm posture: overlay ON (Bytes vehicle for in-process
/// writes), the OVERWRITE lever ON, ACK-after-CQE (deterministic
/// coverage), patch OFF (whole-segment shapes stay on the overlay).
fn live_levers() -> LeverGuard {
    let g = levers(true);
    squeezefs::device_overlay::set_device_overlay_for_tests(true, true);
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(true);
    g
}

fn ow_installs() -> u64 {
    m64(&METRICS.overlay_overwrite_installs)
}
fn ow_bytes() -> u64 {
    m64(&METRICS.overlay_overwrite_bytes)
}

// ---------------------------------------------------------------------------
// Knob registry drift pins (ENG-10) + the A/B gate restored.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overwrite_knob_registry_defaults() {
    let _g = serial().await;
    assert_eq!(
        squeezefs::env_knobs::lookup("SQUEEZEFS_OVERLAY_OVERWRITE")
            .expect("registered (ENG-10)")
            .default,
        "on",
        "KD-B4-9 ON, FIELD-ADJUDICATED (2026-08-15, the B4e row — \
         .benchmarks/2026-08-15-overlay-b4-overwrite.md): the deciding \
         CPU-bound field venue won BOTH orders (32.1 vs 31.0 / 31.6 vs \
         30.9 GiB/s sustained) at HALF the daemon CPU (26.5-27.3 vs \
         52.7-53.1 jiffies/GiB), engagement exact (overwrite share 0.999, \
         nt_copy share 1.000 -> 0.001, rewrite_amp 1.0000, fallbacks/ \
         tripwires 0). The local device-bound zram-tcp venue prefers OFF \
         (control's BDP-depth pipelining wins there, -2 to -30% by qd) — \
         recorded on the registry line as the venue split; `0` is the \
         restored B2 control"
    );
    assert_eq!(
        squeezefs::env_knobs::lookup("SQUEEZEFS_OVERLAY_CLOSE_BARRIER")
            .expect("registered (ENG-10)")
            .default,
        "off",
        "OQ-5: the barrier-before-close lever is deliberately NOT taken \
         by default (the DUR-2 class ships in the rewrite program's own \
         non-fsync closes — B4d prices it)"
    );
}

/// The engagement face + the A/B gate: an aligned whole-block overwrite
/// of a mapped striped block rides the overlay (installs/bytes counted,
/// the feed follows, zero accumulation write-through); the lever OFF
/// restores the B2 fresh-only gate verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_mapped_overwrite_rides_the_overlay_end_to_end() {
    let _g = serial().await;
    let _l = live_levers();
    let h = make_harness("b4c2_live").await;
    let old = pattern((2 * FBS) as usize, 70).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let map_a = ram_block_map(&h, ino).await;

    let (i0, b0, fe0, wt0, t0) = (
        ow_installs(),
        ow_bytes(),
        feeds(),
        m64(&METRICS.write_through_blocks),
        trips(),
    );
    let v = vec![0x71u8; FBS as usize];
    write_at(&h, ino, 0, &v).await;
    eventually(
        || ow_installs() > i0 && feeds() > fe0,
        "the live overwrite must install an overwrite record and feed \
         the epoch (detached publish)",
    )
    .await;
    assert_eq!(
        ow_bytes() - b0,
        FBS,
        "overlay_overwrite_bytes counts at the store CQE — the \
         overwrite-arm subset of overlay_store_bytes (KD-B4-10)"
    );
    assert_eq!(
        m64(&METRICS.write_through_blocks) - wt0,
        0,
        "the merge-share collapse's micro face: the overwrite block \
         never rides the accumulation write-through"
    );
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v, "RYW");
    fsync(&h, ino).await;
    let durable = durable_block_map(&h, ino).await;
    assert_ne!(durable.get(&0), map_a.get(&0), "durably swapped");
    assert_eq!(durable.get(&1), map_a.get(&1), "block 1 untouched");
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v);
    assert_eq!(trips() - t0, 0, "no tripwire anywhere on the live path");

    // The A/B gate: lever OFF = the B2 fresh-only gate verbatim.
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(false);
    let (i1, wt1) = (ow_installs(), m64(&METRICS.write_through_blocks));
    let v2 = vec![0x72u8; FBS as usize];
    write_at(&h, ino, 0, &v2).await;
    fsync(&h, ino).await;
    quiesce(&h).await;
    assert_eq!(
        ow_installs() - i1,
        0,
        "lever OFF: the mapped decline is restored (the B2 gate)"
    );
    assert!(
        m64(&METRICS.write_through_blocks) > wt1,
        "lever OFF: the overwrite rode accumulation"
    );
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v2, "byte-exact");
}

/// §5.1's two named declines: a shadow-BOUND block declines loudly
/// (`overlay_ineligible_shadow_bound` — the accumulation path's
/// same-epoch re-rewrite owns that shape, OQ-3) and a `StorageFull`
/// dest mint DECLINES to accumulation (never errors the write —
/// KD-B4-8: the parked-A supply is exactly what the epoch's KD-1.7
/// early-close ladder recycles).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overwrite_install_declines_shadow_bound_and_enospc() {
    let _g = serial().await;
    // Leg 1 — shadow-bound: an epoch already binds the block.
    {
        let _l = live_levers();
        let h = make_harness("b4c2_shadowbound").await;
        let old = pattern((2 * FBS) as usize, 73).to_vec();
        let ino = striped_fixture_with(&h, "f1", &old).await;
        // Bind block 0 in an epoch via the ACCUMULATION feed (lever off
        // for one write).
        squeezefs::device_overlay::set_overlay_overwrite_for_tests(false);
        write_at(&h, ino, 0, &vec![0x74u8; FBS as usize]).await;
        quiesce(&h).await;
        assert!(open_epochs() > 0, "premise: the epoch binds block 0");
        squeezefs::device_overlay::set_overlay_overwrite_for_tests(true);

        let (i0, sb0) = (ow_installs(), m64(&METRICS.overlay_ineligible_shadow_bound));
        let v = vec![0x75u8; FBS as usize];
        write_at(&h, ino, 0, &v).await;
        fsync(&h, ino).await;
        quiesce(&h).await;
        assert_eq!(
            ow_installs() - i0,
            0,
            "a shadow-bound block must DECLINE the overlay (hazard 1: the \
             captured old_binding would be an unpublished B key the epoch \
             already owns)"
        );
        assert!(
            m64(&METRICS.overlay_ineligible_shadow_bound) > sb0,
            "counted decline (OQ-3's demand instrument)"
        );
        assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v, "byte-exact");
    }
    // Leg 2 — ENOSPC: capacity 2 blocks, both held by the fixture.
    {
        let _l = live_levers();
        squeezefs::block_reclaim::set_elision_class_all(true);
        let h = make_harness_capped("b4c2_enospc", Some(2)).await;
        let old = pattern((2 * FBS) as usize, 76).to_vec();
        let ino = striped_fixture_with(&h, "f1", &old).await;

        let (e0, i0) = (m64(&METRICS.overlay_enospc_declines), ow_installs());
        let v = vec![0x77u8; FBS as usize];
        // The dest mint MUST hit StorageFull (0 free blocks) — the write
        // still SUCCEEDS through accumulation + the epoch's ENOSPC
        // early-close ladder (R7).
        write_at(&h, ino, 0, &v).await;
        fsync(&h, ino).await;
        quiesce(&h).await;
        assert!(
            m64(&METRICS.overlay_enospc_declines) > e0,
            "the StorageFull mint must be a COUNTED decline, never an error"
        );
        assert_eq!(ow_installs() - i0, 0, "no record installed");
        assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v, "converged");
    }
}

// ---------------------------------------------------------------------------
// §5.1 — the ONE-3.5-section capture+install (the interleave pin).
// ---------------------------------------------------------------------------

/// A record can never be born with a displaced `old_binding`: the
/// capture and the install share one `INODE_META_LOCKS` section, so a
/// foreign durable merge PARKS on the section instead of interleaving
/// (driven through the capture-stall seam). The parked merge then runs
/// against the INSTALLED record and takes the §5.7 skip — never the
/// foreign-merge tripwire, never a stale birth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_and_install_share_one_meta_section() {
    let _g = serial().await;
    let _l = live_levers();
    let h = Arc::new(make_harness("b4c2_interleave").await);
    let old = pattern((2 * FBS) as usize, 78).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let old_key = ram_block_map(&h, ino)
        .await
        .get(&0)
        .cloned()
        .expect("mapped");

    // Stall INSIDE the capture+install section (after capture, before
    // install): 300 ms — the foreign merge below must WAIT it out.
    squeezefs::fuse_client::set_test_overlay_capture_stall_ms(300);
    let (t0, sk0) = (trips(), mover_skips());

    let writer = {
        let h = h.clone();
        tokio::spawn(async move {
            // PARTIAL overwrite (the record stays Open afterwards).
            let seg = vec![0x79u8; PAGE as usize];
            write_at(&h, ino, 0, &seg).await;
        })
    };
    // Give the writer time to ENTER the stalled section (bounded ramp —
    // the stall itself is the synchronization window).
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;

    // The foreign mover-shape merge: must PARK on the ino's meta section
    // until the capture+install completes, then SKIP (the §5.7 belt).
    let merge_t0 = std::time::Instant::now();
    let entries = [(0u32, old_key.clone(), format!("{old_key}#moved"))];
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    let displaced =
        h.fs.router
            .merge_block_mappings(
                ino,
                squeezefs::routing::BlockMapOp::MergeExpected(&entries),
                0,
                squeezefs::routing::LayoutFlip::KeepLayout,
                token,
            )
            .await
            .expect("foreign merge");
    let waited = merge_t0.elapsed();
    squeezefs::fuse_client::set_test_overlay_capture_stall_ms(0);
    writer.await.expect("writer");

    assert!(
        waited >= std::time::Duration::from_millis(150),
        "§5.1: the foreign merge must PARK on the ONE capture+install \
         meta section (waited only {waited:?} — the capture ran outside \
         the section, the stale-birth window is open)"
    );
    assert!(
        !displaced.iter().any(|d| d == &old_key),
        "the post-install merge SKIPPED the overlaid index"
    );
    assert!(mover_skips() > sk0, "the belt counted the skip");
    assert_eq!(trips() - t0, 0, "never the foreign-merge tripwire");
    assert_eq!(
        ram_block_map(&h, ino).await.get(&0),
        Some(&old_key),
        "the map is untouched — the record was born with the CURRENT \
         binding, not a displaced one"
    );
    // The record's capture is current: the gap read serves the OLD image
    // (a stale birth would read a moved/freed key).
    let got = read_at(&h, ino, 0, FBS as usize).await;
    assert!(got[..PAGE as usize].iter().all(|&x| x == 0x79));
    assert_eq!(&got[PAGE as usize..], &old[PAGE as usize..FBS as usize]);
    fsync(&h, ino).await;
}

// ---------------------------------------------------------------------------
// The kill-9 crash matrix — OW-1..OW-7 (§5.3 first half; OW-4 is the
// v3 torn-entry law, pinned by the KV suites — detected-and-ignored ≡
// OW-1 — and stated here rather than re-pinned).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill9_crash_matrix_ow1_to_ow7() {
    let _g = serial().await;
    let _l = live_levers();
    squeezefs::block_reclaim::set_elision_class_all(false);

    // Each case: session 1 shapes the crash point, DROP (kill-9), then
    // session 2 (remount + recovery walk) asserts the §5.3 outcome and
    // runs fsck (findings must be 0 — no leak, no double free).
    for case in ["ow1", "ow2", "ow3", "ow5", "ow6", "ow7"] {
        let backing = NamedTempFile::new().unwrap();
        std::fs::File::create(backing.path())
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        let m = NamedTempFile::new().unwrap();
        let old = pattern((2 * FBS) as usize, 80).to_vec();
        let newv = vec![0x8Cu8; FBS as usize];

        let (ino, expect_block0_new) = {
            // ONE volume identity across both sessions — the production
            // remount shape (KD-5: vol identity is durable; a remount
            // never changes it). Distinct per-session ids made the C8
            // oracle's counted census and the durable ledger disagree by
            // construction once the rung-10b default format stamped
            // bit 9 — a harness artifact, not a recovery bug.
            let h = make_harness_on(&format!("b4c2_{case}"), backing, m, true, None).await;
            let ino = striped_fixture_with(&h, "f1", &old).await;
            let expect_new = match case {
                // OW-1: ACKed, record OPEN (partial coverage), DMA'd —
                // map = old entirely at the crash.
                "ow1" => {
                    write_at(&h, ino, 0, &newv[..PAGE as usize]).await;
                    false
                }
                // OW-2: coverage complete, epoch FED, no durable save.
                "ow2" => {
                    write_at(&h, ino, 0, &newv).await;
                    eventually(|| open_epochs() > 0, "the detached feed").await;
                    false
                }
                // OW-3: intermediate save committed mid-epoch — the
                // persisted new binding is REAL (DMA-complete before the
                // feed, O1); its displaced old key is durably
                // unreferenced ⇒ recovery frees it.
                "ow3" => {
                    write_at(&h, ino, 0, &newv).await;
                    eventually(|| open_epochs() > 0, "the detached feed").await;
                    let token = h.fs.dlm().get_fencing_token_ino(ino);
                    h.fs.router
                        .persist_dirty_layout_if_needed(&squeezefs::keys::inode_path(ino), token)
                        .await
                        .expect("mid-epoch save");
                    true
                }
                // OW-5: swap durable (fsync close), displaced frees
                // enqueued but the reclaim may not have run — crash now.
                "ow5" => {
                    write_at(&h, ino, 0, &newv).await;
                    fsync(&h, ino).await;
                    true
                }
                // OW-6: FENCED pre-publish — the close publishes nothing
                // and frees nothing (W5); successor recovery owns all.
                // The GENUINE fence class (the D0 custody poison): a
                // stale token alone is a process-local rotation the
                // close now converges (rewrite_shadow_tests contract 4b).
                "ow6" => {
                    write_at(&h, ino, 0, &newv).await;
                    eventually(|| open_epochs() > 0, "the detached feed").await;
                    let f0 = terminal_frees();
                    let token = h.fs.dlm().get_fencing_token_ino(ino);
                    squeezefs::data_custody::poison("ow6: the D0 fence fired");
                    let res = h.fs.router.close_rewrite_epoch(ino, token).await;
                    squeezefs::data_custody::test_clear_poison();
                    assert!(
                        matches!(
                            res,
                            Err(squeezefs::error::SqueezefsError::WriterGuardFenced)
                        ),
                        "a fenced close must refuse loud in the fence's class: {res:?}"
                    );
                    assert_eq!(terminal_frees() - f0, 0, "W5: freed NOTHING");
                    false
                }
                // OW-7: mid-fsync — data barrier done, meta publish not.
                "ow7" => {
                    write_at(&h, ino, 0, &newv).await;
                    eventually(|| open_epochs() > 0, "the detached feed").await;
                    h.fs.router
                        .backend_router
                        .flush_data_devices()
                        .await
                        .expect("data barrier");
                    false
                }
                _ => unreachable!(),
            };
            quiesce(&h).await;
            // KILL-9: drop the daemon with whatever state the case left.
            let (backing, m) = dismantle(h);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let h2 = make_harness_on(&format!("b4c2_{case}"), backing, m, false, None).await;
            let got = read_at(&h2, ino, 0, FBS as usize).await;
            if expect_new {
                assert_eq!(
                    got, newv,
                    "{case}: a persisted binding is REAL (DMA-complete \
                     before any feed — O1)"
                );
            } else {
                assert_eq!(
                    &got[..],
                    &old[..FBS as usize],
                    "{case}: the OLD image survives byte-intact (never \
                     torn — nothing ever wrote the old offset, KD-B4-2)"
                );
            }
            assert_eq!(
                &read_at(&h2, ino, FBS, FBS as usize).await[..],
                &old[FBS as usize..],
                "{case}: block 1 untouched"
            );
            // No leak, no double free: exactly the referenced blocks are
            // allocated (the dest was reclaimed by the census unless a
            // durable save named it).
            assert_eq!(
                h2.fs.router.block_allocator.get_used_blocks(),
                2,
                "{case}: recovery census — referenced blocks only \
                 (unpublished dests free-listed, displaced olds freed \
                 exactly once)"
            );
            // The volume stays fully writable + fsck-clean.
            let report = run_fsck(&fsck_ctx(&h2), &online_opts())
                .await
                .expect("fsck");
            assert!(
                report.findings.is_empty(),
                "{case}: fsck findings must be 0 across the matrix: {:?}",
                report.findings
            );
            (ino, expect_new)
        };
        let _ = (ino, expect_block0_new);
    }
}

// ---------------------------------------------------------------------------
// OW-8 (§5.3 second half — POWER LOSS, the TEST-1 harness): kill-9
// greens never adjudicate this window.
// ---------------------------------------------------------------------------

/// The disclosure pin: an UNBARRIERED coverage-close of an overlay-fed
/// epoch on a volatile-cache device loses the dest bytes on power loss
/// — the range reads dest residue, exactly as OW-8 states (the DUR-2
/// class inherited verbatim from the shipped rewrite program). A
/// documentation pin: any accidental barrier-order change flips it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ow8_window_is_exactly_as_disclosed() {
    let _g = serial().await;
    let _l = live_levers();
    struct CutGuard;
    impl Drop for CutGuard {
        fn drop(&mut self) {
            squeezefs::dev_power_cut::clear_faults();
        }
    }
    let _cut = CutGuard;
    let h = make_harness("b4c2_ow8").await;
    let old = pattern((2 * FBS) as usize, 90).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;

    // Arm the DATA device only (meta rides its own file). The fixture is
    // already barriered (fsync above).
    let data_path = h.backing.path().to_path_buf();
    squeezefs::dev_power_cut::arm_power_cut(&data_path);

    // Whole-FILE overwrite: the second feed completes epoch coverage —
    // the detached publisher's close fires (KD-1.6), UNBARRIERED with
    // the OQ-5 lever off (the shipped default).
    let s0 = swaps();
    let newv = pattern((2 * FBS) as usize, 91).to_vec();
    write_at(&h, ino, 0, &newv[..FBS as usize]).await;
    write_at(&h, ino, FBS, &newv[FBS as usize..]).await;
    eventually(
        || swaps() > s0,
        "the coverage-triggered close must fire (unbarriered)",
    )
    .await;
    quiesce(&h).await;

    // POWER LOSS: revert everything un-barriered on the data device,
    // then remount.
    let reverted = squeezefs::dev_power_cut::power_cut(&data_path);
    assert!(reverted > 0, "premise: the dest bytes were volatile");
    squeezefs::dev_power_cut::clear_faults();
    let (backing, m) = dismantle(h);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let h2 = make_harness_on("b4c2_ow8_s2", backing, m, false, None).await;
    let got = read_at(&h2, ino, 0, (2 * FBS) as usize).await;
    assert_ne!(
        got, newv,
        "OW-8: the range must NOT read the new bytes (the map durably \
         names a dest whose bytes never left the volatile cache)"
    );
    assert_ne!(
        got, old,
        "OW-8: nor the old image (its key is durably unreferenced) — \
         dest residue, exactly the documented DUR-2 disclosure"
    );
}

/// The OQ-5 GREEN TWIN (lands with the disclosure pin so B4d's pricing
/// leg arrives with its correctness half written): the same sequence
/// behind `SQUEEZEFS_OVERLAY_CLOSE_BARRIER=1` barriers the data device
/// BEFORE the coverage close — the window closes, the range reads the
/// new bytes through the power cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oq5_barrier_before_close_closes_the_ow8_window() {
    let _g = serial().await;
    let _l = live_levers();
    squeezefs::device_overlay::set_overlay_close_barrier_for_tests(true);
    struct CutGuard;
    impl Drop for CutGuard {
        fn drop(&mut self) {
            squeezefs::dev_power_cut::clear_faults();
        }
    }
    let _cut = CutGuard;
    let h = make_harness("b4c2_oq5").await;
    let old = pattern((2 * FBS) as usize, 92).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let data_path = h.backing.path().to_path_buf();
    squeezefs::dev_power_cut::arm_power_cut(&data_path);

    let s0 = swaps();
    let newv = pattern((2 * FBS) as usize, 93).to_vec();
    write_at(&h, ino, 0, &newv[..FBS as usize]).await;
    write_at(&h, ino, FBS, &newv[FBS as usize..]).await;
    eventually(|| swaps() > s0, "the coverage close (barriered)").await;
    quiesce(&h).await;

    let _ = squeezefs::dev_power_cut::power_cut(&data_path);
    squeezefs::dev_power_cut::clear_faults();
    let (backing, m) = dismantle(h);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let h2 = make_harness_on("b4c2_oq5_s2", backing, m, false, None).await;
    assert_eq!(
        read_at(&h2, ino, 0, (2 * FBS) as usize).await,
        newv,
        "OQ-5 twin: the barrier-before-close lever must close the OW-8 \
         window (the new bytes survive the power cut)"
    );
}

// ---------------------------------------------------------------------------
// R1's B4c-ii gate: the generic/209 storm + serialized discriminator +
// the generic/551 sibling shape, with overwrite overlays ENGAGED.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gen209_storm_with_overwrite_overlays_engaged() {
    let _g = serial().await;
    let _l = live_levers();
    let h = Arc::new(make_harness("b4c2_209").await);
    let ino = striped_fixture_with(&h, "f1", &pattern((2 * FBS) as usize, 94)).await;

    const FILE: u64 = 2 * FBS;
    const PASSES: u8 = 8;
    let i0 = ow_installs();

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
                    if (off / PAGE).is_multiple_of(4) {
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
                    let off = ((end - PAGE) / PAGE) * PAGE;
                    let got = read_at(&h, ino, off, PAGE as usize).await;
                    let (cur_pass, cur_end) = *rx.borrow();
                    if cur_pass == pass {
                        for (i, &b) in got.iter().enumerate() {
                            let pos = off + i as u64;
                            assert!(
                                !(pos < cur_end.min(end) && b < pass),
                                "READER FOUND OLD BYTE {b} at {pos} (pass \
                                 {pass}) — generic/209 with overwrite \
                                 overlays engaged"
                            );
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
    assert!(
        ow_installs() > i0,
        "engagement: the storm must have minted overwrite overlays"
    );
    fsync(&h, ino).await;
    let fin = read_at(&h, ino, 0, FILE as usize).await;
    assert!(fin.iter().all(|&b| b == PASSES), "post-storm exact");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serialized_overwrite_discriminator_with_overlays() {
    let _g = serial().await;
    let _l = live_levers();
    let h = make_harness("b4c2_209det").await;
    let ino = striped_fixture_with(&h, "f1", &pattern((2 * FBS) as usize, 95)).await;
    // Page-by-page value 1 across both blocks, verifying after EVERY
    // write that all previously written pages still read 1 (a stale
    // seed source reverts a neighbor — the write-path bug class).
    for p in 0..(2 * FBS / PAGE) {
        write_at(&h, ino, p * PAGE, &vec![1u8; PAGE as usize]).await;
        let got = read_at(&h, ino, 0, ((p + 1) * PAGE) as usize).await;
        assert!(
            got.iter().all(|&b| b == 1),
            "a SERIALIZED overwrite reverted a neighbor after page {p} — \
             write-path seed-source bug (generic/209 discriminator)"
        );
    }
    fsync(&h, ino).await;
}

/// generic/551, the MAPPED population (the ack_early suite pins the
/// fresh-block shape): a single-block sibling rides the OVERWRITE
/// overlay; the straddler's slice settles it (one-authority screen) and
/// classifies its seed off a STALE size snapshot — the sibling's
/// published bytes must survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_aio_551_shape_on_the_mapped_population() {
    let _g = serial().await;
    let _l = live_levers();
    let h = make_harness("b4c2_551").await;
    let old = pattern((3 * FBS) as usize, 96).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;

    // Sibling A: aligned PAGE at block 1 rel PAGE — the overwrite
    // overlay owns the MAPPED block.
    let a = vec![0x5Au8; PAGE as usize];
    let i0 = ow_installs();
    write_at(&h, ino, FBS + PAGE, &a).await;
    assert!(ow_installs() > i0, "A must ride the overwrite overlay");

    // Straddler B (blocks 0→1) presented with the STALE snapshot.
    let token = h.fs.dlm().get_fencing_token_ino(ino);
    h.fs.write_file_staged(
        ino,
        FBS - PAGE,
        bytes::Bytes::from(vec![0x5Bu8; 2 * PAGE as usize]),
        3 * FBS, // the stale request-entry snapshot
        token,
    )
    .await
    .expect("straddler B");

    let back = read_at(&h, ino, FBS + PAGE, PAGE as usize).await;
    assert_eq!(back, a, "A's published overlay bytes survive (551)");
    // The block's UNTOUCHED ranges keep the old image (the overwrite
    // edition's twist: the complement is the OLD binding, never zeros).
    assert_eq!(
        &read_at(&h, ino, FBS + 2 * PAGE, PAGE as usize).await[..],
        &old[(FBS + 2 * PAGE) as usize..(FBS + 3 * PAGE) as usize],
        "the old image survives at the coverage complement"
    );
    fsync(&h, ino).await;
    assert_eq!(
        read_at(&h, ino, FBS + PAGE, PAGE as usize).await,
        a,
        "durable"
    );
    let bband = read_at(&h, ino, FBS - PAGE, 2 * PAGE as usize).await;
    assert_eq!(bband, vec![0x5Bu8; 2 * PAGE as usize], "B's bytes intact");
}

/// The 2026-08-14 stress_recycled_keys_v3 conviction, pinned as a law
/// storm (the in-tree stress suite runs default-ON as the broad
/// sentinel; this is the targeted schedule): a GUARD-LESS discarding
/// belt (`RemoveBlocks` — §5.7) racing a mid-flight settle can win the
/// record's terminal CAS AFTER the settle's publication transferred
/// dest ownership (feed/merge). The Superseded teardown must then
/// DISARM, never free — a freed live B key recycles and binds TWO
/// blocks to one offset (the KD-1.11 corruption the stress caught at
/// ~50 %). Law asserts: no two map entries ever share a key, fsck
/// stays clean, tripwires stay 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discarding_belt_racing_settle_never_double_owns() {
    let _g = serial().await;
    let _l = live_levers();
    squeezefs::block_reclaim::set_elision_class_all(true);
    let h = Arc::new(make_harness("b4c2_beltrace").await);

    for round in 0..12u8 {
        let old = pattern((2 * FBS) as usize, 100 + round).to_vec();
        let ino = striped_fixture_with(&h, &format!("f{round}"), &old).await;
        let v = vec![0xB0 + round; FBS as usize];
        h.fs.test_install_overwrite_overlay(ino, 0, 0, &v)
            .await
            .expect("seam install");

        // The race: the settle (feed) vs the guard-less discarding
        // primitive on the same index.
        let settle = {
            let h = h.clone();
            tokio::spawn(async move { h.fs.test_settle_overlay_block(ino, 0, false).await })
        };
        let discard = {
            let h = h.clone();
            tokio::spawn(async move {
                let token = h.fs.dlm().get_fencing_token_ino(ino);
                let idxs = [0u32];
                h.fs.router
                    .merge_block_mappings(
                        ino,
                        squeezefs::routing::BlockMapOp::RemoveBlocks(&idxs),
                        0,
                        squeezefs::routing::LayoutFlip::KeepLayout,
                        token,
                    )
                    .await
            })
        };
        let _ = settle.await.expect("settle task");
        let displaced = discard.await.expect("discard task").expect("discard op");
        // The primitive's contract: the CALLER frees the displaced keys
        // after the publish (every product discarding caller does —
        // punch/truncate); the belt-race law needs the same hygiene or
        // the pruned keys read as C2 leaks of the TEST's own making.
        for bk in displaced {
            let _ = h.fs.router.backend_router.free_block(&bk).await;
        }
        let token = h.fs.dlm().get_fencing_token_ino(ino);
        h.fs.router
            .close_rewrite_epoch(ino, token)
            .await
            .expect("close");

        // THE LAW: whatever interleaving won, no two blocks may ever
        // share one key (the double-owner shape), and the volume stays
        // fsck-clean with zero tripwires.
        let ram = ram_block_map(&h, ino).await;
        let mut seen = std::collections::HashSet::new();
        for (b, k) in &ram {
            assert!(
                seen.insert(k.clone()),
                "round {round}: blocks share one key (block {b} → {k}; \
                 map {ram:?}) — the double-owner corruption"
            );
        }
        // Convergence hygiene: the detached retires must close the gauge.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while m64(&METRICS.overlay_open) > 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            m64(&METRICS.overlay_open),
            0,
            "round {round}: records retired"
        );
    }
    assert_eq!(trips(), trips(), "sanity");
    let report = run_fsck(&fsck_ctx(&h), &online_opts()).await.expect("fsck");
    assert!(
        report.findings.is_empty(),
        "belt-race storm must leave the volume fsck-clean: {:?}",
        report.findings
    );
}

// ---------------------------------------------------------------------------
// DLM S11 rung 16 — the B4 §5.1 RANGE clause (KD-MW-12;
// `docs/design-full-multi-writer.md` §9.3 item 3 + PR-plan row 16): a
// block any live range grant does not solely cover is OVERLAY-INELIGIBLE,
// counted `overlay_ineligible_range_shared` — the W1 clause-7 twin
// (`patch_ineligible_range_shared`). The composed law this section pins
// red-first: a range-shared span refuses BOTH fast paths — patch AND
// overlay — so no fast path exists for a shared span and the write rides
// the CoW-rewrite + shipped-publish path, the only vehicle whose
// custody/publish laws handle sharing (rung 17's demotion/extent
// machinery). Grant shapes are minted directly through the DLM (the
// extent_patch_tests `exclusion_range_shared_custody` recipe): pre-17 the
// product acquire algebra keeps every live grant edge block-aligned, so
// these are the POST-demotion shapes the clause exists to guard.
// ---------------------------------------------------------------------------

fn ow_range_shared() -> u64 {
    m64(&METRICS.overlay_ineligible_range_shared)
}
fn patch_range_shared_refusals() -> u64 {
    m64(&METRICS.patch_ineligible_range_shared)
}

/// **The composed pin (the rung-16 charter's red-first):** under FOREIGN
/// byte-range custody NO fast path exists for the shared span — every
/// aligned write refuses its class's fast path in ITS OWN ledger bucket
/// and still lands byte-exact through the accumulation (CoW-rewrite)
/// path. Since finding 47 the two fast paths tile the sub-block
/// population at the W1 cap (`overlay_length_eligible` = the predicate-5
/// oversize verdict), so the composition is pinned per class: a SUB-CAP
/// write is W1's — clause 7 refuses it and the overlay is never reached
/// (the floor, `overlay_ineligible_sub_cap`); an ABOVE-CAP write is the
/// overlay's — the §5.1 range clause refuses it and W1 was never a
/// candidate (oversize). Neither class installs a record or patches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_shared_span_refuses_both_patch_and_overlay() {
    let _g = serial().await;
    let _l = live_levers();
    let h = make_harness("s11r16_composed").await;
    let old = pattern((2 * FBS) as usize, 0x21).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let size = old.len() as u64;
    let path = squeezefs::keys::inode_path(ino);

    // Whole-file custody and a byte range are mutually exclusive (S11):
    // drop the handler's cached lease, then take FOREIGN range custody
    // over one page of block 0.
    h.fs.invalidate_local_lease(ino);
    let foreign_client = DlmClient::new().unwrap();
    let foreign = foreign_client
        .acquire_lock(&path, Some((0, PAGE)), std::time::Duration::from_secs(5))
        .await
        .expect("foreign range custody");
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);
    let mut want = old[..FBS as usize].to_vec();
    let (sb0, t0) = (m64(&METRICS.overlay_ineligible_shadow_bound), trips());

    // ---- Above-cap class FIRST (its refusal parks RAM custody on block
    // 0, which the overlay's state screen declines ahead of the range
    // clause): the DERIVED cap (FBS/8 = 2 KiB) makes one page
    // overlay-class — the overlay is the candidate, W1 is oversize by
    // predicate 5.
    squeezefs::fuse_client::set_patch_max_bytes(squeezefs::fuse_client::derived_patch_max_bytes(
        FBS,
    ));
    let (p1, o1, ov0, i1, pw1) = (
        patch_range_shared_refusals(),
        ow_range_shared(),
        m64(&METRICS.patch_ineligible_oversize),
        ow_installs(),
        METRICS.patch_writes.load(Ordering::Relaxed),
    );
    let q = pattern(PAGE as usize, 0xC8);
    h.fs.write_file_staged(ino, 3 * PAGE, bytes::Bytes::from(q.clone()), size, token)
        .await
        .expect("above-cap write under foreign range custody must LAND (rewrite path)");
    want[3 * PAGE as usize..4 * PAGE as usize].copy_from_slice(&q);
    assert_eq!(
        ow_range_shared() - o1,
        1,
        "the B4 §5.1 range clause refused the overlay (counted in ITS ledger) — \
         the composed law: no fast path exists for a range-shared span"
    );
    assert_eq!(
        m64(&METRICS.patch_ineligible_oversize) - ov0,
        1,
        "W1 was never a candidate above the cap (predicate 5)"
    );
    assert_eq!(
        patch_range_shared_refusals() - p1,
        0,
        "clause 7 never evaluated"
    );
    assert_eq!(ow_installs() - i1, 0, "no overlay record installed");
    assert_eq!(
        METRICS.patch_writes.load(Ordering::Relaxed) - pw1,
        0,
        "no patch happened"
    );

    // ---- Sub-cap class: the patch ladder ARMED (the suite posture
    // disarms it) with a cap above the page — W1 is the candidate.
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    let (p0, o0, sc0, i0, pw0) = (
        patch_range_shared_refusals(),
        ow_range_shared(),
        m64(&METRICS.overlay_ineligible_sub_cap),
        ow_installs(),
        METRICS.patch_writes.load(Ordering::Relaxed),
    );
    let p = pattern(PAGE as usize, 0xC7);
    h.fs.write_file_staged(ino, 2 * PAGE, bytes::Bytes::from(p.clone()), size, token)
        .await
        .expect("sub-cap write under foreign range custody must LAND (rewrite path)");
    want[2 * PAGE as usize..3 * PAGE as usize].copy_from_slice(&p);
    assert_eq!(
        patch_range_shared_refusals() - p0,
        1,
        "W1 clause 7 refused the in-place patch (counted in its ledger)"
    );
    assert_eq!(
        m64(&METRICS.overlay_ineligible_sub_cap) - sc0,
        1,
        "the sub-cap write never reached the overlay's range clause — the \
         length floor owns that decline (finding 47)"
    );
    assert_eq!(
        ow_range_shared() - o0,
        0,
        "the range clause was never evaluated"
    );
    assert_eq!(ow_installs() - i0, 0, "no overlay record installed");
    assert_eq!(
        METRICS.patch_writes.load(Ordering::Relaxed) - pw0,
        0,
        "no patch happened"
    );
    assert_eq!(
        m64(&METRICS.overlay_ineligible_shadow_bound) - sb0,
        0,
        "the refusals are the RANGE clause / the floor, not the shadow-bound \
         bucket — the ledgers must not merge (predicate-rot detection)"
    );
    // Byte-exact under the holder's own verification (RYW), then durable.
    assert_eq!(
        read_at(&h, ino, 0, FBS as usize).await,
        want,
        "the refused-fast-path writes landed byte-exact via accumulation"
    );
    foreign.release().await.expect("release foreign");
    fsync(&h, ino).await;
    quiesce(&h).await;
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, want, "durable");
    assert_eq!(trips() - t0, 0, "no tripwire on the composed-refusal path");
}

/// **Classification correctness (rung 15 finding #2's lesson): own
/// custody never refuses itself.** The writer's OWN range grant covering
/// the whole block installs the overlay (the clause is a custody test,
/// not a range veto), and the SHIPPED whole-file-lease shape is
/// structurally inert — the counter cannot move (the dark-posture
/// structural pin; the fast-path tax row is the measured proof).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_covering_custody_never_fires_the_range_clause() {
    let _g = serial().await;
    let _l = live_levers();
    let h = make_harness("s11r16_own").await;
    let old = pattern((2 * FBS) as usize, 0x22).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let size = old.len() as u64;
    let path = squeezefs::keys::inode_path(ino);

    // Arm 1 — OWN range grant covering the whole block: the overlay
    // proceeds, the clause stays silent.
    h.fs.invalidate_local_lease(ino);
    let mine =
        h.fs.router
            .dlm
            .acquire_lock(&path, Some((0, FBS)), std::time::Duration::from_secs(5))
            .await
            .expect("own covering range");
    let token = mine.fencing_token();
    let (o0, i0) = (ow_range_shared(), ow_installs());
    let v = vec![0x91u8; FBS as usize];
    h.fs.write_file_staged(ino, 0, bytes::Bytes::from(v.clone()), size, token)
        .await
        .expect("own-covered overwrite");
    assert_eq!(
        ow_installs() - i0,
        1,
        "own covering custody: the overlay INSTALLS (a clause that refused \
         here would be the finding-#2 class — own custody refusing itself)"
    );
    assert_eq!(ow_range_shared() - o0, 0, "own grant never refuses itself");
    assert_eq!(read_at(&h, ino, 0, FBS as usize).await, v, "RYW");
    mine.release().await.expect("release own");
    fsync(&h, ino).await;
    quiesce(&h).await;

    // Arm 2 — the SHIPPED shape (a whole-file lease IS whole-inode
    // custody): structurally inert, counter frozen. Arm 1's fsync ran
    // the handler, which cached a whole-file lease — drop it first (two
    // whole-file EX grants conflict).
    h.fs.invalidate_local_lease(ino);
    let whole =
        h.fs.router
            .dlm
            .acquire_lock(&path, None, std::time::Duration::from_secs(5))
            .await
            .expect("whole-file lease");
    let token = whole.fencing_token();
    let (o1, i1) = (ow_range_shared(), ow_installs());
    let v2 = vec![0x92u8; FBS as usize];
    h.fs.write_file_staged(ino, FBS, bytes::Bytes::from(v2.clone()), size, token)
        .await
        .expect("whole-file-custody overwrite");
    assert_eq!(ow_installs() - i1, 1, "the shipped shape still overlays");
    assert_eq!(
        ow_range_shared() - o1,
        0,
        "dark posture: a whole-file lease can never fire the range clause"
    );
    assert_eq!(read_at(&h, ino, FBS, FBS as usize).await, v2, "RYW");
    whole.release().await.expect("release whole");
    fsync(&h, ino).await;
    quiesce(&h).await;
}

/// **Both overlay SHAPES are screened** (`design-full-multi-writer` §9.3
/// item 3 says *a block* — not *a mapped block*): the OVERWRITE shape
/// (mapped block 0) and the FRESH shape (unmapped block 1 — its eventual
/// whole-block publish covers every byte too, gap-seeds included) both
/// refuse under a foreign grant that straddles the two blocks, and both
/// writes land byte-exact and durable through accumulation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_range_refuses_fresh_and_overwrite_shapes_durably() {
    let _g = serial().await;
    let _l = live_levers();
    let h = make_harness("s11r16_shapes").await;
    // TWO mapped blocks (the striped-promotion floor of this harness);
    // block 2 stays FRESH (unmapped) — the second write grows into it
    // under the grant.
    let old = pattern((2 * FBS) as usize, 0x23).to_vec();
    let ino = striped_fixture_with(&h, "f1", &old).await;
    let path = squeezefs::keys::inode_path(ino);
    // Grow the file over block 2 SPARSE (the setattr handler owns the
    // size publish — `write_file_staged` alone never grows i_size, and
    // reads clamp to it), BEFORE custody changes hands.
    h.fs.setattr(
        h.req,
        ino,
        None,
        fuse3::SetAttr {
            size: Some(3 * FBS),
            ..Default::default()
        },
    )
    .await
    .expect("sparse grow over block 2");
    let size = 3 * FBS;
    h.fs.invalidate_local_lease(ino);

    // ONE foreign grant straddling the block-1/block-2 boundary: it
    // overlaps BOTH blocks' spans without covering either.
    let foreign_client = DlmClient::new().unwrap();
    let foreign = foreign_client
        .acquire_lock(
            &path,
            Some((2 * FBS - PAGE, 2 * FBS + PAGE)),
            std::time::Duration::from_secs(5),
        )
        .await
        .expect("foreign straddling range");
    let token = h.fs.router.dlm.get_fencing_token_ino(ino);

    let (o0, i0, t0) = (ow_range_shared(), ow_installs(), trips());
    // The OVERWRITE shape: whole-block aligned overwrite of mapped block 1.
    let v0 = vec![0x93u8; FBS as usize];
    h.fs.write_file_staged(ino, FBS, bytes::Bytes::from(v0.clone()), size, token)
        .await
        .expect("overwrite shape under foreign range");
    assert_eq!(
        ow_range_shared() - o0,
        1,
        "the OVERWRITE shape refused under the foreign straddling grant"
    );
    // The FRESH shape: an aligned write into unmapped block 2.
    let v1 = vec![0x94u8; PAGE as usize];
    h.fs.write_file_staged(ino, 2 * FBS, bytes::Bytes::from(v1.clone()), size, token)
        .await
        .expect("fresh shape under foreign range");
    assert_eq!(
        ow_range_shared() - o0,
        2,
        "the FRESH shape is screened by the SAME clause (its publish \
         covers every byte of the block too)"
    );
    assert_eq!(ow_installs() - i0, 0, "no overlay record on either shape");
    foreign.release().await.expect("release foreign");
    fsync(&h, ino).await;
    quiesce(&h).await;
    assert_eq!(
        read_at(&h, ino, FBS, FBS as usize).await,
        v0,
        "block 1 durable"
    );
    assert_eq!(
        read_at(&h, ino, 2 * FBS, PAGE as usize).await,
        v1,
        "block 2 durable"
    );
    assert_eq!(trips() - t0, 0, "no tripwire anywhere");
}
