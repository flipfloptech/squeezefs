//! **Per-volume claim admission — the DERIVED ownership map**
//! (`docs/design-per-volume-claim-admission.md` §5.5.1/§5.10, KD-PV-3,
//! rulings D19/D20; PR 5).
//!
//! # What this rung makes true, and what must stay true beside it
//!
//! PR 4 landed a partial-writer open that can hold metadata authority over
//! a SUBSET of a volume set. Nothing consumed that fact: the ownership map
//! was still `OwnerMap::for_volumes(meta, Vec::new())` — all-local by
//! construction — so `volumes_owned_by` was empty on every mountable
//! topology and `mint_redirects` carried no signal. This file pins the
//! derivation that replaces it and the three consequences the design names:
//!
//! * **KD-PV-3** — the map is DERIVED from the durable **assignment**
//!   (`claim_set.owner` ∪ `successors`) conjoined with the live
//!   **evidence** (the replayed `writer_claim`, resolved to its holder's
//!   durable id through the KD-PV-17 attestation, and the D0 grant this
//!   mount holds on the volumes it opened `Own`). A disagreement REFUSES
//!   at admission and POISONS at runtime; silence never adopts.
//! * **§5.5.1** — `pick_mint_volume` prefers this node's OWN volumes while
//!   the plane is armed, which returns `mint_redirects` to a
//!   must-stay-≈0 gauge and restores balance among a node's own volumes.
//!   It is a PREFERENCE, never a gate: the empty arm is reachable at
//!   runtime through `disabled_volumes` and must fall back to the parent's
//!   volume rather than panic (Issue 29).
//! * **D20** — the owner of the volume hosting slot 0 is the SET
//!   AUTHORITY, and it alone derives the data-plane allocation lanes.
//!
//! # Why the refusal arms are pinned over FABRICATED evidence
//!
//! The `SetAdmissionRequest` precedent, for the same reason: a disagreeing
//! or unattested peer volume **cannot be part of an open set** — PR 4's
//! `open_peer_owned` refuses it at the door and a plain `KvMetaBackend::open`
//! refuses its live foreign claim — so a fixture that builds one through
//! the product's own paths cannot exist. [`VolumeOwnership`] is therefore
//! public and the derivation is split into a pure core plus the gather that
//! reads a live set, exactly as the seven-rung ladder is.
//!
//! The solo re-gate (R12) is PR 4's
//! `the_fresh_foreign_refusal_is_byte_identical_for_an_undeclared_mount`
//! and is untouched by this rung — a plain mount arms nothing, so every
//! derivation here is reached through a decision or a test constructor.

use squeezefs::cowriter::{MwRole, RegistrantEvidence};
use squeezefs::membership::{ClaimHolder, ClaimSet, ClaimSetMember, MemberIdentity, MemberRole};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR};
use squeezefs::meta_backend::kv::builder::FormatV3Options;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::meta_ship::owners::{self, VolumeOwnership};
use squeezefs::meta_ship::{OwnerMap, PeerOwner};
use squeezefs::partial_authority::{
    self as pv, ClaimStanding, PvVolumeEvidence, SetAdmission, SetAdmissionRequest,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;

/// This node's durable enrollment identity (KD-MW-2).
const NODE: &str = "node_00000000deadbeef.m00000001";
/// The peer this node's fixtures assign volumes to.
const PEER: &str = "node_00000000feedface.m00000001";
/// A third node — the stranger every disagreement arm is built around.
const STRANGER: &str = "node_00000000badc0ffe.m00000001";
/// Where `PEER` serves its metadata plane.
const PEER_ENDPOINT: &str = "127.0.0.1:7100";

/// The ownership plane is process-global: every armed test holds this and
/// disarms however it ends.
static PLANE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        squeezefs::meta_ship::disarm_ownership();
    }
}

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// The nine-bit multi-writer stamp (`volume enable-multi-writer`'s offline
/// act — the co-writer and PR-4 suites' helper verbatim).
async fn stamp_capabilities(path: &Path) {
    for (what, res) in [
        ("durable-term", sb::set_durable_term_bit(path).await),
        (
            "durable-block-refcounts",
            sb::set_block_refcounts_bit(path).await,
        ),
        (
            "durable-layout-versions",
            sb::set_layout_versions_bit(path).await,
        ),
        ("ino-lanes", sb::set_ino_lanes_bit(path).await),
        (
            "block-key-incarnation",
            sb::set_block_key_incarnation_bit(path).await,
        ),
        (
            "partitioned-append",
            sb::set_partitioned_append_bit(path).await,
        ),
        (
            "writer-scoped-staging",
            sb::set_writer_scoped_staging_bit(path).await,
        ),
        ("claim-set", sb::set_claim_set_bit(path).await),
        (
            "multi-writer-data",
            sb::set_multi_writer_data_bit(path).await,
        ),
    ] {
        res.unwrap_or_else(|e| panic!("stamping {what} failed: {e}"));
    }
}

