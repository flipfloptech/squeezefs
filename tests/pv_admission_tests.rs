//! **Per-volume claim admission — the seven-rung ladder**
//! (`docs/design-per-volume-claim-admission.md` §5.1/§5.3, rulings
//! **D18/D19/D20**, KD-PV-1…16; the decision `src/partial_authority.rs`).
//!
//! # What this file pins
//!
//! The ladder is the co-writer ladder (`src/cowriter.rs`) **extended, never
//! forked**: the same declaration-then-format-then-enrollment-then-authority
//! -then-device order, with two new rungs and one structural change —
//!
//! | Rung | Co-writer today | Per-volume admission |
//! |---|---|---|
//! | 1 | role `co-writer` + a declared authority | roles **`set-authority`** / **`partial-authority`**; a set authority declares no endpoint (D20 — it IS the endpoint) |
//! | 2 | one UNIFORM verdict over every volume | a **per-volume verdict vector** (`Own` / `Peer`) |
//! | 3 | this node is a `Writer` member | plus: an own volume names this node `owner` (or lists it in `successors`), a peer volume's `owner` is a `Writer` member, a PARTIAL assignment map refuses, and the declared role is verified against the slot-0 volume's assignment |
//! | 4 | `max` over every volume's claim term | the **slot-0 volume's** claim term (D20); a peer volume's own term never enters the comparison |
//! | 5 | the standing WERO hold names this node | unchanged in substance (the shared helper) |
//! | 6 | — | **assignment ∧ evidence, per volume** (KD-PV-3): never adopt on silence |
//! | 7 | — | **the freeze precondition** (§5.9.2): every peer volume's projected `claim_set` shows its own assignment |
//!
//! and the three pins the PR row names by hand:
//!
//! * **the ladder is the only `SetAdmission` constructor** — the decision
//!   is unforgeable, so no caller can open a peer-owned volume without one;
//! * **`covers` refuses a cross-set admission** (the
//!   `KvMetaBackend::open_co_writer` precedent);
//! * **`a_set_admission_resolves_modes_by_durable_volume_id_not_by_position`**
//!   — exercised with a URI order that differs from the canonical set
//!   order. An index-keyed vector plus a permuted list would take the FULL
//!   D0 ladder (flock + PR WEX + claim) on a volume a peer owns: the worst
//!   outcome in the program, reached by an off-by-permutation rather than a
//!   race (§5.3, review Issue 8).
//!
//! # What one process CANNOT pin (stated, not hidden)
//!
//! Every rung here decides over **evidence**: the ladder is pure (it opens
//! nothing, reads no environment and mutates nothing), exactly as
//! `cowriter::classify_admission` is. What produces that evidence — the
//! probe opens, the D0 classification, the checkpoint-consistent projection
//! of a peer volume, the durable-identity resolution of a live claim holder
//! — is the mount path's, and lands in PR 4. Nothing in this file mounts.

use squeezefs::cowriter::{AuthorityLeaseEvidence, MwRole, RegistrantEvidence};
use squeezefs::membership::{ClaimSet, ClaimSetMember, MemberIdentity, MemberRole};
use squeezefs::meta_backend::kv::backend::WriterClaim;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::partial_authority::{
    self as pv, ClaimStanding, PvVolumeEvidence, SetAdmissionRequest, VolumeMode,
};
use std::path::PathBuf;

/// This node's durable enrollment identity (KD-MW-2).
const NODE: &str = "node_00000000deadbeef.m00000001";
/// The peer that owns the slot-0 volume in the partial-authority fixtures.
const PEER: &str = "node_00000000feedface.m00000001";
/// A third fleet member, named in `successors` where the opt-in is exercised.
const THIRD: &str = "node_0000000012345678.m00000001";

/// The durable volume ids (KD-5) — never a path, an ordinal or a position.
const VOL_SLOT0: &str = "vol-0a1b2c3d4e5f6071";
const VOL_A: &str = "vol-1122334455667788";
const VOL_B: &str = "vol-99aabbccddeeff00";

fn member(id: &str, role: MemberRole) -> ClaimSetMember {
    ClaimSetMember {
        identity: MemberIdentity {
            id: id.to_string(),
            role,
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key: 0,
        },
        ts: 1_700_000_000,
    }
}

/// A durable, fully enrolled claim set assigning `owner` (KD-PV-4 enrolls
/// every fleet member as a pid-less `Writer` on EVERY volume).
fn assigned_set(owner: &str, successors: &[&str]) -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.owner = Some(owner.to_string());
    set.successors = successors.iter().map(|s| s.to_string()).collect();
    set.members = vec![
        member(NODE, MemberRole::Writer),
        member(PEER, MemberRole::Writer),
        member(THIRD, MemberRole::Writer),
    ];
    set
}

fn claim(term: u64) -> WriterClaim {
    WriterClaim {
        // A per-mount uuid — never the durable member id (the rung-9
        // finding #3 identity split the ladder must not re-make).
        id: "5f1d0e2a-0000-4000-8000-000000000001".to_string(),
        ts: 1_700_000_000,
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term,
    }
}

