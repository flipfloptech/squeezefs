//! **Per-volume claim admission — the PARTIAL-AUTHORITY ARM**
//! (`docs/design-per-volume-claim-admission.md` §5.7's rev-6 correction 2,
//! §5.10, D19/D20, KD-PV-3/12/17; PR 7b).
//!
//! # The hole this rung fills, in the design's own words
//!
//! §5.7 describes what a partial authority must NOT take (the set-singular
//! planes) and PR 4 wired what such a mount DOES once it is in the
//! posture; PR 7 built the verb that creates the assignment. **Nothing
//! built the act that ENTERS the posture** — and PR 5 closed the only door
//! that half-worked: `arm_multi_writer` now refuses a non-set-authority
//! outright rather than arming a mount that would grant custody nobody may
//! hold or mint lanes nobody granted. So a fleet could be *assigned* and
//! could *open*, and then had no arm. This file is the contract for the
//! arm that closes it.
//!
//! # The composition, which is the whole point
//!
//! A partial authority is the only posture that is a CLIENT and an OWNER
//! at once, and the two halves are not new machinery — they are the two
//! shipped ones, composed:
//!
//! | half | over | installed by |
//! |---|---|---|
//! | client | the volumes a PEER appends to | `cowriter::install_client_halves` — the same custody lease, lane grant, publish client, daemon verb router, closed local accounting and renewal cadence a co-writer arms |
//! | owner | the volumes THIS node appends to | an S8 `MetaShipService` **scoped to exactly those volumes** + the S9 `PublishService`, on a listener with **no custody service** (D20) |
//!
//! Every contract below is therefore about the SEAM: what each half
//! installs, what the arm refuses rather than half-installing, and what it
//! must never take because D20 gave it to the set authority.
//!
//! **RED against `dev` (23b3763d)**: `multi_writer::arm_partial_authority`
//! does not exist, `cowriter::install_client_halves` does not exist, and
//! `owners::{refresh_peer_endpoints, unresolved_peer_endpoints,
//! OwnerMap::local_volume_set}` do not exist — a mount that owns SOME
//! volumes has no arm at all, which is the hole this rung fills.
//!
//! # What one process cannot pin (stated, not hidden)
//!
//! The mw suites' standing limit applies verbatim: a real fleet is two
//! hosts, a PR-capable fabric and two independent D0 claims. Here the SET
//! AUTHORITY is an in-process fixture — a real `WriteCustodyOwner`, a real
//! `PublishService` and a real `MetaShipService` on the real cluster wire,
//! over its own volume — while the partial mount's peer-owned volume
//! carries a planted foreign claim with the KD-PV-17 attestation its owner
//! would have written (`classify_claim` reads a same-pid claim as our own
//! residue, so an in-process peer cannot BE the D0 holder). The wire, the
//! admission, the derivation, the lane and the arm are real.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::cluster_wire as cw;
use squeezefs::config_ops::{self, SetOwnersOptions};
use squeezefs::cowriter::{AuthorityLeaseEvidence, MwRole, RegistrantEvidence};
use squeezefs::data_grant::{self, WriteCustodyOwner};
use squeezefs::dlm::DlmClient;
use squeezefs::membership::{ClaimHolder, ClaimSet, LeaseClock, LeaseClocks, MemberRole};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR};
use squeezefs::meta_backend::kv::builder::FormatV3Options;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::meta_ship::{self as ship, owners, publish, OwnerMap, PeerOwner};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::partial_authority::{
    self as pv, ClaimStanding, PvVolumeEvidence, SetAdmission, SetAdmissionRequest, SetPreflight,
};
use squeezefs::routing::DataRouter;
use squeezefs::{DataVolumeRecord, FormatConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;
const BLOCK: usize = 4096;

/// This node — the PARTIAL authority. **The process's REAL durable
/// identity** (KD-MW-2's `node_{16 hex}[.m{8 hex}]`), not a literal: the
/// arms resolve it themselves (`cowriter::node_member_id`), so a fixture
/// that assigned a made-up id would derive a map in which this mount's own
/// D0 claims disagree with the assignment — a refusal about the FIXTURE
/// rather than about the product.
fn node() -> &'static str {
    static NODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NODE.get_or_init(|| squeezefs::cowriter::node_member_id().expect("a stable node identity"))
}
/// The SET AUTHORITY: the owner of the slot-0 volume (D20).
const AUTH: &str = "node_00000000feedface.m00000001";

/// The `job:enroll`-class storage-trust secret both halves authenticate
/// against (ruling D2's root of trust).
const SECRET: &[u8] = b"pv-partial-arm-storage-trust-secret";

/// A DECOY endpoint, written into the set authority's claim-set member
/// entry the way an armed membership OWNER writes its own: that record
/// carries the MEMBERSHIP plane's address, and nothing metadata-shipped
/// can be served there. D20's declared `SQUEEZEFS_MW_AUTHORITY` must win.
const MEMBERSHIP_DECOY: &str = "127.0.0.1:9";

