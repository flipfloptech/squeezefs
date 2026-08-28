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
use squeezefs::routing::{
    BackendRouter, CachedMetadata, DataRouter, LAYOUT_DELTA_CHAIN_INELIGIBLE,
};
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
    // Production truth (finding 24's instability arm): an authority-
    // written striped block's incarnation word is STABLE by the time any
    // displaced free can name it — the claim tail marked it unstable and
    // the DMA-complete publish stabilized it. The fixtures skip the DMA,
    // so the stabilization is explicit here.
    auth.alloc.publish_block(idx * auth.alloc.chunk_size());
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

/// **Finding 19/20 — a `Refused` verdict retires NOTHING locally**
/// (`.benchmarks/2026-08-25-s11-freeloop-stall.md` §PR 5 attempt 1): the
/// client's success arm retired local tracking VERDICT-BLIND — every
/// entry of a served ship got its read tiers purged, its refcount entry
/// removed, and its incarnation word marked unstable, including entries
/// the authority answered `Refused` (the double-release lineage; 6,321
/// fleet-wide on the attempt-1 row). By the time the refused second ship
/// lands, the offset can already be LIVE CUSTODY AGAIN — the authority
/// freed it on the first ship and any next owner (including this very
/// mount, via lane harvest) may hold it — so the blind retire moves a
/// live block's incarnation word out from under its owner: m56's 48
/// `read_settle_lost_serialized` tripwires ("incarnation moved under
/// BLOCK_FLUSH_LOCKS + INODE_META_LOCKS"), the 4-attempt settle EIO, the
/// latched writeback error, the ior abort.
///
/// The law: local retirement is a PER-VERDICT act — `Freed` retires
/// (the offset is dead), `Refused` touches nothing (the authority
/// refused the accounting act; the offset's local state belongs to its
/// CURRENT owner).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_free_verdict_retires_nothing_locally() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "refretire").await;
    let dev = data_device(dir.path(), "refretire.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "refused.bin", old_idx).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let _new = cwr.rewrite_block(ino, 0, old_idx).await;

    // Ship #1 through the PRODUCT path: Freed, and the sanctioned local
    // retire runs.
    cwr.br
        .free_block(&old_off.to_string())
        .await
        .expect("the displaced free ships");
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(old_idx), "freed on the authority");

    // The offset becomes someone's LIVE custody again — the test stands
    // in for the next owner on this mount (a lane-harvest re-mint):
    // stable word, tracked reference.
    cwr.alloc.publish_block(old_off);
    assert!(
        cwr.alloc.seed_shipped_free_reference(old_off),
        "the next owner's tracking seeds"
    );
    let word = cwr
        .alloc
        .fill_incarnation(old_off)
        .expect("the live owner's word is stable");

    // Ship #2 — the double-release lineage's client face (the epoch/spiral
    // shape: one offset re-enters the free pipeline). The authority
    // REFUSES it; the call itself serves (per-block verdicts).
    cwr.br
        .free_block(&old_off.to_string())
        .await
        .expect("the verb serves — the refusal is the VERDICT, not an error");

    // THE CONTRACT (pre-fix RED): the refused entry moved NOTHING locally.
    assert!(
        cwr.alloc.fill_incarnation_still(old_off, word),
        "finding 20's mechanism, distilled: the verdict-blind retire moved a LIVE owner's \
         incarnation word on a Refused verdict — the mid-settle 'moved under both locks' \
         tripwire (48 counts on the attempt-1 row's m56)"
    );
    assert_eq!(
        cwr.alloc.refcount(old_off),
        Some(1),
        "the live owner's reference tracking survives a refused ship"
    );

    drop(cwr);
    auth.stop().await;
}

/// **Finding 19's `NonTerminal` face — a still-referenced block's word is
/// never destabilized.** A `NonTerminal` verdict means the durable ledger
/// still holds references (a clone sibling — possibly on THIS very mount —
/// keeps the block alive), so the client may release its own tracking of
/// the displaced reference but must never mark the incarnation unstable:
/// an unstable-without-republish word poisons every later fill of the
/// still-live block and is the same mid-settle hazard as the Refused arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nonterminal_free_verdict_never_destabilizes_the_word() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "nonterm").await;
    let dev = data_device(dir.path(), "nonterm.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;

    // A co-writer-minted block with TWO durable references (the clone
    // shape: two inos share it) — the ship of one displaced reference
    // answers NonTerminal.
    let off = cwr
        .alloc
        .allocate_block()
        .await
        .expect("a laned co-writer mints");
    // The mint's word stabilizes at the durable write's publish; the
    // fixture stands in for the completed DMA.
    cwr.alloc.publish_block(off);
    let idx = off / cwr.alloc.chunk_size();
    let tag = volume_tag(DATA_VOL);
    publish::commit_block_refs(
        &cwr.meta,
        90,
        &[BlockRefOp::taken(BlockRef {
            vol_tag: tag,
            block_idx: idx,
            owner_ino: 90,
            block_index: 0,
        })],
    )
    .await
    .expect("ino 90's reference commits");
    publish::commit_block_refs(
        &cwr.meta,
        91,
        &[BlockRefOp::taken(BlockRef {
            vol_tag: tag,
            block_idx: idx,
            owner_ino: 91,
            block_index: 0,
        })],
    )
    .await
    .expect("ino 91's reference commits");
    // Ino 90 displaces its reference (the rewrite's durable delete).
    publish::commit_block_refs(
        &cwr.meta,
        90,
        &[BlockRefOp::released(BlockRef {
            vol_tag: tag,
            block_idx: idx,
            owner_ino: 90,
            block_index: 0,
        })],
    )
    .await
    .expect("ino 90's release commits");
    assert_eq!(auth.population(idx).await, 1, "the sibling keeps it alive");

    let word = cwr
        .alloc
        .fill_incarnation(off)
        .expect("the minted block's word is stable");

    // The displaced free ships; the authority answers NonTerminal (durable
    // population > 0, no RAM tracking of a co-writer mint).
    cwr.br
        .free_block(&off.to_string())
        .await
        .expect("the displaced free ships");

    // THE CONTRACT (pre-fix RED): the sibling's block stays servable —
    // the word never moved.
    assert!(
        cwr.alloc.fill_incarnation_still(off, word),
        "a NonTerminal verdict destabilized a still-referenced block's incarnation word — \
         every later fill of the live sibling's block now refuses to publish, and a \
         mid-settle owner trips the finding-20 invariant"
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
    let (got, _hint) = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9001)
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
    let (again, _hint) =
        publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9001)
            .await
            .expect("the replay is absorbed");
    assert_eq!(
        again, got,
        "the dedup window answered the winner's own grant"
    );
    assert_eq!(publish::stats().harvest_replays - replays_before, 1);

    // A fresh id finds the supply gone.
    let (empty, _hint) =
        publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9002)
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
    let (got2, _hint) = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9003)
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

// ===========================================================================
// Finding 15 (the s11-mpiio acceptance row's blocker,
// `.benchmarks/2026-08-25-s11-freeloop-stall.md`): the LANE HARVEST is a
// remote allocation funnel — it must reach the grace ring
// ===========================================================================

