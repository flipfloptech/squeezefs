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
use squeezefs::membership::{
    self, LeaseClock, LeaseClocks, MembershipOwner, OwnerRecord,
};
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
                    squeezefs_ipc::sqz_time::sleep(Duration::from_millis(
                        client.renewal_due_ms(),
                    ))
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
