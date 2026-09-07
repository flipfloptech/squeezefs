//! **Membership renewal isolation** — finding 15 phase B1
//! (`.benchmarks/2026-09-07-f15-day2-fleet-pair.md` §2; the record is
//! `.benchmarks/2026-09-07-membership-renewal-isolation.md`).
//!
//! The fleet shape: authority + 8 co-writers, file-per-proc phase. Every
//! co-writer's own `free_grace_acked_lag_ms` read ≤ 2.3 s while the
//! authority's `free_grace_member_ack_lag_ms.max` climbed 1 s/s to 10–11.6 s
//! and its served `membership_renewals` collapsed from 22/s to 0/s for
//! whole seconds — the grace ring's bound stalled, 3,200 offsets piled up,
//! every lane starved. The question the parent asked: WHERE did the
//! renewal's 10 s go — (a) the authority's serve, (b) the member's send
//! venue, or (c) the wire?
//!
//! The samples answer (d): none of them. The 10 s is the ROUTINE BEAT.
//! At the instant the ring DRAINED (`free_grace_offsets` 24 → 0 → 0 at
//! t = 99–101 s), every member's acknowledgement covered everything held,
//! so the owner's renewal path answered `take_prod_cadence(acked) = None`
//! — "a member that has acknowledged past the label the writer is waiting
//! on is not asked" — and handed each of them the ROUTINE 10 s cadence.
//! The ring refilled within 2 s (23 → 3,210 offsets), the pressure reading
//! re-armed the ask, but the ask is only ever DELIVERED on a renewal grant,
//! and the members' next beats were 10 s away. The member's LABEL source is
//! the routine beat too (a carriage renewal learns none), so the refill's
//! labels could not even be learned before then: `acked_lag_ms` sat FROZEN
//! at its last promote value (the "≤ 2.3 s" read), `membership_renewals`
//! served 0/s for 7 s, and `member_ack_lag.max` walked to the beat. No
//! member logged a renewal failure, timeout or fence anywhere in the run.
//!
//! Contracts here:
//!
//! 1. **The drain-instant grant never carries the routine hole** (RED
//!    pre-fix): while the ask is LIVE, a caught-up member is relaxed ONE
//!    doubling step (finding 18's unit), never snapped to routine — so
//!    the refill is learned within `2 × cadence`, not one beat later.
//! 2. **The renewal's wire hop never queues behind bulk blocking work**
//!    (RED pre-fix): `RpcClient::call` ran the socket round trip on the
//!    SHARED `sqz-blk` pool, FIFO behind every parked RPC / reclaim lane /
//!    crypto job of the co-writer — the isolation law finding 2 gave the
//!    POLL venue (`sqz-lease`) stopped at the wire. The renewal now rides
//!    its own `sqz-lease-io` thread (`SQUEEZEFS_MEMBERSHIP_RENEW_LANE`).
//! 3. **Serve and RTT stay bounded under an authority storm** (GREEN —
//!    candidate (a)/(c) ruled out in-process): hundreds of RPCs/s on the
//!    plane's listener plus parked meta lanes move neither the serve nor
//!    the member's RTT past a few ms.
//! 4. **The instruments** that decide (a)/(b)/(c)/(d) on the fleet without
//!    guessing export in the standard shape: `membership_renew_phase_ns`
//!    {carry_wait, rtt, total} (member), `membership_renew_serve_ns`
//!    (authority), `membership_renew_cadence_ms` (the beat in force on the
//!    member — 10,000 on every co-writer during the hole while the
//!    authority's `free_grace_prod_renew_ms` read 500).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cluster_wire::{RpcClient, RPC_OK};
use squeezefs::free_grace;
use squeezefs::membership::{
    self, Grant, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    RenewOutcome,
};
use squeezefs::membership_wire::{
    self, encode_census_request, MemberClient, MembershipPlane, MembershipPlaneConfig,
    VERB_MEMBERSHIP_CENSUS,
};
use squeezefs_ipc::sqz_blocking;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Fixtures (the `reader_free_grace_tests` shapes — process-global state)
// ---------------------------------------------------------------------------