/// One grace-armed authority + one reader member + one co-writer whose
/// rewrite displaced `old` into the grace ring — the finding-15 stage,
/// shared by both contracts below. Returns everything a contract needs to
/// drive the reader's acknowledgement and the owner clock.
struct GraceStage {
    auth: Authority,
    m_owner: Arc<MembershipOwner>,
    ticks: Arc<AtomicU64>,
    reader_epoch: u64,
    /// The co-writer's custody lease epoch (still live server-side after
    /// the CoWriter struct drops — revocation is explicit, never a Drop).
    lease_epoch: u64,
    old_off: u64,
    old_idx: u64,
}

async fn grace_stage(dir: &Path, tag: &str) -> GraceStage {
    let vol = fresh_volume(dir, tag).await;
    let dev = data_device(dir, &format!("{tag}.dev"));
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, &format!("{tag}.bin"), old_idx).await;

    // The membership owner + one reader that has acknowledged nothing,
    // with EXPLICIT bounds on a manual clock (the deterministic seam):
    // routine fence 60 s, pressure bound 5 s.
    let ticks = Arc::new(AtomicU64::new(10_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    let m_owner = MembershipOwner::arm(
        "f15-owner",
        3,
        2,
        LeaseClocks::derive(Duration::from_micros(250)).expect("shipped derivation"),
        clock.clone(),
    )
    .expect("the membership owner arms");
    membership::install_owner(Arc::clone(&m_owner));
    free_grace::arm_owner_plane_with(
        clock,
        Duration::from_millis(60_000),
        Duration::from_millis(5_000),
    );
    let reader = match m_owner.join(JoinRequest {
        id: "f15-reader".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-f15".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(g) => g,
        JoinOutcome::Refused { reason, .. } => panic!("join refused: {reason}"),
        JoinOutcome::UnknownLease { reason } => panic!("join answered UnknownLease: {reason}"),
    };
    m_owner.refresh_free_grace_bound();
    assert!(free_grace::armed(), "the grace plane is armed");

    // The co-writer displaces `old` and ships the free: the ring holds it.
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let _new = cwr.rewrite_block(ino, 0, old_idx).await;
    cwr.br
        .free_block(&old_off.to_string())
        .await
        .expect("the displaced free ships");
    auth.br.reclaim_drain().await;
    assert!(
        auth.alloc.grace_holds(old_off),
        "the fixture's displaced offset is IN the grace ring"
    );
    assert!(!auth.free_listed(old_idx), "and not on the free list");
    let lease_epoch = cwr.client.lease_epoch();
    drop(cwr);

    GraceStage {
        auth,
        m_owner,
        ticks,
        reader_epoch: reader.epoch,
        lease_epoch,
        old_off,
        old_idx,
    }
}

/// **A lane harvest must reach ACKNOWLEDGED offsets in the grace ring.**
/// The captured stall: on a grace-armed fleet every displaced offset
/// enters the ring, and the ring is harvested ONLY from the authority's
/// own allocation/free contexts — which stop running exactly when the
/// fleet's writers are the ones starving. `execute_lane_harvest` (the
/// co-writers' ONLY refill; polled 860× against an empty free list in
/// the capture) scanned the free list and drained the reclaim queue but
/// never ran the grace funnel, so a fully-acknowledged, releasable
/// offset was unreachable forever and the co-writer ENOSPC'd on a
/// healthy volume.
///
/// The law: the remote funnel harvests the ring exactly as the local
/// allocation funnel does (`try_allocate_block`'s head), so
/// "reallocatable" means the same thing on both funnels.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_harvest_reaches_acknowledged_offsets_in_the_grace_ring() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let st = grace_stage(dir.path(), "f15-ack").await;

    // The reader acknowledges the held label; the bound now covers it.
    let label = st
        .auth
        .alloc
        .grace_oldest_label()
        .expect("the held entry carries a label");
    assert!(
        matches!(
            st.m_owner.renew("f15-reader", st.reader_epoch, label),
            RenewOutcome::Renewed(_)
        ),
        "the acknowledgement rides the reader's renewal"
    );
    st.m_owner.refresh_free_grace_bound();

    let handed = cowriter::execute_lane_harvest(
        &st.auth.br,
        volume_tag(DATA_VOL),
        0, // writers=1 ⇒ every block is lane 0: the funnel is under test,
        1, // not the residue arithmetic (the lane suites own that)
        8,
        7,
    )
    .await
    .expect("the lane harvest executes");
    assert_eq!(
        handed,
        vec![st.old_idx],
        "an ACKNOWLEDGED grace-held offset is part of the lane's supply — the remote funnel \
         must harvest the ring exactly as the local allocation funnel does (finding 15: the \
         capture's co-writers polled an empty free list forever while the ring held their \
         releasable supply)"
    );
    assert!(
        !st.auth.alloc.grace_holds(st.old_off),
        "the ring entry retired with the harvest"
    );
}

/// **An empty lane harvest IS allocation pressure, and walks the valve's
/// ladder.** The capture's second face: `free_grace_pressure_pct` read 0
/// for the whole run while eight writers ENOSPC'd, because the remote
/// funnel never told the valve anything — the pressure signal only ever
/// fired from the authority's OWN allocation cliff, which was never the
/// starving one.
///
/// The law, in the pressure ruling's own words: a co-writer's lane
/// harvest finding nothing is `StorageFull`-imminent on that writer, so
/// it evaluates the PRESSURE deadline — never a broken promise (an
/// unacknowledged offset pre-deadline stays held and the reading goes to
/// 100), and past the deadline the laggard is FENCED, not waited on
/// (forced release + eviction), with the offset handed to the lane.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_lane_harvest_is_allocation_pressure_and_walks_the_valve_ladder() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let st = grace_stage(dir.path(), "f15-press").await;
    let tag = volume_tag(DATA_VOL);

    // PRE-DEADLINE (2 s past the label; pressure bound 5 s), reader
    // holding: the harvest answers EMPTY — never a broken promise — but
    // the valve must now KNOW (the reading goes to the cliff and the
    // tightening is counted).
    st.ticks.store(12_000, Ordering::SeqCst);
    let tightenings_before = free_grace::bound_tightenings();
    let handed = cowriter::execute_lane_harvest(&st.auth.br, tag, 0, 1, 8, 7)
        .await
        .expect("the pre-deadline harvest executes");
    assert!(
        handed.is_empty(),
        "an unacknowledged offset before the pressure deadline is NEVER released (the \
         pressure ruling), got {handed:?}"
    );
    assert!(
        st.auth.alloc.grace_holds(st.old_off),
        "the promise held: the offset is still in the ring"
    );
    assert_eq!(
        free_grace::pressure_pct(),
        100,
        "an empty lane harvest is a writer at its allocation cliff — the valve's reading \
         must say so (the capture ran a whole ENOSPC storm at pressure_pct 0)"
    );
    assert!(
        free_grace::bound_tightenings() > tightenings_before,
        "the empty harvest evaluated the tightened deadline (rung b engaged)"
    );

    // PAST THE PRESSURE DEADLINE: the laggard is fenced, not waited on —
    // forced release WITH the eviction, and the offset reaches the lane.
    st.ticks.store(16_000, Ordering::SeqCst);
    let forced_before = free_grace::forced_releases();
    let fences_before = free_grace::laggard_fences();
    let handed = cowriter::execute_lane_harvest(&st.auth.br, tag, 0, 1, 8, 7)
        .await
        .expect("the post-deadline harvest executes");
    assert_eq!(
        handed,
        vec![st.old_idx],
        "past the pressure deadline the lane harvest forces progress and hands the offset out"
    );
    assert!(
        free_grace::forced_releases() > forced_before,
        "the release was FORCED (counted)"
    );
    assert!(
        free_grace::laggard_fences() > fences_before,
        "and the responsible laggard was fenced WITH it — never a silently broken promise"
    );
}

