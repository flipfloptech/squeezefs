//! **The co-writer's FREE path** — DLM **S9**, the last functional gap in
//! the multi-writer write story: a co-writer can allocate from its lane,
//! write under custody and ship its publishes, but a rewrite that DISPLACES
//! a block refuses at the displaced free — so the posture is append-only,
//! and ruling D8's target (*"a file especially a large one could be getting
//! read/written to different blocks by different applications"*) is a
//! rewrite workload.
//!
//! # The design under contract here
//!
//! A free's durable footprint has FOUR pieces with different authorities,
//! and the split this file pins is:
//!
//! 1. **the `TREE_BLOCK_REFS` delete rides the publish** — already shipped
//!    (`meta_ship::publish::commit_block_refs` / the layout publishes), one
//!    whole-tx commit on the authority. The publish is the durable ordering
//!    point: once it lands, the offset is durably unreferenced, and losing
//!    everything after it recovers to FREE (never a leak past recovery,
//!    never a double free);
//! 2. **the accounting ladder ships as a VERB** (`PublishCall::FreeBlocks`,
//!    the `RaiseAllocLane` precedent: schema-versioned, owner-validated):
//!    the AUTHORITY executes `begin_free → tier purge → reclaim enqueue →
//!    finish_free` exactly as if it had freed locally, so the grace ring
//!    (§6.8 item 3), S7's quarantine and the reclaim manners all apply
//!    verbatim on the one node whose reclaimer is live;
//! 3. **the exactly-once witness is `(lease_epoch, request_id)`** on the
//!    owner's dedup window — S8's `DedupWindow`, reused, never a third
//!    pattern: the lease epoch is minted by the authority, monotone and
//!    never reused, so a replay under the SAME epoch answers the cached
//!    outcome and a replay under a DEAD epoch refuses by era (a retry never
//!    re-keys across a re-join, which is what closes the re-allocated-ABA
//!    window);
//! 4. **the freed offset lands in the free supply of the lane the
//!    arithmetic names** — frees stay lane-blind (`b % W` derives the
//!    owner), so the authority's free list carries it and whoever owns that
//!    lane reuses it (the authority immediately if it is lane 0's; a lane
//!    holder through its recovery floor otherwise).
//!
//! **W1 stays authority-only** (argued in `docs/operations.md`): the
//! in-place patch retires a LIFETIME — durable ownership state (§6.2 item
//! 6) — and its §5.1 clone/patch fence is a two-word process-local protocol
//! that no wire can compose; a shipped patch-retire would also put a
//! control-class RTT inside the one path whose whole win is "one DMA, zero
//! metadata". A co-writer's small overwrite rides CoW-rewrite + shipped
//! free instead, and `cowriter.accounting_refusals` keeps counting the
//! local W1 arm.
//!
//! RED against `dev` (ab69c5d5): `BackendRouter::free_block` on a co-writer
//! runs `begin_free`, whose `plane_gate` refuses — the free is a **silent
//! local no-op** that counts `cowriter_accounting_refusals` and leaks the
//! offset on every node forever (test 0 pins that shape behaviourally with
//! landed API only). `PublishCall::FreeBlocks`, `publish::ship_free_blocks`,
//! `publish::install_free_executor`, `cowriter::ship_displaced_frees` and
//! `cowriter::router_free_executor` do not exist.
//!
//! **No numbers here — ruling D11.** The bench coverage
//! (`benches/write_path_bench.rs::cowriter_free`) is written and NOT run.
//!
//! # What one process cannot pin (stated, not hidden)
//!
//! Two hosts and a PR-capable fabric are what a real two-host rewrite
//! needs; here the co-writer is a second backend + router over the same
//! files (the S8/S9 test discipline), and the mount-posture latch is
//! process-global — which is exactly why the owner-side executor runs under
//! the explicit authority-accounting scope this file also pins the
//! non-reachability of.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::alloc_lane_grant::{self as grant, LaneFloor};
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::cluster_wire as cw;
use squeezefs::cowriter::{
    self, AdmissionRequest, AuthorityLeaseEvidence, RegistrantEvidence, VolumeAdmissionEvidence,
};
use squeezefs::data_alloc_lane as lane;
use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::dlm::DlmClient;
use squeezefs::free_grace;
use squeezefs::fuse_client::{self, MountPosture, SqueezefsFilesystem, METRICS};
use squeezefs::membership::{
    self, ClaimSet, ClaimSetMember, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks,
    MemberIdentity, MemberRole, MembershipOwner, RenewOutcome,
};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim};
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{
    format_v3, BuilderConfig, FormatV3Options, ImageBuilder,
};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use squeezefs::nvme_dev::{self, NvmeBlockDev};
use squeezefs::routing::{BackendRouter, DataRouter};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const VOL_LEN: u64 = 64 * 1024 * 1024;
/// Sparse data-device backing: big enough that every lane index this file
/// mints stays inside it (punch-on-free needs `offset < len`).
const DEV_LEN: u64 = 2 * 1024 * 1024 * 1024;

/// The `job:enroll`-class storage-trust secret both halves prove possession
/// of (S3's root of trust — ruling D2).
const SECRET: &[u8] = b"s9-cowriter-free-storage-trust-secret";

/// The authority's own claim-set member id (the membership owner's id in
/// production).
const AUTHORITY_ID: &str = "authority-membership-owner";
/// Two enrolled co-writer nodes, in the sorted order the assignment uses.
const NODE_A: &str = "node_00000000aaaaaaaa";
const NODE_B: &str = "node_00000000bbbbbbbb";

