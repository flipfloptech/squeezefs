//! **The co-writer free path's supply closure** — finding 15's root-cause
//! campaign (`.benchmarks/2026-09-06-cowriter-free-refcount-leak.md`).
//!
//! The s11-mpiio fleet row (1 authority + 8 co-writers, range custody, one
//! shared file rewritten 18×) ends every phase with the co-writers' lanes
//! exhausted while the authority refuses **3,449 shipped frees** on its
//! untracked tripwire (`block_untracked_free_refusals`) — 47 % of every
//! free the co-writers shipped — and the co-writers log the mirror
//! `CLAIM ANOMALY … refcount entry lingers`. The wedge note's hypothesis
//! was a harvested offset whose reference the authority never learned. The
//! in-process diagnosis (this file's red runs against `dev` ac717c7f) found
//! three defects, none of them that one:
//!
//! 1. **the LEAK — a RAM-only lifetime under a recomputed publish.** An
//!    overlay destination fed to the rewrite epoch is a RAM-only binding;
//!    a same-epoch write-through of the same block displaces it before any
//!    save persisted it. The owner recomputes the frame as the
//!    head→composed diff, which cannot name a block NEITHER map ever held;
//!    the caller's frame stands down on `recomputed`; the block is freed
//!    by nobody. Red: `a_same_epoch_rerewrite_loop_…` — 4 of 8 displaced
//!    blocks per round unfreed, `StorageFull` at round 1 on a 9-block lane.
//! 2. **the REFUSAL STORM — an own-mint map blob reclaimed twice (and
//!    again).** The save tail that displaces the co-writer's own map blob
//!    and the layout-entry insert chokepoint's orphan reclaim were two
//!    issuers of one free, and a stale clone of the reclaimed entry
//!    re-inserted after a lock-free release-hook discard re-armed the free
//!    at every later discard: on the fleet ≈ 7 reclaims per own-mint save,
//!    the first accepted, the rest `Refused` (3,448 of 3,449). Red:
//!    `an_own_mint_blob_lineage_closes_exactly_once`.
//! 3. **never-published mints abandoned on a live co-writer** (the
//!    superseded overlay destination) — "left to the next derivation",
//!    which a fleet that never remounts never runs. Red:
//!    `a_never_published_mint_on_a_live_co_writer_recycles_…`.
//!
//! Plus a venue/partial-authority fault the indirect loop convicted: the
//! served compose's displaced-blob free ran outside the authority scope,
//! so a process whose posture latch reads co-writer SHIPPED its own blob's
//! free (refused on the live-free shield — `an_indirect_map_rewrite_loop_…`
//! was red on `block_live_free_refusals`).
//!
//! **Term 3 — the residual `CLAIM ANOMALY` lineage**
//! (`.benchmarks/2026-09-06-cowriter-free-residual-lineage.md`, section 1b
//! below): with the three defects fixed, six of eight co-writers still
//! logged 3–323 anomalies and their lane ENOSPC did not move. The
//! evidence correlated them with `rewrite_shadow_fence_drops` (the three
//! co-writers with one fenced epoch close carried 323/205/323; the two
//! with none carried 0) — an fsync whose token a sibling rank's stripe
//! grant had superseded: the flush leg published the whole dirty map
//! under the CURRENT generation (the authority recomputed the parked
//! predecessors free), then the close presented the STALE token, took the
//! W5 arm, and dropped the parked keys' local hygiene. Red:
//! `a_rotated_fsync_token_converges_the_close_and_orphans_no_local_hygiene`.
//! The refused frees themselves (163, evenly spread, none on the anomaly
//! carriers) are a separate lineage the suite now instruments
//! (`cowriter_free_ship_own_lane_untracked`, section 4).
//!
//! The contracts: the full cycle a co-writer's rewrite runs on the fleet —
//! **harvest → publish (full Put and the Lever-B merge) → rewrite/displace
//! → the displaced free reaches the authority exactly once → the block
//! re-enters the lane's supply and is harvested again** — with the supply
//! CLOSING (`block_untracked_free_refusals` unchanged, reachable lane
//! blocks + live blocks == the starting count, the durable ledger at
//! population 0 for freed blocks), at the router level and through the
//! production FUSE write path under range custody (`fs.write` →
//! `range_write_engaged` → the rewrite epoch → `fsync`'s shipped publish).
//!
//! Venue caveat (stated, not hidden): one process plays both nodes, so the
//! authority's publish service serves over the SAME `KvMetaBackend` the
//! co-writer's fs reads (the `mw_ranged_lease_ladder_tests` shape) while
//! the two data planes (allocators, routers) are distinct — exactly the
//! seam the shipped free crosses.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::alloc_lane_grant::{self as grant, LaneFloor};
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::cluster_wire as cw;
use squeezefs::cowriter;
use squeezefs::data_alloc_lane as lane;
use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::dlm::DlmClient;
use squeezefs::free_grace;
use squeezefs::fuse_client::{self, MountPosture, SqueezefsFilesystem, METRICS};
use squeezefs::membership::{
    self, ClaimSet, ClaimSetMember, LeaseClock, LeaseClocks, MemberIdentity, MemberRole,
};
use squeezefs::meta_backend::kv::backend::WriterClaim;
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BackendRouter, CachedMetadata, DataRouter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, TempDir};

const VOL_LEN: u64 = 64 * 1024 * 1024;
/// Sparse — the seed layouts never write data, so a 640-block indirect
/// fixture costs no bytes.
const DEV_LEN: u64 = 16 * 1024 * 1024 * 1024;
const SECRET: &[u8] = b"s9-cowriter-free-leak-storage-trust-secret";
const AUTHORITY_ID: &str = "authority-membership-owner";
const NODE_A: &str = "node_00000000aaaaaaaa";
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

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        fuse_client::set_mount_posture(MountPosture::Writer);
        lane::test_reset_mount_partition();
        squeezefs::meta_backend::kv::block_refs::uninstall_block_ref_resolver();
        squeezefs::meta_backend::kv::indirect_map::uninstall_indirect_map_io();
        grant::uninstall_frontier_source();
        data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(0, Ordering::Relaxed);
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
        squeezefs::meta_ship::tokens::test_clear_range_cache();
        squeezefs::meta_ship::tokens::test_clear_stretch_ceilings();
        // The AUTHORITY-side sticky episode latch (`dlm::ino_has_range_custody`)
        // is process-global and inos recur across fresh volumes.
        squeezefs::dlm::test_clear_range_episodes();
        squeezefs::device_overlay::clear_device_overlay_for_tests();
        std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Volumes, allocators, evidence (the mw_cowriter_free_tests fixtures)
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

fn data_device(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(DEV_LEN).unwrap();
    p
}

async fn allocator(id: &str) -> Arc<BlockAllocator> {
    Arc::new(BlockAllocator::new(id).await.expect("allocator"))
}

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