// ---------------------------------------------------------------------------
// Serialization + restoration (every plane here is process-global)
// ---------------------------------------------------------------------------

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_mount_posture(squeezefs::fuse_client::MountPosture::Writer);
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        ship::uninstall_daemon_verb_router();
        ship::uninstall_delegation_host();
        squeezefs::alloc_lane_grant::uninstall_frontier_source();
        squeezefs::meta_backend::kv::block_refs::uninstall_block_ref_resolver();
        squeezefs::meta_backend::kv::indirect_map::uninstall_indirect_map_io();
        squeezefs::data_alloc_lane::test_reset_mount_partition();
        ship::disarm_ownership();
        // The mount-path selection pin below declares a posture through
        // the environment, and every knob here is process-global.
        for k in [
            "SQUEEZEFS_MULTI_WRITER",
            "SQUEEZEFS_MW_ROLE",
            "SQUEEZEFS_MW_AUTHORITY",
        ] {
            std::env::remove_var(k);
        }
    }
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

/// The nine-bit multi-writer stamp — `volume enable-multi-writer`'s
/// offline act, which the DEFAULT format performs since the rung-10b flip.
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

async fn volume_set(dir: &Path, tag: &str, n: usize) -> Vec<PathBuf> {
    // PR 12: the per-volume-owner recipe is the FLAT multi-writer class's
    // — on a bit-17 set `volume set-owners` is RETIRED (ownership is a slot
    // lease), so these fixtures format FLAT whatever leg the environment
    // selects (the seam is the forest suites' stamp, cleared around the
    // format the way `sym_convert_tests` does).
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

fn uris(vols: &[PathBuf]) -> Vec<String> {
    vols.iter().map(|p| p.display().to_string()).collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A live FOREIGN claim: another host's boot id, so no dead-pid proof
/// reclaims it and the D0 ladder reads it as `Fresh`.
fn foreign_claim() -> WriterClaim {
    WriterClaim {
        id: "5f1d0e2a-0000-4000-8000-0000000000a1".to_string(),
        ts: now_secs() + 2,
        pid: 4242,
        boot: "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string(),
        term: 7,
    }
}

/// PR 7's verb, run the way `docs/operations.md` documents it: a dry run
/// to learn the cross-owner census, then the acknowledged assignment.
async fn assign(u: &[String], specs: &[config_ops::OwnerAssignSpec]) {
    let plan = config_ops::set_owners(
        u,
        specs,
        &SetOwnersOptions {
            dry_run: true,
            ..SetOwnersOptions::default()
        },
    )
    .await
    .expect("the dry run reports the plan");
    config_ops::set_owners(
        u,
        specs,
        &SetOwnersOptions {
            accept_cross_owner_names: Some(plan.census.total),
            ..SetOwnersOptions::default()
        },
    )
    .await
    .expect("the acknowledged assignment applies");
}

fn spec(vol_id: &str, owner: &str, root: &str) -> config_ops::OwnerAssignSpec {
    config_ops::OwnerAssignSpec {
        volume_id: vol_id.to_string(),
        owner: owner.to_string(),
        successors: Vec::new(),
        subtree_root: Some(root.to_string()),
    }
}

/// Everything the SET AUTHORITY would have written on the volume it
/// appends to, written here because an in-process peer cannot hold a D0
/// claim this process would read as foreign: the KD-PV-17 holder
/// attestation, the membership-plane DECOY endpoint on its member entry,
/// and then the claim itself.
async fn plant_authority_claim(path: &Path, claim: &WriterClaim) {
    let be = KvMetaBackend::open(path).await.expect("open to plant");
    let mut set = ClaimSet::load(&be)
        .await
        .expect("the verb wrote a claim set");
    assert!(set.durable, "the verb writes a DURABLE record");
    set.holder = Some(ClaimHolder {
        id: AUTH.to_string(),
        writer_id: claim.id.clone(),
        pid: claim.pid,
        boot: claim.boot.clone(),
    });
    for m in set.members.iter_mut() {
        if squeezefs::membership::member_id_matches(&m.identity.id, AUTH) {
            m.identity.endpoint = Some(MEMBERSHIP_DECOY.to_string());
        }
    }
    ClaimSet::store(&be, &set).await.expect("store claim set");
    be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
        .await
        .expect("plant the claim");
    be.sync_device().await.expect("barrier");
    // DROP, never `shutdown()`: a clean shutdown releases the claim, and
    // what this fixture needs on disk is a LIVE foreign holder's record
    // (the PR 4 suite's `plant` shape).
    drop(be);
}

// ---------------------------------------------------------------------------
// The in-process SET AUTHORITY
// ---------------------------------------------------------------------------

/// The set authority as a peer sees it: a custody owner with this era's
/// lane assignment, the publish + metadata verb blocks on one listener,
/// and its own metadata store (it cannot open the partial mount's peer
/// volume in this process — see the module docs).
struct Authority {
    listener: Arc<cw::RpcListener>,
    svc: Arc<ship::MetaShipService>,
    meta: Arc<RoutedMetaBackend>,
    endpoint: String,
}

impl Authority {
    async fn start(dir: &Path, roster: &ClaimSet) -> Authority {
        let vols = volume_set(dir, "authority", 1).await;
        let meta = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
            .await
            .expect("the set authority mounts its own store");
        let owner = WriteCustodyOwner::arm(
            "mw-set-authority",
            squeezefs::dlm::durable_term() + 1,
            squeezefs::dlm::durable_term(),
            LeaseClocks::with_params(
                std::time::Duration::from_millis(30_000),
                std::time::Duration::from_millis(2_000),
                std::time::Duration::from_millis(4_000),
            )
            .expect("positive T_self"),
            LeaseClock::monotonic(),
            None,
        )
        .expect("the custody authority arms");
        // D20: ONLY the set authority derives the era's lane width, from
        // the durable claim set the offline verb wrote.
        let assignment =
            squeezefs::alloc_lane_grant::LaneAssignment::derive(AUTH, std::slice::from_ref(roster))
                .expect("the roster fits the lane space");
        owner.install_lane_assignment(assignment);
        data_grant::install_custody_owner(Arc::clone(&owner));
        let svc = ship::MetaShipService::new(Arc::clone(&meta));
        let router = data_grant::AsyncVerbRouter::new()
            .with_custody(Arc::clone(&owner))
            .with_publish(publish::PublishService::new(Arc::clone(&meta)))
            .with_meta(Arc::clone(&svc));
        let listener = cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            SECRET.to_vec(),
            Arc::new(router),
        )
        .expect("the set authority listens");
        let endpoint = listener.endpoint().to_string();
        Authority {
            listener,
            svc,
            meta,
            endpoint,
        }
    }

    async fn stop(self) {
        self.listener.shutdown();
        data_grant::uninstall_custody_owner();
        for v in &self.meta.volumes {
            v.shutdown().await.expect("clean shutdown");
        }
    }
}

// ---------------------------------------------------------------------------
// The PARTIAL AUTHORITY's mount
// ---------------------------------------------------------------------------

fn evidence(
    path: &Path,
    id: &str,
    hosts_slot_0: bool,
    claim: Option<WriterClaim>,
    holder: Option<&str>,
    set: ClaimSet,
    endpoint: Option<&str>,
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
        owner_endpoint: endpoint.map(str::to_string),
    }
}