static SERIAL: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL.swap(true, Ordering::AcqRel) {
        std::thread::sleep(Duration::from_millis(2));
    }
    free_grace::reset_for_test();
    membership::uninstall();
    membership_wire::reset_renew_instruments_for_test();
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        free_grace::reset_for_test();
        membership::uninstall();
        membership_wire::reset_renew_instruments_for_test();
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
    let owner = MembershipOwner::arm("isolation-owner", 3, 2, shipped_clocks(), clock.clone())
        .expect("arming a successor term must be admitted");
    membership::install_owner(Arc::clone(&owner));
    owner
}

fn join_req(id: &str, role: MemberRole) -> JoinRequest {
    JoinRequest {
        id: id.to_string(),
        role,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-isolation-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }
}

fn join(owner: &MembershipOwner, id: &str, role: MemberRole) -> Grant {
    match owner.join(join_req(id, role)) {
        JoinOutcome::Granted(g) => g,
        JoinOutcome::Refused { reason, .. } => panic!("join refused: {reason}"),
        JoinOutcome::UnknownLease { reason } => panic!("join answered UnknownLease: {reason}"),
    }
}

fn renew(owner: &MembershipOwner, id: &str, epoch: u64, acked: u64) -> Grant {
    match owner.renew(id, epoch, acked) {
        RenewOutcome::Renewed(g) => g,
        RenewOutcome::UnknownLease { reason } => panic!("renewal refused: {reason}"),
    }
}

async fn allocator(id: &str) -> Arc<BlockAllocator> {
    Arc::new(BlockAllocator::new(id).await.expect("allocator"))
}

fn hist_words(h: &serde_json::Value) -> (u64, u64) {
    (
        h["count"].as_u64().expect("count"),
        h["sum_ns"].as_u64().expect("sum_ns"),
    )
}

/// The simulated member of `tests/reader_free_grace_tests.rs` (its
/// `Storm`), trimmed to what these contracts drive: it beats on the cadence
/// its last grant carried, learns the label every ROUTINE grant carries,
/// and acknowledges a learned label one full ladder cycle later — all
/// PRODUCT decisions run on the owner; only the ladder's fixed cost is
/// modelled.
struct Member {
    owner: Arc<MembershipOwner>,
    clock: LeaseClock,
    ticks: Arc<AtomicU64>,
    id: &'static str,
    epoch: u64,
    learned: std::collections::VecDeque<(u64, u64)>,
    ladder_lag_ms: u64,
    beat_at: u64,
    cadence: u64,
    sweep_at: u64,
    sweep_ms: u64,
}

impl Member {
    fn new(
        owner: &Arc<MembershipOwner>,
        clock: &LeaseClock,
        ticks: &Arc<AtomicU64>,
        id: &'static str,
        grant: &Grant,
    ) -> Self {
        let clocks = owner.clocks();
        let staleness = squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64;
        Self {
            owner: Arc::clone(owner),
            clock: clock.clone(),
            ticks: Arc::clone(ticks),
            id,
            epoch: grant.epoch,
            learned: std::collections::VecDeque::new(),
            ladder_lag_ms: staleness * 2
                + clocks.skew_max.as_millis() as u64
                + clocks.d_purge.as_millis() as u64,
            beat_at: clock.now_ms(),
            cadence: grant.renew_ms,
            sweep_at: clock.now_ms() + clocks.renew_interval.as_millis() as u64,
            sweep_ms: clocks.renew_interval.as_millis() as u64,
        }
    }

    fn carriable(&mut self, now: u64) -> u64 {
        let mut best = 0;
        while let Some(&(at, label)) = self.learned.front() {
            if at + self.ladder_lag_ms > now {
                break;
            }
            best = label;
            self.learned.pop_front();
        }
        if best != 0 {
            self.learned.push_front((0, best));
        }
        best
    }

    fn advance(&mut self, step_ms: u64) {
        self.ticks.fetch_add(step_ms, Ordering::SeqCst);
        let now = self.clock.now_ms();
        while self.clock.now_ms() >= self.beat_at {
            let carried = self.carriable(self.clock.now_ms());
            let grant = renew(&self.owner, self.id, self.epoch, carried);
            let at = self.clock.now_ms();
            self.learned.push_back((at, grant.granted_at_owner_ms));
            self.cadence = grant.renew_ms.max(1);
            self.beat_at = at + self.cadence;
        }
        if now >= self.sweep_at {
            self.owner.refresh_free_grace_bound();
            self.sweep_at = now + self.sweep_ms;
        }
    }
}