/// An `n`-volume stamped set in canonical slot-plan order.
async fn volume_set(dir: &Path, tag: &str, n: usize) -> Vec<PathBuf> {
    let plan = squeezefs::meta_backend::plan_meta_slot_set(n).expect("derived slot plan");
    let mut out = Vec::new();
    for (i, stamp) in plan.stamps.iter().enumerate().take(n) {
        let p = dir.join(format!("{tag}-meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        squeezefs::meta_backend::kv::builder::format_v3_stamped_single_writer(
            &p,
            VOL_LEN,
            &opts(),
            stamp.clone(),
        )
        .await
        .expect("format stamped meta volume");
        stamp_capabilities(&p).await;
        out.push(p);
    }
    out
}

async fn vol_id(path: &Path) -> String {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    probe.durable_volume_id()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn member(id: &str) -> ClaimSetMember {
    ClaimSetMember {
        identity: MemberIdentity {
            id: id.to_string(),
            role: MemberRole::Writer,
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key: 0,
        },
        ts: 1_700_000_000,
    }
}

/// A live FOREIGN claim: another host's boot id, so no dead-pid proof can
/// reclaim it and the D0 ladder reads it as `Fresh`.
fn foreign_claim(tag: u8) -> WriterClaim {
    WriterClaim {
        id: format!("5f1d0e2a-0000-4000-8000-0000000000{tag:02x}"),
        ts: now_secs() + 2,
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term: 7,
    }
}

/// The durable record of a volume assigned to `owner`, optionally attested
/// (KD-PV-17) as being held by `holder`, with `successors` declared.
fn assigned_set(
    owner: &str,
    claim: Option<&WriterClaim>,
    holder: Option<&str>,
    successors: &[&str],
) -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.owner = Some(owner.to_string());
    set.successors = successors.iter().map(|s| s.to_string()).collect();
    set.members = vec![member(NODE), member(PEER), member(STRANGER)];
    if let (Some(claim), Some(holder)) = (claim, holder) {
        set.holder = Some(ClaimHolder {
            id: holder.to_string(),
            writer_id: claim.id.clone(),
            pid: claim.pid,
            boot: claim.boot.clone(),
        });
    }
    set
}

async fn store_set(path: &Path, set: &ClaimSet) {
    let be = KvMetaBackend::open(path).await.expect("store open");
    ClaimSet::store(&be, set).await.expect("store claim set");
    be.sync_device().await.expect("barrier");
    be.shutdown().await.expect("release");
}

/// Plant a live foreign `writer_claim` — dropped, never shut down, so the
/// record on disk names a LIVE holder.
async fn plant_claim(path: &Path, claim: &WriterClaim) {
    let be = KvMetaBackend::open(path).await.expect("planting open");
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
        .await
        .expect("plant claim");
    be.sync_device().await.expect("barrier");
    drop(be);
}

fn evidence(
    path: &Path,
    id: &str,
    hosts_slot_0: bool,
    claim: Option<WriterClaim>,
    holder: Option<&str>,
    set: ClaimSet,
) -> PvVolumeEvidence {
    PvVolumeEvidence {
        path: path.to_path_buf(),
        vol_id: id.to_string(),
        hosts_slot_0,
        features_incompat: squeezefs::cowriter::REQUIRED_INCOMPAT,
        pr_capable: true,
        standing: if claim.is_some() {
            ClaimStanding::Fresh
        } else {
            ClaimStanding::Reclaimable
        },
        claim,
        holder_member_id: holder.map(str::to_string),
        projected_claim_set: Some(set.clone()),
        claim_set: Some(set),
        owner_endpoint: Some(PEER_ENDPOINT.to_string()),
    }
}

/// The set-authority admission over a two-volume set: volume 0 (slot 0) is
/// ours, volume 1 is `PEER`'s.
fn set_authority_admission(
    vols: &[PathBuf],
    ids: &[String],
    peer_claim: &WriterClaim,
) -> SetAdmission {
    let req = SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::SetAuthority,
        read_only: false,
        node_id: NODE.to_string(),
        set_authority_endpoint: None,
        volumes: vec![
            evidence(
                &vols[0],
                &ids[0],
                true,
                None,
                None,
                assigned_set(NODE, None, None, &[]),
            ),
            evidence(
                &vols[1],
                &ids[1],
                false,
                Some(peer_claim.clone()),
                Some(PEER),
                assigned_set(PEER, Some(peer_claim), Some(PEER), &[]),
            ),
        ],
        authority: None,
        registrant: Some(RegistrantEvidence {
            pr_capable: true,
            wero: true,
            reservation_held: true,
            registered: true,
            key: 0xB0B0,
            namespaces: 1,
        }),
    };
    pv::classify_set_admission(&req).expect("the ladder admits the set-authority fixture")
}