/// A volume this node is assigned to OWN: nothing claims it, so the D0
/// ladder classifies `Reclaimable` and the full ladder runs on it.
fn own_volume(vol_id: &str, hosts_slot_0: bool) -> PvVolumeEvidence {
    PvVolumeEvidence {
        path: PathBuf::from(format!("/dev/fake/{vol_id}")),
        vol_id: vol_id.to_string(),
        hosts_slot_0,
        features_incompat: squeezefs::cowriter::REQUIRED_INCOMPAT,
        pr_capable: true,
        claim: None,
        holder_member_id: None,
        standing: ClaimStanding::Reclaimable,
        claim_set: Some(assigned_set(NODE, &[])),
        projected_claim_set: Some(assigned_set(NODE, &[])),
        owner_endpoint: None,
    }
}

/// A volume a PEER owns and is currently appending to.
fn peer_volume(vol_id: &str, hosts_slot_0: bool) -> PvVolumeEvidence {
    PvVolumeEvidence {
        path: PathBuf::from(format!("/dev/fake/{vol_id}")),
        vol_id: vol_id.to_string(),
        hosts_slot_0,
        features_incompat: squeezefs::cowriter::REQUIRED_INCOMPAT,
        pr_capable: true,
        claim: Some(claim(7)),
        holder_member_id: Some(PEER.to_string()),
        standing: ClaimStanding::Fresh,
        claim_set: Some(assigned_set(PEER, &[])),
        projected_claim_set: Some(assigned_set(PEER, &[])),
        owner_endpoint: Some("127.0.0.1:7100".to_string()),
    }
}

/// A volume assigned to `owner` that **nothing claims** — the cold-fleet
/// shape: that owner has not mounted yet (or is down), so the D0 ladder
/// classifies its volume `Reclaimable` and no holder resolves.
fn cold_peer_volume(vol_id: &str, owner: &str, hosts_slot_0: bool) -> PvVolumeEvidence {
    PvVolumeEvidence {
        path: PathBuf::from(format!("/dev/fake/{vol_id}")),
        vol_id: vol_id.to_string(),
        hosts_slot_0,
        features_incompat: squeezefs::cowriter::REQUIRED_INCOMPAT,
        pr_capable: true,
        claim: None,
        holder_member_id: None,
        standing: ClaimStanding::Reclaimable,
        claim_set: Some(assigned_set(owner, &[])),
        projected_claim_set: Some(assigned_set(owner, &[])),
        owner_endpoint: None,
    }
}

fn authority_evidence() -> AuthorityLeaseEvidence {
    AuthorityLeaseEvidence {
        owner_id: "membership-owner-incarnation".to_string(),
        endpoint: "127.0.0.1:7000".to_string(),
        owner_claim_id: PEER.to_string(),
        term: 7,
        live: true,
        member_epoch: 3,
    }
}

fn registrant_evidence() -> RegistrantEvidence {
    RegistrantEvidence {
        pr_capable: true,
        wero: true,
        reservation_held: true,
        registered: true,
        key: 0xB0B0,
        namespaces: 1,
    }
}

/// A PARTIAL AUTHORITY: the peer owns the slot-0 volume (so that peer is
/// the SET AUTHORITY, D20), this node owns `VOL_A`.
fn partial_request() -> SetAdmissionRequest {
    SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::PartialAuthority,
        read_only: false,
        node_id: NODE.to_string(),
        set_authority_endpoint: Some("127.0.0.1:7100".to_string()),
        volumes: vec![peer_volume(VOL_SLOT0, true), own_volume(VOL_A, false)],
        authority: Some(authority_evidence()),
        registrant: Some(registrant_evidence()),
    }
}

/// A SET AUTHORITY: this node owns the slot-0 volume; a peer owns `VOL_A`.
/// It declares no endpoint (D20) and joins no membership lease — it IS the
/// membership owner.
fn set_authority_request() -> SetAdmissionRequest {
    SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::SetAuthority,
        read_only: false,
        node_id: NODE.to_string(),
        set_authority_endpoint: None,
        volumes: vec![own_volume(VOL_SLOT0, true), peer_volume(VOL_A, false)],
        authority: None,
        registrant: Some(registrant_evidence()),
    }
}

/// The index of the volume named `vol_id` in a request (fixtures only —
/// the LADDER never resolves by position, which is what
/// `a_set_admission_resolves_modes_by_durable_volume_id_not_by_position`
/// exists to prove).
fn at<'a>(req: &'a mut SetAdmissionRequest, vol_id: &str) -> &'a mut PvVolumeEvidence {
    req.volumes
        .iter_mut()
        .find(|v| v.vol_id == vol_id)
        .expect("fixture names the volume")
}

// ===========================================================================
// 1. The ladder admits — both postures
// ===========================================================================

/// Contract: all seven rungs satisfied ADMIT a partial authority, and the
/// admission carries the per-volume verdict vector keyed by durable id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_seven_rung_ladder_admits_a_partial_authority() {
    let admission = pv::classify_set_admission(&partial_request()).expect("the full ladder admits");

    assert_eq!(admission.node_id(), NODE);
    assert_eq!(admission.role(), MwRole::PartialAuthority);
    assert!(
        !admission.is_set_authority(),
        "the peer owns the slot-0 volume, so this mount is not the set authority"
    );
    assert!(admission.owns_any(), "this mount owns VOL_A");
    assert_eq!(admission.mode_for(VOL_A), Some(&VolumeMode::Own));
    assert_eq!(
        admission.mode_for(VOL_SLOT0),
        Some(&VolumeMode::Peer {
            owner_id: PEER.to_string(),
            owner_endpoint: "127.0.0.1:7100".to_string(),
        })
    );
    assert_eq!(
        admission.mode_for("vol-not-in-this-set"),
        None,
        "a volume the decision does not name resolves to None (refuse), never to a default"
    );
    assert_eq!(admission.pr_key(), 0xB0B0);
}