fn volume_evidence(path: &Path, node_id: &str) -> cowriter::VolumeAdmissionEvidence {
    cowriter::VolumeAdmissionEvidence {
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

fn full_request(paths: &[PathBuf], node_id: &str) -> cowriter::AdmissionRequest {
    cowriter::AdmissionRequest {
        multi_writer: true,
        role_co_writer: true,
        read_only: false,
        node_id: node_id.to_string(),
        custody_endpoint: Some("127.0.0.1:7100".to_string()),
        volumes: paths.iter().map(|p| volume_evidence(p, node_id)).collect(),
        authority: Some(cowriter::AuthorityLeaseEvidence {
            owner_id: AUTHORITY_ID.to_string(),
            endpoint: "127.0.0.1:7000".to_string(),
            owner_claim_id: String::new(),
            term: 7,
            live: true,
            member_epoch: 3,
        }),
        registrant: Some(cowriter::RegistrantEvidence {
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
// ladder executes the shipped frees, the production arm's resolver +
// geometry) and one co-writer
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
    async fn start(vol: &Path, dev: &Path, members: &[&str], file_size: u64) -> Authority {
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
            cowriter::local_owner_view(),
        ));
        publish::install_harvest_executor(cowriter::router_harvest_executor(Arc::clone(&br)));
        {
            let br = Arc::clone(&br);
            publish::install_binding_probe(Arc::new(move |k: &str| br.block_key_incarnation_ok(k)));
        }
        // The production arm's rung-19 resolver + §9.2 geometry (the
        // custody-scoped compose is structurally inert without them).
        {
            let br = Arc::clone(&br);
            squeezefs::meta_backend::kv::block_refs::install_block_ref_resolver(Arc::new(
                move |k: &str, ino: u64, idx: u32| br.block_ref_for(k, ino, idx),
            ));
        }
        owner.install_range_geometry(data_grant::fixed_range_geometry(
            file_size,
            alloc.chunk_size(),
        ));

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

/// One co-writer: posture latched, ownership armed all-foreign, custody +
/// publish clients installed, its granted lane engaged, its own data
/// plane with the reclaim queue CEASED — `cowriter::arm`'s latch.
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
}

/// The co-writer's production write path: a real [`SqueezefsFilesystem`]
/// over the co-writer's data plane, reading metadata through the SHARED
/// backend (the one-process venue) and shipping every publish to the
/// authority (all-foreign ownership).
struct CoWriterFs {
    fs: SqueezefsFilesystem,
    req: Request,
    _stage: TempDir,
}

async fn cowriter_fs(auth: &Authority, cwr: &CoWriter, dev: &Path) -> CoWriterFs {
    let dlm = DlmClient::new().expect("dlm");
    let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
    let stage = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![stage.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        Arc::clone(&cwr.alloc),
        Arc::clone(&nvme),
        None,
    )
    .await
    .expect("tiered cache");
    let router = DataRouter::new(dlm.clone(), cache, Arc::clone(&cwr.alloc), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.router.set_meta_backend(Arc::clone(&auth.meta));
    fs.meta_backend = Some(Arc::clone(&auth.meta));
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    CoWriterFs {
        fs,
        req,
        _stage: stage,
    }
}

fn pat(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ tag | 1).collect()
}

/// The shared file: created + seeded by the AUTHORITY as a striped layout
/// of `blocks` whole blocks over its own mints (the fleet's rank-0 create
/// shape) — BEFORE the co-writer arms the process's ownership map
/// all-foreign. Returns the ino.
async fn seed_striped_file(auth: &Authority, name: &str, blocks: u64) -> u64 {
    let bs = auth.alloc.chunk_size();
    let fsz = blocks * bs;
    let ino = auth
        .meta
        .create_with_rdev_size(1, name, 0o100644, 0, 0, 0, 0)
        .await
        .expect("create")
        .ino;
    let mut map = std::collections::HashMap::new();
    let mut refs = Vec::new();
    for b in 0..blocks {
        let off = auth.alloc.allocate_block().await.expect("seed mint");
        auth.alloc.publish_block(off);
        map.insert(b as u32, off.to_string());
        refs.push(BlockRefOp::taken(BlockRef {
            vol_tag: volume_tag(DATA_VOL),
            block_idx: off / bs,
            owner_ino: ino,
            block_index: b as u32,
        }));
    }
    let head = bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "striped".into(),
        size: fsz,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map),
    })
    .expect("head bytes");
    auth.meta
        .set_layout_and_size(ino, &head, fsz, &refs)
        .await
        .expect("the seeded head commits");
    ino
}

/// Every block the durable layout of `ino` names, as `(index, block_idx)`.
async fn durable_blocks(auth: &Authority, ino: u64) -> Vec<(u32, u64)> {
    use squeezefs::meta_backend::Metadata as _;
    let bytes = auth
        .meta
        .getxattr(ino, "layout")
        .await
        .expect("layout read")
        .expect("a layout exists");
    let layout = squeezefs::layout_wire::decode_layout_any(&bytes).expect("a bincode layout");
    let chunk = auth.alloc.chunk_size();
    let entries: Vec<(u32, String)> = match layout
        .block_map_id
        .as_deref()
        .and_then(|id| id.strip_prefix("indirect:"))
    {
        Some(blob) => {
            let io = squeezefs::meta_backend::kv::indirect_map::indirect_map_io()
                .expect("the indirect-map hook is armed");
            (io.read)(blob.to_string())
                .await
                .expect("the blob rehydrates")
        }
        None => layout.block_map.unwrap_or_default().into_iter().collect(),
    };
    let mut out: Vec<(u32, u64)> = entries
        .into_iter()
        .map(|(b, key)| {
            let cleaned = squeezefs::routing::clean_block_key(&key);
            let parts = auth
                .br
                .parse_block_key_parts(&cleaned)
                .expect("an allocator-managed key");
            (b, parts.offset / chunk)
        })
        .collect();
    out.sort_unstable();
    out
}

/// The whole lane supply reachable by the co-writer: its LOCAL reachable
/// count plus every lane-owned block on the authority's free list and in
/// the authority's grace ring (the harvest reaches both).
fn lane_supply(
    auth: &Authority,
    cwr: &CoWriter,
    writers: u16,
    lane_id: u64,
    cap_blocks: u64,
) -> u64 {
    let chunk = auth.alloc.chunk_size();
    let in_lane = |idx: &u64| lane::block_lane_of(*idx, writers) == lane_id;
    let on_authority = auth
        .alloc
        .free_block_indices()
        .into_iter()
        .filter(in_lane)
        .count() as u64;
    let on_cowriter = cwr
        .alloc
        .free_block_indices()
        .into_iter()
        .filter(in_lane)
        .count() as u64;
    let graced = (0..cap_blocks)
        .filter(in_lane)
        .filter(|idx| auth.alloc.grace_holds(idx * chunk))
        .count() as u64;
    // The co-writer's virgin tail, counted EXACTLY (the allocator's own
    // `lane_reachable_blocks` divides the tail by the width, which
    // rounds).
    let virgin = (cwr.alloc.highest_block_index()..cap_blocks)
        .filter(in_lane)
        .count() as u64;
    on_authority + on_cowriter + graced + virgin
}

// ===========================================================================
// 1. The field shape: a FUSE-level rewrite loop under range custody
// ===========================================================================

/// Contract (finding 15's supply closure, the production write path): a
/// co-writer rewriting ONE range of a shared striped file under range
/// custody — `fs.write` on whole blocks (the rewrite epoch), `fsync` (the
/// shipped publish), repeated past its lane share so the loop runs on
/// HARVESTED offsets — must free every displaced block EXACTLY ONCE on the
/// authority: `block_untracked_free_refusals` unchanged (no shipped free
/// arrives for a block the authority already freed), no double frees, and
/// the lane's supply closes (every lane block is live in the layout, on a
/// free list, or in the grace ring — none leaked).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_custody_rewrite_loop_frees_every_displaced_block_exactly_once() {
    rewrite_loop(LoopShape {
        writes_per_block: 1,
        tag: "rw-loop",
        burners: 0,
        blocks: 8,
        block_size: None,
    })
    .await;
}