// ===========================================================================
// The free-grace sustain campaign, PR 1 (docs/design-free-grace-sustain.md
// §5.4, KD-FG-10): the lane-reachable counting-set + the allocation split
// ===========================================================================

/// **The lane-reachable count never drifts from the recount** (KD-FG-10's
/// drift tripwire). The free list's mutation census is TEN sites and two
/// of the missed ones run continuously in production (the trim walk, the
/// VL7 mover picks), so the count is maintained INSIDE the set's own
/// insert/remove (the counting-set wrapper — the `pending_block_refs`
/// correct-by-construction precedent) and this contract asserts
/// counter ≡ recount after every stage of a sweep that exercises the
/// census: frees, allocation claims, trim claim+return, both mover picks,
/// the lane-grant take, the harvest adoption, and a lane adoption's
/// repartition recount.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lane_reachable_count_never_drifts_from_the_recount() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let dev = data_device(dir.path(), "drift.dev");
    let (alloc, br) = data_plane(&dev).await;

    // The recount half: the C6 walk's own arithmetic — every free-listed
    // index whose lane this mount owns.
    let recount = |owned: u64, writers: u16| -> u64 {
        alloc
            .free_block_indices()
            .into_iter()
            .filter(|idx| {
                writers == 0
                    || owned & (1u64 << squeezefs::data_alloc_lane::block_lane_of(*idx, writers))
                        != 0
            })
            .count() as u64
    };
    let assert_no_drift = |owned: u64, writers: u16, stage: &str| {
        assert_eq!(
            alloc.lane_owned_free_blocks(),
            recount(owned, writers),
            "the counting-set's lane-owned count drifted from the C6-style \
             recount after {stage} (KD-FG-10's tripwire)"
        );
    };

    // Unpartitioned: the count IS the free-list population.
    let mut offs = Vec::new();
    for _ in 0..8 {
        offs.push(alloc.allocate_block().await.expect("mint"));
    }
    for o in &offs {
        br.free_block(&o.to_string()).await.expect("free");
    }
    br.reclaim_drain().await;
    assert_no_drift(u64::MAX, 0, "eight terminal frees (unpartitioned)");
    assert_eq!(alloc.lane_owned_free_blocks(), 8);

    // The allocation claim (try_allocate_block's free-list exit).
    let claimed = alloc.allocate_block().await.expect("reclaim");
    assert_no_drift(u64::MAX, 0, "a free-list claim");

    // The trim walk: claim + return (two census sites).
    let victim = offs.iter().find(|o| **o != claimed).copied().unwrap();
    let guard = alloc
        .claim_free_for_trim(victim)
        .expect("the trim claim wins");
    assert_no_drift(u64::MAX, 0, "a trim claim");
    alloc.return_from_trim(victim);
    drop(guard);
    assert_no_drift(u64::MAX, 0, "the trim return");

    // The VL7 mover picks (two more sites).
    let below = alloc
        .allocate_block_below(u64::MAX)
        .expect("the contiguity pick claims");
    assert_no_drift(u64::MAX, 0, "the contiguity pick");
    let above = alloc
        .allocate_block_at_or_above(0)
        .expect("the ascending pick claims");
    assert_no_drift(u64::MAX, 0, "the ascending pick");
    let _ = (below, above);

    // Engage a 2-writer partition as lane 0: the repartition recounts, and
    // from here the count is the LANE-owned population only.
    let part = squeezefs::meta_backend::kv::journal::AppendPartition::new(2, 0)
        .expect("a 2-writer partition");
    alloc
        .engage_alloc_lanes(part)
        .expect("the partition engages");
    assert_no_drift(1, 2, "the partition engagement recount");

    // The lane-grant take (the authority serving a peer's harvest) removes
    // a LANE-1 block — foreign to us, so the count must NOT move.
    let lane1_idx = alloc
        .free_block_indices()
        .into_iter()
        .find(|idx| squeezefs::data_alloc_lane::block_lane_of(*idx, 2) == 1);
    if let Some(idx) = lane1_idx {
        let before = alloc.lane_owned_free_blocks();
        assert!(alloc.take_free_for_lane_grant(idx), "the grant takes");
        assert_eq!(
            alloc.lane_owned_free_blocks(),
            before,
            "a foreign-lane take moves nothing (one modulo per mutation)"
        );
        assert_no_drift(1, 2, "the lane-grant take");
        // The harvest ADOPTION on the receiving side is lane-checked, so
        // adopting our own lane-0 block back is refused — exercise the
        // adopt census site with a lane-0 candidate instead.
        let lane0_idx = alloc
            .free_block_indices()
            .into_iter()
            .find(|idx| squeezefs::data_alloc_lane::block_lane_of(*idx, 2) == 0);
        if let Some(own) = lane0_idx {
            assert!(alloc.take_free_for_lane_grant(own), "take our own");
            assert_no_drift(1, 2, "an own-lane take");
            assert_eq!(
                alloc.adopt_lane_free_grant(&[own]),
                1,
                "the harvest adoption re-inserts"
            );
            assert_no_drift(1, 2, "the harvest adoption");
        }
    }

    // Lane adoption (the owned-mask change): the recount follows the mask.
    let proof = squeezefs::data_custody::declare_dead_epoch("drift-contract adoption");
    assert!(
        alloc.adopt_lane(1, proof),
        "lane 1 adopts under a proof of death"
    );
    assert_no_drift(0b11, 2, "the lane adoption recount");
}

/// **The allocation-source split** (§8: `alloc_from_freelist` /
/// `alloc_fresh_mints` — "the attribution split PR 1 exists for"): every
/// allocation exits `try_allocate_block` through exactly one of the two
/// counted arms, so the capture can decompose which stream is
/// recycle-bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_allocation_source_split_accounts_every_allocation() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let dev = data_device(dir.path(), "split.dev");
    let (alloc, br) = data_plane(&dev).await;
    let m = &squeezefs::fuse_client::METRICS;

    let fresh0 = m.alloc_fresh_mints.load(Ordering::Relaxed);
    let list0 = m.alloc_from_freelist.load(Ordering::Relaxed);

    let a = alloc.allocate_block().await.expect("fresh mint");
    let b = alloc.allocate_block().await.expect("fresh mint");
    assert_eq!(
        m.alloc_fresh_mints.load(Ordering::Relaxed) - fresh0,
        2,
        "two virgin-tail mints ride the fresh arm"
    );
    assert_eq!(m.alloc_from_freelist.load(Ordering::Relaxed), list0);

    br.free_block(&a.to_string()).await.expect("free");
    br.reclaim_drain().await;
    let c = alloc.allocate_block().await.expect("freelist reuse");
    assert_eq!(a, c, "the free list serves the offset back");
    assert_eq!(
        m.alloc_from_freelist.load(Ordering::Relaxed) - list0,
        1,
        "the reuse rides the freelist arm"
    );
    assert_eq!(
        m.alloc_fresh_mints.load(Ordering::Relaxed) - fresh0,
        2,
        "and never the fresh one"
    );
    let _ = b;
}