/// Storm the ring until rung (a)'s ask is in force; returns the tightened
/// cadence and the live blocks.
async fn storm_until_asked(
    ba: &BlockAllocator,
    member: &mut Member,
) -> (u64, std::collections::VecDeque<u64>) {
    let mut live: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    let mut tightened = 0u64;
    for pass in 0..120 {
        let fresh = ba
            .allocate_block()
            .await
            .unwrap_or_else(|e| panic!("pass {pass}: the storm stalled ({e})"));
        live.push_back(fresh);
        if live.len() > 4 {
            let victim = live.pop_front().expect("a live block");
            ba.free_block(victim).await.expect("free");
        }
        member.advance(1_000);
        tightened = free_grace::prod_renew_ms();
        if tightened > 0 {
            break;
        }
    }
    assert!(tightened > 0, "the storm must arm rung (a)");
    (tightened, live)
}

/// Poll the allocation-head harvest (the production drain path) until the
/// ring reads `held` offsets or fewer — the tail frees ride the background
/// reclaim worker, whose cadence is real milliseconds.
async fn wait_ring_at_most(
    ba: &BlockAllocator,
    live: &mut std::collections::VecDeque<u64>,
    held: u64,
) {
    for _ in 0..400 {
        if free_grace::held_offsets() <= held {
            return;
        }
        if let Ok(fresh) = ba.allocate_block().await {
            live.push_back(fresh);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the ring did not reach {held} held offset(s) (still {})",
        free_grace::held_offsets()
    );
}

// ---------------------------------------------------------------------------
// 1. The drain-instant grant never carries the routine hole
// ---------------------------------------------------------------------------

/// **The fleet's 10 s, reproduced at the delivery point.** Storm until the
/// ask is in force, drain the ring by acknowledging everything held, and
/// probe the owner's renewal path for the caught-up member while the ask
/// is still live. The shipped arm (`SQUEEZEFS_FREE_GRACE_CAUGHT_UP_RELAX=0`,
/// the control) answers the ROUTINE cadence — the very hole the samples
/// show, one beat wide; the fix answers one doubling step of the ask
/// (finding 18's unit), so a refill inside the reading window is learned
/// within `2 × cadence`. The economy law stands: a caught-up member beats
/// half as often as a laggard, and the ask still retires at routine or on
/// a drained ring once the reading lapses (the existing contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caught_up_member_under_a_live_ask_is_relaxed_one_step_never_handed_the_routine_hole() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-drain-instant", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let routine = grant.renew_ms;

    let ba = allocator("grace-drain-instant").await;
    ba.set_capacity_bytes(32 * ba.chunk_size());
    let mut member = Member::new(&owner, &clock, &ticks, "r-drain-instant", &grant);
    let (tightened, mut live) = storm_until_asked(&ba, &mut member).await;
    assert!(
        tightened * 2 < routine,
        "premise: two steps of the ask ({tightened} ms) must still sit below routine \
         ({routine} ms) or the hole cannot be discriminated"
    );

    // THE DRAIN: an honest acknowledgement of everything held — labels are
    // `now + 1` at defer time, so `now + 1` covers the whole ring and
    // nothing freed after the clock next moves.
    let everything = clock.now_ms() + 1;
    renew(&owner, "r-drain-instant", grant.epoch, everything);
    owner.refresh_free_grace_bound();
    wait_ring_at_most(&ba, &mut live, 0).await;
    assert_eq!(free_grace::held_offsets(), 0, "the ring must have drained");
    assert!(
        free_grace::prod_renew_ms() > 0,
        "premise: the ask must still be LIVE at the drain instant (the fleet's readings were \
         ≤ 1 s old when the ring hit 0)"
    );

    // THE CONTROL — the shipped arm: the caught-up member draws the
    // routine cadence. This IS the fleet's hole: `membership_renewals`
    // 0/s for 7 s while `free_grace_prod_renew_ms` read 500.
    free_grace::test_set_caught_up_relax(Some(false));
    let caught_up_before = free_grace::prods_caught_up();
    let control = renew(&owner, "r-drain-instant", grant.epoch, everything);
    assert_eq!(
        control.renew_ms, routine,
        "the control arm must reproduce the shipped snap-to-routine"
    );
    assert_eq!(
        free_grace::prods_caught_up(),
        caught_up_before,
        "the control arm engages no relaxed ask"
    );

    // THE CONTRACT (pre-fix RED): with the lever on, the same probe is
    // relaxed one doubling step — the ask, not the hole.
    free_grace::test_set_caught_up_relax(Some(true));
    let relaxed = renew(&owner, "r-drain-instant", grant.epoch, everything);
    eprintln!(
        "drain instant: ask in force {tightened} ms, routine {routine} ms — control grant \
         {} ms (the hole), relaxed grant {} ms",
        control.renew_ms, relaxed.renew_ms
    );
    assert!(
        relaxed.renew_ms < routine && relaxed.renew_ms <= tightened * 2,
        "a caught-up member under a live ask must be relaxed ONE step (≤ {} ms), never handed \
         the routine hole — got {} ms (routine {routine})",
        tightened * 2,
        relaxed.renew_ms
    );
    assert!(
        relaxed.renew_ms >= tightened,
        "relaxed, not tightened: a caught-up member is never asked harder than a laggard \
         ({} vs {tightened})",
        relaxed.renew_ms
    );
    assert_eq!(
        free_grace::prods_caught_up(),
        caught_up_before + 1,
        "the relaxed ask has its own ledger (it is not a laggard prod)"
    );

    // THE REFILL inside the reading window: the writer frees again, the
    // ring holds labels past the member's acknowledgement, and at the
    // member's NEXT beat — `relaxed.renew_ms` away, not `routine` — the
    // ask is delivered tightened again.
    ticks.fetch_add(relaxed.renew_ms, Ordering::SeqCst);
    for _ in 0..6 {
        let fresh = ba.allocate_block().await.expect("refill allocation");
        live.push_back(fresh);
        let victim = live.pop_front().expect("a live block");
        ba.free_block(victim).await.expect("refill free");
        ticks.fetch_add(1, Ordering::SeqCst);
    }
    for _ in 0..400 {
        if free_grace::held_offsets() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        free_grace::held_offsets() >= 2,
        "the refill must be held (the member has not acknowledged past it)"
    );
    let asked_again = renew(&owner, "r-drain-instant", grant.epoch, everything);
    assert!(
        asked_again.renew_ms < routine,
        "after the refill the member is behind again and is asked ({} ms vs routine {routine})",
        asked_again.renew_ms
    );
    // The whole hole, in the member's clock: the beat it slept after the
    // drain-instant grant. Pre-fix this was `routine`.
    assert!(
        relaxed.renew_ms <= tightened * 2,
        "the label-learning hole after a drain is bounded by two steps of the ask"
    );
    assert_eq!(free_grace::forced_releases(), 0);
    assert_eq!(free_grace::laggard_fences(), 0);
}

