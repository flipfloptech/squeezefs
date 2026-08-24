//! **Finding 13 — the shipped-free validation must read the ledger where
//! the ledger LIVES** (per-volume claim admission PR 8, leg (c);
//! `docs/design-per-volume-claim-admission.md` §5.11;
//! `.benchmarks/2026-08-22-pv-claim-admission.md` finding 13).
//!
//! # The failure the leg counted
//!
//! `pv-rewrite partial-1: the partial shipped 128 free block(s) and the
//! authority served 0 — the ledgers must close.`
//!
//! # The mechanism under contract here
//!
//! Classic S9 has ONE metadata authority: a co-writer ships every layout
//! publish to it, so the `TREE_BLOCK_REFS` delete lands on the authority's
//! own live tree before the displaced free arrives, and
//! `cowriter::durable_block_refcount` — the owner-side validation of a
//! peer's terminal free — reads current truth from the local volumes.
//!
//! Under D20 the durable ledger is DISTRIBUTED: a **partial authority
//! commits its layout publishes locally on the volumes it owns**, and the
//! set authority holds those volumes only as peer-owned reader snapshots
//! (`KvMetaBackend::open_peer_owned` — no checkpoint task, no live
//! journal). Reading the population off that lagged copy answers a peer's
//! shipped free from state the peer has already moved, in BOTH
//! directions:
//!
//! * **the strand (the leg's shape):** the peer released the reference on
//!   its own live tree, the snapshot still shows it ⇒ population > 0 ⇒
//!   `NonTerminal` ⇒ the free ladder never runs, `free_served_blocks`
//!   stays 0, and the blocks are stranded until the next derivation;
//! * **the destructive twin:** a reference ADDED on the peer's live tree
//!   after the snapshot is invisible ⇒ population 0 ⇒ a false `Freed` on
//!   a block a live inode still references — §6.3's reclaim/discard
//!   hazard, the one reader failure mode that destroys data.
//!
//! The contract: the population read is **owner-partitioned** — owned
//! volumes read locally (the live tree), peer-owned volumes ship the read
//! to their owner, and a SERVING owner answers for its owned volumes only
//! (its own peer-owned copies are somebody else's answer). An unarmed
//! mount — every mount that ships today — keeps the all-local read
//! verbatim (the solo re-gate).
//!
//! RED against `dev` (0f2f4259): `durable_block_refcount` sums
//! `block_ref_count` over EVERY volume of the routed set locally, so the
//! set authority's stale peer-owned copy is counted as truth and both
//! verdicts below are answered inverted.
//!
//! # What one process cannot pin (stated, not hidden)
//!
//! Two hosts are what the real fleet runs; here both partial opens share
//! one process (the pv/S8/S9 test discipline: peer-owned opens take no
//! lock, so the two routed sets coexist), and the process-global ownership
//! arm is the SET AUTHORITY's — the serving peer's side is scoped by its
//! own `PublishService::with_authority`, never by the global map.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cluster_wire as cw;
use squeezefs::cowriter::{AuthorityLeaseEvidence, MwRole, RegistrantEvidence};
use squeezefs::data_grant::AsyncVerbRouter;
use squeezefs::fuse_client::{self, MountPosture};
use squeezefs::membership::{ClaimSet, ClaimSetMember, MemberIdentity, MemberRole};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim};
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::meta_ship::publish::FreeVerdict;
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::partial_authority::{
    self as pv, ClaimStanding, PvVolumeEvidence, SetAdmission, SetAdmissionRequest,
};
use squeezefs::routing::BackendRouter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;
/// Sparse data-device backing — big enough that every block index this
/// file frees stays inside it (punch-on-free needs `offset < len`).
const DEV_LEN: u64 = 2 * 1024 * 1024 * 1024;

/// The `job:enroll`-class storage-trust secret both halves prove
/// possession of (S3's root of trust — ruling D2).
const SECRET: &[u8] = b"pv-shipped-free-ledger-storage-trust-secret";