/// The same loop, every block overwritten TWICE per round before the
/// fsync — the field's same-epoch re-rewrite: the second overwrite finds
/// the block shadow-bound (the overlay declines), rides write-through,
/// and its durable merge displaces the epoch's RAM-only dest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_epoch_rerewrite_loop_frees_every_displaced_block_exactly_once() {
    rewrite_loop(LoopShape {
        writes_per_block: 2,
        tag: "rw-loop-twice",
        burners: 1,
        blocks: 8,
        block_size: None,
    })
    .await;
}

/// The field's map shape: a file whose layout SPILLS to an indirect blob
/// (the 10 GiB shared file), so every co-writer publish mints its own
/// map blob and the authority composes over the rehydrated map.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_indirect_map_rewrite_loop_frees_every_displaced_block_exactly_once() {
    rewrite_loop(LoopShape {
        writes_per_block: 1,
        tag: "rw-loop-indirect",
        burners: 2,
        blocks: 640,
        block_size: None,
    })
    .await;
}

struct LoopShape {
    writes_per_block: u32,
    tag: &'static str,
    /// Burner inos created ahead of the shared file: the fixture's custody
    /// clocks are MANUAL (grants never expire in fixture time) and inos
    /// recur across this binary's fresh volumes, so every range-acquiring
    /// test owns a distinct ino number (the mw_cowriter_free_tests law).
    burners: u32,
    /// File length in blocks (640 entries spill past the 64 KiB-node
    /// fixture volume's 16 KiB inline cap — the indirect shape).
    blocks: u64,
    /// `SQUEEZEFS_DEFAULT_BLOCK_SIZE` pin (`None` = the shipped 4 MiB).
    block_size: Option<u64>,
}