/// **The existing economy half is untouched**: once the reading LAPSES on
/// a drained ring the ask retires at once and a caught-up member draws
/// routine — the relaxed step exists only while the ask is live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relaxed_step_exists_only_while_the_ask_is_live() {
    let _serial = serial();
    let (clock, ticks) = manual_clock();
    let owner = armed_owner(&clock);
    free_grace::arm_owner_plane(clock.clone(), owner.clocks()).expect("derived bound");
    let grant = join(&owner, "r-lapse", MemberRole::Reader);
    owner.refresh_free_grace_bound();
    let routine = grant.renew_ms;

    let ba = allocator("grace-drain-lapse").await;
    ba.set_capacity_bytes(32 * ba.chunk_size());
    let mut member = Member::new(&owner, &clock, &ticks, "r-lapse", &grant);
    let (_tightened, mut live) = storm_until_asked(&ba, &mut member).await;
    let everything = clock.now_ms() + 1;
    renew(&owner, "r-lapse", grant.epoch, everything);
    owner.refresh_free_grace_bound();
    wait_ring_at_most(&ba, &mut live, 0).await;

    // Let the reading lapse: past one reading TTL with the ring empty the
    // ask retires (the finding-18 economy half), so the caught-up probe
    // reads routine and the caught-up ledger does not move.
    let caught_up_before = free_grace::prods_caught_up();
    let mut retired = None;
    for _ in 0..240 {
        ticks.fetch_add(1_000, Ordering::SeqCst);
        let g = renew(&owner, "r-lapse", grant.epoch, everything);
        if g.renew_ms == routine {
            retired = Some(g.renew_ms);
            break;
        }
    }
    assert_eq!(
        retired,
        Some(routine),
        "a drained ring's ask must retire once the reading lapses"
    );
    assert!(
        free_grace::prods_caught_up() > caught_up_before,
        "the relaxed step engaged while the ask was live"
    );
    assert_eq!(
        free_grace::prod_renew_ms(),
        0,
        "no ask in force on a lapsed, drained plane"
    );
}