/// **KD-FG-10's supply re-base + its restore-exactly lever** (PR 3): under
/// `SQUEEZEFS_FREE_GRACE_DEMAND` the grace runway's supply input is the
/// LANE-REACHABLE number (the quantity that troughs on a recycle-bound
/// stream); `DEMAND=0` restores the passed-global pre-campaign input
/// verbatim — whose free-list half accumulates foreign-lane releases
/// nobody here can consume (the original finding-15 global-vs-lane skew,
/// both halves now closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_demand_lever_rebases_the_grace_supply_on_the_lane_reachable_number() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let dev = data_device(dir.path(), "rebase.dev");
    let (alloc, _br) = data_plane(&dev).await;
    // A BOUNDED device (the fixture's allocator is otherwise unbounded and
    // both supply quantities read u64::MAX — space not a constraint).
    alloc.set_capacity_bytes(DEV_LEN);

    // Lane 0 of 2; plant a FOREIGN-lane free-list entry (the trim-return
    // census site inserts without a claim — the accumulation shape).
    let part = squeezefs::meta_backend::kv::journal::AppendPartition::new(2, 0)
        .expect("a 2-writer partition");
    alloc.engage_alloc_lanes(part).expect("engages");
    let chunk = alloc.chunk_size();
    let foreign_idx = (0..64u64)
        .find(|i| squeezefs::data_alloc_lane::block_lane_of(*i, 2) == 1)
        .expect("a lane-1 index exists");
    alloc.return_from_trim(foreign_idx * chunk);
    assert_eq!(
        alloc.lane_owned_free_blocks(),
        0,
        "the foreign entry is not lane-owned"
    );

    let global = alloc.free_supply_blocks();
    let lane = alloc.lane_reachable_blocks();
    assert_eq!(
        global,
        lane + 1,
        "the fixture separates the two quantities by exactly the foreign entry"
    );

    squeezefs::free_grace::test_set_demand(Some(true));
    assert_eq!(
        alloc.grace_supply_blocks(),
        lane,
        "under the DEMAND lever the runway reads the lane-reachable supply"
    );
    squeezefs::free_grace::test_set_demand(Some(false));
    assert_eq!(
        alloc.grace_supply_blocks(),
        global,
        "DEMAND=0 restores the passed-global input verbatim"
    );
    assert!(squeezefs::free_grace::test_clear_demand());
}

// ===========================================================================
// PR 4 — ahead-of-stall lane refill (L5 + the OQ 2 measured horizon;
// design-free-grace-sustain §5.5)
// ===========================================================================

/// **The owed ledger is per-allocator and closes end to end** (§5.5): each
/// `Freed` verdict a shipped displaced free brings back increments the
/// SHIPPING allocator's owed word (the authority's list now holds supply
/// this mount is owed); each harvest adoption decrements it. The
/// process-global gauge is the sum; a second volume's allocator never
/// moves (the per-`(vol_tag, lane)` accounting the two-volume venue needs —
/// a global number cannot say WHICH volume's authority holds the supply).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owed_ledger_tracks_freed_verdicts_and_harvest_adoptions() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "owed").await;
    let dev = data_device(dir.path(), "owed.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let chunk = cwr.alloc.chunk_size();
    let m = &squeezefs::fuse_client::METRICS;

    assert_eq!(cwr.alloc.lane_owed_blocks(), 0, "fresh: nothing owed");
    let owed_gauge0 = m.alloc_lane_owed_blocks.load(Ordering::Relaxed);

    // A displaced block whose free ships and comes back `Freed`.
    let a_off = cwr.alloc.allocate_block().await.expect("mint A");
    let a_idx = a_off / chunk;
    let ino = authority_file_with_block(&auth, "owed.bin", a_idx).await;
    cwr.rewrite_block(ino, 0, a_idx).await;
    cwr.br
        .free_block(&a_off.to_string())
        .await
        .expect("the displaced free ships");
    assert_eq!(
        cwr.alloc.lane_owed_blocks(),
        1,
        "one Freed verdict ⇒ one owed block on the SHIPPING allocator"
    );
    assert_eq!(
        m.alloc_lane_owed_blocks.load(Ordering::Relaxed) - owed_gauge0,
        1,
        "the process gauge is the sum of the per-allocator words"
    );

    // A second volume's allocator is untouched — per-allocator by
    // construction (the routing half of §5.5's two-volume law).
    let other = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("vol-00000000000000f9")
            .await
            .expect("a second allocator"),
    );
    assert_eq!(other.lane_owed_blocks(), 0);

    // The harvest adoption pays the debt down.
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(a_idx), "the supply sits on the authority");
    let epoch = cwr.client.lease_epoch();
    let tag = volume_tag(DATA_VOL);
    let (got, _hint) = publish::ship_harvest_lane_free(&auth.endpoint, tag, 1, 2, 16, epoch, 9101)
        .await
        .expect("the harvest ships");
    assert!(got.contains(&a_idx));
    assert_eq!(cwr.alloc.adopt_lane_free_grant(&got), got.len() as u64);
    assert_eq!(
        cwr.alloc.lane_owed_blocks(),
        0,
        "the adoption closes the owed ledger"
    );
    assert_eq!(
        m.alloc_lane_owed_blocks.load(Ordering::Relaxed),
        owed_gauge0,
        "and the sum gauge closes with it"
    );
}

/// **The ahead-harvest decision is rate-gated, watermark-bounded, and
/// routes only to the owing, starving volume** (§5.5): harvest when
/// `reachable < watermark ∧ owed > 0` for THAT allocator, where
/// `watermark = ceil(rate × horizon)` capped at lane-share/4 — derived,
/// never a knob. A quiet writer (rate 0) never harvests ahead; a volume
/// owed nothing is never asked (no wasted RTT); `AHEAD=0` restores the
/// ENOSPC-only shape verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ahead_decision_harvests_only_the_owing_starved_volume() {
    let _serial = serial();
    let _restore = restore();
    squeezefs::block_allocator::test_set_harvest_ahead(Some(true));
    let a = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("vol-00000000000000fa")
            .await
            .expect("allocator A"),
    );
    let chunk = a.chunk_size();
    // 64-block device, 2 writers ⇒ 32-block lane share ⇒ watermark cap 8.
    a.set_capacity_bytes(64 * chunk);
    let part = squeezefs::meta_backend::kv::journal::AppendPartition::new(2, 0)
        .expect("a 2-writer partition");
    a.engage_alloc_lanes(part).expect("engages");

    // Burn most of the lane so the reachable supply is small.
    for _ in 0..30 {
        a.allocate_block().await.expect("mint");
    }
    // The rate EWMA: 30 claims over one second.
    a.sample_alloc_rate(10_000);
    for _ in 0..2 {
        let _ = a.allocate_block().await;
    }
    a.sample_alloc_rate(11_000);
    assert!(
        a.watermark_blocks() > 0,
        "a claiming writer derives a nonzero watermark"
    );
    assert!(
        a.watermark_blocks() <= 8,
        "the watermark caps at lane-share/4 (got {})",
        a.watermark_blocks()
    );

    // Nothing owed yet: never ask (the no-wasted-RTT half).
    assert_eq!(
        a.should_harvest_ahead(),
        None,
        "a volume owed nothing is never harvested"
    );
    a.note_owed_freed(4);
    assert!(
        a.should_harvest_ahead().is_some(),
        "owed > 0 ∧ reachable < watermark ⇒ the refill fires BEFORE the cliff"
    );

    // A quiet writer (rate decays to 0) stands the refill down.
    for t in 1..40u64 {
        a.sample_alloc_rate(11_000 + t * 1_000);
    }
    assert_eq!(
        a.should_harvest_ahead(),
        None,
        "rate-gated: a quiet writer never harvests ahead"
    );

    // The lever restores the ENOSPC-only shape verbatim.
    for _ in 0..2 {
        let _ = a.allocate_block().await;
    }
    a.sample_alloc_rate(60_000);
    squeezefs::block_allocator::test_set_harvest_ahead(Some(false));
    assert_eq!(a.should_harvest_ahead(), None, "AHEAD=0: shipped shape");
    assert!(squeezefs::block_allocator::test_clear_harvest_ahead());
}