/// A durable data-volume id (KD-5's `vol-{16 hex}` shape) — `volume_tag`
/// decodes it verbatim, so the wire carries the durable identity and never
/// a path.
const DATA_VOL: &str = "vol-00000000000000f1";

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
        publish::uninstall_free_executor();
        publish::uninstall_harvest_executor();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
        free_grace::reset_for_test();
        membership::uninstall();
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Volumes, allocators, evidence (the mw_cowriter_lane_tests fixtures, plus a
// real data-plane router on each side)
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

/// A sparse file the block plane punches into (the async_block_reclaim
/// fixture shape: file backing ⇒ the queued reclaim is a PUNCH_HOLE).
fn data_device(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(DEV_LEN).unwrap();
    p
}

async fn allocator(id: &str) -> Arc<BlockAllocator> {
    Arc::new(BlockAllocator::new(id).await.expect("allocator"))
}

/// One side's data plane: an allocator + a router over the SHARED backing.
async fn data_plane(dev: &Path) -> (Arc<BlockAllocator>, Arc<BackendRouter>) {
    let alloc = allocator(DATA_VOL).await;
    let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
    let chunk = alloc.chunk_size();
    let br = Arc::new(BackendRouter::new(
        Arc::clone(&alloc),
        nvme,
        Arc::new(AtomicU64::new(chunk)),
    ));
    (alloc, br)
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
// The rig: one authority (custody + publish + a live data plane whose
// reclaimer executes the shipped frees) and N co-writers
// ---------------------------------------------------------------------------

struct Authority {
    listener: Arc<cw::RpcListener>,
    owner: Arc<WriteCustodyOwner>,
    meta: Arc<RoutedMetaBackend>,
    endpoint: String,
    alloc: Arc<BlockAllocator>,
    br: Arc<BackendRouter>,
}

impl Authority {
    async fn start(vol: &Path, dev: &Path, members: &[&str]) -> Authority {
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
        let assignment = grant::LaneAssignment::derive(AUTHORITY_ID, std::slice::from_ref(&set))
            .expect("the roster fits the lane space");
        owner.install_lane_assignment(Arc::clone(&assignment));
        data_grant::install_custody_owner(Arc::clone(&owner));

        // The authority's own data plane over the shared backing, its own
        // lane engaged the way `arm_multi_writer` does (floor from its own
        // recovery), and the shipped-free EXECUTOR installed — the owner
        // half this branch adds beside `install_frontier_source`.
        let (alloc, br) = data_plane(dev).await;
        fuse_client::set_mount_posture(MountPosture::Writer);
        grant::engage_allocator_lane(&alloc, assignment.authority_partition(), &meta, {
            LaneFloor::Local
        })
        .await
        .expect("the authority engages its own lane");
        publish::install_free_executor(cowriter::router_free_executor(
            Arc::clone(&br),
            Arc::clone(&meta),
            // One process plays both nodes (this file's module docs): the
            // process-global ownership map is the CO-WRITER's all-foreign
            // one, so the live view would ship the authority's ledger read
            // to itself. The authority's set is entirely its own here —
            // the all-local binding is its truth (finding 13's venue law).
            cowriter::local_owner_view(),
        ));
        publish::install_harvest_executor(cowriter::router_harvest_executor(Arc::clone(&br)));

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
            alloc,
            br,
        }
    }

    /// The durable reference population of one block — the owner-side
    /// validation the free verb runs, read back for assertions.
    async fn population(&self, block_idx: u64) -> usize {
        cowriter::durable_block_refcount(&self.meta, volume_tag(DATA_VOL), block_idx)
            .await
            .expect("the ledger answers")
    }

    fn free_listed(&self, block_idx: u64) -> bool {
        self.alloc.free_block_indices().contains(&block_idx)
    }

    async fn stop(self) {
        self.listener.shutdown();
        for v in &self.meta.volumes {
            v.shutdown().await.expect("the authority unmounts clean");
        }
    }
}

/// One co-writer: posture latched, routed set opened through the admission,
/// ownership armed all-foreign, custody + publish clients installed, its
/// granted lane engaged, and its own router with the reclaim queue CEASED —
/// exactly `cowriter::arm`'s data-plane latch.
struct CoWriter {
    meta: Arc<RoutedMetaBackend>,
    client: Arc<WriteCustodyClient>,
    alloc: Arc<BlockAllocator>,
    br: Arc<BackendRouter>,
}

impl CoWriter {
    async fn join(auth: &Authority, vol: &Path, dev: &Path, node_id: &str) -> CoWriter {
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
        let (alloc, br) = data_plane(dev).await;
        br.reclaim_cease();
        grant::engage_allocator_lane(&alloc, client.lane_partition(), &meta, {
            LaneFloor::Authority
        })
        .await
        .expect("the granted lane engages");
        CoWriter {
            meta,
            client,
            alloc,
            br,
        }
    }

    /// One rewrite's DISPLACEMENT half, end to end: mint a fresh block from
    /// this mount's lane, ship the publish that takes the new reference and
    /// releases the displaced one (`ino`'s block `block_index` moves from
    /// `old_idx` to the new block), and return the new index. The displaced
    /// FREE itself is the caller's — that is the seam under test.
    async fn rewrite_block(&self, ino: u64, block_index: u32, old_idx: u64) -> u64 {
        let new_off = self
            .alloc
            .allocate_block()
            .await
            .expect("a laned co-writer mints from its own residue class");
        let new_idx = new_off / self.alloc.chunk_size();
        let tag = volume_tag(DATA_VOL);
        publish::commit_block_refs(
            &self.meta,
            ino,
            &[
                BlockRefOp::taken(BlockRef {
                    vol_tag: tag,
                    block_idx: new_idx,
                    owner_ino: ino,
                    block_index,
                }),
                BlockRefOp::released(BlockRef {
                    vol_tag: tag,
                    block_idx: old_idx,
                    owner_ino: ino,
                    block_index,
                }),
            ],
        )
        .await
        .expect("the reference delta ships on the landed publish surface");
        new_idx
    }
}

/// A file on the authority whose block 0 is `idx`, with the durable
/// reference committed and the authority's RAM map tracking it — the state
/// an authority-written striped file leaves behind.
async fn authority_file_with_block(auth: &Authority, name: &str, idx: u64) -> u64 {
    let ino = auth
        .meta
        .create_with_rdev_size(1, name, 0o100644, 0, 0, 0, 0)
        .await
        .expect("create on the authority")
        .ino;
    auth.meta
        .commit_block_refs(
            ino,
            &[BlockRefOp::taken(BlockRef {
                vol_tag: volume_tag(DATA_VOL),
                block_idx: idx,
                owner_ino: ino,
                block_index: 0,
            })],
        )
        .await
        .expect("the durable reference commits");
    ino
}

// ===========================================================================
// 0. The seam itself, in landed API only — the behavioural red
// ===========================================================================

/// Contract (**the seam**, landed API only): a co-writer's ROUTER-level
/// displaced free is **never a silent local leak**. Against `dev` the call
/// returns `Ok(())` having freed nothing anywhere — `begin_free`'s
/// plane_gate eats it, counts `cowriter_accounting_refusals`, and the
/// offset is durably unreferenced yet on no free list on any node, forever.
///
/// The closed shape: the free SHIPS. With no publish plane armed (this
/// test arms none) that means a LOUD refusal — the
/// `meta_ship_publish.refusals` law, not a silent local execution — and
/// the accounting-refusal counter does not move, because a terminal free
/// is no longer a local accounting act on this posture at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_co_writers_displaced_free_is_never_a_silent_local_leak() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let dev = data_device(dir.path(), "red.dev");
    let (alloc, br) = data_plane(&dev).await;
    alloc
        .engage_alloc_lanes(part(2, 1))
        .expect("engage the granted lane");
    fuse_client::set_mount_posture(MountPosture::CoWriter);

    let off = alloc
        .allocate_block()
        .await
        .expect("a laned co-writer allocates (the landed seam)");
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);

    let out = br.free_block(&off.to_string()).await;
    assert!(
        out.is_err(),
        "a co-writer's displaced free must SHIP or refuse LOUD — Ok(()) with nothing freed \
         anywhere is the silent leak this branch closes"
    );
    assert_eq!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed),
        refusals_before,
        "the displaced free no longer lands in accounting_refusals — it is not a local \
         accounting act on this posture"
    );
    assert_eq!(
        alloc.refcount(off),
        Some(1),
        "a free that could not ship moved NOTHING locally (leak-safe: the offset is still \
         tracked, and recovery reconciles)"
    );
    assert!(
        !alloc
            .free_block_indices()
            .contains(&(off / alloc.chunk_size())),
        "and it certainly did not enter the local free list"
    );
}

// ===========================================================================
// 1. The headline: a co-writer rewrite frees the displaced block through
//    the authority's FULL ladder, and the offset is reusable afterwards
// ===========================================================================

/// Contract (requirement 1): new block from the co-writer's lane, publish
/// shipped (refs delta — the durable delete — riding it), displaced block
/// freed through the AUTHORITY's ladder (RAM release, tier purge, reclaim
/// enqueue + punch, free-list publish), and the offset reusable afterwards
/// by whoever's lane it belongs to — here lane 0, so the authority itself
/// re-serves it from its free list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writer_rewrite_frees_the_displaced_block_through_the_authoritys_ladder() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "rewrite").await;
    let dev = data_device(dir.path(), "rewrite.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    // The authority wrote a file: block 0 lives at a lane-0 index it minted
    // and tracks.
    let old_off = auth
        .alloc
        .allocate_block()
        .await
        .expect("the authority mints in lane 0");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "rewritten.bin", old_idx).await;
    assert_eq!(auth.population(old_idx).await, 1);

    // The co-writer takes over and rewrites block 0.
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);
    let queued_before = METRICS.block_free_reclaim_queued.load(Ordering::Relaxed);
    let punched_before = METRICS.block_free_file_punches.load(Ordering::Relaxed);
    let shipped_before = publish::stats().free_shipped_blocks;

    let new_idx = cwr.rewrite_block(ino, 0, old_idx).await;
    assert_eq!(
        auth.population(old_idx).await,
        0,
        "the TREE_BLOCK_REFS delete rode the shipped publish — the durable ordering point"
    );
    assert_eq!(auth.population(new_idx).await, 1);

    // The displaced free: the write path's call, on the co-writer's router.
    cwr.br
        .free_block(&old_off.to_string())
        .await
        .expect("the displaced free ships");
    auth.br.reclaim_drain().await;

    assert!(
        auth.free_listed(old_idx),
        "the displaced offset re-entered the free supply — on the AUTHORITY, whose ladder ran"
    );
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped_before,
        1,
        "the free travelled as a verb (the engagement instrument)"
    );
    assert_eq!(
        METRICS.block_free_reclaim_queued.load(Ordering::Relaxed) - queued_before,
        1,
        "exactly one reclaim entry — the authority's; the co-writer's ceased queue saw nothing"
    );
    assert!(
        METRICS.block_free_file_punches.load(Ordering::Relaxed) > punched_before,
        "the device range was returned by the authority's reclaimer (the block_free_* ledger \
         stays honest: device work happens where the accounting lives)"
    );
    assert_eq!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed),
        refusals_before,
        "the product rewrite path no longer counts a single accounting refusal"
    );
    assert_eq!(
        cwr.alloc.refcount(old_off),
        None,
        "the co-writer's local tracking of the displaced offset retired with the shipped free"
    );

    // Reusable by whoever's lane it is: lane 0 is the authority's, and the
    // free-list-first funnel serves it straight back. (One process plays
    // both nodes, so the co-writer's client halves step aside first — the
    // trio-test discipline.)
    drop(cwr);
    ship::disarm_ownership();
    publish::uninstall_client();
    data_grant::uninstall_custody_client();
    fuse_client::set_mount_posture(MountPosture::Writer);
    assert_eq!(
        lane::block_lane_of(old_idx, 2),
        0,
        "the displaced block belongs to lane 0"
    );
    let reused = auth
        .alloc
        .allocate_block()
        .await
        .expect("the authority allocates");
    assert_eq!(
        reused, old_off,
        "the lane's owner reuses the freed offset (free-list-first, lane-filtered)"
    );
    auth.stop().await;
}