// ---------------------------------------------------------------------------
// 2. The renewal's wire hop never queues behind bulk blocking work
// ---------------------------------------------------------------------------

fn loopback_plane(owner: &Arc<MembershipOwner>, secret: &[u8]) -> Arc<MembershipPlane> {
    MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        secret.to_vec(),
        Arc::clone(owner),
    )
    .expect("the plane must bind a loopback listener")
}

/// Park the shared blocking pool solid: `cap + slack` jobs each sleeping
/// `hold` — the co-writer's bulk population (parked RPC round trips,
/// reclaim lanes, crypto) at its worst.
fn saturate_blocking_pool(hold: Duration) -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let jobs = sqz_blocking::pool_cap_from(cpus) + 16;
    for _ in 0..jobs {
        // A dead awaiter is fine by the pool's own law — the job runs.
        drop(sqz_blocking::run_blocking(move || std::thread::sleep(hold)));
    }
    jobs
}

/// **The isolation law's wire half** (finding 2 isolated the POLL venue;
/// the SEND still rode the shared `sqz-blk` FIFO). With the pool parked
/// solid, the control arm (`SQUEEZEFS_MEMBERSHIP_RENEW_LANE=0`) sends its
/// renewal only when a pool thread frees — the parked hold shows up as
/// `carry_wait` — while the fix sends it at once on the dedicated
/// `sqz-lease-io` thread. Pre-fix RED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_renewals_wire_hop_never_queues_behind_bulk_blocking_work() {
    let _serial = serial();
    let secret = b"isolation-storage-trust".to_vec();
    let owner = armed_owner(&LeaseClock::monotonic());
    let plane = loopback_plane(&owner, &secret);
    let endpoint = plane.endpoint().to_string();
    let mut client = MemberClient::join(
        &endpoint,
        &secret,
        join_req("wire-member", MemberRole::Reader),
        LeaseClock::monotonic(),
    )
    .await
    .expect("join over the wire");

    let hold = Duration::from_millis(600);

    // THE CONTROL: the shared pool. The renewal's send waits for a parked
    // job to finish — the hole is the bulk work's own duration.
    membership_wire::test_set_renew_lane(Some(false));
    let jobs = saturate_blocking_pool(hold);
    let t0 = Instant::now();
    client.renew().await.expect("renew on the shared pool");
    let control_wall = t0.elapsed();
    let (n, control_carry_ns) = hist_words(&membership_wire::renew_phase_json()["carry_wait"]);
    assert_eq!(n, 1, "one renewal recorded one carry_wait span");
    assert!(
        control_wall >= hold / 2 && control_carry_ns >= (hold / 2).as_nanos() as u64,
        "premise: {jobs} parked jobs must have queued the control renewal behind them \
         (wall {control_wall:?}, carry_wait {} ms) — otherwise the venue is not the one \
         under test",
        control_carry_ns / 1_000_000
    );
    assert_eq!(membership_wire::renew_lane_calls(), 0);
    // Let the control's parked jobs drain before the contract arm so its
    // saturation is fresh.
    tokio::time::sleep(hold + Duration::from_millis(100)).await;

    // THE CONTRACT (pre-fix RED): the dedicated lane sends at once.
    membership_wire::test_set_renew_lane(Some(true));
    saturate_blocking_pool(hold);
    let t0 = Instant::now();
    client.renew().await.expect("renew on the lease-io lane");
    let wall = t0.elapsed();
    let (n, carry_ns) = hist_words(&membership_wire::renew_phase_json()["carry_wait"]);
    assert_eq!(n, 2);
    // The table exports cumulative words: this probe's span is the delta
    // over the control's.
    let this_carry_ns = carry_ns.saturating_sub(control_carry_ns);
    eprintln!(
        "wire hop: {jobs} parked pool jobs ({hold:?} each) — shared pool: wall {control_wall:?} \
         carry_wait {} ms; sqz-lease-io: wall {wall:?} carry_wait {} µs",
        control_carry_ns / 1_000_000,
        this_carry_ns / 1_000
    );
    assert!(
        wall < hold / 4,
        "the renewal must not queue behind the parked pool: wall {wall:?} against a \
         {hold:?} hold"
    );
    assert!(
        this_carry_ns < (hold / 4).as_nanos() as u64,
        "carry_wait must be the lane's own scheduling only, not the pool's queue ({} ms)",
        this_carry_ns / 1_000_000
    );
    assert_eq!(
        membership_wire::renew_lane_calls(),
        1,
        "the dedicated lane's engagement gauge counts the round trip"
    );
    tokio::time::sleep(hold + Duration::from_millis(100)).await;
    client.leave().await.expect("leave");
    plane.shutdown();
}