/// **The refill horizon is a MEASUREMENT with the derivation as its
/// fallback** (OQ 2, user decision): a harvest reply carrying a nonzero
/// bound-age hint sets `horizon = hint + RTT + one refresh floor` and is
/// counted (`alloc_lane_horizon_hints`); a zero hint (nothing held) and
/// the pre-first-reply state both read the member-local derivation — so a
/// refused or absent reply can never mis-size the watermark, and a lying
/// hint is bounded by the lane-share/4 cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_harvest_horizon_is_a_measurement_with_the_derivation_fallback() {
    let _serial = serial();
    let _restore = restore();
    let a = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("vol-00000000000000fb")
            .await
            .expect("allocator"),
    );
    let m = &squeezefs::fuse_client::METRICS;
    let fallback = a.harvest_horizon_ms();
    assert!(
        fallback > 0,
        "the horizon starts at the member-local derivation"
    );

    let hints0 = m.alloc_lane_horizon_hints.load(Ordering::Relaxed);
    a.note_harvest_hint(23_000, 40);
    assert_eq!(
        a.harvest_horizon_ms(),
        23_040 + a.horizon_floor_ms(),
        "a nonzero hint: horizon = measured bound age + harvest RTT + one \
         refresh floor"
    );
    assert_eq!(
        m.alloc_lane_horizon_hints.load(Ordering::Relaxed) - hints0,
        1,
        "hinted replies are counted"
    );

    a.note_harvest_hint(0, 40);
    assert_eq!(
        a.harvest_horizon_ms(),
        fallback,
        "a zero hint (nothing held) falls back to the derivation — a quiet \
         ring is never over-trusted"
    );
    assert_eq!(
        m.alloc_lane_horizon_hints.load(Ordering::Relaxed) - hints0,
        1,
        "zero hints are not counted as measurements"
    );
}

/// **The harvest reply carries the authority's live bound age** (OQ 2's
/// wire half, `PUBLISH_SCHEMA` 7 → 8): on a grace-armed authority whose
/// ring holds an unacknowledged offset, the reply's hint is the loop
/// latency actually in force — nonzero here, 0 on a drained ring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_harvest_reply_carries_the_authoritys_bound_age() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let st = grace_stage(dir.path(), "hint").await;
    let tag = volume_tag(DATA_VOL);

    // The ring holds an unacknowledged offset: the bound has never
    // advanced, so its age is the owner clock's own reading. The jump
    // stays INSIDE the stage's 5 s pressure deadline relative to the
    // held label (~10.2 s) — the serve's own harvest ladder runs before
    // the reply is built, and past that deadline it would correctly
    // force-release the offset (rung c) and the hint would honestly read
    // a drained ring's 0.
    st.ticks.store(14_000, Ordering::SeqCst);
    let (_blocks, hint) =
        publish::ship_harvest_lane_free(&st.auth.endpoint, tag, 1, 2, 8, st.lease_epoch, 9201)
            .await
            .expect("the harvest ships");
    assert!(
        hint >= 10_000,
        "the reply's hint is the authority's live bound age (got {hint})"
    );
}

// ===========================================================================
// 9. Finding 23 — the indirect-map blob's free has ONE owner
// ===========================================================================
//
// The PR 5 attempt-4 row (`.benchmarks/2026-08-25-s11-freeloop-stall.md`
// finding 23): on a RANGE-SHARED ino the owner's scoped compose
// (`custody_scoped_layout`) recomputes blob custody on every served Put —
// it re-spills to a fresh blob and frees the DURABLE predecessor itself
// (`free_after_commit`), dropping the caller's blob frame ops. A co-writer
// whose cache was invalidated (the release/served-layout hooks) REFETCHES
// that owner-composed head, and its next save's `old_indirect_to_free`
// then claims the OWNER's blob as its own lifecycle — one shipped free per
// co-writer per compose (the live row: 3–9 refusal bursts per offset, 325
// `block_untracked_free_refusals`), and once the offset is REALLOCATED the
// executor's RAM-tracked arm frees the LIVE successor lifetime (192
// `read_settle_lost_serialized` tripwires on block 547, fsync EIO, a
// 112 MiB aggregate-size loss). Two halves under contract:
//
// * the MINT: a range-shared save frees only blobs THIS mount minted —
//   a refetched head's blob is the owner's lifecycle (leak-safe skip);
// * the SHIELD: the shipped-free executor refuses a tracked free whose
//   every RAM reference the durable ledger still justifies (population ≥
//   refcount ⇒ the shipper names a DEAD lifetime of a reallocated offset).

/// A `DataRouter` over one side's existing data plane — the save-funnel
/// drive (`persist_dirty_layout_if_needed` is the pub entry every layout
/// persist funnels through). The caller keeps the `DlmClient` clone: the
/// save's fencing gate compares against ITS reading, so the tests present
/// `dlm.get_fencing_token_ino(ino)` rather than a guessed 0 (the durable
/// term composes into the floor once any set in the process claimed D0).
async fn save_router(
    dlm: &DlmClient,
    alloc: &Arc<BlockAllocator>,
    dev: &Path,
    meta: &Arc<RoutedMetaBackend>,
    stage: &Path,
) -> DataRouter {
    let dlm = dlm.clone();
    let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
    let cache = TieredCache::new(
        vec![stage.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("16MB"),
        Arc::clone(alloc),
        Arc::clone(&nvme),
        None,
    )
    .await
    .expect("tiered cache");
    let router = DataRouter::new(dlm, cache, Arc::clone(alloc), nvme);
    router.set_meta_backend(Arc::clone(meta));
    router
}

/// A dirty cached striped head naming `blob_key` as its indirect map —
/// the shape a co-writer's cache holds after a durable REFETCH (chain
/// ineligible: indirect provenance).
fn refetched_indirect_entry(size: u64, data_key: &str, blob_key: &str) -> CachedMetadata {
    let mut map = std::collections::HashMap::new();
    map.insert(0u32, data_key.to_string());
    CachedMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: Some(std::sync::Arc::from(
            format!("indirect:{blob_key}").as_str(),
        )),
        block_map: Some(std::sync::Arc::new(map)),
        layout_dirty: true,
        layout_delta_chain: LAYOUT_DELTA_CHAIN_INELIGIBLE,
        ..Default::default()
    }
}

