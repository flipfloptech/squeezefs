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

    // Ahead-issue defaults to the ENGAGE-GOVERNOR's depth (2026-08-05,
    // the read-throughput campaign — the probe-adopt-retreat follow-on
    // the read-lane note §8.2 named): an unprobed governor contributes
    // 0 (exact hold-only prior behavior), an adopted probe multiplier
    // flows through verbatim. The 2026-08-01 falsification stands as
    // the RETREAT arm, not as a constant 0.
    assert_eq!(read_lane_depth_blocks(None, 0, bs, 32, false, 64 * gib), 0);
    assert_eq!(read_lane_depth_blocks(None, 0, bs, 1, false, 64 * gib), 0);
    assert_eq!(read_lane_depth_blocks(None, 5, bs, 32, false, 64 * gib), 5);

    // The pin wins verbatim over the governed depth (0 = ahead off —
    // the A0/hold-only measurement control).
    assert_eq!(
        read_lane_depth_blocks(Some(7), 3, bs, 32, false, 64 * gib),
        7
    );
    assert_eq!(
        read_lane_depth_blocks(Some(0), 3, bs, 32, false, 64 * gib),
        0
    );

    // The R5 budget cap is senior to the pin AND the governed depth: a
    // cap that holds 3 blocks/stream clamps deeper values to 3; a cap
    // that cannot hold even ONE block per stream never speculates.
    assert_eq!(read_lane_depth_blocks(Some(16), 0, bs, 2, false, 6 * bs), 3);
    assert_eq!(read_lane_depth_blocks(None, 16, bs, 2, false, 6 * bs), 3);
    assert_eq!(
        read_lane_depth_blocks(Some(16), 0, bs, 32, false, 16 * bs),
        0
    );

    // Red stops speculation outright and is SENIOR to the pin and the
    // governor (the write-pipeline Red-clamp precedent).
    assert_eq!(
        read_lane_depth_blocks(Some(8), 0, bs, 32, true, 64 * gib),
        0
    );
    assert_eq!(read_lane_depth_blocks(None, 8, bs, 32, true, 64 * gib), 0);

    // Zero streams behaves as one (defensive).
    assert_eq!(
        read_lane_depth_blocks(Some(2), 0, bs, 0, false, 64 * gib),
        2
    );

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
// Contract 9 (2026-08-05, the read-throughput campaign): the ahead
// lane's ENGAGE-GOVERNOR — the probe-adopt-retreat follow-on the
// read-lane note §8.2 named. The depth derives from a measured probe
// cycle (launch on saturated epochs with headroom, adopt only when
// fill delivery responds, retreat + cool down on dead gain, bleed to
// zero on unsaturated epochs) — never from a constant, never from the
// falsified pure-BDP/AIMD arithmetic.
// ---------------------------------------------------------------------------

#[test]
fn probe_governed_depth_mapping_table() {
    use squeezefs::read_lane::probe_governed_depth;
    use squeezefs::write_pipeline::{PROBE_MUL_MAX, PROBE_MUL_ONE};

    // ×1.0 (unprobed / fully decayed) = depth 0 — the exact hold-only
    // prior behavior; the first probe gain (+1/4) = the minimal
    // one-block-per-stream pipeline; adopted gains compound.
    assert_eq!(probe_governed_depth(PROBE_MUL_ONE), 0);
    assert_eq!(probe_governed_depth(PROBE_MUL_ONE + PROBE_MUL_ONE / 4), 1);
    assert_eq!(probe_governed_depth(100), 2);
    assert_eq!(probe_governed_depth(125), 3);
    assert_eq!(probe_governed_depth(156), 5);
    assert_eq!(probe_governed_depth(195), 8);
    // Monotone, and bounded by the ProbeCore runaway guard (the R5 cap
    // in read_lane_depth_blocks stays the operative absolute bound).
    let mut prev = 0;
    for mul in PROBE_MUL_ONE..=PROBE_MUL_MAX {
        let d = probe_governed_depth(mul);
        assert!(d >= prev, "depth must be monotone in the multiplier");
        prev = d;
    }
    assert_eq!(probe_governed_depth(PROBE_MUL_MAX), 124);
    // Sub-one multipliers (impossible by ProbeCore's floor; defensive)
    // never underflow.
    assert_eq!(probe_governed_depth(0), 0);
}

