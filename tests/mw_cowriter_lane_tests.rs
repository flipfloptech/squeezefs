//! **The co-writer's allocation lane** — DLM **S9**, the last seam in the
//! multi-writer write path: giving an admitted co-writer a real allocation
//! lane so it can place FRESH blocks.
//!
//! Two landed siblings each deferred this to the other, in their own words:
//!
//! * the allocation partition (`src/data_alloc_lane.rs`,
//!   `docs/design-mw-data-alloc-partition.md` §9 item 1) — *"`mount_partition()`
//!   is `SOLO` until an admission installs one, and nothing in the shipped
//!   mount path does. The natural source is S9's custody lease … deliberately
//!   not built here: it belongs with the co-writer mount posture"*;
//! * the co-writer posture (`src/cowriter.rs`) — *"It **refuses fresh
//!   allocation**, loudly, naming the data-plane allocation partition that
//!   owns the problem … Refusing at the allocation point keeps the seam in ONE
//!   place."*
//!
//! This file is the contract that closes it. The organizing question is the
//! one neither sibling could answer alone: **a lane reservation is a metadata
//! commit, and a co-writer has no metadata authority** — so the reservation
//! must be written BY the authority, ON the co-writer's behalf, BEFORE the
//! offset it covers is handed out, and it must be impossible for a co-writer
//! to name a lane that is not its own.
//!
//! The eight properties, in order:
//!
//! 1. **the assignment** — one lane per enrolled writer, derived by the
//!    authority from the DURABLE claim set (never from a knob, never from a
//!    claim the joining node makes about itself), injective, authority-first,
//!    width-capped;
//! 2. **the lease is the carrier** — `(writer_lane, writers)` travels on the
//!    custody lease, an unnamed member's join is REFUSED, and the roster-growth
//!    rule is that a width change needs a new authority era;
//! 3. **stability** — a renewal never moves a live mount's lane; if it ever
//!    did, the mount self-fences rather than minting in a lane it does not own;
//! 4. **allocation** — an admitted co-writer with a lane allocates, and every
//!    offset it gets is in its own residue class;
//! 5. **the reservation is durable before the hand-out** — proved twice: the
//!    record covers the offset the moment it is handed out, and a raise the
//!    authority REFUSES refuses the allocation instead of handing out an
//!    uncovered offset;
//! 6. **a crash never lets anyone re-mint** — the successor of the lane
//!    recovers above every index the dead co-writer could have minted, and no
//!    other writer's residue class contains them at all;
//! 7. **the open floors at the dense frontier** — a co-writer runs NO
//!    ownership-recovery walk (its cursor starts at 0), so its lane is opened
//!    by the authority at or above the set's durable dense frontier: it can
//!    never mint over live data;
//! 8. **nothing else moved** — solo and reader postures are byte-identical, a
//!    co-writer WITHOUT a lane is still refused exactly as it was, and every
//!    other accounting arm (terminal free, specific claim, W1's incarnation
//!    retire, the recovery walk) still refuses.
//!
//! RED against `dev` (b3681112): `squeezefs::alloc_lane_grant` does not
//! exist, `LeaseFrame` carries no lane, `WriteCustodyOwner` has no assignment,
//! `publish` has no reservation verb, and `BlockAllocator::allocate_block`
//! refuses on every co-writer mount.
//!
//! **No numbers here — ruling D11.** The bench coverage
//! (`benches/write_path_bench.rs::alloc_lane`) is written and NOT run.
//!
//! # What one process cannot pin (stated, not hidden)
//!
//! Two hosts, a PR-capable fabric and stamped capability bits are what a real
//! two-host write needs; here the "co-writer" is a second backend over the
//! same files (the S8/S9 test discipline), which is what makes the assignment,
//! the wire, the durable record and the allocator real.

use squeezefs::alloc_lane_grant::{self as grant, LaneAssignment, LaneFloor};
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cluster_wire as cw;
use squeezefs::cowriter::{
    self, AdmissionRequest, AuthorityLeaseEvidence, RegistrantEvidence, VolumeAdmissionEvidence,
};
use squeezefs::data_alloc_lane as lane;
use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::fuse_client::{self, MountPosture, METRICS};
use squeezefs::membership::{
    ClaimSet, ClaimSetMember, LeaseClock, LeaseClocks, MemberIdentity, MemberRole,
};
use squeezefs::meta_backend::kv::backend::WriterClaim;
use squeezefs::meta_backend::kv::block_refs::{BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const VOL_LEN: u64 = 64 * 1024 * 1024;

/// The `job:enroll`-class storage-trust secret both halves prove possession
/// of (S3's root of trust — ruling D2).
const SECRET: &[u8] = b"s9-cowriter-lane-storage-trust-secret";

/// The authority's own claim-set member id (the membership owner's id in
/// production).
const AUTHORITY_ID: &str = "authority-membership-owner";
/// Two enrolled co-writer nodes, in the sorted order the assignment uses.
const NODE_A: &str = "node_00000000aaaaaaaa";
const NODE_B: &str = "node_00000000bbbbbbbb";

/// A durable data-volume id (KD-5's `vol-{16 hex}` shape) — `volume_tag`
/// decodes it verbatim, so the record name carries the durable identity and
/// never a path.
const DATA_VOL: &str = "vol-00000000000000a1";

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

/// Restores every process-global posture this file can move, so a panicking
/// assertion never leaves the binary armed or partitioned.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        fuse_client::set_mount_posture(MountPosture::Writer);
        lane::test_reset_mount_partition();
        grant::uninstall_frontier_source();
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Volumes, allocators, evidence
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

async fn fresh_volume(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    format_v3(&p, VOL_LEN, &opts()).await.unwrap();
    stamp_capabilities(&p).await;
    p
}

async fn allocator(id: &str, capacity_blocks: u64) -> Arc<BlockAllocator> {
    let a = Arc::new(BlockAllocator::new(id).await.expect("allocator"));
    if capacity_blocks > 0 {
        a.set_capacity_bytes(capacity_blocks * a.chunk_size());
    }
    a
}

fn part(writers: u16, id: u16) -> AppendPartition {
    AppendPartition::new(writers, id).expect("partition")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

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
        ts: now_secs(),
    }
}

/// The durable claim set an authority with two enrolled co-writers holds:
/// itself plus `NODE_A` and `NODE_B` as writer members (the record is sorted
/// by id — `upsert_writer_member`'s own invariant).
fn claim_set_with(members: &[&str]) -> ClaimSet {
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.members.push(member(AUTHORITY_ID, MemberRole::Writer));
    for m in members {
        set.members.push(member(m, MemberRole::Writer));
    }
    set.members
        .sort_by(|a, b| a.identity.id.cmp(&b.identity.id));
    set
}

fn volume_evidence(path: &Path, node_id: &str) -> VolumeAdmissionEvidence {
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
        claim_set: Some(claim_set_with(&[node_id])),
    }
}

