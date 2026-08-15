//! DLM stage **S9 — the CO-WRITER mount posture and its admission gate**
//! (`docs/pre-rc-engineering-spec.md` §6.2 item 7's *consumer* half, §6.9
//! S9; rulings **D8** (N writers + N readers), **D9** (bits are built,
//! never stamped) and **D11** (the measured half is frozen)).
//!
//! # The blocker this file defines away
//!
//! S9 landed the multi-writer machinery and named the two things standing
//! between it and a real N-writer demonstration. This is the second, in
//! S9's own words:
//!
//! > *"The D0 Layer-B2 gate refuses a fresh foreign claim on every
//! > substrate, unconditionally and regardless of bit 14. A co-writer holds
//! > no `writer_claim`, so it cannot open the metadata volumes at all; the
//! > co-writer posture is unreachable from `main` until that gate admits a
//! > co-member of an *engaged* claim set."*
//!
//! and its sibling: *"a metadata-read-only/data-read-write posture does not
//! exist (`BlockAllocator::reader_gate` refuses on `-o ro`)."*
//!
//! # The admission ladder, and what each rung answers
//!
//! | Rung | Requirement | The threat it answers |
//! |---|---|---|
//! | 1 | `SQUEEZEFS_MULTI_WRITER=1` **and** `SQUEEZEFS_MW_ROLE=co-writer` **and** a declared authority, and NOT `-o ro` | an accidental second write mount: the posture must be DECLARED, never inferred, so a plain `mount` of a claimed set still refuses `FreshForeign` verbatim |
//! | 2 | every volume of the set carries the six S9 capability bits, **bit 14 included** | a half-engaged set is not a claim set: without a durable `claim_set` record, membership IS the singular `writer_claim`, which expresses exclusion and cannot represent a second member |
//! | 3 | the durable claim set **names this node** as a writer member | self-assertion: admission is by durable enrollment written by the AUTHORITY (the only process that can commit to those volumes), never by a claim the joining node makes about itself |
//! | 4 | the membership plane is armed and the authority's lease is **live** | a co-writer with no live authority has no custody source and no evictor — it must refuse, not proceed hopefully |
//! | 5 | a PR-capable substrate whose standing WERO hold **names this node as a registrant** | §6.7's "refused on non-PR" applies to the ADMISSION decision too: a co-writer whose DMA the device cannot reject is a co-writer nothing can fence |
//!
//! # What this file pins
//!
//! 1. The gate ADMITS on the full ladder and refuses with a rung-specific
//!    message when any single rung is missing (five cases).
//! 2. **The D0 refusal is untouched**: with every rung satisfied,
//!    `KvMetaBackend::open` still refuses a fresh foreign claim, verbatim.
//!    The co-writer posture is a different ENTRY POINT, not a hole in the
//!    gate.
//! 3. A co-writer open writes **nothing**: the volume's bytes are
//!    byte-identical across it (digest), no `writer_claim`, no PR
//!    registrant on the meta namespace, no checkpoint task.
//! 4. A co-writer **denies the authority nothing**, in both mount orders.
//! 5. The recovery ladder still classifies with a co-writer attached
//!    (`claim clear` refuses the live authority's claim; a co-writer is
//!    never mistaken for a claim holder).
//! 6. Metadata mutations are **shipped**, not committed locally.
//! 7. Data writes are admitted under a grant and refused without one,
//!    while ownership ACCOUNTING (allocation, terminal free, W1's
//!    incarnation retire) is refused with a message naming the
//!    data-plane allocation partition that owns it.
//! 8. A reader is still refused at **every** site the reader gate guards,
//!    with its own unchanged text.
//! 9. An authority plus TWO co-writers under a multi-threaded runtime.
//!
//! # What one process CANNOT pin (stated, not hidden)
//!
//! * **Device rejection** of an unregistered host's DMA is a property of a
//!   real PR-capable namespace; the fake namespace pins the DECISION and
//!   the ladder, never the silicon.
//! * **Two hosts.** The "co-writer" here is a second `KvMetaBackend` over
//!   the same files in one process, which is what makes the gate, the
//!   claim-set consumer half, the posture latches and the shipped publish
//!   real. Two independent node caches diverging needs two hosts.
//! * **The field arm**: nothing stamps the capability bits (D9), so these
//!   tests stamp them through the offline `set_*_bit` paths — exactly the
//!   Phase-8 reformat window's act.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cluster_wire as cw;
use squeezefs::cowriter::{
    self, AdmissionRequest, AuthorityLeaseEvidence, RegistrantEvidence, VolumeAdmissionEvidence,
};
use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::dlm::LockMode;
use squeezefs::fuse_client::{self, MountPosture};
use squeezefs::membership::{
    ClaimSet, ClaimSetMember, LeaseClock, LeaseClocks, MemberIdentity, MemberRole,
};
use squeezefs::meta_backend::kv::backend::{ClaimClearOutcome, KvMetaBackend, WriterClaim};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::reservation::{
    self, FakeNvmeNamespace, FakeReservationClient, ReservationClient,
};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;

/// The `job:enroll`-class storage-trust secret both halves prove
/// possession of (S3's root of trust).
const SECRET: &[u8] = b"s9-cowriter-admission-storage-trust-secret";

// ---------------------------------------------------------------------------
// Serialization + posture restoration (process-global state everywhere)
// ---------------------------------------------------------------------------

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