#[test]
fn engage_governor_probe_cycle_drives_the_default_depth() {
    use squeezefs::read_lane::ReadLaneGovernor;

    // No env pin: the governor owns the depth.
    std::env::remove_var("SQUEEZEFS_READ_LANE_DEPTH");
    std::env::remove_var("SQUEEZEFS_READ_LANE");
    let g = ReadLaneGovernor::from_env();
    let gib = 1024 * 1024 * 1024u64;
    let bs = 4 * 1024 * 1024u64;

    // Unprobed: depth 0 — a default mount speculates nothing until a
    // probe epoch has MEASURED that ahead depth buys delivery.
    assert_eq!(g.governed_depth(), 0);
    assert_eq!(g.depth_blocks(bs, 16, false, 64 * gib), 0);

    // Epoch anchor.
    assert!(!g.probe_roll_at(1_000, true, true));

    // Saturated + headroom + delivery flowing => LAUNCH (+1/4 = depth 1).
    g.probe_on_fill_bytes(10_000_000);
    assert!(g.probe_roll_at(1_600, true, true));
    assert_eq!(g.governed_depth(), 1, "first probe = the minimal pipeline");
    assert_eq!(g.probe_ups(), 1);

    // Delivery responds (2x) => ADOPT (multiplier kept, re-armed).
    g.probe_on_fill_bytes(20_000_000);
    assert!(g.probe_roll_at(2_200, true, true));
    assert_eq!(g.governed_depth(), 1, "adopt keeps the raised multiplier");

    // Next saturated epoch relaunches and compounds (depth 2)...
    g.probe_on_fill_bytes(40_000_000);
    assert!(g.probe_roll_at(2_800, true, true));
    assert_eq!(g.governed_depth(), 2, "discovery compounds");
    assert_eq!(g.probe_ups(), 2);

    // ...and DEAD GAIN retreats to the pre-probe multiplier + cools
    // down — the 2026-08-01 falsified venue (demand already covers the
    // fabric BDP: extra depth buys nothing) is a RETREAT arm, bounded
    // to the probe duty cycle, not a sustained -19 % engagement.
    g.probe_on_fill_bytes(40_000_000);
    assert!(g.probe_roll_at(3_400, true, true));
    assert_eq!(g.governed_depth(), 1, "dead gain retreats");
    assert!(g.probe_backoffs() >= 1);

    // The pinned depth flows through depth_blocks (Red/cap senior —
    // pinned by depth_derivation_tables).
    assert_eq!(g.depth_blocks(bs, 16, false, 64 * gib), 1);
    assert_eq!(g.depth_blocks(bs, 16, true, 64 * gib), 0, "Red senior");

    // Unsaturated epochs bleed the multiplier back to x1.0 — the
    // latency guard: low-offered-load mounts never inherit streaming
    // depth (the write-pipeline probe-up law, verbatim).
    for i in 0..8u64 {
        g.probe_roll_at(4_000 + i * 600, false, true);
    }
    assert_eq!(g.governed_depth(), 0, "idle decay converges to zero");

    // The explicit pin stays senior to the governor (the A/B lever).
    std::env::set_var("SQUEEZEFS_READ_LANE_DEPTH", "0");
    let pinned_off = ReadLaneGovernor::from_env();
    std::env::remove_var("SQUEEZEFS_READ_LANE_DEPTH");
    pinned_off.probe_on_fill_bytes(10_000_000);
    pinned_off.probe_roll_at(1_000, true, true);
    pinned_off.probe_on_fill_bytes(10_000_000);
    pinned_off.probe_roll_at(1_600, true, true);
    assert_eq!(
        pinned_off.depth_blocks(bs, 16, false, 64 * gib),
        0,
        "SQUEEZEFS_READ_LANE_DEPTH=0 pins ahead-issue OFF verbatim"
    );
}

