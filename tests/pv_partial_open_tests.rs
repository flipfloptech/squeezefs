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
use squeezefs::meta_backend::Metadata;
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

/// §5.1.1's `Peer` + `Reclaimable` row, **corrected**: an assigned peer
/// owner that is not claiming has not started yet (or is down), and the
/// open ADMITS the volume degraded — read-only, unclaimed, unadopted —
/// rather than taking the whole namespace down over one absent owner. It
/// is the cold-start state every fleet passes through: the set authority
/// mounts first, by the documented order, with every peer volume
/// unclaimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_owned_volume_with_no_live_claim_opens_degraded() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-dead").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let claim = foreign_claim();
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);
    // The assignment stands; nothing claims the volume (its owner has not
    // mounted, or it died between the gather and the open).
    store_set(&vol0, &peer_owned_set(&claim, true)).await;

    let refused_before = METRICS
        .peer_volume_unclaimed_refusals
        .load(Ordering::Relaxed);
    let admitted_before = METRICS
        .peer_volume_unclaimed_admits
        .load(Ordering::Relaxed);
    let before = digest(&vol0);
    let be = KvMetaBackend::open_peer_owned(&vol0, &admission, &id0)
        .await
        .expect("a peer-owned volume with no appender opens DEGRADED, never refused");
    assert_eq!(
        be.read_only_cause(),
        ReadOnlyCause::PeerOwnedVolume,
        "the degraded open is still a peer-owned open — nothing about it appends"
    );
    let trace = be.open_trace();
    assert!(
        !trace.contains(&"flock_acquired") && !trace.contains(&"claim_committed"),
        "a degraded peer-owned open must NEVER adopt the volume it found unclaimed: {trace:?}"
    );
    drop(be);
    assert_eq!(
        digest(&vol0),
        before,
        "the degraded open wrote to a volume it does not append to"
    );
    assert!(
        KvMetaBackend::open_probe(&vol0)
            .await
            .expect("probe")
            .read_writer_claim()
            .await
            .is_none(),
        "the degraded open took the absent claim — that is the adoption KD-PV-3 forbids"
    );
    assert_eq!(
        METRICS
            .peer_volume_unclaimed_admits
            .load(Ordering::Relaxed),
        admitted_before + 1,
        "the degraded admission is COUNTED: an operator must be able to see that this set came \
         up with an owner missing"
    );
    assert_eq!(
        METRICS
            .peer_volume_unclaimed_refusals
            .load(Ordering::Relaxed),
        refused_before,
        "an absent claim is not a refusal any more"
    );
}

