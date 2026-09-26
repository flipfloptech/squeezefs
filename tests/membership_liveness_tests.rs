//! **Lease-heartbeat liveness isolation** — the fix for finding 2 of the
//! 2026-08-19 real-fabric mw fleet run
//! (`.benchmarks/2026-08-20-fabric-confirm-sessions.md` §3; field log
//! `.benchmarks/cloud/2026-08-19-235559/mw-rows/logs/sqz-mw-cw2.log`).
//!
//! ## The field conviction
//!
//! A co-writer under a write storm missed its 45 s membership TTL with
//! **zero** per-attempt renewal-failure warnings — the renewal tick never
//! ran for > 45 s (silence, then `renew refused: not in owner's census`,
//! then the §6.7 self-fence, which worked exactly as designed). The
//! per-attempt warnings live INSIDE `member_renewal_tick`, so their
//! absence convicts the venue, not the wire.
//!
//! ## The convicted mechanism
//!
//! `spawn_member_renewal` (src/membership.rs) ran the member's whole
//! liveness heartbeat as ONE cooperative task on the process-global
//! 2-thread `sqz-meta` `LaneExec` pool (`meta_lanes_from`,
//! src/meta_exec.rs), shared with every meta-plane task class.
//! `LaneExec::run` (crates/squeezefs-ipc/src/sqz_exec.rs) is FIFO
//! dispatch with no priority class and no preemption: the `sqz-timer`
//! thread fires the renewal's waker on time, but the wake only ENQUEUES
//! the task — delivery waits until a lane thread returns from whatever
//! poll it is inside. A meta-plane poll that occupies its OS thread (a
//! synchronous wait whose holder is parked behind a stalled authority —
//! the finding-1 coupling class; ANY bounded blocking section a remote
//! stall makes unbounded) holds the lane for the stall's whole duration,
//! and two such polls silence the heartbeat past `T_self` with zero
//! warnings. The custody renewal loop (`spawn_custody_renewal`,
//! src/cowriter.rs) had the identical venue exposure, PLUS a second
//! fate-sharing of its own: `WriteCustodyClient::renew_all` serializes
//! behind the ONE wire session (`session.lock().await`,
//! src/data_grant.rs) that the write path's custody-acquire storm holds
//! for a full authority-side park per attempt.
//!
//! ## The design principle these contracts pin
//!
//! **A lease renewal is a heartbeat — it must be isolated from the
//! workload whose stall it is supposed to survive.** Venue: the
//! dedicated `sqz-lease` thread (`meta_exec::spawn_lease`). Wire: the
//! custody heartbeat rides its own session, never the acquire storm's.
//! Time: every renewal attempt is deadline-bounded (~remaining-to-T_self
//! / 3), so a hung attempt logs the ladder warning and retries instead
//! of occupying the lease venue past its cadence. The §6.7 self-fence
//! semantics are byte-identical — the fix makes the fence UNNECESSARY
//! under load, never weaker.
//!
//! RED against `dev` @ `d3672fbb`: both renewal loops ride `sqz-meta`
//! and the custody heartbeat shares the acquire session, so occupying
//! the meta lanes (or the session) past `T_self` gets the member swept.

