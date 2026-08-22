//! **Per-volume claim admission — the partial-writer open**
//! (`docs/design-per-volume-claim-admission.md` §5.1/§5.4, PR 4; the
//! decision is `src/partial_authority.rs`, PR 3).
//!
//! # The headline pin, and why it is the FIRST thing in this file
//!
//! This is the rung at which the program first touches the **shipped D0
//! mount path**, and risk **R12** is the whole reason the ladder ships
//! dark:
//!
//! > *the SOLO RE-GATE LAW — a mount that has not DECLARED a per-volume
//! > posture must meet the pre-program `FreshForeign` refusal, byte for
//! > byte, on a set whose volumes name owners.*
//!
//! `the_fresh_foreign_refusal_is_byte_identical_for_an_undeclared_mount`
//! asserts that against a **frozen literal** rather than against the
//! product's own format string, because a test that re-derives the text
//! from the code it guards proves nothing about the text.
//!
//! # What else this file pins
//!
//! * the `PeerAuthority` arm is **structurally unreachable** without a
//!   [`squeezefs::partial_authority::SetAdmission`] — an admission decided
//!   over a different set is refused at the door (the `open_co_writer`
//!   precedent), and no `Peer`-mode open takes a lock, writes a claim, or
//!   spawns a task;
//! * the rollback ladder releases **exactly** the guards it took;
//! * a peer-owned volume's write gate refuses with its own cause
//!   (`PeerOwnedVolume`), never the co-writer's — whose text is false for a
//!   partial authority and whose must-stay-0 counter would rot.

use squeezefs::cowriter::{AuthorityLeaseEvidence, MwRole, RegistrantEvidence};
use squeezefs::fuse_client::METRICS;
use squeezefs::membership::{ClaimHolder, ClaimSet, ClaimSetMember, MemberIdentity, MemberRole};
use squeezefs::meta_backend::kv::backend::{
    KvMetaBackend, ReadOnlyCause, WriterClaim, WRITER_CLAIM_XATTR,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::partial_authority::{
    self as pv, ClaimStanding, PvVolumeEvidence, SetAdmission, SetAdmissionRequest,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;

/// This node's durable enrollment identity (KD-MW-2).
const NODE: &str = "node_00000000deadbeef.m00000001";
/// The peer that appends to the volume in the assigned fixtures.
const PEER: &str = "node_00000000feedface.m00000001";

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// The nine-bit multi-writer stamp — `volume enable-multi-writer`'s act,
/// offline, between format and open (the co-writer suite's helper
/// verbatim: bit 11 set ⇒ all nine set is a writable-mount invariant).
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

async fn fresh_volume(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    format_v3(&p, VOL_LEN, &opts()).await.unwrap();
    stamp_capabilities(&p).await;
    p
}

/// Whole-volume digest — the "wrote nothing" assertion's instrument.
fn digest(path: &Path) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&std::fs::read(path).expect("read the whole volume"))
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
            // KD-PV-4: the offline verb enrolls the PID-LESS roster form.
            pid: 0,
            boot: String::new(),
            endpoint: None,
            pr_key: 0,
        },
        ts: 1_700_000_000,
    }
}

/// A live FOREIGN claim (another host's boot id, so no dead-pid proof can
/// reclaim it) whose age is pinned at 0 for the frozen refusal literal.
fn foreign_claim() -> WriterClaim {
    WriterClaim {
        id: "5f1d0e2a-0000-4000-8000-000000000001".to_string(),
        // Two seconds ahead so `age_secs` saturates to 0 for the whole
        // test rather than racing a second boundary.
        ts: now_secs() + 2,
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term: 7,
    }
}