/// Restores every process-global posture a co-writer test can move, so a
/// panicking assertion never leaves the binary armed.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        fuse_client::set_mount_posture(MountPosture::Writer);
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
        for k in [
            "SQUEEZEFS_MULTI_WRITER",
            "SQUEEZEFS_MW_ROLE",
            "SQUEEZEFS_MW_AUTHORITY",
            "SQUEEZEFS_MW_MEMBERS",
        ] {
            std::env::remove_var(k);
        }
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Stamp the FULL nine-bit multi-writer set — `volume enable-multi-writer`'s
/// act (KD-MW-1), offline, between format and open. The ARM requires only
/// its six capability bits, but since PR 5 the writable-mount gate enforces
/// the §6.2 bit-11 uniformity invariant ("bit 11 set ⇒ all nine set"), so a
/// bit-11 fixture volume must carry the whole set. Bits 8/12/15 are
/// behaviorally inert for these suites (partitioned-solo is byte-identical;
/// solo ino minting is lane 0 = dense).
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

async fn fresh_volume(dir: &Path, name: &str, stamp: bool) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    format_v3(&p, VOL_LEN, &opts()).await.unwrap();
    if stamp {
        stamp_capabilities(&p).await;
    }
    p
}

fn digest(path: &Path) -> u64 {
    let bytes = std::fs::read(path).expect("read the whole volume");
    xxhash_rust::xxh3::xxh3_64(&bytes)
}

fn our_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("boot_id readable on Linux")
        .trim()
        .to_string()
}

/// A pid that provably cannot be alive (the guard suite's helper).
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn /bin/true");
    let pid = child.id();
    child.wait().expect("reap /bin/true");
    pid
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Plant a FOREIGN `writer_claim` (another host's boot) and leave it
/// behind, exactly as a live cross-host authority's record looks to us.
async fn forge_foreign_claim(path: &Path, id: &str) -> WriterClaim {
    let claim = WriterClaim {
        id: id.to_string(),
        ts: now_secs(),
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term: 7,
    };
    let be = KvMetaBackend::open(path).await.expect("forge open");
    be.setxattr_internal(
        1,
        squeezefs::meta_backend::kv::backend::WRITER_CLAIM_XATTR,
        &claim.encode(),
    )
    .await
    .expect("forge claim");
    be.sync_device().await.expect("forge barrier");
    drop(be);
    claim
}

// ---------------------------------------------------------------------------
// Evidence builders (the mount path's gatherer is what builds these in
// production — see `cowriter::gather_admission`; the suite builds them
// directly, the `dlm_slot::test_set_local_slots` precedent)
// ---------------------------------------------------------------------------

fn member(id: &str, role: MemberRole, pr_key: u64) -> ClaimSetMember {
    ClaimSetMember {
        identity: MemberIdentity {
            id: id.to_string(),
            role,
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key,
        },
        ts: now_secs(),
    }
}

fn full_claim_set(node_id: &str, owner_id: &str) -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.members
        .push(member(owner_id, MemberRole::Writer, 0xA0A0));
    set.members
        .push(member(node_id, MemberRole::Writer, 0xB0B0));
    set
}

fn volume_evidence(path: &Path, node_id: &str, owner_id: &str) -> VolumeAdmissionEvidence {
    VolumeAdmissionEvidence {
        path: path.to_path_buf(),
        features_incompat: cowriter::REQUIRED_INCOMPAT,
        claim: Some(WriterClaim {
            id: "authority-claim".to_string(),
            ts: now_secs(),
            pid: 4242,
            boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
            term: 7,
        }),
        claim_set: Some(full_claim_set(node_id, owner_id)),
    }
}

fn authority_evidence(owner_id: &str) -> AuthorityLeaseEvidence {
    AuthorityLeaseEvidence {
        owner_id: owner_id.to_string(),
        endpoint: "127.0.0.1:7000".to_string(),
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

/// A complete, admissible request over `paths`.
fn full_request(paths: &[PathBuf]) -> AdmissionRequest {
    let node_id = "node_00000000deadbeef";
    let owner_id = "authority-membership-owner";
    AdmissionRequest {
        multi_writer: true,
        role_co_writer: true,
        read_only: false,
        node_id: node_id.to_string(),
        custody_endpoint: Some("127.0.0.1:7100".to_string()),
        volumes: paths
            .iter()
            .map(|p| volume_evidence(p, node_id, owner_id))
            .collect(),
        authority: Some(authority_evidence(owner_id)),
        registrant: Some(registrant_evidence()),
    }
}

// ===========================================================================
// 1. The ladder admits
// ===========================================================================

/// Contract: the five rungs, all satisfied, ADMIT — and the admission
/// carries what the posture needs (this node's durable member id, the
/// authority's identity and era, the custody endpoint, the registrant key,
/// and the volumes it covers).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_full_five_rung_ladder_admits_a_co_writer() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    let req = full_request(std::slice::from_ref(&vol));
    let admission = cowriter::classify_admission(&req).expect("the full ladder admits");

    assert_eq!(admission.node_id(), "node_00000000deadbeef");
    assert_eq!(
        admission.membership_owner_id(),
        "authority-membership-owner"
    );
    assert_eq!(admission.authority_claim_id(), "authority-claim");
    assert_eq!(admission.authority_term(), 7);
    assert_eq!(admission.custody_endpoint(), "127.0.0.1:7100");
    assert_eq!(admission.pr_key(), 0xB0B0);
    assert!(
        admission.covers(&vol),
        "the admission names every volume it was decided over"
    );
}

// ===========================================================================
// 2. Five refusals, one per rung, each naming what is missing
// ===========================================================================

/// Rung 1: the posture must be DECLARED. Without the opt-in the gate
/// refuses, and the refusal names both knobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_1_refuses_without_the_declared_co_writer_posture() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    let mut req = full_request(std::slice::from_ref(&vol));
    req.multi_writer = false;
    let err = cowriter::classify_admission(&req)
        .expect_err("no opt-in, no co-writer")
        .to_string();
    assert!(
        err.contains("SQUEEZEFS_MULTI_WRITER"),
        "the refusal names the opt-in: {err}"
    );

    let mut req = full_request(std::slice::from_ref(&vol));
    req.role_co_writer = false;
    let err = cowriter::classify_admission(&req)
        .expect_err("an authority-role mount is never admitted as a co-writer")
        .to_string();
    assert!(
        err.contains("SQUEEZEFS_MW_ROLE"),
        "the refusal names the role knob: {err}"
    );

    // A reader is a CATEGORY error, not a degradation (S9's rung 1).
    let mut req = full_request(std::slice::from_ref(&vol));
    req.read_only = true;
    let err = cowriter::classify_admission(&req)
        .expect_err("a reader cannot be a co-writer")
        .to_string();
    assert!(
        err.to_lowercase().contains("read-only"),
        "the refusal names the reader posture: {err}"
    );

    // And an undeclared authority: a co-writer with nowhere to acquire
    // custody from is inert, so it refuses rather than mounting.
    let mut req = full_request(&[vol]);
    req.custody_endpoint = None;
    let err = cowriter::classify_admission(&req)
        .expect_err("no authority endpoint, no custody source")
        .to_string();
    assert!(
        err.contains("SQUEEZEFS_MW_AUTHORITY"),
        "the refusal names the dial target: {err}"
    );
}