/// Contract: the SET AUTHORITY posture admits with the mirror-image
/// verdict vector, declares no endpoint and joins no lease (D20).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_seven_rung_ladder_admits_a_set_authority() {
    let admission =
        pv::classify_set_admission(&set_authority_request()).expect("the full ladder admits");

    assert_eq!(admission.role(), MwRole::SetAuthority);
    assert!(
        admission.is_set_authority(),
        "this mount owns the volume hosting slot 0"
    );
    assert_eq!(admission.mode_for(VOL_SLOT0), Some(&VolumeMode::Own));
    assert!(matches!(
        admission.mode_for(VOL_A),
        Some(VolumeMode::Peer { owner_id, .. }) if owner_id == PEER
    ));
}

/// Contract: **the verdict vector is per volume, not uniform** — the
/// structural change rung 2 makes. A three-volume set with two owners
/// admits, which the co-writer ladder's uniform verdict could not express.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_verdict_vector_is_per_volume_and_never_uniform() {
    let mut req = partial_request();
    req.volumes.push(own_volume(VOL_B, false));

    let admission = pv::classify_set_admission(&req).expect("a mixed set admits");
    assert_eq!(admission.mode_for(VOL_A), Some(&VolumeMode::Own));
    assert_eq!(admission.mode_for(VOL_B), Some(&VolumeMode::Own));
    assert!(matches!(
        admission.mode_for(VOL_SLOT0),
        Some(VolumeMode::Peer { .. })
    ));
}

// ===========================================================================
// 2. Seven refusals, one per rung, each naming its rung
// ===========================================================================

/// Rung 1 — the posture is DECLARED, never inferred: the opt-in, the role,
/// the reader category error, and the set authority's endpoint (D20).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_1_refuses_an_undeclared_per_volume_posture() {
    let mut req = partial_request();
    req.multi_writer = false;
    let err = pv::classify_set_admission(&req)
        .expect_err("no opt-in, no per-volume posture")
        .to_string();
    assert!(err.contains("rung 1"), "the refusal names its rung: {err}");
    assert!(
        err.contains("SQUEEZEFS_MULTI_WRITER"),
        "the refusal names the opt-in: {err}"
    );

    let mut req = partial_request();
    req.role = MwRole::CoWriter;
    let err = pv::classify_set_admission(&req)
        .expect_err("a co-writer role is not a per-volume posture")
        .to_string();
    assert!(
        err.contains("SQUEEZEFS_MW_ROLE"),
        "the refusal names the role knob: {err}"
    );

    let mut req = partial_request();
    req.read_only = true;
    let err = pv::classify_set_admission(&req)
        .expect_err("a reader appends to nothing")
        .to_string();
    assert!(
        err.to_lowercase().contains("read-only"),
        "the refusal names the reader posture: {err}"
    );

    let mut req = partial_request();
    req.set_authority_endpoint = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("a partial authority ships to the set authority and must know where")
        .to_string();
    assert!(
        err.contains("SQUEEZEFS_MW_AUTHORITY"),
        "the refusal names the dial target: {err}"
    );

    // ...and the D20 half: a SET authority IS the endpoint, so the knob is
    // NOT READ on that posture (§6.2). A fleet that exports one authority
    // endpoint everywhere and varies only the role must still admit.
    let mut req = set_authority_request();
    req.set_authority_endpoint = Some("127.0.0.1:7100".to_string());
    let admission = pv::classify_set_admission(&req)
        .expect("a set authority ignores the endpoint rather than refusing it");
    assert!(admission.is_set_authority());
}

/// Rung 2 — the format expresses a claim SET on every volume, durably. A
/// half-engaged set is not a claim set, and a PROJECTION cannot carry an
/// assignment at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_2_refuses_a_half_engaged_or_projected_claim_set() {
    let mut req = partial_request();
    at(&mut req, VOL_A).features_incompat =
        squeezefs::cowriter::REQUIRED_INCOMPAT & !sb::FEATURE_INCOMPAT_KV_CLAIM_SET;
    let err = pv::classify_set_admission(&req)
        .expect_err("bit 14 must be engaged on EVERY volume")
        .to_string();
    assert!(err.contains("rung 2"), "the refusal names its rung: {err}");
    assert!(
        err.contains("14") && err.contains(VOL_A),
        "the refusal names the bit and the volume: {err}"
    );

    let mut req = partial_request();
    if let Some(set) = at(&mut req, VOL_SLOT0).claim_set.as_mut() {
        set.durable = false;
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("a projection expresses exclusion, not a set")
        .to_string();
    assert!(
        err.contains("rung 2") && err.contains(VOL_SLOT0),
        "the refusal names the rung and the volume: {err}"
    );

    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).claim_set = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("no claim set at all is no assignment at all")
        .to_string();
    assert!(err.contains("rung 2"), "the refusal names its rung: {err}");
}