async fn rewrite_loop(shape: LoopShape) {
    let LoopShape {
        writes_per_block,
        tag,
        burners,
        blocks,
        block_size,
    } = shape;
    let _serial = serial();
    let _restore = restore();
    match block_size {
        Some(bs) => std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", bs.to_string()),
        None => std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE"),
    }
    fuse_client::set_patch_max_bytes(0);
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), tag).await;
    let dev = data_device(dir.path(), &format!("{tag}.dev"));

    // Geometry: `blocks` blocks; the co-writer rewrites blocks 2..6 (its
    // range) each round.
    const RANGE: std::ops::Range<u64> = 2..6;
    let auth = Authority::start(&vol, &dev, &[NODE_A], 0).await;
    let bs = auth.alloc.chunk_size();
    let fsz = blocks * bs;
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(fsz, bs));
    // The production arm's blob-aware compose hook (rung 20): without it
    // an indirect head REFUSES every shipped publish.
    squeezefs::meta_backend::kv::indirect_map::install_indirect_map_io(
        squeezefs::multi_writer::indirect_map_io_for(Arc::clone(&auth.br)),
    );
    // A small store so the loop crosses the lane share: the seed plus 32
    // blocks, W = 2 ⇒ the co-writer's lane holds ~16; four blocks per
    // round means the fresh supply is gone after ~4 rounds and every
    // later round runs on harvested offsets.
    let cap_blocks: u64 = 2 * blocks + 32;
    auth.alloc.set_capacity_bytes(cap_blocks * bs);

    // The file: created + seeded by the AUTHORITY as a striped layout over
    // its own blocks (the fleet's rank-0 create shape) — BEFORE the
    // co-writer arms the process's ownership map all-foreign.
    for i in 0..burners {
        auth.meta
            .create_with_rdev_size(1, &format!("{tag}-burner-{i}"), 0o100644, 0, 0, 0, 0)
            .await
            .expect("burner create");
    }
    let ino = seed_striped_file(&auth, "shared.bin", blocks).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    cwr.alloc.set_capacity_bytes(cap_blocks * bs);
    let lane_id = u64::from(cwr.client.lane_partition().writer_id());
    let h = cowriter_fs(&auth, &cwr, &dev).await;
    data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(1, Ordering::Relaxed);

    let supply_before = lane_supply(&auth, &cwr, 2, lane_id, cap_blocks);
    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);
    let live_before = METRICS.block_live_free_refusals.load(Ordering::Relaxed);
    let doubles_before = METRICS.block_double_frees.load(Ordering::Relaxed);
    let enospc_before = METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed);
    let served_before = publish::stats().free_served_blocks;
    let shipped_before = publish::stats().free_shipped_blocks;
    let recomputed_before = publish::stats().free_recomputed_blocks;
    let reclaims_before = METRICS.publish_blob_orphan_reclaims.load(Ordering::Relaxed);
    let recycles_before = METRICS
        .cowriter_unpublished_recycles
        .load(Ordering::Relaxed);

    let fh =
        h.fs.open(h.req, ino, libc::O_WRONLY as u32, 0)
            .await
            .expect("the co-writer opens the shared file")
            .fh;
    const ROUNDS: u32 = 10;
    for round in 0..ROUNDS {
        for pass in 0..writes_per_block {
            for b in RANGE {
                let data = pat(
                    bs as usize,
                    (round as u8).wrapping_mul(7) ^ (b as u8) ^ (pass as u8).wrapping_mul(31),
                );
                let w =
                    h.fs.write(h.req, ino, fh, b * bs, bytes::Bytes::from(data), 0, 0)
                        .await
                        .unwrap_or_else(|e| {
                            panic!("round {round} pass {pass} block {b}: write failed: {e:?}")
                        });
                assert_eq!(w.written as u64, bs, "round {round} block {b}: short write");
            }
        }
        let fs_res = h.fs.fsync(h.req, ino, fh, false).await;
        // The freed offsets sit on the authority's list; the fixture's
        // reclaimer is the authority's.
        auth.br.reclaim_drain().await;
        let st = publish::stats();
        eprintln!(
            "round {round}: shipped {} served {} recomputed {} untracked {} supply {} \
             (auth free {} cwr free {})",
            st.free_shipped_blocks - shipped_before,
            st.free_served_blocks - served_before,
            st.free_recomputed_blocks - recomputed_before,
            METRICS
                .block_untracked_free_refusals
                .load(Ordering::Relaxed)
                - untracked_before,
            lane_supply(&auth, &cwr, 2, lane_id, cap_blocks),
            auth.alloc
                .free_block_indices()
                .into_iter()
                .filter(|i| lane::block_lane_of(*i, 2) == lane_id)
                .count(),
            cwr.alloc.free_block_indices().len(),
        );
        fs_res.unwrap_or_else(|e| panic!("round {round}: fsync failed: {e:?}"));
    }
    h.fs.release(h.req, ino, fh, 0, 0, false)
        .await
        .expect("release");
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the co-writer's pipeline drains"
    );
    auth.br.reclaim_drain().await;

    let durable = durable_blocks(&auth, ino).await;
    assert_eq!(
        durable.len(),
        blocks as usize,
        "the layout names every block"
    );
    for (b, idx) in &durable {
        if RANGE.contains(&u64::from(*b)) {
            assert_eq!(
                lane::block_lane_of(*idx, 2),
                lane_id,
                "block {b} of the rewritten range is a co-writer mint"
            );
        }
        assert!(
            !auth.free_listed(*idx),
            "live block {b} (idx {idx}) must not sit on the authority's free list"
        );
        assert!(
            !cwr.alloc.free_block_indices().contains(idx),
            "live block {b} (idx {idx}) must not sit on the co-writer's free list"
        );
    }

    let stats = publish::stats();
    let shipped = stats.free_shipped_blocks - shipped_before;
    let served = stats.free_served_blocks - served_before;
    let recomputed = stats.free_recomputed_blocks - recomputed_before;
    let untracked = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed)
        - untracked_before;
    let displaced_total =
        u64::from(ROUNDS) * u64::from(writes_per_block) * (RANGE.end - RANGE.start);
    eprintln!(
        "rewrite loop: displaced {displaced_total} · shipped {shipped} · served {served} · \
         recomputed {recomputed} · untracked refusals {untracked} · enospc {} · recycles {}",
        METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed) - enospc_before,
        METRICS
            .cowriter_unpublished_recycles
            .load(Ordering::Relaxed)
            - recycles_before
    );
    assert_eq!(
        untracked, 0,
        "every shipped free must be a FIRST release — a refusal here is a free the \
         co-writer shipped for a block the authority already freed (the fleet's \
         3,449-refusal double-release stream) or a free nobody accepted"
    );
    assert_eq!(
        METRICS.block_live_free_refusals.load(Ordering::Relaxed),
        live_before,
        "no live-free refusal"
    );
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles_before,
        "no double free"
    );
    // Every displaced DATA block of a recomputed publish is freed by the
    // recompute; the only frees a co-writer SHIPS on this loop are its
    // own-mint map blobs' orphan reclaims (the indirect shape), each
    // accepted exactly once.
    let blob_reclaims =
        METRICS.publish_blob_orphan_reclaims.load(Ordering::Relaxed) - reclaims_before;
    assert_eq!(
        recomputed, displaced_total,
        "every displaced data block was freed exactly once by the authority's recompute \
         (the RAM-only lifetimes included — finding 15)"
    );
    assert_eq!(
        shipped, blob_reclaims,
        "the only shipped frees are the own-mint blob reclaims, one per lineage"
    );
    assert_eq!(
        served, shipped,
        "every shipped free was accepted (a FIRST release)"
    );
    assert_eq!(
        METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed) - enospc_before,
        0,
        "the loop ran {ROUNDS} rounds of {} blocks on a 16-block lane share without \
         starving: the freed supply came back through the harvest",
        RANGE.end - RANGE.start
    );
    // Supply closure: the lane's blocks are live (4 in the layout) or
    // reachable (free-listed on either side / in grace) — none leaked.
    let live_lane_blocks = durable
        .iter()
        .filter(|(_, idx)| lane::block_lane_of(*idx, 2) == lane_id)
        .count() as u64;
    let supply_after = lane_supply(&auth, &cwr, 2, lane_id, cap_blocks);
    assert_eq!(
        supply_after + live_lane_blocks,
        supply_before,
        "the lane supply closes: reachable-after + live == reachable-before (leaked = {})",
        supply_before as i64 - supply_after as i64 - live_lane_blocks as i64
    );
    for (_, idx) in &durable {
        assert_eq!(
            auth.population(*idx).await,
            1,
            "live block {idx} carries exactly one durable reference"
        );
    }

    drop(h);
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 1b. Term 3 — the fenced-close lineage (the s11 co-writers' CLAIM ANOMALY)
// ===========================================================================

/// Contract (finding 15 term 3, `.benchmarks/2026-09-06-cowriter-free-
/// residual-lineage.md`): the fleet shape is FOUR ranks per co-writer on
/// one ino — an fsync whose `acquire_write_lease` token has been superseded
/// by sibling stripe grants (a PROCESS-LOCAL rotation). Inside that fsync
/// the flush leg publishes a parked partial block under the ino's CURRENT
/// generation (the 2026-08-06 tail-loss law), and that durable merge
/// carries the WHOLE dirty RAM map — every fed rewrite-epoch binding — so
/// the authority's custody-scoped compose adopts them and RECOMPUTES the
/// displaced predecessors free. Then `close_rewrite_epoch` presented the
/// fsync's STALE token, took the W5 arm ("publish nothing, free nothing"),
/// and dropped the epoch's parked keys — whose device frees the authority
/// had already run — WITHOUT their local hygiene: the co-writer's refcount
/// entries lingered at 1, the offsets came back through the lane harvest,
/// and every re-claim logged `CLAIM ANOMALY` (m52/m53/m56: one fence drop
/// each, 323/205/323 anomalies; m50/m57: none, none). The same arm
/// discarded the uncovered fed bindings — acked bytes lost on a LIVE
/// mount.
///
/// The law: within one process a stale token is a lease rotation, never
/// a fence — the close re-presents the current generation and converges
/// (`rewrite_shadow_close_retries`), exactly like the flush leg it runs
/// beside. Pinned: the fsync succeeds; the durable map names the epoch's
/// bindings; every covered predecessor is freed ONCE on the authority and
/// its local tracking is gone; `block_claim_anomalies` stays 0 as the
/// freed offsets are re-harvested and re-claimed across the following
/// rounds; `block_untracked_free_refusals` unchanged; the lane supply
/// closes. RED on dev c1450c66: `FencingTokenExpired` out of the fsync,
/// `rewrite_shadow_fence_drops` +1, four lingering refcounts, and 4
/// anomalies on the re-claims.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rotated_fsync_token_converges_the_close_and_orphans_no_local_hygiene() {
    let _serial = serial();
    let _restore = restore();
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    fuse_client::set_patch_max_bytes(0);
    // The write-through pipeline is the epoch's vehicle here (the overlay
    // feeds the same epoch on the fleet; the pipeline's quiesce makes the
    // shadow records deterministic in-process).
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(false);
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "rotated-close").await;
    let dev = data_device(dir.path(), "rotated-close.dev");

    const BLOCKS: u64 = 8;
    const RANGE: std::ops::Range<u64> = 2..6;
    let auth = Authority::start(&vol, &dev, &[NODE_A], 0).await;
    let bs = auth.alloc.chunk_size();
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(BLOCKS * bs, bs));
    squeezefs::meta_backend::kv::indirect_map::install_indirect_map_io(
        squeezefs::multi_writer::indirect_map_io_for(Arc::clone(&auth.br)),
    );
    let cap_blocks: u64 = 2 * BLOCKS + 32;
    auth.alloc.set_capacity_bytes(cap_blocks * bs);
    let ino = seed_striped_file(&auth, "shared.bin", BLOCKS).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    cwr.alloc.set_capacity_bytes(cap_blocks * bs);
    let lane_id = u64::from(cwr.client.lane_partition().writer_id());
    let h = cowriter_fs(&auth, &cwr, &dev).await;
    data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(1, Ordering::Relaxed);

    let supply_before = lane_supply(&auth, &cwr, 2, lane_id, cap_blocks);
    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);
    let anomalies_before = METRICS.block_claim_anomalies.load(Ordering::Relaxed);
    let fence_drops_before = METRICS.rewrite_shadow_fence_drops.load(Ordering::Relaxed);
    let retries_before = METRICS.rewrite_shadow_close_retries.load(Ordering::Relaxed);
    let enospc_before = METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed);

    let fh =
        h.fs.open(h.req, ino, libc::O_WRONLY as u32, 0)
            .await
            .expect("the co-writer opens the shared file")
            .fh;
    let hr = &h;
    let write_whole = |b: u64, tag: u8| async move {
        let w = hr
            .fs
            .write(
                hr.req,
                ino,
                fh,
                b * bs,
                bytes::Bytes::from(pat(bs as usize, tag)),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("block {b}: write failed: {e:?}"));
        assert_eq!(w.written as u64, bs, "block {b}: short write");
    };

    // Round 0 (warm-up): the range is the co-writer's — its mints are now
    // the durable predecessors the next round displaces.
    for b in RANGE {
        write_whole(b, 0x11).await;
    }
    h.fs.fsync(h.req, ino, fh, false)
        .await
        .expect("round 0 fsync");
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the round-0 pipeline drains"
    );
    let round0: Vec<(u32, u64)> = durable_blocks(&auth, ino)
        .await
        .into_iter()
        .filter(|(b, _)| RANGE.contains(&u64::from(*b)))
        .collect();
    assert_eq!(round0.len(), 4, "premise: the range is durably mapped");
    for (b, idx) in &round0 {
        assert_eq!(
            lane::block_lane_of(*idx, 2),
            lane_id,
            "premise: block {b} is a co-writer mint"
        );
        assert_eq!(
            cwr.alloc.refcount(idx * bs),
            Some(1),
            "premise: the co-writer tracks its live mint {idx}"
        );
    }

    // Round 1: the whole-block rewrites feed the epoch (RAM-only bindings;
    // the round-0 keys park), then a PARTIAL write of block 6 parks a
    // coverage-incomplete buffer — the flush leg's covering publish.
    let recomputed_before = publish::stats().free_recomputed_blocks;
    for b in RANGE {
        write_whole(b, 0x22).await;
    }
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the round-1 pipeline drains"
    );
    assert!(
        METRICS.rewrite_shadow_open_epochs.load(Ordering::Relaxed) > 0,
        "premise: the rewrite epoch holds the round-1 bindings"
    );
    let partial =
        h.fs.write(
            h.req,
            ino,
            fh,
            6 * bs,
            bytes::Bytes::from(pat(4096, 0x33)),
            0,
            0,
        )
        .await
        .expect("the partial write of block 6");
    assert_eq!(partial.written, 4096, "short partial write");

    // The rotation: the fsync's token is stale and not a live range grant
    // (a sibling rank's stripe grant advanced the generation and this
    // rank's own grant died).
    let current = h.fs.dlm().get_fencing_token_ino(ino);
    let stale = (1..current)
        .rev()
        .take(64)
        .find(|t| !squeezefs::meta_ship::tokens::range_token_live(ino, *t))
        .expect("premise: a superseded, dead token exists below the current generation");
    h.fs.flush_inode_to_backend(ino, stale)
        .await
        .expect("the rotated fsync converges (RED: FencingTokenExpired — the W5 arm)");
    assert_eq!(
        METRICS.rewrite_shadow_fence_drops.load(Ordering::Relaxed) - fence_drops_before,
        0,
        "a process-local rotation is not a fence"
    );
    assert!(
        METRICS.rewrite_shadow_close_retries.load(Ordering::Relaxed) > retries_before,
        "the close re-presented the current generation"
    );
    assert_eq!(
        METRICS.rewrite_shadow_open_epochs.load(Ordering::Relaxed),
        0,
        "the epoch closed"
    );
    auth.br.reclaim_drain().await;

    // The durable map names the round-1 bindings — the acked bytes are
    // durable, not discarded with a phantom fence.
    let round1: Vec<(u32, u64)> = durable_blocks(&auth, ino)
        .await
        .into_iter()
        .filter(|(b, _)| RANGE.contains(&u64::from(*b)))
        .collect();
    for ((b0, idx0), (b1, idx1)) in round0.iter().zip(round1.iter()) {
        assert_eq!(b0, b1);
        assert_ne!(
            idx0, idx1,
            "block {b0}: the round-1 binding displaced the round-0 mint {idx0}"
        );
    }
    // Every round-0 predecessor: freed ONCE on the authority (the covering
    // publish's recompute), and the co-writer's LOCAL tracking of it is
    // gone — the hygiene the fenced arm used to drop.
    for (b, idx) in &round0 {
        assert!(
            auth.free_listed(*idx),
            "block {b}: the round-0 mint {idx} is on the authority's free list"
        );
        assert_eq!(
            auth.population(*idx).await,
            0,
            "block {b}: the round-0 mint {idx} holds no durable reference"
        );
        assert_eq!(
            cwr.alloc.refcount(idx * bs),
            None,
            "block {b}: the co-writer's tracking of the freed mint {idx} lingers — the \
             CLAIM ANOMALY lineage (RED)"
        );
    }
    assert_eq!(
        publish::stats().free_recomputed_blocks - recomputed_before,
        // the four round-0 predecessors + block 6's seed predecessor
        5,
        "the authority freed every displaced block by recompute, once"
    );

    // The anomaly venue, exactly: drain the lane's fresh supply so the
    // allocation funnel HARVESTS the freed round-0 offsets back from the
    // authority and RE-CLAIMS them (`claim_block_idx` — where a lingering
    // entry fires `CLAIM ANOMALY`), then hand the probe mints back through
    // the never-published recycle arm (their supply stays this lane's).
    let mut handed_out: Vec<u64> = Vec::new();
    let mut seen_round0 = 0usize;
    for _ in 0..cap_blocks {
        let off = cwr
            .alloc
            .allocate_block()
            .await
            .expect("the lane funnel serves (fresh, then harvested)");
        handed_out.push(off);
        if round0.iter().any(|(_, idx)| idx * bs == off) {
            seen_round0 += 1;
            if seen_round0 == round0.len() {
                break;
            }
        }
    }
    assert_eq!(
        seen_round0,
        round0.len(),
        "every freed round-0 offset came back through the harvest and was re-claimed"
    );
    assert_eq!(
        METRICS.block_claim_anomalies.load(Ordering::Relaxed) - anomalies_before,
        0,
        "no re-claim found a lingering local refcount (CLAIM ANOMALY = 0 — RED: one per \
         round-0 offset)"
    );
    for off in handed_out {
        cwr.alloc
            .abandon_unpublished_offset(off)
            .await
            .expect("the probe mint recycles into its own lane");
    }

    // Closure: keep rewriting on the recycled + harvested supply — every
    // round displaces four blocks the authority recomputes free, every
    // fsync closes an epoch under a live token.
    for round in 2..8u8 {
        for b in RANGE {
            write_whole(b, round.wrapping_mul(0x17)).await;
        }
        h.fs.fsync(h.req, ino, fh, false)
            .await
            .unwrap_or_else(|e| panic!("round {round}: fsync failed: {e:?}"));
        auth.br.reclaim_drain().await;
    }
    h.fs.release(h.req, ino, fh, 0, 0, false)
        .await
        .expect("release");
    assert!(
        h.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the co-writer's pipeline drains"
    );
    auth.br.reclaim_drain().await;

    assert_eq!(
        METRICS.block_claim_anomalies.load(Ordering::Relaxed) - anomalies_before,
        0,
        "no offset was claimed while this mount still tracked it (CLAIM ANOMALY = 0)"
    );
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed)
            - untracked_before,
        0,
        "no shipped free arrived for a block the authority already freed"
    );
    assert_eq!(
        METRICS.alloc_lane_enospc_refusals.load(Ordering::Relaxed) - enospc_before,
        0,
        "the lane never starved"
    );
    let durable = durable_blocks(&auth, ino).await;
    let live_lane_blocks = durable
        .iter()
        .filter(|(_, idx)| lane::block_lane_of(*idx, 2) == lane_id)
        .count() as u64;
    let supply_after = lane_supply(&auth, &cwr, 2, lane_id, cap_blocks);
    assert_eq!(
        supply_after + live_lane_blocks,
        supply_before,
        "the lane supply closes (leaked = {})",
        supply_before as i64 - supply_after as i64 - live_lane_blocks as i64
    );
    for (_, idx) in &durable {
        assert_eq!(
            auth.population(*idx).await,
            1,
            "live block {idx} carries exactly one durable reference"
        );
    }

    drop(h);
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 2. The router-level contract: harvest → publish → displace → free, both
//    publish shapes, the supply closing every cycle
// ===========================================================================