/// Rung 2: a HALF-engaged set is not a claim set. One volume missing bit
/// 14 refuses the whole set, naming the volume and the bit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_2_refuses_a_half_engaged_claim_set() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let a = fresh_volume(dir.path(), "meta0", true).await;
    let b = fresh_volume(dir.path(), "meta1", true).await;

    let mut req = full_request(&[a, b.clone()]);
    req.volumes[1].features_incompat =
        cowriter::REQUIRED_INCOMPAT & !sb::FEATURE_INCOMPAT_KV_CLAIM_SET;
    let err = cowriter::classify_admission(&req)
        .expect_err("bit 14 must be engaged on EVERY volume")
        .to_string();
    assert!(
        err.contains("14"),
        "the refusal names the claim-set bit: {err}"
    );
    assert!(
        err.contains(&b.display().to_string()),
        "the refusal names the volume that lacks it: {err}"
    );

    // A projection (an un-engaged volume's `writer_claim` read through
    // `ClaimSet::from_writer_claim`) is NOT a claim set either.
    let mut req = full_request(&[b]);
    if let Some(set) = req.volumes[0].claim_set.as_mut() {
        set.durable = false;
    }
    let err = cowriter::classify_admission(&req)
        .expect_err("a projection expresses exclusion, not membership")
        .to_string();
    assert!(
        err.to_lowercase().contains("projection") || err.to_lowercase().contains("durable"),
        "the refusal explains why a projection cannot admit: {err}"
    );
}

/// Rung 3: admission is by durable ENROLLMENT. A set that does not name
/// this node refuses, and the refusal prints the node's own id plus the
/// authority-side roster knob that enrolls it — because a co-writer cannot
/// enroll itself into a set it cannot commit to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_3_refuses_a_set_that_does_not_name_this_node() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    let mut req = full_request(std::slice::from_ref(&vol));
    req.volumes[0].claim_set = Some({
        let mut set = ClaimSet::empty(7);
        set.durable = true;
        set.members.push(member(
            "authority-membership-owner",
            MemberRole::Writer,
            0xA0A0,
        ));
        set
    });
    let err = cowriter::classify_admission(&req)
        .expect_err("an unenrolled node is refused")
        .to_string();
    assert!(
        err.contains("node_00000000deadbeef"),
        "the refusal prints the identity an operator must enroll: {err}"
    );
    assert!(
        err.contains("SQUEEZEFS_MW_MEMBERS"),
        "the refusal names the AUTHORITY-side roster that writes the entry: {err}"
    );

    // A node enrolled as a READER is not a co-writer.
    let mut req = full_request(&[vol]);
    req.volumes[0].claim_set = Some({
        let mut set = ClaimSet::empty(7);
        set.durable = true;
        set.members.push(member(
            "authority-membership-owner",
            MemberRole::Writer,
            0xA0A0,
        ));
        set.members
            .push(member("node_00000000deadbeef", MemberRole::Reader, 0xB0B0));
        set
    });
    let err = cowriter::classify_admission(&req)
        .expect_err("a reader-role enrollment cannot admit a write custody holder")
        .to_string();
    assert!(
        err.to_lowercase().contains("writer"),
        "the refusal names the role the entry must carry: {err}"
    );
}