/// A complete, admissible request over `paths` for `node_id`.
fn full_request(paths: &[PathBuf], node_id: &str) -> AdmissionRequest {
    AdmissionRequest {
        multi_writer: true,
        role_co_writer: true,
        read_only: false,
        node_id: node_id.to_string(),
        custody_endpoint: Some("127.0.0.1:7100".to_string()),
        volumes: paths.iter().map(|p| volume_evidence(p, node_id)).collect(),
        authority: Some(AuthorityLeaseEvidence {
            owner_id: AUTHORITY_ID.to_string(),
            endpoint: "127.0.0.1:7000".to_string(),
            owner_claim_id: String::new(),
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
    }
}

// ---------------------------------------------------------------------------
// The rig: one authority (custody + publish, on the wire) and N co-writers
// ---------------------------------------------------------------------------

/// One authority: its own write mount of the metadata set, the custody
/// authority armed over a lane assignment, and both vocabularies served on
/// the S3 wire's pinned lanes.
struct Authority {
    listener: Arc<cw::RpcListener>,
    owner: Arc<WriteCustodyOwner>,
    meta: Arc<RoutedMetaBackend>,
    endpoint: String,
}

impl Authority {
    async fn start(vol: &Path, members: &[&str]) -> Authority {
        let meta = squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
            .await
            .expect("the authority mounts its own set");
        let owner = WriteCustodyOwner::arm(
            "mw-authority",
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
        let set = claim_set_with(members);
        let assignment = LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&set))
            .expect("the roster fits the lane space");
        owner.install_lane_assignment(Arc::clone(&assignment));
        // The publish owner side validates a peer's lane raise against the
        // assignment THIS authority made, and it reaches the authority through
        // the process registry — exactly as `multi_writer::arm_multi_writer`
        // installs it.
        data_grant::install_custody_owner(Arc::clone(&owner));
        let router = data_grant::AsyncVerbRouter::new()
            .with_custody(Arc::clone(&owner))
            .with_publish(publish::PublishService::new(Arc::clone(&meta)));
        let listener = cw::RpcListener::start_async(
            cw::RpcListenerConfig {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                service_threads: 2,
                ..cw::RpcListenerConfig::default()
            },
            SECRET.to_vec(),
            Arc::new(router),
        )
        .expect("the authority listens");
        let endpoint = listener.endpoint().to_string();
        Authority {
            listener,
            owner,
            meta,
            endpoint,
        }
    }

    /// Every durable lane reservation record the set carries for the data
    /// volume (the offline-probe shape).
    async fn records(&self) -> Vec<lane::LaneReservation> {
        lane::load_lane_reservations(&self.meta, DATA_VOL)
            .await
            .expect("the reservation records decode")
    }

    async fn record_for(&self, lane_id: u16) -> Option<lane::LaneReservation> {
        self.records().await.into_iter().find(|r| r.lane == lane_id)
    }

    async fn stop(self) {
        self.listener.shutdown();
        for v in &self.meta.volumes {
            v.shutdown().await.expect("the authority unmounts clean");
        }
    }
}

/// One co-writer: the posture latched, its routed metadata set opened
/// through the admission, the ownership plane armed all-foreign (so every
/// metadata mutation — the lane raise included — ships), and a live custody
/// client whose lease carries this mount's lane.
struct CoWriter {
    meta: Arc<RoutedMetaBackend>,
    client: Arc<WriteCustodyClient>,
    alloc: Arc<BlockAllocator>,
}

impl CoWriter {
    async fn join(auth: &Authority, vol: &Path, node_id: &str, capacity_blocks: u64) -> CoWriter {
        fuse_client::set_mount_posture(MountPosture::CoWriter);
        let admission = cowriter::classify_admission(&full_request(
            std::slice::from_ref(&vol.to_path_buf()),
            node_id,
        ))
        .expect("the five-rung ladder admits");
        let meta = squeezefs::meta_backend::open_routed_meta_set_co_writer(
            &[vol.display().to_string()],
            &admission,
        )
        .await
        .expect("the co-writer's routed set");
        let map = OwnerMap::for_volumes(
            &meta,
            vec![(0, PeerOwner::new("mw-authority", auth.endpoint.clone()))],
        )
        .expect("an all-foreign owner map");
        ship::arm_ownership(map);
        publish::install_client(publish::PublishClient::new(node_id, SECRET.to_vec()));
        let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, node_id)
            .await
            .expect("the co-writer dials the custody authority");
        data_grant::install_custody_client(Arc::clone(&client));
        let alloc = allocator(DATA_VOL, capacity_blocks).await;
        CoWriter {
            meta,
            client,
            alloc,
        }
    }

    /// The lane the AUTHORITY granted this mount, on its lease.
    fn part(&self) -> AppendPartition {
        self.client.lane_partition()
    }

    /// Engage the granted lane on this mount's allocator, the way
    /// `cowriter::arm` does: the routed reservation sink, and a floor opened
    /// THROUGH the authority (a co-writer runs no recovery walk of its own).
    async fn engage(&self) {
        grant::engage_allocator_lane(&self.alloc, self.part(), &self.meta, LaneFloor::Authority)
            .await
            .expect("the granted lane engages");
    }
}

// ===========================================================================
// 0. The seam itself, in landed API only — the behavioural red
// ===========================================================================

/// Contract (**the seam**, stated with nothing but the two landed siblings'
/// own public API): a co-writer whose allocator has an ENGAGED allocation lane
/// allocates. Against `dev` this is a refusal — `BlockAllocator::plane_gate`
/// refuses every allocation on a co-writer mount unconditionally, so the
/// partition's residue class is unreachable from the posture that needs it.
///
/// This test needs no assignment, no lease and no wire: it is the *gate's*
/// contract, and it is deliberately the file's first one so the red is an
/// allocation that should succeed and does not — not a missing symbol.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_co_writer_with_an_engaged_lane_is_no_longer_refused_allocation() {
    let _serial = serial();
    let _restore = restore();
    let alloc = allocator("vol-00000000000000c1", 0).await;
    let p = part(4, 2);
    alloc
        .engage_alloc_lanes(p)
        .expect("engage the granted lane");
    fuse_client::set_mount_posture(MountPosture::CoWriter);

    let chunk = alloc.chunk_size();
    let off = alloc
        .allocate_block()
        .await
        .expect("a co-writer with an engaged lane ALLOCATES — this is the seam");
    assert_eq!(
        lane::offset_lane_of(off, chunk, p.writers()),
        u64::from(p.writer_id()),
        "and the offset is in the lane it was granted"
    );

    // The same mount, on a volume with NO engaged lane, is refused exactly as
    // it was: the relaxation is "an owned lane", never "a co-writer".
    let laneless = allocator("vol-00000000000000c2", 0).await;
    assert!(
        laneless.allocate_block().await.is_err(),
        "without a lane there is no disjointness, so nothing changed"
    );
}

// ===========================================================================
// 1. The assignment: one lane per enrolled writer, derived from the DURABLE
//    claim set — never from a knob
// ===========================================================================