fn uris(vols: &[PathBuf]) -> Vec<String> {
    vols.iter().map(|v| v.display().to_string()).collect()
}

/// The endpoint resolver the mount path supplies: a peer's published
/// endpoint, when the gather knew one.
fn endpoints(id: &str) -> Option<String> {
    (id != NODE).then(|| PEER_ENDPOINT.to_string())
}

async fn shutdown(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// One volume's fabricated ownership evidence.
fn ownership(
    vol_id: &str,
    set: Option<ClaimSet>,
    claim: Option<WriterClaim>,
    appended_locally: bool,
) -> VolumeOwnership {
    VolumeOwnership {
        vol_id: vol_id.to_string(),
        path: PathBuf::from(format!("/dev/null/{vol_id}")),
        claim_set: set,
        claim,
        appended_locally,
    }
}

// ===========================================================================
// 1. The derivation — assignment ∧ evidence (KD-PV-3, §5.10)
// ===========================================================================

/// The shipped shape: a set no operator has assigned derives an ALL-LOCAL
/// map. This is the solo posture's own pin — the derivation replaces
/// `for_volumes(meta, Vec::new())` and must answer exactly what it did on
/// every set the field can mount today.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unassigned_set_derives_an_all_local_map() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "unassigned", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("an ordinary write mount");

    let map = owners::derive_owner_map(&routed, NODE, &endpoints)
        .await
        .expect("an unassigned set derives");
    assert_eq!(map.volume_count(), 2);
    assert_eq!(map.local_volumes(), 2, "every volume is this node's");
    assert!(
        !map.multi_owner(),
        "an unassigned set is not a multi-owner plane"
    );
    assert!(map.owns_slot_0(), "the sole authority hosts slot 0");
    assert!(map.peers().is_empty());
    shutdown(&routed).await;
}

/// KD-PV-3's agreeing case, end to end over a REAL partial open: the
/// durable record assigns volume 1 to `PEER`, the live claim on it resolves
/// — through the KD-PV-17 attestation — to `PEER`, so the map ships that
/// volume to `PEER` at the endpoint the gather supplied. This is the entry
/// that has never existed on a mountable topology before this rung.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_derived_map_ships_an_assigned_volume_to_the_peer_that_holds_its_claim() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "derive", 2).await;
    let ids = vec![vol_id(&vols[0]).await, vol_id(&vols[1]).await];
    let claim = foreign_claim(1);
    store_set(&vols[0], &assigned_set(NODE, None, None, &[])).await;
    store_set(&vols[1], &assigned_set(PEER, Some(&claim), Some(PEER), &[])).await;
    plant_claim(&vols[1], &claim).await;
    let admission = set_authority_admission(&vols, &ids, &claim);
    let routed = squeezefs::meta_backend::open_routed_meta_set_partial(&uris(&vols), &admission)
        .await
        .expect("the partial set opens");

    let map = owners::derive_owner_map(&routed, NODE, &endpoints)
        .await
        .expect("assignment and evidence agree");
    assert_eq!(map.local_volumes(), 1);
    assert!(map.multi_owner(), "this IS a multi-owner plane");
    assert!(
        map.owns_slot_0(),
        "we hold slot 0, so we are the SET AUTHORITY"
    );
    let peer = map
        .owner_of_volume(1)
        .unwrap_or_else(|| panic!("volume 1 must name its peer owner"));
    assert_eq!(peer.peer_id, PEER);
    assert_eq!(peer.endpoint, PEER_ENDPOINT);
    assert_eq!(
        map.volumes_owned_by(PEER),
        vec![1],
        "the placement policy's candidate inversion finally answers something"
    );
    shutdown(&routed).await;
}