/// Plant a fresh foreign `writer_claim` and — when `assign` — a durable
/// `claim_set` naming `PEER` as this volume's OWNER and this node as an
/// enrolled writer member: the exact shape an assigned multi-owner set
/// presents to a mount that declared nothing.
async fn plant(path: &Path, claim: &WriterClaim, assign: bool) {
    let be = KvMetaBackend::open(path).await.expect("planting open");
    if assign {
        let mut set = ClaimSet::empty(7);
        set.durable = true;
        set.owner = Some(PEER.to_string());
        set.members = vec![member(NODE), member(PEER)];
        ClaimSet::store(&be, &set).await.expect("store claim set");
    }
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
        .await
        .expect("plant claim");
    be.sync_device().await.expect("barrier");
    // DROP, never `shutdown()`: a clean shutdown releases the claim, and
    // what this fixture needs on disk is a LIVE foreign holder's record
    // (the co-writer suite's `forge_foreign_claim` shape).
    drop(be);
}

/// **R12 — the solo re-gate law.** The pre-program refusal text, frozen
/// here as a literal so a change to the product's format string fails this
/// test instead of silently rewriting the contract.
fn expected_fresh_foreign_refusal(path: &Path, claim: &WriterClaim) -> String {
    format!(
        "{}: metadata volume is claimed by a live writer (claim: id={}, pid={}, boot={}, \
         age=0s) — concurrent mounts of one metadata volume are refused (single-writer \
         guard). A crashed holder on THIS host is reclaimed automatically once its pid is \
         provably dead; otherwise stop that writer or wait for its claim to expire (ttl 45s)",
        path.display(),
        claim.id,
        claim.pid,
        claim.boot,
    )
}

/// **THE headline pin of PR 4** (risk R12, `docs/design-…§5.13`'s
/// must-stay list): per-volume claim admission is a **different door**,
/// never a hole in the D0 gate. An undeclared mount — which is every mount
/// that ships — meets the same `FreshForeign` refusal it always has, with
/// the same text, on a plain set AND on a set whose durable `claim_set`
/// assigns this very volume to the live holder and enrolls this node as a
/// writer member of it.
///
/// The second arm is the one that matters: every ingredient the
/// `PeerAuthority` arm consumes is present and the arm must still not be
/// taken, because the posture was not DECLARED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fresh_foreign_refusal_is_byte_identical_for_an_undeclared_mount() {
    let dir = TempDir::new().unwrap();

    for (name, assign) in [("plain", false), ("assigned", true)] {
        let vol = fresh_volume(dir.path(), &format!("r12-{name}")).await;
        let claim = foreign_claim();
        plant(&vol, &claim, assign).await;

        let err = KvMetaBackend::open(&vol)
            .await
            .err()
            .unwrap_or_else(|| panic!("[{name}] a fresh foreign claim must refuse the D0 open"));
        assert_eq!(
            err.to_string(),
            expected_fresh_foreign_refusal(&vol, &claim),
            "[{name}] the FreshForeign refusal is not byte-identical for an undeclared mount \
             (R12: per-volume claim admission must be a different DOOR, not a weakening)"
        );

        // The whole-set path refuses identically and rolls the set back:
        // the routed open is what a mount actually calls.
        let set_err = squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
            .await
            .err()
            .unwrap_or_else(|| panic!("[{name}] the routed set open must refuse too"));
        assert!(
            set_err
                .to_string()
                .contains("metadata volume is claimed by a live writer"),
            "[{name}] the routed set open's refusal changed: {set_err}"
        );
    }
}

// ---------------------------------------------------------------------------
// The partial open's per-volume evidence and the admission the ladder takes
// over it (`src/partial_authority.rs`, PR 3 — the ONLY `SetAdmission`
// constructor, which is what makes `open_peer_owned` unreachable without a
// decision)
// ---------------------------------------------------------------------------

