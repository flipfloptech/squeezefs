//! The cold-stream read lane (2026-08-01 campaign — `src/read_lane.rs`,
//! design amendment docs/design-read-path.md §5.5 / §Observability;
//! evidence `.benchmarks/2026-08-01-read-lane.md`).
//!
//! The field conviction these contracts pin closed (fio gap accounting
//! 2026-07-31 §6.2 + this campaign's instrumentation rows): FS reads at
//! the EXA validation shape ran 0.49–0.53× the matched-shape raw read
//! ceiling because (a) beyond-budget cold streams ride the R2
//! pipeline's ZERO-RESIDENT-SHARE regime — `prefetch_issued` = 114
//! against 1.5 M ops at 37 active streams — so every stream serializes
//! on one whole-block fabric RTT per block, and (b) deeper client qd
//! BREAKS single-flight cohorts (read_amp 1.05 → 1.41 from qd8 → qd32:
//! same-block stragglers miss the flight window, lose the hot-probation
//! clock race, and refetch whole blocks).
//!
//! The contracts:
//! 1. **Engagement**: on a classified stream in the zero-share regime
//!    the lane issues pipelined whole-block fetches (R2 stays declined:
//!    `prefetch_issued` = 0) with no double-fetch (single-flight
//!    dedupe: `get_obj` ≈ blocks).
//! 2. **Ledger invisibility**: lane fills never publish to the NVMe
//!    tier, never enter the hot tier, never record ghost heat, and
//!    never touch the admission governor's ledger — the 2026-07-26
//!    scan-resistance verdict stands.
//! 3. **Cohort stability**: a same-block sub-read that lost the
//!    single-flight window AND the hot-probation clock race serves from
//!    the hold instead of refetching (the qd32 read_amp fix), and
//!    coverage retirement returns the hold to empty — memory converges
//!    by consumption.
//! 4. **Depth derivation**: no fixed depth anywhere — floor 2/stream,
//!    BDP-derived, budget-capped, Red ⇒ 0 (senior to the measurement
//!    pin).
//! 5. **R5 Red**: no new speculation, no new deposits; in-flight and
//!    hold gauges converge.
//! 6. **Lever-off (`SQUEEZEFS_READ_LANE=0`) = exact prior behavior**:
//!    the A0 attribution control — the qd32-class refetch reappears and
//!    every `read_lane_*` counter stays 0.
//! 7. **Purge integration**: overwritten blocks never serve stale from
//!    the hold (the R-6 law).
//!
//! Counter-asserting phases take deltas within each test (METRICS is
//! process-global; the suite runs under `--test-threads=1` per the
//! house gate).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::read_lane::{
    hold_budget_bytes, lane_issue_admits, read_lane_depth_blocks, ReadLaneHold,
};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};

const BS: u64 = 524_288;

// ---------------------------------------------------------------------------
// Contract 4 — depth derivation tables (pure; the no-constants law).
// ---------------------------------------------------------------------------