// ===========================================================================
// 2. Exactly-once: the (lease_epoch, request_id) witness
// ===========================================================================

/// Contract (requirement 2): a REPLAY of the free verb — the retry after a
/// lost reply, carrying the SAME lease epoch and request id — is absorbed
/// by the owner's dedup window and answers the cached outcome. One free,
/// zero double-frees, and the replay is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_free_verb_is_absorbed_by_the_dedup_window() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "replay").await;
    let dev = data_device(dir.path(), "replay.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "replayed.bin", old_idx).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let _new = cwr.rewrite_block(ino, 0, old_idx).await;

    let doubles_before = METRICS.block_double_frees.load(Ordering::Relaxed);
    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);
    let replays_before = publish::stats().free_replays;

    let epoch = cwr.client.lease_epoch();
    let request_id = 0xF00D;
    let first = publish::ship_free_blocks(
        &auth.endpoint,
        volume_tag(DATA_VOL),
        vec![old_idx],
        epoch,
        request_id,
    )
    .await
    .expect("the free executes");
    // The replay: same epoch, same id — the retry-after-lost-reply shape.
    let second = publish::ship_free_blocks(
        &auth.endpoint,
        volume_tag(DATA_VOL),
        vec![old_idx],
        epoch,
        request_id,
    )
    .await
    .expect("the replay is ANSWERED, not re-applied");
    assert_eq!(
        first, second,
        "a replay answers the winner's own cached outcome"
    );
    assert_eq!(
        publish::stats().free_replays - replays_before,
        1,
        "and it is counted as a dedup hit"
    );

    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(old_idx), "freed exactly once");
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles_before,
        "no double free under replay"
    );
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed),
        untracked_before,
        "and no refused-release residue either — the window absorbed it before the ladder"
    );

    drop(cwr);
    auth.stop().await;
}

/// Contract (requirement 2's other face — the tripwires keep firing): a
/// SECOND free of the same block under a FRESH request id is not a replay,
/// it is the double-release lineage — and the authority refuses it loud on
/// the existing untracked-free tripwire, with the free list never holding
/// the index twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_free_of_the_same_block_is_refused_on_the_double_release_tripwire() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "double").await;
    let dev = data_device(dir.path(), "double.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "doubled.bin", old_idx).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let _new = cwr.rewrite_block(ino, 0, old_idx).await;

    let epoch = cwr.client.lease_epoch();
    let tag = volume_tag(DATA_VOL);
    let first = publish::ship_free_blocks(&auth.endpoint, tag, vec![old_idx], epoch, 1)
        .await
        .expect("the first free executes");
    assert_eq!(first, vec![publish::FreeVerdict::Freed]);
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(old_idx));

    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);
    let second = publish::ship_free_blocks(&auth.endpoint, tag, vec![old_idx], epoch, 2)
        .await
        .expect("the verb is served — its VERDICT is the refusal");
    assert_eq!(
        second,
        vec![publish::FreeVerdict::Refused],
        "a fresh-id re-free of a freed block is the double-release lineage, refused"
    );
    assert!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed)
            > untracked_before,
        "and the EXISTING tripwire fires — the shipped path never bypasses it"
    );
    assert_eq!(
        auth.alloc
            .free_block_indices()
            .iter()
            .filter(|i| **i == old_idx)
            .count(),
        1,
        "the free list holds the index exactly once"
    );

    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 3. Fencing composes: era refusal, and what a dead co-writer leaves
// ===========================================================================

/// Contract (requirement 3): a revoked co-writer's in-flight free verb is
/// refused **by era** — the presented lease epoch is no longer custody —
/// counted on `free_stale_refusals`, and nothing is freed. Its
/// displaced-but-unfreed blocks are owned by the DURABLE LEDGER: the next
/// derivation (mount recovery / fsck C6) re-derives them as free, which
/// the second half of this test performs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_era_free_verb_is_refused_and_recovery_owns_the_unfreed_block() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "stale").await;
    let dev = data_device(dir.path(), "stale.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "fenced.bin", old_idx).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let new_idx = cwr.rewrite_block(ino, 0, old_idx).await;
    let epoch = cwr.client.lease_epoch();

    // The authority revokes this co-writer (the eviction / lease-loss
    // shape); its in-flight free verb now names a dead era.
    auth.owner.revoke_client(NODE_A, "test: revoked mid-free");
    let stale_before = publish::stats().free_stale_refusals;
    let err = publish::ship_free_blocks(
        &auth.endpoint,
        volume_tag(DATA_VOL),
        vec![old_idx],
        epoch,
        7,
    )
    .await
    .expect_err("a free under a dead lease epoch is refused by era");
    // Finding #6 (design-mw-layout-versions §6a): a stale refusal whose
    // presented epoch is this client's CURRENT lease surfaces in the FENCE
    // class and composes the full fence — the pull-based revocation law at
    // the publish round trip (the operator detail rides the log line).
    assert!(
        matches!(err, squeezefs::error::SqueezefsError::WriterGuardFenced),
        "the era refusal surfaces in the fence class: {err}"
    );
    assert!(
        data_custody::poisoned(),
        "a current-epoch stale refusal IS the fence signal (custody poisoned)"
    );
    assert_eq!(publish::stats().free_stale_refusals - stale_before, 1);
    assert!(
        !auth.free_listed(old_idx),
        "nothing was freed under a dead era"
    );
    assert_eq!(
        auth.alloc.refcount(old_off),
        Some(1),
        "the authority's tracking is untouched — the leak-safe direction"
    );

    // The durable ledger owns the unfreed displaced block: a fresh
    // derivation (the mount-recovery walk over durable references) finds
    // the old index unreferenced and re-derives it FREE — never a
    // permanent leak, never a double free.
    let successor = allocator(DATA_VOL).await;
    fuse_client::set_mount_posture(MountPosture::Writer);
    let refs = auth.meta.volumes[0]
        .block_ref_scan(volume_tag(DATA_VOL))
        .await
        .expect("the durable census");
    for r in &refs {
        successor
            .recover_block(r.block_idx)
            .await
            .expect("seed a live reference");
    }
    assert!(
        refs.iter().any(|r| r.block_idx == new_idx),
        "the rewrite's new block is durably referenced"
    );
    assert!(
        refs.iter().all(|r| r.block_idx != old_idx),
        "the displaced block is durably unreferenced"
    );
    assert!(
        successor.free_block_indices().contains(&old_idx),
        "recovery derives the lost-verb offset straight back to FREE (the crash contract: \
         referenced or free, never a leak past recovery)"
    );

    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 4. Crash windows: before the verb, and after the ack
// ===========================================================================

/// Contract: the publish is the durable ordering point, and the verb is
/// pure accounting downstream of it — so a co-writer that dies BEFORE its
/// free verb lands loses hygiene only (recovery derives the offset free,
/// pinned above), and one that dies AFTER the ack owes nothing (the
/// authority's ladder is crash-safe locally, and a resurrected replay of
/// the acked id under the old epoch refuses by era rather than
/// double-applying).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_the_verb_loses_hygiene_only_and_crash_after_ack_owes_nothing() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "crash").await;
    let dev = data_device(dir.path(), "crash.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let off_a = auth.alloc.allocate_block().await.expect("mint");
    let idx_a = off_a / auth.alloc.chunk_size();
    let ino_a = authority_file_with_block(&auth, "crash-a.bin", idx_a).await;
    let off_b = auth.alloc.allocate_block().await.expect("mint");
    let idx_b = off_b / auth.alloc.chunk_size();
    let ino_b = authority_file_with_block(&auth, "crash-b.bin", idx_b).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    // Rewrite A: publish lands, the co-writer DIES before shipping the
    // free (nothing more happens for idx_a in this era).
    let _new_a = cwr.rewrite_block(ino_a, 0, idx_a).await;
    // Rewrite B: publish lands, free ships, ACK received — then it dies.
    let _new_b = cwr.rewrite_block(ino_b, 0, idx_b).await;
    let epoch = cwr.client.lease_epoch();
    let verdicts =
        publish::ship_free_blocks(&auth.endpoint, volume_tag(DATA_VOL), vec![idx_b], epoch, 11)
            .await
            .expect("the acked free");
    assert_eq!(verdicts, vec![publish::FreeVerdict::Freed]);
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(idx_b));

    // The crash: revoked (evicted), custody gone.
    auth.owner.revoke_client(NODE_A, "test: crashed");

    // After-ack: a resurrected replay of the SAME acked id under the dead
    // epoch refuses by era — it can never double-apply, even though the
    // offset is now free (and could in principle be re-owned).
    let doubles_before = METRICS.block_double_frees.load(Ordering::Relaxed);
    let replay =
        publish::ship_free_blocks(&auth.endpoint, volume_tag(DATA_VOL), vec![idx_b], epoch, 11)
            .await;
    assert!(
        replay.is_err(),
        "a dead era's replay is refused BEFORE the window — retries never re-key, so the \
         re-allocated-ABA window is structurally closed"
    );
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles_before
    );

    // Before-the-verb: idx_a is durably unreferenced, RAM-tracked, on no
    // free list — unavailable space until a derivation reconciles it, and
    // that is the leak-safe direction (test 3 pins the derivation).
    assert_eq!(auth.population(idx_a).await, 0);
    assert!(!auth.free_listed(idx_a));
    assert_eq!(auth.alloc.refcount(off_a), Some(1));

    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 5. §6.8 item 3: shipped frees enter the grace ring like local ones