/// **The fail-closed law, in the shape the field can actually reach**: an
/// operator assigns ownership offline and then mounts a node with no peer
/// running, so the D0 ladder grants THIS node every claim — including the
/// volume the record assigns to `PEER`. Assignment and evidence disagree,
/// and the derivation refuses the arm naming both sides rather than
/// installing a map that makes this node an appender nobody assigned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_claim_on_a_peer_assigned_volume_refuses_the_derivation() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "graball", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("an ordinary write mount takes every claim");
    ClaimSet::store(&routed.volumes[0], &assigned_set(NODE, None, None, &[]))
        .await
        .expect("assign volume 0 to this node");
    ClaimSet::store(&routed.volumes[1], &assigned_set(PEER, None, None, &[]))
        .await
        .expect("assign volume 1 to the peer");

    let err = owners::derive_owner_map(&routed, NODE, &endpoints)
        .await
        .err()
        .unwrap_or_else(|| panic!("holding a peer-assigned volume's claim must refuse"));
    let text = err.to_string();
    assert!(
        text.contains(PEER) && text.contains("this node"),
        "the refusal must name the record's owner AND who is actually appending: {text}"
    );
    shutdown(&routed).await;
}

/// A partial assignment map — one volume owned, its sibling unassigned —
/// has no coherent appender story: the unassigned volume belongs to
/// everyone and to nobody, so the derivation refuses (the rung-3 law,
/// restated where the map is built).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partial_assignment_map_refuses_the_derivation() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "partialmap", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("an ordinary write mount");
    ClaimSet::store(&routed.volumes[0], &assigned_set(NODE, None, None, &[]))
        .await
        .expect("assign volume 0 only");

    let err = owners::derive_owner_map(&routed, NODE, &endpoints)
        .await
        .err()
        .unwrap_or_else(|| panic!("a partial assignment map must refuse"));
    assert!(
        err.to_string().contains("NO owner"),
        "the refusal must name the unassigned volume: {err}"
    );
    shutdown(&routed).await;
}

/// **Never adopt on silence** (KD-PV-3), over fabricated evidence because
/// the shape cannot be opened: a live claim with no KD-PV-17 attestation
/// resolves to nothing — `WriterClaim.id` is a per-mount uuid, not an
/// enrollment identity — so the derivation refuses rather than assuming
/// the assignment is being honoured by whoever is there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_derivation_never_adopts_on_silence() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "silence", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount (the slot map's source)");
    let claim = foreign_claim(3);

    let err = owners::derive_owner_map_from(
        &routed,
        NODE,
        &[
            ownership(
                "vol-a",
                Some(assigned_set(NODE, None, None, &[])),
                None,
                true,
            ),
            // Assigned to the peer, claimed by SOMEBODY, attested by nobody.
            ownership(
                "vol-b",
                Some(assigned_set(PEER, None, None, &[])),
                Some(claim),
                false,
            ),
        ],
        &endpoints,
    )
    .err()
    .unwrap_or_else(|| panic!("silence must refuse"));
    assert!(
        err.to_string().contains("SILENCE"),
        "the refusal must say what it read: {err}"
    );
    shutdown(&routed).await;
}

/// An assigned volume nothing claims has no appender: the derivation
/// refuses rather than serving a set with a hole (R13's operational face —
/// a dead owner is a fleet-wide stop, loud and immediate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_assigned_volume_no_node_claims_refuses_the_derivation() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "unclaimed", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount (the slot map's source)");

    let err = owners::derive_owner_map_from(
        &routed,
        NODE,
        &[
            ownership(
                "vol-a",
                Some(assigned_set(NODE, None, None, &[])),
                None,
                true,
            ),
            ownership(
                "vol-b",
                Some(assigned_set(PEER, None, None, &[])),
                None,
                false,
            ),
        ],
        &endpoints,
    )
    .err()
    .unwrap_or_else(|| panic!("an assigned-but-unclaimed volume must refuse"));
    assert!(
        err.to_string().contains("NOTHING claims it"),
        "the refusal must name the shape: {err}"
    );
    shutdown(&routed).await;
}