/// The other half of the correction: where a claim EXISTS it must still be
/// attributable to the admitted holder. A TTL-stale claim the volume's own
/// record cannot attribute (no KD-PV-17 attestation) refuses — silence
/// about WHO appended is evidence this mount cannot reconcile, and it is
/// counted by `peer_volume_unclaimed_refusals`, whose narrowed meaning is
/// exactly this class.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_peer_claim_admits_only_when_it_attributes_to_the_admitted_holder() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "peer-stale").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    // TTL-stale: older than CLIENT_STALE_TTL_SECS, from another host's
    // boot (so no dead-pid proof reclaims it).
    let mut claim = foreign_claim();
    claim.ts = now_secs() - 10_000;
    let admission = partial_admission(&vol0, &id0, &claim, &vol1, &id1);

    // (a) ATTESTED to the admitted holder: its owner died, the volume has
    // no live appender, and the open admits it degraded.
    store_set(&vol0, &peer_owned_set(&claim, true)).await;
    plant(&vol0, &claim, false).await;
    let admitted_before = METRICS
        .peer_volume_unclaimed_admits
        .load(Ordering::Relaxed);
    let be = KvMetaBackend::open_peer_owned(&vol0, &admission, &id0)
        .await
        .expect("a dead owner's own stale claim opens degraded");
    assert_eq!(be.read_only_cause(), ReadOnlyCause::PeerOwnedVolume);
    drop(be);
    assert_eq!(
        METRICS
            .peer_volume_unclaimed_admits
            .load(Ordering::Relaxed),
        admitted_before + 1
    );

    // (b) UNATTESTED: the same aged claim with nothing naming its holder.
    // The mount cannot say whether the assignment is being honoured, so it
    // fails closed exactly as the fresh arm does.
    let (vol2, _) = two_volume_set(dir.path(), "peer-stale-mute").await;
    let id2 = vol_id(&vol2).await;
    let admission = partial_admission(&vol2, &id2, &claim, &vol1, &id1);
    store_set(&vol2, &peer_owned_set(&claim, false)).await;
    plant(&vol2, &claim, false).await;
    let refused_before = METRICS
        .peer_volume_unclaimed_refusals
        .load(Ordering::Relaxed);
    let err = KvMetaBackend::open_peer_owned(&vol2, &admission, &id2)
        .await
        .expect_err("an unattributable claim must refuse");
    assert!(
        err.to_string().contains("volume set-owners"),
        "the refusal must name the operator remedy: {err}"
    );
    assert_eq!(
        METRICS
            .peer_volume_unclaimed_refusals
            .load(Ordering::Relaxed),
        refused_before + 1
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
    .err()
    .unwrap_or_else(|| panic!("a peer volume with no appender refuses the whole set open"));
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
    let claim = foreign_claim();
    store_set(&vol1, &peer_owned_set(&claim, true)).await;
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
    // The plain arm runs BEFORE the peer's claim is planted: with one
    // there, D0's own `FreshForeign` refusal (correctly) fires first.
    let plain = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .err()
        .unwrap_or_else(|| panic!("an ordinary writable mount refuses mid-assignment"));
    assert!(
        plain.to_string().contains("owner_assign:")
            && plain.to_string().contains("volume set-owners"),
        "the refusal must name the record and the idempotent re-run: {plain}"
    );

    plant(&vol1, &claim, false).await;
    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, Some(&claim));
    let partial = squeezefs::meta_backend::open_routed_meta_set_partial(&uris, &admission)
        .await
        .err()
        .unwrap_or_else(|| panic!("the partial twin refuses the same way"));
    assert!(
        partial.to_string().contains("owner_assign:"),
        "the partial open must run the sibling probe too: {partial}"
    );
}

/// Sweep row 3 / §5.4a case (c): an open intent whose steps span TWO
/// OWNERS refuses the mount loud and moves the must-stay-0
/// `xv_cross_owner_intents` counter — it is reachable-by-bug (M1 is what
/// keeps it 0), never rolled forward by a node that owns half of it.
///
/// The intent is produced by the REAL path — a genuine cross-volume
/// unlink severed at its commit-boundary seam after step 0 — because a
/// hand-planted record would prove the refusal reads a record, not that
/// the shape one ordinary `rm` produces is the shape that refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_owner_open_intent_refuses_the_partial_mount() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "xv-cross").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];

    {
        let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
            .await
            .expect("an ordinary write mount builds the fixture");
        let parent = mkdir_on(&routed, 1, 0, "xv").await;
        let (child, name) = mkfile_on(&routed, parent, 1, "xv").await;
        assert_ne!(
            routed.route_ino(parent).0,
            routed.route_ino(child).0,
            "the fixture must produce a CROSS-volume unlink"
        );
        // Window 2: the parent-side commit (which CARRIES the intent) is
        // durable, the child-side nlink step is not — a dead process, not
        // a device error.
        squeezefs::meta_backend::crossvol_tx::TEST_XV_SEAM_AFTER_STEPS.store(2, Ordering::Relaxed);
        let _ = routed.unlink(parent, &name).await;
        squeezefs::meta_backend::crossvol_tx::TEST_XV_SEAM_AFTER_STEPS.store(0, Ordering::Relaxed);
        for vol in &routed.volumes {
            vol.shutdown().await.expect("release the fixture's guards");
        }
    }

    let claim = foreign_claim();
    store_set(&vol0, &own_set()).await;
    store_set(&vol1, &peer_owned_set(&claim, true)).await;
    plant(&vol1, &claim, false).await;

    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, Some(&claim));
    let before = METRICS.xv_cross_owner_intents.load(Ordering::Relaxed);
    let err = squeezefs::meta_backend::open_routed_meta_set_partial(&uris, &admission)
        .await
        .err()
        .unwrap_or_else(|| panic!("an intent spanning two owners must refuse the mount"));
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