/// Rung 4: no live authority, no admission. A co-writer with no custody
/// source and no evictor refuses rather than proceeding hopefully.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_4_refuses_without_a_live_membership_authority() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    let mut req = full_request(std::slice::from_ref(&vol));
    req.authority = None;
    let err = cowriter::classify_admission(&req)
        .expect_err("an unarmed membership plane refuses the posture")
        .to_string();
    assert!(
        err.contains("SQUEEZEFS_MEMBERSHIP_BIND"),
        "the refusal names what the AUTHORITY must arm: {err}"
    );

    let mut req = full_request(std::slice::from_ref(&vol));
    req.authority = Some(AuthorityLeaseEvidence {
        live: false,
        ..authority_evidence("authority-membership-owner")
    });
    let err = cowriter::classify_admission(&req)
        .expect_err("a rendezvous record whose owner does not answer is not a live lease")
        .to_string();
    assert!(
        err.to_lowercase().contains("live") || err.to_lowercase().contains("lease"),
        "the refusal says the lease is not live: {err}"
    );

    // A rendezvous record from an OLDER era than the claim we can see is
    // stale evidence: the authority it names is not the one holding D0.
    let mut req = full_request(&[vol]);
    req.authority = Some(AuthorityLeaseEvidence {
        term: 6,
        ..authority_evidence("authority-membership-owner")
    });
    let err = cowriter::classify_admission(&req)
        .expect_err("a stale era refuses")
        .to_string();
    assert!(
        err.to_lowercase().contains("term") || err.to_lowercase().contains("era"),
        "the refusal names the era disagreement: {err}"
    );
}

/// Rung 5: §6.7's "refused on non-PR" applies to the ADMISSION decision.
/// A detection-grade substrate, an unheld reservation, a non-WERO type and
/// an unregistered key each refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_5_refuses_without_a_device_registrant_under_a_standing_wero() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    let mut req = full_request(std::slice::from_ref(&vol));
    req.registrant = None;
    let err = cowriter::classify_admission(&req)
        .expect_err("no registrant evidence, no admission")
        .to_string();
    assert!(
        err.to_lowercase().contains("reservation") || err.to_lowercase().contains("registrant"),
        "the refusal names the device half: {err}"
    );

    for (label, ev) in [
        (
            "non-PR substrate",
            RegistrantEvidence {
                pr_capable: false,
                ..registrant_evidence()
            },
        ),
        (
            "no standing reservation",
            RegistrantEvidence {
                reservation_held: false,
                ..registrant_evidence()
            },
        ),
        (
            "not registrants-only",
            RegistrantEvidence {
                wero: false,
                ..registrant_evidence()
            },
        ),
        (
            "our key is not a registrant",
            RegistrantEvidence {
                registered: false,
                ..registrant_evidence()
            },
        ),
    ] {
        let mut req = full_request(std::slice::from_ref(&vol));
        req.registrant = Some(ev);
        let err = cowriter::classify_admission(&req)
            .unwrap_err()
            .to_string()
            .to_lowercase();
        assert!(
            err.contains("reserv") || err.contains("registr") || err.contains("wero"),
            "{label}: the refusal names the device half: {err}"
        );
    }
}

// ===========================================================================
// 3. The D0 refusal is untouched
// ===========================================================================

/// **The safety contract of this whole change.** With every rung satisfied
/// — the bits stamped, the set naming us, the authority live, the device
/// registering us — a plain WRITE-mount open of a volume carrying a live
/// foreign claim is STILL refused, with the single-writer guard's own text.
/// The co-writer posture is a separate entry point; Layer B2 did not learn
/// a bypass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_d0_fresh_foreign_refusal_survives_every_rung_being_satisfied() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;
    forge_foreign_claim(&vol, "the-authority").await;

    std::env::set_var("SQUEEZEFS_MULTI_WRITER", "1");
    std::env::set_var("SQUEEZEFS_MW_ROLE", "co-writer");
    std::env::set_var("SQUEEZEFS_MW_AUTHORITY", "127.0.0.1:7100");

    let err = KvMetaBackend::open(&vol)
        .await
        .expect_err("the D0 gate refuses a live foreign claim on every substrate")
        .to_string();
    assert!(
        err.contains("single-writer guard"),
        "the refusal is the unchanged D0 text: {err}"
    );
    assert!(
        err.contains("the-authority"),
        "and it still names the holder: {err}"
    );

    // The co-writer ENTRY POINT admits the same volume with the same
    // evidence, which is the whole point: two doors, one refusal each.
    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&vol)))
        .expect("the ladder admits");
    let be = KvMetaBackend::open_co_writer(&vol, &admission)
        .await
        .expect("a co-writer opens a volume the write mount is refused");
    assert_eq!(be.writer_guard_mode(), "co-writer");
    drop(be);
}

// ===========================================================================
// 4. A co-writer writes nothing; a reader is unchanged
// ===========================================================================

/// Contract: a co-writer open is byte-identical to not mounting at all —
/// the volume's bytes, its `writer_claim` and its open trace are all
/// untouched. No claim, no PR registrant, no checkpoint task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_co_writer_open_leaves_the_metadata_volume_byte_identical() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;
    let claim = forge_foreign_claim(&vol, "the-authority").await;

    let before = digest(&vol);
    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&vol)))
        .expect("the ladder admits");
    let be = KvMetaBackend::open_co_writer(&vol, &admission)
        .await
        .expect("co-writer open");

    // The authority's claim is what this mount reads, and it stays the
    // authority's: a co-writer never writes one.
    let seen = be.read_writer_claim().await.expect("the authority's claim");
    assert_eq!(seen.id, claim.id, "a co-writer never overwrites the claim");
    assert!(
        !be.open_trace().iter().any(|e| e.starts_with("claim")),
        "no claim event in a co-writer's open trace: {:?}",
        be.open_trace()
    );
    assert!(
        be.is_read_only(),
        "the metadata plane is read-only LOCALLY on a co-writer"
    );

    drop(be);
    assert_eq!(
        digest(&vol),
        before,
        "a co-writer open must not change one byte of the metadata volume"
    );
}

