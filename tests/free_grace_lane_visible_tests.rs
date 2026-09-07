//! **Finding 15, term 2 — the fleet-only `min_acked→released` hop, and the
//! co-writer's lane-visible hop behind it**
//! (`.benchmarks/2026-09-06-free-grace-lane-visible.md`; the program input
//! is `.benchmarks/2026-09-06-free-grace-hold-time.md` §6: on the s11
//! fleet `free_grace_hold_phase_ns.min_acked_released` read a 2,500 ms
//! MEAN over 45,540 samples where the in-process model read 9 ms — with
//! 76 % of the samples under 512 ms and 3,849 past 16 s).
//!
//! # What the code says the third stage is
//!
//! `released` IS the harvest's pop: `GraceRing::harvest_with` stamps the
//! hold ledger at the instant it pops, and the allocator publishes the
//! popped offset to its free list in the same synchronous act. So
//! `min_acked→released` is the wait for the NEXT HARVEST EVENT after the
//! covering bound advance — and the authority's harvest is DEMAND-driven:
//! a terminal free on the authority (a co-writer's shipped free landing), an
//! allocation on the authority, or a co-writer's harvest RPC. Nothing
//! harvests on a timer, and nothing harvests when the acknowledgement that
//! covered the offsets ARRIVES (lever (d) only marks the bound dirty for
//! the next harvest). On a fleet whose writers are the parked ones, the
//! demand events are the co-writers' own RPCs; when the fleet is quiet
//! (a barrier, a fsync wedge), covered offsets sit until the next one.
//!
//! Behind the release sits a hop the hold ledger cannot see: a released
//! CO-WRITER-lane block is on the AUTHORITY's list, reachable by nobody
//! but that co-writer's next harvest RPC — its ENOSPC park slices (50 ms
//! on a co-writer, which has no plane) or its 1 s watermark tick (which a
//! quiet lane's decayed rate turns dark). The co-writer's allocator can
//! mint the block only when the reply is adopted.
//!
//! # The instrument (`alloc_lane_visible_phase_ns`)
//!
//! `released_served` (authority clock: the block's wait on the authority's
//! list, from its grace release to the lane harvest that took it),
//! `served_visible` (co-writer clock: the harvest round trip — the reply's
//! adoption is the visibility instant), `total ≡ released_served +
//! served_visible` per sample. The authority stamps `released_served` as it
//! serves and carries each age on the reply (publish schema 14); the
//! co-writer stamps all three. The two clocks never mix.
//!
//! # The lever (`SQUEEZEFS_FREE_GRACE_LANE_PUSH`, default on)
//!
//! * **release on ack** (authority): a BINDING member's advancing
//!   acknowledgement — lever (d)'s one-compare gate — harvests every ring
//!   to its uncovered front on arrival, rate-limited by exactly lever (d)'s
//!   law; the release follows the ack, not the next demand event;
//! * **the lane-supply hint** (wire): the membership renewal grant carries
//!   `lane_supply_blocks`, the blocks of that member's lane on the
//!   authority's free lists (O(1) per-lane counters — never a scan in the
//!   renewal hot op, KD-FG-4);
//! * **the pushed refill** (co-writer): a nonzero hint while OWED blocks
//!   wakes the refill at once (the ahead task and the bounded allocation
//!   park both wait on the wake beside their own cadence).
//!
//! # Contracts
//!
//! 1. the authority stamps a lane block's release→serve wait exactly (manual
//!    clock), an unmarked block counts unplaced;
//! 2. the co-writer's three stages close exactly per sample and in sum;
//! 3. on real allocators: a deferred lane block is served by NO harvest
//!    before its acknowledgement (the safety law), and once released the
//!    two ledgers chain — the hold ledger ends where the lane ledger
//!    begins — with the wire's `release_ages_ms` the age the authority
//!    measured;
//! 4. the pushed-refill decision is pure and lever-gated;
//! 5. the co-writer-lane model at the fleet cadences, lever OFF: a quiet
//!    phase (the fleet's close) leaves covered offsets unreleased until
//!    demand resumes — `min_acked→released` reads the pause, the fleet's
//!    shape; lever ON: it reads lever (d)'s rate limit, every co-writer's
//!    supply is released at the ack and refilled on the renewal's heels;
//! 6. the supply-adequate shape, lever OFF: the lane-visible hop reads
//!    the watermark tick's cadence; ON: pushed refills collapse it;
//! 7. the lever moves no safety law: closure `deferrals ≡ releases +
//!    held`, forced = fences = 0, no offset visible before its release,
//!    the lane-visible family exact-sum, and OFF is the shipped shape
//!    verbatim (zero engagement).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::data_alloc_lane::{LaneHarvest, LaneHarvestSink};
use squeezefs::free_grace::{self, AckInputs, GraceRing, ReaderAckLadder};
use squeezefs::fuse_client::METRICS;
use squeezefs::membership::{
    self, Grant, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    RenewOutcome,
};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fixtures (the reader_free_grace_tests shape: process-global planes)
// ---------------------------------------------------------------------------

static SERIAL: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL.swap(true, Ordering::AcqRel) {
        std::thread::sleep(Duration::from_millis(2));
    }
    free_grace::reset_for_test();
    membership::uninstall();
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        free_grace::reset_for_test();
        membership::uninstall();
        SERIAL.store(false, Ordering::Release);
    }
}

fn manual_clock() -> (LeaseClock, Arc<AtomicU64>) {
    let ticks = Arc::new(AtomicU64::new(10_000));
    (LeaseClock::manual(Arc::clone(&ticks)), ticks)
}

fn shipped_clocks() -> LeaseClocks {
    LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation must be safe")
}

fn armed_owner(clock: &LeaseClock) -> Arc<MembershipOwner> {
    let owner = MembershipOwner::arm("lane-visible-owner", 3, 2, shipped_clocks(), clock.clone())
        .expect("arming a successor term must be admitted");
    membership::install_owner(Arc::clone(&owner));
    owner
}