#[test]
fn depth_derivation_tables() {
    let gib = 1024 * 1024 * 1024u64;
    let bs = 4 * 1024 * 1024u64;

    // Ahead-issue is OPT-IN (default 0): the campaign brackets
    // falsified both adaptive derivations (pure-BDP = the write
    // campaign's self-fulfilling equilibrium; the AIMD window = -19%
    // vs the hold alone on demand-concurrent venues — ahead-fetches
    // died FIFO-unconsumed racing the cohort). The pin remains for
    // high-latency/low-qd fabrics.
    assert_eq!(read_lane_depth_blocks(None, bs, 32, false, 64 * gib), 0);
    assert_eq!(read_lane_depth_blocks(None, bs, 1, false, 64 * gib), 0);

    // The pin wins verbatim (0 = no ahead issue).
    assert_eq!(read_lane_depth_blocks(Some(7), bs, 32, false, 64 * gib), 7);
    assert_eq!(read_lane_depth_blocks(Some(0), bs, 32, false, 64 * gib), 0);

    // The R5 budget cap is senior to the pin: a cap that holds 3
    // blocks/stream clamps a deeper pin to 3; a cap that cannot hold
    // even ONE block per stream never speculates.
    assert_eq!(read_lane_depth_blocks(Some(16), bs, 2, false, 6 * bs), 3);
    assert_eq!(read_lane_depth_blocks(Some(16), bs, 32, false, 16 * bs), 0);

    // Red stops speculation outright and is SENIOR to the pin (the
    // write-pipeline Red-clamp precedent).
    assert_eq!(read_lane_depth_blocks(Some(8), bs, 32, true, 64 * gib), 0);

    // Zero streams behaves as one (defensive).
    assert_eq!(read_lane_depth_blocks(Some(2), bs, 0, false, 64 * gib), 2);

    // Issue admission: per-lane depth bound AND the aggregate cap.
    assert!(lane_issue_admits(1, 2, 0, bs, 64 * gib));
    assert!(!lane_issue_admits(2, 2, 0, bs, 64 * gib), "depth bound");
    assert!(
        !lane_issue_admits(0, 2, 63 * gib + gib, bs, 64 * gib - bs + 1),
        "aggregate R5 cap bounds issue"
    );

    // Hold budget: the live consume-behind window (4 x streams x depth
    // blocks — cohort span + FIFO-skew margin), floored at 4 blocks,
    // capped by the R5 share. Round-2 field lesson: a flat mem/8
    // budget pinned 23.6 GiB of FIFO churn; round-4: a 2x window still
    // evicted half the deposits unconsumed under 38-stream FIFO skew.
    assert_eq!(hold_budget_bytes(80 * gib, bs, 32, 16), 4 * 32 * 16 * bs);
    assert_eq!(hold_budget_bytes(80 * gib, bs, 1, 2), 8 * bs);
    assert_eq!(
        hold_budget_bytes(80 * gib, bs, 1, 0),
        4 * bs,
        "4-block floor"
    );
    assert_eq!(
        hold_budget_bytes(64 * bs * 8, bs, 64, 64),
        64 * bs,
        "the R5 share caps the consume window"
    );
    assert_eq!(
        hold_budget_bytes(0, bs, 1, 2),
        4 * bs,
        "floor survives a zero budget"
    );
}

// ---------------------------------------------------------------------------
// Hold-store unit semantics (coverage retirement, FIFO trim, purge,
// exact gauge accounting).
// ---------------------------------------------------------------------------

#[test]
fn hold_store_unit_semantics() {
    let hold = ReadLaneHold::new();
    let blk = bytes::Bytes::from(vec![7u8; BS as usize]);
    let budget = 2 * BS; // two entries

    let retired0 = METRICS.read_lane_hold_retired.load(Ordering::Relaxed);
    let evicted0 = METRICS
        .read_lane_hold_evicted_unconsumed
        .load(Ordering::Relaxed);

    // Insert + duplicate keeps the first (no double-charge).
    hold.insert("k1", blk.clone(), budget);
    hold.insert("k1", blk.clone(), budget);
    assert_eq!(hold.bytes(), BS, "duplicate insert must not double-charge");
    assert!(hold.contains("k1"));

    // Zero-credit serves never retire.
    for _ in 0..8 {
        assert!(hold.serve("k1", 0).is_some());
    }
    assert!(hold.contains("k1"), "zero-credit serves must not retire");

    // Coverage retirement: 4 x 128 KiB serves retire the 512 KiB entry
    // exactly at the boundary crossing.
    for i in 0..4u64 {
        let served = hold.serve("k1", BS / 4);
        assert!(served.is_some(), "serve {i} must hit");
    }
    assert!(!hold.contains("k1"), "full coverage retires");
    assert_eq!(hold.bytes(), 0, "gauge returns to zero on retirement");
    assert_eq!(
        METRICS.read_lane_hold_retired.load(Ordering::Relaxed) - retired0,
        1
    );

    // Credit-only path retires too (primary-slice / hot-serve credits).
    hold.insert("k2", blk.clone(), budget);
    hold.credit("k2", BS / 2);
    hold.credit("k2", BS / 2);
    assert!(!hold.contains("k2"), "credit coverage retires");
    assert_eq!(hold.bytes(), 0);

    // FIFO trim: budget for 2, insert 3 => the OLDEST evicts, counted
    // unconsumed; gauge stays exact.
    hold.insert("a", blk.clone(), budget);
    hold.insert("b", blk.clone(), budget);
    hold.insert("c", blk.clone(), budget);
    assert_eq!(hold.bytes(), 2 * BS, "trim holds the budget");
    assert!(!hold.contains("a"), "oldest-first eviction");
    assert!(hold.contains("b") && hold.contains("c"));
    assert_eq!(
        METRICS
            .read_lane_hold_evicted_unconsumed
            .load(Ordering::Relaxed)
            - evicted0,
        1,
        "an unconsumed eviction is the spiral detector's signal"
    );

    // Purge is unconditional and exact; serves after purge miss.
    hold.purge("b");
    assert_eq!(hold.bytes(), BS);
    assert!(hold.serve("b", BS).is_none());

    // Shed hook: trim_to(0) empties (unconsumed evictions counted).
    hold.trim_to(0);
    assert_eq!(hold.bytes(), 0);

    // Hold-budget cache hysteresis (round-4 flap lesson): fast-up,
    // 1/8-per-epoch down — a pass-boundary lane claim at window 2 must
    // not trim a healthy multi-GiB hold.
    let gov = squeezefs::read_lane::ReadLaneGovernor::from_env();
    gov.set_hold_budget_at(8 * BS, 10_000);
    assert_eq!(gov.hold_budget(), 8 * BS, "fast up");
    gov.set_hold_budget_at(BS, 10_500);
    assert_eq!(gov.hold_budget(), 8 * BS, "no shrink within the epoch");
    gov.set_hold_budget_at(BS, 12_500);
    assert_eq!(gov.hold_budget(), 7 * BS, "1/8 decay per epoch");
    gov.set_hold_budget_at(16 * BS, 12_600);
    assert_eq!(gov.hold_budget(), 16 * BS, "fast up again");
}