/// Rung 2, the ADMIT direction that separates this ladder from the
/// co-writer's: an own-mode volume that **nothing claims** is admissible
/// (the D0 ladder is about to claim it), where the co-writer's rung 2
/// refuses every volume without a live `writer_claim`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_2_admits_an_unclaimed_volume_this_node_is_assigned_to_own() {
    let mut req = partial_request();
    assert!(
        at(&mut req, VOL_A).claim.is_none(),
        "the fixture's own volume carries no claim"
    );
    let admission = pv::classify_set_admission(&req).expect("an unclaimed OWN volume admits");
    assert_eq!(admission.mode_for(VOL_A), Some(&VolumeMode::Own));
}

/// Rung 3 — durable enrollment and a COMPLETE assignment map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_3_refuses_an_unenrolled_node_and_an_incomplete_assignment_map() {
    // (a) the set does not name this node at all.
    let mut req = partial_request();
    for vol in &mut req.volumes {
        if let Some(set) = vol.claim_set.as_mut() {
            set.members.retain(|m| m.identity.id != NODE);
        }
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("admission is by durable ENROLLMENT")
        .to_string();
    assert!(err.contains("rung 3"), "the refusal names its rung: {err}");
    assert!(
        err.contains(NODE),
        "the refusal prints the id the operator must enroll: {err}"
    );

    // (b) it names this node as a READER.
    let mut req = partial_request();
    for vol in &mut req.volumes {
        if let Some(set) = vol.claim_set.as_mut() {
            set.members.retain(|m| m.identity.id != NODE);
            set.members.push(member(NODE, MemberRole::Reader));
        }
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("a reader member appends to nothing")
        .to_string();
    assert!(
        err.contains("rung 3") && err.to_lowercase().contains("reader"),
        "the refusal names the rung and the role: {err}"
    );

    // (c) a peer volume's assigned owner is not a Writer member — an
    //     assignment naming a node the set does not enroll.
    let mut req = partial_request();
    if let Some(set) = at(&mut req, VOL_SLOT0).claim_set.as_mut() {
        set.members.retain(|m| m.identity.id != PEER);
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("the assigned owner must be an enrolled writer")
        .to_string();
    assert!(
        err.contains("rung 3") && err.contains(PEER),
        "the refusal names the rung and the owner: {err}"
    );

    // (d) the PARTIAL map: one volume assigned, its sibling not.
    let mut req = partial_request();
    if let Some(set) = at(&mut req, VOL_A).claim_set.as_mut() {
        set.owner = None;
    }
    if let Some(set) = at(&mut req, VOL_A).projected_claim_set.as_mut() {
        set.owner = None;
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("a partially assigned set is not an assignment")
        .to_string();
    assert!(
        err.contains("rung 3") && err.contains(VOL_A),
        "the refusal names the rung and the unassigned volume: {err}"
    );

    // (e) NOTHING is assigned: this is a plain single-authority set, and
    //     the per-volume posture has no durable basis at all.
    let mut req = partial_request();
    for vol in &mut req.volumes {
        if let Some(set) = vol.claim_set.as_mut() {
            set.owner = None;
        }
        if let Some(set) = vol.projected_claim_set.as_mut() {
            set.owner = None;
        }
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("an unassigned set admits no per-volume posture")
        .to_string();
    assert!(
        err.contains("rung 3") && err.contains("set-owners"),
        "the refusal names its rung and the remedy verb: {err}"
    );
}

/// Rung 3 — **the declared role is verified against the slot-0 volume's
/// assignment** (D20/KD-PV-6: the role value is a declaration, the ladder
/// decides). Both directions, plus the owns-nothing shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_3_verifies_the_declared_role_against_the_slot_0_assignment() {
    // A partial authority that is in fact assigned the slot-0 volume: it
    // IS the set authority and must say so.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).claim_set = Some(assigned_set(NODE, &[]));
    at(&mut req, VOL_SLOT0).projected_claim_set = Some(assigned_set(NODE, &[]));
    at(&mut req, VOL_SLOT0).standing = ClaimStanding::Reclaimable;
    at(&mut req, VOL_SLOT0).claim = None;
    at(&mut req, VOL_SLOT0).holder_member_id = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("the slot-0 owner is the SET authority")
        .to_string();
    assert!(
        err.contains("rung 3") && err.contains("set-authority"),
        "the refusal names the rung and the posture it should have declared: {err}"
    );

    // ...and the mirror: a set authority that does NOT own the slot-0
    // volume is a partial authority calling itself the set authority.
    let mut req = set_authority_request();
    at(&mut req, VOL_SLOT0).claim_set = Some(assigned_set(PEER, &[]));
    at(&mut req, VOL_SLOT0).projected_claim_set = Some(assigned_set(PEER, &[]));
    at(&mut req, VOL_SLOT0).standing = ClaimStanding::Fresh;
    at(&mut req, VOL_SLOT0).claim = Some(claim(7));
    at(&mut req, VOL_SLOT0).holder_member_id = Some(PEER.to_string());
    at(&mut req, VOL_SLOT0).owner_endpoint = Some("127.0.0.1:7100".to_string());
    let err = pv::classify_set_admission(&req)
        .expect_err("slot 0 decides who the set authority is")
        .to_string();
    assert!(
        err.contains("rung 3") && err.contains("partial-authority"),
        "the refusal names the rung and the posture it should have declared: {err}"
    );

    // A partial authority that owns NOTHING is a co-writer, and refusing
    // says so rather than mounting a metadata plane with no local half.
    let mut req = partial_request();
    at(&mut req, VOL_A).claim_set = Some(assigned_set(PEER, &[]));
    at(&mut req, VOL_A).projected_claim_set = Some(assigned_set(PEER, &[]));
    at(&mut req, VOL_A).standing = ClaimStanding::Fresh;
    at(&mut req, VOL_A).claim = Some(claim(7));
    at(&mut req, VOL_A).holder_member_id = Some(PEER.to_string());
    at(&mut req, VOL_A).owner_endpoint = Some("127.0.0.1:7100".to_string());
    let err = pv::classify_set_admission(&req)
        .expect_err("a partial authority owns at least one volume")
        .to_string();
    assert!(
        err.contains("rung 3") && err.contains("co-writer"),
        "the refusal names the rung and the posture that fits: {err}"
    );

    // And the evidence the role check consumes: ino 1 pins to slot 0 and
    // slot 0 pins to one volume (KD-PV-6), so a set that names none — or
    // two — leaves the SET AUTHORITY underivable. Refuse rather than pick.
    for hosts in [false, true] {
        let mut req = partial_request();
        at(&mut req, VOL_A).hosts_slot_0 = hosts;
        at(&mut req, VOL_SLOT0).hosts_slot_0 = hosts;
        let err = pv::classify_set_admission(&req)
            .expect_err("exactly one volume hosts slot 0")
            .to_string();
        assert!(
            err.contains("rung 3") && err.contains("slot 0"),
            "the refusal names the rung and what is underivable: {err}"
        );
    }
}

/// Rung 4 — a LIVE authority for a partial authority (custody source and,
/// above all, evictor).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_4_refuses_without_a_live_membership_lease() {
    let mut req = partial_request();
    req.authority = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("a partial authority that cannot be SEEN cannot be EVICTED")
        .to_string();
    assert!(err.contains("rung 4"), "the refusal names its rung: {err}");

    let mut req = partial_request();
    if let Some(auth) = req.authority.as_mut() {
        auth.live = false;
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("a rendezvous record only says an owner once armed")
        .to_string();
    assert!(err.contains("rung 4"), "the refusal names its rung: {err}");
}

/// Rung 4 — **the per-volume term comparison** (D20). The membership
/// authority's term is compared against the SLOT-0 volume's claim term
/// only; a peer-owned volume's own term diverges per owner by design and
/// is learned at runtime through `era_relearns`, never at admission.
///
/// Both directions in one case, because the co-writer ladder's `max` over
/// every volume is exactly what this rung replaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_4_compares_the_membership_term_against_the_slot_0_volume_only() {
    // A peer-owned NON-slot-0 volume in a LATER era than the membership
    // plane: terms diverge per owner, so this must ADMIT.
    let mut req = partial_request();
    req.volumes.push(PvVolumeEvidence {
        claim: Some(claim(11)),
        ..peer_volume(VOL_B, false)
    });
    pv::classify_set_admission(&req)
        .expect("a peer volume's own era never gates the membership plane");

    // The SLOT-0 volume in a later era than the plane we joined: the plane
    // belongs to an older era than the D0 holder we can see. Refuse.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).claim = Some(claim(11));
    let err = pv::classify_set_admission(&req)
        .expect_err("the plane and the set authority must be the same era")
        .to_string();
    assert!(
        err.contains("rung 4") && err.contains("11"),
        "the refusal names its rung and the era it saw: {err}"
    );
}

/// Rung 5 — the DEVICE names this node a registrant of the standing WERO
/// hold. Unchanged in substance from the co-writer ladder (the shared
/// helper), so this pins the wiring rather than re-pinning the texts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_5_refuses_without_a_device_registrant_under_a_standing_wero() {
    let mut req = partial_request();
    req.registrant = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("no reservation evidence, no fence")
        .to_string();
    assert!(err.contains("rung 5"), "the refusal names its rung: {err}");

    let mut req = partial_request();
    if let Some(ev) = req.registrant.as_mut() {
        ev.pr_capable = false;
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("a non-PR substrate can detect but never reject")
        .to_string();
    assert!(
        err.contains("rung 5") && err.contains("RESCAP"),
        "the refusal names the rung and the device answer: {err}"
    );

    let mut req = partial_request();
    if let Some(ev) = req.registrant.as_mut() {
        ev.registered = false;
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("no registrant key, no drain proof")
        .to_string();
    assert!(err.contains("rung 5"), "the refusal names its rung: {err}");
}

/// Rung 6 — **assignment ∧ evidence on an OWN volume** (KD-PV-3): the D0
/// ladder must be able to grant the claim. `Reclaimable` admits;
/// `StaleForeign` admits only where the PR preempt would grant it;
/// `FreshForeign` never does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_6_admits_an_own_volume_the_d0_ladder_would_grant_and_refuses_a_live_one() {
    // Stale, PR-capable: the D0 ladder's preempt grants it (KD-PV-12 (ii)).
    let mut req = partial_request();
    at(&mut req, VOL_A).standing = ClaimStanding::Stale;
    at(&mut req, VOL_A).claim = Some(claim(6));
    pv::classify_set_admission(&req).expect("a PR preempt grants a stale claim");

    // Stale on a NON-PR substrate: the D0 ladder refuses (operator
    // attestation only), so admission must refuse too rather than promise
    // an open that will fail.
    let mut req = partial_request();
    at(&mut req, VOL_A).standing = ClaimStanding::Stale;
    at(&mut req, VOL_A).claim = Some(claim(6));
    at(&mut req, VOL_A).pr_capable = false;
    let err = pv::classify_set_admission(&req)
        .expect_err("a non-PR stale claim is an attestation, not an admission")
        .to_string();
    assert!(
        err.contains("rung 6") && err.contains("claim clear"),
        "the refusal names its rung and the attested remedy: {err}"
    );

    // FRESH on a volume we are assigned to own: somebody else is
    // appending to it. Fail closed (§5.10) — never race the D0 ladder.
    let mut req = partial_request();
    at(&mut req, VOL_A).standing = ClaimStanding::Fresh;
    at(&mut req, VOL_A).claim = Some(claim(7));
    at(&mut req, VOL_A).holder_member_id = Some(PEER.to_string());
    let err = pv::classify_set_admission(&req)
        .expect_err("a live foreign claim on our own volume is a disagreement")
        .to_string();
    assert!(
        err.contains("rung 6") && err.contains(VOL_A),
        "the refusal names its rung and the volume: {err}"
    );
}

/// Rung 6 — **a peer-assigned volume NOTHING claims is ADMITTED, degraded**
/// (the cold-start correction to §5.1.1's `Peer` column).
///
/// "Never adopt on silence" (KD-PV-3) forbids TAKING a volume this node is
/// not assigned. It never required refusing to mount because a peer has
/// not started yet — and reading it that way made an assigned set
/// **unmountable by construction**: at a cold fleet start NO volume carries
/// a claim, so the set authority could not mount before its peers and the
/// peers could not mount before it (the arm was symmetric, so the deadlock
/// was total). The verdict here is the one `docs/operations.md`'s own
/// bring-up recipe already documents: admit, never adopt, and let the ship
/// path refuse loud until the owner arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_6_admits_a_peer_volume_with_no_live_appender_and_never_adopts_it() {
    // The COLD SET AUTHORITY — the first node of the fleet, mounting in
    // exactly the order operations.md prescribes. Its peer's volume is
    // unclaimed because that peer has not mounted yet.
    let mut req = set_authority_request();
    *at(&mut req, VOL_A) = cold_peer_volume(VOL_A, PEER, false);
    let admission = pv::classify_set_admission(&req).expect(
        "a cold set authority must admit: refusing here makes the fleet's own bring-up order \
         unsatisfiable",
    );
    assert!(
        admission.is_set_authority(),
        "the slot-0 volume is still this mount's"
    );
    assert!(
        matches!(
            admission.mode_for(VOL_A),
            Some(VolumeMode::Peer { owner_id, owner_endpoint })
                if owner_id == PEER && owner_endpoint.is_empty()
        ),
        "the unclaimed volume stays the PEER's, with no resolved endpoint — the not-yet-up \
         state, never an adoption: {:?}",
        admission.mode_for(VOL_A)
    );
    assert_ne!(
        admission.mode_for(VOL_A),
        Some(&VolumeMode::Own),
        "admitting a cold peer volume must never make this mount its appender"
    );

    // A cold PARTIAL AUTHORITY: the set authority is up (rung 4 demands a
    // live lease from it), and a THIRD owner's volume is unclaimed because
    // that node has not mounted yet. Nothing orders the partials among
    // themselves, so refusing here deadlocked every fleet of K ≥ 3.
    let mut req = partial_request();
    req.volumes.push(cold_peer_volume(VOL_B, THIRD, false));
    let admission = pv::classify_set_admission(&req).expect("a cold partial authority admits");
    assert_eq!(admission.mode_for(VOL_A), Some(&VolumeMode::Own));
    assert!(matches!(
        admission.mode_for(VOL_B),
        Some(VolumeMode::Peer { owner_id, .. }) if owner_id == THIRD
    ));

    // A TTL-stale claim left by the volume's OWN assigned owner is the
    // same degraded state read on a cross-host substrate (where no
    // dead-pid proof exists): the owner is gone, nothing appends there,
    // and this mount neither preempts the claim nor takes the volume.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).standing = ClaimStanding::Stale;
    let admission =
        pv::classify_set_admission(&req).expect("a dead owner's stale claim admits, degraded");
    assert!(matches!(
        admission.mode_for(VOL_SLOT0),
        Some(VolumeMode::Peer { owner_id, .. }) if owner_id == PEER
    ));
}

/// Rung 6 — **assignment ∧ evidence on a PEER volume** (§5.1.1's `Peer`
/// column): where a claim EXISTS it must agree with the assignment. A
/// holder the assignment set does not name, and a holder that cannot be
/// resolved at all, both refuse — fresh or TTL-stale — because each is a
/// statement that SOMETHING appended to that volume which the record
/// cannot account for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_6_refuses_a_peer_volume_whose_claim_disagrees_with_the_assignment() {
    // A live holder the assignment does not name: assignment ∧ evidence
    // DISAGREE, which is two appenders or an orphaned volume (§5.10).
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).holder_member_id = Some(THIRD.to_string());
    let err = pv::classify_set_admission(&req)
        .expect_err("the record's owner and the claim's holder must agree")
        .to_string();
    assert!(
        err.contains("rung 6") && err.contains(THIRD) && err.contains(PEER),
        "the refusal prints BOTH identities: {err}"
    );

    // An UNRESOLVED holder is silence, and silence is never adoption: the
    // live claim's `id` is a per-mount uuid, so a holder that cannot be
    // resolved to a durable member id proves nothing.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).holder_member_id = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("an unresolvable holder is not evidence")
        .to_string();
    assert!(err.contains("rung 6"), "the refusal names its rung: {err}");

    // The same over a TTL-STALE claim: age does not make an unattributable
    // claim readable. A volume with NO claim admits (above); a volume with
    // a claim nobody can name does not.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).standing = ClaimStanding::Stale;
    at(&mut req, VOL_SLOT0).holder_member_id = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("a stale claim this mount cannot attribute is not silence — it is evidence")
        .to_string();
    assert!(
        err.contains("rung 6") && err.contains(VOL_SLOT0),
        "the refusal names its rung and the volume: {err}"
    );

    // And a stale claim held by a STRANGER: the disagreement arm, aged.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).standing = ClaimStanding::Stale;
    at(&mut req, VOL_SLOT0).holder_member_id = Some(THIRD.to_string());
    let err = pv::classify_set_admission(&req)
        .expect_err("an aged claim from a node the record does not entitle still disagrees")
        .to_string();
    assert!(
        err.contains("rung 6") && err.contains(THIRD) && err.contains(PEER),
        "the refusal prints BOTH identities: {err}"
    );
}

