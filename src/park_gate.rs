//! **The park gate** — the process plane around [`crate::park_core`]:
//! KD-SYM-15 (extended), docs/design-symmetric-metadata.md §5.5.3 (PR 8).
//!
//! A manager's death is its home shard's membership outage: the shard's
//! members cannot renew until the successor arms (`manager_failover_bound_ms`
//! ≈ 46.5 s) while their own `T_self` (≈ 40–44 s) expires FIRST by
//! construction. The shipped Writer's `self_fence` poisons `data_custody`
//! — terminal until remount — which is the wrong action for a symmetric
//! appender, because S6's successor opens a grace window admitting
//! reclaim and everything the member holds is reclaimable. So the
//! symmetric appender's `T_self` action is **PARK**:
//!
//! * the conveyor stops ADMITTING new commits — [`pre_admission`] is a
//!   distinct pre-admission STATE, consulted by the pass before ring
//!   admission and NEVER feeding `note_journal_failure` (the D1.b
//!   escalation cannot turn a park into the fail-stop it exists to avoid;
//!   `SQUEEZEFS_TIMEOUT` = 30 s < `T_park_max` by construction);
//! * in-flight entries land but their ACKS ARE HELD — [`hold_acks`] on the
//!   durability lane, released in journal order on the grant, failed
//!   `EIO` at expiry;
//! * reads keep serving, token grants/recalls continue ([`admits_token_
//!   service`]), DMA to granted blocks continues, the checkpoint/flush
//!   task CONTINUES (the flush ceiling holds through a park); the page
//!   stays `Live`; nothing is poisoned;
//! * the FUSE op watchdog labels parked ops ([`is_parked`]) and the
//!   bounded-error timers on barrier / `fsync` waits are SUSPENDED
//!   ([`bounded_timers_suspended`]) while parked;
//! * **the park is bounded**: [`t_park_max_ms`] = `manager_failover_bound_ms
//!   + grace_ms` — past it the member self-fences terminally exactly as
//!   today ([`crate::data_custody::poison`], `EIO` to the parked ops);
//!   `appender_park_expiries` is the ONE terminal signal.
//!
//! `data_custody::poison` therefore SPLITS ([`FenceClass`]): poison at
//! `T_self` only for objects whose lease is NOT reclaimable under the S6
//! grace contract (today's S9 remote-custody semantics); park for slot
//! leases and the appender region.
//!
//! Gauges: `appender_parked` (posture 0/1), `appender_parks`,
//! `appender_park_ns` (exact-sum `wait_successor / reclaim_rtt / total`),
//! `appender_park_expiries` (must-stay-0 on a healthy failover),
//! `slot_lease_reclaims`. Contracts: `tests/sym_block_grant_tests.rs`;
//! loom `park_gate_models`.

use crate::park_core::{EnterVerdict, ParkCore};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static GATE: once_cell::sync::Lazy<ParkCore> = once_cell::sync::Lazy::new(ParkCore::new);
/// Parked committers wait here; the release wakes them all.
static ADMISSION_WAKE: squeezefs_ipc::sqz_notify::Notify = squeezefs_ipc::sqz_notify::Notify::new();
/// The durability lane's held acks wait here.
static ACK_WAKE: squeezefs_ipc::sqz_notify::Notify = squeezefs_ipc::sqz_notify::Notify::new();
/// `true` once the mount armed the symmetric plane as an APPENDER — the
/// posture whose `T_self` action is the park. Set by the plane's arm,
/// cleared at its drop; a mount that never arms takes the shipped poison.
static SYMMETRIC_APPENDER: AtomicBool = AtomicBool::new(false);
/// The mount's HOME metadata volume ordinal (the appender page's
/// `home_volume`) — the membership shard it renews with (KD-SYM-15).
static HOME_VOLUME: AtomicU64 = AtomicU64::new(0);
/// `T_park_max` in force (0 = not armed).
static T_PARK_MAX_MS: AtomicU64 = AtomicU64::new(0);
/// `appender_park_ns` — exact-sum `wait_successor / reclaim_rtt / total`.
static PARK_NS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
/// The park's raise instant on the process monotonic clock (0 = not
/// parked) — the FUSE watchdog's age witness (the lease clock the park is
/// judged by is the member's own, a manual one in the contracts).
static PARKED_AT_MONO_NS: AtomicU64 = AtomicU64::new(0);

/// Which fence class an object's lease falls in at `T_self`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceClass {
    /// A lease NOT reclaimable under the S6 grace contract (S9 remote
    /// custody): the shipped poison.
    RemoteCustody,
    /// A slot lease / the appender region: reclaimable in the successor's
    /// grace window — the park.
    SymmetricAppender,
}