fn join(owner: &MembershipOwner, id: &str, role: MemberRole) -> Grant {
    match owner.join(JoinRequest {
        id: id.to_string(),
        role,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-lane-visible-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(g) => g,
        JoinOutcome::Refused { reason, .. } => panic!("join refused: {reason}"),
        JoinOutcome::UnknownLease { reason } => panic!("join answered UnknownLease: {reason}"),
    }
}

fn part(writers: u16, id: u16) -> AppendPartition {
    AppendPartition::new(writers, id).expect("partition")
}

/// The lane-visible family as the stats inode exports it.
fn lane_visible() -> serde_json::Value {
    free_grace::lane_visible_stats()
}

fn phase(family: &serde_json::Value, key: &str, name: &str) -> (u64, u64) {
    let h = &family[key][name];
    (
        h["count"].as_u64().unwrap_or(0),
        h["sum_ns"].as_u64().unwrap_or(0),
    )
}

fn phase_mean_ms(family: &serde_json::Value, key: &str, name: &str) -> f64 {
    family[key][name]["mean_ns"].as_u64().unwrap_or(0) as f64 / 1e6
}

/// Samples of one stage past one second (the histogram's seconds-class
/// buckets: `<=2s` … `>16s`) — the fleet's tail, counted.
fn phase_tail_over_1s(family: &serde_json::Value, key: &str, name: &str) -> u64 {
    family[key][name]["buckets"]
        .as_object()
        .map(|b| {
            b.iter()
                .filter(|(k, _)| k.ends_with('s') && !k.ends_with("ms") && !k.ends_with("us"))
                .map(|(_, v)| v.as_u64().unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 1–2 — the ledger's arithmetic
// ---------------------------------------------------------------------------

/// **The authority stamps a lane block's release→serve wait exactly.** A
/// release mark taken 700 ms later reads 700 ms in `released_served`; a
/// block with no mark is `None` and counts unplaced; a mark is taken once.
#[test]
fn the_authority_stamps_a_lane_blocks_release_to_serve_wait() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");

    free_grace::mark_lane_release(0xA, 17);
    free_grace::mark_lane_release(0xA, 33);
    assert_eq!(free_grace::lane_release_marks(), 2, "two marks outstanding");
    ticks.fetch_add(700, Ordering::SeqCst);
    assert_eq!(
        free_grace::take_lane_release(0xA, 17),
        Some(700),
        "the age is the owner-clock gap between the release and the serve"
    );
    ticks.fetch_add(300, Ordering::SeqCst);
    assert_eq!(free_grace::take_lane_release(0xA, 33), Some(1_000));
    assert_eq!(free_grace::lane_release_marks(), 0, "both marks consumed");
    assert_eq!(
        free_grace::take_lane_release(0xA, 17),
        None,
        "a mark is taken exactly once"
    );
    assert_eq!(
        free_grace::take_lane_release(0xB, 17),
        None,
        "a block that never went through a grace release has no mark"
    );
    assert_eq!(
        free_grace::lane_visible_unplaced(),
        2,
        "the two markless takes are counted, never stamped"
    );
    let fam = lane_visible();
    let (n, sum) = phase(&fam, "alloc_lane_visible_phase_ns", "released_served");
    assert_eq!(n, 2, "two placed samples");
    assert_eq!(sum, 1_700 * 1_000_000, "700 + 1,000 ms, to the ns");
    assert_eq!(
        phase(&fam, "alloc_lane_visible_phase_ns", "served_visible").0,
        0,
        "the authority never stamps the co-writer's stage"
    );
}

/// **The co-writer's three stages close exactly.** Per sample `total` is
/// the sum of the two it was given; over N samples the sums close to the
/// ns; an unplaced age stamps nothing and counts.
#[test]
fn the_co_writer_stamps_three_stages_that_sum_exactly() {
    let _serial = serial();
    let ages = [700u64, 12, 0, 4_999, 250];
    let rtt = 3u64;
    for age in ages {
        free_grace::note_lane_visible(age, rtt);
    }
    free_grace::note_lane_visible(free_grace::LANE_RELEASE_AGE_UNPLACED, rtt);
    let fam = lane_visible();
    let (n_rs, sum_rs) = phase(&fam, "alloc_lane_visible_phase_ns", "released_served");
    let (n_sv, sum_sv) = phase(&fam, "alloc_lane_visible_phase_ns", "served_visible");
    let (n_t, sum_t) = phase(&fam, "alloc_lane_visible_phase_ns", "total");
    assert_eq!(
        (n_rs, n_sv, n_t),
        (5, 5, 5),
        "one sample per stage per placed block"
    );
    assert_eq!(sum_rs, ages.iter().sum::<u64>() * 1_000_000);
    assert_eq!(sum_sv, rtt * 5 * 1_000_000);
    assert_eq!(sum_t, sum_rs + sum_sv, "exact-sum closure to the ns");
    assert_eq!(free_grace::lane_visible_unplaced(), 1);
}

/// **The pushed-refill decision is pure and lever-gated**: harvest ⇔ the
/// lever is on and the hint says supply exists. The hint ALONE suffices
/// (the refill-hint gate, `.benchmarks/2026-09-07-lane-refill-hint-gate.md`):
/// the authority's own count of this lane's blocks on its lists is the
/// ground truth, and the owed ledger — the explicit-ship arm's face — is a
/// strict subset of it (on the s11 fleet ≈ 90 % of a co-writer's displaced
/// blocks return through the authority's publish recompute, which notes
/// nothing owed). `SQUEEZEFS_ALLOC_LANE_REFILL_HINT=0` restores the
/// owed-only gate verbatim (the A/B control).
#[test]
fn the_pushed_refill_decision_is_pure_and_lever_gated() {
    let _serial = serial();
    free_grace::test_set_lane_push(Some(true));
    assert!(free_grace::lane_push_wants_harvest(3, 1));
    assert!(
        !free_grace::lane_push_wants_harvest(0, 1),
        "no supply ⇒ no RPC"
    );
    assert!(
        free_grace::lane_push_wants_harvest(3, 0),
        "the hint alone suffices — the push exists because the authority said the lane has supply"
    );
    assert!(
        !free_grace::lane_push_wants_harvest(0, 0),
        "hint 0 and owed 0 ⇒ no RPC (the quiet-lane posture)"
    );
    // The refill-hint lever off: the retired owed-only gate.
    free_grace::test_set_refill_hint(Some(false));
    assert!(
        !free_grace::lane_push_wants_harvest(3, 0),
        "REFILL_HINT=0: owed nothing ⇒ no RPC (the retired gate verbatim)"
    );
    assert!(
        free_grace::lane_push_wants_harvest(3, 1),
        "REFILL_HINT=0: hint + owed still pushes"
    );
    assert!(free_grace::test_clear_refill_hint());
    free_grace::test_set_lane_push(Some(false));
    assert!(
        !free_grace::lane_push_wants_harvest(3, 1),
        "the lever off never pushes — the watermark tick and the park slices stand"
    );
    assert!(
        !free_grace::lane_push_wants_harvest(3, 0),
        "…and the hint alone cannot push with the lever off"
    );
    // The hint itself is inert with the lever off: nothing stored, nothing
    // woken.
    free_grace::note_lane_supply_hint(9);
    assert_eq!(free_grace::lane_supply_hint(), 0);
    assert_eq!(free_grace::lane_push_wakes(), 0);
    free_grace::test_set_lane_push(Some(true));
    free_grace::note_lane_supply_hint(9);
    assert_eq!(free_grace::lane_supply_hint(), 9);
    assert_eq!(free_grace::lane_push_wakes(), 1, "a nonzero hint is a wake");
    free_grace::note_lane_supply_hint(0);
    assert_eq!(free_grace::lane_supply_hint(), 0);
    assert_eq!(free_grace::lane_push_wakes(), 1, "a zero hint wakes nobody");
}

// ---------------------------------------------------------------------------
// 3 — real allocators: the safety law and the chained ledgers
// ---------------------------------------------------------------------------

/// The authority as a co-writer's harvest sink sees it: the SAME two
/// functions the product's `execute_lane_harvest` runs per pass (the ring
/// head, then the lane take with its ages), on a real allocator.
fn authority_sink(auth: &Arc<BlockAllocator>, lane: u16, writers: u16) -> LaneHarvestSink {
    let auth = Arc::clone(auth);
    Arc::new(move |max: u64| {
        let auth = Arc::clone(&auth);
        Box::pin(async move {
            auth.harvest_grace_to_front();
            let served = auth.take_lane_free_blocks(lane, writers, max as usize);
            let (blocks, release_ages_ms): (Vec<u64>, Vec<u64>) = served.into_iter().unzip();
            Ok(LaneHarvest {
                blocks,
                bound_age_hint_ms: free_grace::bound_age_ms(),
                rtt_ms: 2,
                release_ages_ms,
            })
        })
    })
}

/// **A deferred lane block is served by no harvest before its
/// acknowledgement, and once released the two ledgers chain.** The
/// authority (lane 0 of 2) `finish_free`s a lane-1 block: it enters the
/// ring; the co-writer's harvest (the ring head + the lane take) serves
/// NOTHING while the reader has not acknowledged; the acknowledgement
/// releases it to the authority's list WITH a release mark; 400 ms later
/// the co-writer's harvest takes it — `released_served` 400 on the
/// authority, and the reply carries 400 so the co-writer stamps 400 / 2 /
/// 402. The hold ledger's `min_acked→released` ended at the very instant
/// the lane ledger began.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deferred_lane_block_waits_for_the_ack_then_the_two_ledgers_chain() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let reader = join(&owner, "lv-reader", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    assert!(free_grace::armed());

    // The authority's allocator: lane 0 of 2, a small device.
    let auth = Arc::new(
        BlockAllocator::new("vol-00000000000000c1")
            .await
            .expect("allocator"),
    );
    let chunk = auth.chunk_size();
    auth.set_capacity_bytes(64 * chunk);
    auth.engage_alloc_lanes(part(2, 0)).expect("lane 0 of 2");
    // The co-writer's allocator: lane 1 of 2, wired to the authority.
    let cw = Arc::new(
        BlockAllocator::new("vol-00000000000000c1")
            .await
            .expect("allocator"),
    );
    cw.set_capacity_bytes(64 * chunk);
    cw.engage_alloc_lanes(part(2, 1)).expect("lane 1 of 2");
    cw.set_lane_harvest_sink(authority_sink(&auth, 1, 2));

    // A lane-1 block displaced by the co-writer, its free executed on the
    // authority: `finish_free` defers it into the ring.
    let idx = 9u64;
    assert_eq!(idx % 2, 1, "a lane-1 block");
    auth.finish_free(idx * chunk);
    assert!(auth.grace_holds(idx * chunk), "deferred, not free-listed");
    assert_eq!(auth.lane_free_count(1), 0, "nothing on the list for lane 1");

    // The co-writer polls (the authority's serve: the ring head, then the
    // lane take): nothing is served while the reader holds the label.
    let pushed0 = METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed);
    let owed_before = cw.lane_owed_blocks();
    cw.note_owed_freed(1);
    assert_eq!(cw.lane_owed_blocks(), owed_before + 1);
    auth.harvest_grace_to_front();
    assert!(
        auth.take_lane_free_blocks(1, 2, 8).is_empty(),
        "the safety law: an unacknowledged offset is served by nobody"
    );
    assert!(auth.grace_holds(idx * chunk));
    assert_eq!(
        free_grace::lane_visible_unplaced(),
        0,
        "an empty take stamps nothing"
    );

    // The reader acknowledges past the label: the harvest releases the
    // block to the authority's list WITH its release mark.
    let label = auth.grace_oldest_label().expect("held");
    assert!(matches!(
        owner.renew("lv-reader", reader.epoch, label),
        RenewOutcome::Renewed(_)
    ));
    owner.refresh_free_grace_bound();
    auth.harvest_grace_to_front();
    assert!(!auth.grace_holds(idx * chunk), "released");
    assert_eq!(
        auth.lane_free_count(1),
        1,
        "on the authority's list, lane 1"
    );
    assert_eq!(
        auth.foreign_lane_free_blocks(),
        1,
        "the supply held for a peer"
    );
    assert_eq!(
        free_grace::lane_release_marks(),
        1,
        "marked for the lane ledger"
    );
    let hold = free_grace::stats_snapshot();
    assert_eq!(
        hold["free_grace_hold_phase_ns"]["total"]["count"].as_u64(),
        Some(1),
        "the hold ledger stamped the release"
    );

    // 400 ms later the co-writer's refill takes it — the PUSHED form: the
    // renewal grant's hint (the authority's lane-1 count) wakes it. The
    // reply carries the age the authority measured, and both ledgers stamp.
    ticks.fetch_add(400, Ordering::SeqCst);
    free_grace::test_set_lane_push(Some(true));
    free_grace::note_lane_supply_hint(auth.lane_free_count(1));
    assert_eq!(free_grace::lane_push_wakes(), 1, "a nonzero hint is a wake");
    let adopted = cw.pushed_refill_tick(3).await;
    assert_eq!(adopted, 1, "the released block is adopted");
    assert_eq!(
        METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed),
        pushed0 + 1,
        "the pushed harvest is counted"
    );
    assert_eq!(auth.lane_free_count(1), 0);
    assert_eq!(free_grace::lane_release_marks(), 0);
    assert_eq!(
        cw.lane_owed_blocks(),
        owed_before,
        "the owed ledger paid down"
    );
    let fam = lane_visible();
    // The authority's stamp and the co-writer's stamp of the SAME block
    // both read 400 (one process plays both nodes here).
    let (n_rs, sum_rs) = phase(&fam, "alloc_lane_visible_phase_ns", "released_served");
    assert_eq!(
        n_rs, 2,
        "the authority's stamp at the take + the co-writer's from the reply"
    );
    assert_eq!(sum_rs, 800 * 1_000_000);
    let (n_sv, sum_sv) = phase(&fam, "alloc_lane_visible_phase_ns", "served_visible");
    assert_eq!(
        (n_sv, sum_sv),
        (1, 2 * 1_000_000),
        "the co-writer's round trip"
    );
    let (n_t, sum_t) = phase(&fam, "alloc_lane_visible_phase_ns", "total");
    assert_eq!((n_t, sum_t), (1, 402 * 1_000_000), "exact-sum");
    assert_eq!(free_grace::lane_visible_unplaced(), 0);
    // And the block is now mintable by the co-writer — in its lane.
    let off = cw.allocate_block().await.expect("the adopted block mints");
    assert_eq!(off / chunk, idx, "the very block the authority released");
}

/// **The pushed refill fires on the hint alone** (the refill-hint gate,
/// `.benchmarks/2026-09-07-lane-refill-hint-gate.md`): a lane-1 block the
/// AUTHORITY freed without the co-writer ever shipping it — the publish
/// recompute's arm, `meta_ship_publish.free_recomputed_blocks`, which on
/// the s11 fleet is ≈ 90 % of a co-writer's displaced blocks — is on the
/// authority's list with the co-writer OWED nothing. The renewal grant's
/// hint says the lane has supply; the pushed refill harvests it, counted
/// as a hint refill (`alloc_lane_hint_refills` — a proactive harvest the
/// owed gate would have declined), and the owed ledger never moves (it is
/// the explicit-ship arm's face, not the gate). Before the fix this
/// block was reachable only from inside an ENOSPC park.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pushed_refill_fires_on_the_hint_alone_for_a_recomputed_free() {
    let _serial = serial();
    free_grace::test_set_lane_push(Some(true));
    let auth = Arc::new(
        BlockAllocator::new("vol-00000000000000c2")
            .await
            .expect("allocator"),
    );
    let chunk = auth.chunk_size();
    auth.set_capacity_bytes(64 * chunk);
    auth.engage_alloc_lanes(part(2, 0)).expect("lane 0 of 2");
    let cw = Arc::new(
        BlockAllocator::new("vol-00000000000000c2")
            .await
            .expect("allocator"),
    );
    cw.set_capacity_bytes(64 * chunk);
    cw.engage_alloc_lanes(part(2, 1)).expect("lane 1 of 2");
    cw.set_lane_harvest_sink(authority_sink(&auth, 1, 2));

    // No plane armed: the authority's terminal free publishes straight to
    // its free list — the recompute arm's effect as the co-writer sees it
    // (nothing shipped, nothing noted owed).
    let idx = 11u64;
    assert_eq!(idx % 2, 1, "a lane-1 block");
    auth.finish_free(idx * chunk);
    assert_eq!(
        auth.lane_free_count(1),
        1,
        "on the authority's list, lane 1"
    );
    assert_eq!(cw.lane_owed_blocks(), 0, "the co-writer is owed NOTHING");

    let pushed0 = METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed);
    let hint_refills0 = METRICS.alloc_lane_hint_refills.load(Ordering::Relaxed);
    let owed_gauge0 = METRICS.alloc_lane_owed_blocks.load(Ordering::Relaxed);

    // A wake with hint 0 (the authority's list holds nothing of this lane)
    // pushes nothing: the quiet-lane posture — no RPC storms on an idle lane.
    free_grace::note_lane_supply_hint(0);
    assert_eq!(
        cw.pushed_refill_tick(1).await,
        0,
        "hint 0 ∧ owed 0 ⇒ no RPC"
    );
    assert_eq!(
        METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed),
        pushed0,
        "no pushed harvest counted"
    );

    // The renewal grant carries the authority's count: the push fires.
    free_grace::note_lane_supply_hint(auth.lane_free_count(1));
    assert_eq!(
        cw.pushed_refill_tick(2).await,
        1,
        "the recomputed block is adopted"
    );
    assert_eq!(
        METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed),
        pushed0 + 1,
        "the pushed harvest is counted"
    );
    assert_eq!(
        METRICS.alloc_lane_hint_refills.load(Ordering::Relaxed),
        hint_refills0 + 1,
        "…and as a HINT refill: the owed gate would have declined it"
    );
    assert_eq!(cw.lane_owed_blocks(), 0, "the owed ledger is untouched");
    assert_eq!(
        METRICS.alloc_lane_owed_blocks.load(Ordering::Relaxed),
        owed_gauge0,
        "the sum gauge too (nothing to pay down)"
    );
    assert_eq!(
        auth.lane_free_count(1),
        0,
        "the authority's list is drained"
    );
    let off = cw.allocate_block().await.expect("the adopted block mints");
    assert_eq!(off / chunk, idx, "the very block the authority recomputed");

    // The A/B control: `REFILL_HINT=0` is the owed-only gate — the same
    // shape declines the push with owed 0.
    let idx2 = 13u64;
    auth.finish_free(idx2 * chunk);
    free_grace::note_lane_supply_hint(auth.lane_free_count(1));
    free_grace::test_set_refill_hint(Some(false));
    assert_eq!(
        cw.pushed_refill_tick(3).await,
        0,
        "REFILL_HINT=0: owed nothing ⇒ the retired gate declines"
    );
    assert!(free_grace::test_clear_refill_hint());
    assert_eq!(
        METRICS.alloc_lane_hint_refills.load(Ordering::Relaxed),
        hint_refills0 + 1,
        "a declined push is not a hint refill"
    );
}

