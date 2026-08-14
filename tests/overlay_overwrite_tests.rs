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
    _backing: NamedTempFile,
    _m: NamedTempFile,
    staging_path: std::path::PathBuf,
    _s: tempfile::TempDir,
}

async fn make_harness_capped(test_id: &str, capacity_blocks: Option<u64>) -> H {
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
    fs.meta_backend = Some(routed.clone());

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
        _backing: backing,
        _m: m,
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
    let pw0 = m64(&METRICS.patch_writes);
    let patch2 = vec![0xC3u8; PAGE as usize];
    write_at(&h, ino2, 0, &patch2).await;
    quiesce(&h).await;
    assert!(
        m64(&METRICS.patch_writes) > pw0,
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