/// What `T_self` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TSelfAction {
    /// Process data custody was poisoned (terminal).
    Poisoned,
    /// The gate parked (acks held, reclaim pending).
    Parked,
    /// A park already stood.
    AlreadyParked,
}

/// `T_park_max = manager_failover_bound_ms + grace_ms` — the longest a live
/// successor can take to arm and admit reclaims (derived; tie-tested in
/// `derivation_sweep_tests`).
pub fn t_park_max_ms(failover_bound_ms: u64, grace_ms: u64) -> u64 {
    failover_bound_ms.saturating_add(grace_ms)
}

/// Arm the park posture: this mount is a symmetric APPENDER homed on
/// `home_volume`, its park bounded by `t_park_max`.
pub fn arm_symmetric_appender(home_volume: u16, t_park_max: u64) {
    HOME_VOLUME.store(u64::from(home_volume), Ordering::Release);
    T_PARK_MAX_MS.store(t_park_max.max(1), Ordering::Release);
    SYMMETRIC_APPENDER.store(true, Ordering::Release);
}

/// `T_park_max` for a failover bound under the S6 clocks in force: the
/// grace term is `LeaseClocks::grace` — the owner-failover window — read
/// off the derivation (review round 1, Issue 16).
pub fn t_park_max_for(failover_bound_ms: u64, clocks: &crate::membership::LeaseClocks) -> u64 {
    t_park_max_ms(failover_bound_ms, clocks.grace.as_millis() as u64)
}

/// The leave is releasing the held acks of LANDED entries over a standing
/// park (Issue 18): `hold_acks` hands them back `Ok` while the door is
/// closed to the committers that never landed.
static LEAVE_RELEASING_ACKS: AtomicBool = AtomicBool::new(false);
/// The leave was REFUSED over a park — the process-wide latch every volume
/// of the set consults at its own `shutdown` (review round 2, Issue 24):
/// the refusal must be set-wide, and the gate word alone is cleared by the
/// first volume's `close_at_leave`. Sticky for the process (a parked
/// appender's dismount is the crash shape on every volume).
static LEAVE_REFUSED: AtomicBool = AtomicBool::new(false);

/// `true` ⇔ this process's leave was refused over a standing park: every
/// volume's clean leave is refused from here (the pages stay `Live`).
pub fn leave_refused() -> bool {
    LEAVE_REFUSED.load(Ordering::SeqCst)
}

/// Disarm the posture (the plane's drop / the leave / test teardown). Over
/// a STANDING park the leave splits the two populations (review round 1,
/// Issue 18): the held acks of LANDED entries are released `Ok` — the
/// entries are in ring 0, replayable, and the successor's recovery reads
/// them — while the committers parked at the door are FAILED (`EIO`): the
/// leave admits nothing into a ring whose region a successor may already
/// have recovered (PR 10 owns the leave-vs-recovery interplay; this is the
/// boundary it inherits).
pub fn disarm_symmetric_appender() {
    SYMMETRIC_APPENDER.store(false, Ordering::Release);
    T_PARK_MAX_MS.store(0, Ordering::Release);
    close_at_leave();
}

/// **The leave over a standing park** (Issue 18): the held acks of LANDED
/// entries are released as they are (the entries are in ring 0,
/// replayable), the door closes on the committers parked at it (`EIO` —
/// they never landed, and a leave admits nothing into a region a
/// successor may already have recovered). `true` ⇔ a park was standing.
/// Not an expiry: `appender_park_expiries` does not move.
pub fn close_at_leave() -> bool {
    if !GATE.is_parked() {
        return false;
    }
    LEAVE_REFUSED.store(true, Ordering::SeqCst);
    LEAVE_RELEASING_ACKS.store(true, Ordering::SeqCst);
    ACK_WAKE.notify_waiters();
    let closed = GATE.close_at_leave();
    if closed {
        ADMISSION_WAKE.notify_waiters();
        log::warn!(
            "symmetric appender LEAVING while parked: the landed entries' held acks are \
             released (in ring 0, replayable); the committers parked at the door are failed — \
             nothing is admitted into a region a successor may have recovered"
        );
    }
    closed
}

/// `true` ⇔ this mount's `T_self` class is the park.
pub fn symmetric_appender_armed() -> bool {
    SYMMETRIC_APPENDER.load(Ordering::Acquire)
}

/// The mount's home volume ordinal (0 until armed — the shipped shape).
pub fn home_volume() -> u16 {
    HOME_VOLUME.load(Ordering::Acquire) as u16
}

/// `T_park_max` in force.
pub fn t_park_max_in_force_ms() -> u64 {
    T_PARK_MAX_MS.load(Ordering::Acquire)
}

