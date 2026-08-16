//! Rung-9 findings #1/#2 — the CO-LOCATED co-writer's device half
//! (`.benchmarks/2026-08-16-mw-s8-arm.md`; docs/operations.md §Multi-writer
//! co-writer mounts, the stated "honest residual").
//!
//! **Finding #1 (live, device-proven on the mwfleet tcp devsub):** the
//! first real co-writer mount attempt DESTROYED its authority's live WERO
//! hold. A co-located co-writer's `join_wero_as_registrant` ran PR
//! mutations through the box's merged multipath head, whose ioctls
//! round-robin across ASSOCIATIONS — so the register ladder's "own-stale"
//! proof (`wire_host_id == the conflicting registration's host id`)
//! matched the LIVE authority's holder key and unregistered it, releasing
//! the reservation for the whole set (device truth after: `rtype 0,
//! regctl 0, gen 6` on nvme11n1) while the authority's
//! `data_plane_fence_mode` gauge kept reading 1. The fix is the shape
//! ops.md already documents: a co-writer sharing its authority's BOOT is
//! inside the same PR arbitration domain, so it **ADOPTS the standing
//! hold** ([`data_custody::adopt_wero_colocated`]) — read-only evidence,
//! zero device mutations, the adopted key cross-checked against the
//! durable claim set's ENROLLED writer keys — and never registers a
//! second key through an association it cannot pin. Cross-host co-writers
//! (guests, real remotes — the 5b/VM shapes) keep the register path,
//! where the head is their own and the ladder's proof is sound.
//!
//! **Finding #2:** the authority never RE-VERIFIES its data-plane WERO —
//! after the destruction above (or any PTPL-less target power cycle,
//! the `writer_guard_pr_reacquires` class) it kept gauging
//! `data_plane_fence_mode=1` with no reservation on the device.
//! [`WeroHold::reverify_and_heal`] is the meta-guard law mirrored onto
//! the data plane: a VANISHED hold is re-acquired (counted —
//! `data_plane_wero_reacquires`); a FOREIGN-usurped hold is never healed
//! over (foreign arbitration stays the claim/preempt path) — it poisons
//! process data custody and drops the gauge, loudly.

use squeezefs::cowriter::{
    self, AdmissionRequest, AuthorityLeaseEvidence, RegistrantEvidence, VolumeAdmissionEvidence,
};
use squeezefs::data_custody::{self, WeroReverify};
use squeezefs::membership::{ClaimSet, ClaimSetMember, MemberIdentity, MemberRole};
use squeezefs::meta_backend::kv::backend::WriterClaim;
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

/// A namespace whose standing WERO is held by the AUTHORITY's key under
/// the authority's own association — the device state a co-located
/// co-writer meets at mount time.
fn authority_held_ns(tag: &str) -> (Arc<FakeNvmeNamespace>, PathBuf) {
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
    // The co-writer's own association (a DIFFERENT identity — the merged
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

/// Contract: a co-located co-writer ADOPTS the standing WERO hold — the
/// evidence names the authority's enrolled key, and NOTHING on the device
/// moves: no register, no unregister, no acquire, not at join and not at
/// drop. (The destroyed-authority shape this replaces: the register
/// ladder's own-stale proof matched the LIVE holder through the merged
/// head's round-robined associations and unregistered it, releasing the
/// reservation for the whole set.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_co_located_co_writer_adopts_the_standing_hold_and_mutates_nothing() {
    let _s = serial();
    let (ns, p) = authority_held_ns("adopt");
    let _r = Restore(vec![p.clone()]);

    let join = data_custody::adopt_wero_colocated(std::slice::from_ref(&p), &[AUTHORITY_KEY])
        .expect("the co-located shape adopts the standing hold");
    let ev = join.evidence();
    assert!(ev.pr_capable && ev.wero && ev.reservation_held && ev.registered);
    assert_eq!(
        ev.key, AUTHORITY_KEY,
        "the adopted key IS the authority's (same PR arbitration domain — the ops.md residual)"
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

/// Contract: no standing hold ⇒ refuse (the authority's arm is the
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
        "the refusal names the missing hold / the authority arm: {err}"
    );
}

/// Contract: a standing hold whose key the durable claim set does NOT
/// enroll is somebody else's fence — adopting it would authenticate this
/// mount against an authority it was never admitted by. Refuse loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adoption_refuses_a_holder_key_the_claim_set_does_not_enroll() {
    let _s = serial();
    let (ns, p) = authority_held_ns("foreignkey");
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