/// Contract (finding 23, the MINT): a range-shared co-writer save whose
/// cached indirect head was REFETCHED from the durable base ships **no
/// free** for that blob — its lifecycle (ledger release + device free)
/// belongs to the owner's compose. RED against dev: the save's
/// `old_indirect_to_free` tail ships exactly the duplicate free the live
/// row stormed on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_shared_saves_refetched_blob_free_never_ships() {
    let _serial = serial();
    let _restore = restore();
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f23-mint").await;
    let dev = data_device(dir.path(), "f23-mint.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    // The durable truth: a file whose data block AND map blob the
    // AUTHORITY minted and tracks — exactly what a co-writer's refetch
    // of the owner-composed head observes.
    let data_off = auth.alloc.allocate_block().await.expect("data block");
    let data_idx = data_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "shared.bin", data_idx).await;
    let blob_off = auth.alloc.allocate_block().await.expect("map blob");
    let blob_idx = blob_off / auth.alloc.chunk_size();
    auth.meta
        .commit_block_refs(
            ino,
            &[BlockRefOp::taken(BlockRef {
                vol_tag: volume_tag(DATA_VOL),
                block_idx: blob_idx,
                owner_ino: ino,
                block_index: squeezefs::meta_backend::kv::block_refs::BLOCK_INDEX_MAP_BLOB,
            })],
        )
        .await
        .expect("the durable MAP_BLOB record commits");

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let stage = tempdir().unwrap();
    let dlm = DlmClient::new().expect("dlm");
    let router = save_router(&dlm, &cwr.alloc, &dev, &cwr.meta, stage.path()).await;

    // The ino is RANGE-SHARED on this mount (rung 15's client cache is
    // the co-writer-side discriminator), and the cached head is the
    // REFETCHED durable base.
    squeezefs::meta_ship::tokens::record_range_grant(ino, (0, 4 * 1024 * 1024), 9001);
    router.metadata_cache.insert(
        ino,
        refetched_indirect_entry(
            4 * 1024 * 1024,
            &data_off.to_string(),
            &blob_off.to_string(),
        ),
    );

    let shipped_before = publish::stats().free_shipped_blocks;
    let tok = dlm.get_fencing_token_ino(ino);
    router
        .persist_dirty_layout_if_needed(&format!("inode_{ino}"), tok)
        .await
        .expect("the save lands (the Put ships to the authority)");
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped_before,
        0,
        "the refetched blob's free never ships — the owner's compose owns that \
         lifecycle (finding 23's mint: one shipped free per co-writer per compose)"
    );
    assert_eq!(
        auth.alloc.refcount(blob_off),
        Some(1),
        "the authority still tracks its own blob — nobody freed a lifetime they \
         do not own"
    );

    squeezefs::meta_ship::tokens::test_clear_range_cache();
    drop(cwr);
    auth.stop().await;
}

/// Contract (finding 23's posture scope, pinned green): a SOLO writer's
/// save keeps freeing the refetched blob LOCALLY — on a single-writer
/// mount every durable blob is this mount's own lifecycle, and the gate
/// must not turn the collapse arm into a leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_solo_writers_refetched_blob_free_stays_local() {
    let _serial = serial();
    let _restore = restore();
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f23-solo").await;
    let dev = data_device(dir.path(), "f23-solo.dev");
    fuse_client::set_mount_posture(MountPosture::Writer);
    let meta = squeezefs::meta_backend::open_routed_meta_set(&[vol.display().to_string()])
        .await
        .expect("the solo writer mounts");
    let (alloc, _br) = data_plane(&dev).await;
    let stage = tempdir().unwrap();
    let dlm = DlmClient::new().expect("dlm");
    let router = save_router(&dlm, &alloc, &dev, &meta, stage.path()).await;

    let ino = meta
        .create_with_rdev_size(1, "solo.bin", 0o100644, 0, 0, 0, 0)
        .await
        .expect("create")
        .ino;
    let data_off = alloc.allocate_block().await.expect("data block");
    let blob_off = alloc.allocate_block().await.expect("map blob");
    let blob_idx = blob_off / alloc.chunk_size();
    router.metadata_cache.insert(
        ino,
        refetched_indirect_entry(
            4 * 1024 * 1024,
            &data_off.to_string(),
            &blob_off.to_string(),
        ),
    );

    let shipped_before = publish::stats().free_shipped_blocks;
    let tok = dlm.get_fencing_token_ino(ino);
    router
        .persist_dirty_layout_if_needed(&format!("inode_{ino}"), tok)
        .await
        .expect("the solo save lands");
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped_before,
        0,
        "a solo save ships nothing"
    );
    router.backend_router.reclaim_drain().await;
    assert!(
        alloc.free_block_indices().contains(&blob_idx),
        "the displaced blob re-entered the free supply locally — the solo \
         collapse arm is byte-identical to the shipped shape"
    );
    for v in &meta.volumes {
        v.shutdown().await.expect("clean unmount");
    }
}

/// Contract (finding 23, the SHIELD): a shipped free naming an offset
/// whose every RAM reference the durable ledger still JUSTIFIES is the
/// stale-duplicate lineage — the offset was freed and REALLOCATED, and
/// the verb names the dead lifetime. The executor refuses; the live
/// successor's block, refcount and durable reference are untouched. RED
/// against dev: the RAM-tracked arm frees the live block (the row's 192
/// `read_settle_lost_serialized` tripwires and the fsync EIO wedge).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shipped_free_the_durable_ledger_still_justifies_is_refused() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f23-shield").await;
    let dev = data_device(dir.path(), "f23-shield.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "victim.bin", old_idx).await;

    // A legitimate displaced free: the co-writer's rewrite releases the
    // reference on the publish, then ships the free — Freed, and the
    // offset returns to the supply.
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let _new_idx = cwr.rewrite_block(ino, 0, old_idx).await;
    cwr.br
        .free_block(&old_off.to_string())
        .await
        .expect("the displaced free ships");
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(old_idx), "the first free executed");

    // The offset is REALLOCATED: a new lifetime, tracked in RAM and
    // durably referenced by a new file.
    let reused = auth.alloc.allocate_block().await.expect("reallocate");
    assert_eq!(reused, old_off, "free-list-first hands the offset back");
    let ino2 = authority_file_with_block(&auth, "reborn.bin", old_idx).await;
    assert_eq!(auth.population(old_idx).await, 1);
    assert_eq!(auth.alloc.refcount(old_off), Some(1));

    // The stale duplicate (the cross-mount shape: a DIFFERENT request id,
    // same epoch — the dedup window cannot absorb it).
    let verdicts = publish::ship_free_blocks(
        &auth.endpoint,
        volume_tag(DATA_VOL),
        vec![old_idx],
        cwr.client.lease_epoch(),
        0xF23_0001,
    )
    .await
    .expect("the verb travels");
    assert_eq!(
        verdicts,
        vec![publish::FreeVerdict::Refused],
        "a free the durable ledger still justifies is REFUSED — the shipper \
         names a dead lifetime of a reallocated offset"
    );
    assert_eq!(
        auth.alloc.refcount(old_off),
        Some(1),
        "the live successor's RAM reference is untouched"
    );
    assert_eq!(
        auth.population(old_idx).await,
        1,
        "the live successor's durable reference is untouched (ino {ino2})"
    );
    auth.br.reclaim_drain().await;
    assert!(
        !auth.free_listed(old_idx),
        "the live successor's block never re-entered the free list"
    );

    drop(cwr);
    auth.stop().await;
}