use squeezefs::cluster_wire as cw;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::dlm::LockMode;
use squeezefs::fuse_client::METRICS;
use squeezefs::membership::{self, LeaseClock, LeaseClocks, MembershipOwner, OwnerRecord};
use squeezefs::membership_wire::{MembershipPlane, MembershipPlaneConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SECRET: &[u8] = b"membership-liveness-storage-trust-secret";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Process-global state (the S7 poison latch, the installed-member
/// registry, the METRICS membership family, the sqz-meta lanes this suite
/// deliberately occupies) forces serialization: libtest runs a file's
/// tests on threads, and the gate's `--test-threads=1` bounds files, not
/// tests within one.
static SERIAL_HELD: AtomicBool = AtomicBool::new(false);

struct Serial;

fn serial() -> Serial {
    while SERIAL_HELD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        std::thread::yield_now();
    }
    Serial
}

impl Drop for Serial {
    fn drop(&mut self) {
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

/// Restores every process-global posture a fenced test could leave
/// behind, so a panicking (red) run can never poison the binary's later
/// tests — the `tests/dlm_multi_writer_tests.rs` pattern.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        squeezefs::data_custody::test_clear_poison();
        squeezefs::data_custody::test_reset_custody_generation();
    }
}

fn restore() -> Restore {
    Restore
}

/// Short lease clocks so the liveness window is test-sized: `T_owner`
/// 1.5 s, `T_self` 1.3 s, renewal cadence ≈ 433 ms (the same §6.7 law as
/// the shipped 45 s shape — only the scale differs).
fn short_clocks() -> LeaseClocks {
    LeaseClocks::with_params(
        Duration::from_millis(1_500),
        Duration::from_millis(50),
        Duration::from_millis(100),
    )
    .expect("2·skew + purge < TTL, so T_self is positive")
}

/// **The adversarial-but-legal meta-lane load**: tasks whose poll blocks
/// the lane OS thread on a synchronous wait until released — the exact
/// shape of a meta-plane poll holding a bounded blocking section that a
/// stalled authority makes unbounded (the finding-1 coupling class).
///
/// Deliberately NOT a `Pending`-parked future (a parked future *yields*
/// its lane — that shape cannot starve `LaneExec`) and NOT a spin: the
/// convicted mechanism is lane-thread OCCUPANCY, and a blocking wait is
/// its honest minimal form.
struct LaneHold {
    /// Dropping the senders errors every occupier's `recv()` — release.
    _release: Vec<std::sync::mpsc::Sender<()>>,
}

fn occupy_meta_lanes() -> LaneHold {
    let lanes = squeezefs::meta_exec::meta_lanes_from(squeezefs::cpu::process_parallelism());
    // Two extras queue behind the blockers so the lanes stay covered even
    // if a lane pops a non-blocking stray task first.
    let occupiers = lanes + 2;
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let mut release = Vec::new();
    for _ in 0..occupiers {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        release.push(tx);
        let started = started_tx.clone();
        squeezefs::meta_exec::spawn_meta("liveness_test_lane_occupier", async move {
            let _ = started.send(());
            // The blocking wait INSIDE the poll — this is what holds the
            // lane OS thread (released when the test drops the sender).
            let _ = rx.recv();
        });
    }
    // Only `lanes` occupiers can be running at once; wait until every lane
    // thread is provably inside a blocking poll.
    for _ in 0..lanes {
        started_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("an occupier must start on every sqz-meta lane");
    }
    LaneHold { _release: release }
}

// ---------------------------------------------------------------------------
// 1. The membership heartbeat survives meta-lane occupancy past T_self
// ---------------------------------------------------------------------------

/// **The finding-2 contract.** A member whose host is otherwise healthy
/// — the wire up, the owner answering, only the `sqz-meta` lanes wedged
/// by workload-class polls — must keep renewing on its cadence: it is
/// never swept by the owner, it never self-fences, and the renewal
/// tick's own scheduling lag stays far below `T_self`.
///
/// RED today: the renewal loop rides the occupied `sqz-meta` pool, so
/// the owner sweeps the member at `T_owner` with ZERO per-attempt
/// warnings — the exact field shape (cw2, 2026-08-20 03:57:48Z).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn membership_heartbeat_survives_meta_lane_occupancy_past_t_self() {
    let _serial = serial();
    let _restore = restore();
    let clocks = short_clocks();
    let t_owner_ms = clocks.t_owner.as_millis() as u64;
    let owner = MembershipOwner::arm(
        "owner-liveness",
        7,
        6,
        clocks.clone(),
        LeaseClock::monotonic(),
    )
    .expect("the owner arms");
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        SECRET.to_vec(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");

    let rec = OwnerRecord {
        v: 1,
        id: "owner-liveness".to_string(),
        term: 7,
        endpoint: plane.endpoint().to_string(),
        ttl_ms: t_owner_ms,
        owner_claim_id: String::new(),
        ts: 0,
        pid: std::process::id(),
        boot: "boot-liveness".to_string(),
    };
    let renewals_before = METRICS.membership_renewals.load(Ordering::Relaxed);
    let fences_before = METRICS.membership_self_fences.load(Ordering::Relaxed);

    let arm = membership::join_as_writer_member(&rec, SECRET.to_vec(), "liveness-node", 0, None)
        .await
        .expect("the join must be admitted")
        .expect("a rendezvous record exists, so a member arms");

    // Prove the loop is ALIVE before wedging the lanes: at least one
    // renewal lands on its own cadence.
    let alive_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while METRICS.membership_renewals.load(Ordering::Relaxed) == renewals_before {
        assert!(
            std::time::Instant::now() < alive_deadline,
            "the renewal loop must land its first heartbeat on a healthy host"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Wedge every sqz-meta lane for > 2 × T_owner, driving the owner's
    // TTL sweep from the TEST thread the whole time (the owner side is
    // healthy by construction — its listener threads are not meta lanes).
    let hold = occupy_meta_lanes();
    let renewals_at_hold = METRICS.membership_renewals.load(Ordering::Relaxed);
    let mut swept: Vec<String> = Vec::new();
    let window = Duration::from_millis(2 * t_owner_ms + 500);
    let t0 = std::time::Instant::now();
    while t0.elapsed() < window {
        for ev in owner.expire_due() {
            swept.push(ev.id.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(hold);

    assert!(
        swept.is_empty(),
        "a member on an otherwise-healthy host must NEVER be swept because the \
         sqz-meta lanes are occupied — the heartbeat is isolated from the workload \
         whose stall it survives (swept: {swept:?})"
    );
    assert_eq!(
        METRICS.membership_self_fences.load(Ordering::Relaxed),
        fences_before,
        "no §6.7 self-fence: the fence machinery is correct, and the fix makes it \
         UNNECESSARY under load"
    );
    let landed = METRICS.membership_renewals.load(Ordering::Relaxed) - renewals_at_hold;
    assert!(
        landed >= 3,
        "renewals must keep landing on their cadence while the meta lanes are \
         wedged (got {landed} in {window:?} at a ~433 ms cadence)"
    );
    assert!(
        owner.epoch_of("liveness-node").is_some(),
        "the member's lease is still custody at the owner"
    );
    let lag = METRICS
        .membership_renew_sched_lag_ms
        .load(Ordering::Relaxed);
    assert!(
        lag < 1_300,
        "the renewal tick's own scheduling lag (max-gauge {lag} ms) must stay far \
         below T_self while the meta lanes are wedged — this is the instrument that \
         surfaces venue starvation BEFORE it becomes a fence"
    );

    arm.disarm().await;
    plane.shutdown();
}

// ---------------------------------------------------------------------------
// 2. The custody heartbeat rides its own wire session, not the acquire
//    storm's
// ---------------------------------------------------------------------------

/// **The custody twin's second fate-sharing** (same class, different
/// resource): `renew_all` used to serialize behind the ONE
/// `WriteCustodyClient` wire session that the write path's custody
/// acquires hold for a full authority-side park (min(wait, one renewal
/// cadence)) per attempt — the field's 35-attempt POSIX-5 ladders kept
/// that mutex saturated, so the heartbeat starved behind its own
/// workload. The heartbeat must ride a DEDICATED session: under a
/// continuous conflicting-acquire storm the client keeps renewing, is
/// never expired by the authority, and never self-fences.
///
/// RED today: renew_all queues FIFO behind ~12 parked acquires × one
/// cadence each — several seconds against a 1.3 s `T_self`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn custody_heartbeat_rides_its_own_wire_session_not_the_acquire_storms() {
    let _serial = serial();
    let _restore = restore();
    let clocks = short_clocks();
    let t_owner_ms = clocks.t_owner.as_millis() as u64;
    let term = squeezefs::dlm::durable_term();
    let owner = WriteCustodyOwner::arm(
        "authority-liveness",
        term + 1,
        term,
        clocks.clone(),
        LeaseClock::monotonic(),
        None,
    )
    .expect("the custody authority arms");
    let router = data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&owner));
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 4,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), Arc::new(router))
        .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();

    // The incumbent: holds exclusive custody of ino 42 forever, renewing
    // on its own (uncontended) session so it never expires.
    let holder = WriteCustodyClient::connect_with_clock(
        &endpoint,
        SECRET,
        "node-holder",
        LeaseClock::monotonic(),
        0,
    )
    .await
    .expect("the holder joins");
    let _held = holder
        .acquire(42, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("the holder takes whole-file custody of ino 42");

    let victim = WriteCustodyClient::connect_with_clock(
        &endpoint,
        SECRET,
        "node-victim",
        LeaseClock::monotonic(),
        0,
    )
    .await
    .expect("the victim joins");

    let stop = Arc::new(AtomicBool::new(false));

    // The acquire STORM (the field's POSIX-5 retry-ladder shape): every
    // attempt parks at the authority for one renewal cadence behind the
    // incumbent, holding the victim's workload session for the whole park.
    let mut storm = Vec::new();
    for _ in 0..12 {
        let victim = Arc::clone(&victim);
        let stop = Arc::clone(&stop);
        storm.push(tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let _ = victim
                    .acquire(42, None, LockMode::Exclusive, Duration::from_secs(10))
                    .await;
            }
        }));
    }

    // The heartbeats — the victim's is the one under test; the holder's
    // exists so the incumbency (and hence the storm's contention) lasts
    // the whole window.
    let renew_loops: Vec<_> = [Arc::clone(&victim), Arc::clone(&holder)]
        .into_iter()
        .map(|client| {
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                while !stop.load(Ordering::Acquire) {
                    squeezefs_ipc::sqz_time::sleep(Duration::from_millis(client.renewal_due_ms()))
                        .await;
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    let _ = client.renew_all().await;
                }
            })
        })
        .collect();

    let renewals_before = data_grant::stats().renewals;
    let mut dead: Vec<String> = Vec::new();
    let window = Duration::from_millis(3 * t_owner_ms);
    let t0 = std::time::Instant::now();
    while t0.elapsed() < window {
        for d in owner.expire_due() {
            dead.push(d.client.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.store(true, Ordering::Release);
    for t in storm {
        t.await.expect("storm task");
    }
    for t in renew_loops {
        t.await.expect("renew loop");
    }

    assert!(
        !dead.iter().any(|c| c == "node-victim"),
        "the custody heartbeat must never starve behind its own acquire storm — \
         the renewal rides a dedicated wire session (expired: {dead:?})"
    );
    assert!(
        !squeezefs::data_custody::poisoned(),
        "no self-fence: the victim's lease never died, so nothing poisoned custody"
    );
    let landed = data_grant::stats().renewals - renewals_before;
    assert!(
        landed >= 6,
        "renewals must keep their cadence under the storm (got {landed} across two \
         clients in {window:?} at a ~433 ms cadence)"
    );
}

// ---------------------------------------------------------------------------
// 3. The custody renewal LOOP survives meta-lane occupancy past T_self
// ---------------------------------------------------------------------------

/// **The custody twin of contract 1** — same venue conviction, the REAL
/// cadence loop (`data_grant::spawn_custody_renewal`): with every sqz-meta
/// lane wedged past `T_self` by workload-class blocking polls, the
/// co-writer's custody heartbeat must keep renewing on the dedicated
/// `sqz-lease` lane — the authority never expires it, and nothing
/// poisons process data custody.
///
/// (Red-by-construction against the pre-fix tree: the loop rode
/// `sqz-meta` and this suite's contract 1 reproduced the sweep on the
/// membership twin; the loop was not public before the fix, so this
/// contract lands with it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custody_renewal_loop_survives_meta_lane_occupancy_past_t_self() {
    let _serial = serial();
    let _restore = restore();
    let clocks = short_clocks();
    let t_owner_ms = clocks.t_owner.as_millis() as u64;
    let term = squeezefs::dlm::durable_term();
    let owner = WriteCustodyOwner::arm(
        "authority-venue",
        term + 1,
        term,
        clocks.clone(),
        LeaseClock::monotonic(),
        None,
    )
    .expect("the custody authority arms");
    let router = data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&owner));
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), Arc::new(router))
        .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();

    let client = WriteCustodyClient::connect_with_clock(
        &endpoint,
        SECRET,
        "node-venue",
        LeaseClock::monotonic(),
        0,
    )
    .await
    .expect("the co-writer joins");
    let renewals_before = data_grant::stats().renewals;
    let stop = Arc::new(AtomicBool::new(false));
    squeezefs::data_grant::spawn_custody_renewal(Arc::clone(&client), Arc::clone(&stop));

    // Prove the loop is alive before wedging the lanes.
    let alive_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while data_grant::stats().renewals == renewals_before {
        assert!(
            std::time::Instant::now() < alive_deadline,
            "the custody renewal loop must land its first heartbeat on a healthy host"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let hold = occupy_meta_lanes();
    let renewals_at_hold = data_grant::stats().renewals;
    let mut dead: Vec<String> = Vec::new();
    let window = Duration::from_millis(2 * t_owner_ms + 500);
    let t0 = std::time::Instant::now();
    while t0.elapsed() < window {
        for d in owner.expire_due() {
            dead.push(d.client.clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(hold);
    stop.store(true, Ordering::Release);

    assert!(
        dead.is_empty(),
        "a co-writer on an otherwise-healthy host must NEVER lose custody because \
         the sqz-meta lanes are occupied — the heartbeat is isolated from the \
         workload whose stall it survives (expired: {dead:?})"
    );
    assert!(
        !squeezefs::data_custody::poisoned(),
        "no self-fence: the lease never died, so nothing poisoned custody"
    );
    let landed = data_grant::stats().renewals - renewals_at_hold;
    assert!(
        landed >= 3,
        "custody renewals must keep landing while the meta lanes are wedged (got \
         {landed} in {window:?} at a ~433 ms cadence)"
    );
}

// ---------------------------------------------------------------------------
// 4. A wedged renewal attempt never occupies the lease venue past its
//    bound — and the §6.7 fence semantics stay byte-identical
// ---------------------------------------------------------------------------

/// Drop guard: a panicking test must never leak a 60 s tick hold into the
/// binary's later tests.
struct TickHoldGuard;

impl Drop for TickHoldGuard {
    fn drop(&mut self) {
        membership::TEST_RENEW_TICK_HOLD_MS.store(0, Ordering::Relaxed);
    }
}

/// The TIME side of the isolation law: one lease-class loop's wedged
/// attempt (emulated by the `TEST_RENEW_TICK_HOLD_MS` seam — the shape of
/// an RPC parked forever behind a hung owner) must be deadline-bounded at
/// `max(remaining-to-T_self/3, one cadence)`, so the OTHER lease-class
/// loop sharing the ONE `sqz-lease` venue keeps its cadence. And §6.7 is
/// byte-identical: once the hold lifts, the swept member's next attempt
/// completes, is refused, and self-fences exactly once — the fix makes
/// the fence unnecessary under load, never weaker.
///
/// (Red-by-construction against the pre-fix tree: with no per-attempt
/// deadline, the FIRST wedged attempt parks the loop forever — no retry
/// warnings, and no fence even after the wire recovers.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wedged_renewal_attempt_never_occupies_the_lease_venue_past_its_bound() {
    let _serial = serial();
    let _restore = restore();
    let _hold_guard = TickHoldGuard;
    let clocks = short_clocks();
    let t_owner_ms = clocks.t_owner.as_millis() as u64;

    // The membership plane whose member's ticks will wedge.
    let m_owner = MembershipOwner::arm(
        "owner-wedged",
        7,
        6,
        clocks.clone(),
        LeaseClock::monotonic(),
    )
    .expect("the owner arms");
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        SECRET.to_vec(),
        Arc::clone(&m_owner),
    )
    .expect("plane binds loopback");
    let rec = OwnerRecord {
        v: 1,
        id: "owner-wedged".to_string(),
        term: 7,
        endpoint: plane.endpoint().to_string(),
        ttl_ms: t_owner_ms,
        owner_claim_id: String::new(),
        ts: 0,
        pid: std::process::id(),
        boot: "boot-wedged".to_string(),
    };

    // The custody plane whose heartbeat shares the sqz-lease venue.
    let term = squeezefs::dlm::durable_term();
    let c_owner = WriteCustodyOwner::arm(
        "authority-wedged",
        term + 1,
        term,
        clocks.clone(),
        LeaseClock::monotonic(),
        None,
    )
    .expect("the custody authority arms");
    let router = data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&c_owner));
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), Arc::new(router))
        .expect("the authority listens");
    let custody = WriteCustodyClient::connect_with_clock(
        &listener.endpoint().to_string(),
        SECRET,
        "node-beside-the-wedge",
        LeaseClock::monotonic(),
        0,
    )
    .await
    .expect("the co-writer joins");
    let stop = Arc::new(AtomicBool::new(false));
    squeezefs::data_grant::spawn_custody_renewal(Arc::clone(&custody), Arc::clone(&stop));

    let fences_before = METRICS.membership_self_fences.load(Ordering::Relaxed);
    let arm = membership::join_as_writer_member(&rec, SECRET.to_vec(), "wedged-node", 0, None)
        .await
        .expect("the join must be admitted")
        .expect("a rendezvous record exists, so a member arms");
    let renewals_before = METRICS.membership_renewals.load(Ordering::Relaxed);
    let alive_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while METRICS.membership_renewals.load(Ordering::Relaxed) == renewals_before {
        assert!(
            std::time::Instant::now() < alive_deadline,
            "the member's renewal loop must land its first heartbeat before the wedge"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Wedge every subsequent membership tick (the hung-attempt shape),
    // and let both owners' TTL clocks run.
    membership::TEST_RENEW_TICK_HOLD_MS.store(60_000, Ordering::Relaxed);
    let custody_renewals_at_wedge = data_grant::stats().renewals;
    let mut custody_dead: Vec<String> = Vec::new();
    let window = Duration::from_millis(2 * t_owner_ms + 500);
    let t0 = std::time::Instant::now();
    while t0.elapsed() < window {
        for d in c_owner.expire_due() {
            custody_dead.push(d.client.clone());
        }
        // The membership owner sweeps the wedged member — that is the
        // DESIGNED outcome for a member whose attempts genuinely cannot
        // complete; this test asserts its neighbor's isolation.
        let _ = m_owner.expire_due();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        custody_dead.is_empty(),
        "the custody heartbeat shares the ONE sqz-lease venue with the wedged \
         membership loop — the per-attempt deadline bound is what keeps its \
         cadence (expired: {custody_dead:?})"
    );
    let landed = data_grant::stats().renewals - custody_renewals_at_wedge;
    assert!(
        landed >= 3,
        "custody renewals must keep landing beside the wedged membership ticks \
         (got {landed} in {window:?} at a ~433 ms cadence)"
    );

    // Lift the wedge: the member's next attempt completes, the owner
    // refuses it (swept), and the §6.7 self-fence fires EXACTLY as before
    // the fix — same ladder, same counters.
    membership::TEST_RENEW_TICK_HOLD_MS.store(0, Ordering::Relaxed);
    let fence_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while METRICS.membership_self_fences.load(Ordering::Relaxed) == fences_before {
        assert!(
            std::time::Instant::now() < fence_deadline,
            "once the wire recovers, the swept member must reach the §6.7 \
             self-fence through the ordinary ladder — the fix never weakens it"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        METRICS.membership_self_fences.load(Ordering::Relaxed) - fences_before,
        1,
        "the fence fires exactly once (a WRITER member's deadline fence is terminal)"
    );

    stop.store(true, Ordering::Release);
    arm.disarm().await;
    plane.shutdown();
}

// ---------------------------------------------------------------------------
// 5. A refused reclaim beats at the cadence law — never a spin — and lands
//    at the successor the observation names
// ---------------------------------------------------------------------------

/// **The un-parked reclaim arm's spin** (symmetric PR 12b, found by the
/// fidelity tier's N = 3 leg): a member whose owner died re-asserted its
/// reclaim against the DEAD listener at 1 ms for its whole `T_self`
/// (≈ 900 refusals a second per joiner — `renew_at_ms` sits in the past
/// after a failed renewal, so the shell's due read 1 ms), and it never
/// reached the successor whose record already stood in the rendezvous:
/// the un-parked arm dialed the venue it was spawned with, and only the
/// PARKED arm (PR 8, Issue 10) read `successor_endpoint()`. ONE law now,
/// before and after `T_self`: the beat is the cadence law over the window
/// left (`min(renew_interval, remaining / 3)`), cut short by the successor
/// observation, and the venue is the successor's when one is observed.
///
/// Two shapes, with the shipped 45 s clocks scaled to `short_clocks`
/// (`T_self` 1.3 s, cadence ≈ 433 ms):
/// (a) no successor — the writer member fences at `T_self` exactly as
///     before, having re-asserted a HANDFUL of times (the spin re-asserted
///     ≈ 900);
/// (b) a successor observed after the first refusal — the paced wait wakes
///     and the reclaim lands in the successor's grace window: no fence,
///     the member's lease custody at the successor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_reclaim_beats_at_the_cadence_law_never_a_spin() {
    let _serial = serial();
    let _restore = restore();
    membership::note_successor_observed(None);
    let clocks = short_clocks();
    let t_owner_ms = clocks.t_owner.as_millis() as u64;
    let owner = MembershipOwner::arm(
        "owner-dies-a",
        7,
        6,
        clocks.clone(),
        LeaseClock::monotonic(),
    )
    .expect("the owner arms");
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        SECRET.to_vec(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");
    let rec = OwnerRecord {
        v: 1,
        id: "owner-dies-a".to_string(),
        term: 7,
        endpoint: plane.endpoint().to_string(),
        ttl_ms: t_owner_ms,
        owner_claim_id: String::new(),
        ts: 0,
        pid: std::process::id(),
        boot: "boot-dies-a".to_string(),
    };
    let refusals0 = METRICS.membership_reclaim_refusals.load(Ordering::Relaxed);
    let fences0 = METRICS.membership_self_fences.load(Ordering::Relaxed);
    let renewals0 = METRICS.membership_renewals.load(Ordering::Relaxed);
    let arm = membership::join_as_writer_member(&rec, SECRET.to_vec(), "reclaim-node-a", 0, None)
        .await
        .expect("the join must be admitted")
        .expect("a rendezvous record exists, so a member arms");
    let alive_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while METRICS.membership_renewals.load(Ordering::Relaxed) == renewals0 {
        assert!(
            std::time::Instant::now() < alive_deadline,
            "the renewal loop must land its first heartbeat on a healthy host"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The owner DIES (its listener closes); no successor ever appears.
    plane.shutdown();
    let fence_deadline = std::time::Instant::now() + Duration::from_millis(4 * t_owner_ms);
    while METRICS.membership_self_fences.load(Ordering::Relaxed) == fences0 {
        assert!(
            std::time::Instant::now() < fence_deadline,
            "a WRITER member with no successor fences at T_self through the ordinary ladder"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let refused = METRICS.membership_reclaim_refusals.load(Ordering::Relaxed) - refusals0;
    assert!(
        (1..=60).contains(&refused),
        "a refused reclaim re-asserts at the cadence law over the window left (a handful of \
         attempts before T_self), never at 1 ms against the dead venue (got {refused} \
         refusals in ≈ {} ms)",
        clocks.t_self.as_millis()
    );
    arm.disarm().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_reclaim_lands_at_the_successor_the_observation_names() {
    let _serial = serial();
    let _restore = restore();
    membership::note_successor_observed(None);
    let clocks = short_clocks();
    let t_owner_ms = clocks.t_owner.as_millis() as u64;
    let owner = MembershipOwner::arm(
        "owner-dies-b",
        7,
        6,
        clocks.clone(),
        LeaseClock::monotonic(),
    )
    .expect("the owner arms");
    let plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        SECRET.to_vec(),
        Arc::clone(&owner),
    )
    .expect("plane binds loopback");
    let rec = OwnerRecord {
        v: 1,
        id: "owner-dies-b".to_string(),
        term: 7,
        endpoint: plane.endpoint().to_string(),
        ttl_ms: t_owner_ms,
        owner_claim_id: String::new(),
        ts: 0,
        pid: std::process::id(),
        boot: "boot-dies-b".to_string(),
    };
    let refusals0 = METRICS.membership_reclaim_refusals.load(Ordering::Relaxed);
    let fences0 = METRICS.membership_self_fences.load(Ordering::Relaxed);
    let renewals0 = METRICS.membership_renewals.load(Ordering::Relaxed);
    let arm = membership::join_as_writer_member(&rec, SECRET.to_vec(), "reclaim-node-b", 0, None)
        .await
        .expect("the join must be admitted")
        .expect("a rendezvous record exists, so a member arms");
    let alive_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while METRICS.membership_renewals.load(Ordering::Relaxed) == renewals0 {
        assert!(
            std::time::Instant::now() < alive_deadline,
            "the renewal loop must land its first heartbeat on a healthy host"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The owner DIES; the member's next renewal fails and its reclaim
    // against the dead venue is refused once.
    plane.shutdown();
    let first_refusal = std::time::Instant::now() + Duration::from_millis(2 * t_owner_ms);
    while METRICS.membership_reclaim_refusals.load(Ordering::Relaxed) == refusals0 {
        assert!(
            std::time::Instant::now() < first_refusal,
            "the renewal against the dead venue fails and the reclaim is refused"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The SUCCESSOR arms (term 8 over the predecessor's 7 — its grace
    // window admits reclaims) and the rendezvous observation names it: the
    // paced wait wakes, the reclaim re-points, the lease is custody there.
    let successor = MembershipOwner::arm(
        "owner-successor-b",
        8,
        7,
        clocks.clone(),
        LeaseClock::monotonic(),
    )
    .expect("the successor arms");
    // The successor's grace window (the mount path opens it from the
    // predecessor's durable claim set): reclaim admitted, fresh refused.
    successor.open_grace(vec!["reclaim-node-b".to_string()]);
    let succ_plane = MembershipPlane::start(
        MembershipPlaneConfig::loopback(),
        SECRET.to_vec(),
        Arc::clone(&successor),
    )
    .expect("successor plane binds loopback");
    membership::note_successor_observed(Some(succ_plane.endpoint().to_string()));
    let landed = std::time::Instant::now() + Duration::from_millis(t_owner_ms);
    while successor.epoch_of("reclaim-node-b").is_none() {
        assert!(
            std::time::Instant::now() < landed,
            "the refused reclaim must land at the observed successor inside one lease \
             period — the un-parked arm read no successor before PR 12b"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        METRICS.membership_self_fences.load(Ordering::Relaxed),
        fences0,
        "the member never fenced: its reclaim landed in the successor's grace window"
    );
    let refused = METRICS.membership_reclaim_refusals.load(Ordering::Relaxed) - refusals0;
    assert!(
        refused <= 8,
        "between the owner's death and the successor's observation the member re-asserted \
         at the cadence law, never a spin (got {refused} refusals)"
    );
    // The lease stays custody at the successor across a renewal.
    tokio::time::sleep(Duration::from_millis(
        clocks.renew_interval.as_millis() as u64 + 200,
    ))
    .await;
    let _ = successor.expire_due();
    assert!(
        successor.epoch_of("reclaim-node-b").is_some(),
        "the reclaimed lease renews at the successor on its cadence"
    );

    membership::note_successor_observed(None);
    arm.disarm().await;
    succ_plane.shutdown();
}

// ---------------------------------------------------------------------------
// 6. A custody renewal at a DEAD authority paces its dials at the cadence
//    law and fences at T_self from the LAST success — never a 25 ms storm
// ---------------------------------------------------------------------------

/// **The custody twin of contract 5** (symmetric PR 12b review round 3,
/// Issue 22 — the `sym-crash` legs' per-holder renewal after a manager
/// failover, an UNARMED S9 surface the shipped co-writer shares): a failed
/// renewal retried after a fixed 25 ms sleep, so a dead authority was
/// dialed ≈ 80 times a second (two dials per attempt — the verb's one
/// reconnect-and-resend) for the whole `T_self` window, with two log lines
/// per attempt. The S9 fence law itself was right — `T_self` from the
/// LAST successful renewal — and stays byte-identical; what changes is the
/// retry's PACE: the cadence law over the window left to `T_self`
/// (`clamp(remaining / 3, floor, one cadence)`), so a dead venue is dialed
/// a handful of times before the fence, never a storm.
///
/// The venue is the authority's OWN address after its listener died — a
/// raw acceptor counts every dial and closes each socket at once (the
/// handshake fails without a challenge, the shape of a daemon that died
/// and a successor that answers elsewhere).
///
/// RED against the 25 ms sleep: ≈ 100 dials inside the 1.3 s `T_self`
/// window (the bound below admits ≤ 30).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_custody_renewal_at_a_dead_authority_paces_its_dials_and_fences_at_t_self() {
    let _serial = serial();
    let _restore = restore();
    let clocks = short_clocks();
    let t_self_ms = clocks.t_self.as_millis() as u64;
    let cadence_ms = clocks.renew_interval.as_millis() as u64;
    let term = squeezefs::dlm::durable_term();
    let owner = WriteCustodyOwner::arm(
        "authority-dies",
        term + 1,
        term,
        clocks.clone(),
        LeaseClock::monotonic(),
        None,
    )
    .expect("the custody authority arms");
    let router = data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&owner));
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, SECRET.to_vec(), Arc::new(router))
        .expect("the authority listens");
    let addr = listener.endpoint();
    let client = WriteCustodyClient::connect_with_clock(
        &addr.to_string(),
        SECRET,
        "node-outlives-its-authority",
        LeaseClock::monotonic(),
        0,
    )
    .await
    .expect("the co-writer joins");
    let stop = Arc::new(AtomicBool::new(false));
    let renewals_before = data_grant::stats().renewals;
    let fences_before = data_grant::stats().self_fences;
    assert!(
        !squeezefs::data_custody::poisoned(),
        "the fixture starts unpoisoned"
    );
    squeezefs::data_grant::spawn_custody_renewal(Arc::clone(&client), Arc::clone(&stop));

    // The loop lands its first heartbeat on the live authority.
    let alive_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while data_grant::stats().renewals == renewals_before {
        assert!(
            std::time::Instant::now() < alive_deadline,
            "the custody renewal loop must land its first heartbeat on a healthy host"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let last_success = std::time::Instant::now();

    // The authority dies at its address; a raw acceptor at the SAME
    // address counts every dial and closes it at once.
    listener.shutdown();
    drop(listener);
    let acceptor = {
        let bind_deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match std::net::TcpListener::bind(addr) {
                Ok(l) => break l,
                Err(e) => {
                    assert!(
                        std::time::Instant::now() < bind_deadline,
                        "the dead authority's address must be re-bindable: {e}"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    };
    let dials = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let accepting = Arc::new(AtomicBool::new(true));
    let acceptor_thread = {
        let dials = Arc::clone(&dials);
        let accepting = Arc::clone(&accepting);
        acceptor
            .set_nonblocking(true)
            .expect("a non-blocking acceptor");
        std::thread::spawn(move || {
            while accepting.load(Ordering::Acquire) {
                match acceptor.accept() {
                    Ok((sock, _)) => {
                        dials.fetch_add(1, Ordering::Relaxed);
                        drop(sock);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        })
    };

    // The loop retries until its OWN deadline, then fences — the S9 law:
    // `T_self` from the LAST successful renewal, never from a failed dial.
    // A set authority's lease is the whole mount's custody, so its fence
    // is the process POISON (the shipped terminal fence for this scope).
    let fence_deadline = last_success + Duration::from_millis(t_self_ms + 2 * cadence_ms + 500);
    while !squeezefs::data_custody::poisoned() {
        assert!(
            std::time::Instant::now() < fence_deadline,
            "a co-writer whose authority died must reach the §6.7 self-fence at T_self \
             ({t_self_ms} ms) — the paced retry never weakens the fence (dials so far: {})",
            dials.load(Ordering::Relaxed)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let fenced_after = last_success.elapsed();
    accepting.store(false, Ordering::Release);
    acceptor_thread.join().expect("the acceptor thread");
    stop.store(true, Ordering::Release);

    assert_eq!(
        data_grant::stats().self_fences - fences_before,
        1,
        "the fence fires exactly once (the S9 terminal fence for the set authority's lease)"
    );
    assert!(
        fenced_after >= Duration::from_millis(t_self_ms.saturating_sub(cadence_ms + 100)),
        "the fence is T_self from the last successful renewal ({t_self_ms} ms) — never \
         earlier because a dial failed (fenced {fenced_after:?} after the last success)"
    );
    let dialed = dials.load(Ordering::Relaxed);
    assert!(
        dialed >= 2,
        "the loop retried the dead venue before its deadline (dials: {dialed})"
    );
    // Two dials per attempt (the verb's one reconnect-and-resend), attempts
    // no closer than the 100 ms pace floor across the T_self window, plus
    // the first failed cadence tick and the fencing attempt.
    let bound = 2 * (t_self_ms / 100 + 2);
    assert!(
        dialed <= bound,
        "a dead authority is dialed at the cadence law over the window left to T_self, \
         never a 25 ms storm: {dialed} dials in {fenced_after:?} (bound {bound})"
    );
}