/// The seven-rung ladder's real verdict over this fixture: vol0 (slot 0)
/// is the set authority's, vol1 is ours.
async fn partial_admission(vols: &[PathBuf], claim: &WriterClaim, authority: &str) -> SetAdmission {
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let set0 = read_claim_set(&vols[0]).await;
    let set1 = read_claim_set(&vols[1]).await;
    let req = SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::PartialAuthority,
        read_only: false,
        node_id: node().to_string(),
        set_authority_endpoint: Some(authority.to_string()),
        volumes: vec![
            evidence(
                &vols[0],
                &id0,
                true,
                Some(claim.clone()),
                Some(AUTH),
                set0,
                Some(authority),
            ),
            evidence(&vols[1], &id1, false, None, None, set1, None),
        ],
        authority: Some(AuthorityLeaseEvidence {
            owner_id: AUTH.to_string(),
            endpoint: authority.to_string(),
            owner_claim_id: AUTH.to_string(),
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

async fn read_claim_set(path: &Path) -> ClaimSet {
    let probe = KvMetaBackend::open_probe(path).await.expect("probe open");
    ClaimSet::load(&probe).await.expect("a durable claim set")
}

fn base_format_config(data_lv: &Path) -> FormatConfig {
    FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BLOCK as u64,
        capacity: 1 << 34,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(vec![data_lv.display().to_string()]),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    }
}

async fn data_router(dir: &Path, records: &[DataVolumeRecord]) -> (DataRouter, TempDir) {
    let dlm = DlmClient::new().unwrap();
    let first = &records[0];
    let dev = Arc::new(NvmeBlockDev::new(&first.backing_dev));
    let alloc = Arc::new(BlockAllocator::new(&first.id).await.unwrap());
    if let Ok(cap) = squeezefs::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        alloc.set_capacity_bytes(cap);
    }
    let staging = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        alloc.clone(),
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, alloc, dev);
    for rec in records {
        router
            .backend_router
            .register_backend(rec)
            .await
            .unwrap_or_else(|e| panic!("register_backend({}) failed: {e:?}", rec.id));
    }
    router.backend_router.set_volume_records(records.to_vec());
    let _ = dir;
    (router, staging)
}

/// The whole fixture: an ASSIGNED two-volume set, a live set authority,
/// and this node's partial-authority mount — everything the arm consumes,
/// built the way an operator builds it.
struct Fleet {
    meta: Arc<RoutedMetaBackend>,
    router: DataRouter,
    admission: SetAdmission,
    authority: Authority,
    vols: Vec<PathBuf>,
    _staging: TempDir,
    _dir: TempDir,
}

impl Fleet {
    async fn build() -> Fleet {
        let dir = TempDir::new().unwrap();
        let vols = volume_set(dir.path(), "set", 2).await;
        let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
        // D19: the assignment is an operator act, through PR 7's verb.
        assign(
            &uris(&vols),
            &[
                spec(&id0, AUTH, "/authority"),
                spec(&id1, node(), "/partial"),
            ],
        )
        .await;
        let claim = foreign_claim();
        plant_authority_claim(&vols[0], &claim).await;

        let roster = read_claim_set(&vols[0]).await;
        let authority = Authority::start(dir.path(), &roster).await;

        squeezefs::fuse_client::set_mount_posture(
            squeezefs::fuse_client::MountPosture::PartialAuthority,
        );
        let admission = partial_admission(&vols, &claim, &authority.endpoint).await;
        let meta = squeezefs::meta_backend::open_routed_meta_set_partial(&uris(&vols), &admission)
            .await
            .expect("the partial open serves the volumes this node owns");
        let data_lv = dir.path().join("oss1");
        std::fs::File::create(&data_lv)
            .unwrap()
            .set_len(1 << 30)
            .unwrap();
        let records = base_format_config(&data_lv).resolved_data_volumes();
        let (router, staging) = data_router(dir.path(), &records).await;
        router.set_meta_backend(meta.clone());
        Fleet {
            meta,
            router,
            admission,
            authority,
            vols,
            _staging: staging,
            _dir: dir,
        }
    }