/// A two-volume set: `vol0` hosts slot 0 and is appended to by `PEER`,
/// `vol1` is this node's. That is the smallest shape carrying BOTH modes,
/// which is what the rollback ladder and the posture rows are about.
async fn two_volume_set(dir: &Path, tag: &str) -> (PathBuf, PathBuf) {
    let plan = squeezefs::meta_backend::plan_meta_slot_set(2).expect("derived slot plan");
    let mut out = Vec::new();
    for (i, stamp) in plan.stamps.iter().enumerate() {
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
    (out[0].clone(), out[1].clone())
}

/// The durable `vol-{hex}` identity of a real volume, as the mount path
/// derives it (KD-5: never a path, an ordinal or a set position).
async fn vol_id(path: &Path) -> String {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    probe.durable_volume_id()
}

/// The durable claim set of a peer-owned volume: `PEER` owns it, both
/// nodes are enrolled, and — the mechanism PR 4 supplies for §5.1.1's
/// `recognizes` clause (a) — the live holder's ATTESTATION binds the
/// per-mount claim to `PEER`'s durable member id.
fn peer_owned_set(claim: &WriterClaim, attest: bool) -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.owner = Some(PEER.to_string());
    set.members = vec![member(NODE), member(PEER)];
    if attest {
        set.holder = Some(ClaimHolder {
            id: PEER.to_string(),
            writer_id: claim.id.clone(),
            pid: claim.pid,
            boot: claim.boot.clone(),
        });
    }
    set
}

fn own_set() -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.owner = Some(NODE.to_string());
    set.members = vec![member(NODE), member(PEER)];
    set
}

async fn store_set(path: &Path, set: &ClaimSet) {
    let be = KvMetaBackend::open(path).await.expect("store open");
    ClaimSet::store(&be, set).await.expect("store claim set");
    be.sync_device().await.expect("barrier");
    be.shutdown().await.expect("release");
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
        owner_endpoint: Some("127.0.0.1:7100".to_string()),
    }
}

/// The admission a partial authority takes over the two-volume set: `vol0`
/// (slot 0) is `PEER`'s, `vol1` is ours.
fn partial_admission(
    vol0: &Path,
    id0: &str,
    claim0: &WriterClaim,
    vol1: &Path,
    id1: &str,
) -> SetAdmission {
    let req = SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::PartialAuthority,
        read_only: false,
        node_id: NODE.to_string(),
        set_authority_endpoint: Some("127.0.0.1:7100".to_string()),
        volumes: vec![
            evidence(
                vol0,
                id0,
                true,
                Some(claim0.clone()),
                Some(PEER),
                peer_owned_set(claim0, true),
            ),
            evidence(vol1, id1, false, None, None, own_set()),
        ],
        authority: Some(AuthorityLeaseEvidence {
            owner_id: PEER.to_string(),
            endpoint: "127.0.0.1:7100".to_string(),
            owner_claim_id: PEER.to_string(),
            term: 7,
            live: true,
            member_epoch: 3,
        }),
        registrant: Some(RegistrantEvidence {
            pr_capable: true,
            wero: true,
            reservation_held: true,
            registered: true,
            key: 0xB0B0,
            namespaces: 1,
        }),
    };
    pv::classify_set_admission(&req).expect("the seven-rung ladder admits this fixture")
}

/// Sweep rows 1/5/7 and §5.1.2: a `Peer`-mode open takes Layer A only as a
/// released probe, writes nothing at all, spawns neither the checkpoint nor
/// the times-drain task, and reports its own guarantee class — so the
/// volume's owner is denied nothing and the bytes are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_owned_open_takes_no_guard_writes_nothing_and_spawns_no_task() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-open").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let claim = foreign_claim();
    store_set(&vol0, &peer_owned_set(&claim, true)).await;
    plant(&vol0, &claim, false).await;
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);

    let before = digest(&vol0);
    let be = KvMetaBackend::open_peer_owned(&vol0, &admission, &id0)
        .await
        .expect("a peer-owned open is admitted by the decision");
    assert_eq!(
        be.read_only_cause(),
        ReadOnlyCause::PeerOwnedVolume,
        "a peer-owned volume must carry its OWN cause: reusing CoWriterMount would rot both \
         its refusal text and the meaning of cowriter_local_commit_refusals (§5.1.2)"
    );
    assert_eq!(be.writer_guard_mode(), "peer-owned");
    let trace = be.open_trace();
    assert!(
        !trace.contains(&"flock_acquired")
            && !trace.contains(&"claim_committed")
            && !trace.contains(&"checkpoint_task_spawned"),
        "a peer-owned open must take no Layer-A lock, write no claim and spawn no checkpoint \
         task (sweep rows 1/5): {trace:?}"
    );
    drop(be);
    assert_eq!(
        digest(&vol0),
        before,
        "a peer-owned open wrote to a volume it does not append to"
    );

    // The owner is denied nothing: the D0 ladder still grants the claim to
    // a mount that can prove the holder dead — here, our own residue path.
    std::fs::remove_file(dir.path().join("peer-open-meta0")).ok();
}