/// A holder the assignment set does not name is the R5 divergence itself:
/// shipping this volume's verbs there would make a node an appender nobody
/// assigned, so the derivation refuses naming the record's owner, its
/// declared successors and the observed holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_outside_the_assignment_set_refuses_the_derivation() {
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "stranger", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount (the slot map's source)");
    let claim = foreign_claim(4);

    let err = owners::derive_owner_map_from(
        &routed,
        NODE,
        &[
            ownership(
                "vol-a",
                Some(assigned_set(NODE, None, None, &[])),
                None,
                true,
            ),
            ownership(
                "vol-b",
                Some(assigned_set(PEER, Some(&claim), Some(STRANGER), &[])),
                Some(claim.clone()),
                false,
            ),
        ],
        &endpoints,
    )
    .err()
    .unwrap_or_else(|| panic!("a holder outside the assignment set must refuse"));
    let text = err.to_string();
    assert!(
        text.contains(PEER) && text.contains(STRANGER),
        "the refusal must name both sides of the disagreement: {text}"
    );
    shutdown(&routed).await;
}

/// **R4 — the global-ino stability law.** Ownership is a statement about
/// who appends, never about where an ino lives: `route_ino_width` is pure
/// arithmetic over `(ino, frozen W)`, so an assignment can never move a
/// single inode and ino 1 keeps pinning to slot 0 (KD-PV-6).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ownership_assignment_never_changes_route_ino_width() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "r4", 2).await;
    let ids = vec![vol_id(&vols[0]).await, vol_id(&vols[1]).await];
    let claim = foreign_claim(5);
    store_set(&vols[0], &assigned_set(NODE, None, None, &[])).await;
    store_set(&vols[1], &assigned_set(PEER, Some(&claim), Some(PEER), &[])).await;
    plant_claim(&vols[1], &claim).await;
    let admission = set_authority_admission(&vols, &ids, &claim);
    let routed = squeezefs::meta_backend::open_routed_meta_set_partial(&uris(&vols), &admission)
        .await
        .expect("the partial set opens");

    let width = routed.routing_width();
    let before: Vec<(u64, u64)> = (1..4096u64)
        .map(|ino| squeezefs::meta_backend::route_ino_width(ino, width))
        .collect();
    let routes_before: Vec<usize> = (1..4096u64).map(|ino| routed.route_ino(ino).0).collect();

    let map = owners::derive_owner_map(&routed, NODE, &endpoints)
        .await
        .expect("derive");
    assert_eq!(
        map.routing_width(),
        width,
        "the map records the set's frozen width verbatim"
    );
    squeezefs::meta_ship::arm_ownership(map);
    let _guard = ArmGuard;
    let after: Vec<(u64, u64)> = (1..4096u64)
        .map(|ino| squeezefs::meta_backend::route_ino_width(ino, width))
        .collect();
    let routes_after: Vec<usize> = (1..4096u64).map(|ino| routed.route_ino(ino).0).collect();
    assert_eq!(
        before, after,
        "an ownership assignment moved an ino — global inos are eternally stable (R4)"
    );
    assert_eq!(
        routes_before, routes_after,
        "an ownership assignment changed a volume route"
    );
    assert_eq!(
        squeezefs::meta_backend::route_ino_width(1, width),
        (0, 1),
        "ino 1 pins to slot 0 whatever the assignment says (KD-PV-6)"
    );
    shutdown(&routed).await;
}

// ===========================================================================
// 2. The runtime half — poison, and what never poisons (§5.10)
// ===========================================================================

/// Arm a map derived over fabricated evidence: volume 1 is assigned to
/// `PEER` with `successors` declared, and `holder` is appending to it.
fn arm_derived(
    routed: &RoutedMetaBackend,
    claim: &WriterClaim,
    holder: &str,
    successors: &[&str],
) -> ArmGuard {
    let map = owners::derive_owner_map_from(
        routed,
        NODE,
        &[
            ownership(
                "vol-a",
                Some(assigned_set(NODE, None, None, &[])),
                None,
                true,
            ),
            ownership(
                "vol-b",
                Some(assigned_set(PEER, Some(claim), Some(holder), successors)),
                Some(claim.clone()),
                false,
            ),
        ],
        &endpoints,
    )
    .expect("assignment and evidence agree");
    squeezefs::meta_ship::arm_ownership(map);
    ArmGuard
}