// ---------------------------------------------------------------------------
// 5–7 — the co-writer-lane model at the fleet cadences
// ---------------------------------------------------------------------------
//
// The D-4 closed loop (tests/reader_free_grace_tests.rs) with the lane leg
// the fleet has and it had not: the starving streams are CO-WRITER lanes
// whose supply is RPC-mediated, and the authority mints nothing itself —
// so its harvest runs only on the events the fleet gives it (a shipped
// free landing, a co-writer's harvest RPC, and — lever on — a binding
// ack). Product code decides every step: the ring, the ladder, the owner's
// renewal (which carries the hint and runs the release hook), the pushed
// decision; the sim performs only the acts (an RPC, an adoption).

const LANES: u64 = 16;
const BLOCK: u64 = 4 * 1024 * 1024;
const TAG: u64 = 0x1a1e;
const GRAIN: usize = 64;
/// One simulated round trip on the wire, ms.
const RTT_MS: u64 = 1;
/// The co-writer's ENOSPC park slice — `pressure_park_slice_ms()` on a
/// mount with no plane (a co-writer): 50 ms; and its wall backstop, the
/// `pressure_park_wall_ms()` floor.
const CW_SLICE_MS: u64 = 50;
const CW_WALL_MS: u64 = 1_000;
const AHEAD_TICK_MS: u64 = 1_000;
const STEP_MS: u64 = 1;