// ---------------------------------------------------------------------------
// Fixture (the read_stream_transient_tests shape: 512 KiB blocks;
// `staging` selects cache-ful — the NVMe-tier-visible posture the
// ledger-invisibility contract needs — vs cache-less, the field
// posture).
// ---------------------------------------------------------------------------

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<TempDir>,
}

async fn make_with(uuid: [u8; 16], alloc_ns: &str, staging: bool) -> H {
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", "524288");
    let dlm = DlmClient::new().unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = if staging {
        Some(TempDir::new().unwrap())
    } else {
        None
    };
    let staging_dirs = s
        .as_ref()
        .map(|d| vec![d.path().to_path_buf()])
        .unwrap_or_default();
    let cache = TieredCache::new(
        staging_dirs,
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
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_5EA5_1DE5,
            uuid,
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
    h.fs.read(h.req, ino, 0, off, size, libc::O_DIRECT as u32)
        .await
        .unwrap()
        .data
        .to_vec()
}

async fn block_map_of(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router
        .fetch_metadata(&path)
        .await
        .unwrap()
        .block_map
        .unwrap_or_default()
}

async fn make_cold(h: &H, ino: u64) -> std::sync::Arc<std::collections::HashMap<u32, String>> {
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();
    let map = block_map_of(h, ino).await;
    assert!(!map.is_empty(), "fixture must promote to striped");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }
    map
}

fn tier_has(h: &H, key: &str) -> bool {
    h.fs.router.cache.nvme.get_cached_read_block(key).is_some()
}

/// One CONTIGUOUS sequential pass in 128 KiB requests (4 per block) —
/// classifies at request 4 and stays classified.
async fn stream_pass(h: &H, ino: u64, blocks: u64, expect: impl Fn(u64) -> u8) {
    let req = 128 * 1024u64;
    for i in 0..(blocks * BS / req) {
        let off = i * req;
        let d = read_at(h, ino, off, req as u32).await;
        let b = off / BS;
        assert!(
            d.iter().all(|&x| x == expect(b)),
            "byte parity at block {b} offset {off}"
        );
    }
}

async fn striped_file(
    h: &H,
    name: &str,
    blocks: u64,
) -> (u64, std::sync::Arc<std::collections::HashMap<u32, String>>) {
    let ino = create(h, name).await;
    for b in 0..blocks {
        write_at(h, ino, b * BS, &vec![(b % 250) as u8 + 1; BS as usize]).await;
    }
    let map = make_cold(h, ino).await;
    (ino, map)
}