/// Rung 6 + KD-PV-12 — the successor opt-in. A declared successor adopts
/// **only** what the D0 ladder would grant (`Reclaimable`, or stale on a
/// PR substrate), and NEVER while the assigned owner is alive: ownership
/// never moves because a node is slow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_successor_adopts_only_what_the_d0_ladder_would_grant() {
    // The owner is dead (nothing claims the volume) and this node is its
    // declared successor: the volume becomes OWN.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).claim_set = Some(assigned_set(PEER, &[NODE]));
    at(&mut req, VOL_SLOT0).projected_claim_set = Some(assigned_set(PEER, &[NODE]));
    at(&mut req, VOL_SLOT0).standing = ClaimStanding::Reclaimable;
    at(&mut req, VOL_SLOT0).claim = None;
    at(&mut req, VOL_SLOT0).holder_member_id = None;
    req.role = MwRole::SetAuthority;
    req.set_authority_endpoint = None;
    req.authority = None;
    let admission = pv::classify_set_admission(&req).expect("a declared successor adopts");
    assert_eq!(
        admission.mode_for(VOL_SLOT0),
        Some(&VolumeMode::Own),
        "the adopted volume is OWN, and owning slot 0 makes this mount the set authority"
    );
    assert!(admission.is_set_authority());

    // The same declaration while the owner is ALIVE: still a peer volume.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).claim_set = Some(assigned_set(PEER, &[NODE]));
    at(&mut req, VOL_SLOT0).projected_claim_set = Some(assigned_set(PEER, &[NODE]));
    let admission = pv::classify_set_admission(&req).expect("a live owner keeps its volume");
    assert!(matches!(
        admission.mode_for(VOL_SLOT0),
        Some(VolumeMode::Peer { owner_id, .. }) if owner_id == PEER
    ));

    // A successor's own live claim on a peer-assigned volume is the
    // ASSIGNMENT SET reading (§5.10, Issue 24): the durable `owner` still
    // names the dead predecessor after a legitimate adoption, so a
    // singleton comparison would refuse the volume the opt-in recovered.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).claim_set = Some(assigned_set(PEER, &[THIRD]));
    at(&mut req, VOL_SLOT0).projected_claim_set = Some(assigned_set(PEER, &[THIRD]));
    at(&mut req, VOL_SLOT0).holder_member_id = Some(THIRD.to_string());
    let admission =
        pv::classify_set_admission(&req).expect("an adopted volume's successor is its owner");
    assert!(matches!(
        admission.mode_for(VOL_SLOT0),
        Some(VolumeMode::Peer { owner_id, .. }) if owner_id == THIRD
    ));
}