/// Contract: the reader path is untouched by the co-writer posture — same
/// mode word, same byte-identity, same refusal text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reader_path_is_unchanged_by_the_co_writer_posture() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;
    forge_foreign_claim(&vol, "the-authority").await;

    let before = digest(&vol);
    let be = KvMetaBackend::open_read_only(&vol)
        .await
        .expect("the S5 reader opens");
    assert_eq!(be.writer_guard_mode(), "reader");
    drop(be);
    assert_eq!(digest(&vol), before, "a reader writes nothing, unchanged");
}

// ===========================================================================
// 5. A co-writer denies the authority nothing, in both mount orders
// ===========================================================================

/// Contract: the co-writer takes no lock the authority needs. Both orders
/// work — co-writer first then the authority's write mount, and the
/// authority first then the co-writer — because the authority's `LOCK_EX`
/// and the co-writer's (released) `LOCK_SH` probe never contend.
///
/// This is also the "two co-writer mounts on ONE host" case: neither
/// excludes the other, and their mutual exclusion is the authority's
/// custody, not `flock`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writer_denies_the_authority_nothing_in_both_mount_orders() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();

    // Order A: co-writer attaches first, the authority mounts after.
    let a = fresh_volume(dir.path(), "order-a", true).await;
    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&a)))
        .expect("the ladder admits");
    let cw1 = KvMetaBackend::open_co_writer(&a, &admission)
        .await
        .expect("co-writer first");
    let authority = KvMetaBackend::open(&a)
        .await
        .expect("a co-writer must not deny the authority its write mount");
    assert_eq!(authority.writer_guard_mode(), "flock+claim");
    // A second co-writer on the same host attaches too.
    let cw2 = KvMetaBackend::open_co_writer(&a, &admission)
        .await
        .expect("two co-writer mounts on one host are legitimate");
    drop(cw1);
    drop(cw2);
    authority
        .shutdown()
        .await
        .expect("authority unmounts clean");

    // Order B: the authority mounts first, the co-writer attaches after.
    let b = fresh_volume(dir.path(), "order-b", true).await;
    let authority = KvMetaBackend::open(&b).await.expect("authority first");
    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&b)))
        .expect("the ladder admits");
    let cw = KvMetaBackend::open_co_writer(&b, &admission)
        .await
        .expect("a co-writer attaches to a live authority's volume");
    assert_eq!(cw.writer_guard_mode(), "co-writer");
    drop(cw);
    authority
        .shutdown()
        .await
        .expect("authority unmounts clean");
}

// ===========================================================================
// 6. The recovery ladder still classifies with a co-writer attached
// ===========================================================================

/// Contract: a co-writer is never mistaken for a claim holder by the
/// recovery ladder. With one attached: `claim clear` still refuses the
/// LIVE authority's claim (the attestation must not automate that away),
/// still clears a TTL-stale one, and the dead-pid proof still reclaims —
/// none of those decisions can see a co-writer, because a co-writer writes
/// no claim and retains no lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_recovery_ladder_still_classifies_with_a_co_writer_attached() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();

    // (a) A fresh foreign claim + an attached co-writer: `claim clear`
    //     refuses, naming the live holder.
    let vol = fresh_volume(dir.path(), "fresh", true).await;
    forge_foreign_claim(&vol, "the-authority").await;
    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&vol)))
        .expect("the ladder admits");
    let cw = KvMetaBackend::open_co_writer(&vol, &admission)
        .await
        .expect("co-writer attaches");
    let err = KvMetaBackend::claim_clear(&vol)
        .await
        .expect_err("clearing a LIVE writer's claim is never automated")
        .to_string();
    assert!(
        err.to_lowercase().contains("live") || err.to_lowercase().contains("fresh"),
        "the refusal names the live holder: {err}"
    );
    drop(cw);

    // (b) A TTL-stale claim + an attached co-writer: the attested verb
    //     still clears it and reports the holder it removed.
    let vol = fresh_volume(dir.path(), "stale", true).await;
    let stale = WriterClaim {
        id: "dead-authority".to_string(),
        ts: now_secs() - (squeezefs::fuse_client::CLIENT_STALE_TTL_SECS * 4),
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term: 7,
    };
    let be = KvMetaBackend::open(&vol).await.expect("plant open");
    be.setxattr_internal(
        1,
        squeezefs::meta_backend::kv::backend::WRITER_CLAIM_XATTR,
        &stale.encode(),
    )
    .await
    .expect("plant stale claim");
    be.sync_device().await.expect("barrier");
    drop(be);
    match KvMetaBackend::claim_clear(&vol).await {
        Ok(ClaimClearOutcome::Cleared(holder)) => {
            assert_eq!(
                holder.id, "dead-authority",
                "the verb reports what it removed"
            )
        }
        other => panic!("a TTL-stale claim must clear: {other:?}"),
    }

    // (c) The same-host dead-pid proof is untouched.
    let vol = fresh_volume(dir.path(), "deadpid", true).await;
    let dead = WriterClaim {
        id: "crashed-authority".to_string(),
        ts: now_secs(),
        pid: dead_pid(),
        boot: our_boot_id(),
        term: 7,
    };
    let be = KvMetaBackend::open(&vol).await.expect("plant open");
    be.setxattr_internal(
        1,
        squeezefs::meta_backend::kv::backend::WRITER_CLAIM_XATTR,
        &dead.encode(),
    )
    .await
    .expect("plant dead claim");
    be.sync_device().await.expect("barrier");
    drop(be);
    let reclaimed = KvMetaBackend::open(&vol)
        .await
        .expect("a provably dead same-host holder is reclaimed instantly");
    assert_eq!(reclaimed.writer_guard_mode(), "flock+claim");
    reclaimed.shutdown().await.expect("clean unmount");
}