/// The runtime arm of KD-PV-3: a re-derivation from a fresh read that finds
/// the live holder OUTSIDE the volume's durable assignment set poisons that
/// entry — verbs on it then refuse loud rather than shipping to a node the
/// record does not entitle to append there — and the must-stay-0 gauge
/// `owner_map_poisoned_volumes` says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_holder_outside_the_assignment_set_poisons_the_volume() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "poison", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount (the slot map's source)");
    let claim = foreign_claim(6);
    let _guard = arm_derived(&routed, &claim, PEER, &[]);
    assert_eq!(owners::poisoned_volumes(), 0, "a healthy map is unpoisoned");
    assert!(owners::route_volume(1).expect("healthy route").is_some());

    // A successor nobody declared takes the volume: a NEW claim, attested
    // by a node the assignment set does not name.
    let usurper = foreign_claim(7);
    let fresh = ownership(
        "vol-b",
        Some(assigned_set(PEER, Some(&usurper), Some(STRANGER), &[])),
        Some(usurper),
        false,
    );
    assert!(
        !owners::reconcile_owner_from(1, &fresh),
        "the fresh read disagrees with the installed map"
    );
    assert!(owners::volume_poisoned(1), "the entry is poisoned");
    assert_eq!(
        owners::poisoned_volumes(),
        1,
        "owner_map_poisoned_volumes is the must-stay-0 tripwire and it fired"
    );
    let err = owners::route_volume(1)
        .err()
        .unwrap_or_else(|| panic!("a verb on a poisoned volume must refuse loud"));
    assert!(
        err.to_string().contains("poisoned"),
        "the refusal must name the fail-closed state: {err}"
    );
    assert!(
        owners::route_volume(0).is_ok(),
        "poison is per VOLUME — the rest of the set keeps routing"
    );
    shutdown(&routed).await;
}

/// §5.10 (rev 3, Issue 24) — **`a_successor_adoption_does_not_poison_peers_map_entries`**.
/// After a legitimate KD-PV-12 adoption the durable `owner` still names the
/// dead predecessor (the adoption deliberately writes nothing), so a poison
/// predicate reading `owner` ALONE would make every peer poison exactly the
/// volume the opt-in just recovered. The predicate reads the assignment SET
/// — `owner` ∪ `successors` — and the live claim disambiguates which member
/// of it is appending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successor_adoption_does_not_poison_peers_map_entries() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "successor", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount (the slot map's source)");
    let claim = foreign_claim(8);
    let _guard = arm_derived(&routed, &claim, PEER, &[STRANGER]);

    // The DECLARED successor adopts: a new claim, attested by it, while the
    // record's `owner` still names the dead predecessor.
    let adopted = foreign_claim(9);
    let fresh = ownership(
        "vol-b",
        Some(assigned_set(
            PEER,
            Some(&adopted),
            Some(STRANGER),
            &[STRANGER],
        )),
        Some(adopted),
        false,
    );
    assert!(
        owners::reconcile_owner_from(1, &fresh),
        "a DECLARED successor's adoption is inside the assignment SET and must not poison"
    );
    assert_eq!(
        owners::poisoned_volumes(),
        0,
        "the opt-in's own recovery path must not trip its tripwire"
    );
    let peer = owners::owner_of_volume(1).expect("still shipped");
    assert_eq!(
        peer.peer_id, STRANGER,
        "verbs follow the HOLDER, not the record's stale `owner` (§5.10)"
    );
    shutdown(&routed).await;
}