    /// The preflight the mount path hands the arm (its membership lease
    /// and device registration are the ladder's, and `None` here says so:
    /// this fixture proves the ARM, not rungs 4/5).
    fn preflight(&self) -> SetPreflight {
        SetPreflight {
            admission: self.admission.clone(),
            membership: None,
            registrant: None,
            hold: None,
            secret: SECRET.to_vec(),
        }
    }

    async fn close(self) {
        self.authority.stop().await;
        for v in &self.meta.volumes {
            let _ = v.shutdown().await;
        }
    }
}

/// The first ino this set homes on volume `v` — what a shipped verb about
/// that volume names.
fn ino_on_volume(meta: &RoutedMetaBackend, v: usize) -> u64 {
    (1u64..4096)
        .find(|&ino| meta.route_ino(ino).0 == v)
        .unwrap_or_else(|| panic!("no ino in 1..4096 routes to volume {v}"))
}

// ===========================================================================
// 1. The arm itself: both halves, and only the halves D20 allows
// ===========================================================================

/// Contract (**the rung**): a mount that owns SOME volumes arms BOTH
/// halves in one act — the client half over the volumes a peer appends to
/// (a custody lease from the set authority, the allocation lane that lease
/// carries, the publish client, the daemon verb router) and the owner half
/// over the volumes it appends to (a listener serving the S8 metadata
/// verbs and the S9 publish path) — and takes NONE of the set-singular
/// planes D20 gives the set authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partial_authority_arms_both_halves_over_a_verb_created_assignment() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let fleet = Fleet::build().await;

    let arm = squeezefs::multi_writer::arm_partial_authority(
        &fleet.meta,
        &fleet.router,
        fleet.preflight(),
    )
    .await
    .expect("the partial-authority arm composes both halves");

    // The DERIVED map (KD-PV-3), not a caller's assertion: vol0 ships to
    // the set authority, vol1 is ours.
    let map = owners::owner_map().expect("the ownership plane is armed");
    assert_eq!(map.local_volume_set(), vec![1], "this node appends to vol1");
    let peer = map.owner_of_volume(0).expect("vol0 is peer-owned").clone();
    assert_eq!(peer.peer_id, AUTH, "verbs follow the HOLDER (§5.10)");
    assert!(!map.owns_slot_0(), "slot 0 is the set authority's (D20)");

    // D20's endpoint law: the declared set-authority endpoint WINS over
    // the membership-plane address the roster carries for it. Reading the
    // roster first sent every metadata verb to a port that answers
    // RPC_UNKNOWN_VERB.
    assert_eq!(
        peer.endpoint, fleet.authority.endpoint,
        "the slot-0 owner's endpoint is the DECLARED custody/publish one, never the \
         membership decoy {MEMBERSHIP_DECOY} its member entry carries"
    );

    // The CLIENT half. The arm's own handle and the process-global one are
    // the same client — the arm holds what it installed.
    let client = data_grant::custody_client().expect("the custody client is installed");
    assert!(
        Arc::ptr_eq(&client, arm.client()),
        "the arm holds the custody client it installed"
    );
    assert_eq!(client.endpoint(), fleet.authority.endpoint);
    let lane = arm.client().lane_partition();
    assert!(
        !lane.is_solo() && lane.writers() == 2 && lane.writer_id() == 1,
        "the allocation lane is the one the SET AUTHORITY granted on the lease, never one this \
         mount derived: got {} of {}",
        lane.writer_id(),
        lane.writers()
    );
    assert_eq!(
        squeezefs::data_alloc_lane::mount_partition().writer_id(),
        1,
        "and it is ENGAGED on this mount's allocators"
    );
    // The daemon verb router routes by VOLUME, which is what makes one
    // router correct for a mount that owns some: peer inos ship, own inos
    // stay local.
    let peer_ino = ino_on_volume(&fleet.meta, 0);
    let own_ino = ino_on_volume(&fleet.meta, 1);
    assert!(
        ship::daemon_verb_router(&fleet.meta, &[peer_ino]).is_some(),
        "a verb about the peer's volume must SHIP"
    );
    assert!(
        ship::daemon_verb_router(&fleet.meta, &[own_ino]).is_none(),
        "a verb about a volume this node APPENDS to must stay local — a partial authority that \
         shipped its own volumes' verbs would be a co-writer"
    );

    // The OWNER half: a listener of its own, reachable.
    assert!(
        !arm.endpoint().is_empty(),
        "the owner half publishes where peers reach the volumes this node owns"
    );
    // And NOT the set-singular planes (D20): the owner half serves the
    // metadata and publish blocks and NOTHING of the custody block, so a
    // peer that mistook this node for the custody source is told the verb
    // does not exist here rather than being granted bytes twice. (The
    // process-global custody OWNER in this fixture is the in-process set
    // authority's — one process plays both nodes — so the honest
    // assertion is the one a peer can make: over the wire.)
    let mut probe = cw::RpcClient::connect(arm.endpoint(), SECRET, "a-peer", None)
        .await
        .expect("a peer can reach the owner half");
    let reply = probe
        .call(data_grant::VERB_CUSTODY_ACQUIRE, Vec::new())
        .await
        .expect("the listener answers");
    assert_eq!(
        reply.status,
        cw::RPC_UNKNOWN_VERB,
        "a partial authority grants NO custody: the hold, the endpoint and the grant table are \
         the set authority's (D20)"
    );

    arm.disarm().await;
    assert!(
        !ship::ownership_armed() && data_grant::custody_client().is_none(),
        "disarm leaves neither half standing"
    );
    fleet.close().await;
}