/// The SET AUTHORITY (D20: owns the slot-0 volume). KD-MW-2 spelling.
const NODE_A: &str = "node_00000000deadbeef.m00000001";
/// The PARTIAL AUTHORITY (owns the second volume).
const NODE_B: &str = "node_00000000feedface.m00000001";

/// A durable data-volume id (KD-5's `vol-{16 hex}` shape) — `volume_tag`
/// decodes it verbatim, so the wire carries the durable identity.
const DATA_VOL: &str = "vol-00000000000000f2";

// ---------------------------------------------------------------------------
// Serialization + restoration (process-global state everywhere)
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
        // Panic-safe restoration: a failed assert must not leak the arm
        // into the next test of this binary.
        ship::disarm_ownership();
        publish::uninstall_client();
        fuse_client::set_mount_posture(MountPosture::Writer);
        SERIAL_HELD.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Fixtures (the pv_partial_open / mw_cowriter_free patterns, verbatim)
// ---------------------------------------------------------------------------

fn opts() -> squeezefs::meta_backend::kv::builder::FormatV3Options {
    squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// The nine-bit multi-writer stamp — `volume enable-multi-writer`'s
/// offline act, as every PV suite spells it.
async fn stamp_capabilities(path: &Path) {
    sb::set_durable_term_bit(path).await.expect("bit 7");
    sb::set_block_refcounts_bit(path).await.expect("bit 9");
    sb::set_layout_versions_bit(path).await.expect("bit 15");
    sb::set_ino_lanes_bit(path).await.expect("bit 12");
    sb::set_block_key_incarnation_bit(path)
        .await
        .expect("bit 13");
    sb::set_partitioned_append_bit(path).await.expect("bit 8");
    sb::set_writer_scoped_staging_bit(path)
        .await
        .expect("bit 10");
    sb::set_claim_set_bit(path).await.expect("bit 14");
    sb::set_multi_writer_data_bit(path).await.expect("bit 11");
}

/// A two-volume stamped set in canonical slot-plan order: `vol0` hosts
/// slot 0 (the set authority's), `vol1` is the partial's.
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

/// The durable claim set the offline `volume set-owners` verb writes on a
/// volume owned by `owner`, with both nodes enrolled as writer members.
fn owned_set(owner: &str) -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.owner = Some(owner.to_string());
    set.members = vec![member(NODE_A), member(NODE_B)];
    set
}

async fn store_set(path: &Path, set: &ClaimSet) {
    let be = KvMetaBackend::open(path).await.expect("store open");
    ClaimSet::store(&be, set).await.expect("store claim set");
    be.sync_device().await.expect("barrier");
    be.shutdown().await.expect("release");
}

async fn vol_id(path: &Path) -> String {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    probe.durable_volume_id()
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

fn registrant(key: u64) -> RegistrantEvidence {
    RegistrantEvidence {
        pr_capable: true,
        wero: true,
        reservation_held: true,
        registered: true,
        key,
        namespaces: 1,
    }
}

/// The SET AUTHORITY's admission over the two-volume set: `vol0` (slot 0)
/// is OURS, `vol1` belongs to `NODE_B` — unclaimed at this point (the
/// degraded not-yet-up bootstrap state, which admits).
fn set_authority_admission(vol0: &Path, id0: &str, vol1: &Path, id1: &str) -> SetAdmission {
    let req = SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::SetAuthority,
        read_only: false,
        node_id: NODE_A.to_string(),
        set_authority_endpoint: None,
        volumes: vec![
            evidence(vol0, id0, true, None, None, owned_set(NODE_A)),
            evidence(vol1, id1, false, None, None, owned_set(NODE_B)),
        ],
        authority: None,
        registrant: Some(registrant(0xB0B0)),
    };
    pv::classify_set_admission(&req).expect("the ladder admits the set-authority fixture")
}

