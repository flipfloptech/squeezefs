//! The data plane's standing WERO hold — its two device laws
//! (`.benchmarks/2026-08-16-mw-s8-arm.md` findings #1/#2; since PR 14 the
//! co-writer posture that found them is retired and BOTH laws are the
//! symmetric join ladder's rung 4 on a JOINED appender —
//! `data_custody::join_wero_as_appender`, pinned end to end in
//! `tests/sym_n_daemon_tests.rs`; this suite drives the two device arms
//! directly on the fake namespace).
//!
//! **Finding #1 (live, device-proven on the mwfleet tcp devsub):** the
//! first real second mount attempt DESTROYED its manager's live WERO
//! hold. A co-located mount's `join_wero_as_registrant` ran PR mutations
//! through the box's merged multipath head, whose ioctls round-robin
//! across ASSOCIATIONS — so the register ladder's "own-stale" proof
//! (`wire_host_id == the conflicting registration's host id`) matched the
//! LIVE holder's key and unregistered it, releasing the reservation for
//! the whole set (device truth after: `rtype 0, regctl 0, gen 6` on
//! nvme11n1) while the holder's `data_plane_fence_mode` gauge kept
//! reading 1. The law: a mount sharing its manager's BOOT is inside the
//! same PR arbitration domain, so it **ADOPTS the standing hold**
//! ([`data_custody::adopt_wero_colocated`]) — read-only evidence, zero
//! device mutations, the adopted key cross-checked against the durable
//! claim set's ENROLLED writer keys — and never registers a second key
//! through an association it cannot pin (KD-SYM-22). Cross-host members
//! (guests, real remotes — the two-host fixture) keep the register path,
//! where the head is their own and the ladder's proof is sound.
//!
//! **Finding #2:** the holder never RE-VERIFIED its data-plane WERO —
//! after the destruction above (or any PTPL-less target power cycle,
//! the `writer_guard_pr_reacquires` class) it kept gauging
//! `data_plane_fence_mode=1` with no reservation on the device.
//! [`WeroHold::reverify_and_heal`] is the meta-guard law mirrored onto
//! the data plane: a VANISHED hold is re-acquired (counted —
//! `data_plane_wero_reacquires`); a FOREIGN-usurped hold is never healed
//! over (foreign arbitration stays the claim/preempt path) — it poisons
//! process data custody and drops the gauge, loudly.

use squeezefs::data_custody::{self, WeroReverify};
use squeezefs::meta_backend::reservation::{
    self, FakeNvmeNamespace, FakeReservationClient, ReservationClient,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// Process-global reservation overrides + custody poison: serialize.
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

struct Restore(Vec<PathBuf>);

impl Drop for Restore {
    fn drop(&mut self) {
        for p in &self.0 {
            reservation::clear_override(p);
        }
        data_custody::test_clear_poison();
    }
}

const AUTHORITY_KEY: u64 = 0xA0A0_5EED_0000_0001;

fn ns_path(tag: &str) -> PathBuf {
    PathBuf::from(format!("/dev/fake-colocated-{tag}"))
}

/// A namespace whose standing WERO is held by the MANAGER's key under
/// the manager's own association — the device state a co-located joiner
/// meets at its join.
fn manager_held_ns(tag: &str) -> (Arc<FakeNvmeNamespace>, PathBuf) {
    let ns = FakeNvmeNamespace::new();
    let authority = FakeReservationClient::new(
        ns.clone(),
        "nqn.2014-08.org.nvmexpress:uuid:authority",
        "cafef1e7-0000-4000-8000-000000000001",
    );
    authority
        .register(AUTHORITY_KEY)
        .expect("authority register");
    authority
        .acquire_write_exclusive_registrants_only(AUTHORITY_KEY)
        .expect("authority WERO acquire");
    let p = ns_path(tag);
    // The joiner's own association (a DIFFERENT identity — the merged
    // head's default association) is what resolve_for_mount answers.
    let cw = FakeReservationClient::new(
        ns.clone(),
        "nqn.2014-08.org.nvmexpress:uuid:cowriter",
        "cafef1e7-0000-4000-8000-000000000002",
    );
    reservation::install_override(&p, cw);
    (ns, p)
}

// ===========================================================================
// Finding #1 — the co-located adoption
// ===========================================================================

/// Contract: a co-located joiner ADOPTS the standing WERO hold — the
/// evidence names the manager's enrolled key, and NOTHING on the device
/// moves: no register, no unregister, no acquire, not at join and not at
/// drop. (The destroyed-holder shape this replaces: the register
/// ladder's own-stale proof matched the LIVE holder through the merged
/// head's round-robined associations and unregistered it, releasing the
/// reservation for the whole set.) `join_wero_as_appender(colocated =
/// true)` is this arm verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_co_located_joiner_adopts_the_standing_hold_and_mutates_nothing() {
    let _s = serial();
    let (ns, p) = manager_held_ns("adopt");
    let _r = Restore(vec![p.clone()]);

    let join =
        data_custody::join_wero_as_appender(std::slice::from_ref(&p), true, &[AUTHORITY_KEY], None)
            .expect("the co-located shape adopts the standing hold");
    let ev = join.evidence();
    assert!(ev.pr_capable && ev.wero && ev.reservation_held && ev.registered);
    assert_eq!(
        ev.key, AUTHORITY_KEY,
        "the adopted key IS the manager's (same PR arbitration domain — KD-SYM-22)"
    );
    assert_eq!(
        ns.holder(),
        Some(AUTHORITY_KEY),
        "the hold stands untouched"
    );
    assert!(ns.is_registered(AUTHORITY_KEY));
    assert_eq!(ns.unregister_count(), 0, "adoption unregisters NOTHING");

    drop(join);
    assert_eq!(
        ns.holder(),
        Some(AUTHORITY_KEY),
        "an adopted hold's teardown leaves the device exactly as found"
    );
    assert!(ns.is_registered(AUTHORITY_KEY));
    assert_eq!(
        ns.unregister_count(),
        0,
        "teardown unregisters NOTHING it never registered"
    );
}