// ---------------------------------------------------------------------------
// Contract 11 (2026-08-05, the hold-churn campaign — the field's
// retirement-death conviction, local S1 evidence: 570 retired of
// 121,639 holds, 119k FIFO evictions, hold_bytes pinned at ~8-11 GB):
// the governor must never PROBE into a thrashing hold — an ahead-class
// eviction since the last epoch is zero headroom (else probes measure
// their own thrash as dead gain forever), and the hold's oldest-first
// trim structurally retains ahead entries (the newest deposits) while
// classing its evictions so the pressure signal is clean.
// ---------------------------------------------------------------------------

#[test]
fn governor_refuses_to_probe_into_ahead_eviction_pressure() {
    use squeezefs::read_lane::ReadLaneGovernor;
    std::env::remove_var("SQUEEZEFS_READ_LANE_DEPTH");
    std::env::remove_var("SQUEEZEFS_READ_LANE");
    let g = ReadLaneGovernor::from_env();

    // Epoch anchor.
    g.note_probe_saturation();
    assert!(!g.probe_epoch_tick_at(1_000, true, 0));

    // Saturated + headroom, but ahead-class hold evictions moved since
    // the last epoch: the landing zone is thrashing — a probe launched
    // now would fetch into a hold that evicts its deposits before
    // their readers arrive and adjudicate its own thrash as dead gain.
    // NO launch.
    g.note_probe_saturation();
    g.probe_on_fill_bytes(10_000_000);
    assert!(g.probe_epoch_tick_at(1_600, true, 5));
    assert_eq!(
        g.probe_ups(),
        0,
        "ahead-eviction pressure must read as zero headroom"
    );
    assert_eq!(g.governed_depth(), 0);

    // Pressure cleared (counter unchanged since the snapshot): the
    // probe launches on the next saturated epoch.
    g.note_probe_saturation();
    g.probe_on_fill_bytes(10_000_000);
    assert!(g.probe_epoch_tick_at(2_200, true, 5));
    assert_eq!(g.probe_ups(), 1, "clean epoch probes again");
    assert_eq!(g.governed_depth(), 1);
}

#[test]
fn hold_trim_retains_ahead_entries_and_classes_its_evictions() {
    let hold = ReadLaneHold::new();
    let blk = bytes::Bytes::from(vec![9u8; BS as usize]);
    let budget = 2 * BS; // two entries

    let evicted0 = METRICS
        .read_lane_hold_evicted_unconsumed
        .load(Ordering::Relaxed);
    let ahead0 = METRICS
        .read_lane_hold_ahead_evictions
        .load(Ordering::Relaxed);

    // Demand transit d1, then the ahead entry a1 (the newest), then
    // demand transit d2: the oldest-first trim must evict d1 — the
    // flow-through population — and RETAIN the ahead entry (the item-2
    // adjudication: FIFO oldest-first + retirement-by-consumption IS
    // the ahead-priority mechanism; ahead entries are structurally
    // last in line).
    hold.insert_demand("d1", blk.clone(), budget);
    hold.insert("a1", blk.clone(), budget); // ahead class (lane fetch)
    hold.insert_demand("d2", blk.clone(), budget);
    assert!(!hold.contains("d1"), "oldest demand transit evicts first");
    assert!(hold.contains("a1"), "the ahead entry is retained");
    assert!(hold.contains("d2"));
    assert_eq!(
        METRICS
            .read_lane_hold_ahead_evictions
            .load(Ordering::Relaxed)
            - ahead0,
        0,
        "a demand-class eviction must not read as ahead pressure"
    );

    // Trimming past the ahead entry classes it: the governor's
    // pressure signal (contract 11's headroom input) counts exactly
    // the ahead-class starvation evictions.
    hold.trim_to(0);
    assert_eq!(
        METRICS
            .read_lane_hold_ahead_evictions
            .load(Ordering::Relaxed)
            - ahead0,
        1,
        "an unconsumed ahead-class eviction is the pressure signal"
    );
    assert_eq!(
        METRICS
            .read_lane_hold_evicted_unconsumed
            .load(Ordering::Relaxed)
            - evicted0,
        3,
        "the total evicted-unconsumed ledger counts both classes"
    );
}