/// The PARTIAL's admission, gathered AFTER the set authority mounted: its
/// live claim + KD-PV-17 holder attestation on `vol0` are read back from
/// the volume itself, exactly as `gather_volume_evidence` reads them.
async fn partial_admission(vol0: &Path, id0: &str, vol1: &Path, id1: &str) -> SetAdmission {
    let probe = KvMetaBackend::open_probe(vol0).await.expect("probe vol0");
    let claim0 = probe
        .read_writer_claim()
        .await
        .expect("the set authority's live claim on vol0");
    let set0 = ClaimSet::load(&probe)
        .await
        .expect("vol0's durable claim set (with the holder attestation)");
    let claim_term = claim0.term;
    drop(probe);
    let req = SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::PartialAuthority,
        read_only: false,
        node_id: NODE_B.to_string(),
        set_authority_endpoint: Some("127.0.0.1:7100".to_string()),
        volumes: vec![
            evidence(vol0, id0, true, Some(claim0), Some(NODE_A), set0),
            evidence(vol1, id1, false, None, None, owned_set(NODE_B)),
        ],
        authority: Some(AuthorityLeaseEvidence {
            owner_id: NODE_A.to_string(),
            endpoint: "127.0.0.1:7100".to_string(),
            owner_claim_id: NODE_A.to_string(),
            // Rung 4 compares against the slot-0 volume's LIVE claim, whose
            // term every open bumps — so the evidence carries the real one.
            term: claim_term,
            live: true,
            member_epoch: 3,
        }),
        registrant: Some(registrant(0xB0B1)),
    };
    pv::classify_set_admission(&req).expect("the seven-rung ladder admits the partial fixture")
}

/// One side's data plane: an allocator + a router over the shared backing
/// (the mw_cowriter_free helper verbatim).
async fn data_plane(dev: &Path) -> (Arc<BlockAllocator>, Arc<BackendRouter>) {
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL).await.expect("allocator"));
    let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
    let chunk = alloc.chunk_size();
    let br = Arc::new(BackendRouter::new(
        Arc::clone(&alloc),
        nvme,
        Arc::new(AtomicU64::new(chunk)),
    ));
    (alloc, br)
}

fn dev_file(dir: &Path) -> PathBuf {
    let p = dir.join("data-dev");
    std::fs::File::create(&p).unwrap().set_len(DEV_LEN).unwrap();
    p
}

/// First global ino ≥ 2 routing to volume `v_idx` of `routed`'s set.
fn ino_on_volume(routed: &RoutedMetaBackend, v_idx: usize) -> u64 {
    (2..100_000)
        .find(|i| routed.route_ino(*i).0 == v_idx)
        .expect("an ino routing to the volume exists in the first 100k")
}

fn taken(tag: u64, block_idx: u64, owner_ino: u64) -> BlockRefOp {
    BlockRefOp::taken(BlockRef {
        vol_tag: tag,
        block_idx,
        owner_ino,
        block_index: 0,
    })
}

fn released(tag: u64, block_idx: u64, owner_ino: u64) -> BlockRefOp {
    BlockRefOp::released(BlockRef {
        vol_tag: tag,
        block_idx,
        owner_ino,
        block_index: 0,
    })
}

/// The whole two-node stage: seed refs offline, open A (set authority,
/// vol1 peer-owned — the lagged snapshot), open B (partial, vol1 live),
/// start B's scoped publish listener, arm A's ownership + client.
///
/// Returns `(A's routed set, B's routed set, B's listener, tag, ino_v1)`.
struct Stage {
    a_meta: Arc<RoutedMetaBackend>,
    b_meta: Arc<RoutedMetaBackend>,
    listener: Arc<cw::RpcListener>,
    tag: u64,
    ino_v1: u64,
}