/// Contract (finding 23's leak direction, pinned green): a range-shared
/// co-writer save still frees the blob it MINTED ITSELF — the shipper's
/// own predecessor is the bounded one-blob residue the design assigns to
/// its `old_indirect_to_free` tail, and the provenance gate must not
/// widen into a per-save leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_shared_save_still_frees_its_own_minted_blob() {
    let _serial = serial();
    let _restore = restore();
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f23-own").await;
    let dev = data_device(dir.path(), "f23-own.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    let ino = authority_file_with_block(&auth, "own.bin", 0).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let stage = tempdir().unwrap();
    let dlm = DlmClient::new().expect("dlm");
    let router = save_router(&dlm, &cwr.alloc, &dev, &cwr.meta, stage.path()).await;
    squeezefs::meta_ship::tokens::record_range_grant(ino, (0, 4 * 1024 * 1024), 9002);

    // Save 1: a map past the inline ceiling (64 KiB nodes ⇒ ~16 KiB cap)
    // MINTS this mount's own blob.
    let mut big = std::collections::HashMap::new();
    for i in 0..900u32 {
        big.insert(i, format!("be://data:k{i:05}"));
    }
    let entry = CachedMetadata {
        file_type: "striped".into(),
        size: 4 * 1024 * 1024,
        block_map: Some(std::sync::Arc::new(big)),
        layout_dirty: true,
        layout_delta_chain: LAYOUT_DELTA_CHAIN_INELIGIBLE,
        ..Default::default()
    };
    router.metadata_cache.insert(ino, entry);
    let tok = dlm.get_fencing_token_ino(ino);
    router
        .persist_dirty_layout_if_needed(&format!("inode_{ino}"), tok)
        .await
        .expect("the over-cap save mints and ships");
    let minted = router
        .metadata_cache
        .get(&ino)
        .and_then(|m| m.block_map_id.clone())
        .expect("save 1 republished an indirect head");
    assert!(minted.starts_with("indirect:"), "{minted}");

    // Save 2: the map collapses back inline — the displaced blob is THIS
    // mount's own mint, and its free SHIPS (the leak direction stays
    // closed).
    let mut m2 = router.metadata_cache.get(&ino).expect("cached");
    let mut small = std::collections::HashMap::new();
    small.insert(0u32, "be://data:k00000".to_string());
    m2.block_map = Some(std::sync::Arc::new(small));
    m2.layout_dirty = true;
    router.metadata_cache.insert(ino, m2);

    let shipped_before = publish::stats().free_shipped_blocks;
    let tok = dlm.get_fencing_token_ino(ino);
    router
        .persist_dirty_layout_if_needed(&format!("inode_{ino}"), tok)
        .await
        .expect("the collapse save lands");
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped_before,
        1,
        "the own-minted predecessor's free ships — bounded one-blob residue, \
         never a per-save leak"
    );

    squeezefs::meta_ship::tokens::test_clear_range_cache();
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 10. Finding 24 — the f23 gate's two documented residuals, closed
// ===========================================================================
//
// Attempt 5 (`.benchmarks/2026-08-25-s11-freeloop-stall.md` finding 24):
// f23 cut the duplicate mint 325 → 4 refusals, and one of the four
// landed in the shield's documented narrow window — freed → REALLOCATED →
// written-but-UNPUBLISHED (the ledger cannot justify the live reference
// yet, so `population ≥ refcount` reads 0 ≥ 1 false and the executor
// freed the mid-write block). The wedged holder's settle EIO then
// stopped its freed-offset acks, the grace ring's release lag ballooned
// to 17.5 s at ~675 displaced blocks/s, and the whole fleet ENOSPC'd on
// a 99.96 %-allocated volume (28 fsync StorageFull failures, the same
// 112 MiB shortfall). Two rungs:
//
// * the MINT's discriminator becomes STICKY: `range_span_hull` samples
//   the LIVE grants, and under churn an ino's grants can all momentarily
//   retire — the save in that gap claims the refetched blob again. A
//   range EPISODE is a monotone per-mount fact;
// * the SHIELD gains the instability arm: a tracked offset whose
//   incarnation word is UNSTABLE (claimed / written-unpublished — the
//   claim tail marks it, the publish stabilizes it) is mid-write by its
//   CURRENT owner, and no legitimate displaced free names one (the
//   displaced block's last event was its own publish).

/// Contract (finding 24, the sticky episode): the blob-lifecycle gate
/// holds through a grant-retirement gap — an ino that has EVER been
/// range-shared on this mount keeps foreign-blob frees skipped even at
/// an instant when no grant happens to be live. RED against dev: retire
/// the grant and the hull-sampled gate ships the duplicate again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_episode_outlives_its_grants_for_the_blob_gate() {
    let _serial = serial();
    let _restore = restore();
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f24-episode").await;
    let dev = data_device(dir.path(), "f24-episode.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let data_off = auth.alloc.allocate_block().await.expect("data block");
    let data_idx = data_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "episodic.bin", data_idx).await;
    let blob_off = auth.alloc.allocate_block().await.expect("map blob");
    let blob_idx = blob_off / auth.alloc.chunk_size();
    auth.meta
        .commit_block_refs(
            ino,
            &[BlockRefOp::taken(BlockRef {
                vol_tag: volume_tag(DATA_VOL),
                block_idx: blob_idx,
                owner_ino: ino,
                block_index: squeezefs::meta_backend::kv::block_refs::BLOCK_INDEX_MAP_BLOB,
            })],
        )
        .await
        .expect("the durable MAP_BLOB record commits");

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let stage = tempdir().unwrap();
    let dlm = DlmClient::new().expect("dlm");
    let router = save_router(&dlm, &cwr.alloc, &dev, &cwr.meta, stage.path()).await;

    // The episode: a grant was recorded — and RETIRED before the save
    // (the churn gap the trim/doubling interplay produces at every
    // learned ceiling). The hull is empty at save time.
    squeezefs::meta_ship::tokens::record_range_grant(ino, (0, 4 * 1024 * 1024), 9401);
    squeezefs::meta_ship::tokens::retire_range_grant(ino, 9401);
    assert!(
        squeezefs::meta_ship::tokens::range_span_hull(ino).is_none(),
        "fixture: no LIVE grant remains — the gap under contract"
    );
    router.metadata_cache.insert(
        ino,
        refetched_indirect_entry(
            4 * 1024 * 1024,
            &data_off.to_string(),
            &blob_off.to_string(),
        ),
    );

    let shipped_before = publish::stats().free_shipped_blocks;
    let tok = dlm.get_fencing_token_ino(ino);
    router
        .persist_dirty_layout_if_needed(&format!("inode_{ino}"), tok)
        .await
        .expect("the save lands");
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped_before,
        0,
        "the episode is STICKY: a grant-retirement gap never re-opens the \
         refetched-blob free (attempt 5's 4 residual refusals)"
    );
    assert_eq!(auth.alloc.refcount(blob_off), Some(1));

    squeezefs::meta_ship::tokens::test_clear_range_cache();
    drop(cwr);
    auth.stop().await;
}