// ---------------------------------------------------------------------------
// 3. Serve and RTT stay bounded under an authority storm (green — (a)/(c))
// ---------------------------------------------------------------------------

/// **Candidates (a) and (c), ruled out in-process.** The authority's
/// listener serves hundreds of census/renew RPCs per second from storm
/// clients while its `sqz-meta` lanes sit parked; the member's renewals run
/// meanwhile. The serve is one `scc` probe plus atomics on the connection's
/// own thread and the RTT is loopback framing — both stay in the
/// milliseconds whatever the storm does, which is what the fleet's
/// instruments must show for the beat-hole diagnosis to hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewal_serve_and_rtt_stay_bounded_under_an_authority_storm() {
    let _serial = serial();
    let secret = b"isolation-storm-secret".to_vec();
    let owner = armed_owner(&LeaseClock::monotonic());
    let plane = loopback_plane(&owner, &secret);
    let endpoint = plane.endpoint().to_string();
    let mut member = MemberClient::join(
        &endpoint,
        &secret,
        join_req("storm-member", MemberRole::Reader),
        LeaseClock::monotonic(),
    )
    .await
    .expect("join over the wire");

    // The storm: 8 clients hammering the plane's listener, plus the
    // authority's meta lanes parked (the harvest/free-class population).
    let stop = Arc::new(AtomicBool::new(false));
    let storm_calls = Arc::new(AtomicU64::new(0));
    let mut storm = Vec::new();
    for i in 0..8u32 {
        let endpoint = endpoint.clone();
        let secret = secret.clone();
        let stop = Arc::clone(&stop);
        let calls = Arc::clone(&storm_calls);
        storm.push(std::thread::spawn(move || {
            sqz_blocking::block_on(async move {
                let id = format!("storm-{i}");
                let mut rpc = RpcClient::connect(&endpoint, &secret, &id, None)
                    .await
                    .expect("storm client dials");
                while !stop.load(Ordering::Acquire) {
                    let reply = rpc
                        .call(
                            VERB_MEMBERSHIP_CENSUS,
                            encode_census_request(0, 64).expect("encode"),
                        )
                        .await
                        .expect("storm census");
                    assert_eq!(reply.status, RPC_OK);
                    calls.fetch_add(1, Ordering::Relaxed);
                }
            });
        }));
    }
    for _ in 0..8 {
        squeezefs::meta_exec::spawn_meta("isolation-storm-park", async {
            std::thread::sleep(Duration::from_millis(1_500));
        });
    }

    let mut rtt_max = Duration::ZERO;
    let started = Instant::now();
    let mut renewals = 0u64;
    while started.elapsed() < Duration::from_millis(1_200) {
        let t0 = Instant::now();
        member.renew().await.expect("renew under the storm");
        rtt_max = rtt_max.max(t0.elapsed());
        renewals += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.store(true, Ordering::Release);
    for h in storm {
        h.join().expect("storm thread");
    }

    let calls = storm_calls.load(Ordering::Relaxed);
    assert!(
        calls >= 200,
        "premise: the storm must have served hundreds of RPCs ({calls})"
    );
    let (n, serve_sum) = hist_words(&membership_wire::renew_serve_json());
    assert!(
        n >= renewals,
        "every renewal is served exactly once ({n} vs {renewals})"
    );
    let serve_mean = Duration::from_nanos(serve_sum / n.max(1));
    let (rn, rtt_sum) = hist_words(&membership_wire::renew_phase_json()["rtt"]);
    assert_eq!(rn, renewals);
    let rtt_mean = Duration::from_nanos(rtt_sum / rn.max(1));
    eprintln!(
        "storm: {calls} storm RPCs, {renewals} renewals — serve mean {serve_mean:?}, \
         rtt mean {rtt_mean:?} max {rtt_max:?}"
    );
    assert!(
        serve_mean < Duration::from_millis(2),
        "the authority's renewal serve must stay RAM-only under the storm (mean {serve_mean:?})"
    );
    assert!(
        rtt_mean < Duration::from_millis(20) && rtt_max < Duration::from_millis(250),
        "the member's renewal RTT must stay bounded under the storm (mean {rtt_mean:?}, \
         max {rtt_max:?})"
    );
    member.leave().await.expect("leave");
    plane.shutdown();
}