/// Contract (requirement 1 + 4): the authority derives the lane map from the
/// durable claim-set roster. It takes lane 0 itself; every enrolled writer
/// member gets the next lane in the record's own sorted order; the width is
/// the member count. The map is INJECTIVE — which is the collision a knob
/// would allow and this derivation cannot.
#[test]
fn the_assignment_gives_one_lane_per_enrolled_writer_and_is_injective() {
    let set = claim_set_with(&[NODE_A, NODE_B]);
    let map = LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&set)).expect("derive");

    assert_eq!(
        map.writers(),
        4,
        "three writers round UP to a power-of-two width — what AppendPartition admits — so lane \
         3 belongs to nobody (honest, published, unreachable capacity)"
    );
    assert_eq!(
        map.lane_of(AUTHORITY_ID),
        Some(0),
        "the authority is lane 0 — so a solo authority is lane 0 of 1, i.e. today's allocator"
    );
    assert_eq!(map.lane_of(NODE_A), Some(1));
    assert_eq!(map.lane_of(NODE_B), Some(2));
    assert_eq!(
        map.co_writers(),
        [NODE_A.to_string(), NODE_B.to_string()],
        "lane order is the record's own sorted order, not the reader's"
    );
    assert_eq!(
        map.lane_of("node_never_enrolled"),
        None,
        "a node the durable record does not name has NO lane — admission is by enrollment"
    );

    // Injective: no two members share a residue class.
    let lanes: std::collections::BTreeSet<u16> = [AUTHORITY_ID, NODE_A, NODE_B]
        .iter()
        .map(|id| map.lane_of(id).expect("named"))
        .collect();
    assert_eq!(lanes.len(), 3, "one lane per member, never shared");

    // The partition each member runs under.
    assert_eq!(map.partition_for(NODE_A).expect("named").writer_id(), 1);
    assert_eq!(map.partition_for(NODE_A).expect("named").writers(), 4);
    assert_eq!(map.authority_partition().writer_id(), 0);
    assert_eq!(map.authority_partition().writers(), 4);
    // Two writers need no rounding at all: the common shape is exact.
    let pair = LaneAssignment::derive(
        AUTHORITY_ID,
        std::slice::from_ref(&claim_set_with(&[NODE_A])),
    )
    .expect("derive");
    assert_eq!(
        pair.writers(),
        2,
        "one authority + one co-writer is exactly 2"
    );

    // Deterministic: the same record derives the same map, always (which is
    // what makes two nodes' views agree without a message).
    let again = LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&set)).expect("derive");
    assert_eq!(again.writers(), map.writers());
    assert_eq!(again.lane_of(NODE_B), map.lane_of(NODE_B));
}

/// Contract (requirement 5's structural half): **no roster means SOLO.** An
/// authority with no enrolled co-writer derives width 1, whose partition is
/// `SOLO` — and a solo partition installs nothing at all, which is why the
/// authority's own allocation is not merely equivalent to the shipped path
/// but literally it.
#[test]
fn an_authority_with_no_co_writers_is_solo_and_installs_nothing() {
    let set = claim_set_with(&[]);
    let map = LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&set)).expect("derive");
    assert_eq!(map.writers(), 1, "one writer");
    assert!(
        map.authority_partition().is_solo(),
        "lane 0 of 1 is SOLO, so `engage_alloc_lanes` installs nothing"
    );
    // A reader member never consumes a lane: it allocates nothing.
    let mut with_reader = claim_set_with(&[]);
    with_reader
        .members
        .push(member("node_reader", MemberRole::Reader));
    let map =
        LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&with_reader)).expect("derive");
    assert_eq!(
        map.writers(),
        1,
        "a READER member takes no lane — it mints no offsets"
    );
    assert_eq!(map.lane_of("node_reader"), None);
}

/// Contract: the lane mask is a `u64`, so the width has a hard ceiling
/// (`BlockAllocator::engage_alloc_lanes` refuses beyond it). A roster past
/// the ceiling refuses **loud** at derivation — before any member is told a
/// lane the allocator could not hold.
#[test]
fn a_roster_past_the_lane_ceiling_refuses_loud() {
    assert_eq!(
        grant::MAX_LANES,
        squeezefs::meta_backend::kv::journal::MAX_APPENDERS,
        "the ceiling DERIVES from the append-partition descriptor every partitioned structure \
         already runs on — it is not a second constant"
    );
    let ids: Vec<String> = (0..grant::MAX_LANES as usize)
        .map(|i| format!("node_{i:016x}"))
        .collect();
    let over: Vec<&str> = ids.iter().map(String::as_str).collect();
    let set = claim_set_with(&over);
    let err = LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&set))
        .expect_err("the ceiling refuses");
    let msg = err.to_string();
    assert!(
        msg.contains(&grant::MAX_LANES.to_string()),
        "the refusal names the ceiling: {msg}"
    );

    // Exactly at the ceiling is admissible, and every member has a lane.
    let ok: Vec<&str> = over[..grant::MAX_LANES as usize - 1].to_vec();
    let set = claim_set_with(&ok);
    let map = LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&set)).expect("derive");
    assert_eq!(map.writers(), grant::MAX_LANES);
    for id in &ok {
        assert!(map.lane_of(id).is_some(), "{id} holds a lane");
    }
}

// ===========================================================================
// 2. The lease is the carrier, and a lane cannot be self-chosen
// ===========================================================================

/// Contract (requirement 1): `(writer_lane, writers)` reaches a co-writer on
/// its **custody lease** — minted by the authority from a durable record only
/// the authority can write. There is no knob, and the joining node's own
/// opinion never enters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_custody_lease_carries_the_lane_the_authority_assigned() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "lease-lane").await;
    let auth = Authority::start(&vol, &[NODE_A, NODE_B]).await;

    let a = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE_A)
        .await
        .expect("node A joins");
    let b = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE_B)
        .await
        .expect("node B joins");

    assert_eq!(a.lane_partition().writers(), 4);
    assert_eq!(a.lane_partition().writer_id(), 1, "node A is lane 1 of 4");
    assert_eq!(b.lane_partition().writer_id(), 2, "node B is lane 2 of 4");
    assert_ne!(
        a.lane_partition().writer_id(),
        b.lane_partition().writer_id(),
        "two co-writers are never told the same lane"
    );
    auth.stop().await;
}

/// Contract (requirement 4, the roster-growth rule): a width change is an
/// act of a new authority ERA, never of a live set. A node the authority's
/// arm-time assignment does not name is REFUSED at the custody join —
/// because the alternative (deriving its own lane from a record it read
/// later) is exactly how two live co-writers end up in overlapping residue
/// classes: with `W = 2` node A mints `b % 2 == 1`, and a node that later
/// read `W = 3` would mint `b % 3 == 2`, which contains index 5 twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_the_arm_time_assignment_does_not_name_is_refused_at_the_join() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "roster-growth").await;
    // The authority armed with ONE enrolled co-writer.
    let auth = Authority::start(&vol, &[NODE_A]).await;

    WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE_A)
        .await
        .expect("the named member joins");

    let err = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE_B)
        .await
        .expect_err("a member enrolled AFTER the arm has no lane in this era")
        .to_string();
    assert!(
        err.to_lowercase().contains("lane"),
        "the refusal is about the allocation lane: {err}"
    );
    assert!(
        err.to_lowercase().contains("re-arm") || err.to_lowercase().contains("era"),
        "and it names the remedy — a new authority era is what changes the width: {err}"
    );
    auth.stop().await;
}