#[derive(Debug, Clone, Copy)]
struct LaneShape {
    label: &'static str,
    /// Co-writers — each a lane 1..=members of `LANES`, each a reader too.
    members: usize,
    /// Each co-writer's virgin spare at t0, blocks.
    spare: u64,
    /// Each co-writer's rewrite demand, blocks/s.
    demand_per_s: u64,
    duration_ms: u64,
    checkpoint_ms: u64,
    /// The fleet's iteration structure: `write_ms` of demand, then
    /// `pause_ms` of silence (the close / barrier); `pause_ms == 0` is a
    /// continuous storm.
    write_ms: u64,
    pause_ms: u64,
}

/// The authority's data plane as the sim needs it: the ring, and the free
/// list partitioned by lane (the lane-counted free set's per-lane view).
struct SimAuthority {
    ring: GraceRing,
    lists: Vec<VecDeque<u64>>,
}

impl SimAuthority {
    fn new() -> Self {
        Self {
            ring: GraceRing::new(1 << 22),
            lists: (0..LANES).map(|_| VecDeque::new()).collect(),
        }
    }

    /// The routine harvest (the ring head every demand event runs): the
    /// released offsets reach the free list and, being co-writer lanes',
    /// are MARKED for the lane ledger — `BlockAllocator::publish_grace_release`.
    fn harvest(&mut self, max: usize) -> usize {
        let supply: u64 = self.lists.iter().map(|l| l.len() as u64).sum();
        let released = self.ring.harvest_with_supply(max, supply, 0);
        let n = released.len();
        for off in released {
            let idx = off / BLOCK;
            self.lists[(idx % LANES) as usize].push_back(idx);
            free_grace::mark_lane_release(TAG, idx);
        }
        n
    }

    /// The release-on-ack hook: to the uncovered front.
    fn harvest_to_front(&mut self) {
        while self.harvest(free_grace::HARVEST_BATCH) == free_grace::HARVEST_BATCH {}
    }

    /// `execute_lane_harvest`: the ring head, then the lane take with its
    /// ages; an empty pass 0 walks to the pressure pass.
    fn serve(&mut self, lane: u64, max: usize) -> Vec<(u64, u64)> {
        self.harvest(free_grace::HARVEST_BATCH);
        let mut out = self.take(lane, max);
        if out.is_empty() {
            let released = self.ring.harvest_pressure(free_grace::HARVEST_BATCH);
            for off in released {
                let idx = off / BLOCK;
                self.lists[(idx % LANES) as usize].push_back(idx);
                free_grace::mark_lane_release(TAG, idx);
            }
            out = self.take(lane, max);
        }
        out
    }