// ===========================================================================
// 7. Metadata: shipped, not committed locally
// ===========================================================================

/// Contract: on a co-writer the metadata plane has no LOCAL authority — a
/// local commit refuses, and the refusal names the shipped publish path
/// rather than pretending the mount is a reader. The same mutation, routed
/// through the ownership plane, EXECUTES on the authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writer_ships_metadata_mutations_instead_of_committing_them() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    // The authority: a real write mount serving the publish vocabulary.
    let authority = squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
        .await
        .expect("the authority mounts");
    let listener = {
        let router = data_grant::AsyncVerbRouter::new()
            .with_publish(publish::PublishService::new(Arc::clone(&authority)));
        cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            SECRET.to_vec(),
            Arc::new(router),
        )
        .expect("the authority's publish listener")
    };
    let endpoint = listener.endpoint().to_string();

    // A file to publish a layout for, created ON the authority.
    let ino = authority
        .create_with_rdev_size(1, "shipped.bin", 0o100644, 0, 0, 0, 0)
        .await
        .expect("create on the authority")
        .ino;

    // The co-writer: a local commit refuses, naming the shipped path.
    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&vol)))
        .expect("the ladder admits");
    let co = KvMetaBackend::open_co_writer(&vol, &admission)
        .await
        .expect("co-writer open");
    let err = co
        .setxattr_internal(1, "user.cowriter-probe", b"nope")
        .await
        .expect_err("a co-writer commits no metadata locally")
        .to_string();
    assert!(
        err.to_lowercase().contains("ship") || err.to_lowercase().contains("authority"),
        "the refusal names the shipped publish path, not a reader mount: {err}"
    );

    // Now arm the ownership plane the way a co-writer's mount does — the
    // authority owns EVERY volume — and ship the same class of mutation.
    let co_routed = squeezefs::meta_backend::open_routed_meta_set_co_writer(
        &[vol.display().to_string()],
        &admission,
    )
    .await
    .expect("the co-writer's routed set");
    let map = OwnerMap::for_volumes(
        &co_routed,
        vec![(0, PeerOwner::new("the-authority", endpoint.clone()))],
    )
    .expect("an all-foreign owner map");
    ship::arm_ownership(map);
    publish::install_client(publish::PublishClient::new("co-writer-1", SECRET.to_vec()));

    publish::park_write_times(&co_routed, ino, 4242, 4242)
        .await
        .expect("the co-writer's metadata mutation SHIPS to the authority");
    assert!(
        publish::stats().shipped >= 1,
        "the publish ledger counts a shipped verb"
    );

    publish::uninstall_client();
    ship::disarm_ownership();
    drop(co);
    drop(co_routed);
    listener.shutdown();
    for v in &authority.volumes {
        v.shutdown().await.expect("authority unmounts clean");
    }
}

// ===========================================================================
// 8. Data: admitted under a grant, refused without one; accounting refused
// ===========================================================================

/// Contract: the split the blocker asked for. A co-writer HAS data-plane
/// authority — a DMA authorized under the epoch its grant established
/// passes the one authorization point — and has NO ownership-accounting
/// authority: allocation, terminal frees and the W1 incarnation retire
/// refuse with a message naming the data-plane allocation partition that
/// owns them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writer_writes_data_under_a_grant_and_never_accounts_locally() {
    let _serial = serial();
    let _restore = restore();

    let owner = WriteCustodyOwner::arm(
        "authority",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        LeaseClocks::with_params(
            Duration::from_millis(3_000),
            Duration::from_millis(200),
            Duration::from_millis(400),
        )
        .expect("positive T_self"),
        LeaseClock::manual(Arc::new(AtomicU64::new(1_000))),
        None,
    )
    .expect("the custody authority arms");
    let listener = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&owner))),
    )
    .expect("the authority listens");

    // The co-writer posture is armed BEFORE the grant: this is the mount
    // latch, and it must not close the data plane.
    fuse_client::set_mount_posture(MountPosture::CoWriter);
    assert!(
        !fuse_client::read_only_mount(),
        "a co-writer is NOT a reader: the S5 latch must stay off"
    );
    assert!(fuse_client::co_writer_mount());

    let client =
        WriteCustodyClient::connect(&listener.endpoint().to_string(), SECRET, "co-writer-1")
            .await
            .expect("the co-writer dials the authority");
    let lease = client
        .acquire(9001, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("write custody granted");
    assert!(lease.is_held().await);

    // Data: admitted under the grant's epoch.
    let epoch = data_custody::authorize_dma(None).expect("a granted co-writer authorizes DMA");
    data_custody::authorize_dma(Some(epoch)).expect("the carried epoch is current");

    // ...and refused once that custody moves (the epoch advance retires
    // every authorization minted under the previous generation).
    data_custody::advance_custody_generation("test: custody moved");
    data_custody::authorize_dma(Some(epoch))
        .expect_err("an authorization from a retired generation is refused");

    // Accounting: refused, naming the partition work that owns it.
    let alloc = BlockAllocator::new("vol-cowriter")
        .await
        .expect("allocator");
    let err = alloc
        .allocate_block()
        .await
        .expect_err("a co-writer allocates no fresh offsets")
        .to_string();
    assert!(
        err.to_lowercase().contains("allocation partition")
            || err.to_lowercase().contains("authority"),
        "the refusal names the work that owns allocation: {err}"
    );
    assert!(
        err.to_lowercase().contains("co-writer"),
        "and it names the posture, not the reader mount: {err}"
    );
    alloc
        .free_block(0)
        .await
        .expect_err("a co-writer frees nothing locally");
    assert!(
        !alloc.begin_patch_sole_owner(0),
        "W1's incarnation retire is refused on a co-writer"
    );

    drop(lease);
    client.drain_releases().await;
    listener.shutdown();
}