/// Contract (deliverable 1's router-level form, both publish shapes): a
/// co-writer HARVESTS a lane offset, publishes a layout that binds it —
/// through the full `set_layout_and_size` Put (chain-ineligible
/// provenance) and through the Lever-B merge conveyor, alternating — and
/// each publish displaces the PREVIOUS round's harvested block. Every
/// displaced block is freed exactly once on the authority (the recompute
/// frees it on the merge shape; the verbatim Put hands it back and the
/// co-writer's shipped free is ACCEPTED), the untracked tripwire never
/// moves, the ledger reads population 0 for the freed block and 1 for the
/// live one, the freed block re-enters the authority's lane supply and a
/// later harvest hands it out again — N cycles, closure exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_harvested_offset_published_then_displaced_frees_and_is_reharvested() {
    let _serial = serial();
    let _restore = restore();
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "harvest-cycle").await;
    let dev = data_device(dir.path(), "harvest-cycle.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A], 4 * 1024 * 1024).await;
    let ino = auth
        .meta
        .create_with_rdev_size(1, "cycle.bin", 0o100644, 0, 0, 0, 0)
        .await
        .expect("create")
        .ino;
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let bs = cwr.alloc.chunk_size();
    let tag = volume_tag(DATA_VOL);
    let epoch = cwr.client.lease_epoch();
    let lane_id = u64::from(cwr.client.lane_partition().writer_id());
    let stage = tempdir().unwrap();
    let dlm = DlmClient::new().expect("dlm");
    let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
    let cache = TieredCache::new(
        vec![stage.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("16MB"),
        Arc::clone(&cwr.alloc),
        Arc::clone(&nvme),
        None,
    )
    .await
    .expect("tiered cache");
    let router = DataRouter::new(dlm.clone(), cache, Arc::clone(&cwr.alloc), nvme);
    router.set_meta_backend(Arc::clone(&cwr.meta));

    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);
    let live_before = METRICS.block_live_free_refusals.load(Ordering::Relaxed);
    let doubles_before = METRICS.block_double_frees.load(Ordering::Relaxed);

    // The durable truth to start from: block 0 → A, a co-writer lane mint
    // the co-writer published (verbatim, first Put) — and a second lane
    // block B freed through the shipped ladder so the authority's list
    // holds harvestable lane supply.
    let a_off = cwr.alloc.allocate_block().await.expect("mint A");
    cwr.alloc.publish_block(a_off);
    let mut entry = CachedMetadata {
        file_type: "striped".into(),
        size: bs,
        block_map: Some(Arc::new(std::collections::HashMap::new())),
        layout_dirty: true,
        layout_delta_chain: squeezefs::routing::LAYOUT_DELTA_CHAIN_INELIGIBLE,
        ..Default::default()
    };
    router.metadata_cache.insert(ino, entry.clone());
    let tok = dlm.get_fencing_token_ino(ino);
    router
        .merge_block_mappings_coalesced(
            ino,
            vec![(0u32, a_off.to_string())],
            0,
            squeezefs::routing::LayoutFlip::KeepLayout,
            tok,
        )
        .await
        .expect("the first Put lands");
    assert_eq!(
        auth.population(a_off / bs).await,
        1,
        "fixture: A is durable"
    );
    let b_off = cwr.alloc.allocate_block().await.expect("mint B");
    cwr.br
        .free_block(&b_off.to_string())
        .await
        .expect("B's free ships (never published: the untracked→seed arm)");
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(b_off / bs), "fixture: B is harvestable");

    let mut prev_off = a_off;
    let mut request_id = 7_000u64;
    for (round, shape) in ["full-put", "lever-b-merge", "full-put", "lever-b-merge"]
        .iter()
        .enumerate()
    {
        // HARVEST: the authority hands out the lane's free supply; the
        // co-writer adopts it and its free-list-first funnel serves one.
        request_id += 1;
        let got = publish::ship_harvest_lane_free(
            &auth.endpoint,
            tag,
            lane_id as u16,
            2,
            16,
            epoch,
            request_id,
        )
        .await
        .expect("the harvest ships")
        .blocks;
        assert!(
            !got.is_empty(),
            "{shape} round {round}: the lane has harvestable supply"
        );
        assert_eq!(cwr.alloc.adopt_lane_free_grant(&got), got.len() as u64);
        let new_off = cwr.alloc.allocate_block().await.expect("reuse");
        let new_idx = new_off / bs;
        assert!(
            got.contains(&new_idx),
            "{shape} round {round}: the funnel serves a HARVESTED offset (got {new_idx}, \
             harvest {got:?})"
        );
        cwr.alloc.publish_block(new_off);

        // PUBLISH: block 0 → the harvested offset, displacing the previous
        // round's block — the co-writer's RAM base mirrors the durable
        // head (block 0 → prev), as a real co-writer's does.
        let mut base = std::collections::HashMap::new();
        base.insert(0u32, prev_off.to_string());
        entry.block_map = Some(Arc::new(base));
        entry.layout_delta_chain = if *shape == "full-put" {
            squeezefs::routing::LAYOUT_DELTA_CHAIN_INELIGIBLE
        } else {
            0
        };
        router.metadata_cache.insert(ino, entry.clone());
        let served_before = publish::stats().free_served_blocks;
        let recomputed_before = publish::stats().free_recomputed_blocks;
        let tok = dlm.get_fencing_token_ino(ino);
        let displaced = router
            .merge_block_mappings_coalesced(
                ino,
                vec![(0u32, new_off.to_string())],
                0,
                squeezefs::routing::LayoutFlip::KeepLayout,
                tok,
            )
            .await
            .unwrap_or_else(|e| panic!("{shape} round {round}: the publish ships: {e}"));
        for k in &displaced {
            router
                .backend_router
                .free_block(k)
                .await
                .unwrap_or_else(|e| panic!("{shape} round {round}: the displaced free ships: {e}"));
        }
        auth.br.reclaim_drain().await;

        let freed_on_authority = (publish::stats().free_served_blocks - served_before)
            + (publish::stats().free_recomputed_blocks - recomputed_before);
        assert_eq!(
            freed_on_authority, 1,
            "{shape} round {round}: the displaced block is freed exactly once on the authority"
        );
        assert_eq!(
            auth.population(new_idx).await,
            1,
            "{shape} round {round}: the harvested offset's reference landed durably"
        );
        assert_eq!(
            auth.population(prev_off / bs).await,
            0,
            "{shape} round {round}: the displaced block's reference released"
        );
        assert!(
            auth.free_listed(prev_off / bs),
            "{shape} round {round}: the displaced block re-entered the lane supply"
        );
        assert!(
            !auth.free_listed(new_idx) && !cwr.alloc.free_block_indices().contains(&new_idx),
            "{shape} round {round}: the live block is on no free list"
        );
        assert_eq!(
            METRICS
                .block_untracked_free_refusals
                .load(Ordering::Relaxed),
            untracked_before,
            "{shape} round {round}: no refused release anywhere in the cycle"
        );
        prev_off = new_off;
        router.metadata_cache.remove(&ino);
    }
    assert_eq!(
        METRICS.block_live_free_refusals.load(Ordering::Relaxed),
        live_before,
        "no live-free refusal across the cycles"
    );
    assert_eq!(
        METRICS.block_double_frees.load(Ordering::Relaxed),
        doubles_before,
        "no double free across the cycles"
    );

    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 3. The own-mint map blob's lineage closes exactly once (the refusal storm)