fn get_obj() -> u64 {
    METRICS.get_obj.load(Ordering::Relaxed)
}
fn lane_fetches() -> u64 {
    METRICS.read_lane_fetches.load(Ordering::Relaxed)
}
fn lane_serve_bytes() -> u64 {
    METRICS.read_lane_serve_bytes.load(Ordering::Relaxed)
}

/// Zero-share env posture: hot tier 1 MiB (2 slots) + prefetch share
/// 1 % => R2's `resident_share` truncates to 0 (the field regime) while
/// the classifier still classifies. Ranged dispatch is killed
/// (threshold 0) for SHAPE PARITY: the field rows' 1 MiB requests are
/// 4x the 256 KiB threshold, but this fixture's 512 KiB blocks force
/// 128 KiB sub-reads, which would otherwise ride R3 windows instead of
/// the whole-block path under test.
fn set_zero_share_env() {
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1");
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_SHARE_PCT", "1");
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
}
fn clear_zero_share_env() {
    std::env::remove_var("SQUEEZEFS_READ_TIER_ADMISSION");
    std::env::remove_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB");
    std::env::remove_var("SQUEEZEFS_READ_PREFETCH_SHARE_PCT");
    std::env::remove_var("SQUEEZEFS_READ_RANGED_THRESHOLD");
}
/// Ahead-issue contracts additionally pin the opt-in depth (the
/// shipped default is hold-only — ahead-issue was falsified on
/// demand-concurrent venues and remains the high-latency-fabric
/// lever).
fn set_ahead_env() {
    std::env::set_var("SQUEEZEFS_READ_LANE_DEPTH", "4");
}
fn clear_ahead_env() {
    std::env::remove_var("SQUEEZEFS_READ_LANE_DEPTH");
}

// ---------------------------------------------------------------------------
// Contracts 1 + 2 — engagement in the zero-share regime, ledger
// invisibility (cache-FUL fixture so the NVMe tier is real), fetch
// economy, and hold engagement accounting.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lane_engages_on_zero_share_streams_and_stays_ledger_invisible() {
    set_zero_share_env();
    set_ahead_env();
    let h = make_with(*b"read-lane-test01", "rdlane_ns_a", true).await;
    clear_ahead_env();
    clear_zero_share_env();

    let blocks = 16u64;
    let (ino, map) = striped_file(&h, "coldstream", blocks).await;

    let (g0, f0, p0, adm0, ghost0, waste0, sb0) = (
        get_obj(),
        lane_fetches(),
        METRICS.prefetch_issued.load(Ordering::Relaxed),
        METRICS.read_tier_admissions.load(Ordering::Relaxed),
        METRICS
            .read_tier_admission_ghost_hits
            .load(Ordering::Relaxed),
        METRICS.read_admission_wasted_bytes.load(Ordering::Relaxed),
        lane_serve_bytes(),
    );

    stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;
    // Lane fetches settle asynchronously; give in-flight tasks a bounded
    // drain before asserting the gauges (channel-free poll: the gauge is
    // the contract, not a timing).
    for _ in 0..200 {
        if h.fs.router.read_lane_inflight_bytes() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let df = lane_fetches() - f0;
    let dg = get_obj() - g0;
    let dp = METRICS.prefetch_issued.load(Ordering::Relaxed) - p0;

    // Contract 1: the lane engages where R2 declines.
    assert_eq!(dp, 0, "R2 must stay declined in the zero-share regime");
    assert!(
        df >= blocks / 2,
        "the lane must front-run a classified zero-share stream \
         (read_lane_fetches = {df} across {blocks} blocks)"
    );
    // Fetch economy: single-flight dedupe holds — one device fetch per
    // block (small slack for the classification races).
    assert!(
        dg <= blocks + 3,
        "no double-fetch: {dg} device fetches for {blocks} blocks"
    );
    // Hold engagement: the pass's user bytes are accounted by hold
    // serves (lane-fetched blocks) — the engagement-exact instrument.
    let dsb = lane_serve_bytes() - sb0;
    assert!(
        dsb >= (blocks / 2) * BS,
        "hold serves must account the lane-covered user bytes \
         (read_lane_serve_bytes = {dsb})"
    );

    // Contract 2: ledger invisibility. No NVMe-tier publication for ANY
    // key of the pass (first-touch demand fills skip by second-touch
    // policy; lane fills skip by DESIGN), no admissions, no governor
    // ledger movement.
    for key in map.values() {
        assert!(
            !tier_has(&h, key),
            "lane/first-touch fills must never publish to the NVMe tier"
        );
    }
    assert_eq!(
        METRICS.read_tier_admissions.load(Ordering::Relaxed) - adm0,
        0,
        "no tier admissions from a governed cold stream pass"
    );
    assert_eq!(
        METRICS.read_admission_wasted_bytes.load(Ordering::Relaxed) - waste0,
        0,
        "the governor's waste ledger must never see lane fills"
    );
    // Lane fills never enter the hot tier: probe the tail half's keys
    // (fetched by the lane after classification at block 0).
    let mut hot_lane_entries = 0u32;
    for b in (blocks / 2)..blocks {
        if let Some(key) = map.get(&(b as u32)) {
            if h.fs.router.cache.hot_block.get_no_promote(key).is_some() {
                hot_lane_entries += 1;
            }
        }
    }
    assert_eq!(
        hot_lane_entries, 0,
        "lane fills must not land in the hot tier (ledger-invisible)"
    );

    // Pass 2 — the sustained-loop regime: lane blocks were never
    // ghost-recorded, so the pass must not manufacture second-touch
    // heat (only the pre-classification demand blocks may ghost-hit).
    let ghost_before_p2 = METRICS
        .read_tier_admission_ghost_hits
        .load(Ordering::Relaxed);
    stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;
    let dghost = METRICS
        .read_tier_admission_ghost_hits
        .load(Ordering::Relaxed)
        - ghost_before_p2;
    assert!(
        dghost <= 6,
        "lane fills must not record ghost heat (pass-2 ghost hits = {dghost}; \
         only the pre-classification demand blocks may hit)"
    );
    let _ = ghost0;
    drop(h);
}