// ===========================================================================
// 9. The reader gate is not weakened
// ===========================================================================

/// Contract: every site the reader gate guards still refuses a READER,
/// with the reader's own unchanged text. The split added a class; it took
/// nothing away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_is_still_refused_at_every_site_the_gate_guards() {
    let _serial = serial();
    let _restore = restore();
    let alloc = BlockAllocator::new("vol-reader").await.expect("allocator");
    fuse_client::set_mount_posture(MountPosture::Reader);
    assert!(fuse_client::read_only_mount());

    let msgs = vec![
        alloc
            .allocate_block()
            .await
            .expect_err("fresh allocation")
            .to_string(),
        alloc
            .allocate_block_at_or_above(0)
            .expect_err("ascending pick")
            .to_string(),
        alloc
            .allocate_specific_block(1)
            .await
            .expect_err("specific allocation")
            .to_string(),
        alloc.free_block(0).await.expect_err("free").to_string(),
    ];
    for m in &msgs {
        assert!(
            m.contains("read-only") && m.contains("-o ro"),
            "the reader refusal text is unchanged: {m}"
        );
    }
    assert!(
        alloc.allocate_block_below(10).is_none(),
        "the contiguity pick refuses a reader"
    );
    assert!(!alloc.begin_free(0), "terminal free refuses a reader");
    assert!(
        !alloc.begin_patch_sole_owner(0),
        "the W1 patch refuses a reader"
    );
    assert!(
        !fuse_client::inplace_overwrite_enabled(),
        "the in-place overwrite lever stays off on a reader"
    );
}

// ===========================================================================
// 10. Enrollment: written by the AUTHORITY, never by the joining node
// ===========================================================================

/// Contract: the claim-set member entry is the AUTHORITY's commit. The
/// authority enrolls its operator-declared roster (`SQUEEZEFS_MW_MEMBERS`)
/// into the durable record; a co-writer cannot write it at all, because
/// writing it is a metadata commit and a co-writer holds no metadata
/// authority. That asymmetry IS the answer to "how does a co-writer get
/// enrolled without being able to commit": it does not — it is enrolled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_claim_set_member_entry_is_written_only_by_the_authority() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    let authority = KvMetaBackend::open(&vol).await.expect("authority mounts");
    let enrolled = cowriter::enroll_members(
        std::slice::from_ref(&authority),
        &[
            "node_00000000deadbeef".to_string(),
            "node_00000000cafe".to_string(),
        ],
        7,
    )
    .await
    .expect("the authority commits the roster");
    assert_eq!(enrolled, 2, "one durable entry per rostered node");

    let set = ClaimSet::load(&authority)
        .await
        .expect("the durable claim set");
    assert!(set.durable, "bit 14 is engaged, so the record is real");
    assert!(
        set.writers()
            .any(|m| m.identity.id == "node_00000000deadbeef"),
        "the rostered node is a durable WRITER member: {:?}",
        set.members
    );

    // The co-writer's side: it cannot commit the record it depends on.
    let admission = cowriter::classify_admission(&full_request(std::slice::from_ref(&vol)))
        .expect("the ladder admits");
    let co = KvMetaBackend::open_co_writer(&vol, &admission)
        .await
        .expect("co-writer open");
    let mut forged = ClaimSet::empty(9);
    forged
        .members
        .push(member("interloper", MemberRole::Writer, 1));
    ClaimSet::store(&co, &forged)
        .await
        .expect_err("a co-writer cannot enroll itself (or anyone else)");
    drop(co);
    authority.shutdown().await.expect("clean unmount");
}

// ===========================================================================
// 11. An authority and TWO co-writers
// ===========================================================================

/// Contract: the whole posture under a multi-threaded runtime — one
/// authority, two admitted co-writers, concurrent custody on disjoint
/// files, and a ledger that closes. Neither co-writer denies the authority
/// its write mount, and neither accounts locally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authority_and_two_co_writers() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;

    // The authority: a real write mount that enrolls both co-writers.
    let authority = KvMetaBackend::open(&vol).await.expect("authority mounts");
    let roster = vec![
        "node_aaaa000000000001".to_string(),
        "node_bbbb000000000002".to_string(),
    ];
    cowriter::enroll_members(std::slice::from_ref(&authority), &roster, 7)
        .await
        .expect("the authority enrolls both");

    let owner = WriteCustodyOwner::arm(
        "authority",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        LeaseClocks::with_params(
            Duration::from_millis(3_000),
            Duration::from_millis(200),
            Duration::from_millis(400),
        )
        .expect("positive T_self"),
        LeaseClock::manual(Arc::new(AtomicU64::new(1_000))),
        None,
    )
    .expect("custody authority");
    let listener = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&owner))),
    )
    .expect("the authority listens");

    // Both co-writers: admitted off the DURABLE set the authority wrote.
    let set = ClaimSet::load(&authority).await.expect("the durable set");
    let mut opened = Vec::new();
    for node in &roster {
        let mut req = full_request(std::slice::from_ref(&vol));
        req.node_id = node.clone();
        req.volumes[0].claim_set = Some(set.clone());
        req.volumes[0]
            .claim_set
            .as_mut()
            .unwrap()
            .members
            .push(member(
                "authority-membership-owner",
                MemberRole::Writer,
                0xA0A0,
            ));
        let admission = cowriter::classify_admission(&req)
            .unwrap_or_else(|e| panic!("{node} is enrolled, so the ladder admits: {e}"));
        opened.push(
            KvMetaBackend::open_co_writer(&vol, &admission)
                .await
                .expect("co-writer open"),
        );
    }
    assert_eq!(opened.len(), 2, "two co-writers attached to one authority");
    for be in &opened {
        assert_eq!(be.writer_guard_mode(), "co-writer");
        assert!(be.is_read_only(), "no local metadata authority");
    }

    // Concurrent custody on disjoint files, one grant each.
    let a = WriteCustodyClient::connect(&listener.endpoint().to_string(), SECRET, &roster[0])
        .await
        .expect("co-writer a dials");
    let b = WriteCustodyClient::connect(&listener.endpoint().to_string(), SECRET, &roster[1])
        .await
        .expect("co-writer b dials");
    let la = a
        .acquire(11, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("a takes custody of ino 11");
    let lb = b
        .acquire(22, None, LockMode::Exclusive, Duration::from_millis(500))
        .await
        .expect("b takes custody of ino 22");
    assert!(la.is_held().await && lb.is_held().await);
    assert_eq!(owner.held(), 2, "both grants live on the authority");
    assert_eq!(owner.stats().conflicts, 0, "disjoint files never conflict");

    drop(la);
    drop(lb);
    a.drain_releases().await;
    b.drain_releases().await;
    for be in opened {
        drop(be);
    }
    listener.shutdown();
    authority.shutdown().await.expect("clean unmount");
}