// ===========================================================================
// 2. The owner half is SCOPED — it serves its own volumes and refuses the rest
// ===========================================================================

/// Contract (§5.7's correction 2, *"an owner half serving only the volumes
/// it appends to"*): the S8 service the arm starts holds authority over
/// exactly this node's volumes, so a peer's frame about one of them is
/// EXECUTED and a frame about a volume the set assigns elsewhere meets the
/// `not_owner` refusal — the map divergence caught at the owner rather
/// than silently executed by a node the record does not entitle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owner_half_serves_only_the_volumes_this_node_appends_to() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let fleet = Fleet::build().await;
    let arm = squeezefs::multi_writer::arm_partial_authority(
        &fleet.meta,
        &fleet.router,
        fleet.preflight(),
    )
    .await
    .expect("the arm composes");
    let endpoint = arm.endpoint().to_string();
    let own_ino = ino_on_volume(&fleet.meta, 1);
    let peer_ino = ino_on_volume(&fleet.meta, 0);

    // A PEER's client half, pointed at this mount's owner half. The
    // ownership map is process-global, so installing the peer's view
    // replaces ours — which is exactly why the service's authority set is
    // frozen at construction and read from ITS OWN answer, never from
    // whatever map is installed when a frame lands.
    let peer_dir = TempDir::new().unwrap();
    let peer_vols = volume_set(peer_dir.path(), "peer", 1).await;
    let peer_be = squeezefs::meta_backend::open_routed_meta_set(&uris(&peer_vols))
        .await
        .expect("the peer mounts its own set");
    ship::arm_ownership(
        OwnerMap::for_volumes(&peer_be, vec![(0, PeerOwner::new(node(), &endpoint))])
            .expect("an all-foreign map toward the partial authority"),
    );
    let peer_router = ship::MetaShipRouter::new(Arc::clone(&peer_be), "a-peer", SECRET.to_vec());

    let before = ship::stats();
    let served = peer_router
        .setattr(own_ino, Some(0o600), None, None, None, None, None, None)
        .await;
    let served_detail = match &served {
        Ok(_) => String::new(),
        Err(e) => format!("{e}"),
    };
    assert!(
        !served_detail.contains("holds no authority over"),
        "a verb about a volume this node APPENDS to must be EXECUTED (whatever the inode's own \
         answer is), never refused by authority: {served_detail}"
    );
    assert_eq!(
        ship::stats().served_verbs - before.served_verbs,
        1,
        "the owner half's ledger counts the execution"
    );

    let refused = peer_router
        .setattr(peer_ino, Some(0o600), None, None, None, None, None, None)
        .await
        .expect_err("a verb about a volume the set assigns to a PEER must be refused");
    let msg = format!("{refused}");
    assert!(
        msg.contains("holds no authority over"),
        "the refusal names the reason so a stale client learns it: {msg}"
    );
    assert_eq!(
        ship::stats().not_owner_refusals - before.not_owner_refusals,
        1,
        "and it lands on the not_owner ledger, never on the served one"
    );
    assert_eq!(
        ship::stats().served_verbs - before.served_verbs,
        1,
        "the refused frame executed nothing"
    );

    ship::disarm_ownership();
    for v in &peer_be.volumes {
        v.shutdown().await.expect("peer shutdown");
    }
    arm.disarm().await;
    fleet.close().await;
}

// ===========================================================================
// 3. Fail-closed: refuse rather than hold one half
// ===========================================================================

/// Contract (KD-PV-3's fail-closed law, the ARMING face): an arm that
/// cannot reach the set authority refuses **and leaves nothing standing**
/// — no ownership plane, no owner-half listener, no publish client, no
/// daemon verb router. A mount holding one half is worse than a refused
/// mount: it would serve peers a metadata authority whose data plane can
/// never write a byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_set_authority_refuses_the_arm_and_leaves_nothing_installed() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let fleet = Fleet::build().await;
    // The authority's listener is gone — the admission and every durable
    // record still name it, which is the shape a fleet meets when the set
    // authority is down.
    fleet.authority.listener.shutdown();

    let err = squeezefs::multi_writer::arm_partial_authority(
        &fleet.meta,
        &fleet.router,
        fleet.preflight(),
    )
    .await
    .expect_err("an unreachable set authority must refuse the arm");
    let msg = format!("{err}");
    assert!(
        msg.contains("partial authority arm failed") && msg.contains(&fleet.authority.endpoint),
        "the refusal names the posture and the endpoint it could not reach: {msg}"
    );

    assert!(
        !ship::ownership_armed(),
        "a refused arm leaves no ownership plane — half a posture is not a posture"
    );
    assert!(data_grant::custody_client().is_none(), "no custody client");
    assert!(
        ship::daemon_verb_router(&fleet.meta, &[1]).is_none(),
        "no daemon verb router"
    );
    assert_eq!(
        owners::unresolved_peer_endpoints(),
        0,
        "and no map at all, so nothing is waiting to be resolved"
    );
    fleet.close().await;
}

// ===========================================================================
// 4. The two arms are exclusive doors, both ways
// ===========================================================================