    fn take(&mut self, lane: u64, max: usize) -> Vec<(u64, u64)> {
        let list = &mut self.lists[lane as usize];
        let mut out = Vec::new();
        while out.len() < max {
            let Some(idx) = list.pop_front() else { break };
            let age = free_grace::take_lane_release(TAG, idx)
                .unwrap_or(free_grace::LANE_RELEASE_AGE_UNPLACED);
            out.push((idx, age));
        }
        out
    }

    fn lane_supply(&self, lane: u64) -> u64 {
        self.lists[lane as usize].len() as u64
    }
}

struct SimCoWriter {
    id: String,
    lane: u64,
    epoch: u64,
    // -- the reader half (the ladder is product code) --
    ladder: ReaderAckLadder,
    learned: (u64, u64),
    acked: u64,
    next_renew_ms: u64,
    next_pass_ms: u64,
    last_pass_ms: u64,
    // -- the lane half --
    local_free: VecDeque<u64>,
    virgin: u64,
    mint: u64,
    owed: u64,
    acc: u64,
    parked_until_ms: u64,
    park_started_ms: Option<u64>,
    next_ahead_ms: u64,
    /// The product's claim-rate EWMA (milli-blocks/s) and the watermark it
    /// derives (`BlockAllocator::sample_alloc_rate`'s arithmetic).
    claims: u64,
    rate_last_claims: u64,
    rate_mblk_per_s: u64,
    // -- the row's counters --
    rpcs: u64,
    empty_rpcs: u64,
    pushed_rpcs: u64,
    /// Pushed RPCs that found nothing — must stay 0: the hint is the
    /// authority's exact count, so a push always finds its supply.
    pushed_empty: u64,
    terminal_refusals: u64,
}

impl SimCoWriter {
    fn reachable(&self) -> u64 {
        self.local_free.len() as u64 + self.virgin
    }

    /// `sample_alloc_rate` once per second: EWMA α = 1/4, watermark =
    /// ceil(rate × horizon) capped at share/4, horizon = the authority's
    /// live bound age + RTT + one floor (the hinted form).
    fn watermark(&mut self, share: u64) -> u64 {
        let inst_mblk = (self.claims - self.rate_last_claims) * 1_000;
        self.rate_last_claims = self.claims;
        let old = self.rate_mblk_per_s;
        self.rate_mblk_per_s = old.saturating_sub(old.div_ceil(4)) + inst_mblk / 4;
        let horizon = free_grace::bound_age_ms().saturating_add(RTT_MS + AHEAD_TICK_MS);
        let inflight = if self.rate_mblk_per_s == 0 {
            0
        } else {
            self.rate_mblk_per_s
                .saturating_mul(horizon)
                .div_ceil(1_000_000)
        };
        inflight.min(share / 4)
    }
}

#[derive(Debug, Clone)]
struct LaneRow {
    lever: bool,
    steady_allocs_per_s: f64,
    stalls: u64,
    terminal_refusals: u64,
    deferrals: u64,
    releases: u64,
    held_end: u64,
    forced: u64,
    fences: u64,
    bound_age_mean_ms: f64,
    hold_acked_rel_ms: f64,
    /// `min_acked→released` samples past one second — the fleet's tail.
    hold_acked_rel_tail: u64,
    hold_total_ms: f64,
    vis_released_served_ms: f64,
    vis_served_visible_ms: f64,
    vis_total_ms: f64,
    vis_samples: u64,
    vis_unplaced: u64,
    vis_closure_ok: bool,
    /// Release marks outstanding at the end: released co-writer-lane
    /// blocks still on the authority's list, asked for by nobody.
    marks_end: u64,
    rpcs: u64,
    empty_rpcs: u64,
    pushed_rpcs: u64,
    pushed_empty: u64,
    push_releases: u64,
    push_hints: u64,
    push_wakes: u64,
}

impl LaneRow {
    fn render(&self, shape: &LaneShape) -> String {
        format!(
            "ROW {label} lane_push={lever}: per-cw {mibs:.1} MiB/s ({allocs:.2} blk/s) stalls {stalls} \
             terminal {term} | deferrals {d} releases {r} held {h} closure {clo} | forced {f} fences {fe} | \
             bound_age mean {ba:.0} ms | hold min_acked→rel {hrel:.0} ms (>1s: {htail}) total {ht:.0} ms | \
             lane_visible released→served {rs:.0} served→visible {sv:.0} total {vt:.0} ms \
             (n {vn}, unplaced {vu}, closure {vc}) marks_end {me} | rpcs {rpc} empty {erpc} pushed {prpc} (empty {pemp}) | \
             push releases {prel} hints {phint} wakes {pwake}",
            label = shape.label,
            lever = self.lever,
            mibs = self.steady_allocs_per_s * 4.0,
            allocs = self.steady_allocs_per_s,
            stalls = self.stalls,
            term = self.terminal_refusals,
            d = self.deferrals,
            r = self.releases,
            h = self.held_end,
            clo = if self.deferrals == self.releases + self.held_end { "OK" } else { "BROKEN" },
            f = self.forced,
            fe = self.fences,
            ba = self.bound_age_mean_ms,
            hrel = self.hold_acked_rel_ms,
            htail = self.hold_acked_rel_tail,
            ht = self.hold_total_ms,
            rs = self.vis_released_served_ms,
            sv = self.vis_served_visible_ms,
            vt = self.vis_total_ms,
            vn = self.vis_samples,
            vu = self.vis_unplaced,
            vc = if self.vis_closure_ok { "OK" } else { "BROKEN" },
            me = self.marks_end,
            rpc = self.rpcs,
            erpc = self.empty_rpcs,
            prpc = self.pushed_rpcs,
            pemp = self.pushed_empty,
            prel = self.push_releases,
            phint = self.push_hints,
            pwake = self.push_wakes,
        )
    }
}

/// One co-writer's harvest RPC: the authority serves (its clock), the
/// co-writer adopts one RTT later (its clock) — the product's
/// `harvest_lane_supply` stamps `note_lane_visible(age, rtt)` per block.
fn rpc(auth: &Arc<Mutex<SimAuthority>>, cw: &mut SimCoWriter) -> usize {
    cw.rpcs += 1;
    let served = auth.lock().unwrap().serve(cw.lane, GRAIN);
    let n = served.len();
    if n == 0 {
        cw.empty_rpcs += 1;
    }
    for (idx, age) in served {
        cw.local_free.push_back(idx);
        free_grace::note_lane_visible(age, RTT_MS);
    }
    cw.owed = cw.owed.saturating_sub(n as u64);
    n
}