/// Contract (finding 24, the shield's instability arm): a shipped free
/// naming a tracked offset whose incarnation word is UNSTABLE — claimed
/// and mid-write, not yet published — is REFUSED: the ledger cannot
/// justify the live reference yet (population 0 < refcount 1), but the
/// instability IS the evidence of a live successor. RED against dev: the
/// executor frees the mid-write block (attempt 5's block-2266 kill — the
/// settle wedge, the ack stall, the fleet ENOSPC).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shipped_free_of_a_mid_write_reallocated_offset_is_refused() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f24-shield").await;
    let dev = data_device(dir.path(), "f24-shield.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    let old_off = auth.alloc.allocate_block().await.expect("mint");
    let old_idx = old_off / auth.alloc.chunk_size();
    let ino = authority_file_with_block(&auth, "victim24.bin", old_idx).await;
    auth.alloc.publish_block(old_off);

    // The legitimate free: rewrite releases the reference, the verb runs
    // the ladder, the offset returns to the supply.
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let _new_idx = cwr.rewrite_block(ino, 0, old_idx).await;
    cwr.br
        .free_block(&old_off.to_string())
        .await
        .expect("the displaced free ships");
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(old_idx), "the first free executed");

    // The REALLOCATION, caught mid-write: the claim tail tracked the
    // offset (refcount 1) and marked its incarnation UNSTABLE; no
    // durable reference exists yet (the publish has not run).
    let reused = auth.alloc.allocate_block().await.expect("reallocate");
    assert_eq!(reused, old_off, "free-list-first hands the offset back");
    assert_eq!(auth.alloc.refcount(old_off), Some(1));
    assert_eq!(
        auth.alloc.fill_incarnation(old_off),
        None,
        "fixture: the claim tail left the word UNSTABLE (mid-write)"
    );
    assert_eq!(
        auth.population(old_idx).await,
        0,
        "unpublished: no ledger ref yet"
    );

    // The stale duplicate lands exactly in the window.
    let verdicts = publish::ship_free_blocks(
        &auth.endpoint,
        volume_tag(DATA_VOL),
        vec![old_idx],
        cwr.client.lease_epoch(),
        0xF24_0001,
    )
    .await
    .expect("the verb travels");
    assert_eq!(
        verdicts,
        vec![publish::FreeVerdict::Refused],
        "a free of a mid-write (unstable-incarnation) tracked offset is \
         REFUSED — the live successor's DMA is in flight"
    );
    assert_eq!(
        auth.alloc.refcount(old_off),
        Some(1),
        "the live successor's RAM reference is untouched"
    );
    auth.br.reclaim_drain().await;
    assert!(
        !auth.free_listed(old_idx),
        "the mid-write block never re-entered the free list"
    );

    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 11. Finding 28 — a stale merge never regresses a block to a dead binding
// ===========================================================================

/// Finding 28 (`.benchmarks/2026-08-25-s11-freeloop-stall.md`, the first
/// cheap-first local probe): the authority's fold published a NEW binding
/// for a block and legally freed the displaced offset (re-minted by
/// another lifetime) — then a co-writer's shipped MERGE whose cached map
/// still named the OLD binding REGRESSED the durable head (per-block
/// last-writer-wins in the compose). Every subsequent fold/read of the
/// block propagated "names a dead incarnation" EIO for a full minute
/// (the head cannot heal: the shipper's next publish is parked behind the
/// failing fsync), and ior's rank called MPI_ABORT. The law: **the
/// arbiter never adopts a caller's block binding whose stamped
/// incarnation is DEAD** — the caller's entry drops, the durable's
/// stands, and the shipper's stale cache heals through the served-layout
/// invalidation it already rides.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_merge_never_regresses_a_block_to_a_dead_binding() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f28-regress").await;
    let dev = data_device(dir.path(), "f28-regress.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;

    // Authority-side mints happen BEFORE the co-writer join (fresh mints
    // raise the lane frontier; a joined process's raises ship under the
    // CO-WRITER's identity and refuse for the authority's lane — the
    // suite's established order. The post-free RE-mint below is
    // free-list-first and needs no raise).
    //
    // The block's FIRST lifetime: minted, published (word live), and the
    // stamped key the co-writer's cache captured.
    let off_old = auth.alloc.allocate_block().await.expect("first mint");
    auth.alloc.publish_block(off_old);
    let gen_old = auth
        .alloc
        .fill_incarnation(off_old)
        .expect("published word is stable");
    let dead_key_body = off_old.to_string();
    let stale_key = squeezefs::routing::block_key_with_incarnation(&dead_key_body, gen_old);

    // The durable head the fold established AFTER displacing it: a NEW
    // binding at a fresh offset.
    let off_new = auth.alloc.allocate_block().await.expect("fold's mint");
    auth.alloc.publish_block(off_new);
    let gen_new = auth
        .alloc
        .fill_incarnation(off_new)
        .expect("published word is stable");
    let live_key = squeezefs::routing::block_key_with_incarnation(&off_new.to_string(), gen_new);

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let epoch = cwr.client.lease_epoch();
    let ino = publish::create_with_rdev_size(&cwr.meta, 1, "regress.bin", 0o100644, 0, 0, 0, 0)
        .await
        .expect("shipped create")
        .ino;
    let head = bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "striped".into(),
        size: 4 * 1024 * 1024,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some([(0u32, live_key.clone())].into_iter().collect()),
    })
    .expect("head bytes");
    squeezefs::meta_ship::publish::set_layout_and_size(&cwr.meta, ino, &head, 4 * 1024 * 1024, &[])
        .await
        .expect("the fold's head lands");

    // The displaced offset's first lifetime DIES (legal post-publish
    // free) and the offset is re-minted by another lifetime.
    cwr.br
        .free_block(&dead_key_body)
        .await
        .expect("the displaced free ships");
    auth.br.reclaim_drain().await;
    let reused = auth.alloc.allocate_block().await.expect("re-mint");
    assert_eq!(reused, off_old, "free-list-first re-mints the offset");
    auth.alloc.publish_block(off_old);
    assert_ne!(
        auth.alloc.fill_incarnation(off_old),
        Some(gen_old),
        "fixture: the stale key's incarnation is DEAD (a new lifetime owns the offset)"
    );

    // The co-writer's STALE merge: its cached map still names the dead
    // binding for block 0 (the probe's field shape).
    let pc = publish::PublishClient::new(NODE_A, SECRET.to_vec());
    let mut d = squeezefs::layout_wire::LayoutDelta::from_final_state(
        "striped",
        4 * 1024 * 1024,
        None,
        None,
        None,
        None,
        vec![(0u32, stale_key.clone())],
    );
    d.set_versions(0, squeezefs::dlm::mint_layout_version());
    let frame = publish::PublishCall::MergeLayoutAndSize {
        ino,
        delta: d.encode(),
        full_layout: head.clone(),
        size: 4 * 1024 * 1024,
        refs: Vec::new(),
        lease_epoch: epoch,
        request_id: 0xF28_0001,
    };
    pc.ship(&auth.endpoint, frame)
        .await
        .expect("the stale merge is SERVED (the drop is per-entry, never a frame refusal)");

    // The law: the durable head still names the LIVE binding.
    use squeezefs::meta_backend::Metadata;
    let raw = auth
        .meta
        .getxattr(ino, "layout")
        .await
        .expect("head read")
        .expect("layout present");
    let after = squeezefs::layout_wire::decode_base_layout(&raw).expect("decodable head");
    let got = after
        .block_map
        .as_ref()
        .and_then(|m| m.get(&0))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        got, live_key,
        "a caller's block binding whose stamped incarnation is DEAD is never \
         adopted — the head regressing to '{stale_key}' is finding 28's \
         EIO-forever wedge (fsync MPI_ABORT on the probe)"
    );

    drop(cwr);
    auth.stop().await;
}