// ---------------------------------------------------------------------------
// 4. The instruments export in the standard shape
// ---------------------------------------------------------------------------

/// `membership_renew_phase_ns` is three standard histograms (carry_wait /
/// rtt / total), `membership_renew_serve_ns` one; the member's posture block
/// carries `membership_renew_cadence_ms` — the beat in force, which is the
/// word that names the hole (10,000 on the members while the authority
/// asked for 500).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_renewal_instruments_export_the_standard_shape() {
    let _serial = serial();
    let phases = membership_wire::renew_phase_json();
    for phase in ["carry_wait", "rtt", "total"] {
        let h = &phases[phase];
        assert!(h["buckets"].is_object(), "{phase} carries the bucket map");
        assert_eq!(hist_words(h), (0, 0), "{phase} starts empty");
    }
    assert_eq!(hist_words(&membership_wire::renew_serve_json()), (0, 0));

    let secret = b"isolation-shape-secret".to_vec();
    let owner = armed_owner(&LeaseClock::monotonic());
    let plane = loopback_plane(&owner, &secret);
    let endpoint = plane.endpoint().to_string();
    let mut client = MemberClient::join(
        &endpoint,
        &secret,
        join_req("shape-member", MemberRole::Reader),
        LeaseClock::monotonic(),
    )
    .await
    .expect("join over the wire");
    membership::install_member(Arc::clone(client.session()));
    client.renew().await.expect("renew");

    let phases = membership_wire::renew_phase_json();
    let (carry, carry_ns) = hist_words(&phases["carry_wait"]);
    let (rtt, rtt_ns) = hist_words(&phases["rtt"]);
    let (total, total_ns) = hist_words(&phases["total"]);
    assert_eq!((carry, rtt, total), (1, 1, 1));
    assert!(
        total_ns >= carry_ns + rtt_ns,
        "containment: total ≥ carry_wait + rtt ({total_ns} vs {carry_ns} + {rtt_ns})"
    );
    let (served, _) = hist_words(&membership_wire::renew_serve_json());
    assert_eq!(
        served, 1,
        "the authority recorded the one renewal it served"
    );

    let snap = membership::stats_snapshot();
    assert_eq!(snap["membership_mode"], "member");
    assert_eq!(
        snap["membership_renew_cadence_ms"].as_u64(),
        Some(client.session().renew_interval_ms()),
        "the member publishes the beat in force"
    );
    client.leave().await.expect("leave");
    plane.shutdown();
}