/// A directory under `parent` whose ino routes to `want_vol` (the
/// crossvol suite's helper — placement is a health round-robin, so the
/// fixture asks for the volume it needs rather than hoping).
async fn mkdir_on(
    routed: &squeezefs::meta_backend::RoutedMetaBackend,
    parent: u64,
    want_vol: usize,
    tag: &str,
) -> u64 {
    for i in 0..64 {
        let ino = routed
            .create(parent, &format!("{tag}_d{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir")
            .ino;
        if routed.route_ino(ino).0 == want_vol {
            return ino;
        }
    }
    panic!("directory striping never placed a directory on volume {want_vol}");
}

async fn mkfile_on(
    routed: &squeezefs::meta_backend::RoutedMetaBackend,
    parent: u64,
    want_vol: usize,
    tag: &str,
) -> (u64, String) {
    for i in 0..64 {
        let name = format!("{tag}_f{i}");
        let ino = routed
            .create(parent, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        if routed.route_ino(ino).0 == want_vol {
            return (ino, name);
        }
    }
    panic!("inode placement never minted a file on volume {want_vol}");
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

// ---------------------------------------------------------------------------
// §5.4a — the M1 cross-owner pre-check, M2's placement invariant, and the
// frozen cross-owner reference set (§5.9.2's four arms)
// ---------------------------------------------------------------------------

/// Arms an ownership plane in which volume 1 belongs to a PEER, and
/// disarms it however the test ends (the plane is process-global).
struct ArmedPlane;

impl Drop for ArmedPlane {
    fn drop(&mut self) {
        squeezefs::meta_ship::disarm_ownership();
    }
}

fn arm_peer_owns_volume_1(routed: &squeezefs::meta_backend::RoutedMetaBackend) -> ArmedPlane {
    let map = squeezefs::meta_ship::OwnerMap::for_volumes(
        routed,
        vec![(
            1,
            squeezefs::meta_ship::PeerOwner::new(PEER, "127.0.0.1:7100"),
        )],
    )
    .expect("a per-volume map over this set");
    squeezefs::meta_ship::arm_ownership(map);
    ArmedPlane
}

/// **§5.4a M1** — the correctness-class fix that lands unconditionally.
/// `unlink` discovers its child under the guards it already holds, so the
/// ROUTER cannot see it (`named_inos` returns the parent alone) and an
/// ordinary `rm` of a peer-owned child would build an `XvPlan`, commit its
/// first half, fail-stop BOTH volumes and leave a durable intent that
/// refuses the next mount. The pre-check refuses EXDEV **before any plan
/// is minted**, so there is no durable effect at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_owner_unlink_refuses_before_the_plan_is_minted() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "m1-unlink").await;
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .expect("write mount");
    let parent = mkdir_on(&routed, 1, 0, "m1").await;
    let (child, name) = mkfile_on(&routed, parent, 1, "m1").await;

    let _plane = arm_peer_owns_volume_1(&routed);
    let intents_before = open_intent_count(&routed).await;
    let err = routed
        .unlink(parent, &name)
        .await
        .err()
        .unwrap_or_else(|| panic!("unlinking a peer-owned child must refuse"));
    assert_eq!(
        err.to_errno(),
        libc::EXDEV,
        "the refusal is EXDEV — the errno POSIX already gives for two names that are not on \
         one object graph: {err}"
    );
    assert!(
        err.to_string().contains("S3.5"),
        "the refusal must name the machinery that is NOT built: {err}"
    );

    // No durable effect: the name is still there, the inode still has its
    // link, and — the corruption vector itself — NO intent was minted and
    // neither volume was fail-stopped.
    assert_eq!(
        lookup_child(&routed, parent, &name).await,
        Some(child),
        "the name must survive a refused cross-owner unlink"
    );
    assert_eq!(
        open_intent_count(&routed).await,
        intents_before,
        "an XvPlan was minted for a cross-owner unlink — this is the shape that fail-stops \
         two volumes and bricks the next mount (§5.4a, R1)"
    );
    assert!(
        !routed.disabled_volumes.contains_key(&0) && !routed.disabled_volumes.contains_key(&1),
        "a refused cross-owner unlink must not fail-stop anything"
    );
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// The negative twin (§5.4a): with the pre-check bypassed through its
/// test seam, the SAME `rm` on a REAL partial mount — the peer's volume
/// opened `PeerOwnedVolume`, its write gate live — reaches the
/// cross-volume machinery, commits step 0, refuses step 1, and fail-stops
/// BOTH volumes. That is what records *why* M1 exists rather than a
/// comment claiming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pre_check_absent_shape_is_what_fail_stops_two_volumes() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "m1-absent").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];

    let (parent, name) = {
        let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
            .await
            .expect("an ordinary write mount builds the pre-assignment tree");
        let parent = mkdir_on(&routed, 1, 0, "m1x").await;
        let (_child, name) = mkfile_on(&routed, parent, 1, "m1x").await;
        for vol in &routed.volumes {
            vol.shutdown().await.expect("release");
        }
        (parent, name)
    };

    let claim = foreign_claim();
    store_set(&vol0, &own_set()).await;
    store_set(&vol1, &peer_owned_set(&claim, true)).await;
    plant(&vol1, &claim, false).await;
    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, Some(&claim));
    let routed = squeezefs::meta_backend::open_routed_meta_set_partial(&uris, &admission)
        .await
        .expect("the partial mount comes up");
    let _plane = arm_peer_owns_volume_1(&routed);

    squeezefs::meta_backend::test_disable_cross_owner_precheck(true);
    let out = routed.unlink(parent, &name).await;
    squeezefs::meta_backend::test_disable_cross_owner_precheck(false);

    assert!(
        out.is_err(),
        "without M1 the plan is minted and the peer-owned half refuses mid-plan"
    );
    assert!(
        routed.disabled_volumes.contains_key(&0) && routed.disabled_volumes.contains_key(&1),
        "the pre-M1 shape fail-stops BOTH volumes mid-plan and leaves a durable intent — if \
         this stops being true, M1's justification has changed and the design owes a \
         re-reading"
    );
    assert_eq!(
        open_intent_count(&routed).await,
        1,
        "the half-committed transaction's intent is durable, and it spans two owners: the \
         next mount refuses on it (§5.4a case (c))"
    );
    for vol in &routed.volumes {
        let _ = vol.shutdown().await;
    }
}

/// §5.4a's plural participant set: the rename pre-check covers the MOVED
/// ino, the OVERWRITE VICTIM and — on `RENAME_EXCHANGE` — both
/// participants, mirroring the owner-side loop at `service.rs:1140-1150`
/// rather than checking "the child", singular.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rename_precheck_covers_the_moved_ino_the_overwrite_victim_and_both_exchange_participants(
) {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "m1-rename").await;
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .expect("write mount");
    let home = mkdir_on(&routed, 1, 0, "rn").await;
    // Three participants, each on the volume its arm needs.
    let (_peer_moved, peer_moved_name) = mkfile_on(&routed, home, 1, "moved").await;
    let (_local_a, local_a) = mkfile_on(&routed, home, 0, "locala").await;
    let (_peer_victim, peer_victim) = mkfile_on(&routed, home, 1, "victim").await;
    let (_local_b, local_b) = mkfile_on(&routed, home, 0, "localb").await;

    let _plane = arm_peer_owns_volume_1(&routed);
    for (what, old, new, flags) in [
        (
            "the moved ino",
            peer_moved_name.as_str(),
            "fresh-name",
            0u32,
        ),
        (
            "the overwrite victim",
            local_a.as_str(),
            peer_victim.as_str(),
            0,
        ),
        (
            "an exchange participant",
            local_b.as_str(),
            peer_victim.as_str(),
            libc::RENAME_EXCHANGE,
        ),
    ] {
        let err = routed
            .rename(home, old, home, new, flags)
            .await
            .err()
            .unwrap_or_else(|| panic!("[{what}] a cross-owner rename must refuse"));
        assert_eq!(
            err.to_errno(),
            libc::EXDEV,
            "[{what}] the refusal must be EXDEV: {err}"
        );
    }
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// **M2** (§5.4a): every ino minted under an armed plane shares its
/// parent's owner. This is the invariant that makes cross-owner
/// parent→child pairs unreachable for everything the fleet CREATES — and
/// therefore the invariant that keeps `rm` working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_ino_minted_under_an_armed_plane_shares_its_parents_owner() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "m2").await;
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .expect("write mount");
    let parent = mkdir_on(&routed, 1, 0, "m2").await;
    let parent_vol = routed.route_ino(parent).0;

    let _plane = arm_peer_owns_volume_1(&routed);
    for i in 0..48 {
        let ino = routed
            .create(parent, &format!("m2_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        let child_vol = routed.route_ino(ino).0;
        assert_eq!(
            squeezefs::meta_ship::owners::owns_volume(child_vol),
            squeezefs::meta_ship::owners::owns_volume(parent_vol),
            "child {ino} landed on volume {child_vol}, whose owner differs from its parent's \
             ({parent_vol}) — M2 is what makes cross-owner unlink unreachable for new work"
        );
    }
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// Sweep row 13 / the freeze law's third arm: while a multi-owner plane is
/// armed, no cross-owner dentry can be RELOCATED — `migrate_slot` refuses
/// when either endpoint is peer-owned (D19's follow-on), and refuses slot
/// 0 outright (KD-PV-6: the set authority cannot silently relocate).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slot_migration_refuses_cross_owner_endpoints_and_slot_zero_while_armed() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "row13").await;
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .expect("write mount");
    let peer_slot = routed
        .slot_map_snapshot()
        .iter()
        .position(|&v| v == 1)
        .expect("volume 1 hosts a slot") as u16;

    let _plane = arm_peer_owns_volume_1(&routed);
    let opts = squeezefs::meta_backend::slot_migration::MigrationOptions::default();
    let hooks = squeezefs::meta_backend::slot_migration::MigrationTestHooks::default();

    let cross =
        squeezefs::meta_backend::slot_migration::migrate_slot(&routed, peer_slot, 0, &opts, &hooks)
            .await
            .err()
            .unwrap_or_else(|| panic!("a cross-owner slot migration must refuse"));
    assert!(
        cross.to_string().contains("cross-owner"),
        "the refusal must name what it is: {cross}"
    );

    let slot0 = squeezefs::meta_backend::slot_migration::migrate_slot(&routed, 0, 1, &opts, &hooks)
        .await
        .err()
        .unwrap_or_else(|| panic!("slot 0 is non-migratable while a multi-owner plane is armed"));
    assert!(
        slot0.to_string().contains("slot 0"),
        "the refusal must name slot 0 and the set authority: {slot0}"
    );
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

async fn open_intent_count(routed: &squeezefs::meta_backend::RoutedMetaBackend) -> usize {
    let mut n = 0;
    for vol in &routed.volumes {
        n += vol.xv_scan_intents().await.expect("intent scan").len();
    }
    n
}

async fn lookup_child(
    routed: &squeezefs::meta_backend::RoutedMetaBackend,
    parent: u64,
    name: &str,
) -> Option<u64> {
    routed
        .lookup_dentry(parent, name)
        .await
        .expect("lookup")
        .map(|(ino, _)| ino)
}

// ---------------------------------------------------------------------------
// §5.1.3 + sweep rows 14/15 — the two postures, and per-VOLUME revalidation
// arming (risk R18: a set authority latches NEITHER latch)
// ---------------------------------------------------------------------------

/// Restores the process-global posture whatever a case does to it.
struct PostureGuard;

impl Drop for PostureGuard {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_mount_posture(squeezefs::fuse_client::MountPosture::Writer);
    }
}

/// §5.1.3's latch table, in full. `PARTIAL_META` is ADDITIVE — every
/// data-plane consumer of `read_only_mount()` / `co_writer_mount()` keeps
/// its exact meaning — and the two new postures differ from each other in
/// exactly the co-writer latch, which is what gives a set authority a
/// `writer`-identical data plane and a partial authority a co-writer one.
#[test]
fn the_two_new_postures_latch_exactly_what_the_table_says() {
    use squeezefs::fuse_client::{
        co_writer_mount, mount_posture, partial_meta_mount, read_only_mount, set_mount_posture,
        MountPosture,
    };
    let _restore = PostureGuard;
    for (posture, ro, cw, pm, word) in [
        (MountPosture::Writer, false, false, false, "writer"),
        (MountPosture::Reader, true, false, false, "reader"),
        (MountPosture::CoWriter, false, true, false, "co-writer"),
        (
            MountPosture::SetAuthority,
            false,
            false,
            true,
            "set-authority",
        ),
        (
            MountPosture::PartialAuthority,
            false,
            true,
            true,
            "partial-authority",
        ),
    ] {
        set_mount_posture(posture);
        assert_eq!(read_only_mount(), ro, "{word}: READ_ONLY latch");
        assert_eq!(co_writer_mount(), cw, "{word}: CO_WRITER latch");
        assert_eq!(partial_meta_mount(), pm, "{word}: PARTIAL_META latch");
        assert_eq!(mount_posture(), posture, "{word}: posture round-trip");
        assert_eq!(mount_posture().as_str(), word);
    }
}

/// **Sweep row 15 / risk R18** — the arming predicate is per VOLUME, not
/// per mount, and it is derived from each volume's own cause rather than
/// from a process latch. A set authority latches neither `READ_ONLY` nor
/// `CO_WRITER`, so the shipped `if reader_mount || co_writer` site skipped
/// it entirely — and it holds K−1 peer-owned volumes whose node caches
/// would then never step an epoch, serving its mount-time read for ever
/// and never running the R-6 purge, on the node that coordinates
/// maintenance and homes ino 1.
#[test]
fn revalidation_arms_over_the_volumes_a_mount_does_not_append_to() {
    use squeezefs::meta_backend::kv::backend::ReadOnlyCause as C;
    // The selector answers the four causes exactly: a volume this mount
    // APPENDS to is never armed (arming one trips
    // `meta_kv_revalidate_dirty_skips`, a must-stay-0 counter), and the
    // §4.11 degradation is not a coherence posture at all — it is a WRITE
    // mount holding Layer A whose volume has no other appender.
    assert!(squeezefs::ro_coherence::volume_wants_revalidation(
        C::ReaderMount
    ));
    assert!(squeezefs::ro_coherence::volume_wants_revalidation(
        C::CoWriterMount
    ));
    assert!(squeezefs::ro_coherence::volume_wants_revalidation(
        C::PeerOwnedVolume
    ));
    assert!(!squeezefs::ro_coherence::volume_wants_revalidation(
        C::Writable
    ));
    assert!(!squeezefs::ro_coherence::volume_wants_revalidation(
        C::UnknownRoFeatureBits
    ));
}

/// Both arming pins of the PR row, on real mounts: a partial authority and
/// a SET authority each arm revalidation over their peer-owned subset and
/// nothing else, and `meta_kv_revalidate_dirty_skips` — the tripwire that
/// fires when a mount arms a volume it appends to — stays 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partial_and_a_set_authority_arm_revalidation_on_peer_volumes_only() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "reval").await;
    let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let claim = foreign_claim();
    store_set(&vol0, &own_set()).await;
    store_set(&vol1, &peer_owned_set(&claim, true)).await;
    plant(&vol1, &claim, false).await;

    let admission = set_authority_admission(&vol0, &id0, &vol1, &id1, Some(&claim));
    let routed = squeezefs::meta_backend::open_routed_meta_set_partial(&uris, &admission)
        .await
        .expect("the set authority's partial mount");
    let wants: Vec<bool> = routed
        .volumes
        .iter()
        .map(|v| squeezefs::ro_coherence::volume_wants_revalidation(v.read_only_cause()))
        .collect();
    assert_eq!(
        wants,
        vec![false, true],
        "a set authority must arm revalidation over its PEER-OWNED subset and nothing else \
         (R18) — it appends to volume 0"
    );
    assert_eq!(
        squeezefs::meta_backend::kv::revalidate::revalidation_stats().dirty_skips,
        0,
        "meta_kv_revalidate_dirty_skips is the tripwire for arming a volume this mount \
         appends to"
    );
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// **Sweep row 17** — the membership rendezvous under a per-volume
/// posture. An owner publishes its record to EVERY volume today and a
/// member picks the highest-term record across all of them; under
/// multi-owner only the set authority may write, so peer volumes would
/// keep **stale** records nobody can remove (they are pinned against slot
/// travel) and max-term selection could point a member at a dead endpoint.
/// The rendezvous is therefore the slot-0 volume alone (D20).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_joins_the_slot_0_owner_and_never_a_stale_peer_rendezvous() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "rendezvous").await;
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .expect("write mount");
    assert_eq!(
        routed.route_ino(1).0,
        0,
        "ino 1 pins to slot 0, which homes on volume 0 (KD-PV-6)"
    );

    let _restore = PostureGuard;
    // A STALE record on the peer's volume, carrying a HIGHER term than the
    // live one: exactly the shape max-term selection would pick.
    squeezefs::membership::publish_owner_record(
        &routed.volumes[1],
        &squeezefs::membership::OwnerRecord {
            v: 1,
            id: "dead-incarnation".to_string(),
            term: 99,
            endpoint: "10.9.9.9:9999".to_string(),
            owner_claim_id: PEER.to_string(),
            ttl_ms: 45_000,
            ts: 1_700_000_000,
            pid: 4242,
            boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        },
    )
    .await
    .expect("plant the stale peer rendezvous");
    squeezefs::membership::publish_owner_record(
        &routed.volumes[0],
        &squeezefs::membership::OwnerRecord {
            v: 1,
            id: "live-set-authority".to_string(),
            term: 7,
            endpoint: "127.0.0.1:7100".to_string(),
            owner_claim_id: NODE.to_string(),
            ttl_ms: 45_000,
            ts: 1_700_000_000,
            pid: std::process::id(),
            boot: String::new(),
        },
    )
    .await
    .expect("plant the live slot-0 rendezvous");

    squeezefs::fuse_client::set_mount_posture(
        squeezefs::fuse_client::MountPosture::PartialAuthority,
    );
    let scoped = squeezefs::membership::test_rendezvous_records(&routed).await;
    assert_eq!(
        scoped.len(),
        1,
        "under a per-volume posture the rendezvous is the slot-0 volume ALONE"
    );
    assert_eq!(
        scoped[0].id, "live-set-authority",
        "a member must join the slot-0 owner, never the higher-term stale record a peer \
         volume kept: {:?}",
        scoped[0]
    );

    squeezefs::fuse_client::set_mount_posture(squeezefs::fuse_client::MountPosture::Writer);
    assert_eq!(
        squeezefs::membership::test_rendezvous_records(&routed)
            .await
            .len(),
        2,
        "every other posture reads the whole set, exactly as shipped"
    );
    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}