// ===========================================================================

/// Contract (requirement 5): the shipped free runs the authority's OWN
/// `finish_free`, so with the reader plane armed the freed offset is
/// admitted to the grace ring — not the free list — until every live
/// reader acknowledges past it, exactly as a local free would be. The gate
/// composes downstream of the verb: custody proof first, reader coherence
/// second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shipped_frees_enter_the_grace_ring_when_the_reader_plane_is_armed() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "grace").await;
    let dev = data_device(dir.path(), "grace.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "graced.bin", old_idx).await;

    // Arm the reader plane on the AUTHORITY (the membership owner) with
    // one live reader that has acknowledged nothing.
    let ticks = Arc::new(AtomicU64::new(10_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    let m_owner = MembershipOwner::arm(
        "grace-owner",
        3,
        2,
        LeaseClocks::derive(Duration::from_micros(250)).expect("shipped derivation"),
        clock.clone(),
    )
    .expect("the membership owner arms");
    membership::install_owner(Arc::clone(&m_owner));
    free_grace::arm_owner_plane(clock, m_owner.clocks()).expect("the derived bound is safe");
    let reader = match m_owner.join(JoinRequest {
        id: "r-1".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-free-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(g) => g,
        JoinOutcome::Refused { reason, .. } => panic!("join refused: {reason}"),
        JoinOutcome::UnknownLease { reason } => panic!("join answered UnknownLease: {reason}"),
    };
    m_owner.refresh_free_grace_bound();
    assert!(free_grace::armed());

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let _new = cwr.rewrite_block(ino, 0, old_idx).await;
    let deferrals_before = free_grace::deferrals();
    cwr.br
        .free_block(&old_off.to_string())
        .await
        .expect("the displaced free ships");
    auth.br.reclaim_drain().await;

    assert_eq!(
        free_grace::deferrals() - deferrals_before,
        1,
        "the shipped free's finish_free DEFERRED into the grace ring"
    );
    assert!(
        !auth.free_listed(old_idx),
        "a graced offset is never on the free list"
    );
    assert!(auth.alloc.grace_holds(old_off));

    // The reader acknowledges — the offset returns to the free supply.
    let label = auth
        .alloc
        .grace_oldest_label()
        .expect("the held entry carries a label");
    assert!(
        matches!(
            m_owner.renew("r-1", reader.epoch, label),
            RenewOutcome::Renewed(_)
        ),
        "the acknowledgement rides the reader's renewal"
    );
    m_owner.refresh_free_grace_bound();

    // The harvest runs at the allocation funnel's head (posture flip: the
    // one-process rig steps the co-writer aside first).
    drop(cwr);
    ship::disarm_ownership();
    publish::uninstall_client();
    data_grant::uninstall_custody_client();
    fuse_client::set_mount_posture(MountPosture::Writer);
    let reused = auth.alloc.allocate_block().await.expect("allocate");
    assert_eq!(
        reused, old_off,
        "an acknowledged shipped-free offset is served exactly like a local one"
    );

    auth.stop().await;
}

// ===========================================================================
// 6. Postures that ship are byte-identical; the local arms stay refused
// ===========================================================================

/// Contract (requirement 4): the authority's local free path is
/// byte-identical (no publish counter moves, no scope needed); a READER's
/// free arms refuse with the reader's own text; and a co-writer's DIRECT
/// allocator-level arms — terminal free, `free_block`, the specific claim,
/// the W1 retire — still refuse and still count `accounting_refusals`:
/// those are the defense-in-depth arms behind the router seam, and W1 is
/// DECIDED authority-only (the patch retires a lifetime, and its §5.1
/// fence is process-local — a co-writer's small overwrite rides
/// CoW-rewrite + shipped free instead).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authority_solo_and_reader_free_paths_are_unchanged_and_w1_stays_refused() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let dev = data_device(dir.path(), "identity.dev");

    // Solo WRITER: the shipped path, verbatim — and the free-verb gauges
    // do not move for it.
    fuse_client::set_mount_posture(MountPosture::Writer);
    let before = (
        publish::stats().free_shipped_blocks,
        publish::stats().free_served_blocks,
        publish::stats().free_replays,
        publish::stats().free_stale_refusals,
        publish::stats().free_ship_failures,
    );
    let (alloc, br) = data_plane(&dev).await;
    let off = alloc.allocate_block().await.expect("mint");
    br.free_block(&off.to_string()).await.expect("local free");
    br.reclaim_drain().await;
    assert!(alloc
        .free_block_indices()
        .contains(&(off / alloc.chunk_size())));
    assert_eq!(
        (
            publish::stats().free_shipped_blocks,
            publish::stats().free_served_blocks,
            publish::stats().free_replays,
            publish::stats().free_stale_refusals,
            publish::stats().free_ship_failures,
        ),
        before,
        "not one free-verb gauge moved for a single-writer free"
    );

    // READER: refused with the reader's own unchanged text.
    fuse_client::set_mount_posture(MountPosture::Reader);
    let msg = alloc
        .free_block(off)
        .await
        .expect_err("a reader frees nothing")
        .to_string();
    assert!(
        msg.contains("read-only") && msg.contains("-o ro"),
        "the reader refusal text is unchanged: {msg}"
    );
    assert!(!alloc.begin_free(off), "terminal free refuses a reader");

    // CO-WRITER, allocator-level: still refused, still counted — the seam
    // is the ROUTER, and nothing below it was weakened.
    fuse_client::set_mount_posture(MountPosture::CoWriter);
    let laned = allocator("vol-00000000000000f2").await;
    laned.engage_alloc_lanes(part(2, 1)).expect("lane");
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);
    let free_err = laned
        .free_block(0)
        .await
        .expect_err("a co-writer's ALLOCATOR-level free still refuses")
        .to_string();
    assert!(
        free_err.to_lowercase().contains("co-writer"),
        "the co-writer text: {free_err}"
    );
    assert!(!laned.begin_free(0), "terminal free refuses");
    assert!(
        laned.allocate_specific_block(7).await.is_err(),
        "the specific claim stays the authority's act"
    );
    assert!(
        !laned.begin_patch_sole_owner(0),
        "W1's incarnation retire stays refused — the DECIDED posture: a lifetime retire is \
         durable ownership state, and the §5.1 fence is process-local"
    );
    assert!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed) > refusals_before,
        "accounting_refusals keeps counting exactly these arms"
    );

    // And the authority-accounting scope is NOT ambiently active on a
    // co-writer task: it exists for the owner-side executor only.
    assert!(
        !cowriter::authority_accounting_scope_active(),
        "the scope is the executor's venue marker, never a co-writer capability"
    );
}

// ===========================================================================
// 7. The storm: two co-writers rewrite disjoint files while a reader acks
// ===========================================================================