/// Contract: no standing hold ⇒ refuse (the manager's arm is the
/// remedy), never a silent detection-grade admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adoption_refuses_when_no_standing_hold_exists() {
    let _s = serial();
    let ns = FakeNvmeNamespace::new();
    let p = ns_path("empty");
    let cw = FakeReservationClient::new(
        ns.clone(),
        "nqn.2014-08.org.nvmexpress:uuid:cowriter",
        "cafef1e7-0000-4000-8000-000000000002",
    );
    reservation::install_override(&p, cw);
    let _r = Restore(vec![p.clone()]);

    let err = data_custody::adopt_wero_colocated(std::slice::from_ref(&p), &[AUTHORITY_KEY])
        .expect_err("nothing to adopt")
        .to_string();
    assert!(
        err.contains("no Write Exclusive") || err.to_lowercase().contains("arm"),
        "the refusal names the missing hold / the manager's arm: {err}"
    );
}

/// Contract: a standing hold whose key the durable claim set does NOT
/// enroll is somebody else's fence — adopting it would authenticate this
/// mount against a manager it was never admitted by. Refuse loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adoption_refuses_a_holder_key_the_claim_set_does_not_enroll() {
    let _s = serial();
    let (ns, p) = manager_held_ns("foreignkey");
    let _r = Restore(vec![p.clone()]);

    let err = data_custody::adopt_wero_colocated(std::slice::from_ref(&p), &[0xDEAD])
        .expect_err("an un-enrolled holder key is not adoptable")
        .to_string();
    assert!(
        err.contains("enroll") || err.contains("claim set"),
        "the refusal names the enrollment cross-check: {err}"
    );
    assert_eq!(
        ns.holder(),
        Some(AUTHORITY_KEY),
        "and the device is untouched"
    );
}

/// Contract: a REMOTE joiner (its own association — the two-host shape)
/// keeps the register path: `join_wero_as_appender(colocated = false)`
/// registers the joiner's own key under the standing hold, the manager's
/// fence untouched, and its teardown unregisters exactly that key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_joiner_registers_its_own_key_under_the_standing_hold() {
    let _s = serial();
    let (ns, p) = manager_held_ns("remote");
    let _r = Restore(vec![p.clone()]);

    let join = data_custody::join_wero_as_appender(
        std::slice::from_ref(&p),
        false,
        &[AUTHORITY_KEY],
        Some(0xB0B0_0000_0000_0007),
    )
    .expect("a remote joiner registers under the standing hold");
    let ev = join.evidence();
    assert_eq!(
        ev.key, 0xB0B0_0000_0000_0007,
        "the caller-chosen key (one key per member)"
    );
    assert!(ns.is_registered(ev.key), "the device registered it");
    assert_eq!(
        ns.holder(),
        Some(AUTHORITY_KEY),
        "the manager's fence is untouched by a remote registration"
    );
    drop(join);
    assert!(
        !ns.is_registered(0xB0B0_0000_0000_0007),
        "the leave unregisters the joiner's own key"
    );
    assert_eq!(ns.holder(), Some(AUTHORITY_KEY));
}