/// **§5.9.2's frozen-cross-owner-reference law, all four arms in one
/// place** — the property the online inode plane rests on (KD-PV-8): while
/// a multi-owner plane is armed, no cross-owner dentry can be CREATED
/// (M2's mint constraint), REMOVED (M1 on `unlink`/`rmdir`), ADDED by name
/// (M1 on `link`/`rename`, which the router and the owner side also
/// refuse) or RELOCATED (row 13's cross-owner slot-migration refusal). The
/// cross-owner reference set is fixed at the assignment instant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cross_owner_dentry_set_is_frozen_under_an_armed_plane() {
    let dir = TempDir::new().unwrap();
    let (vol0, vol1) = two_volume_set(dir.path(), "freeze").await;
    let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
    let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
        .await
        .expect("write mount");
    let home = mkdir_on(&routed, 1, 0, "fz").await;
    // Pre-assignment residue: a name on OUR volume for an inode on the
    // peer's — the population M3 counts and this law freezes.
    let (peer_child, peer_name) = mkfile_on(&routed, home, 1, "fz").await;

    let _plane = arm_peer_owns_volume_1(&routed);

    // Arm 1 — CREATE: every new ino shares its parent's owner.
    let fresh = routed
        .create(home, "fz_new", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    assert!(
        squeezefs::meta_ship::owners::owns_volume(routed.route_ino(fresh).0),
        "arm 1: a create under an owned parent minted into a peer's volume"
    );

    // Arm 2 — REMOVE.
    assert_eq!(
        routed
            .unlink(home, &peer_name)
            .await
            .err()
            .map(|e| e.to_errno()),
        Some(libc::EXDEV),
        "arm 2: a cross-owner name must not be removable in place"
    );

    // Arm 3 — ADD (link; rename is pinned by its own case).
    assert_eq!(
        routed
            .link(peer_child, home, "fz_link")
            .await
            .err()
            .map(|e| e.to_errno()),
        Some(libc::EXDEV),
        "arm 3: no NEW cross-owner name may be created"
    );

    // Arm 4 — RELOCATE.
    let peer_slot = routed
        .slot_map_snapshot()
        .iter()
        .position(|&v| v == 1)
        .expect("volume 1 hosts a slot") as u16;
    assert!(
        squeezefs::meta_backend::slot_migration::migrate_slot(
            &routed,
            peer_slot,
            0,
            &squeezefs::meta_backend::slot_migration::MigrationOptions::default(),
            &squeezefs::meta_backend::slot_migration::MigrationTestHooks::default(),
        )
        .await
        .is_err(),
        "arm 4: a cross-owner slot migration would MOVE a name's inode away from it"
    );

    for vol in &routed.volumes {
        vol.shutdown().await.expect("release");
    }
}