/// Rung 7 — **the freeze precondition** (§5.9.2, KD-PV-8): every
/// peer-owned volume's PROJECTED claim set must show its own assignment. A
/// monotone projection that shows the assignment record shows every commit
/// that preceded it on that volume — including every cross-owner dentry
/// that will ever exist there. Both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_7_refuses_a_peer_volume_whose_projection_predates_its_assignment() {
    // No projection at all: nothing certifies the freeze.
    let mut req = partial_request();
    at(&mut req, VOL_SLOT0).projected_claim_set = None;
    let err = pv::classify_set_admission(&req)
        .expect_err("an absent projection certifies nothing")
        .to_string();
    assert!(err.contains("rung 7"), "the refusal names its rung: {err}");

    // A projection PREDATING the assignment: the record is there, the
    // `owner` field is not.
    let mut req = partial_request();
    if let Some(set) = at(&mut req, VOL_SLOT0).projected_claim_set.as_mut() {
        set.owner = None;
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("a projection without the owner field predates the assignment")
        .to_string();
    assert!(
        err.contains("rung 7") && err.contains(VOL_SLOT0),
        "the refusal names the rung and the volume: {err}"
    );

    // A projection showing an OLDER assignment (a different owner): also
    // behind its own volume's assignment.
    let mut req = partial_request();
    if let Some(set) = at(&mut req, VOL_SLOT0).projected_claim_set.as_mut() {
        set.owner = Some(THIRD.to_string());
    }
    let err = pv::classify_set_admission(&req)
        .expect_err("a stale projected assignment predates the durable one")
        .to_string();
    assert!(
        err.contains("rung 7") && err.contains(THIRD),
        "the refusal prints what the projection showed: {err}"
    );

    // An OWN volume needs no projection: this mount APPENDS to it, so it
    // reads its own records rather than a projection of a peer's.
    let mut req = partial_request();
    at(&mut req, VOL_A).projected_claim_set = None;
    pv::classify_set_admission(&req).expect("rung 7 governs peer-owned volumes only");
}