/// §5.1.2 + §11.1: the write gate's `PeerOwnedVolume` arm is DISTINCT from
/// the co-writer's — its own text (a partial authority does hold metadata
/// authority, just not over THIS volume) and its own must-stay-0 counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_owned_write_gate_refuses_with_its_own_cause_and_counter() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-gate").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let claim = foreign_claim();
    store_set(&vol0, &peer_owned_set(&claim, true)).await;
    plant(&vol0, &claim, false).await;
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);
    let be = KvMetaBackend::open_peer_owned(&vol0, &admission, &id0)
        .await
        .expect("admitted");

    let peer_before = METRICS
        .peer_volume_local_commit_refusals
        .load(Ordering::Relaxed);
    let cw_before = METRICS
        .cowriter_local_commit_refusals
        .load(Ordering::Relaxed);
    let err = be
        .setxattr_internal(1, "user.pv-probe", b"x")
        .await
        .expect_err("a metadata mutation on a peer-owned volume must refuse");
    let text = err.to_string();
    assert!(
        text.contains("PEER authority of this set") && text.contains("SHIP"),
        "the refusal must name the shipped path and the peer authority, not a mount option: \
         {text}"
    );
    assert!(
        !text.contains("CO-WRITER"),
        "the co-writer text is FALSE for a partial authority — it holds metadata authority \
         over the volumes it owns: {text}"
    );
    assert_eq!(
        METRICS
            .peer_volume_local_commit_refusals
            .load(Ordering::Relaxed),
        peer_before + 1,
        "peer_volume_local_commit_refusals is the must-stay-0 tripwire for this class"
    );
    assert_eq!(
        METRICS
            .cowriter_local_commit_refusals
            .load(Ordering::Relaxed),
        cw_before,
        "a peer-owned refusal must never move the co-writer counter (their meanings differ)"
    );
}

/// Sweep row 7: the claim heartbeat self-skips on a peer-owned volume —
/// `guard_fd.is_none() || read_only` is already true, so nothing refreshes
/// a claim this mount did not write. Pinned, not changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_heartbeat_self_skips_on_a_peer_owned_volume() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-beat").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let claim = foreign_claim();
    store_set(&vol0, &peer_owned_set(&claim, true)).await;
    plant(&vol0, &claim, false).await;
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);
    let be = KvMetaBackend::open_peer_owned(&vol0, &admission, &id0)
        .await
        .expect("admitted");

    let before = digest(&vol0);
    be.guard_heartbeat().await;
    let held = be
        .read_writer_claim()
        .await
        .expect("the peer's claim is still there");
    assert_eq!(
        held.id, claim.id,
        "the heartbeat rewrote a PEER's claim — that would make this mount the appender of a \
         volume it is not assigned"
    );
    drop(be);
    assert_eq!(
        digest(&vol0),
        before,
        "the heartbeat wrote to a peer volume"
    );
}