// ===========================================================================
// Finding #2 — the holder's hold re-verification
// ===========================================================================

/// Contract: a VANISHED reservation (a PTPL-less power cycle; the
/// destroyed-hold shape finding #1 produced live) is detected by the
/// cadence re-verify and RE-ACQUIRED — the `writer_guard_pr_reacquires`
/// law mirrored onto the data plane, counted on
/// `data_plane_wero_reacquires`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vanished_wero_hold_is_detected_and_reacquired_by_reverify() {
    let _s = serial();
    let ns = FakeNvmeNamespace::new();
    let p = ns_path("vanish");
    let me = FakeReservationClient::new(
        ns.clone(),
        "nqn.2014-08.org.nvmexpress:uuid:authority",
        "cafef1e7-0000-4000-8000-000000000001",
    );
    reservation::install_override(&p, me);
    let _r = Restore(vec![p.clone()]);

    let hold = data_custody::acquire_wero(std::slice::from_ref(&p)).expect("the manager arms");
    let key = hold.key();
    assert_eq!(ns.holder(), Some(key));
    assert_eq!(
        hold.reverify_and_heal(),
        WeroReverify::Held,
        "a standing hold re-verifies clean"
    );

    // The target loses everything (PTPL-less power cycle).
    ns.power_cycle();
    assert_eq!(ns.holder(), None);

    let before = data_custody::wero_reacquires();
    assert_eq!(
        hold.reverify_and_heal(),
        WeroReverify::Healed,
        "a vanished hold is re-acquired, not gauged over"
    );
    assert_eq!(ns.holder(), Some(key), "the fence is BACK at the device");
    assert_eq!(
        data_custody::wero_reacquires(),
        before + 1,
        "the heal is counted (data_plane_wero_reacquires)"
    );
    drop(hold);
}

/// Contract: a hold USURPED by a foreign key is never healed over —
/// foreign arbitration stays the claim/preempt path. The re-verify
/// answers `Usurped`, poisons process data custody (the RES-6 latch) and
/// drops the fence-mode gauge: the gauge can no longer lie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_usurped_hold_poisons_instead_of_healing_over() {
    let _s = serial();
    let ns = FakeNvmeNamespace::new();
    let p = ns_path("usurp");
    let me = FakeReservationClient::new(
        ns.clone(),
        "nqn.2014-08.org.nvmexpress:uuid:authority",
        "cafef1e7-0000-4000-8000-000000000001",
    );
    reservation::install_override(&p, me.clone());
    let _r = Restore(vec![p.clone()]);

    let hold = data_custody::acquire_wero(std::slice::from_ref(&p)).expect("the manager arms");

    // A foreign host preempts our registration and takes the hold.
    let foreign = FakeReservationClient::new(
        ns.clone(),
        "nqn.2014-08.org.nvmexpress:uuid:foreign",
        "cafef1e7-0000-4000-8000-00000000f0f0",
    );
    foreign.register(0xF0F0).expect("foreign register");
    foreign
        .preempt_registrants_only(0xF0F0, hold.key())
        .expect("foreign preempt");
    assert_eq!(ns.holder(), Some(0xF0F0));

    assert!(!data_custody::poisoned());
    match hold.reverify_and_heal() {
        WeroReverify::Usurped { key } => assert_eq!(key, 0xF0F0),
        other => panic!("a usurped hold must answer Usurped, got {other:?}"),
    }
    assert!(
        data_custody::poisoned(),
        "usurpation latches the RES-6 custody poison — never a silent gauge"
    );
    assert_eq!(
        ns.holder(),
        Some(0xF0F0),
        "and the foreign hold is NOT preempted back (arbitration stays the claim path)"
    );
    drop(hold);
}