// ===========================================================================
// 3. The three pins the PR row names by hand
// ===========================================================================

/// **The Issue-8 pin.** The mount path opens `disc.ordered_paths` in
/// canonical `member_position` order, which is NOT the caller's URI order.
/// An index-keyed mode vector plus a permuted list would take the FULL D0
/// ladder — flock + PR WEX + claim — on a volume a peer owns: the worst
/// outcome in the program, reached by an off-by-permutation rather than a
/// race.
///
/// So: classify the SAME set twice, in two different orders, and assert
/// the modes follow the DURABLE volume id both times — including that the
/// mode at a given position differs between the two runs, which is what
/// makes the pin bite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_admission_resolves_modes_by_durable_volume_id_not_by_position() {
    let canonical = partial_request();
    let mut permuted = partial_request();
    permuted.volumes.reverse();

    let a = pv::classify_set_admission(&canonical).expect("admits in canonical order");
    let b = pv::classify_set_admission(&permuted).expect("admits in URI order");

    for vol_id in [VOL_SLOT0, VOL_A] {
        assert_eq!(
            a.mode_for(vol_id),
            b.mode_for(vol_id),
            "the mode of {vol_id} is a property of the volume, not of its position"
        );
    }
    assert_eq!(a.mode_for(VOL_A), Some(&VolumeMode::Own));
    assert!(matches!(
        a.mode_for(VOL_SLOT0),
        Some(VolumeMode::Peer { .. })
    ));

    // The permutation really did swap positions: the two requests' first
    // volumes have opposite modes, so a position-keyed reading would have
    // answered `Own` for the peer-owned volume.
    assert_ne!(
        canonical.volumes[0].vol_id, permuted.volumes[0].vol_id,
        "the fixture must actually permute"
    );
    assert!(matches!(
        b.mode_for(&permuted.volumes[1].vol_id),
        Some(VolumeMode::Peer { .. })
    ));
}