/// Poison is per VOLUME and only its own fresh read clears it. An
/// adoption republishes the map (the `PlacementTable` precedent — the
/// table is replaced, never mutated under readers), and a rebuild that
/// dropped every latch would silently un-poison a sibling nobody
/// re-derived.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adoption_on_one_volume_never_clears_a_siblings_poison() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "sibling", 3).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount (the slot map's source)");
    let claim_b = foreign_claim(10);
    let claim_c = foreign_claim(11);
    let map = owners::derive_owner_map_from(
        &routed,
        NODE,
        &[
            ownership(
                "vol-a",
                Some(assigned_set(NODE, None, None, &[])),
                None,
                true,
            ),
            ownership(
                "vol-b",
                Some(assigned_set(PEER, Some(&claim_b), Some(PEER), &[STRANGER])),
                Some(claim_b),
                false,
            ),
            ownership(
                "vol-c",
                Some(assigned_set(PEER, Some(&claim_c), Some(PEER), &[])),
                Some(claim_c),
                false,
            ),
        ],
        &endpoints,
    )
    .expect("assignment and evidence agree on both peer volumes");
    squeezefs::meta_ship::arm_ownership(map);
    let _guard = ArmGuard;

    // Volume 2 is poisoned by a usurper; volume 1 then legitimately
    // adopts its declared successor.
    let usurper = foreign_claim(12);
    assert!(!owners::reconcile_owner_from(
        2,
        &ownership(
            "vol-c",
            Some(assigned_set(PEER, Some(&usurper), Some(STRANGER), &[])),
            Some(usurper),
            false,
        ),
    ));
    let adopted = foreign_claim(13);
    assert!(owners::reconcile_owner_from(
        1,
        &ownership(
            "vol-b",
            Some(assigned_set(
                PEER,
                Some(&adopted),
                Some(STRANGER),
                &[STRANGER],
            )),
            Some(adopted),
            false,
        ),
    ));
    assert!(
        owners::volume_poisoned(2),
        "the adoption's republished map dropped a SIBLING's poison latch"
    );
    assert!(
        !owners::volume_poisoned(1),
        "the adopted volume routes again"
    );
    assert_eq!(owners::poisoned_volumes(), 1);
    shutdown(&routed).await;
}

// ===========================================================================
// 3. The mint funnel (§5.5.1) — a PREFERENCE, never a gate
// ===========================================================================

/// Arm a map over `routed` in which every volume in `peers` belongs to
/// `PEER` (the in-process constructor — the only way an assignment exists
/// before PR 7's verb).
fn arm_peer_owns(routed: &RoutedMetaBackend, peers: &[usize]) -> ArmGuard {
    let foreign = peers
        .iter()
        .map(|&v| (v, PeerOwner::new(PEER, PEER_ENDPOINT)))
        .collect();
    let map = OwnerMap::for_volumes(routed, foreign).expect("a per-volume map over this set");
    squeezefs::meta_ship::arm_ownership(map);
    ArmGuard
}

/// §5.5.1 + §11.2: with the plane armed, the mint pick's candidate set is
/// filtered to volumes this node OWNS — so no pick ever proposes a peer's
/// volume and `mint_redirects` returns to a must-stay-≈0 gauge instead of
/// growing structurally at rate `1 − owned/total`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_mint_pick_never_proposes_a_peer_owned_volume() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "mintfilter", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount");
    // Arm FIRST: the directory's own mint is filtered too, so a
    // multi-volume set places the parent on the volume this node keeps
    // rather than leaving it to the rotor's luck.
    let _guard = arm_peer_owns(&routed, &[1]);
    let redirects_before = squeezefs::meta_ship::stats().mint_redirects;
    let parent = routed
        .create(1, "mintdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir")
        .ino;
    assert_eq!(
        routed.route_ino(parent).0,
        0,
        "a DIRECTORY mint must be filtered to an owned volume too"
    );
    for i in 0..32 {
        let ino = routed
            .create(parent, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        assert_eq!(
            routed.route_ino(ino).0,
            0,
            "a mint landed on the PEER's volume"
        );
    }
    assert_eq!(
        squeezefs::meta_ship::stats().mint_redirects,
        redirects_before,
        "the filter must PREVENT the redirect, not rely on it: mint_redirects is a \
         must-stay-≈0 health gauge again (§11.2), and growth means the pick escaped the filter"
    );
    shutdown(&routed).await;
}