/// 4 KiB-aligned scratch destination (the registered-payload stand-in —
/// the read_copy_ledger_tests pattern).
struct AlignedDest {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}
impl AlignedDest {
    fn new(size: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(size, 4096).unwrap();
        // SAFETY: valid non-zero layout; zeroed so reads of unwritten
        // bytes are defined.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Self { ptr, layout }
    }
    fn dest(&self) -> squeezefs::routing::ReadDest {
        // SAFETY: the allocation outlives every read it is handed to and
        // is exclusively this test's.
        unsafe { squeezefs::routing::ReadDest::new(self.ptr as u64, self.layout.size()) }
    }
}
impl Drop for AlignedDest {
    fn drop(&mut self) {
        // SAFETY: allocated with this exact layout above.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

// ---------------------------------------------------------------------------
// Contract 12 (2026-08-05, the hold-churn campaign — the coverage-credit
// truth): a dest-armed fetch-loop serve must credit its TRUE block
// coverage. The shipped tail credit computed
// `min(slice_len, served_slice.len() − slice_start)` — on the dest arm
// `served_slice` is the POST-SLICE dest bytes (len == slice_len), so
// every sub-read past a block's first credited ZERO: coverage
// retirement died on deep-qd cohort rows (local S1 evidence: 570
// retired of 121,639 holds; the hold pinned at its ~10 GiB budget with
// 119k FIFO evictions reading as churn; the field's 9,025
// `read_lane_hold_evicted_unconsumed`).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dest_armed_loop_serves_credit_true_block_coverage() {
    // Whole-block shape parity (the zero-share-env rule): the fixture's
    // 512 KiB blocks force 128 KiB sub-reads, which would otherwise
    // ride R3 ranged windows (no whole-block fill, no deposit).
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    let h = make_with(*b"read-lane-test10", "rdlane_ns_j", false).await;
    std::env::remove_var("SQUEEZEFS_READ_RANGED_THRESHOLD");
    let blocks = 2u64;
    let (ino, map) = striped_file(&h, "credittruth", blocks).await;
    let key0 = map.get(&0).unwrap().clone();
    let path = squeezefs::keys::inode_path(ino);
    let dest = AlignedDest::new(128 * 1024);

    // Cold, dest-armed, slice_start = 128 KiB: the primary rides the
    // validated fetch loop, deposits the 512 KiB fill in the hold, and
    // must credit THIS op's block coverage — 128 KiB.
    let (data, _b) =
        h.fs.router
            .read_file_range_zero_copy_with_meta(
                &path,
                128 * 1024,
                128 * 1024,
                Some(dest.dest()),
                Default::default(),
                None,
            )
            .await
            .unwrap();
    assert!(data.iter().all(|&x| x == 1), "byte parity");
    assert!(
        h.fs.router.cache.read_lane_hold.contains(&key0),
        "the demand primary deposits"
    );
    assert_eq!(
        h.fs.router.cache.read_lane_hold.served_bytes(&key0),
        Some(128 * 1024),
        "a dest-armed loop serve must credit its true block coverage \
         (the tail credit read 0 for every sub-read past the block's first)"
    );

    // The remaining three sub-reads complete the coverage (whatever arm
    // serves them — hot/hold serves already credit correctly): the
    // entry RETIRES — memory converges by consumption, not eviction.
    let r0 = METRICS.read_lane_hold_retired.load(Ordering::Relaxed);
    for off in [0u64, 256 * 1024, 384 * 1024] {
        let (d, _b) =
            h.fs.router
                .read_file_range_zero_copy_with_meta(
                    &path,
                    off,
                    128 * 1024,
                    Some(dest.dest()),
                    Default::default(),
                    None,
                )
                .await
                .unwrap();
        assert!(d.iter().all(|&x| x == 1), "byte parity at {off}");
    }
    assert!(
        !h.fs.router.cache.read_lane_hold.contains(&key0),
        "full coverage retires the entry"
    );
    assert_eq!(
        METRICS.read_lane_hold_retired.load(Ordering::Relaxed) - r0,
        1,
        "retirement is the converge-by-consumption verdict"
    );
    drop(h);
}

// ---------------------------------------------------------------------------
// Contract 13 (2026-08-05, the hold-churn campaign — the marginal-issue
// walk): blocks the demand front or any store already covers must
// advance the lane cursor as SYNC probes — never a spawned task, never
// a depth slot. The field's depth-1 probes died here: ~92 % of issue
// opportunities raced into demand-covered blocks and skip-settled
// through full task latency (1,912 real fetches against ~25k
// opportunities), so probe delivery never responded and every probe
// honestly retreated (ups = backoffs = 14).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lane_walk_skips_covered_blocks_without_spending_depth() {
    set_zero_share_env();
    // 4 MiB hot tier (8 slots) instead of the zero-share default 1 MiB
    // (2 slots): the demand fills' own hot landings must not evict the
    // pre-covered blocks before the walk probes them. Share stays 0
    // (1 % x 4 MiB / 512 KiB truncates to 0).
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "4");
    set_ahead_env();
    let h = make_with(*b"read-lane-test11", "rdlane_ns_k", false).await;
    clear_ahead_env();
    clear_zero_share_env();

    let blocks = 12u64;
    let (ino, map) = striped_file(&h, "walkfile", blocks).await;
    // Pre-cover blocks 2 and 3 (hot-resident — the demand-front
    // stand-in: a block the reader will find without a lane fetch).
    for b in [2u32, 3] {
        let key = map.get(&b).unwrap();
        h.fs.router.cache.hot_block.put(
            key,
            bytes::Bytes::from(vec![(b % 250) as u8 + 1; BS as usize]),
        );
    }

    let skips0 = METRICS.read_lane_covered_skips.load(Ordering::Relaxed);
    let f0 = lane_fetches();
    stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;
    for _ in 0..200 {
        if h.fs.router.read_lane_inflight_bytes() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert!(
        METRICS.read_lane_covered_skips.load(Ordering::Relaxed) - skips0 >= 2,
        "covered blocks must advance the cursor as sync probes \
         (the walk's engagement instrument)"
    );
    assert!(
        lane_fetches() - f0 >= blocks / 2,
        "the lane still front-runs the uncovered span"
    );
    drop(h);
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
// RES-3 (pre-RC engineering spec §7) — the FIFO must not accumulate
// tombstones in the HEALTHY steady state.
//
// `insert` pushes one `(u64, String)` node per deposit and `trim_to` is
// the only popper — gated on `bytes > target`. Coverage retirement (the
// designed, healthy exit) removes the map entry and credits the byte
// gauge but leaves the node, so a hold that never reaches its budget
// grows a node per deposit FOREVER, and the R5 `read_lane_hold`
// component — which gauges payload bytes — is structurally blind to it.
// ---------------------------------------------------------------------------

#[test]
fn hold_fifo_does_not_accumulate_retirement_tombstones() {
    const DEPOSITS: u64 = 4096;
    let hold = ReadLaneHold::new();
    let blk = bytes::Bytes::from(vec![7u8; BS as usize]);
    // Budget for 8 entries: the steady state below never trims, because
    // every entry retires on full coverage before the next deposit.
    let budget = 8 * BS;

    for i in 0..DEPOSITS {
        let k = format!("res3_blk{i}");
        hold.insert(&k, blk.clone(), budget);
        // Full coverage ⇒ retire (the healthy exit).
        assert!(hold.serve(&k, BS).is_some(), "deposit {i} must serve");
        assert!(!hold.contains(&k), "deposit {i} must retire on coverage");
    }
    assert_eq!(hold.bytes(), 0, "the byte gauge converges — it always did");
    assert!(
        hold.fifo_len() as u64 <= 64,
        "RES-3: {} FIFO tombstones survive {DEPOSITS} retired deposits — \
         the queue grows one (u64, String) node per deposit in the healthy \
         steady state and the R5 read_lane_hold gauge cannot see any of it",
        hold.fifo_len()
    );
}

/// Tombstone reclamation must not break the oldest-first trim, and must
/// not drop LIVE nodes: a long-lived entry pinned at the head while
/// tombstones pile up behind it is the shape a head-only skim misses.
#[test]
fn hold_fifo_reclamation_keeps_live_nodes_and_oldest_first_trim() {
    let hold = ReadLaneHold::new();
    let blk = bytes::Bytes::from(vec![3u8; BS as usize]);
    let budget = 4 * BS;

    // Oldest entry: never consumed, so it stays live at the FIFO head.
    hold.insert("pinned", blk.clone(), budget);

    // Churn behind it: every deposit retires, leaving a tombstone the
    // head-live case would never skim.
    for i in 0..2048u64 {
        let k = format!("churn{i}");
        hold.insert(&k, blk.clone(), budget);
        hold.credit(&k, BS);
        assert!(!hold.contains(&k), "churn {i} retires on coverage");
    }
    assert!(
        hold.fifo_len() <= 64,
        "RES-3: {} nodes behind a pinned live head",
        hold.fifo_len()
    );
    assert!(
        hold.contains("pinned"),
        "reclamation must not drop live nodes"
    );
    assert_eq!(hold.bytes(), BS, "gauge exact after reclamation");

    // Oldest-first trim still names the pinned entry first.
    hold.insert("newer", blk.clone(), budget);
    hold.trim_to(BS);
    assert!(
        !hold.contains("pinned"),
        "oldest-first survives reclamation"
    );
    assert!(hold.contains("newer"), "the newer entry is kept");
    hold.trim_to(0);
    assert_eq!(hold.bytes(), 0);
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
// Contract 10 (2026-08-05, the read-throughput campaign — the field's
// 16-stream shape): a resident share of ONE block is BELOW R2's AIMD
// start window (2), so R2's hot-probation landing is structurally
// evicted-before-consume — the field row issued 823 prefetches in 60 s
// and 575 died unconsumed, quiescing every lane while foreground reads
// waited 5.4 ms on demand fills (27.4 GB/s vs the 41.8 raw ceiling).
// The lane — whose hold landing is coverage-retired, never
// clock-churned by demand fills — is the vehicle for the WHOLE
// below-start-window regime, not only share == 0.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lane_engages_below_the_r2_start_window() {
    // Hot budget 1 MiB + share 50 % at 512 KiB blocks and one stream:
    // resident_share = 50 % x 1 MiB / 512 KiB / 1 = 1 — the sub-start
    // regime the field's 16-job row rides (share oscillating 0..1).
    std::env::set_var("SQUEEZEFS_READ_TIER_ADMISSION", "second-touch");
    std::env::set_var("SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB", "1");
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_SHARE_PCT", "50");
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    set_ahead_env();
    let h = make_with(*b"read-lane-test09", "rdlane_ns_i", false).await;
    clear_ahead_env();
    clear_zero_share_env();

    let blocks = 16u64;
    let (ino, _map) = striped_file(&h, "subwindow", blocks).await;

    let (g0, f0, p0) = (
        get_obj(),
        lane_fetches(),
        METRICS.prefetch_issued.load(Ordering::Relaxed),
    );

    stream_pass(&h, ino, blocks, |b| (b % 250) as u8 + 1).await;
    for _ in 0..200 {
        if h.fs.router.read_lane_inflight_bytes() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let df = lane_fetches() - f0;
    let dg = get_obj() - g0;
    let dp = METRICS.prefetch_issued.load(Ordering::Relaxed) - p0;

    // R2 must DECLINE the whole sub-start-window regime: a share of 1
    // cannot express even the AIMD start plan, and its issues die in
    // hot probation (the field's 70 % evicted-unconsumed).
    assert_eq!(
        dp, 0,
        "R2 must not issue hot-landing speculation below its start window"
    );
    // The lane front-runs instead (hold landing — coverage-retired).
    assert!(
        df >= blocks / 2,
        "the lane must front-run a share-1 stream \
         (read_lane_fetches = {df} across {blocks} blocks)"
    );
    // Fetch economy unchanged: one device fetch per block.
    assert!(
        dg <= blocks + 3,
        "no double-fetch: {dg} device fetches for {blocks} blocks"
    );
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