/// KD-PV-3's *never adopt on silence*, at the OPEN: a peer volume whose
/// live holder cannot be resolved to a durable member id refuses, even
/// though an admission names the volume. The claim's `id` is a per-mount
/// uuid, so an unattested claim proves nothing about WHO is appending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unattested_holder_refuses_the_peer_owned_open() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-silence").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let claim = foreign_claim();
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);
    // On disk: the assignment, but NO holder attestation.
    store_set(&vol0, &peer_owned_set(&claim, false)).await;
    plant(&vol0, &claim, false).await;

    let err = KvMetaBackend::open_peer_owned(&vol0, &admission, &id0)
        .await
        .expect_err("an unresolvable holder is SILENCE and must refuse");
    assert!(
        err.to_string().contains("durable member id"),
        "the refusal must name the identity it could not resolve: {err}"
    );
}

/// §5.1.1's `Peer` + `Reclaimable` row: an assigned peer owner that is not
/// claiming means a dead owner, and the mount refuses loud rather than
/// serving a set with a volume no node appends to (R13's visible face).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_owned_volume_with_no_live_claim_refuses_the_open() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-dead").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let claim = foreign_claim();
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);
    // The assignment stands; the owner's claim is gone (it died between
    // the gather and the open).
    store_set(&vol0, &peer_owned_set(&claim, true)).await;

    let before = METRICS
        .peer_volume_unclaimed_refusals
        .load(Ordering::Relaxed);
    let err = KvMetaBackend::open_peer_owned(&vol0, &admission, &id0)
        .await
        .expect_err("a peer-owned volume with no appender must refuse the mount");
    assert!(
        err.to_string().contains("volume set-owners"),
        "the refusal must name the operator remedy: {err}"
    );
    assert_eq!(
        METRICS
            .peer_volume_unclaimed_refusals
            .load(Ordering::Relaxed),
        before + 1
    );
}

/// The `open_co_writer` precedent: an admission decided over a DIFFERENT
/// set may never open a volume of this one, and a volume the decision does
/// not name is a refusal rather than a default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_owned_open_refuses_an_admission_that_does_not_name_the_volume() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-foreign").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let claim = foreign_claim();
    store_set(&vol0, &peer_owned_set(&claim, true)).await;
    plant(&vol0, &claim, false).await;
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);

    let foreign = dir.path().join("not-in-the-set");
    std::fs::File::create(&foreign)
        .unwrap()
        .set_len(4096)
        .unwrap();
    let err = KvMetaBackend::open_peer_owned(&foreign, &admission, &id0)
        .await
        .expect_err("an admission is per-SET and may never be carried across sets");
    assert!(
        err.to_string().contains("DIFFERENT volume set"),
        "the refusal must say the admission was decided elsewhere: {err}"
    );

    // And the OWN-mode volume is not openable through the peer door.
    let err = KvMetaBackend::open_peer_owned(&vol1, &admission, &id1)
        .await
        .expect_err("an own-mode volume must run the full D0 ladder, never the peer door");
    assert!(
        err.to_string().contains("this node's OWN"),
        "the refusal must name the mode the decision actually took: {err}"
    );
}

// ---------------------------------------------------------------------------
// The SET-level partial open (§5.4 sweep rows 2/3/4/18): the rollback
// ladder, the `owner_assign:` probe, and ownership-scoped intent recovery
// ---------------------------------------------------------------------------