// ===========================================================================
// 3. Stability across a renewal
// ===========================================================================

/// Contract (requirement 4's last clause): a renewal never moves a live
/// mount's lane. The authority answers the SAME `(lane, writers)` it granted
/// at join — and if it ever answered a different one, this mount **self-fences**
/// rather than minting in a lane it does not own (its already-minted offsets
/// belong to the old residue class, so continuing would hand a peer's lane
/// indices out).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renewal_never_moves_a_live_mounts_lane() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "renew-lane").await;
    let auth = Authority::start(&vol, &[NODE_A, NODE_B]).await;

    let a = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE_A)
        .await
        .expect("node A joins");
    let before = a.lane_partition();
    a.renew_all().await.expect("the renewal completes");
    assert_eq!(
        a.lane_partition().writer_id(),
        before.writer_id(),
        "a renewal is a heartbeat, not a re-assignment"
    );
    assert_eq!(a.lane_partition().writers(), before.writers());

    // Now MOVE the assignment under the live mount (nothing in production
    // does this — an authority installs its map once per era — which is
    // exactly why the client must treat it as a fault rather than adopt it).
    let moved = LaneAssignment::derive(
        AUTHORITY_ID,
        std::slice::from_ref(&claim_set_with(&[NODE_B])),
    )
    .expect("derive");
    auth.owner.install_lane_assignment(moved);
    let fences_before = data_grant::stats().self_fences;
    let err = a
        .renew_all()
        .await
        .expect_err("a moved lane is a fault, never an adoption");
    assert!(
        err.to_string().to_lowercase().contains("lane"),
        "the failure names the lane: {err}"
    );
    assert!(
        data_grant::stats().self_fences > fences_before,
        "and the mount fail-stopped its own custody rather than minting in a lane it does not own"
    );
    auth.stop().await;
}

// ===========================================================================
// 4. Allocation: the headline
// ===========================================================================

/// Contract (the seam this branch closes): an admitted co-writer with a
/// granted lane **allocates**, and every offset it is handed is in its own
/// residue class. Before this branch the same call refused, naming the
/// partition — that refusal is what "the seam" was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writer_allocates_from_its_own_lane_after_admission() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "cw-alloc").await;
    let auth = Authority::start(&vol, &[NODE_A]).await;
    let cw = CoWriter::join(&auth, &vol, NODE_A, 0).await;
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);

    let p = cw.part();
    assert!(
        !p.is_solo(),
        "the lease granted a real lane (authority + one co-writer = 2 writers)"
    );
    cw.engage().await;
    assert_eq!(
        cw.alloc.lane_partition().map(|p| p.writer_id()),
        Some(p.writer_id()),
        "the allocator engaged the lane the lease named"
    );

    let chunk = cw.alloc.chunk_size();
    let mut offsets = Vec::new();
    for _ in 0..8 {
        offsets.push(
            cw.alloc
                .allocate_block()
                .await
                .expect("a co-writer with a lane ALLOCATES — this is the seam"),
        );
    }
    for off in &offsets {
        assert_eq!(
            lane::offset_lane_of(*off, chunk, p.writers()),
            u64::from(p.writer_id()),
            "every offset a co-writer is handed is in its own residue class"
        );
    }
    let unique: std::collections::BTreeSet<u64> = offsets.iter().copied().collect();
    assert_eq!(
        unique.len(),
        offsets.len(),
        "a lane never repeats an offset"
    );
    assert_eq!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed),
        refusals_before,
        "allocation stopped counting as an accounting refusal — it is no longer refused"
    );

    drop(cw);
    auth.stop().await;
}

// ===========================================================================
// 5. The reservation is durable BEFORE the hand-out
// ===========================================================================

/// Contract (the hard part): a lane reservation is a metadata commit and a
/// co-writer holds no metadata authority, so the raise is **shipped** and the
/// AUTHORITY commits it — before the offset it covers is handed out.
///
/// Proved twice, because one half alone proves nothing:
///
/// 1. the moment the first offset exists, the durable record on the
///    authority's volume already covers it (and it is OUR lane's record, at
///    OUR width);
/// 2. a raise the authority REFUSES refuses the allocation — the offset is
///    never handed out uncovered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writers_reservation_is_durable_before_its_first_hand_out() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "cw-reserve").await;
    let auth = Authority::start(&vol, &[NODE_A]).await;
    let cw = CoWriter::join(&auth, &vol, NODE_A, 0).await;
    let p = cw.part();
    cw.engage().await;

    let shipped_before = METRICS
        .alloc_lane_shipped_reservations
        .load(Ordering::Relaxed);
    let off = cw
        .alloc
        .allocate_block()
        .await
        .expect("the first fresh mint");
    let idx = off / cw.alloc.chunk_size();

    let rec = auth
        .record_for(p.writer_id())
        .await
        .expect("the authority committed THIS lane's reservation record");
    assert_eq!(rec.writers, p.writers(), "at our width");
    assert!(
        rec.reserved_upto > idx,
        "the durable frontier {} must already cover the handed-out index {idx}",
        rec.reserved_upto
    );
    assert!(
        METRICS
            .alloc_lane_shipped_reservations
            .load(Ordering::Relaxed)
            > shipped_before,
        "a co-writer's raises TRAVEL — the record is the authority's commit, not ours"
    );
    // And the authority's own lane never gained a record from our raise: a
    // client's raise declares ITS lane, and the owner names the record.
    assert!(
        auth.record_for(0).await.is_none(),
        "a co-writer's raise can never write another lane's record"
    );

    drop(cw);
    auth.stop().await;
}

/// Contract (forgery, and the reason the owner names the record): a raise for
/// a lane this client was not assigned is REFUSED — so the offset is never
/// handed out, the durable frontier is untouched, and the refusal is counted
/// on a must-stay-0 tripwire. The client names a LANE; the owner derives the
/// record name, validates the lane against the assignment it made, and holds
/// the monotonicity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raise_naming_a_lane_the_client_was_not_assigned_is_refused() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "cw-forge").await;
    let auth = Authority::start(&vol, &[NODE_A, NODE_B]).await;
    let cw = CoWriter::join(&auth, &vol, NODE_A, 0).await;

    // Engage the allocator with a lane that is NOT ours (node B's) — the
    // shape a forged or drifted partition would produce.
    let foreign = part(4, 3);
    let refusals_before = METRICS.alloc_lane_raise_refusals.load(Ordering::Relaxed);
    let engaged =
        grant::engage_allocator_lane(&cw.alloc, foreign, &cw.meta, LaneFloor::Authority).await;
    let err = match engaged {
        Err(e) => e.to_string(),
        Ok(()) => cw
            .alloc
            .allocate_block()
            .await
            .expect_err("an offset in a lane the authority did not assign us")
            .to_string(),
    };
    assert!(
        err.to_lowercase().contains("lane"),
        "the refusal is about the lane: {err}"
    );
    assert!(
        METRICS.alloc_lane_raise_refusals.load(Ordering::Relaxed) > refusals_before,
        "and it lands on the must-stay-0 tripwire"
    );
    assert!(
        auth.record_for(3).await.is_none(),
        "no record was written for a lane the client does not hold"
    );

    drop(cw);
    auth.stop().await;
}