// ===========================================================================

/// A `DataRouter` over the co-writer's data plane — the save-funnel drive
/// (`persist_dirty_layout_if_needed` is the pub entry every layout persist
/// funnels through).
async fn save_router(
    dlm: &DlmClient,
    alloc: &Arc<BlockAllocator>,
    dev: &Path,
    meta: &Arc<RoutedMetaBackend>,
    stage: &Path,
) -> DataRouter {
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
    let router = DataRouter::new(dlm.clone(), cache, Arc::clone(alloc), nvme);
    router.set_meta_backend(Arc::clone(meta));
    router
}

/// Contract (the fleet's `block_untracked_free_refusals` storm — 3,449
/// refusals against 3,914 served, ≈ 7 orphan reclaims per own-mint save on
/// every co-writer): a range-episode co-writer's OWN-MINT map blob is freed
/// **exactly once**, whichever of its two issuers runs first — the save
/// tail that displaces it (`old_indirect_to_free`) and the layout-entry
/// insert chokepoint's orphan reclaim (finding 35c) — and a STALE CLONE of
/// the reclaimed entry re-inserted afterwards (the merge that read it
/// before a lock-free release-hook discard) never re-arms the blob's
/// free at the next discard. One shipped free, accepted; the untracked
/// tripwire silent; the dedup gauge names the declined re-arms.
///
/// RED against dev: the collapsing save ships TWO frees for the blob
/// (tail + replacing insert) and every later discard of the re-inserted
/// clone ships another — the second and later come back `Refused`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_own_mint_blob_lineage_closes_exactly_once() {
    let _serial = serial();
    let _restore = restore();
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "blob-lineage").await;
    let dev = data_device(dir.path(), "blob-lineage.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A], 4 * 1024 * 1024).await;
    let ino = auth
        .meta
        .create_with_rdev_size(1, "blob-lineage.bin", 0o100644, 0, 0, 0, 0)
        .await
        .expect("create")
        .ino;
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let bs = cwr.alloc.chunk_size();
    let tag = volume_tag(DATA_VOL);
    let stage = tempdir().unwrap();
    let dlm = DlmClient::new().expect("dlm");
    let router = save_router(&dlm, &cwr.alloc, &dev, &cwr.meta, stage.path()).await;

    // The co-writer's OWN mint K: its map blob, durably taken (the first
    // save's MAP_BLOB record), plus a data block the map names.
    let data_off = cwr.alloc.allocate_block().await.expect("data mint");
    cwr.alloc.publish_block(data_off);
    let k_off = cwr.alloc.allocate_block().await.expect("blob mint K");
    cwr.alloc.publish_block(k_off);
    let k_idx = k_off / bs;
    let (be_id, _, _) = router.backend_router.get_active_backend().expect("backend");
    let k_key = router.backend_router.persist_block_key(&be_id, k_off);
    auth.meta
        .commit_block_refs(
            ino,
            &[
                BlockRefOp::taken(BlockRef {
                    vol_tag: tag,
                    block_idx: data_off / bs,
                    owner_ino: ino,
                    block_index: 0,
                }),
                BlockRefOp::taken(BlockRef {
                    vol_tag: tag,
                    block_idx: k_idx,
                    owner_ino: ino,
                    block_index: squeezefs::meta_backend::kv::block_refs::BLOCK_INDEX_MAP_BLOB,
                }),
            ],
        )
        .await
        .expect("the durable records commit");
    // The ino is a RANGE EPISODE on this mount (the co-writer-side
    // discriminator every blob-lifecycle arm keys on).
    squeezefs::meta_ship::tokens::record_range_grant(ino, (0, bs), 9001);
    let own_mint_entry = || {
        let mut map = std::collections::HashMap::new();
        map.insert(0u32, data_off.to_string());
        CachedMetadata {
            file_type: "striped".into(),
            size: bs,
            block_map_id: Some(Arc::from(format!("indirect:{k_key}").as_str())),
            block_map_id_own_mint: true,
            block_map: Some(Arc::new(map)),
            layout_dirty: true,
            layout_delta_chain: squeezefs::routing::LAYOUT_DELTA_CHAIN_INELIGIBLE,
            ..Default::default()
        }
    };

    let shipped_before = publish::stats().free_shipped_blocks;
    let served_before = publish::stats().free_served_blocks;
    let refused_before = publish::stats().free_refused_blocks;
    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);
    let dedups_before = METRICS
        .publish_blob_orphan_reclaim_dedups
        .load(Ordering::Relaxed);

    // The collapsing save: the tiny map fits inline, so the save stops
    // naming K — the tail frees the own-mint predecessor AND the
    // republish replaces an own-mint entry with a blob-less one.
    router.metadata_cache.insert(ino, own_mint_entry());
    let tok = dlm.get_fencing_token_ino(ino);
    router
        .persist_dirty_layout_if_needed(&format!("inode_{ino}"), tok)
        .await
        .expect("the collapsing save lands");
    // The insert chokepoint's reclaim is DETACHED (quiesce → release →
    // free); give it its bounded window before reading the ledger.
    let settle = async {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            auth.br.reclaim_drain().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    settle.await;
    // K's free ran the authority's ladder (the `Freed` verdict below is the
    // supply's return; the §5.5 ahead refill may already have harvested it
    // back to a lane holder, so free-list membership is not the probe).
    assert_eq!(
        auth.population(k_idx).await,
        0,
        "K's MAP_BLOB record was released by the save"
    );
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped_before,
        1,
        "the own-mint blob's free travelled EXACTLY once (tail + replacing insert are one \
         lineage closure, not two issuers)"
    );
    assert_eq!(publish::stats().free_served_blocks - served_before, 1);
    assert_eq!(
        publish::stats().free_refused_blocks - refused_before,
        0,
        "no refused duplicate on the owner ledger"
    );
    assert_eq!(
        METRICS
            .publish_blob_orphan_reclaim_dedups
            .load(Ordering::Relaxed)
            - dedups_before,
        1,
        "the replacing insert's reclaim arm DECLINED the closed lineage (the dedup gauge)"
    );

    // The stale-clone re-arm: a merge that cloned the own-mint entry before
    // a lock-free discard re-publishes it afterwards; the next discard
    // (the range-release hook) must free nothing.
    router.publish_layout_cache_entry(ino, own_mint_entry());
    router.discard_layout_cache(ino);
    router.publish_layout_cache_entry(ino, own_mint_entry());
    router.discard_layout_cache(ino);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        auth.br.reclaim_drain().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped_before,
        1,
        "a re-inserted stale clone of the closed lineage re-arms NO free"
    );
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed),
        untracked_before,
        "the double-release tripwire never fired"
    );
    assert_eq!(publish::stats().free_refused_blocks - refused_before, 0);

    squeezefs::meta_ship::tokens::test_clear_range_cache();
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 4. The owner names WHY it refused, and the ledger closes on refusals
// ===========================================================================