/// The set-authority admission over the two-volume set: `vol0` (slot 0) is
/// OURS — which under D20 makes this mount the set authority — and `vol1`
/// belongs to `PEER`. Opening in canonical order therefore takes a real D0
/// guard BEFORE it reaches the volume that refuses, which is the only shape
/// in which a rollback ladder has anything to release.
fn set_authority_admission(
    vol0: &Path,
    id0: &str,
    vol1: &Path,
    id1: &str,
    claim1: Option<&WriterClaim>,
) -> SetAdmission {
    let peer_set = claim1
        .map(|c| peer_owned_set(c, true))
        .unwrap_or_else(|| peer_owned_set(&foreign_claim(), false));
    let req = SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::SetAuthority,
        read_only: false,
        node_id: NODE.to_string(),
        set_authority_endpoint: None,
        volumes: vec![
            evidence(vol0, id0, true, None, None, own_set()),
            evidence(
                vol1,
                id1,
                false,
                Some(claim1.cloned().unwrap_or_else(foreign_claim)),
                Some(PEER),
                peer_set,
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

/// §5.4: **the rollback ladder releases exactly the guards it took.** The
/// own volume's Layer-A flock, its `writer_claim` and its reservation are
/// released when a later volume refuses; the peer volume — which took no
/// guard at all — has nothing to release and is not written to either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partial_set_open_failure_releases_exactly_the_owned_volumes_guards() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "rollback").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    store_set(&vol0, &own_set()).await;
    // The peer volume is ASSIGNED but unclaimed: its owner is dead, which
    // §5.1.1 makes a loud refusal rather than a degraded serve.
    store_set(&vol1, &peer_owned_set(&foreign_claim(), true)).await;
    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, None);

    let peer_before = digest(&vol1);
    let err = squeezefs::meta_backend::open_meta_volume_set_partial(
        &[vol0.display().to_string(), vol1.display().to_string()],
        &[id0.clone(), id1.clone()],
        &admission,
    )
    .await
    .expect_err("a peer volume with no appender refuses the whole set open");
    assert!(
        err.to_string().contains("NOTHING claims it"),
        "the set open must propagate the volume's own refusal: {err}"
    );

    // Exactly what it took: the own volume's guard is FREE again, proven
    // by taking it (the flock is the guard's own instrument).
    let reopened = KvMetaBackend::open(&vol0)
        .await
        .expect("the owned volume's Layer-A flock and claim were released by the rollback");
    reopened.shutdown().await.expect("release");
    assert_eq!(
        digest(&vol1),
        peer_before,
        "the rollback wrote to a peer-owned volume — a Peer-mode backend's shutdown must be a \
         no-op because it took nothing"
    );
}

/// Sweep row 2: the `owner_assign:` bracket is the `mw_upgrade:` mechanism
/// verbatim — a writable mount refuses while it exists, in BOTH the
/// ordinary routed open and the partial twin, naming the idempotent re-run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn any_writable_mount_refuses_while_an_owner_assign_bracket_is_open() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "assign-marker").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    store_set(&vol0, &own_set()).await;
    store_set(&vol1, &peer_owned_set(&foreign_claim(), true)).await;
    {
        let be = KvMetaBackend::open(&vol0).await.expect("marker open");
        be.setxattr_internal(
            1,
            squeezefs::OWNER_ASSIGN_MARKER_XATTR,
            &squeezefs::config_ops::OwnerAssignMarker {
                assignments: vec![squeezefs::config_ops::OwnerAssignment {
                    volume_id: id0.clone(),
                    owner: Some(NODE.to_string()),
                    successors: Vec::new(),
                }],
            }
            .encode(),
        )
        .await
        .expect("plant the bracket");
        be.sync_device().await.expect("barrier");
        be.shutdown().await.expect("release");
    }

    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let plain = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .err()
        .expect("an ordinary writable mount refuses mid-assignment");
    assert!(
        plain.to_string().contains("owner_assign:")
            && plain.to_string().contains("volume set-owners"),
        "the refusal must name the record and the idempotent re-run: {plain}"
    );

    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, None);
    let partial = squeezefs::meta_backend::open_routed_meta_set_partial(&uris, &admission)
        .await
        .err()
        .expect("the partial twin refuses the same way");
    assert!(
        partial.to_string().contains("owner_assign:"),
        "the partial open must run the sibling probe too: {partial}"
    );
}