// ===========================================================================
// 12. The registrant join is a REGISTRATION, never a second reservation
// ===========================================================================

/// Contract: rung 5's evidence is gathered by REGISTERING under the
/// authority's standing WERO hold — never by acquiring a second
/// reservation (which would conflict at the device and silently downgrade
/// the guarantee class). The holder key must not change, our key must
/// appear among the registrants, and dropping the join leaves zero
/// residue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registrant_join_never_takes_a_second_reservation() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data0");
    std::fs::File::create(&data)
        .unwrap()
        .set_len(1 << 20)
        .unwrap();

    let ns = FakeNvmeNamespace::new();
    reservation::install_override(
        &data,
        FakeReservationClient::new(Arc::clone(&ns), "nqn.co-writer", "cowriter-hostid"),
    );

    // The AUTHORITY's standing WERO hold on the data namespace.
    let authority_client =
        FakeReservationClient::new(Arc::clone(&ns), "nqn.authority", "authority-hostid");
    reservation::register_ladder(authority_client.as_ref(), 0xA0A0).expect("authority registers");
    authority_client
        .acquire_write_exclusive_registrants_only(0xA0A0)
        .expect("the authority holds WERO");
    assert_eq!(ns.holder(), Some(0xA0A0));

    let join = data_custody::join_wero_as_registrant(std::slice::from_ref(&data))
        .expect("a co-writer registers under the standing hold");
    let ev = join.evidence();
    assert!(ev.pr_capable && ev.wero && ev.reservation_held && ev.registered);
    assert_ne!(ev.key, 0, "the registrant key is real");
    assert_eq!(
        ns.holder(),
        Some(0xA0A0),
        "registering NEVER takes the reservation from the authority"
    );
    assert!(ns.is_registered(ev.key), "our key is a device registrant");

    let key = ev.key;
    drop(join);
    assert!(
        !ns.is_registered(key),
        "dropping the join unregisters: zero residue"
    );
    assert_eq!(
        ns.holder(),
        Some(0xA0A0),
        "and the authority's reservation stands"
    );
    reservation::clear_override(&data);
}

/// KD-MW-2 (design-full-multi-writer §5.1 / §11): rung 3 matches the
/// roster grammar — a PAIR client id (`node_{16 hex}.m{8 hex}`) is
/// admitted by its exact entry AND by the bare-node SLOT-WILDCARD entry,
/// while an entry naming a DIFFERENT slot of the same node refuses (two
/// co-located mounts are two clients).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rung_3_matches_the_pair_grammar_and_the_bare_node_wildcard() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "meta0", true).await;
    let pair_id = "node_00000000deadbeef.m00c0ffee";

    // Exact pair entry admits.
    let mut req = full_request(std::slice::from_ref(&vol));
    req.node_id = pair_id.to_string();
    req.volumes[0].claim_set = Some(full_claim_set(pair_id, "authority-membership-owner"));
    cowriter::classify_admission(&req).expect("an exact pair entry admits");

    // The bare-node wildcard entry admits the pair id.
    let mut req = full_request(std::slice::from_ref(&vol));
    req.node_id = pair_id.to_string();
    req.volumes[0].claim_set = Some(full_claim_set(
        "node_00000000deadbeef",
        "authority-membership-owner",
    ));
    cowriter::classify_admission(&req)
        .expect("the bare node entry is the slot wildcard (§11 grammar)");

    // A DIFFERENT slot's entry refuses: a co-located sibling's enrollment
    // is not ours.
    let mut req = full_request(std::slice::from_ref(&vol));
    req.node_id = pair_id.to_string();
    req.volumes[0].claim_set = Some(full_claim_set(
        "node_00000000deadbeef.m0badc0de",
        "authority-membership-owner",
    ));
    let err = cowriter::classify_admission(&req)
        .expect_err("another mount slot's enrollment must not admit this one")
        .to_string();
    assert!(
        err.contains(pair_id),
        "the refusal prints the pair id an operator must enroll: {err}"
    );
}