// ---------------------------------------------------------------------------
// Contract 8 (round 3 — the field's qd-reorder wedge): a CLASSIFIED
// stream whose requests arrive with completion swaps (the libaio qd
// arrival order) keeps lane membership — the lane keeps issuing and
// classification does not churn. Under the exact-contiguity matcher a
// single swap wedged the lane forever (field: 0.7 % arrival match,
// 1,233 declassify/reclassify events per 70 s row, lane engagement 3 %).
// Run-BUILDING stays exact: the random-vs-stream discriminator is
// pinned by the sibling governor suites.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reordered_qd_stream_keeps_membership_and_lane_engagement() {
    set_zero_share_env();
    set_ahead_env();
    let h = make_with(*b"read-lane-test06", "rdlane_ns_f", false).await;
    clear_ahead_env();
    clear_zero_share_env();

    let blocks = 16u64;
    let (ino, _map) = striped_file(&h, "reorder", blocks).await;
    let (g0, f0, c0) = (
        get_obj(),
        lane_fetches(),
        METRICS.read_streams_classified.load(Ordering::Relaxed),
    );

    let q = BS / 4;
    // Classify with 4 exact-contiguous requests (run building is exact
    // by design), then stream the rest with a persistent qd-style swap
    // in every pair — each pair arrives (n+1, n): NO request after the
    // 4th matches exact contiguity.
    for i in 0..4u64 {
        let d = read_at(&h, ino, i * q, q as u32).await;
        assert!(d.iter().all(|&x| x == 1));
    }
    let total = blocks * BS / q;
    let mut i = 4u64;
    while i + 1 < total {
        for j in [i + 1, i] {
            let off = j * q;
            let b = off / BS;
            let d = read_at(&h, ino, off, q as u32).await;
            assert!(
                d.iter().all(|&x| x == (b % 250) as u8 + 1),
                "parity at swapped offset {off}"
            );
        }
        i += 2;
    }

    for _ in 0..200 {
        if h.fs.router.read_lane_inflight_bytes() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let df = lane_fetches() - f0;
    let dc = METRICS.read_streams_classified.load(Ordering::Relaxed) - c0;
    assert!(
        df >= blocks / 2,
        "a swapped-arrival stream must keep its lane engaged \
         (read_lane_fetches = {df} across {blocks} blocks — 0 is the \
         field's exact-contiguity wedge)"
    );
    let dg = get_obj() - g0;
    assert!(
        dg <= blocks + 4,
        "single-issuer economy: sibling reorder-claimed cursors must not \
         over-fetch ({dg} device fetches for {blocks} blocks — the r3C1 \
         duplicate-cursor waste class)"
    );
    assert!(
        dc <= 2,
        "membership tolerance must stop the declassify/reclassify churn \
         (classify events = {dc})"
    );
    drop(h);
}