fn run_lane_loop(shape: &LaneShape, lever: bool) -> LaneRow {
    free_grace::reset_for_test();
    membership::uninstall();
    // The shipped configuration of every other lever; this campaign's
    // lever per the row.
    free_grace::test_set_ack_pipeline(Some(true));
    free_grace::test_set_demand(Some(true));
    free_grace::test_set_ack_renewal(Some(true));
    free_grace::test_set_refresh_on_ack(Some(true));
    free_grace::test_set_lane_push(Some(lever));
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let clocks = owner.clocks().clone();
    let renew_ms = clocks.renew_interval.as_millis() as u64;
    let skew_ms = clocks.skew_max.as_millis() as u64;
    let staleness_ms = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
    let pass_ms = squeezefs::ro_coherence::reader_revalidate_interval().as_millis() as u64;
    let qualify_lag_ms = staleness_ms + skew_ms;
    let drain_lag_ms = staleness_ms + clocks.d_purge.as_millis() as u64;
    let refresh_floor_ms = pass_ms.max(skew_ms);

    let auth = Arc::new(Mutex::new(SimAuthority::new()));
    // The product's two authority-side installs (`multi_writer::arm`):
    // the release hook and the per-member lane-supply source. Both read
    // the same sim plane the RPCs serve from.
    {
        let a = Arc::clone(&auth);
        free_grace::install_release_hook(Arc::new(move || {
            a.lock().unwrap().harvest_to_front();
        }));
        let a = Arc::clone(&auth);
        free_grace::install_lane_supply_source(Arc::new(move |id: &str| {
            let lane: u64 = id
                .rsplit('-')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            a.lock().unwrap().lane_supply(lane)
        }));
    }

    let t0 = clock.now_ms();
    let share = shape.spare * 2;
    let mut cws: Vec<SimCoWriter> = (1..=shape.members as u64)
        .map(|lane| {
            let id = format!("sim-cw-{lane}");
            let grant = join(&owner, &id, MemberRole::Writer);
            let i = lane - 1;
            SimCoWriter {
                id,
                lane,
                epoch: grant.epoch,
                ladder: ReaderAckLadder::new(),
                learned: (grant.granted_at_owner_ms, t0),
                acked: 0,
                next_renew_ms: t0 + renew_ms * (i + 1) / shape.members as u64,
                next_pass_ms: t0 + pass_ms + pass_ms * i / shape.members as u64,
                last_pass_ms: t0,
                local_free: VecDeque::new(),
                virgin: shape.spare,
                mint: 0,
                owed: 0,
                acc: 0,
                parked_until_ms: 0,
                park_started_ms: None,
                next_ahead_ms: t0 + AHEAD_TICK_MS + AHEAD_TICK_MS * i / shape.members as u64,
                claims: 0,
                rate_last_claims: 0,
                rate_mblk_per_s: 0,
                rpcs: 0,
                empty_rpcs: 0,
                pushed_rpcs: 0,
                pushed_empty: 0,
                terminal_refusals: 0,
            }
        })
        .collect();
    owner.refresh_free_grace_bound();
    assert!(free_grace::armed());

    let mut next_checkpoint_ms = t0 + shape.checkpoint_ms;
    let mut last_checkpoint_ms = t0;
    let mut next_sweep_ms = t0 + renew_ms;
    let mut next_sample_ms = t0 + pass_ms;
    let mut bound_age_samples: Vec<u64> = Vec::new();
    let mut stalls = 0u64;
    let third_ms = shape.duration_ms / 3;
    let steady_from_ms = t0 + third_ms;
    let end = t0 + shape.duration_ms;
    let mut steady_landed = 0u64;
    let iteration_ms = shape.write_ms + shape.pause_ms;

    while clock.now_ms() < end {
        ticks.fetch_add(STEP_MS, Ordering::SeqCst);
        let now = clock.now_ms();
        let writing = shape.pause_ms == 0 || ((now - t0) % iteration_ms) < shape.write_ms;
        let steady = now >= steady_from_ms;

        if now >= next_checkpoint_ms {
            free_grace::note_checkpoint_completed(Duration::from_millis(20));
            last_checkpoint_ms = now;
            next_checkpoint_ms = now + shape.checkpoint_ms;
        }

        for cw in cws.iter_mut() {
            // The lane stream: allocate from the local list, then virgin,
            // else the ENOSPC arm — harvest RPC, park a slice, and past the
            // wall the write's refusal is terminal (the co-writer wall law).
            if writing {
                cw.acc = (cw.acc + shape.demand_per_s * STEP_MS).min(16_000);
                while cw.acc >= 1_000 && now >= cw.parked_until_ms {
                    let landed = if cw.local_free.pop_front().is_some() {
                        true
                    } else if cw.virgin > 0 {
                        cw.virgin -= 1;
                        true
                    } else if rpc(&auth, cw) > 0 {
                        cw.local_free.pop_front();
                        true
                    } else {
                        stalls += 1;
                        let started = *cw.park_started_ms.get_or_insert(now);
                        if now.saturating_sub(started) >= CW_WALL_MS {
                            cw.terminal_refusals += 1;
                            cw.park_started_ms = None;
                            cw.acc -= 1_000;
                        }
                        cw.parked_until_ms = now + CW_SLICE_MS;
                        false
                    };
                    if !landed {
                        break;
                    }
                    cw.park_started_ms = None;
                    cw.acc -= 1_000;
                    cw.claims += 1;
                    if steady {
                        steady_landed += 1;
                    }
                    // The rewrite displaces one of this lane's blocks: its
                    // free ships and the authority `finish_free`s it —
                    // deferred into the ring, the ring head run.
                    let displaced = cw.mint * LANES + cw.lane;
                    cw.mint += 1;
                    let mut a = auth.lock().unwrap();
                    assert!(a.ring.defer(displaced * BLOCK, BLOCK), "armed ⇒ deferred");
                    a.harvest(free_grace::HARVEST_BATCH);
                    drop(a);
                    cw.owed += 1;
                }
            }
            // The ahead-refill tick (product: `ahead_refill_tick`).
            if now >= cw.next_ahead_ms {
                let wm = cw.watermark(share);
                if cw.owed > 0 && cw.reachable() < wm {
                    rpc(&auth, cw);
                }
                cw.next_ahead_ms = now + AHEAD_TICK_MS;
            }
            // The reader half: passes and renewals — lever (b) carriage.
            if now >= cw.next_pass_ms {
                let promoted = cw.ladder.note_pass(AckInputs {
                    label: cw.learned.0,
                    learned_at_ms: cw.learned.1,
                    pass_start_ms: now,
                    now_ms: now,
                    advanced: last_checkpoint_ms > cw.last_pass_ms,
                    qualify_lag_ms,
                    drain_lag_ms,
                    refresh_floor_ms,
                    // This model pins the lane-visible campaign on the
                    // pre-re-derivation windows: the timer alone decides.
                    drain_gen: 0,
                    drain_budget_ms: clocks.d_purge.as_millis() as u64,
                });
                cw.last_pass_ms = now;
                if let Some(label) = promoted {
                    cw.acked = label;
                    if free_grace::ack_renewal_enabled() {
                        match owner.renew(&cw.id, cw.epoch, cw.acked) {
                            RenewOutcome::Renewed(grant) => {
                                // The lever's co-writer half: the grant's
                                // hint wakes the refill (product:
                                // `note_lane_supply_hint` → the ahead task's
                                // `pushed_refill_tick`).
                                if free_grace::lane_push_wants_harvest(
                                    grant.lane_supply_blocks,
                                    cw.owed,
                                ) {
                                    cw.pushed_rpcs += 1;
                                    if rpc(&auth, cw) == 0 {
                                        cw.pushed_empty += 1;
                                    }
                                }
                            }
                            other => panic!("a carriage renewal is admitted: {other:?}"),
                        }
                    }
                }
                cw.next_pass_ms += pass_ms;
            }
            if now >= cw.next_renew_ms {
                match owner.renew(&cw.id, cw.epoch, cw.acked) {
                    RenewOutcome::Renewed(grant) => {
                        cw.learned = (grant.granted_at_owner_ms, now);
                        cw.next_renew_ms = now + grant.renew_ms.max(STEP_MS);
                        if free_grace::lane_push_wants_harvest(grant.lane_supply_blocks, cw.owed) {
                            cw.pushed_rpcs += 1;
                            if rpc(&auth, cw) == 0 {
                                cw.pushed_empty += 1;
                            }
                        }
                    }
                    other => panic!("a healthy member's renewal is admitted: {other:?}"),
                }
            }
        }

        if now >= next_sweep_ms {
            owner.refresh_free_grace_bound();
            next_sweep_ms += renew_ms;
        }
        if now >= next_sample_ms {
            if steady {
                bound_age_samples.push(free_grace::bound_age_ms());
            }
            next_sample_ms += pass_ms;
        }
    }

    let steady_secs = (shape.duration_ms - third_ms) as f64 / 1_000.0;
    let snap = free_grace::stats_snapshot();
    let fam = lane_visible();
    let hold_mean = |name: &str| -> f64 {
        snap["free_grace_hold_phase_ns"][name]["mean_ns"]
            .as_u64()
            .unwrap_or(0) as f64
            / 1e6
    };
    let (n_rs, sum_rs) = phase(&fam, "alloc_lane_visible_phase_ns", "released_served");
    let (_n_sv, sum_sv) = phase(&fam, "alloc_lane_visible_phase_ns", "served_visible");
    let (n_t, sum_t) = phase(&fam, "alloc_lane_visible_phase_ns", "total");
    // One process plays both nodes: `released_served` carries the
    // authority's stamp AND the co-writer's of every block, so its count is
    // twice the total's and its sum twice the co-writer's share.
    let vis_closure_ok = n_rs == 2 * n_t && sum_t == sum_rs / 2 + sum_sv;
    let row = LaneRow {
        lever,
        steady_allocs_per_s: steady_landed as f64 / steady_secs / shape.members as f64,
        stalls,
        terminal_refusals: cws.iter().map(|c| c.terminal_refusals).sum(),
        deferrals: free_grace::deferrals(),
        releases: free_grace::releases(),
        held_end: free_grace::held_offsets(),
        forced: free_grace::forced_releases(),
        fences: free_grace::laggard_fences(),
        bound_age_mean_ms: bound_age_samples.iter().sum::<u64>() as f64
            / bound_age_samples.len().max(1) as f64,
        hold_acked_rel_ms: hold_mean("min_acked_released"),
        hold_acked_rel_tail: phase_tail_over_1s(
            &snap,
            "free_grace_hold_phase_ns",
            "min_acked_released",
        ),
        hold_total_ms: hold_mean("total"),
        vis_released_served_ms: phase_mean_ms(
            &fam,
            "alloc_lane_visible_phase_ns",
            "released_served",
        ),
        vis_served_visible_ms: phase_mean_ms(&fam, "alloc_lane_visible_phase_ns", "served_visible"),
        vis_total_ms: phase_mean_ms(&fam, "alloc_lane_visible_phase_ns", "total"),
        vis_samples: n_t,
        vis_unplaced: free_grace::lane_visible_unplaced(),
        vis_closure_ok,
        marks_end: free_grace::lane_release_marks(),
        rpcs: cws.iter().map(|c| c.rpcs).sum(),
        empty_rpcs: cws.iter().map(|c| c.empty_rpcs).sum(),
        pushed_rpcs: cws.iter().map(|c| c.pushed_rpcs).sum(),
        pushed_empty: cws.iter().map(|c| c.pushed_empty).sum(),
        push_releases: free_grace::lane_push_releases(),
        push_hints: free_grace::lane_push_hints(),
        push_wakes: free_grace::lane_push_wakes(),
    };
    println!("{}", row.render(shape));
    drop(cws);
    assert!(free_grace::test_clear_ack_pipeline());
    assert!(free_grace::test_clear_demand());
    assert!(free_grace::test_clear_ack_renewal());
    assert!(free_grace::test_clear_refresh_on_ack());
    assert!(free_grace::test_clear_lane_push());
    free_grace::uninstall_release_hook();
    free_grace::uninstall_lane_supply_source();
    row
}