/// Contract: a genuine double release stays REFUSED (the tripwire keeps
/// its meaning), the owner's ledger counts it on `free_refused_blocks` so
/// `served + refused ≡ shipped` closes on the wire, and the class is
/// named on the owner's log line (`already on the free list` here).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_genuine_double_release_is_refused_counted_and_named() {
    let _serial = serial();
    let _restore = restore();
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "double-release").await;
    let dev = data_device(dir.path(), "double-release.dev");
    let auth = Authority::start(&vol, &dev, &[NODE_A], 4 * 1024 * 1024).await;
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let bs = cwr.alloc.chunk_size();
    let tag = volume_tag(DATA_VOL);
    let epoch = cwr.client.lease_epoch();

    let off = cwr.alloc.allocate_block().await.expect("mint");
    cwr.alloc.publish_block(off);
    let idx = off / bs;
    let shipped_before = publish::stats().free_shipped_blocks;
    let served_before = publish::stats().free_served_blocks;
    let refused_before = publish::stats().free_refused_blocks;
    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);

    let first = publish::ship_free_blocks(&auth.endpoint, tag, vec![idx], epoch, 4_001)
        .await
        .expect("the first free ships");
    assert_eq!(first, vec![publish::FreeVerdict::Freed]);
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(idx), "the first release freed the block");

    let second = publish::ship_free_blocks(&auth.endpoint, tag, vec![idx], epoch, 4_002)
        .await
        .expect("the duplicate ships (a NEW request id: a real second act)");
    assert_eq!(
        second,
        vec![publish::FreeVerdict::Refused],
        "the genuine double release is REFUSED"
    );
    assert_eq!(publish::stats().free_shipped_blocks - shipped_before, 2);
    assert_eq!(publish::stats().free_served_blocks - served_before, 1);
    assert_eq!(
        publish::stats().free_refused_blocks - refused_before,
        1,
        "the owner ledger counts the refusal: served + refused ≡ shipped"
    );
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed)
            - untracked_before,
        1,
        "the tripwire fired exactly once, for the duplicate"
    );
    assert!(
        auth.free_listed(idx),
        "the refused duplicate touched nothing: the block stays free exactly once"
    );

    // The same duplicate through the ROUTER seam (a displaced key this
    // mount already released — the stale-refetch re-displacement shape):
    // the co-writer-side instrument names it BEFORE the wire does, as an
    // own-lane free of an offset this mount no longer tracks.
    let own_lane_untracked_before = METRICS
        .cowriter_free_ship_own_lane_untracked
        .load(Ordering::Relaxed);
    let key = cwr.br.persist_block_key("backend_0", off);
    assert_eq!(
        cwr.alloc.refcount(off),
        Some(1),
        "premise: the mint is tracked"
    );
    cwr.alloc.retire_shipped_free_tracking(off);
    assert_eq!(
        cwr.alloc.refcount(off),
        None,
        "premise: the first release retired it"
    );
    let _ = cwr.br.free_block(&key).await;
    assert_eq!(
        METRICS
            .cowriter_free_ship_own_lane_untracked
            .load(Ordering::Relaxed)
            - own_lane_untracked_before,
        1,
        "the co-writer counted the own-lane untracked ship"
    );
    assert_eq!(
        publish::stats().free_refused_blocks - refused_before,
        2,
        "the wire refused it too — the two faces of one residual lineage"
    );
    assert!(auth.free_listed(idx), "still free exactly once");

    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 5. A never-published mint on a LIVE co-writer returns to its own lane