/// The class's `T_self` action: park for a symmetric appender, poison
/// otherwise (`data_custody::poison` — the split's one entry).
pub fn fence_at_t_self(class: FenceClass, now_ms: u64, reason: &str) -> TSelfAction {
    match class {
        FenceClass::RemoteCustody => {
            crate::data_custody::poison(reason);
            TSelfAction::Poisoned
        }
        FenceClass::SymmetricAppender => match GATE.park(now_ms) {
            Some(inflight) => {
                PARKED_AT_MONO_NS.store(
                    crate::mono_core::monotonic_ns_u64().max(1),
                    Ordering::Release,
                );
                log::warn!(
                    "symmetric appender PARKED at T_self ({reason}): {inflight} commit(s) in \
                     flight will land with their acks HELD, admission waits, reads and the \
                     checkpoint task continue; reclaim under the successor's grace within \
                     T_park_max = {} ms, else the member self-fences (appender_parks; \
                     design-symmetric-metadata §5.5.3)",
                    t_park_max_in_force_ms()
                );
                TSelfAction::Parked
            }
            None if GATE.is_expired() => TSelfAction::Poisoned,
            None => TSelfAction::AlreadyParked,
        },
    }
}

/// The posture word `appender_parked`.
pub fn is_parked() -> bool {
    GATE.is_parked()
}

/// `true` ⇔ the park expired (terminal — custody poisoned).
pub fn is_expired() -> bool {
    GATE.is_expired()
}

/// The FUSE op watchdog's label and the barrier/`fsync` bounded-error
/// timers' suspension read the same word.
pub fn bounded_timers_suspended() -> bool {
    GATE.is_parked()
}

/// A parked lessee is still the lock master for its slots: token grants
/// and recalls continue; an EXPIRED park poisoned custody and its slots
/// are the successor's. Read by the token holder's dispatch
/// (`meta_ship::token_plane::TokenService` — every verb refuses past the
/// expiry, `dlm_token_park_expired_refusals`).
pub fn admits_token_service() -> bool {
    !GATE.is_expired()
}

/// The RAII pass an admitted commit holds to its terminal outcome.
#[derive(Debug)]
pub struct AdmissionPass(());

impl Drop for AdmissionPass {
    fn drop(&mut self) {
        GATE.leave();
    }
}

/// **The pre-admission door** (§5.5.3 item 1): admitted at once while
/// running; parked — waiting on the release, then re-entering — while
/// parked; `EIO` once expired. Never counts a stall, never escalates.
pub async fn pre_admission() -> std::result::Result<AdmissionPass, std::io::Error> {
    loop {
        // Register-recheck-await: the wake is armed BEFORE the verdict is
        // read, so a release between the two is never lost.
        let released = ADMISSION_WAKE.notified();
        match GATE.try_enter() {
            EnterVerdict::Admitted => return Ok(AdmissionPass(())),
            EnterVerdict::Expired => {
                return Err(std::io::Error::other(
                    "commit refused: this appender's park outlived T_park_max and it \
                     self-fenced (appender_park_expiries)",
                ));
            }
            EnterVerdict::Parked => released.await,
        }
    }
}

/// **Hold the lane's acks** (§5.5.3 item 1's D-2 discipline): while
/// parked the outcomes wait here — in journal order, since the lane
/// answers windows in handoff order and every later window queues behind
/// this hold — and are handed back on the release; at expiry every `Ok`
/// becomes `EIO`; at the LEAVE they are handed back as they are (Issue
/// 18). Running ⇒ passed through untouched (one relaxed load). The hold
/// rests on the TOKEN (review round 1, Issue 8): every entry here still
/// owns its `AdmissionPass` (taken at the door, dropped at fan-out), so
/// the parker's in-flight count is exactly the commits whose acks this
/// hold will keep — the Dekker pair `park_core` models is what production
/// relies on.
pub async fn hold_acks<T, E>(
    outcomes: Vec<(T, std::result::Result<(), E>)>,
    expired: impl Fn(std::io::Error) -> E,
) -> Vec<(T, std::result::Result<(), E>)> {
    if !GATE.is_parked() {
        return outcomes;
    }
    GATE.note_acks_held(outcomes.len() as u64);
    loop {
        let released = ACK_WAKE.notified();
        if LEAVE_RELEASING_ACKS.load(Ordering::SeqCst) {
            return outcomes;
        }
        match GATE.state() {
            crate::park_core::PARKED => released.await,
            crate::park_core::EXPIRED => {
                return outcomes
                    .into_iter()
                    .map(|(t, o)| {
                        (
                            t,
                            o.and_then(|()| {
                                Err(expired(std::io::Error::other(
                                    "ack failed: the appender's park outlived T_park_max \
                                     (appender_park_expiries)",
                                )))
                            }),
                        )
                    })
                    .collect();
            }
            _ => return outcomes,
        }
    }
}