/// Contract (the integration, multi-thread): two co-writers rewrite
/// DISJOINT files — each minting from its own lane and shipping frees for
/// its own displaced blocks concurrently — while a live reader
/// acknowledges. Every displaced block is freed exactly once through the
/// authority, the grace ledger closes (`deferrals ≡ releases + held`), and
/// the double-free tripwires stay silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_co_writers_rewrite_disjoint_files_while_a_reader_acks() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "storm").await;
    let dev = data_device(dir.path(), "storm.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A, NODE_B]).await;

    // Reader plane armed on the authority.
    let ticks = Arc::new(AtomicU64::new(10_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    let m_owner = MembershipOwner::arm(
        "storm-owner",
        3,
        2,
        LeaseClocks::derive(Duration::from_micros(250)).expect("shipped derivation"),
        clock.clone(),
    )
    .expect("the membership owner arms");
    membership::install_owner(Arc::clone(&m_owner));
    free_grace::arm_owner_plane(clock, m_owner.clocks()).expect("derived bound");
    let reader = match m_owner.join(JoinRequest {
        id: "r-storm".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-storm".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(g) => g,
        JoinOutcome::Refused { reason, .. } => panic!("join refused: {reason}"),
        JoinOutcome::UnknownLease { reason } => panic!("join answered UnknownLease: {reason}"),
    };
    m_owner.refresh_free_grace_bound();

    // Eight authority-written files whose blocks the co-writers displace
    // (disjoint: A rewrites the even files, B the odd ones).
    let mut displaced: Vec<(u64, u64, u64)> = Vec::new(); // (ino, idx, off)
    for i in 0..8u32 {
        let off = auth.alloc.allocate_block().await.expect("mint");
        let idx = off / auth.alloc.chunk_size();
        let ino = authority_file_with_block(&auth, &format!("storm-{i}.bin"), idx).await;
        displaced.push((ino, idx, off));
    }

    let doubles_before = METRICS.block_double_frees.load(Ordering::Relaxed);
    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);

    // Each co-writer joins in turn (one process holds one posture latch —
    // the trio-test discipline), rewrites its half, and leaves an
    // epoch-stamped free plan to ship concurrently below.
    let mut plans: Vec<(u64, Vec<u64>)> = Vec::new(); // (lease_epoch, idxs)
    for (node, parity) in [(NODE_A, 0u32), (NODE_B, 1u32)] {
        let cwr = CoWriter::join(&auth, &vol, &dev, node).await;
        let mut idxs = Vec::new();
        for (i, (ino, idx, _off)) in displaced.iter().enumerate() {
            if i as u32 % 2 != parity {
                continue;
            }
            let _new = cwr.rewrite_block(*ino, 0, *idx).await;
            idxs.push(*idx);
        }
        plans.push((cwr.client.lease_epoch(), idxs));
        drop(cwr);
        ship::disarm_ownership();
        publish::uninstall_client();
        data_grant::uninstall_custody_client();
    }

    // The concurrent half: both co-writers' frees ship at once (one verb
    // per displaced block — the write path's per-key shape) while the
    // reader acknowledges in a loop. The wire client is re-armed for the
    // ship phase (the frame's client string is audit-only; what the owner
    // VALIDATES is each plan's lease epoch).
    publish::install_client(publish::PublishClient::new(
        "storm-shipper",
        SECRET.to_vec(),
    ));
    let stop_acks = Arc::new(AtomicBool::new(false));
    let acker = {
        let m_owner = Arc::clone(&m_owner);
        let auth_alloc = Arc::clone(&auth.alloc);
        let stop = Arc::clone(&stop_acks);
        let epoch = reader.epoch;
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                let label = auth_alloc.grace_oldest_label().unwrap_or(u64::MAX);
                assert!(
                    matches!(
                        m_owner.renew("r-storm", epoch, label),
                        RenewOutcome::Renewed(_)
                    ),
                    "the reader's renewal must stay admitted"
                );
                m_owner.refresh_free_grace_bound();
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    };
    let mut ships = Vec::new();
    let mut req_id = 100u64;
    for (epoch, idxs) in &plans {
        for idx in idxs {
            req_id += 1;
            let endpoint = auth.endpoint.clone();
            let (epoch, idx, id) = (*epoch, *idx, req_id);
            ships.push(tokio::spawn(async move {
                publish::ship_free_blocks(&endpoint, volume_tag(DATA_VOL), vec![idx], epoch, id)
                    .await
                    .expect("a storm free executes")
            }));
        }
    }
    for handle in ships {
        let verdicts = handle.await.expect("no shipped task panics");
        assert_eq!(verdicts, vec![publish::FreeVerdict::Freed]);
    }
    auth.br.reclaim_drain().await;

    // Give the acker a short window to release what it can (harvests run
    // on the free/allocation venues, so held-at-deadline entries are
    // legitimate — the closure assertion below counts them).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while auth.alloc.grace_len() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop_acks.store(true, Ordering::Release);
    acker.await.expect("the acker exits clean");

    fuse_client::set_mount_posture(MountPosture::Writer);
    // ONE funnel pass triggers the head harvest for everything already
    // acknowledged — and may legitimately re-CLAIM one freed lane-0
    // offset (free-list-first is the property test 1 pins), so the
    // census below admits exactly that one re-allocation.
    let trigger = auth.alloc.allocate_block().await.expect("harvest trigger");
    for (_ino, idx, off) in &displaced {
        assert!(
            auth.free_listed(*idx) || auth.alloc.grace_holds(*off) || trigger == *off,
            "displaced block {idx} was lost by the storm"
        );
        assert_eq!(
            auth.population(*idx).await,
            0,
            "its durable reference is gone"
        );
    }
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles_before,
        "no double free under two co-writers + a reader"
    );
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed),
        untracked_before,
        "no refused-release residue either"
    );
    assert_eq!(
        free_grace::deferrals(),
        free_grace::releases() + auth.alloc.grace_len() as u64,
        "the grace ledger closes: deferrals ≡ releases + held"
    );

    auth.stop().await;
}

// ===========================================================================
// 8. Rung 10 (residual 2) — co-writer FREED-BLOCK REUSE: the lane free
//    harvest. Rung 9's shipped free returned displaced offsets to "the free
//    supply of lane b % W" — but that supply lives on the AUTHORITY's free
//    list, whose allocation funnel is lane-filtered, so a co-writer's freed
//    blocks were reachable by NOBODY: the co-writer's own allocator never
//    learned of them (frontier-monotone for the mount's lifetime — the
//    rung-9 named residual), and the authority's lane gate refused them.
//    Sustained rewrite therefore leaked toward ENOSPC on a store with free
//    space.
// ===========================================================================

/// **THE residual-2 red** (rung 10 charter, verbatim): a co-writer rewrite
/// loop on a small volume must reach steady state, never `StorageFull`,
/// with `alloc_lane_enospc_refusals == 0`. The loop rewrites ONE block ~3×
/// the co-writer's whole lane share, so it is unreachable on fresh mints
/// alone: it holds only if the freed supply comes back — the HARVEST
/// (`PublishCall::HarvestLaneFree`): at lane exhaustion the allocator ships
/// a lane-scoped harvest to the authority, which hands back free-listed
/// offsets of THAT lane (removing them from its own list — exactly-once),
/// draining its reclaim queue first if the lane's supply is still queued.
///
/// RED against the wave-A tip: no harvest exists — the loop dies
/// `StorageFull` at the lane share with the whole store's freed supply
/// sitting unreachable on the authority's free list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writer_rewrite_loop_reuses_its_lanes_freed_blocks_and_never_hits_storagefull() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "reuse-loop").await;
    let dev = data_device(dir.path(), "reuse-loop.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;

    // A small store: 64 blocks, W = 2 ⇒ the co-writer's lane holds 32.
    const CAP_BLOCKS: u64 = 64;
    cwr.alloc
        .set_capacity_bytes(CAP_BLOCKS * cwr.alloc.chunk_size());
    auth.alloc
        .set_capacity_bytes(CAP_BLOCKS * auth.alloc.chunk_size());
    let lane_share = lane::lane_capacity_blocks(CAP_BLOCKS, 2, 1);
    let iterations = lane_share * 3;

    let enospc_before = METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed);
    let chunk = cwr.alloc.chunk_size();

    // Seed: the co-writer's first block, referenced by a real inode.
    let first_off = cwr
        .alloc
        .allocate_block()
        .await
        .expect("the first mint is a fresh lane-1 block");
    let mut cur_idx = first_off / chunk;
    let ino = authority_file_with_block(&auth, "rewritten-forever.bin", cur_idx).await;

    for i in 0..iterations {
        assert_eq!(
            lane::block_lane_of(cur_idx, 2),
            1,
            "iteration {i}: every offset this mount holds is in its own residue class"
        );
        let new_idx = cwr.rewrite_block(ino, 0, cur_idx).await;
        cwr.br
            .free_block(&(cur_idx * chunk).to_string())
            .await
            .unwrap_or_else(|e| panic!("iteration {i}: the displaced free ships: {e}"));
        cur_idx = new_idx;
    }

    assert_eq!(
        METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed) - enospc_before,
        0,
        "steady state: the lane never starved while the set had free space \
         (the rung-10 residual-2 gate, verbatim)"
    );
    let stats = publish::stats();
    assert!(
        stats.harvest_served_blocks >= iterations - lane_share,
        "the loop ran {iterations} rewrites on a {lane_share}-block lane share, so at least \
         {} offsets came back through the harvest (served {})",
        iterations - lane_share,
        stats.harvest_served_blocks
    );
    assert!(
        stats.harvest_shipped_blocks >= iterations - lane_share,
        "the client-side harvest ledger accounts for the reuse (shipped {})",
        stats.harvest_shipped_blocks
    );

    auth.stop().await;
}