/// Contract (monotonicity is the AUTHORITY's, never the client's): the
/// committed frontier only ever RISES. A raise that names a lower bound than
/// the record already carries leaves the record alone — because lowering it
/// is precisely how a successor would re-mint a live peer's offsets, and a
/// client must not be able to ask for that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_committed_frontier_only_ever_rises() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "monotone").await;
    let meta = squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
        .await
        .expect("mount");
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL);

    let high = lane::commit_lane_raise(&meta, tag, 1, 2, 5_000, None)
        .await
        .expect("raise");
    assert_eq!(high, 5_000);
    let low = lane::commit_lane_raise(&meta, tag, 1, 2, 10, None)
        .await
        .expect("a lower raise is not an error — it is a no-op");
    assert_eq!(
        low, 5_000,
        "the frontier a client may ask for can never lower the durable one"
    );
    let recs = lane::load_lane_reservations(&meta, DATA_VOL)
        .await
        .expect("records");
    assert_eq!(recs.len(), 1, "one record per (volume, lane)");
    assert_eq!(recs[0].reserved_upto, 5_000);

    for v in &meta.volumes {
        v.shutdown().await.expect("clean unmount");
    }
}

// ===========================================================================
// 6. A crashed co-writer's offsets are never re-minted
// ===========================================================================

/// Contract (the invariant end to end): a co-writer dies holding offsets it
/// minted, DMA'd into and never published. Nothing may re-mint them:
///
/// * a **successor of its lane** recovers above them, from the durable record
///   the authority committed on its behalf (the derived floor cannot see
///   them — nothing references them);
/// * **every other writer** never had them at all, because they are not in
///   any other residue class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_co_writers_offsets_are_never_re_minted_by_anyone() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "cw-crash").await;
    let auth = Authority::start(&vol, &[NODE_A]).await;
    let cw = CoWriter::join(&auth, &vol, NODE_A, 0).await;
    let p = cw.part();
    cw.engage().await;

    let chunk = cw.alloc.chunk_size();
    let mut minted = Vec::new();
    for _ in 0..6 {
        minted.push(cw.alloc.allocate_block().await.expect("mint") / chunk);
    }
    let highest = *minted.iter().max().unwrap();
    // Crash: no publish, no unmount, no release. The allocator's RAM dies
    // with the process; the durable record is all that survives.
    drop(cw);

    // The successor of this lane (a remount of the same node) recovers from
    // the records the authority holds. Derived floor 0: nothing was published.
    let records = auth.records().await;
    let floor = lane::recover_lane_floor(&records, 0, p);
    assert!(
        floor > highest,
        "the recovered floor {floor} must dominate the dead co-writer's last mint {highest}"
    );
    let successor = allocator(DATA_VOL, 0).await;
    successor.engage_alloc_lanes(p).expect("engage");
    successor.install_lane_floor(floor);
    let first = successor.allocate_block().await.expect("mint") / chunk;
    assert!(
        !minted.contains(&first),
        "the successor re-minted an offset the dead co-writer may still have been writing"
    );

    // And no other writer's residue class contains them — the authority
    // (lane 0) can never mint a lane-1 index at all.
    for idx in &minted {
        assert_ne!(
            lane::block_lane_of(*idx, p.writers()),
            0,
            "a dead co-writer's index must not be in the authority's lane"
        );
    }

    // The counterfactual that makes the first half meaningful: with no
    // record, the floor is the derived one and the successor re-mints.
    assert_eq!(
        lane::recover_lane_floor(&[], 0, p),
        u64::from(p.writer_id()),
        "without the shipped, authority-committed record the floor is the lane's first index — \
         the crash window this whole seam exists to close"
    );
    auth.stop().await;
}

// ===========================================================================
// 7. The open floors at the durable dense frontier
// ===========================================================================

/// Contract (the hole a lane alone would leave): a co-writer runs **no**
/// ownership-recovery walk and **no** durable-reference seed — its cursor
/// starts at 0 — so a lane alone would have it minting `w, w+W, …` from the
/// bottom of a device whose low blocks are LIVE. Its lane is therefore
/// *opened* by the authority, which answers the set's durable dense frontier
/// (the referenced-set complement — the recovery rule's clause 1, computed
/// from durable state on the node that has it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_authority_opens_a_co_writers_lane_above_the_durable_dense_frontier() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "cw-open").await;
    let auth = Authority::start(&vol, &[NODE_A]).await;

    // Live data on the shared device: durable references at high indices,
    // exactly what an authority that has been writing leaves behind.
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL);
    let ino = auth
        .meta
        .create_with_rdev_size(1, "live.bin", 0o100644, 0, 0, 0, 0)
        .await
        .expect("create on the authority")
        .ino;
    let refs: Vec<BlockRefOp> = [900u64, 901, 902]
        .iter()
        .enumerate()
        .map(|(i, idx)| {
            BlockRefOp::taken(BlockRef {
                vol_tag: tag,
                block_idx: *idx,
                owner_ino: ino,
                block_index: i as u32,
            })
        })
        .collect();
    auth.meta
        .commit_block_refs(ino, &refs)
        .await
        .expect("the durable ledger records the authority's live blocks");
    assert!(
        lane::durable_dense_frontier(&auth.meta, tag)
            .await
            .expect("frontier")
            > 902,
        "the dense frontier is the referenced-set complement"
    );

    // The other half of the open floor: the authority's LIVE cursor. The
    // ledger only knows what has been published, and a mount that is writing
    // knows a fresher frontier — an offset it minted and has not published is
    // in neither the ledger nor its layouts. `arm_multi_writer` installs this
    // source from its data-plane router; here it is the same contract as a
    // closure.
    grant::install_frontier_source(Arc::new(move |asked: u64| (asked == tag).then_some(5_000)));

    let cw = CoWriter::join(&auth, &vol, NODE_A, 0).await;
    let p = cw.part();
    cw.engage().await;
    let idx = cw.alloc.allocate_block().await.expect("mint") / cw.alloc.chunk_size();
    assert!(
        idx >= 5_000,
        "the open must also dominate the authority's LIVE cursor ({idx} < 5000): an offset it \
         minted and has not published yet is in no durable record at all"
    );
    assert!(
        idx > 902,
        "a co-writer's first mint {idx} must be above the set's live blocks — it never walks the \
         tree, so the authority's open is the only thing that can tell it where the data ends"
    );
    assert_eq!(
        lane::block_lane_of(idx, p.writers()),
        u64::from(p.writer_id()),
        "and still in its own lane"
    );

    drop(cw);
    auth.stop().await;
}

// ===========================================================================
// 8. Two co-writers never collide
// ===========================================================================