/// Contract (D20, PR 5's refusal preserved): the SET authority's arm still
/// refuses a mount that does not own the slot-0 volume — that refusal is
/// unchanged — and the PARTIAL arm refuses a set-authority decision for
/// the mirror reason. Neither posture can enter through the other's door.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_set_authority_arm_and_the_partial_arm_are_exclusive_doors() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let fleet = Fleet::build().await;

    // PR 5's refusal, verbatim: reaching `arm_multi_writer` as a partial
    // authority refuses before anything is acquired.
    let err = squeezefs::multi_writer::arm_multi_writer(
        &fleet.meta,
        &[],
        false,
        None,
        None,
        Some(&fleet.admission),
    )
    .await
    .expect_err("the SET authority's arm refuses a partial authority");
    let msg = format!("{err}");
    assert!(
        msg.contains("PARTIAL AUTHORITY under D20") && msg.contains(AUTH),
        "it names the posture and the node that IS the set authority: {msg}"
    );
    assert!(
        !ship::ownership_armed(),
        "and it arms nothing on the way out — the refusal is BEFORE the substrate, the \
         membership plane and the roster commit"
    );

    // The mirror: a set-authority decision cannot enter the partial arm.
    let set_auth = SetPreflight {
        admission: set_authority_admission(&fleet.vols).await,
        membership: None,
        registrant: None,
        hold: None,
        secret: SECRET.to_vec(),
    };
    let err = squeezefs::multi_writer::arm_partial_authority(&fleet.meta, &fleet.router, set_auth)
        .await
        .expect_err("the partial arm refuses a set-authority decision");
    let msg = format!("{err}");
    assert!(
        msg.contains("set-authority") && msg.contains("arm_multi_writer"),
        "it names the arm that posture belongs to: {msg}"
    );
    assert!(!ship::ownership_armed(), "and installs nothing");
    fleet.close().await;
}