/// `covers` refuses a cross-set admission — the
/// `KvMetaBackend::open_co_writer` precedent: a decision taken over one
/// set may never open a volume of another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn covers_refuses_a_cross_set_admission() {
    let admission = pv::classify_set_admission(&partial_request()).expect("admits");

    let mine: Vec<String> = partial_request()
        .volumes
        .iter()
        .map(|v| v.path.display().to_string())
        .collect();
    assert!(admission.covers(&mine), "the set it was decided over");

    let mut reordered = mine.clone();
    reordered.reverse();
    assert!(
        admission.covers(&reordered),
        "coverage is a property of the SET, not of the order it is listed in"
    );

    let mut foreign = mine.clone();
    foreign.push("/dev/fake/vol-000000000000ffff".to_string());
    assert!(
        !admission.covers(&foreign),
        "a volume of another set is never covered"
    );

    assert!(
        !admission.covers(&mine[..1]),
        "a subset is not the set the decision was taken over"
    );
    assert!(!admission.covers(&[]), "an empty set covers nothing");

    // A list that REPEATS one volume and omits another has the right
    // length and every entry covered — and must still be refused, or a
    // permuted-and-deduped path list would open a volume twice while a
    // peer's went unnamed.
    let duplicated = vec![mine[0].clone(), mine[0].clone()];
    assert!(
        !admission.covers(&duplicated),
        "a repeated volume never stands in for the one it displaced"
    );
}

/// **The ladder is the ONLY `SetAdmission` constructor.** The decision must
/// be unforgeable, because PR 4's `open_peer_owned` takes one as its proof
/// that a per-volume admission was decided — a caller that could build one
/// could open a volume a peer holds the D0 claim on.
///
/// Pinned at the source level (the `tests/env_knob_convention_tests.rs`
/// census precedent): a `pub` field, a `Default` impl or a second struct
/// literal would each make the type forgeable, and none of the three is
/// expressible as a runtime assertion.
#[test]
fn the_ladder_is_the_only_set_admission_constructor() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/partial_authority.rs"
    ))
    .expect("the ladder's source");

    // Struct literals only: the declaration (`pub struct SetAdmission {`)
    // and the inherent impl (`impl SetAdmission {`) wear the same shape.
    let literals = src
        .match_indices("SetAdmission {")
        .filter(|(at, _)| {
            let before = src[..*at].trim_end();
            !before.ends_with("struct") && !before.ends_with("impl")
        })
        .count();
    assert_eq!(
        literals, 1,
        "exactly one `SetAdmission {{` literal (in classify_set_admission); found {literals}"
    );

    assert!(
        !src.contains("impl Default for SetAdmission"),
        "a Default impl would yield an admitted state nobody decided"
    );

    let decl = src
        .split_once("pub struct SetAdmission {")
        .expect("the struct declaration")
        .1;
    let body = decl.split_once("\n}").expect("the struct body").0;
    for line in body.lines() {
        assert!(
            !line.trim_start().starts_with("pub "),
            "SetAdmission fields are private (found `{}`)",
            line.trim()
        );
    }
}