/// Sweep row 3 / §5.4a case (c): an open intent whose steps span TWO
/// OWNERS refuses the mount loud and moves the must-stay-0
/// `xv_cross_owner_intents` counter — it is reachable-by-bug (M1 is what
/// keeps it 0), never rolled forward by a node that owns half of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_owner_open_intent_refuses_the_partial_mount() {
    use squeezefs::meta_backend::crossvol_tx::{intent_name, IntentRecord, XvOp, XvStep};
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "xv-cross").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    store_set(&vol0, &own_set()).await;
    let claim = foreign_claim();
    store_set(&vol1, &peer_owned_set(&claim, true)).await;
    plant(&vol1, &claim, false).await;

    // A plan whose first step homes on OUR volume (ino 1 → slot 0) and
    // whose second homes on the PEER's: the shape one ordinary `rm`
    // produced before M1 existed.
    let peer_ino = squeezefs::meta_backend::make_global_ino_width(
        1,
        7,
        u64::from(squeezefs::meta_backend::DERIVED_ROUTING_WIDTH),
    );
    let rec = IntentRecord {
        tx_id: 0x5151_5151_5151_5151,
        op: XvOp::Unlink,
        steps: vec![
            XvStep::RemoveDentry {
                parent: 1,
                name: "victim".to_string(),
                expect_child: peer_ino,
                parent_update: 0,
            },
            XvStep::SetNlink {
                ino: peer_ino,
                pre: 1,
                post: 0,
                ctime: None,
            },
        ],
    };
    {
        let be = KvMetaBackend::open(&vol0).await.expect("intent open");
        be.setxattr_internal(1, &intent_name(rec.tx_id), &rec.encode().unwrap())
            .await
            .expect("plant the intent");
        be.sync_device().await.expect("barrier");
        be.shutdown().await.expect("release");
    }

    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, Some(&claim));
    let before = METRICS.xv_cross_owner_intents.load(Ordering::Relaxed);
    let err = squeezefs::meta_backend::open_routed_meta_set_partial(
        &[vol0.display().to_string(), vol1.display().to_string()],
        &admission,
    )
    .await
    .expect_err("an intent spanning two owners must refuse the mount");
    assert!(
        err.to_string().contains("two metadata OWNERS"),
        "the refusal must say what makes the intent unrecoverable here: {err}"
    );
    assert_eq!(
        METRICS.xv_cross_owner_intents.load(Ordering::Relaxed),
        before + 1,
        "xv_cross_owner_intents is the must-stay-0 tripwire M1 exists to hold at zero"
    );
}

/// Sweep rows 3/4/18 plus KD-PV-17's publisher: a healthy partial mount
/// comes up, recovers only what it owns, covers bring-up residue on owned
/// volumes only, and ATTESTS itself as the holder of the claims it took —
/// which is what lets a peer resolve this node's per-mount uuid later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partial_routed_open_serves_and_attests_only_the_volumes_it_owns() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "routed-partial").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    store_set(&vol0, &own_set()).await;
    let claim = foreign_claim();
    store_set(&vol1, &peer_owned_set(&claim, true)).await;
    plant(&vol1, &claim, false).await;
    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, Some(&claim));

    let peer_before = digest(&vol1);
    let routed = squeezefs::meta_backend::open_routed_meta_set_partial(
        &[vol0.display().to_string(), vol1.display().to_string()],
        &admission,
    )
    .await
    .expect("a healthy partial set opens");
    assert_eq!(routed.volumes[0].writer_guard_mode(), "flock+claim");
    assert_eq!(routed.volumes[1].writer_guard_mode(), "peer-owned");

    // The attestation is on the OWNED volume and nowhere else.
    let attested = ClaimSet::load(&routed.volumes[0])
        .await
        .expect("the owned volume answers a claim set");
    let held = routed.volumes[0]
        .read_writer_claim()
        .await
        .expect("we hold the claim we took");
    assert_eq!(
        attested.resolve_holder(&held),
        Some(NODE),
        "an owned volume must attest THIS mount as its holder, or no peer can ever resolve \
         our per-mount uuid to a durable member id (KD-PV-17)"
    );
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
    assert_eq!(
        digest(&vol1),
        peer_before,
        "a partial mount wrote to a peer-owned volume"
    );
}