/// A `set-authority` decision over the same set (vol0 — slot 0 — is ours).
async fn set_authority_admission(vols: &[PathBuf]) -> SetAdmission {
    let (id0, id1) = (vol_id(&vols[0]).await, vol_id(&vols[1]).await);
    let mut set0 = read_claim_set(&vols[0]).await;
    set0.owner = Some(node().to_string());
    set0.holder = None;
    let set1 = read_claim_set(&vols[1]).await;
    let req = SetAdmissionRequest {
        multi_writer: true,
        role: MwRole::SetAuthority,
        read_only: false,
        node_id: node().to_string(),
        set_authority_endpoint: None,
        volumes: vec![
            evidence(&vols[0], &id0, true, None, None, set0, None),
            evidence(&vols[1], &id1, false, None, None, set1, None),
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
    pv::classify_set_admission(&req).expect("a set-authority decision over the same set")
}

// ===========================================================================
// 5. Publishing WHERE this node serves — and not breaking KD-PV-4 doing it
// ===========================================================================

/// Contract (the fleet's other endpoint half): the arm publishes its owner
/// half's address on the volumes it OWNS, so a peer deriving a map can
/// ship to it at all — and it does so by EDITING the enrollment the
/// offline verb wrote, leaving KD-PV-4's **pid-less** form intact. A
/// live-pid roster entry is prunable by the rung-8 same-boot dead-writer
/// sweep, and pruning an entry the record still ASSIGNS a volume to is the
/// assignment-vs-enrollment disagreement that exemption exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_arm_publishes_where_it_serves_and_keeps_the_enrollment_pid_less() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let fleet = Fleet::build().await;
    let before = read_claim_set(&fleet.vols[0]).await;
    let arm = squeezefs::multi_writer::arm_partial_authority(
        &fleet.meta,
        &fleet.router,
        fleet.preflight(),
    )
    .await
    .expect("the arm composes");
    let endpoint = arm.endpoint().to_string();
    arm.disarm().await;

    let owned = read_claim_set(&fleet.vols[1]).await;
    let me = owned
        .members
        .iter()
        .find(|m| squeezefs::membership::member_id_matches(&m.identity.id, node()))
        .expect("the verb enrolled this node on every volume");
    assert_eq!(
        me.identity.endpoint.as_deref(),
        Some(endpoint.as_str()),
        "the volume this node APPENDS to names where its owner serves"
    );
    assert_eq!(me.identity.pid, 0, "KD-PV-4's pid-less form survives");
    assert!(me.identity.boot.is_empty(), "and so does its empty boot");
    assert_eq!(me.identity.role, MemberRole::Writer);
    assert_eq!(
        owned.owner.as_deref(),
        Some(node()),
        "the assignment itself is untouched"
    );

    let peer_now = read_claim_set(&fleet.vols[0]).await;
    assert_eq!(
        peer_now.members.len(),
        before.members.len(),
        "and the PEER's volume is not written at all — its roster is its owner's"
    );
    assert_eq!(
        peer_now
            .members
            .iter()
            .find(|m| squeezefs::membership::member_id_matches(&m.identity.id, node()))
            .and_then(|m| m.identity.endpoint.clone()),
        None,
        "a partial authority never edits a peer-owned volume's record"
    );
    fleet.close().await;
}

// ===========================================================================
// 6. `note_era_relearn` — the end-to-end FLEET pin PR 5 left owed
// ===========================================================================

/// Contract (§5.10's runtime conjunction, end to end): when the SET
/// AUTHORITY restarts into a higher era, the partial authority's next
/// shipped verb is refused BY ERA, the client relearns it, and the relearn
/// re-derives that volume from a FRESH read. Because the durable
/// assignment still names the holder, the entry is **not** poisoned, the
/// map still ships there, and the retry succeeds.
///
/// PR 5 pinned the predicate in isolation; this drives the real router,
/// the real refusal and the real relearn site through a real arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authority_restart_relearns_the_era_without_poisoning_the_derived_map() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let fleet = Fleet::build().await;
    let arm = squeezefs::multi_writer::arm_partial_authority(
        &fleet.meta,
        &fleet.router,
        fleet.preflight(),
    )
    .await
    .expect("the arm composes");

    // One shipped verb, so this client learns the owner's era.
    fleet
        .meta
        .create(1, "before-failover", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a verb about the peer's volume ships and executes there");

    // The failover: a successor bumps `term` DURABLY before arming (S2),
    // so every frame the client still builds on the old era is stale by
    // construction.
    let before = ship::stats();
    fleet
        .authority
        .svc
        .bump_term(squeezefs::dlm::durable_term() + 5);
    let stale = fleet
        .meta
        .create(1, "old-era", libc::S_IFREG | 0o644, 0, 0)
        .await;
    assert!(stale.is_err(), "an old-era frame is refused WHOLE");
    assert_eq!(
        ship::stats().era_relearns - before.era_relearns,
        1,
        "the client's own counter records the relearn (never the owner's)"
    );

    // The relearn's follow-up, run to its verdict rather than raced: a
    // FRESH read of the volume agrees, because the durable assignment
    // names its holder.
    assert!(
        owners::reconcile_volume_owner(&fleet.meta, 0)
            .await
            .expect("the volume re-reads"),
        "assignment ∧ evidence still agree, so the re-derivation reaches an agreeing verdict"
    );
    assert_eq!(
        owners::poisoned_volumes(),
        0,
        "owner_map_poisoned_volumes must stay 0 across an ordinary failover"
    );
    assert_eq!(
        owners::owner_map()
            .and_then(|m| m.owner_of_volume(0).map(|p| p.peer_id.clone()))
            .as_deref(),
        Some(AUTH),
        "and the map still ships that volume to the node the record entitles"
    );

    // The relearned era makes the retry admissible again.
    fleet
        .meta
        .create(1, "new-era", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("the client relearned the successor's era");

    arm.disarm().await;
    fleet.close().await;
}

/// Contract (PR 5's own caught regression, at fleet level): the same
/// authority restart against a map built by `OwnerMap::for_volumes` — the
/// CO-WRITER's constructor, where no durable `claim_set.owner` stands
/// behind the caller's assertion — poisons nothing. Before the fix, the
/// relearn that follows every ordinary authority restart re-derived every
/// volume, resolved a holder the map's per-mount-uuid peer id did not
/// match, and fail-stopped a healthy co-writer's whole set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_relearn_never_poisons_a_map_built_for_a_co_writer() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let fleet = Fleet::build().await;

    // The co-writer's shape: every volume owned by the authority it
    // dialled, asserted by the caller rather than derived.
    ship::arm_ownership(
        OwnerMap::for_volumes(
            &fleet.meta,
            (0..fleet.meta.volumes.len())
                .map(|v| {
                    (
                        v,
                        PeerOwner::new("mw-set-authority", &fleet.authority.endpoint),
                    )
                })
                .collect(),
        )
        .expect("an all-foreign map"),
    );
    publish::install_client(publish::PublishClient::new(node(), SECRET.to_vec()));
    ship::install_daemon_verb_router(ship::MetaShipRouter::new(
        Arc::clone(&fleet.meta),
        node(),
        SECRET.to_vec(),
    ));

    fleet
        .meta
        .create(1, "learn", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("a shipped create executes on the authority");
    let before = ship::stats();
    fleet
        .authority
        .svc
        .bump_term(squeezefs::dlm::durable_term() + 9);
    assert!(
        fleet
            .meta
            .create(1, "stale", libc::S_IFREG | 0o644, 0, 0)
            .await
            .is_err(),
        "the old-era frame is refused"
    );
    assert_eq!(
        ship::stats().era_relearns - before.era_relearns,
        1,
        "the relearn fires exactly as it does for a derived map"
    );
    assert!(
        owners::reconcile_volume_owner(&fleet.meta, 0)
            .await
            .expect("the volume re-reads"),
        "with no durable assignment behind the entry there is nothing to disagree WITH, so the \
         conjunction reaches no adverse verdict"
    );
    assert_eq!(
        owners::poisoned_volumes(),
        0,
        "a healthy co-writer must not be fail-stopped by its authority's restart"
    );
    fleet.close().await;
}

// ===========================================================================
// 7. The shipped path is untouched
// ===========================================================================

/// Contract (R12's law, this rung's face): an UNASSIGNED set — every set
/// in the field until an operator runs `volume set-owners` — derives the
/// all-local map, so the set authority's arm scopes its metadata service
/// to every volume (`with_authority` ≡ `new`), publishes no endpoint and
/// spawns no refresh cadence. The multi-owner machinery is reachable only
/// through an assignment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unassigned_set_derives_the_all_local_map_and_arms_none_of_it() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "solo", 2).await;
    let meta = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("an ordinary write mount");

    let map = owners::derive_owner_map(&meta, node(), &|_| None)
        .await
        .expect("an unassigned set derives all-local");
    assert!(!map.multi_owner(), "no volume is owned elsewhere");
    assert_eq!(
        map.local_volume_set(),
        vec![0, 1],
        "so the S8 service's authority set is every volume — `with_authority` is `new`"
    );
    assert!(map.owns_slot_0(), "and this node IS the set authority");

    ship::arm_ownership(Arc::clone(&map));
    assert_eq!(
        owners::unresolved_peer_endpoints(),
        0,
        "nothing to resolve, so the refresh cadence never starts"
    );
    assert_eq!(
        owners::refresh_peer_endpoints(&|_| Some("127.0.0.1:1".to_string())),
        0,
        "and a refresh pass over an all-local map is a no-op"
    );
    ship::disarm_ownership();
    for v in &meta.volumes {
        v.shutdown().await.expect("clean shutdown");
    }
}

/// Contract (the fleet-ordering fix, isolated): a map derived before a
/// peer published its endpoint installs the entry WITHOUT one and the
/// refresh pass fills it in later — without moving ownership. The set
/// authority always derives its map before any peer is admitted (rung 4
/// requires its membership plane to be live first), so without this the
/// endpoint-less entry would stand for the life of the mount and every
/// verb about a peer's volume would refuse forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_that_publishes_late_is_resolved_by_the_refresh_pass() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;
    let dir = TempDir::new().unwrap();
    let vols = volume_set(dir.path(), "late", 1).await;
    let meta = squeezefs::meta_backend::open_routed_meta_set(&uris(&vols))
        .await
        .expect("a set to hang the map on");
    ship::arm_ownership(
        OwnerMap::for_volumes(&meta, vec![(0, PeerOwner::new(AUTH, ""))])
            .expect("a peer entry with no endpoint yet"),
    );
    assert_eq!(owners::unresolved_peer_endpoints(), 1);

    assert_eq!(
        owners::refresh_peer_endpoints(&|id| (id == AUTH).then(|| "127.0.0.1:7100".to_string())),
        1,
        "the pass fills exactly the entries whose owner has published"
    );
    let map = owners::owner_map().expect("armed");
    let peer = map.owner_of_volume(0).expect("still peer-owned");
    assert_eq!(
        peer.peer_id, AUTH,
        "ownership did not move — only the address"
    );
    assert_eq!(peer.endpoint, "127.0.0.1:7100");
    assert_eq!(owners::unresolved_peer_endpoints(), 0);

    ship::disarm_ownership();
    for v in &meta.volumes {
        v.shutdown().await.expect("clean shutdown");
    }
}