/// **Release the park** (the successor's grant confirmed the leases):
/// wakes the parked committers and the held acks. `reclaim_rtt_ns` is the
/// reclaim renewal's measured wall, `wait_successor_ns` the park's age
/// when the successor was observed. `true` ⇔ a park was released.
pub fn release(wait_successor_ns: u64, reclaim_rtt_ns: u64) -> bool {
    let moved = GATE.release();
    if moved {
        PARK_NS[0].fetch_add(wait_successor_ns, Ordering::Relaxed);
        PARK_NS[1].fetch_add(reclaim_rtt_ns, Ordering::Relaxed);
        PARK_NS[2].fetch_add(
            wait_successor_ns.saturating_add(reclaim_rtt_ns),
            Ordering::Relaxed,
        );
        ADMISSION_WAKE.notify_waiters();
        ACK_WAKE.notify_waiters();
        log::warn!(
            "symmetric appender RECLAIMED its leases under the successor's grace: held acks \
             released in journal order, admission resumed (slot_lease_reclaims)"
        );
    }
    moved
}

/// **Expire the park if due** at `now_ms`: past `T_park_max` the member
/// self-fences terminally — custody poisoned, parked committers and held
/// acks failed. `true` ⇔ this call expired it.
pub fn expire_if_due(now_ms: u64, reason: &str) -> bool {
    let bound = t_park_max_in_force_ms();
    if bound == 0 {
        return false;
    }
    let moved = GATE.expire(now_ms, bound);
    if moved {
        crate::data_custody::poison(&format!(
            "symmetric appender's park outlived T_park_max ({bound} ms): {reason}"
        ));
        ADMISSION_WAKE.notify_waiters();
        ACK_WAKE.notify_waiters();
        log::error!(
            "symmetric appender park EXPIRED after {bound} ms: the member self-fenced \
             terminally (appender_park_expiries — must-stay-0 on a healthy failover)"
        );
    }
    moved
}

/// The park's age at `now_ms` (0 when not parked).
pub fn parked_for_ms(now_ms: u64) -> u64 {
    GATE.parked_for_ms(now_ms)
}

/// The standing park's age on the process monotonic clock, ms (0 when not
/// parked) — what the FUSE op watchdog compares an op's age against.
pub fn parked_age_mono_ms() -> u64 {
    if !GATE.is_parked() {
        return 0;
    }
    let since = PARKED_AT_MONO_NS.load(Ordering::Acquire);
    if since == 0 {
        return 0;
    }
    crate::mono_core::monotonic_ns_u64().saturating_sub(since) / 1_000_000
}

/// Commits in flight past the door.
pub fn inflight() -> u64 {
    GATE.inflight()
}

/// `appender_parks`.
pub fn parks() -> u64 {
    GATE.parks()
}

/// `appender_park_expiries`.
pub fn expiries() -> u64 {
    GATE.expiries()
}

/// `slot_lease_reclaims`.
pub fn reclaims() -> u64 {
    GATE.reclaims()
}

/// Acks held across parks.
pub fn acks_held() -> u64 {
    GATE.acks_held()
}

/// `appender_park_ns` — `[wait_successor, reclaim_rtt, total]`.
pub fn park_ns() -> [u64; 3] {
    [
        PARK_NS[0].load(Ordering::Relaxed),
        PARK_NS[1].load(Ordering::Relaxed),
        PARK_NS[2].load(Ordering::Relaxed),
    ]
}

/// **Test seam**: reset the gate to `Running` with its counters cleared
/// (the `data_custody::test_clear_poison` precedent) — the contracts run
/// several parks in one process.
pub fn test_reset() {
    disarm_symmetric_appender();
    LEAVE_RELEASING_ACKS.store(false, Ordering::SeqCst);
    LEAVE_REFUSED.store(false, Ordering::SeqCst);
    GATE.test_reset();
    for w in &PARK_NS {
        w.store(0, Ordering::Relaxed);
    }
    ADMISSION_WAKE.notify_waiters();
    ACK_WAKE.notify_waiters();
}

/// **Expire the park NOW** whatever its age — the reclaim was answered
/// "not custody" (evicted past grace, or an owner that never lost us): the
/// lease is gone and the shipped terminal fence follows. `true` ⇔ a park
/// was standing.
pub fn expire_now(reason: &str) -> bool {
    let moved = GATE.expire(u64::MAX, 0);
    if moved {
        crate::data_custody::poison(&format!("symmetric appender's reclaim refused: {reason}"));
        ADMISSION_WAKE.notify_waiters();
        ACK_WAKE.notify_waiters();
    }
    moved
}