/// The laws every row obeys, lever on or off.
fn assert_safety(row: &LaneRow) {
    assert_eq!(
        row.deferrals,
        row.releases + row.held_end,
        "closure: deferrals ≡ releases + held"
    );
    assert_eq!(row.forced, 0, "no forced release");
    assert_eq!(row.fences, 0, "no laggard fenced");
    assert!(row.vis_closure_ok, "the lane-visible family closes exactly");
    assert_eq!(
        row.vis_unplaced, 0,
        "every served block had its release mark"
    );
    assert_eq!(
        row.pushed_empty, 0,
        "a pushed RPC always finds its supply — the hint is the authority's exact count"
    );
}

/// The fleet's s11 shape: 8 co-writers at ~50 blk/s each (200 MiB/s), a
/// 1 s checkpoint, 4 GiB lanes with ~700 spare blocks (the row's live
/// share ≈ 320 of 1,024), and the iteration structure the row ran — ~4 s
/// of writing, then a close of the given length.
fn fleet_shape(label: &'static str, pause_ms: u64, duration_ms: u64) -> LaneShape {
    LaneShape {
        label,
        members: 8,
        spare: 700,
        demand_per_s: 50,
        duration_ms,
        checkpoint_ms: 1_000,
        write_ms: 3_000,
        pause_ms,
    }
}

/// **Contract 5 — the fleet's quiet phases are the third stage, and the
/// lever removes them.** With the storm pausing 12 s every 16 s (the
/// row's long closes, scaled), covered offsets sit unreleased until
/// demand resumes: lever OFF, `min_acked→released` reads SECONDS; lever
/// ON, the binding ack's arrival releases them and the stage reads
/// lever (d)'s rate limit (`floor ÷ members` = 125 ms at 8 members) or
/// less. Every co-writer's supply is refilled on the renewal's heels
/// (`pushed_rpcs > 0`, `push_releases > 0`, `push_hints > 0`).
#[test]
fn a_quiet_fleet_phase_is_the_third_stage_and_release_on_ack_removes_it() {
    let _serial = serial();
    let shape = fleet_shape("fleet(8cw,50/s,write 3s,pause 21s)", 21_000, 240_000);
    let off = run_lane_loop(&shape, false);
    let on = run_lane_loop(&shape, true);
    assert_safety(&off);
    assert_safety(&on);
    assert!(
        off.hold_acked_rel_ms >= 1_000.0,
        "lever OFF: covered offsets wait for the next demand event — the fleet's shape \
         (got {:.0} ms)",
        off.hold_acked_rel_ms
    );
    let rate_limit_ms =
        free_grace::refresh_on_ack_interval_ms(1_000, shape.members as u64, 0) as f64;
    assert!(
        on.hold_acked_rel_ms <= rate_limit_ms,
        "lever ON: the release follows the ack within lever (d)'s rate limit ({rate_limit_ms} \
         ms) — got {:.0} ms",
        on.hold_acked_rel_ms
    );
    assert!(
        on.hold_acked_rel_ms * 4.0 < off.hold_acked_rel_ms,
        "the stage collapses by more than 4× ({:.0} → {:.0} ms)",
        off.hold_acked_rel_ms,
        on.hold_acked_rel_ms
    );
    assert!(on.push_releases > 0, "release-on-ack engaged");
    assert!(on.push_hints > 0, "the grants carried hints");
    assert!(on.pushed_rpcs > 0, "hints woke refills");
    assert_eq!(
        off.push_releases + off.push_hints + off.pushed_rpcs,
        0,
        "OFF: zero engagement"
    );
    assert!(
        on.vis_released_served_ms <= off.vis_released_served_ms,
        "the lane hop never grows under the lever ({:.0} → {:.0} ms)",
        off.vis_released_served_ms,
        on.vis_released_served_ms
    );
}