impl Stage {
    /// `seed` runs on the offline full-set open (the D19 coordinator
    /// shape) BEFORE either node mounts — what it commits is in BOTH
    /// nodes' opening views.
    async fn build(
        dir: &Path,
        tag_name: &str,
        seed: impl AsyncFnOnce(&Arc<RoutedMetaBackend>, u64, u64),
    ) -> Stage {
        let (vol0, vol1) = two_volume_set(dir, tag_name).await;
        let (id0, id1) = (vol_id(&vol0).await, vol_id(&vol1).await);
        store_set(&vol0, &owned_set(NODE_A)).await;
        store_set(&vol1, &owned_set(NODE_B)).await;
        let uris = vec![vol0.display().to_string(), vol1.display().to_string()];
        let tag = volume_tag(DATA_VOL);

        // Seed on the offline coordinator open — durable before ANY node's
        // snapshot, so both opening views carry it.
        let ino_v1;
        {
            let routed = squeezefs::meta_backend::open_routed_meta_set(&uris)
                .await
                .expect("the offline coordinator opens the whole set");
            ino_v1 = ino_on_volume(&routed, 1);
            let ino_v0 = ino_on_volume(&routed, 0);
            seed(&routed, ino_v1, ino_v0).await;
            for v in &routed.volumes {
                v.shutdown().await.expect("seeding open releases clean");
            }
        }

        // The SET AUTHORITY mounts first: vol0 own (D0 ladder + KD-PV-17
        // attestation), vol1 peer-owned — the SNAPSHOT this file is about.
        let a_adm = set_authority_admission(&vol0, &id0, &vol1, &id1);
        let a_meta = squeezefs::meta_backend::open_routed_meta_set_partial(&uris, &a_adm)
            .await
            .expect("the set authority's partial open");

        // The PARTIAL mounts second, with A's live claim as evidence: vol1
        // own — ITS live tree is the ledger's home for vol1's records.
        let b_adm = partial_admission(&vol0, &id0, &vol1, &id1).await;
        let b_meta = squeezefs::meta_backend::open_routed_meta_set_partial(&uris, &b_adm)
            .await
            .expect("the partial authority's open");

        // B's publish service, scoped to the volume it OWNS (index 1) —
        // its own peer-owned copy of vol0 is A's to answer, never B's.
        let router = AsyncVerbRouter::new().with_publish(publish::PublishService::with_authority(
            Arc::clone(&b_meta),
            &[1],
        ));
        let listener = cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            SECRET.to_vec(),
            Arc::new(router),
        )
        .expect("the partial listens");
        let endpoint = listener.endpoint().to_string();

        // A's ownership plane: vol0 local, vol1 → the partial. Plus the
        // publish client the shipped read travels on.
        let map = OwnerMap::for_volumes(&a_meta, vec![(1, PeerOwner::new(NODE_B, endpoint))])
            .expect("A's owner map");
        ship::arm_ownership(map);
        publish::install_client(publish::PublishClient::new(NODE_A, SECRET.to_vec()));
        fuse_client::set_mount_posture(MountPosture::Writer);

        Stage {
            a_meta,
            b_meta,
            listener,
            tag,
            ino_v1,
        }
    }

    async fn stop(self) {
        self.listener.shutdown();
        for v in &self.b_meta.volumes {
            let _ = v.shutdown().await;
        }
        for v in &self.a_meta.volumes {
            let _ = v.shutdown().await;
        }
    }
}

// ---------------------------------------------------------------------------
// The contracts
// ---------------------------------------------------------------------------