/// Contract: the harvest is **lane-scoped, exactly-once, replay-safe, and
/// its undischarged handouts join the dead-epoch cohort**.
///
/// * a harvest hands out only free-listed offsets of the CALLER's lane and
///   removes them from the authority's free list (nobody can receive one
///   twice);
/// * a REPLAY (same `(lease_epoch, request_id)` — the lost-reply retry) is
///   absorbed by the dedup window and answers the winner's own grant;
/// * a fresh id finds the supply gone (exactly-once);
/// * a handed-out offset whose reference never landed durably is part of
///   the client's death cohort (`DeadCustody.offsets`) — the §3.1 zombie
///   window closed for REUSED offsets the way the durable frontier closes
///   it for fresh mints; a handout DISCHARGED by the offset's next shipped
///   free is not (it is back under the authority's own ladder);
/// * a stale-era harvest refuses (`harvest_refusals`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_harvest_is_exactly_once_lane_scoped_and_quarantines_undischarged_handouts() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "harvest").await;
    let dev = data_device(dir.path(), "harvest.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let chunk = cwr.alloc.chunk_size();
    let tag = volume_tag(DATA_VOL);
    let epoch = cwr.client.lease_epoch();

    // A displaced lane-1 block, freed through the shipped ladder: the
    // authority's free list now carries it, unreachable by lane-0.
    let a_off = cwr.alloc.allocate_block().await.expect("mint A");
    let a_idx = a_off / chunk;
    let ino = authority_file_with_block(&auth, "harvested.bin", a_idx).await;
    let b_idx = cwr.rewrite_block(ino, 0, a_idx).await;
    cwr.br
        .free_block(&a_off.to_string())
        .await
        .expect("A's displaced free ships");
    auth.br.reclaim_drain().await;
    assert!(
        auth.free_listed(a_idx),
        "fixture: A is in the lane-1 free supply"
    );

    // The harvest: lane-scoped, exactly-once, removed from the source list.
    let got = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9001)
        .await
        .expect("the harvest ships");
    assert!(got.contains(&a_idx), "the lane-1 supply came back: {got:?}");
    assert!(got.iter().all(|i| lane::block_lane_of(*i, 2) == 1));
    assert!(
        !auth.free_listed(a_idx),
        "the handout removed A from the authority's free list (exactly-once)"
    );

    // Replay (the lost-reply retry): the SAME witness answers the SAME grant.
    let replays_before = publish::stats().harvest_replays;
    let again = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9001)
        .await
        .expect("the replay is absorbed");
    assert_eq!(
        again, got,
        "the dedup window answered the winner's own grant"
    );
    assert_eq!(publish::stats().harvest_replays - replays_before, 1);

    // A fresh id finds the supply gone.
    let empty = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9002)
        .await
        .expect("the second harvest ships");
    assert!(empty.is_empty(), "exactly-once: {empty:?}");

    // DISCHARGE: the co-writer reuses A, rewrites away again, and ships its
    // free — A is back under the authority's own ladder, so the handout is
    // discharged and A is free-listed again.
    assert_eq!(cwr.alloc.adopt_lane_free_grant(&got), got.len() as u64);
    let reused = cwr.alloc.allocate_block().await.expect("reuse");
    assert_eq!(reused, a_off, "the free-list-first funnel serves A back");
    cwr.rewrite_block(ino, 0, b_idx).await;
    auth.meta
        .commit_block_refs(
            ino,
            &[BlockRefOp::taken(BlockRef {
                vol_tag: tag,
                block_idx: a_idx,
                owner_ino: ino,
                block_index: 1,
            })],
        )
        .await
        .expect("A's reuse reference commits");
    auth.meta
        .commit_block_refs(
            ino,
            &[BlockRefOp::released(BlockRef {
                vol_tag: tag,
                block_idx: a_idx,
                owner_ino: ino,
                block_index: 1,
            })],
        )
        .await
        .expect("A's displacement releases");
    cwr.br
        .free_block(&a_off.to_string())
        .await
        .expect("A's second displaced free ships");
    auth.br.reclaim_drain().await;
    assert!(
        auth.free_listed(a_idx),
        "the full cycle returned A to the supply"
    );

    // Harvest A once more and let the epoch DIE with the handout
    // undischarged: A must be named in the death cohort (the quarantine's
    // input), exactly like a declared in-flight destination.
    let got2 = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9003)
        .await
        .expect("the third harvest ships");
    assert!(got2.contains(&a_idx));
    let dead = auth
        .owner
        .revoke_client(NODE_A, "test: epoch death with an undischarged handout");
    assert_eq!(dead.len(), 1, "one custody died");
    assert!(
        dead[0].offsets.contains(&a_off),
        "the undischarged handout joined the dead-epoch cohort (got {:?})",
        dead[0].offsets
    );

    // A stale-era harvest refuses loud and is counted.
    let refusals_before = publish::stats().harvest_refusals;
    let stale = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9004).await;
    assert!(stale.is_err(), "a dead era's harvest is refused");
    assert_eq!(publish::stats().harvest_refusals - refusals_before, 1);

    auth.stop().await;
}

/// **Rung-10 finding #4** (found live by the s9-fanout row: every
/// co-writer ended the row with `local_commit_refusals == 2` — one per
/// fsync — and the backtrace tracer named `drain_pending_times_now` via
/// `sync_device_for_ino`, the fsync path's M6 times drain): the drain
/// consulted the WRITE GATE before checking whether it had any work, so a
/// structurally-EMPTY drain on a co-writer (nothing ever parks locally —
/// the write path's `park_write_times` SHIPS) counted a false
/// `cowriter_local_commit_refusals` on every fsync, polluting the S8-b
/// falsifier ("zero un-routed local commits") with a no-op.
///
/// The law: **a drain with no work touches no gate** — the falsifier
/// counts only a gate refusal that had a commit behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_times_drain_on_a_co_writer_fsync_is_not_a_local_commit_refusal() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "times-drain").await;
    let dev = data_device(dir.path(), "times-drain.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;

    let refusals_before = METRICS
        .cowriter_local_commit_refusals
        .load(Ordering::Relaxed);
    // The fsync path's exact call (flush_inode_to_backend → sync_device_for_ino
    // → drain_pending_times_now): the co-writer's pending set is empty BY
    // CONSTRUCTION, so this must be a pure barrier, not a refused commit.
    cwr.meta
        .sync_device_for_ino(1)
        .await
        .expect("a co-writer fsync barrier is legal");
    assert_eq!(
        METRICS
            .cowriter_local_commit_refusals
            .load(Ordering::Relaxed),
        refusals_before,
        "an EMPTY times drain moved the S8-b falsifier — the gate was probed before the work \
         check (rung-10 finding #4)"
    );

    auth.stop().await;
}