/// The second half of the same finding: without the filter the redirect
/// always lands on the PARENT's volume, so a node owning two volumes gets
/// no health/balance placement among its own at all. With it, both of this
/// node's volumes receive mints.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_volume_owner_balances_across_both_of_its_own_volumes() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "balance", 3).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount");
    let parent = routed
        .create(1, "baldir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir")
        .ino;

    // Volume 2 is the peer's; 0 and 1 are ours.
    let _guard = arm_peer_owns(&routed, &[2]);
    let mut seen = [false, false, false];
    for i in 0..64 {
        let ino = routed
            .create(parent, &format!("b{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        seen[routed.route_ino(ino).0] = true;
    }
    assert!(
        seen[0] && seen[1],
        "both of this node's own volumes must receive mints (the balance the redirect \
         destroyed): {seen:?}"
    );
    assert!(!seen[2], "the peer's volume received a mint: {seen:?}");
    shutdown(&routed).await;
}

/// **Issue 29 — the filter is a PREFERENCE, never a gate.** Rung 3
/// guarantees this node owns a volume at ADMISSION time; `disabled_volumes`
/// is populated at RUNTIME by the fail-stop lattice, so the filtered
/// candidate set can be empty exactly when an implementer would have
/// trusted it not to be. The existing `candidates.is_empty()` arm carries
/// it: fall back to the parent's volume (owned by construction under M2),
/// never panic, never a new refusal — and when the parent's volume is the
/// disabled one, the very next statement turns it into the clean typed
/// `check_volume_enabled` error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_mint_pick_with_every_owned_volume_disabled_falls_back_to_the_parents_volume() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "disabled", 3).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount");
    // A parent on volume 0 and a directory whose ino homes on volume 1, so
    // both arms of the empty case are exercised: the fallback lands on an
    // OWNED-but-disabled volume in one and on the enabled parent in the
    // other.
    let parent = routed
        .create(1, "disdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir")
        .ino;
    let parent_vol = routed.route_ino(parent).0;

    // Volume 2 is the peer's; every volume this node owns is then disabled
    // by the fail-stop lattice.
    let _guard = arm_peer_owns(&routed, &[2]);
    routed.disabled_volumes.insert(0, true);
    routed.disabled_volumes.insert(1, true);
    let created = routed
        .create(parent, "fallback", libc::S_IFREG | 0o644, 0, 0)
        .await;
    match created {
        Ok(entry) => assert_eq!(
            routed.route_ino(entry.ino).0,
            parent_vol,
            "the empty filtered set must fall back to the PARENT's volume, never a peer's"
        ),
        Err(e) => assert!(
            e.to_string().contains("disabled") || e.to_string().contains("fail-stop"),
            "a disabled parent volume must produce the clean typed check_volume_enabled error, \
             never a panic: {e}"
        ),
    }
    routed.disabled_volumes.clear();
    shutdown(&routed).await;
}

// ===========================================================================
// 4. D20 — the set-authority planes
// ===========================================================================

/// **D20 / §5.7**: two nodes deriving an allocation-lane width from one
/// roster is the collision the partition exists to prevent, so ONLY the
/// owner of the slot-0 volume derives it; a partial authority installs
/// `(writer_lane, writers)` from its custody lease instead. The refusal is
/// the mechanism — a non-set-authority that reached the derivation would
/// hand itself a lane nobody granted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_set_authority_never_derives_a_lane_assignment() {
    let _plane = PLANE.lock().await;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "d20", 2).await;
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("write mount");
    let slot_0_vol = routed.route_ino(1).0;

    // A partial authority: the slot-0 volume is the PEER's.
    let peer_map = OwnerMap::for_volumes(
        &routed,
        vec![(slot_0_vol, PeerOwner::new(PEER, PEER_ENDPOINT))],
    )
    .expect("map");
    assert!(!peer_map.owns_slot_0());
    let err = squeezefs::multi_writer::derive_lane_assignment(&routed, &peer_map)
        .await
        .err()
        .unwrap_or_else(|| panic!("a partial authority must never derive a lane assignment"));
    assert!(
        err.to_string().contains("slot 0") && err.to_string().contains("custody lease"),
        "the refusal must name D20's rule and where the lane actually comes from: {err}"
    );

    // The SET authority derives (solo here — no membership owner is
    // installed, which is the arm's own no-partition answer).
    let own_map = OwnerMap::for_volumes(&routed, Vec::new()).expect("map");
    assert!(own_map.owns_slot_0());
    let assignment = squeezefs::multi_writer::derive_lane_assignment(&routed, &own_map)
        .await
        .expect("the set authority derives");
    assert!(
        assignment.authority_partition().is_solo(),
        "with no enrolled co-writer the derivation is solo, which installs nothing"
    );
    shutdown(&routed).await;
}