// ===========================================================================
// The MOUNT PATH's own gate (PR 8's blocker)
// ===========================================================================

/// Contract: **a declared per-volume posture is SELECTABLE by the mount
/// path.** `src/main.rs` resolves `cowriter::co_writer_requested()` before
/// it reads `partial_authority::requested()`, so anything that refuse
/// there makes both per-volume roles unreachable no matter how complete
/// the ladder, the partial open and the arms are — which is exactly what
/// PR 3's scaffolding did, and what every in-process contract in this file
/// stepped over by calling the arm directly. A per-volume role is not a
/// co-writer declaration: this reader answers `false` for it and the
/// seven-rung ladder owns every refusal about it (rung 1 owns the missing
/// opt-in, pinned in `tests/pv_admission_tests.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_per_volume_posture_is_selectable_by_the_mount_path() {
    let _serial = SERIAL.lock().await;
    let _restore = Restore;

    for (role, authority) in [
        ("partial-authority", Some("127.0.0.1:7100")),
        ("set-authority", None),
    ] {
        std::env::set_var("SQUEEZEFS_MULTI_WRITER", "1");
        std::env::set_var("SQUEEZEFS_MW_ROLE", role);
        match authority {
            Some(a) => std::env::set_var("SQUEEZEFS_MW_AUTHORITY", a),
            None => std::env::remove_var("SQUEEZEFS_MW_AUTHORITY"),
        }
        assert!(
            pv::requested(),
            "{role} is a per-volume declaration the mount path must route to the partial door"
        );
        assert!(
            !squeezefs::cowriter::co_writer_requested()
                .unwrap_or_else(|e| panic!("{role} is refused before the ladder ever runs: {e}")),
            "{role} is not the CO-WRITER posture, so the co-writer door stays shut"
        );

        // Without the opt-in the answer is the same: not a co-writer. The
        // refusal belongs to rung 1, which names SQUEEZEFS_MULTI_WRITER —
        // answering it here would refuse the posture instead of the
        // missing half.
        std::env::remove_var("SQUEEZEFS_MULTI_WRITER");
        assert!(
            !squeezefs::cowriter::co_writer_requested().unwrap_or_else(|e| panic!(
                "{role} without the opt-in must reach rung 1, not a mount-path refusal: {e}"
            )),
            "{role} is not the CO-WRITER posture with the opt-in off either"
        );
        assert!(
            pv::requested(),
            "{role} is still a declaration — rung 1 is what refuses it"
        );
    }

    // The co-writer arms are untouched by that widening.
    std::env::set_var("SQUEEZEFS_MULTI_WRITER", "1");
    std::env::set_var("SQUEEZEFS_MW_ROLE", "co-writer");
    assert!(
        squeezefs::cowriter::co_writer_requested().expect("a declared co-writer is admitted"),
        "the co-writer door is unchanged"
    );
    assert!(!pv::requested(), "and it is not a per-volume declaration");
    std::env::remove_var("SQUEEZEFS_MULTI_WRITER");
    assert!(
        squeezefs::cowriter::co_writer_requested().is_err(),
        "a co-writer without the opt-in still refuses at the mount path — that arm has no \
         ladder of its own to defer to"
    );
}