/// **The population read is owner-partitioned** — each volume's count
/// comes from its OWNER's live tree: owned volumes locally, peer-owned
/// volumes shipped, and the SERVING owner answers only for what it owns.
///
/// Fixture arithmetic: block 7 carries one reference on vol0 (A's, live,
/// stays) and one on vol1 (B's, released on B's live tree after A's
/// snapshot). Truth = 1.
///
/// * A's local-sum read answers 2 (its stale vol1 copy still shows the
///   released reference) — the RED shape;
/// * a serve that ignored its authority scope would ALSO answer 2 through
///   the wire (B's stale vol0 copy re-counting A's reference), so this
///   one number pins both halves of the partition law.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_population_read_is_owner_partitioned_not_a_local_snapshot_sum() {
    let _serial = serial();
    let dir = TempDir::new().unwrap();
    let stage = Stage::build(dir.path(), "pv-pop", async |routed, ino_v1, ino_v0| {
        let tag = volume_tag(DATA_VOL);
        routed
            .commit_block_refs(ino_v0, &[taken(tag, 7, ino_v0)])
            .await
            .expect("seed vol0's reference");
        routed
            .commit_block_refs(ino_v1, &[taken(tag, 7, ino_v1)])
            .await
            .expect("seed vol1's reference");
        // The solo re-gate, pinned in passing: an UNARMED mount reads the
        // ledger locally and must see both seeds.
        assert_eq!(
            squeezefs::cowriter::durable_block_refcount(routed, tag, 7)
                .await
                .expect("the unarmed local read answers"),
            2,
            "the seeding fixture must be visible to the unarmed local read"
        );
    })
    .await;

    // The partial releases ITS reference on ITS live tree — the layout
    // publish's durable half, exactly what a rewrite's displacement
    // commits locally under D20.
    stage
        .b_meta
        .commit_block_refs(stage.ino_v1, &[released(stage.tag, 7, stage.ino_v1)])
        .await
        .expect("the partial releases its reference locally");

    let population = squeezefs::cowriter::durable_block_refcount(&stage.a_meta, stage.tag, 7)
        .await
        .expect("the owner-partitioned read answers");
    stage.stop().await;
    assert_eq!(
        population, 1,
        "the set authority must read vol1's population from the PARTIAL's live tree (0 there \
         after the release) plus its own vol0 (1) — a count of 2 means it summed its lagged \
         peer-owned snapshot as truth (finding 13's read seam)"
    );
}

/// **Shipped-free verdicts come from the owner's live ledger** — the two
/// inverted directions in one call:
///
/// * block 7: referenced in both snapshots, RELEASED on the partial's
///   live tree ⇒ truth 0 ⇒ **`Freed`** (the leg's strand read
///   `NonTerminal` here forever — ship=128/serve=0);
/// * block 9: unreferenced in A's snapshot, ADDED on the partial's live
///   tree ⇒ truth 1 ⇒ **`NonTerminal`** (the stale read answers `Freed`,
///   which frees a block a live inode still references — the destructive
///   twin).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shipped_free_verdicts_come_from_the_owners_live_ledger() {
    let _serial = serial();
    let dir = TempDir::new().unwrap();
    let stage = Stage::build(dir.path(), "pv-verdict", async |routed, ino_v1, _ino_v0| {
        let tag = volume_tag(DATA_VOL);
        routed
            .commit_block_refs(ino_v1, &[taken(tag, 7, ino_v1)])
            .await
            .expect("seed block 7's reference on vol1");
    })
    .await;

    // After A's snapshot, on B's live tree: block 7's reference RELEASED
    // (the displaced half of a rewrite's publish), block 9's ADDED (a
    // fresh reference only the owner's live tree holds).
    stage
        .b_meta
        .commit_block_refs(
            stage.ino_v1,
            &[
                released(stage.tag, 7, stage.ino_v1),
                taken(stage.tag, 9, stage.ino_v1),
            ],
        )
        .await
        .expect("the partial moves its live ledger");

    let (_alloc, br) = data_plane(&dev_file(dir.path())).await;
    let verdicts =
        squeezefs::cowriter::execute_shipped_frees(&br, &stage.a_meta, stage.tag, &[7, 9])
            .await
            .expect("the authority executes the shipped frees");
    stage.stop().await;
    assert_eq!(
        verdicts,
        vec![FreeVerdict::Freed, FreeVerdict::NonTerminal],
        "the owner-side validation must be the OWNER's live ledger: a released-on-the-owner \
         block frees (the leg's ship=128/serve=0 strand reads NonTerminal here), and a block \
         whose reference exists only on the owner's live tree must answer NonTerminal (a \
         Freed here destroys a live inode's block — §6.3's reclaim hazard)"
    );
}
