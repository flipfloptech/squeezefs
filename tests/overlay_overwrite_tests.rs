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

const FBS: u64 = 4096;

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

    // Leg A (overlay-fed): overwrite block 0 through the seam; the
    // fsync window contains the feed (RAM-only — ZERO entries) + the
    // one epoch-close save.
    let ino_a = striped_fixture(&h, "fa", 2, 4).await;
    let va = pattern(FBS as usize, 15);
    h.fs.test_install_overwrite_overlay(ino_a, 0, 0, &va)
        .await
        .expect("seam install");
    quiesce(&h).await;
    let ja0 = journal_entries();
    fsync(&h, ino_a).await;
    let ja = journal_entries() - ja0;

    // Leg B (the un-fed control): the SAME overwrite via the ordinary
    // accumulation write-through (whose publish feeds the epoch on the
    // pipeline's own arm) — then the same fsync-window measurement.
    let ino_b = striped_fixture(&h, "fb", 2, 5).await;
    let vb = pattern(FBS as usize, 17);
    write_at(&h, ino_b, 0, &vb).await;
    quiesce(&h).await;
    let jb0 = journal_entries();
    fsync(&h, ino_b).await;
    let jb = journal_entries() - jb0;

    assert_eq!(
        ja, jb,
        "hazard 4: the overlay feed must add ZERO journal entries over \
         the un-fed control — one fsync authority, one save, the ref \
         deltas riding the SAME tx (law 7)"
    );
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
    assert_eq!(m64(&METRICS.overlay_feed_fallbacks), 0, "no fallbacks");
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