/// Contract: the co-located test is the BOOT id — a `writer_claim` whose
/// `boot` equals this kernel's boot id is a same-host authority (the
/// merged-head shape where PR mutations are unsound), any other boot is a
/// remote authority (the register path stays).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_location_is_decided_by_the_claim_boot_id() {
    let our_boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("boot_id readable on Linux")
        .trim()
        .to_string();
    let req = |boot: &str| -> AdmissionRequest {
        let mut set = ClaimSet::empty(7);
        set.durable = true;
        set.members.push(ClaimSetMember {
            identity: MemberIdentity {
                id: "node_00000000aaaaaaaa".into(),
                role: MemberRole::Writer,
                pid: 0,
                boot: String::new(),
                endpoint: None,
                pr_key: AUTHORITY_KEY,
            },
            ts: 0,
        });
        AdmissionRequest {
            multi_writer: true,
            role_co_writer: true,
            read_only: false,
            node_id: "node_00000000aaaaaaaa.m00000001".into(),
            custody_endpoint: Some("127.0.0.1:7100".into()),
            volumes: vec![VolumeAdmissionEvidence {
                path: PathBuf::from("/dev/fake-meta"),
                features_incompat: cowriter::REQUIRED_INCOMPAT,
                claim: Some(WriterClaim {
                    id: "authority-claim".into(),
                    ts: 0,
                    pid: 4242,
                    boot: boot.to_string(),
                    term: 7,
                }),
                claim_set: Some(set.clone()),
            }],
            authority: Some(AuthorityLeaseEvidence {
                owner_id: "owner".into(),
                endpoint: "127.0.0.1:7000".into(),
                owner_claim_id: String::new(),
                term: 7,
                live: true,
                member_epoch: 1,
            }),
            registrant: Some(RegistrantEvidence {
                pr_capable: true,
                wero: true,
                reservation_held: true,
                registered: true,
                key: AUTHORITY_KEY,
                namespaces: 1,
            }),
        }
    };
    assert!(
        cowriter::co_located_with_authority(&req(&our_boot)),
        "same boot id = same host = the adoption shape"
    );
    assert!(
        !cowriter::co_located_with_authority(&req("ffffffff-ffff-ffff-ffff-ffffffffffff")),
        "a foreign boot is a remote authority — the register path stays"
    );
}

/// Rung-9 **finding #3** (live): rung 4's plane-linkage check compared the
/// membership rendezvous **incarnation uuid** against the claim set's
/// **durable node ids** — the two identity planes rung-8 finding #3
/// deliberately split — so every healthy fleet's first co-writer admission
/// refused ("the durable claim set does not name the membership
/// authority"). The rendezvous record now carries the owner's durable
/// claim identity, and rung 4 links through it; a LEGACY record (empty
/// claim id) keeps the uuid match, so the pre-split shape still links the
/// pre-split way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_4_links_the_plane_through_the_owners_durable_claim_identity() {
    let mk = |owner_claim_id: &str| -> AdmissionRequest {
        let node_claim_id = "node_00000000aaaaaaaa.m00000099"; // the AUTHORITY's durable id
        let mut set = ClaimSet::empty(7);
        set.durable = true;
        set.members.push(ClaimSetMember {
            identity: MemberIdentity {
                id: node_claim_id.into(),
                role: MemberRole::Writer,
                pid: 0,
                boot: String::new(),
                endpoint: None,
                pr_key: AUTHORITY_KEY,
            },
            ts: 0,
        });
        set.members.push(ClaimSetMember {
            identity: MemberIdentity {
                id: "node_00000000bbbbbbbb.m00000001".into(),
                role: MemberRole::Writer,
                pid: 0,
                boot: String::new(),
                endpoint: None,
                pr_key: 0xB0B0,
            },
            ts: 0,
        });
        AdmissionRequest {
            multi_writer: true,
            role_co_writer: true,
            read_only: false,
            node_id: "node_00000000bbbbbbbb.m00000001".into(),
            custody_endpoint: Some("127.0.0.1:7100".into()),
            volumes: vec![VolumeAdmissionEvidence {
                path: PathBuf::from("/dev/fake-meta"),
                features_incompat: cowriter::REQUIRED_INCOMPAT,
                claim: Some(WriterClaim {
                    id: "authority-claim".into(),
                    ts: 0,
                    pid: 4242,
                    boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".into(),
                    term: 7,
                }),
                claim_set: Some(set.clone()),
            }],
            authority: Some(AuthorityLeaseEvidence {
                // The RAM plane's identity: the INCARNATION uuid — never in
                // the claim set (the live shape that refused).
                owner_id: "74fd5dd0-3842-4d3a-ba9c-e88ae2e8c659".into(),
                endpoint: "127.0.0.1:7000".into(),
                owner_claim_id: owner_claim_id.into(),
                term: 7,
                live: true,
                member_epoch: 1,
            }),
            registrant: Some(RegistrantEvidence {
                pr_capable: true,
                wero: true,
                reservation_held: true,
                registered: true,
                key: 0xB0B0,
                namespaces: 1,
            }),
        }
    };

    // The healthy fleet's shape: the rendezvous carries the durable claim
    // identity — the ladder ADMITS.
    cowriter::classify_admission(&mk("node_00000000aaaaaaaa.m00000099"))
        .expect("the split identity planes link through owner_claim_id");

    // A plane whose claim identity names an id the set does NOT enroll —
    // and whose uuid isn't enrolled either — still refuses at rung 4 (an
    // unrelated plane must never admit a node into a set it has no
    // authority over).
    let err = cowriter::classify_admission(&mk("node_00000000cccccccc"))
        .expect_err("an unrelated plane still refuses")
        .to_string();
    assert!(err.contains("rung 4"), "the refusal names its rung: {err}");
}

// ===========================================================================
// Finding #2 — the authority's hold re-verification
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

    let hold = data_custody::acquire_wero(std::slice::from_ref(&p)).expect("the authority arms");
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

    let hold = data_custody::acquire_wero(std::slice::from_ref(&p)).expect("the authority arms");

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