/// Contract (requirement 1's threat model, verbatim: *"a knob would let two
/// co-writers claim one lane, which is the collision the partition exists to
/// prevent"*): two admitted co-writers of one authority mint concurrently and
/// never produce the same offset — with no arbitration between them, because
/// their residue classes are disjoint by the authority's injective map.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_co_writers_never_collide_on_one_offset() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "cw-collide").await;
    let auth = Authority::start(&vol, &[NODE_A, NODE_B]).await;

    // Two co-writer mounts of one set. They share this process's posture
    // latch and its custody-client registry (one process = one mount in
    // production), so each drives its own allocator through its own lease.
    let a = CoWriter::join(&auth, &vol, NODE_A, 0).await;
    let pa = a.part();
    a.engage().await;
    let mut mine = Vec::new();
    for _ in 0..16 {
        mine.push(a.alloc.allocate_block().await.expect("A mints"));
    }
    drop(a);

    let b = CoWriter::join(&auth, &vol, NODE_B, 0).await;
    let pb = b.part();
    b.engage().await;
    let mut theirs = Vec::new();
    for _ in 0..16 {
        theirs.push(b.alloc.allocate_block().await.expect("B mints"));
    }

    assert_ne!(pa.writer_id(), pb.writer_id(), "distinct lanes");
    let set: std::collections::BTreeSet<u64> = mine.iter().copied().collect();
    for off in &theirs {
        assert!(
            !set.contains(off),
            "offset {off} was handed to BOTH co-writers — the partition failed"
        );
    }
    let chunk = b.alloc.chunk_size();
    for off in &mine {
        assert_eq!(
            lane::offset_lane_of(*off, chunk, pa.writers()),
            u64::from(pa.writer_id())
        );
    }
    for off in &theirs {
        assert_eq!(
            lane::offset_lane_of(*off, chunk, pb.writers()),
            u64::from(pb.writer_id())
        );
    }
    // Each lane carries its OWN durable record, and neither carries the
    // other's (the record is keyed on the lane, and the owner names it).
    assert!(auth.record_for(pa.writer_id()).await.is_some());
    assert!(auth.record_for(pb.writer_id()).await.is_some());

    drop(b);
    auth.stop().await;
}

// ===========================================================================
// 9. Nothing else moved: solo / reader byte-identity, and a laneless
//    co-writer is still refused
// ===========================================================================

/// Contract (requirement 5): the postures that ship are untouched. A WRITER
/// with no partition allocates exactly as it always did; a READER is refused
/// with the reader's own text at every arm; and a co-writer that holds NO
/// lane is refused exactly as it was before this branch — which is what makes
/// the relaxation precise rather than a hole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn solo_reader_and_laneless_co_writer_are_unchanged() {
    let _serial = serial();
    let _restore = restore();
    let gauges = (
        METRICS.alloc_lane_writers.load(Ordering::Relaxed),
        METRICS.alloc_lane_reservations.load(Ordering::Relaxed),
        METRICS
            .alloc_lane_shipped_reservations
            .load(Ordering::Relaxed),
    );

    // A solo WRITER: engaging the derived solo partition installs nothing,
    // and the allocation sequence is the shipped dense one.
    fuse_client::set_mount_posture(MountPosture::Writer);
    let solo = allocator("vol-00000000000000b1", 64).await;
    solo.engage_alloc_lanes(AppendPartition::SOLO)
        .expect("a solo engagement is a no-op, never a refusal");
    assert!(
        solo.lane_partition().is_none(),
        "a solo partition installs NOTHING — that is the byte-identity proof"
    );
    let plain = allocator("vol-00000000000000b2", 64).await;
    let chunk = solo.chunk_size();
    for i in 0..8u64 {
        assert_eq!(
            solo.allocate_block().await.expect("mint"),
            i * chunk,
            "the shipped dense sequence, stride 1"
        );
        assert_eq!(plain.allocate_block().await.expect("mint"), i * chunk);
    }
    assert_eq!(
        (
            METRICS.alloc_lane_writers.load(Ordering::Relaxed),
            METRICS.alloc_lane_reservations.load(Ordering::Relaxed),
            METRICS
                .alloc_lane_shipped_reservations
                .load(Ordering::Relaxed),
        ),
        gauges,
        "not one alloc_lane gauge moved for a single writer"
    );

    // A READER: refused at every arm, with its own unchanged text.
    fuse_client::set_mount_posture(MountPosture::Reader);
    let reader = allocator("vol-00000000000000b3", 64).await;
    for m in [
        reader
            .allocate_block()
            .await
            .expect_err("fresh allocation")
            .to_string(),
        reader
            .allocate_block_at_or_above(0)
            .expect_err("ascending pick")
            .to_string(),
        reader
            .allocate_specific_block(1)
            .await
            .expect_err("specific")
            .to_string(),
        reader.free_block(0).await.expect_err("free").to_string(),
    ] {
        assert!(
            m.contains("read-only") && m.contains("-o ro"),
            "the reader refusal text is unchanged: {m}"
        );
    }

    // A CO-WRITER with no lane: still refused, still naming the partition —
    // the landed posture, verbatim, because without a lane there is no
    // disjointness and nothing has changed about what it may do.
    fuse_client::set_mount_posture(MountPosture::CoWriter);
    let laneless = allocator("vol-00000000000000b4", 64).await;
    let err = laneless
        .allocate_block()
        .await
        .expect_err("no lane means no allocation")
        .to_string();
    assert!(
        err.to_lowercase().contains("co-writer"),
        "the co-writer text, not the reader's: {err}"
    );
    assert!(
        err.to_lowercase().contains("lane") || err.to_lowercase().contains("partition"),
        "and it names what is missing: {err}"
    );
}

// ===========================================================================
// 10. What stays refused on a co-writer WITH a lane
// ===========================================================================

/// Contract (requirement 3): the relaxation is fresh allocation from an owned
/// lane, and nothing else. Every arm whose durable home is metadata the
/// co-writer cannot commit still refuses, and those refusals are what
/// `cowriter.accounting_refusals` keeps counting:
///
/// * **terminal free** and `free_block` — a free's durable effect is the
///   authority's `TREE_BLOCK_REFS` delete, and the reclaim queue that would
///   deallocate the device range is ceased on this posture;
/// * **`allocate_specific_block`** — deliberately lane-blind (a clone /
///   recovery path naming an index it already owns durably), which on a
///   co-writer means claiming an offset in a lane it may not hold;
/// * **the W1 incarnation retire** — it retires a lifetime, which is durable
///   ownership state;
/// * **the ownership recovery walk** — it declares gaps free from the tree it
///   happened to see.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_other_accounting_arms_still_refuse_on_a_laned_co_writer() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "cw-refuse").await;
    let auth = Authority::start(&vol, &[NODE_A]).await;
    let cw = CoWriter::join(&auth, &vol, NODE_A, 0).await;
    cw.engage().await;

    let off = cw.alloc.allocate_block().await.expect("allocation passes");
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);

    let free_err = cw
        .alloc
        .free_block(off)
        .await
        .expect_err("a co-writer frees nothing locally")
        .to_string();
    assert!(
        free_err.to_lowercase().contains("co-writer"),
        "the co-writer text: {free_err}"
    );
    assert!(!cw.alloc.begin_free(off), "terminal free refuses");
    assert!(
        cw.alloc.allocate_specific_block(7).await.is_err(),
        "a specific claim is lane-blind, so it stays the authority's act"
    );
    assert!(
        !cw.alloc.begin_patch_sole_owner(off),
        "W1's incarnation retire refuses"
    );
    assert!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed) > refusals_before,
        "accounting_refusals keeps counting exactly the arms that remain refused"
    );

    drop(cw);
    auth.stop().await;
}