// ===========================================================================

/// Contract (the superseded overlay destination / failed-publish upload
/// class): a never-published mint on a live laned co-writer goes back to
/// this mount's OWN free list — no ledger anywhere moves, the next
/// allocation serves it — counted `cowriter_unpublished_recycles`; the
/// quiet abandon stays for a poisoned custody era and for an offset
/// outside this mount's lanes. RED against dev: every such offset was
/// abandoned "to the next derivation" — gone from the lane until a remount.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_never_published_mint_on_a_live_co_writer_recycles_into_its_own_lane() {
    let _serial = serial();
    let _restore = restore();
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dir = TempDir::new().unwrap();
    let dev = data_device(dir.path(), "recycle.dev");
    let (alloc, _br) = data_plane(&dev).await;
    alloc
        .engage_alloc_lanes(
            squeezefs::meta_backend::kv::journal::AppendPartition::new(2, 1).unwrap(),
        )
        .expect("engage the granted lane");
    fuse_client::set_mount_posture(MountPosture::CoWriter);
    let recycles_before = METRICS
        .cowriter_unpublished_recycles
        .load(Ordering::Relaxed);
    let abandons_before = METRICS
        .cowriter_unpublished_abandons
        .load(Ordering::Relaxed);

    let off = alloc
        .allocate_block()
        .await
        .expect("a laned co-writer mints");
    let idx = off / alloc.chunk_size();
    alloc
        .abandon_unpublished_offset(off)
        .await
        .expect("the never-published cleanup arm");
    assert!(
        alloc.free_block_indices().contains(&idx),
        "the never-published mint is back on this mount's own lane free list"
    );
    assert_eq!(alloc.refcount(off), None, "its tracking is gone");
    assert_eq!(
        METRICS
            .cowriter_unpublished_recycles
            .load(Ordering::Relaxed)
            - recycles_before,
        1
    );
    assert_eq!(
        METRICS
            .cowriter_unpublished_abandons
            .load(Ordering::Relaxed)
            - abandons_before,
        0,
        "nothing was abandoned"
    );
    let again = alloc.allocate_block().await.expect("re-mint");
    assert_eq!(
        again, off,
        "the free-list-first funnel serves the recycled offset back"
    );

    // A poisoned custody era keeps the quiet abandon (the post-fence law).
    data_custody::poison("test: the post-fence law");
    alloc
        .abandon_unpublished_offset(again)
        .await
        .expect("the abandon arm");
    assert!(
        !alloc.free_block_indices().contains(&idx),
        "a poisoned era abandons: nothing re-enters this mount's supply"
    );
    assert_eq!(
        METRICS
            .cowriter_unpublished_abandons
            .load(Ordering::Relaxed)
            - abandons_before,
        1
    );
}

// ===========================================================================
// 6. The frame's RAM-only lifetimes, as a pure law
// ===========================================================================

/// Contract: `frame_ram_only_candidates` names exactly the frame's
/// net-zero data lifetimes (≥ 1 take, takes == releases) the recomputed
/// set does not already name — map-blob custody excluded, a net-positive
/// (live) block excluded, a release-only (durable) block excluded, a block
/// the recompute releases itself excluded, first-appearance order kept.
#[test]
fn frame_ram_only_candidates_names_exactly_the_net_zero_unnamed_lifetimes() {
    use squeezefs::meta_backend::kv::block_refs::{
        frame_ram_only_candidates, BLOCK_INDEX_MAP_BLOB,
    };
    let r = |block_idx: u64, block_index: u32| BlockRef {
        vol_tag: 0xf1,
        block_idx,
        owner_ino: 2,
        block_index,
    };
    let caller = vec![
        BlockRefOp::released(r(4, 0)), // A: durable, released only
        BlockRefOp::taken(r(15, 0)),   // B: RAM-only (take + release)
        BlockRefOp::released(r(15, 0)),
        BlockRefOp::taken(r(23, 0)), // C: live (take only)
        BlockRefOp::taken(r(17, 1)), // D: RAM-only at another index
        BlockRefOp::released(r(17, 1)),
        BlockRefOp::taken(r(90, BLOCK_INDEX_MAP_BLOB)), // blob custody, excluded
        BlockRefOp::released(r(90, BLOCK_INDEX_MAP_BLOB)),
        BlockRefOp::taken(r(31, 2)), // E: net zero but the recompute names it
        BlockRefOp::released(r(31, 2)),
    ];
    let recomputed = vec![
        BlockRefOp::released(r(4, 0)),
        BlockRefOp::taken(r(23, 0)),
        BlockRefOp::released(r(31, 2)),
    ];
    assert_eq!(
        frame_ram_only_candidates(&caller, &recomputed),
        vec![r(15, 0), r(17, 1)],
        "B and D are the frame's RAM-only lifetimes; A is durable, C live, the blob excluded, \
         E already the recompute's"
    );
    assert!(frame_ram_only_candidates(&[], &recomputed).is_empty());
    assert!(
        frame_ram_only_candidates(&[BlockRefOp::taken(r(15, 0))], &[]).is_empty(),
        "a lone take is a live block"
    );
}