/// **Contract 6 — the supply-adequate shape: a co-writer with supply never
/// asks, so its released blocks sit on the authority for the whole run;
/// the pushed refill brings them home.** With spare above hold × demand
/// nothing ever parks and the lane-reachable stock never dips below the
/// watermark, so the ONLY refill trigger the shipped tree has — the
/// watermark tick — never fires: lever OFF, ZERO harvest RPCs, and every
/// released block is still on the authority's list at the end
/// (`marks_end == releases`); lever ON, the hint on the renewal grant
/// wakes the refill (`pushed > 0`), the blocks come home within the
/// renewal cadence, and no RPC ever finds nothing.
#[test]
fn a_supplied_co_writer_never_asks_and_the_push_brings_its_blocks_home() {
    let _serial = serial();
    let shape = LaneShape {
        label: "adequate(8cw,20/s,spare 2000,continuous)",
        members: 8,
        spare: 2_000,
        demand_per_s: 20,
        duration_ms: 90_000,
        checkpoint_ms: 1_000,
        write_ms: 0,
        pause_ms: 0,
    };
    let off = run_lane_loop(&shape, false);
    let on = run_lane_loop(&shape, true);
    assert_safety(&off);
    assert_safety(&on);
    assert_eq!(off.stalls + on.stalls, 0, "supply-adequate: nothing parks");
    assert_eq!(
        off.rpcs, 0,
        "lever OFF: a co-writer above its watermark never asks the authority"
    );
    assert_eq!(
        off.marks_end, off.releases,
        "lever OFF: every released block is still on the authority's list at the end"
    );
    assert!(
        on.pushed_rpcs > 0,
        "lever ON: the hints woke pushed refills"
    );
    assert!(
        on.marks_end * 10 < on.releases,
        "lever ON: the released blocks came home ({} of {} still on the list)",
        on.marks_end,
        on.releases
    );
    let renew_ms = shipped_clocks().renew_interval.as_millis() as f64;
    assert!(
        on.vis_released_served_ms <= renew_ms,
        "lever ON: a released block is home within one renewal cadence ({renew_ms} ms) — \
         got {:.0} ms",
        on.vis_released_served_ms
    );
    assert_eq!(
        on.empty_rpcs, 0,
        "no RPC of any kind found nothing on this shape"
    );
    assert!(
        (on.steady_allocs_per_s - off.steady_allocs_per_s).abs() < 0.5,
        "a supply-adequate stream is not paced by the loop either way"
    );
}

/// **Contract 7 — the lever off is the shipped shape verbatim, and the
/// recycle-bound storm keeps every law under both.** The continuous
/// recycle-bound shape (spare below hold × demand): the parked co-writers'
/// 50 ms slices are the poll, so the lane hop is short OFF and stays short
/// ON; closure, zero forced/fences, exact-sum on both; OFF engages
/// nothing.
#[test]
fn the_recycle_bound_storm_keeps_every_law_under_both_settings() {
    let _serial = serial();
    let shape = LaneShape {
        label: "bound(8cw,50/s,spare 200,continuous)",
        members: 8,
        spare: 200,
        demand_per_s: 50,
        duration_ms: 90_000,
        checkpoint_ms: 1_000,
        write_ms: 0,
        pause_ms: 0,
    };
    let off = run_lane_loop(&shape, false);
    let on = run_lane_loop(&shape, true);
    assert_safety(&off);
    assert_safety(&on);
    assert!(off.stalls > 0, "recycle-bound: the streams park");
    assert_eq!(
        off.push_releases + off.push_hints + off.pushed_rpcs,
        0,
        "OFF: zero engagement"
    );
    assert!(
        on.vis_released_served_ms <= off.vis_released_served_ms + 5.0,
        "the lane hop never grows under the lever ({:.0} → {:.0} ms)",
        off.vis_released_served_ms,
        on.vis_released_served_ms
    );
    assert!(
        on.steady_allocs_per_s >= off.steady_allocs_per_s * 0.95,
        "the lever never slows a recycle-bound stream ({:.2} → {:.2} blk/s)",
        off.steady_allocs_per_s,
        on.steady_allocs_per_s
    );
}

/// **The 4 GiB-equivalent lane budget, continuous (the fleet's lanes:
/// 1,024 blocks, ≈ 320 live ⇒ ≈ 700 spare, 50 blk/s), and the same lane at
/// the capacity law's edge (spare ≈ hold × churn).** The lever moves the
/// lane hop and the release's demand-coupling, never the hold — so the
/// exhaustion rows read the same stalls and throughput under both settings
/// (the note's §5 rows), and every law holds.
#[test]
fn the_4gib_equivalent_lane_budget_rows_under_both_settings() {
    let _serial = serial();
    for (label, spare) in [
        ("4gib(8cw,50/s,spare 700,continuous)", 700u64),
        ("edge(8cw,50/s,spare 450,continuous)", 450u64),
    ] {
        let shape = LaneShape {
            label,
            members: 8,
            spare,
            demand_per_s: 50,
            duration_ms: 120_000,
            checkpoint_ms: 1_000,
            write_ms: 0,
            pause_ms: 0,
        };
        let off = run_lane_loop(&shape, false);
        let on = run_lane_loop(&shape, true);
        assert_safety(&off);
        assert_safety(&on);
        assert!(
            on.stalls <= off.stalls,
            "{label}: the lever never adds a stall ({} → {})",
            off.stalls,
            on.stalls
        );
        assert!(
            on.steady_allocs_per_s >= off.steady_allocs_per_s * 0.95,
            "{label}: the lever never slows the stream ({:.2} → {:.2} blk/s)",
            off.steady_allocs_per_s,
            on.steady_allocs_per_s
        );
        assert!(
            on.vis_released_served_ms <= off.vis_released_served_ms + 5.0,
            "{label}: the lane hop never grows ({:.0} → {:.0} ms)",
            off.vis_released_served_ms,
            on.vis_released_served_ms
        );
    }
}

// ---------------------------------------------------------------------------
// The stats inode faces
// ---------------------------------------------------------------------------

/// The family's export shape: the three stage keys, the unplaced count,
/// the outstanding marks, and the co-writer's wake/hint words; and the
/// pushed-harvest counter exists on `METRICS`.
#[test]
fn the_lane_visible_family_exports_its_faces() {
    let _serial = serial();
    let fam = lane_visible();
    for name in free_grace::LANE_VISIBLE_PHASE_NAMES {
        assert!(
            fam["alloc_lane_visible_phase_ns"][name]["count"].is_u64(),
            "stage {name} exported"
        );
    }
    assert!(fam["alloc_lane_visible_unplaced"].is_u64());
    assert!(fam["alloc_lane_release_marks"].is_u64());
    assert!(fam["free_grace_lane_push_wakes"].is_u64());
    assert!(fam["free_grace_lane_supply_hint"].is_u64());
    let _ = METRICS.alloc_lane_pushed_harvests.load(Ordering::Relaxed);
}