/// **The reclaim-shaped release batch** (the 2026-08-17 C8 fix's wire
/// face — `.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md`): when a
/// co-writer reclaims an ino whose rewrite epoch was still OPEN,
/// `delete_file` ships ONE witnessed `CommitBlockRefs` whose release set
/// is the union of the snapshot map's bindings and the drained deferred
/// notes — which means the frame legitimately carries a `Delete` for a
/// record that was NEVER staged (the shadow key the RAM map named)
/// beside the `Delete` for the record that WAS (the displaced key the
/// durable map still named). Pinned here, over the real wire:
///
/// * the mixed batch applies exactly — the staged record dies, the
///   absent key's `Delete` is a no-op, the ledger reads 0 for both;
/// * the frame is IDEMPOTENT under the finding-#6 witness: a verbatim
///   re-ship answers the winner's cached outcome, stages nothing
///   (journal-entry equality), and is counted as a replay;
/// * release-before-free heals the strand: the subsequent shipped frees
///   of BOTH blocks answer `Freed` — never the `NonTerminal` the field
///   run logged 6 of (a free arriving while the orphaned record still
///   held the ledger's population above zero);
/// * both offsets re-enter the authority's free supply.
///
/// (The era gate's own arm — a swept epoch's `commit_block_refs` refuses
/// with nothing applied — is the standing pin in
/// `mw_publish_era_gate_tests`; this frame changes nothing about it.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reclaim_shaped_release_batch_is_exact_idempotent_and_frees_read_freed() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "reclaim-batch").await;
    let dev = data_device(dir.path(), "reclaim-batch.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    // The durable state at reclaim time, exactly: the ino's STAGED record
    // names the displaced key (old_idx — the authority-written block the
    // epoch displaced); the shadow key (shadow_idx — the co-writer's CoW
    // dest) was never staged, because its take still sat in the deferred
    // accumulator when the reclaim drained it.
    let old_off = auth.alloc.allocate_block().await.expect("lane-0 mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "reclaimed.bin", old_idx).await;
    assert_eq!(auth.population(old_idx).await, 1);

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let shadow_off = cwr.alloc.allocate_block().await.expect("lane-1 mint");
    let shadow_idx = shadow_off / cwr.alloc.chunk_size();
    assert_eq!(
        auth.population(shadow_idx).await,
        0,
        "premise: never staged"
    );

    let tag = volume_tag(DATA_VOL);
    let epoch = cwr.client.lease_epoch();
    let pc = publish::PublishClient::new(NODE_A, SECRET.to_vec());
    let frame = publish::PublishCall::CommitBlockRefs {
        ino,
        refs: vec![
            // The RAM map's binding — the no-op arm.
            publish::WireBlockRefOp {
                vol_tag: tag,
                block_idx: shadow_idx,
                owner_ino: ino,
                block_index: 0,
                take: false,
            },
            // The drained note — the record that must die.
            publish::WireBlockRefOp {
                vol_tag: tag,
                block_idx: old_idx,
                owner_ino: ino,
                block_index: 0,
                take: false,
            },
        ],
        lease_epoch: epoch,
        request_id: 0xC8,
    };
    let first = pc
        .ship(&auth.endpoint, frame.clone())
        .await
        .expect("the mixed release batch applies");
    assert_eq!(auth.population(old_idx).await, 0, "the staged record died");
    assert_eq!(
        auth.population(shadow_idx).await,
        0,
        "the no-op stayed a no-op"
    );

    // Idempotence: the verbatim re-ship (a lost-reply retry never re-keys).
    let replays_before = publish::stats().replays;
    let journal_before =
        squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let replayed = pc
        .ship(&auth.endpoint, frame)
        .await
        .expect("the duplicate is ANSWERED, not re-applied");
    assert_eq!(replayed, first, "a replay answers the winner's own outcome");
    assert_eq!(publish::stats().replays - replays_before, 1);
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed),
        journal_before,
        "journal-entry equality: the duplicate staged NOTHING"
    );
    assert_eq!(auth.population(old_idx).await, 0);

    // Release-before-free: both frees read Freed — the NonTerminal strand
    // (a free racing its own orphaned record) is structurally gone.
    let verdicts =
        publish::ship_free_blocks(&auth.endpoint, tag, vec![shadow_idx, old_idx], epoch, 0xC9)
            .await
            .expect("the corpse's frees ship");
    assert_eq!(
        verdicts,
        vec![publish::FreeVerdict::Freed, publish::FreeVerdict::Freed],
        "release-before-free must read Freed on BOTH keys — NonTerminal here is the \
         orphaned-record strand the field run logged"
    );
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(old_idx), "the displaced offset returned");
    assert!(auth.free_listed(shadow_idx), "the shadow offset returned");

    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 9. Error-cleanup on a co-writer: leak-safe quiet abandon, never a storm
// ===========================================================================

/// Contract (the 2026-08-19 field conviction, sqz-mw-cw2: after a custody
/// poison/self-fence, EVERY in-flight upload's publish refused and its
/// cleanup hit the allocator's ERROR-logging plane gate — one `block free
/// refused: this mount is a CO-WRITER…` per in-flight block, plus one
/// `cowriter_accounting_refusals` each): the error-cleanup arms (a pipeline
/// upload whose DMA/publish failed, the RES-9 mint guard, the
/// lane-reservation give-back, the mover's destination undo) hold a
/// minted-but-NEVER-PUBLISHED offset no map names, and their ONE sanctioned
/// exit is [`BlockAllocator::abandon_unpublished_offset`]: on a co-writer it
/// ABANDONS the offset leak-safe to the next derivation (the
/// `data_grant::check_free` recovery statement — "stays durably unreferenced
/// and the next derivation (mount recovery / fsck C6) returns them to the
/// free supply" — i.e. the `free_ship_failures` pattern), counts
/// `cowriter_unpublished_abandons`, and moves neither the
/// `cowriter_accounting_refusals` bug tripwire nor the local free list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unpublished_offset_cleanup_on_a_co_writer_abandons_quietly() {
    let _serial = serial();
    let _restore = restore();

    // Mint as the WRITER (the offset's custody existed; its PUBLISH is what
    // failed), then latch the co-writer posture for the cleanup — the
    // post-fence shape, where the in-flight upload's publish verb refused.
    fuse_client::set_mount_posture(MountPosture::Writer);
    let alloc = allocator("vol-00000000000000f3").await;
    let off = alloc.allocate_block().await.expect("mint as the writer");
    fuse_client::set_mount_posture(MountPosture::CoWriter);

    let abandons_before = METRICS
        .cowriter_unpublished_abandons
        .load(Ordering::Relaxed);
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);
    alloc
        .abandon_unpublished_offset(off)
        .await
        .expect("the abandon is QUIET — a cleanup arm has no error to surface");
    assert_eq!(
        METRICS
            .cowriter_unpublished_abandons
            .load(Ordering::Relaxed)
            - abandons_before,
        1,
        "one abandon, counted (expected nonzero ONLY around custody loss)"
    );
    assert_eq!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed) - refusals_before,
        0,
        "the must-stay-≈0 accounting tripwire does not move for sanctioned cleanup"
    );
    assert!(
        !alloc
            .free_block_indices()
            .contains(&(off / alloc.chunk_size())),
        "ABANDONED, not freed: a co-writer's local free list is a private opinion \
         about shared hardware — the offset stays durably unreferenced until the \
         next derivation (mount recovery / fsck C6) returns it to the free supply"
    );
}

/// Contract: on the AUTHORITY / solo-writer posture the same helper IS
/// [`BlockAllocator::free_block`] verbatim — begin + finish with nothing
/// between, the offset back on the free list, zero abandon-gauge movement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_authority_posture_keeps_the_verbatim_free() {
    let _serial = serial();
    let _restore = restore();
    fuse_client::set_mount_posture(MountPosture::Writer);
    let alloc = allocator("vol-00000000000000f4").await;
    let off = alloc.allocate_block().await.expect("mint");

    let abandons_before = METRICS
        .cowriter_unpublished_abandons
        .load(Ordering::Relaxed);
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);
    alloc
        .abandon_unpublished_offset(off)
        .await
        .expect("the authority arm is free_block verbatim");
    assert!(
        alloc
            .free_block_indices()
            .contains(&(off / alloc.chunk_size())),
        "the offset returned to the free list exactly as free_block leaves it"
    );
    assert_eq!(
        METRICS
            .cowriter_unpublished_abandons
            .load(Ordering::Relaxed)
            - abandons_before,
        0,
        "the abandon gauge is a CO-WRITER instrument: 0 on every other posture"
    );
    assert_eq!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed) - refusals_before,
        0,
        "no refusal on the authority arm either"
    );
}

// ===========================================================================
// 10. The staged legs' error cleanup (the d575be03 residual sweep): a
//     co-writer's staged flush/fold/promotion/spill undo abandons quietly
// ===========================================================================