// ===========================================================================
// 11. The whole machinery: one authority, two co-writers, under a storm
// ===========================================================================

/// Contract (the integration): an authority and two co-writers each allocate
/// from their own lane at the same time, and the AUTHORITY frees blocks the
/// co-writers minted — **lane-blind**, with no ownership lookup, no message
/// and no record, which is the property that makes the whole partition cheap.
///
/// The asymmetry is deliberate and is the honest shape of this stage: a
/// co-writer's free is refused (a free's durable effect is the authority's
/// ledger), so "freeing each other's blocks" is the authority freeing theirs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authority_and_two_co_writers_allocate_and_the_authority_frees_their_blocks() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "trio").await;
    let auth = Authority::start(&vol, &[NODE_A, NODE_B]).await;

    // The authority's own lane, engaged the way its multi-writer arm does:
    // its floor is its OWN recovery's answer (it walks; a co-writer does not).
    let authority_alloc = allocator(DATA_VOL, 0).await;
    let map = LaneAssignment::derive(
        AUTHORITY_ID,
        std::slice::from_ref(&claim_set_with(&[NODE_A, NODE_B])),
    )
    .expect("derive");
    let ap = map.authority_partition();
    fuse_client::set_mount_posture(MountPosture::Writer);
    grant::engage_allocator_lane(&authority_alloc, ap, &auth.meta, LaneFloor::Local)
        .await
        .expect("the authority engages its own lane");

    let mut authority_offsets = Vec::new();
    for _ in 0..8 {
        authority_offsets.push(
            authority_alloc
                .allocate_block()
                .await
                .expect("authority mint"),
        );
    }

    // The two co-writers, in turn (one process holds one posture latch and
    // one custody-client registry; in the field these are two hosts).
    let mut cw_offsets: Vec<(u16, Vec<u64>)> = Vec::new();
    for node in [NODE_A, NODE_B] {
        let cw = CoWriter::join(&auth, &vol, node, 0).await;
        let p = cw.part();
        cw.engage().await;
        let mut offs = Vec::new();
        for _ in 0..8 {
            offs.push(cw.alloc.allocate_block().await.expect("co-writer mint"));
        }
        // A co-writer's own free is refused — its accounting is the
        // authority's.
        assert!(
            cw.alloc.free_block(offs[0]).await.is_err(),
            "a co-writer frees nothing, not even its own block"
        );
        cw_offsets.push((p.writer_id(), offs));
        drop(cw);
    }

    // Back to the AUTHORITY's own posture. One process holds ONE ownership
    // plane and ONE publish client, so a rig that plays three nodes must put
    // them back before exercising the authority's allocator again — otherwise
    // the authority's own lane raise routes to itself as if it were a peer and
    // is (correctly) refused for naming lane 0 under node B's identity. In the
    // field these are three processes on three hosts.
    ship::disarm_ownership();
    publish::uninstall_client();
    data_grant::uninstall_custody_client();
    // Global disjointness: three writers, no offset twice.
    fuse_client::set_mount_posture(MountPosture::Writer);
    let mut all: Vec<u64> = authority_offsets.clone();
    for (_, offs) in &cw_offsets {
        all.extend(offs.iter().copied());
    }
    let unique: std::collections::BTreeSet<u64> = all.iter().copied().collect();
    assert_eq!(
        unique.len(),
        all.len(),
        "one device offset was handed to two owners"
    );
    let chunk = authority_alloc.chunk_size();
    for (lane_id, offs) in &cw_offsets {
        for off in offs {
            assert_eq!(
                lane::offset_lane_of(*off, chunk, map.writers()),
                u64::from(*lane_id),
                "each co-writer stayed in its residue class"
            );
        }
    }

    // The AUTHORITY frees the co-writers' blocks: lane-blind, no lookup, and
    // the freed indices re-enter the free supply of the lane the arithmetic
    // names — which is NOT the authority's, so it never re-allocates them.
    let doubles_before = METRICS.block_double_frees.load(Ordering::Relaxed);
    for (_, offs) in &cw_offsets {
        for off in offs {
            authority_alloc
                .recover_block(off / chunk)
                .await
                .expect("the set-wide ledger seeds a reference to a peer's block");
            authority_alloc
                .free_block(*off)
                .await
                .expect("a free never consults a lane");
        }
    }
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles_before,
        "no double free under a partition"
    );
    let freed: std::collections::BTreeSet<u64> = cw_offsets
        .iter()
        .flat_map(|(_, offs)| offs.iter().copied())
        .collect();
    assert!(
        authority_alloc.foreign_lane_free_blocks() >= freed.len() as u64,
        "the freed co-writer blocks are free supply the authority cannot reach"
    );
    for _ in 0..16 {
        let off = authority_alloc.allocate_block().await.expect("mint");
        assert_eq!(
            lane::offset_lane_of(off, chunk, map.writers()),
            0,
            "reuse obeys the same residue class as a fresh mint"
        );
        assert!(
            !freed.contains(&off),
            "the authority re-allocated offset {off}, which belongs to a co-writer's lane"
        );
    }

    auth.stop().await;
}

/// Rung-8 finding #4 (found live by the S7-b kill-matrix + the C6 oracle,
/// 2026-08-16 — the second face of the claim-identity mismatch): the MW
/// arm derived the lane map with the membership owner's INCARNATION uuid
/// as `authority_id` while the claim-set entry it had just written carries
/// the DURABLE node id — so the authority's OWN entry read as a foreign
/// co-writer and every solo MW mount ran a W=2 partition against itself:
/// a phantom lane + reservation frontier (observed live: `lane 0 of 2,
/// resuming at block 1056` on a 1-member set), whose reserved-never-minted
/// residue fsck C6 correctly reported as used-vs-tracked drift (~33
/// blocks/volume/crash) — surviving clean remounts because the frontier
/// records are durable.
///
/// The law: the lane derivation and the claim upsert use ONE identity
/// (`membership::owner_claim_identity`), and a 1-writer set derives SOLO.
/// The second arm documents the wrong shape so the mismatch class stays
/// named: an authority id that does not match its own entry manufactures
/// a 2-writer partition out of a 1-writer set.
#[test]
fn a_one_writer_set_derives_solo_under_the_one_claim_identity_law() {
    let claim_id = squeezefs::membership::owner_claim_identity("uuid-incarnation-1");
    let mut set = ClaimSet::empty(7);
    set.durable = true;
    set.members.push(member(&claim_id, MemberRole::Writer));

    // The fixed caller's shape: authority_id == its own claim entry.
    let solo =
        LaneAssignment::derive(&claim_id, std::slice::from_ref(&set)).expect("a 1-writer set fits");
    assert!(
        solo.authority_partition().is_solo(),
        "a 1-writer set MUST derive SOLO (installs nothing — the single-writer \
         byte-identity law); a non-solo answer here is the phantom self-lane"
    );

    // The pre-fix caller's shape, kept as the named wrong form: a foreign
    // authority id turns the authority's own entry into a co-writer.
    let phantom = LaneAssignment::derive("uuid-incarnation-1", std::slice::from_ref(&set))
        .expect("derive answers");
    assert!(
        !phantom.authority_partition().is_solo(),
        "the mismatch shape manufactures W=2 out of a 1-writer set — this arm is \
         what the caller must never do (it must pass owner_claim_identity)"
    );
}