// ---------------------------------------------------------------------------
// Contract 3 — deep-qd cohort stability: a sub-read that lost both the
// single-flight window and the hot-probation clock race serves from the
// hold, never refetches; coverage retirement empties the hold.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hold_serves_cohort_stragglers_across_hot_eviction_without_refetch() {
    set_zero_share_env();
    let h = make_with(*b"read-lane-test02", "rdlane_ns_b", false).await;
    clear_zero_share_env();

    // The straggler file is ONE block (nothing ahead for the lane to
    // legitimately speculate on when the straggler run classifies);
    // the interleaver is a 3-block file read in never-contiguous
    // quarter order (never classifies — pure demand fills whose hot
    // inserts evict the straggler's block from the 2-slot hot tier:
    // the qd32 clock-race face).
    let (ino_a, _ma) = striped_file(&h, "cohort_a", 1).await;
    let (ino_x, _mx) = striped_file(&h, "cohort_x", 3).await;
    let (g0, r0) = (
        get_obj(),
        METRICS.read_lane_hold_retired.load(Ordering::Relaxed),
    );

    let q = (BS / 4) as u32;
    // One sub-read of A (demand fill: hot insert + hold deposit)…
    let a = read_at(&h, ino_a, 0, q).await;
    assert!(a.iter().all(|&x| x == 1));
    // …then the interleaver: each X block fully consumed but in
    // non-contiguous quarter order (q0,q2,q1,q3) so no lane ever
    // classifies — 3 demand fills evict A from the 2-slot hot tier.
    for b in 0..3u64 {
        for quarter in [0u64, 2, 1, 3] {
            let off = b * BS + quarter * u64::from(q);
            let d = read_at(&h, ino_x, off, q).await;
            assert!(d.iter().all(|&x| x == (b % 250) as u8 + 1));
        }
    }
    // A's straggler sub-reads arrive after the eviction: the hold — not
    // a whole-block refetch — must serve them.
    for i in 1..4u64 {
        let d = read_at(&h, ino_a, i * u64::from(q), q).await;
        assert!(d.iter().all(|&x| x == 1), "straggler sub-read {i} parity");
    }

    let dg = get_obj() - g0;
    assert_eq!(
        dg, 4,
        "cohort stability: 4 unique blocks => 4 device fetches (a 5th is \
         the qd32 straggler-refetch regression this contract outlaws)"
    );

    // Memory converges by consumption: every block was fully covered,
    // so the hold is empty and retirements are counted.
    for _ in 0..200 {
        if h.fs.router.cache.read_lane_hold.bytes() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.fs.router.cache.read_lane_hold.bytes(),
        0,
        "full coverage must retire every hold entry"
    );
    assert!(
        METRICS.read_lane_hold_retired.load(Ordering::Relaxed) - r0 >= 4,
        "coverage retirements must be counted"
    );
    drop(h);
}