/// In-process FUSE-layer fixture (the `rw5a_never_lossy_tests` /
/// `fsync_writeback_tail_loss_tests` house shape): a real
/// [`SqueezefsFilesystem`] over file-backed data + meta sandboxes with a
/// live staging dir — the venue the staged flush/fold/spill legs run in
/// (co-writers DO stage: writer-scoped staging, incompat bit 10, exists
/// exactly so multiple writers' staged payloads coexist). The allocator
/// handle stays out so a test can engage the granted co-writer lane the
/// S9 posture allocates from.
struct FsH {
    fs: SqueezefsFilesystem,
    req: Request,
    alloc: Arc<BlockAllocator>,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// Drops the write-failure injection and the block-size env pin on every
/// exit (panic hygiene — the file's [`Restore`] restores the posture;
/// this guard owns the seams only the fs-level tests below arm).
struct SeamGuard;

impl Drop for SeamGuard {
    fn drop(&mut self) {
        nvme_dev::clear_fail_next_writes();
        std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    }
}

async fn fs_fixture(uuid: [u8; 16], alloc_ns: &str, block_size: u64, write_cap: &str) -> FsH {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", block_size.to_string());
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = allocator(alloc_ns).await;
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(write_cap),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0x51EE_9A6E_2026_0820,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    FsH {
        fs,
        req,
        alloc: ba,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn fs_create(h: &FsH, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn fs_write(h: &FsH, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

fn pat(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ tag | 1).collect()
}

/// Contract (the d575be03 landing report's UNSWEPT residual, leg class 1
/// — the `fuse_client` staged flush funnel: `upload_active_block_bytes` /
/// `fold_upload_block` / `flush_one_active_block`): a co-writer's staged
/// flush whose DMA or merge fails holds a minted-but-NEVER-PUBLISHED
/// offset no map names, and its cleanup must exit through the ONE
/// sanctioned abandon arm ([`BlockAllocator::abandon_unpublished_offset`]
/// — quiet, counted `cowriter_unpublished_abandons`), never through the
/// allocator-level `free_block` whose plane gate turns a post-fence flush
/// sweep into the convicted ERROR-per-block refusal storm (2026-08-19,
/// sqz-mw-cw2 — the same funnel, one custody layer lower).
///
/// RED against d575be03: the staged flush legs still call
/// `allocator.free_block(offset)` directly — the refusal tripwire moves
/// and the abandon gauge stays 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fenced_co_writers_staged_flush_cleanup_abandons_quietly() {
    let _serial = serial();
    let _restore = restore();
    let _seams = SeamGuard;
    fuse_client::set_mount_posture(MountPosture::Writer);
    // The fsync-tail-loss fixture pins (house pattern): W1 patch OFF (a
    // 64 KiB block would make sub-block segments patch-eligible and
    // bypass the parked-ActiveBlockBuf ladder under test; the co-writer
    // posture also declines W1 upstream, but the SETUP writes run as the
    // writer), overlay fresh-write store OFF (an overlay-stored fresh
    // write parks no custody, so no flush unit would form).
    fuse_client::set_patch_max_bytes(0);
    squeezefs::device_overlay::set_device_overlay_for_tests(false, false);
    const BS: u64 = 65536;
    let h = fs_fixture(*b"mw-abandon-flush", "mw_abandon_flush", BS, "64MB").await;

    // As the WRITER: a striped base (blocks 0-1 published) plus one
    // PARTIAL block-2 write that parks in RAM custody (coverage union
    // incomplete — no write-through, no DMA yet).
    let ino = fs_create(&h, "flushee").await;
    fs_write(&h, ino, 0, &pat(2 * BS as usize, 0x11)).await;
    h.fs.fsync(h.req, ino, 0, false).await.expect("base fsync");
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "fixture pipeline must drain"
    );
    fs_write(&h, ino, 2 * BS, &pat(8192, 0x5C)).await;

    // The post-fence shape: the co-writer posture latches with a granted
    // lane (a laned co-writer allocates — the landed lane-grant seam;
    // reservation raises stay RAM-only without a durable sink, stated by
    // `reserve_lane_frontier`), and the flush's one DMA fails (the
    // injected device completion — classless, so no fence/poison latch).
    h.alloc
        .engage_alloc_lanes(part(2, 1))
        .expect("engage the granted lane");
    fuse_client::set_mount_posture(MountPosture::CoWriter);
    let abandons_before = METRICS
        .cowriter_unpublished_abandons
        .load(Ordering::Relaxed);
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);
    nvme_dev::set_fail_next_writes(1);
    let res = h.fs.fsync(h.req, ino, 0, false).await;
    nvme_dev::clear_fail_next_writes();
    assert!(
        res.is_err(),
        "the injected DMA failure surfaces (never-lossy: the acked bytes keep \
         their staged custody behind the loud fsync error)"
    );
    assert_eq!(
        METRICS
            .cowriter_unpublished_abandons
            .load(Ordering::Relaxed)
            - abandons_before,
        1,
        "the staged flush's never-published cleanup exits through the ONE \
         sanctioned abandon arm (quiet, counted)"
    );
    assert_eq!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed) - refusals_before,
        0,
        "and the must-stay-≈0 accounting tripwire does not move — the \
         2026-08-19 post-fence storm shape, closed for the staged flush legs"
    );
}

/// Contract (leg class 2 — the `DataRouter` staged legs: the promotion
/// commit undo, the rider-fold / staged-clone durable-spill undos, the
/// truncate durable-clip undo, `write_striped`'s stored-image-fits
/// refusal): the same law for the router's allocator-direct error
/// cleanups. Driven through the rider-fold spill (the rw5a arm-B shape:
/// full ring forces the durable-spill escalation, whose DMA then fails
/// injected) — one leg stands in for the class, all of whose members share
/// the identical `allocate → device write → free-on-error` shape against
/// an offset no map ever named.
///
/// RED against d575be03: the spill undo calls
/// `block_allocator.free_block(be_offset)` — refusal counted, no abandon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_co_writers_staged_spill_cleanup_abandons_quietly() {
    let _serial = serial();
    let _restore = restore();
    let _seams = SeamGuard;
    fuse_client::set_mount_posture(MountPosture::Writer);
    const BS: u64 = 1024 * 1024;
    let h = fs_fixture(*b"mw-abandon-spill", "mw_abandon_spill", BS, "4MB").await;

    // The rw5a arm-B shape, as the WRITER: a staged rider file plus a
    // ring oversubscribed by live staged files (each pinned ring-resident
    // by its own rider record — the W2 rider fence defers promotion), so
    // the fold's same-key replace is refused and the durable-spill
    // escalation engages.
    let ino = fs_create(&h, "rider").await;
    let img_len = 700 * 1024;
    fs_write(&h, ino, 0, &pat(img_len, 0xA1)).await;
    fs_write(&h, ino, 128 * 1024, &pat(4096, 0xB2)).await;
    assert!(
        METRICS.staged_rider_extent_writes.load(Ordering::Relaxed) > 0,
        "fixture: the sub-image overwrite must have parked as a rider record"
    );
    for i in 0..8 {
        let filler = fs_create(&h, &format!("spill_filler_{i}")).await;
        fs_write(&h, filler, 0, &pat(img_len, 0xC3)).await;
        fs_write(&h, filler, 4096, &pat(2048, 0xCC)).await;
    }

    // Co-writer latch + granted lane; the spill's one DMA fails injected.
    h.alloc
        .engage_alloc_lanes(part(2, 1))
        .expect("engage the granted lane");
    fuse_client::set_mount_posture(MountPosture::CoWriter);
    let abandons_before = METRICS
        .cowriter_unpublished_abandons
        .load(Ordering::Relaxed);
    let refusals_before = METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed);
    let spills_before = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    nvme_dev::set_fail_next_writes(1);
    let res = h.fs.fold_extent_block(ino, 0).await;
    nvme_dev::clear_fail_next_writes();
    assert!(
        res.is_err(),
        "the injected spill DMA failure surfaces loud (the fold's own error \
         contract; StorageFull is what must never surface, not EIO)"
    );
    assert!(
        METRICS.staged_spill_escalations.load(Ordering::Relaxed) > spills_before,
        "engagement: the fold under a full ring must take the counted \
         durable-spill leg — otherwise this row tested nothing"
    );
    assert_eq!(
        METRICS
            .cowriter_unpublished_abandons
            .load(Ordering::Relaxed)
            - abandons_before,
        1,
        "the spill's never-published cleanup exits through the ONE sanctioned \
         abandon arm (quiet, counted)"
    );
    assert_eq!(
        METRICS.cowriter_accounting_refusals.load(Ordering::Relaxed) - refusals_before,
        0,
        "and the must-stay-≈0 accounting tripwire does not move — the router's \
         staged legs no longer storm the plane gate"
    );
}