// ===========================================================================
// 9. Phantom-era frontier hygiene (rung 10 — rung-8 finding #4's named
//    residual: "volumes that already ran the phantom-width eras keep their
//    stale alloc_lane frontier records + stranded blocks; record hygiene
//    for retired widths belongs to rungs 9-10")
// ===========================================================================

/// Contract (residual 1, the SOLO arm): a multi-writer arm whose era derives
/// SOLO **prunes every `alloc_lane:` record the set carries** — under solo
/// the records are read by nothing (a solo engagement installs nothing), and
/// the D0 admission that let this mount arm at all is the proof no live peer
/// of ANY prior width exists: the claim set is the era, and a solo era means
/// every prior writer member was explicitly retired from it. Leaving the
/// records would re-floor every lane of the NEXT partitioned era at a
/// phantom frontier (recovery clause 3), stranding blocks forever.
///
/// RED against dev tip: `prune_stale_lane_records` does not exist — the
/// records survive every arm, exactly as rung 8's evidence note stated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_solo_era_arm_prunes_every_stale_lane_record() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "prune-solo").await;
    let meta = Arc::new(
        squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
            .await
            .expect("the authority mounts its own set"),
    );
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL);

    // The phantom-era residue, exactly as finding #4 minted it: a W=2 era's
    // frontier records on a set whose claim roster names ONE writer.
    lane::commit_lane_raise(&meta, tag, 0, 2, 1056, None)
        .await
        .expect("the phantom lane-0 record commits");
    lane::commit_lane_raise(&meta, tag, 1, 2, 640, None)
        .await
        .expect("the phantom lane-1 record commits");
    assert_eq!(
        lane::load_lane_reservations(&meta, DATA_VOL)
            .await
            .expect("records decode")
            .len(),
        2,
        "fixture: the stale records are durable"
    );

    let pruned_before = METRICS
        .alloc_lane_stale_records_pruned
        .load(Ordering::Relaxed);
    let report = lane::prune_stale_lane_records(&meta, 1)
        .await
        .expect("the hygiene pass runs");
    assert_eq!(report.pruned, 2, "both phantom-era records are deleted");
    assert_eq!(
        report.folded, 0,
        "a solo era folds nothing — there is no current-width record to \
         carry the protection, and none is needed (the D0 admission is the \
         no-live-peer proof)"
    );
    assert!(
        lane::load_lane_reservations(&meta, DATA_VOL)
            .await
            .expect("records decode")
            .is_empty(),
        "the volume carries no alloc_lane residue after the solo prune"
    );
    assert_eq!(
        METRICS
            .alloc_lane_stale_records_pruned
            .load(Ordering::Relaxed)
            - pruned_before,
        2,
        "the hygiene pass is gauged (alloc_lane_stale_records_pruned)"
    );
    // Idempotent: a second pass finds nothing.
    let again = lane::prune_stale_lane_records(&meta, 1)
        .await
        .expect("the second pass runs");
    assert_eq!((again.pruned, again.folded), (0, 0));

    for v in &meta.volumes {
        v.shutdown().await.expect("clean unmount");
    }
}

/// Contract (residual 1, the PARTITIONED arm): under a live width `W`, a
/// record at a FOREIGN width is **folded before it is deleted** — every
/// current-width lane's record is raised to at least the stale record's
/// frontier, and only then is the stale record removed. That preserves the
/// recovery rule's clause-3 guarantee ("a foreign-width record floors every
/// lane") through the deletion: a fenced-but-live writer of the retired era
/// may still hold minted-unpublished offsets below that frontier, and no
/// lane of the CURRENT era may ever mint over them. Fold-then-delete is
/// also crash-safe by construction: a crash between the fold and the delete
/// leaves BOTH protections standing, and the next arm's pass converges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_width_record_is_folded_into_every_current_lane_before_deletion() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "prune-fold").await;
    let meta = Arc::new(
        squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
            .await
            .expect("the authority mounts its own set"),
    );
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL);

    // A retired W=4 era's frontier, plus a CURRENT-era (W=2) record that
    // must survive the pass untouched except for the fold's raise.
    lane::commit_lane_raise(&meta, tag, 3, 4, 800, None)
        .await
        .expect("the retired-era record commits");
    lane::commit_lane_raise(&meta, tag, 1, 2, 100, None)
        .await
        .expect("the current-era record commits");

    let report = lane::prune_stale_lane_records(&meta, 2)
        .await
        .expect("the hygiene pass runs");
    assert_eq!(
        report.pruned, 1,
        "exactly the retired-era record is deleted"
    );
    assert_eq!(report.folded, 1, "and it was folded first");

    let records = lane::load_lane_reservations(&meta, DATA_VOL)
        .await
        .expect("records decode");
    assert!(
        records.iter().all(|r| r.writers == 2),
        "only current-width records remain: {records:?}"
    );
    for lane_id in 0..2u16 {
        let rec = records
            .iter()
            .find(|r| r.lane == lane_id)
            .unwrap_or_else(|| panic!("lane {lane_id} carries a folded record"));
        assert!(
            rec.reserved_upto >= 800,
            "lane {lane_id}'s frontier absorbed the retired era's protection \
             (got {}, want >= 800)",
            rec.reserved_upto
        );
        // The recovery rule over the POST-PRUNE records still refuses to
        // mint below the retired era's frontier — the honesty half.
        let floor = lane::recover_lane_floor(&records, 0, part(2, lane_id));
        assert!(
            floor >= 800,
            "lane {lane_id} recovery floor {floor} dropped below the retired \
             era's 800 — the fold lost the clause-3 protection"
        );
    }

    for v in &meta.volumes {
        v.shutdown().await.expect("clean unmount");
    }
}

/// Contract (residual 1, the KEEP arm): a record at the CURRENT width is
/// never touched by the hygiene pass — it IS this era's protection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_current_width_record_is_never_pruned() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "prune-keep").await;
    let meta = Arc::new(
        squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
            .await
            .expect("the authority mounts its own set"),
    );
    let tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL);
    lane::commit_lane_raise(&meta, tag, 1, 2, 64, None)
        .await
        .expect("the current-era record commits");

    let report = lane::prune_stale_lane_records(&meta, 2)
        .await
        .expect("the hygiene pass runs");
    assert_eq!((report.pruned, report.folded), (0, 0));
    let records = lane::load_lane_reservations(&meta, DATA_VOL)
        .await
        .expect("records decode");
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0],
        lane::LaneReservation {
            writers: 2,
            lane: 1,
            reserved_upto: 64
        },
        "the current-width record is byte-identical after the pass"
    );

    for v in &meta.volumes {
        v.shutdown().await.expect("clean unmount");
    }
}