// ---------------------------------------------------------------------------
// Contract 6 — the A0 lever: SQUEEZEFS_READ_LANE=0 is EXACT prior
// behavior (the straggler refetches; every read_lane_* counter flat).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_off_is_exact_prior_behavior() {
    set_zero_share_env();
    std::env::set_var("SQUEEZEFS_READ_LANE", "0");
    let h = make_with(*b"read-lane-test03", "rdlane_ns_c", false).await;
    std::env::remove_var("SQUEEZEFS_READ_LANE");
    clear_zero_share_env();

    let (ino_a, _ma) = striped_file(&h, "leveroff_a", 1).await;
    let (ino_x, _mx) = striped_file(&h, "leveroff_x", 3).await;
    let (g0, f0, h0, s0) = (
        get_obj(),
        lane_fetches(),
        METRICS.read_lane_holds.load(Ordering::Relaxed),
        METRICS.read_lane_serves.load(Ordering::Relaxed),
    );

    // The contract-3 straggler shape…
    let q = (BS / 4) as u32;
    let a = read_at(&h, ino_a, 0, q).await;
    assert!(a.iter().all(|&x| x == 1));
    for b in 0..3u64 {
        for quarter in [0u64, 2, 1, 3] {
            let off = b * BS + quarter * u64::from(q);
            let d = read_at(&h, ino_x, off, q).await;
            assert!(d.iter().all(|&x| x == (b % 250) as u8 + 1));
        }
    }
    for i in 1..4u64 {
        let d = read_at(&h, ino_a, i * u64::from(q), q).await;
        assert!(d.iter().all(|&x| x == 1));
    }

    // …reproduces the prior shape: A is refetched after eviction.
    assert_eq!(
        get_obj() - g0,
        5,
        "lever-off must reproduce the pre-campaign refetch shape exactly"
    );
    // And a full stream pass leaves R2 declined with the lane dark.
    let (ino2, _m2) = striped_file(&h, "leveroff2", 8).await;
    stream_pass(&h, ino2, 8, |b| (b % 250) as u8 + 1).await;
    assert_eq!(lane_fetches() - f0, 0, "lever-off: no lane fetches");
    assert_eq!(
        METRICS.read_lane_holds.load(Ordering::Relaxed) - h0,
        0,
        "lever-off: no deposits"
    );
    assert_eq!(
        METRICS.read_lane_serves.load(Ordering::Relaxed) - s0,
        0,
        "lever-off: no hold serves"
    );
    assert_eq!(h.fs.router.cache.read_lane_hold.bytes(), 0);
    drop(h);
}

// ---------------------------------------------------------------------------
// Contract 5 — R5 Red: no new speculation, no new deposits; gauges
// converge by completion.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn red_stops_lane_speculation_and_deposits_and_converges() {
    set_zero_share_env();
    set_ahead_env();
    let h = make_with(*b"read-lane-test04", "rdlane_ns_d", false).await;
    clear_ahead_env();
    clear_zero_share_env();

    let (ino, _map) = striped_file(&h, "redlane", 8).await;
    let f0 = lane_fetches();
    let h0 = METRICS.read_lane_holds.load(Ordering::Relaxed);

    squeezefs::mem_budget::MEM_BUDGET.force_level_for_test(squeezefs::mem_budget::Level::Red);
    stream_pass(&h, ino, 8, |b| (b % 250) as u8 + 1).await;
    squeezefs::mem_budget::MEM_BUDGET.force_level_for_test(squeezefs::mem_budget::Level::Green);

    assert_eq!(
        lane_fetches() - f0,
        0,
        "Red must stop lane speculation outright"
    );
    assert_eq!(
        METRICS.read_lane_holds.load(Ordering::Relaxed) - h0,
        0,
        "Red must pause hold deposits"
    );
    assert_eq!(
        h.fs.router.read_lane_inflight_bytes(),
        0,
        "in-flight converges by completion"
    );
    assert_eq!(h.fs.router.cache.read_lane_hold.bytes(), 0);
    drop(h);
}

// ---------------------------------------------------------------------------
// Contract 7 — purge integration: an overwritten block never serves
// stale bytes from the hold (the R-6 law, hold arm).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overwritten_blocks_never_serve_stale_from_the_hold() {
    set_zero_share_env();
    let h = make_with(*b"read-lane-test05", "rdlane_ns_e", false).await;
    clear_zero_share_env();

    let (ino, _map) = striped_file(&h, "purgelane", 2).await;

    // Deposit block 0 in the hold (partial consumption keeps it held).
    let q = (BS / 4) as u32;
    let d = read_at(&h, ino, 0, q).await;
    assert!(d.iter().all(|&x| x == 1));

    // Overwrite block 0 through the write path (its displace/publish
    // purges every block-key store — the hold must be one of them),
    // then make it durable.
    write_at(&h, ino, 0, &vec![99u8; BS as usize]).await;
    h.fs.fsync(h.req, ino, 0, false).await.unwrap();

    let d = read_at(&h, ino, 0, BS as u32).await;
    assert!(
        d.iter().all(|&x| x == 99),
        "stale hold serve after overwrite — the R-6 purge arm is broken"
    );
    drop(h);
}
