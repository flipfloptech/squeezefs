//! **The authority's read of a RECYCLED co-writer block** — finding 51
//! (`.benchmarks/2026-09-07-read-settle-lost-serialized-authority.md`).
//!
//! The s11-mpiio fleet row (1 authority + 8 co-writers under range
//! custody, one shared 10 GiB file rewritten every ior iteration) burns the
//! must-stay-0 `read_settle_lost_serialized` tripwire on the AUTHORITY —
//! 71 rate-limited lines / 326 counted in one ~150 s row, always block
//! 1290 of inode 2, the key changing every iteration — while three
//! co-writers log `FUSE Fsync: FlushExtents barrier for ino 2 failed:
//! … block 1290 of inode_2 did not settle after 4 serialized stripe-held
//! settle attempts` once per rank per iteration, and ior prints
//! `WARNING: fsync(15) failed` ×101. The stats close exactly:
//! `seed_settle_escalations` 101 (= the failed fsyncs) × 4 attempts = 404
//! settle losses = 78 `read_settle_stale_head_refetches` + 326 tripwires;
//! `stale_binding_rebinds` 2,424 = 101 × the 24-rung ladder. Every
//! attempt lost; nothing ever served.
//!
//! **The reader** is the authority's `FlushExtents` executor
//! (`SqueezefsFilesystem::flush_shipped_extents` → `flush_inode_to_backend`
//! → the fold of the shipped extents parked for block 1290 →
//! `fetch_seed_image` → `get_block_for_index_stripe_held` → the ladder →
//! the caller-stripe settle arm). No application read runs on the
//! authority (`fuse_ops` 77 for the whole row).
//!
//! **The mechanism** is structural, not a racing mutator. Every tripwired
//! offset is in lane 2 — a co-writer's residue class, one the authority
//! can never mint — and the diagnosis reads `live_incarnation 0,
//! fill(word) None, refcount None, free_listed false, inflight false,
//! quarantined false`: an incarnation WORD EXISTS on the authority and is
//! UNSTABLE (a missing word would read `Some(UNKNOWN_STABLE)`). The only
//! authority-side creator of an unstable word for a never-minted offset
//! is `begin_free`'s retire — the shipped/recomputed free of the offset's
//! PREVIOUS lifetime, which the authority executes for its co-writers.
//! The offset then rode grace → free list → lane harvest → the co-writer
//! minted it again, DMA'd block 1290's new content into it and published
//! the map naming it. The co-writer's DMA-complete `publish_block`
//! stabilizes the CO-WRITER's word; the authority's word for the same
//! offset has no publisher — the authority never claims a foreign-lane
//! offset — so it stays retired for ever. From then on every authority
//! fill of that key fails `fill_incarnation` (None), the ladder loses 24
//! rebinds, the settle arm loses 4 attempts under both locks against a
//! backend-fresh head, and the fsync's `FlushExtents` returns EIO. A
//! rewrite workload recycles every displaced block through exactly this
//! path, so after the first iteration the whole shared file is
//! unreadable and un-foldable from the authority.
//!
//! **The law this file pins**: a SERVED layout publish that adopts a
//! foreign-lane key is the authority's witness that the shipper's device
//! write behind that key completed (a co-writer publishes strictly after
//! its DMA), so the authority stabilizes its own word for the offset at
//! that serve — the local protocol's `publish_block` after DMA, performed
//! by the one node that observes the co-writer's publish. Between the
//! authority's free of the offset and that re-adopting serve the word
//! stays retired (a straggler fill of the DEAD lifetime during the
//! co-writer's DMA must never publish into the authority's tiers), and
//! own-lane words are never touched (the local protocol owns them).
//!
//! What the application saw: every `fsync(2)` on the three co-writers
//! holding retained extents returned **EIO** (ior warned and continued;
//! the row lost 112 MiB of aggregate size and failed the sustained gate).
//! A plain `cat` of the shared file ON THE AUTHORITY would have returned
//! EIO for every recycled block for the same reason — the same
//! validation primitive, the pure-read arm.
//!
//! Venue caveat (stated, not hidden): one process plays both nodes, so
//! the authority's publish service serves over the SAME `KvMetaBackend`
//! the co-writer's fs reads (the `mw_cowriter_free_leak_tests` shape),
//! while the two data planes (allocators, routers, incarnation words) are
//! distinct — exactly the seam the finding lives on.

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
use squeezefs::routing::{BackendRouter, DataRouter};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, TempDir};

const VOL_LEN: u64 = 64 * 1024 * 1024;
/// Sparse data-device backing: the loop writes a handful of blocks.
const DEV_LEN: u64 = 16 * 1024 * 1024 * 1024;
const SECRET: &[u8] = b"s9-authority-recycled-binding-storage-trust-secret";
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
        publish::uninstall_binding_witness();
        // The rung-17 assembler pair + the rung-18 sink an authority fs
        // installs (`install_extent_assembler`) capture THAT fs — left
        // installed they would route a later test's served extents and
        // invalidations to a dropped filesystem.
        publish::uninstall_extent_merge_executor();
        publish::uninstall_extent_flush_executor();
        publish::uninstall_served_layout_invalidation();
        publish::uninstall_served_displacement_sink();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
        free_grace::reset_for_test();
        membership::uninstall();
        squeezefs::meta_ship::tokens::test_clear_range_cache();
        squeezefs::meta_ship::tokens::test_clear_stretch_ceilings();
        squeezefs::dlm::test_clear_range_episodes();
        squeezefs::device_overlay::clear_device_overlay_for_tests();
        fuse_client::set_patch_max_bytes(fuse_client::derived_patch_max_bytes(
            squeezefs::block_allocator::CHUNK_SIZE,
        ));
    }
}

fn restore() -> Restore {
    Restore
}

// ---------------------------------------------------------------------------
// Volumes, allocators, evidence (the mw_cowriter_free_leak_tests fixtures)
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
// ladder executes the shipped frees — the production arm's executors,
// probe, resolver and geometry) and one co-writer with the production
// FUSE write path
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

        let (alloc, br) = data_plane(dev).await;
        fuse_client::set_mount_posture(MountPosture::Writer);
        grant::engage_allocator_lane(&alloc, assignment.authority_partition(), &meta, {
            LaneFloor::Local
        })
        .await
        .expect("the authority engages its own lane");
        // The production arm's data-plane installs, mirrored (the
        // fixture-truth discipline): the shipped-free executor, the lane
        // harvest, the finding-28 binding probe, the finding-51 binding
        // witness, the rung-19 resolver.
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
        {
            let br = Arc::clone(&br);
            publish::install_binding_witness(Arc::new(move |taken: &[BlockRef]| {
                br.witness_served_bindings(taken);
            }));
        }
        {
            let br = Arc::clone(&br);
            squeezefs::meta_backend::kv::block_refs::install_block_ref_resolver(Arc::new(
                move |k: &str, ino: u64, idx: u32| br.block_ref_for(k, ino, idx),
            ));
        }

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
    client: Arc<WriteCustodyClient>,
    alloc: Arc<BlockAllocator>,
    meta: Arc<RoutedMetaBackend>,
    node_id: String,
    endpoint: String,
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
            client,
            alloc,
            meta,
            node_id: node_id.to_string(),
            endpoint: auth.endpoint.clone(),
        }
    }

    /// The one-process venue's posture flip, OUT: the co-writer's
    /// process-global halves stand down so the authority's fs can act as
    /// the plain writer it is (the posture latch, the ownership map and
    /// the client installs are all process-wide). The custody lease, the
    /// allocator and its engaged lane stay — [`Self::rearm`] restores the
    /// halves without re-joining.
    fn stand_down(&self) {
        data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(0, Ordering::Relaxed);
        data_grant::uninstall_custody_client();
        publish::uninstall_client();
        ship::disarm_ownership();
        fuse_client::set_mount_posture(MountPosture::Writer);
    }

    /// The posture flip, IN: the same installs [`Self::join`] performed,
    /// on the same lease.
    fn rearm(&self) {
        fuse_client::set_mount_posture(MountPosture::CoWriter);
        let map = OwnerMap::for_volumes(
            &self.meta,
            vec![(0, PeerOwner::new("mw-authority", self.endpoint.clone()))],
        )
        .expect("an all-foreign owner map");
        ship::arm_ownership(map);
        publish::install_client(publish::PublishClient::new(&self.node_id, SECRET.to_vec()));
        data_grant::install_custody_client(Arc::clone(&self.client));
        data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(1, Ordering::Relaxed);
    }
}

/// A [`SqueezefsFilesystem`] over ONE side's data plane, reading metadata
/// through the SHARED backend (the one-process venue). On the co-writer's
/// plane it is the production write path (every publish ships); on the
/// authority's plane it is the reader under contract.
struct SideFs {
    fs: SqueezefsFilesystem,
    req: Request,
    _stage: TempDir,
}

async fn side_fs(auth: &Authority, alloc: &Arc<BlockAllocator>, dev: &Path) -> SideFs {
    let dlm = DlmClient::new().expect("dlm");
    let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
    let stage = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![stage.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        Arc::clone(alloc),
        Arc::clone(&nvme),
        None,
    )
    .await
    .expect("tiered cache");
    let router = DataRouter::new(dlm.clone(), cache, Arc::clone(alloc), nvme);
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
    SideFs {
        fs,
        req,
        _stage: stage,
    }
}

fn pat(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ tag | 1).collect()
}

/// The pattern round `round` writes into block `b` (one whole block).
fn round_pat(round: u32, b: u64, bs: u64) -> Vec<u8> {
    pat(bs as usize, (round as u8).wrapping_mul(7) ^ (b as u8))
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
        // The router's own naming: bare on an un-engaged router, the
        // `offset@stamp` lifetime form once incarnation keys are engaged
        // (`engage_stamped_keys`) — the fleet's authority keys.
        map.insert(b as u32, auth.br.persist_block_key("backend_0", off));
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

/// Engage spec §6.2 item-6 incarnation keys on the authority's data plane
/// — what `DataRouter::set_meta_backend` does on every armed mount whose
/// volumes carry bit 13: persisted keys name their offset's lifetime
/// (`offset@stamp`), and the read/free paths validate them. The rig's
/// volumes are stamped; the era is the authority's own durable term.
fn engage_stamped_keys(auth: &Authority) {
    let era = auth
        .meta
        .volumes
        .iter()
        .map(|v| v.writer_term())
        .max()
        .unwrap_or(0);
    assert!(era > 0, "the authority's D0 claim minted a durable term");
    auth.br
        .engage_incarnation_keys(
            era,
            squeezefs::meta_backend::kv::journal::AppendPartition::SOLO,
        )
        .expect("incarnation keys engage");
}

/// Every key the durable layout of `ino` names, as `(index, key)`.
async fn durable_keys(auth: &Authority, ino: u64) -> Vec<(u32, String)> {
    use squeezefs::meta_backend::Metadata as _;
    let bytes = auth
        .meta
        .getxattr(ino, "layout")
        .await
        .expect("layout read")
        .expect("a layout exists");
    let layout = squeezefs::layout_wire::decode_layout_any(&bytes).expect("a bincode layout");
    let mut out: Vec<(u32, String)> = match layout
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
    out.sort_unstable_by_key(|&(b, _)| b);
    out
}

/// Every block the durable layout of `ino` names, as `(index, block_idx)`.
async fn durable_blocks(auth: &Authority, ino: u64) -> Vec<(u32, u64)> {
    let chunk = auth.alloc.chunk_size();
    let mut out: Vec<(u32, u64)> = durable_keys(auth, ino)
        .await
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

/// The authority-side counters a losing read moves: the RES-22 tripwire,
/// the two settle-arm engagement gauges, and the ladder's rebind count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadLedger {
    tripwires: u64,
    settle_escalations: u64,
    seed_settle_escalations: u64,
    stale_head_refetches: u64,
    rebinds: u64,
}

fn read_ledger() -> ReadLedger {
    ReadLedger {
        tripwires: METRICS.invariant_tripwires.load(Ordering::Relaxed),
        settle_escalations: METRICS.stale_binding_escalations.load(Ordering::Relaxed),
        seed_settle_escalations: METRICS.seed_settle_escalations.load(Ordering::Relaxed),
        stale_head_refetches: METRICS
            .read_settle_stale_head_refetches
            .load(Ordering::Relaxed),
        rebinds: METRICS.stale_binding_rebinds.load(Ordering::Relaxed),
    }
}

/// The authority reads block `b` of `ino` through the validated read path
/// in the PURE-READ posture (`escalate_contended`) — the same
/// `fetch_block_device_true` verdict and the same
/// `settled_resolve_fetch_locked` interior the fold's stripe-held seed
/// fetch runs; `device_true` keeps the fixture router's tiers out of the
/// verdict (in production ONE router owns tiers and words; this rig's
/// authority reader is a second router over the shared allocator).
async fn authority_read(auth_fs: &SideFs, ino: u64, b: u64) -> Result<Vec<u8>, String> {
    auth_fs.fs.router.discard_layout_cache(ino);
    auth_fs
        .fs
        .router
        .get_block_for_index(&format!("inode_{ino}"), b as u32, None, true, true)
        .await
        .map_err(|e| format!("{e:?}"))
        .and_then(|v| v.map(|v| v.to_vec()).ok_or_else(|| "a hole".to_string()))
}

// ===========================================================================
// 1. The fleet shape: a range-custody rewrite loop, read by the authority
// ===========================================================================

/// Contract (finding 51): after a co-writer's range-custody rewrite loop
/// has run past its lane share — so the rewritten range sits on offsets
/// the AUTHORITY freed (displaced lifetimes, executed through its own
/// `begin_free`) and the co-writer re-minted through the lane harvest — an
/// authority read of every block in the range serves the LAST round's
/// bytes on the ladder's FIRST attempt: `invariant_tripwires`,
/// `stale_binding_escalations`, `read_settle_stale_head_refetches` and
/// `stale_binding_rebinds` all unchanged. Word-level: every foreign-lane
/// key the durable head names has a STABLE word on the authority, and
/// every displaced lifetime the authority freed reads UNSTABLE until the
/// offset is re-adopted (the dead-lifetime protection is kept, not
/// traded away).
///
/// RED against dev `2a486273`: the first round on harvested offsets fails
/// `fill_incarnation` for every recycled key — the authority's word was
/// retired by its own free of the previous lifetime and nothing on the
/// authority ever re-publishes it — so the read burns 24 rebinds, 4
/// serialized settle losses (the tripwire) and returns the "did not
/// settle" EIO.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authority_read_of_a_recycled_co_writer_block_validates_first_try() {
    let _serial = serial();
    let _restore = restore();
    fuse_client::set_patch_max_bytes(0);
    let dir = TempDir::new().unwrap();
    let vol = fresh_volume(dir.path(), "f51-read").await;
    let dev = data_device(dir.path(), "f51-read.dev");

    // Geometry: 8 blocks; the co-writer rewrites blocks 2..6 (its range)
    // each round.
    const BLOCKS: u64 = 8;
    const RANGE: std::ops::Range<u64> = 2..6;
    const ROUNDS: u32 = 10;
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    let bs = auth.alloc.chunk_size();
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(BLOCKS * bs, bs));
    // The production arm's blob-aware compose hook (rung 20): without it
    // an indirect head REFUSES every shipped publish.
    squeezefs::meta_backend::kv::indirect_map::install_indirect_map_io(
        squeezefs::multi_writer::indirect_map_io_for(Arc::clone(&auth.br)),
    );
    // A small store so the loop crosses the lane share: the seed plus 32
    // blocks, W = 2 ⇒ the co-writer's lane holds ~24 fresh mints; four
    // blocks per round means every round past ~4 runs on HARVESTED
    // offsets — the recycled population the finding lives on.
    let cap_blocks: u64 = 2 * BLOCKS + 32;
    auth.alloc.set_capacity_bytes(cap_blocks * bs);
    let ino = seed_striped_file(&auth, "shared.bin", BLOCKS).await;

    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    cwr.alloc.set_capacity_bytes(cap_blocks * bs);
    let lane_id = u64::from(cwr.client.lane_partition().writer_id());
    let w = side_fs(&auth, &cwr.alloc, &dev).await;
    // The authority's reader: its own data plane (the allocator whose
    // words the shipped frees retire), the shared backend's head.
    let r = side_fs(&auth, &auth.alloc, &dev).await;
    data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(1, Ordering::Relaxed);

    let fh =
        w.fs.open(w.req, ino, libc::O_WRONLY as u32, 0)
            .await
            .expect("the co-writer opens the shared file")
            .fh;
    let witnesses_before = METRICS.served_binding_witnesses.load(Ordering::Relaxed);
    let mut ever_freed: BTreeSet<u64> = BTreeSet::new();
    let mut recycled_reads = 0u32;
    let mut prev: Vec<(u32, u64)> = durable_blocks(&auth, ino).await;
    for round in 0..ROUNDS {
        for b in RANGE {
            let data = round_pat(round, b, bs);
            let wr =
                w.fs.write(w.req, ino, fh, b * bs, bytes::Bytes::from(data), 0, 0)
                    .await
                    .unwrap_or_else(|e| panic!("round {round} block {b}: write failed: {e:?}"));
            assert_eq!(
                wr.written as u64, bs,
                "round {round} block {b}: short write"
            );
        }
        w.fs.fsync(w.req, ino, fh, false)
            .await
            .unwrap_or_else(|e| panic!("round {round}: the co-writer's fsync failed: {e:?}"));
        // The displaced offsets' frees ran on the authority's ladder; the
        // fixture's reclaimer is the authority's.
        auth.br.reclaim_drain().await;

        let cur = durable_blocks(&auth, ino).await;
        assert_eq!(
            cur.len(),
            BLOCKS as usize,
            "round {round}: the layout names every block"
        );
        let cur_idx: BTreeSet<u64> = cur.iter().map(|&(_, idx)| idx).collect();
        // Every displaced lifetime the authority freed this round reads
        // UNSTABLE on the authority until re-adopted — the straggler-fill
        // protection the finding's fix must not trade away.
        for &(b, idx) in &prev {
            if RANGE.contains(&u64::from(b)) && !cur_idx.contains(&idx) {
                ever_freed.insert(idx);
                assert_eq!(
                    auth.alloc.fill_incarnation(idx * bs),
                    None,
                    "round {round}: displaced block {b} (idx {idx}) — the authority freed this \
                     lifetime; its word must read retired until a served publish re-adopts \
                     the offset"
                );
            }
        }
        // The authority READS the range (the symptom): first-attempt
        // serves of the round's bytes, no ladder loss, no settle, no
        // tripwire.
        let before = read_ledger();
        for b in RANGE {
            let idx = cur
                .iter()
                .find(|&&(bb, _)| u64::from(bb) == b)
                .map(|&(_, idx)| idx)
                .expect("the head names the block");
            let got = authority_read(&r, ino, b).await.unwrap_or_else(|e| {
                panic!(
                    "round {round}: the authority's read of block {b} (idx {idx}, recycled = \
                     {}) failed: {e} (ledger delta: {:?} → {:?})",
                    ever_freed.contains(&idx),
                    before,
                    read_ledger()
                )
            });
            assert!(
                got == round_pat(round, b, bs),
                "round {round}: block {b} served bytes that are not this round's pattern"
            );
        }
        let after = read_ledger();
        assert_eq!(
            after, before,
            "round {round}: the authority's reads of the range moved a loss counter"
        );
        // The cause, word-level: every foreign-lane key the head names is a
        // completed co-writer write whose publish the authority SERVED,
        // so its word must be stable here.
        for &(b, idx) in &cur {
            if RANGE.contains(&u64::from(b)) {
                assert_eq!(
                    lane::block_lane_of(idx, 2),
                    lane_id,
                    "round {round}: block {b} of the rewritten range is a co-writer mint"
                );
                assert!(
                    auth.alloc.fill_incarnation(idx * bs).is_some(),
                    "round {round}: block {b} (idx {idx}) — the durable head names this \
                     co-writer key, so the authority's word must be STABLE (the served \
                     publish is its DMA witness); recycled = {}",
                    ever_freed.contains(&idx)
                );
                if ever_freed.contains(&idx) {
                    recycled_reads += 1;
                }
            }
        }
        prev = cur;
    }
    assert!(
        recycled_reads > 0,
        "the loop never reached the recycled population — the rig no longer crosses the \
         lane share (harvest_served_blocks {})",
        publish::stats().harvest_served_blocks
    );
    // Engagement: every served publish's adopted foreign-lane block is one
    // witness — four per round, fresh mints (a first STABLE word) and
    // recycled offsets (the retired word re-published) alike.
    let witnesses = METRICS.served_binding_witnesses.load(Ordering::Relaxed) - witnesses_before;
    assert_eq!(
        witnesses,
        u64::from(ROUNDS) * (RANGE.end - RANGE.start),
        "served_binding_witnesses accounts for every foreign-lane block the served \
         publishes adopted"
    );
    eprintln!(
        "finding 51 loop: {ROUNDS} rounds, {} recycled-block serves, {} offsets ever freed by \
         the authority, harvest served {}, binding witnesses {witnesses}",
        recycled_reads,
        ever_freed.len(),
        publish::stats().harvest_served_blocks
    );

    // A second publish naming the SAME (unchanged) bindings — a full Put
    // re-stating the co-writer's map, the file-per-proc close shape — is
    // served without a stale-binding refusal, and the authority still
    // serves every recycled key first-try. (Pinned green from birth: the
    // witness publishes the seqlock WORD only; the lifetime stamp
    // `incarnation_ok` compares is a separate field it never touches, so
    // a re-stated key can never disagree with itself.)
    let refusals_before = METRICS
        .block_key_incarnation_refusals
        .load(Ordering::Relaxed);
    let path = format!("inode_{ino}");
    let mut entry =
        w.fs.router
            .fetch_metadata(&path)
            .await
            .expect("the co-writer resolves the head");
    entry.layout_dirty = true;
    w.fs.router.metadata_cache.insert(ino, entry);
    let tok = w.fs.dlm().get_fencing_token_ino(ino);
    w.fs.router
        .persist_dirty_layout_if_needed(&path, tok)
        .await
        .expect("the re-stating Put ships and is served");
    let before = read_ledger();
    for b in RANGE {
        let got = authority_read(&r, ino, b)
            .await
            .unwrap_or_else(|e| panic!("post-restate read of block {b} failed: {e}"));
        assert!(
            got == round_pat(ROUNDS - 1, b, bs),
            "block {b}: not the last round's bytes"
        );
    }
    assert_eq!(
        read_ledger(),
        before,
        "the re-stated publish moved a loss counter"
    );
    assert_eq!(
        METRICS
            .block_key_incarnation_refusals
            .load(Ordering::Relaxed)
            - refusals_before,
        0,
        "re-stating the same keys is never a stale-binding refusal"
    );

    // The DEFAULT read path (tiers engaged) on a FRESH authority reader —
    // the app-visible face of a `cat` on the authority — serves the last
    // round's bytes too.
    let fresh = side_fs(&auth, &auth.alloc, &dev).await;
    let before = read_ledger();
    for b in RANGE {
        let got = fresh
            .fs
            .router
            .get_block_for_index(&format!("inode_{ino}"), b as u32, None, false, true)
            .await
            .unwrap_or_else(|e| panic!("default-path read of block {b} failed: {e:?}"))
            .unwrap_or_else(|| panic!("default-path read of block {b}: a hole"));
        assert!(
            got.as_ref() == round_pat(ROUNDS - 1, b, bs).as_slice(),
            "default-path read of block {b}: not the last round's bytes"
        );
    }
    assert_eq!(
        read_ledger(),
        before,
        "the default-path reads moved a loss counter"
    );

    w.fs.release(w.req, ino, fh, 0, 0, false)
        .await
        .expect("release");
    assert!(
        w.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the co-writer's pipeline drains"
    );
    drop(fresh);
    drop(r);
    drop(w);
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 2. The witness's two edges: own-lane words untouched, retire kept
// ===========================================================================

/// Contract (finding 51's edges, pinned green): the binding witness
/// publishes words for FOREIGN-lane blocks only. An authority-lane offset
/// the authority freed (its word retired by the local protocol) stays
/// retired when a served frame names it — the local claim → DMA → publish
/// protocol owns own-lane words, and a peer's take on one is a clone of an
/// already-stable block or a stale view the compose dropped, never a DMA
/// this witness may vouch for. An unpartitioned allocator (a solo mount)
/// owns every lane and publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binding_witness_never_publishes_an_own_lane_or_solo_word() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let dev = data_device(dir.path(), "f51-edges.dev");
    let tag = volume_tag(DATA_VOL);
    let reference = |block_idx: u64| BlockRef {
        vol_tag: tag,
        block_idx,
        owner_ino: 2,
        block_index: 0,
    };

    // A SOLO allocator (no partition): every lane is its own.
    let (solo, solo_br) = data_plane(&dev).await;
    let bs = solo.chunk_size();
    let solo_off = solo.allocate_block().await.expect("solo mint");
    assert_eq!(
        solo.fill_incarnation(solo_off),
        None,
        "a claimed, un-published offset reads unstable"
    );
    assert_eq!(
        solo_br.witness_served_bindings(&[reference(solo_off / bs)]),
        0,
        "a solo allocator owns every lane — the witness publishes nothing"
    );
    assert_eq!(
        solo.fill_incarnation(solo_off),
        None,
        "the solo word is untouched"
    );

    // A PARTITIONED authority allocator (W = 2, lane 0): its own lane's
    // retired word stays retired; a foreign-lane block's word publishes.
    let (auth_alloc, auth_br) = data_plane(&dev).await;
    auth_alloc
        .engage_alloc_lanes(
            squeezefs::meta_backend::kv::journal::AppendPartition::new(2, 0).expect("partition"),
        )
        .expect("engage lane 0");
    let own_off = auth_alloc.allocate_block().await.expect("own-lane mint");
    assert_eq!(lane::block_lane_of(own_off / bs, 2), 0);
    assert_eq!(auth_alloc.fill_incarnation(own_off), None);
    let witnesses_before = METRICS.served_binding_witnesses.load(Ordering::Relaxed);
    assert_eq!(
        auth_br.witness_served_bindings(&[reference(own_off / bs)]),
        0,
        "an own-lane reference is the local protocol's — never witnessed"
    );
    assert_eq!(
        auth_alloc.fill_incarnation(own_off),
        None,
        "the own-lane word stays retired"
    );
    // A foreign-lane offset the authority never saw: the witness records
    // its first STABLE word (a fresh co-writer mint's publish).
    let foreign_idx = (own_off / bs) + 1;
    assert_eq!(lane::block_lane_of(foreign_idx, 2), 1);
    // A never-seen offset reads unknown-stable (§6.3's honest degradation:
    // the `u64::MAX` sentinel, no recorded word).
    assert_eq!(
        auth_alloc.fill_incarnation(foreign_idx * bs),
        Some(u64::MAX),
        "a never-seen offset reads unknown-stable"
    );
    assert_eq!(
        auth_br.witness_served_bindings(&[reference(foreign_idx)]),
        1,
        "a foreign-lane reference is witnessed"
    );
    assert!(
        auth_alloc
            .fill_incarnation(foreign_idx * bs)
            .is_some_and(|w| w != u64::MAX),
        "the foreign-lane word is now a recorded STABLE word"
    );
    // The retire edge: the authority's free of that offset (the executor's
    // `begin_free`) retires the word; the NEXT witness re-publishes it.
    assert!(auth_alloc.seed_shipped_free_reference(foreign_idx * bs));
    assert!(auth_alloc.begin_free(foreign_idx * bs), "terminal free");
    assert_eq!(
        auth_alloc.fill_incarnation(foreign_idx * bs),
        None,
        "the freed lifetime's word is retired — a straggler fill of the dead binding \
         must not publish"
    );
    assert_eq!(
        auth_br.witness_served_bindings(&[reference(foreign_idx)]),
        1
    );
    assert!(
        auth_alloc.fill_incarnation(foreign_idx * bs).is_some(),
        "the re-adopting serve re-publishes the word under its new generation"
    );
    assert_eq!(
        METRICS.served_binding_witnesses.load(Ordering::Relaxed) - witnesses_before,
        2,
        "the gauge counts the two foreign-lane publishes and neither own-lane refusal"
    );
    auth_alloc.finish_free(foreign_idx * bs);
}

// ===========================================================================
// 3. Phase B1 (2026-09-07): a served publish displacing the captured old
//    binding of an OPEN authority overlay record
// ===========================================================================

/// One phase-B1 scenario, built to the moment of truth: the authority
/// holds an OPEN device-overlay record for block `B` of a striped file
/// (an overwrite record whose captured `old_binding` is the authority's
/// own STAMPED lane-0 key — the fleet's assembler shape); a co-writer's
/// whole-block write of `B` is served (the owner's recompute frees the
/// displaced authority block through the authority's own ladder); the
/// authority re-mints the freed offset (free-list-first), so the captured
/// key now names a DEAD lifetime of its offset. The co-writer side is torn
/// down before the authority acts (the process posture latch is global).
struct DisplacedOverlay {
    auth: Authority,
    a: SideFs,
    ino: u64,
    bs: u64,
    old_key: String,
    old_off: u64,
    cw_pattern: Vec<u8>,
    open_before: u64,
    refusals_before: u64,
    tripwires_before: u64,
    screened_before: u64,
    belt_before: u64,
}

/// The victim block and the overlay slice (a 64 KiB page-aligned segment
/// in the block's interior — gaps on both sides, so the settle MUST seed
/// from the captured old binding).
const OV_BLOCK: u64 = 3;
const OV_REL: usize = 1024 * 1024;
const OV_LEN: usize = 64 * 1024;

async fn displaced_overlay(dir: &Path, tag: &str, install_assembler: bool) -> DisplacedOverlay {
    const BLOCKS: u64 = 8;
    let vol = fresh_volume(dir, tag).await;
    let dev = data_device(dir, &format!("{tag}.dev"));
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    engage_stamped_keys(&auth);
    let bs = auth.alloc.chunk_size();
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(BLOCKS * bs, bs));
    squeezefs::meta_backend::kv::indirect_map::install_indirect_map_io(
        squeezefs::multi_writer::indirect_map_io_for(Arc::clone(&auth.br)),
    );
    let ino = seed_striped_file(&auth, "shared.bin", BLOCKS).await;
    let (old_key, old_off) = {
        let keys = durable_keys(&auth, ino).await;
        let key = keys
            .iter()
            .find(|&&(b, _)| u64::from(b) == OV_BLOCK)
            .map(|(_, k)| k.clone())
            .expect("the seed names the victim block");
        let parts = auth
            .br
            .parse_block_key_parts(&squeezefs::routing::clean_block_key(&key))
            .expect("a stamped authority key");
        assert!(
            key.contains('@') && parts.incarnation != 0,
            "premise: the authority's keys name their lifetime (key {key})"
        );
        assert_eq!(
            auth.alloc.live_incarnation(parts.offset),
            parts.incarnation,
            "premise: the seed key IS the offset's live lifetime"
        );
        assert_eq!(
            lane::block_lane_of(parts.offset / bs, 2),
            0,
            "an authority-lane block"
        );
        (key, parts.offset)
    };

    // The authority's fs (its reader AND its overlay holder), with the
    // production assembler installs when the scenario asks for them —
    // the rung-17 pair, the finding-28 probe, the rung-18 invalidation
    // sink, and the served-displacement screen this file pins.
    let a = side_fs(&auth, &auth.alloc, &dev).await;
    if install_assembler {
        a.fs.install_extent_assembler();
    }
    a.fs.router
        .fetch_metadata(&format!("inode_{ino}"))
        .await
        .expect("the authority fs resolves the seeded layout");
    let open_before = METRICS.overlay_open.load(Ordering::Relaxed);
    let refusals_before = METRICS
        .block_key_incarnation_refusals
        .load(Ordering::Relaxed);
    let tripwires_before = METRICS.invariant_tripwires.load(Ordering::Relaxed);
    let screened_before = METRICS
        .overlay_superseded_by_served_publish
        .load(Ordering::Relaxed);
    let belt_before = METRICS
        .overlay_superseded_dead_old_binding
        .load(Ordering::Relaxed);
    // The open overwrite record (the assembler's shipped-slice shape),
    // capturing the seed key as its old binding.
    a.fs.test_install_overwrite_overlay(ino, OV_BLOCK as u32, OV_REL, &pat(OV_LEN, 0xA5))
        .await
        .expect("the overwrite record installs");
    assert_eq!(
        METRICS.overlay_open.load(Ordering::Relaxed),
        open_before + 1,
        "premise: one open overlay record"
    );

    // The co-writer's whole-block write of the victim block, served.
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    let w = side_fs(&auth, &cwr.alloc, &dev).await;
    data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(1, Ordering::Relaxed);
    let cw_pattern = pat(bs as usize, 0x5C);
    let fh =
        w.fs.open(w.req, ino, libc::O_WRONLY as u32, 0)
            .await
            .expect("the co-writer opens the shared file")
            .fh;
    let wr =
        w.fs.write(
            w.req,
            ino,
            fh,
            OV_BLOCK * bs,
            bytes::Bytes::from(cw_pattern.clone()),
            0,
            0,
        )
        .await
        .expect("the co-writer's whole-block write");
    assert_eq!(wr.written as u64, bs);
    w.fs.fsync(w.req, ino, fh, false)
        .await
        .expect("the co-writer's fsync ships the publish");
    w.fs.release(w.req, ino, fh, 0, 0, false)
        .await
        .expect("release");
    assert!(
        w.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the co-writer's pipeline drains"
    );
    auth.br.reclaim_drain().await;
    // The durable head names the co-writer's block; the authority's own
    // block was displaced and freed through its ladder (word retired,
    // offset back on the lane-0 free list).
    let cur = durable_keys(&auth, ino).await;
    let cur_key = cur
        .iter()
        .find(|&&(b, _)| u64::from(b) == OV_BLOCK)
        .map(|(_, k)| k.clone())
        .expect("block still named");
    assert_ne!(
        cur_key, old_key,
        "the served publish displaced the authority's block"
    );
    assert_eq!(
        auth.alloc.refcount(old_off),
        None,
        "the recompute freed the displaced authority block"
    );
    // The authority re-mints the freed offset: the captured old binding
    // now names a DEAD lifetime (the fleet: live stamp > key stamp, same
    // era, all ten offsets lane 0). Free-list-first hands back the lowest
    // free index — a superseded record's freed dest may sit beside the
    // displaced block, so mint until the victim offset comes round.
    let mut reminted = false;
    for _ in 0..8 {
        if auth
            .alloc
            .allocate_block()
            .await
            .expect("the authority mints")
            == old_off
        {
            reminted = true;
            break;
        }
    }
    assert!(
        reminted,
        "the freed offset came back through the authority's own mint"
    );
    assert!(
        !auth.br.block_key_incarnation_ok(&old_key),
        "premise: the captured old binding is a dead lifetime now"
    );
    // The premise probe itself counted one refusal — rebase the ledger.
    let refusals_before = refusals_before.max(
        METRICS
            .block_key_incarnation_refusals
            .load(Ordering::Relaxed),
    );

    // The co-writer side leaves: the authority acts as the plain writer
    // it is (the posture latch and the ownership map are process-global).
    drop(w);
    drop(cwr);
    data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(0, Ordering::Relaxed);
    data_grant::uninstall_custody_client();
    publish::uninstall_client();
    ship::disarm_ownership();
    fuse_client::set_mount_posture(MountPosture::Writer);

    DisplacedOverlay {
        auth,
        a,
        ino,
        bs,
        old_key,
        old_off,
        cw_pattern,
        open_before,
        refusals_before,
        tripwires_before,
        screened_before,
        belt_before,
    }
}

/// Bounded wait for the detached overlay retire to converge the gauge.
async fn wait_overlay_open(target: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while METRICS.overlay_open.load(Ordering::Relaxed) > target
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        METRICS.overlay_open.load(Ordering::Relaxed),
        target,
        "the superseded record's detached retire converges overlay_open"
    );
}

/// Contract (phase B1, the screen): a SERVED layout publish that displaces
/// block `b`'s binding is a foreign durable `Merge` on an open-overlay
/// index — design-overlay-overwrite §5.7's containment applies (the
/// durable map is the authority the moment the commit lands; the record is
/// superseded and retired), performed by the authority's served-publish
/// screen at the commit: the record's captured old binding is never read
/// again, the gap read composes from the durable head (the co-writer's
/// bytes), the authority's fsync settles nothing and returns, and
/// `block_key_incarnation_refusals` stays flat. The served publisher is a
/// LEGAL peer, so `invariant_tripwires` stays flat too (the
/// `overlay_foreign_merge` tripwire is for code-path escapes).
///
/// RED on dev `a7cab076` (the B1 row's shape, 28,039 refusals on 10 wedged
/// records): the record survives the displacement, its settle reads the
/// dead old binding — `STALE BLOCK-KEY BINDING refused` — and the fsync's
/// read-venue settle exhausts 24 attempts into EIO (the write arm retries
/// for ever at 50 ms; the gap read EIOs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_publish_displacing_an_open_overlays_old_binding_supersedes_it_at_the_commit() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let sc = displaced_overlay(dir.path(), "f51-b1-screen", true).await;
    let path = format!("inode_{}", sc.ino);

    // A gap read (block start — outside the overlay's slice) composes
    // from the durable head: the co-writer's bytes, no dead-key serve.
    let got =
        sc.a.fs
            .read(sc.a.req, sc.ino, 0, OV_BLOCK * sc.bs, 4096, 0)
            .await
            .unwrap_or_else(|e| panic!("the gap read on the authority failed: {e:?}"))
            .data
            .to_vec();
    assert_eq!(
        got,
        &sc.cw_pattern[..4096],
        "the gap read serves the durable authority's bytes (the served publish)"
    );
    // The fsync finds no live record to settle.
    sc.a.fs
        .fsync(sc.a.req, sc.ino, 0, false)
        .await
        .unwrap_or_else(|e| panic!("the authority's fsync failed: {e:?}"));
    wait_overlay_open(sc.open_before).await;
    assert_eq!(
        METRICS
            .block_key_incarnation_refusals
            .load(Ordering::Relaxed)
            - sc.refusals_before,
        0,
        "the captured old binding is never read: no STALE BLOCK-KEY BINDING refusal"
    );
    assert_eq!(
        METRICS.invariant_tripwires.load(Ordering::Relaxed) - sc.tripwires_before,
        0,
        "a served publish is a legal foreign publisher — no one-authority-screen tripwire"
    );
    assert_eq!(
        METRICS
            .overlay_superseded_by_served_publish
            .load(Ordering::Relaxed)
            - sc.screened_before,
        1,
        "the screen counted exactly the one containment"
    );
    assert_eq!(
        METRICS
            .overlay_superseded_dead_old_binding
            .load(Ordering::Relaxed)
            - sc.belt_before,
        0,
        "the belt never had to fire — the screen caught the displacement at the commit"
    );
    // The whole block reads as the co-writer's durable content.
    let got =
        sc.a.fs
            .read(sc.a.req, sc.ino, 0, OV_BLOCK * sc.bs, sc.bs as u32, 0)
            .await
            .expect("whole-block read")
            .data
            .to_vec();
    assert!(
        got == sc.cw_pattern,
        "block {OV_BLOCK} is the durable authority's"
    );
    let _ = (&path, sc.old_off, &sc.old_key);
    drop(sc.a);
    sc.auth.stop().await;
}

/// Contract (phase B1, the belt): with NO served-publish screen installed
/// (a served path the screen missed, or an install that captured a
/// pre-commit RAM binding a moment before the sink invalidated it), the
/// settle that finds its captured old binding DEAD supersedes the record
/// instead of retrying the dead key — the fsync returns, the record
/// retires, and the block reads as the durable authority's. The one
/// refusal is the detection itself (the belt's own gauge names it).
///
/// RED on dev `a7cab076`: the read-venue settle exhausts 24 attempts into
/// EIO, +24 refusals.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settle_whose_captured_old_binding_died_supersedes_instead_of_retrying_the_dead_key() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let sc = displaced_overlay(dir.path(), "f51-b1-belt", false).await;

    assert_eq!(
        METRICS.overlay_open.load(Ordering::Relaxed),
        sc.open_before + 1,
        "premise: with no screen installed the record survived the served publish"
    );
    sc.a.fs
        .fsync(sc.a.req, sc.ino, 0, false)
        .await
        .unwrap_or_else(|e| panic!("the authority's fsync failed: {e:?}"));
    wait_overlay_open(sc.open_before).await;
    assert_eq!(
        METRICS
            .overlay_superseded_dead_old_binding
            .load(Ordering::Relaxed)
            - sc.belt_before,
        1,
        "the belt superseded the record whose capture died"
    );
    assert_eq!(
        METRICS
            .overlay_superseded_by_served_publish
            .load(Ordering::Relaxed)
            - sc.screened_before,
        0,
        "no screen was installed on this authority"
    );
    assert_eq!(
        METRICS
            .block_key_incarnation_refusals
            .load(Ordering::Relaxed)
            - sc.refusals_before,
        1,
        "exactly ONE refusal — the detection — never the 24-attempt storm"
    );
    assert_eq!(
        METRICS.invariant_tripwires.load(Ordering::Relaxed) - sc.tripwires_before,
        0
    );
    // This scenario installed NO served-publish sinks at all, so the
    // rung-18 invalidation never ran either: drop the fs's stale RAM head
    // by hand (in production the two sinks are installed together).
    sc.a.fs.router.discard_layout_cache(sc.ino);
    let got =
        sc.a.fs
            .read(sc.a.req, sc.ino, 0, OV_BLOCK * sc.bs, sc.bs as u32, 0)
            .await
            .expect("whole-block read")
            .data
            .to_vec();
    assert!(
        got == sc.cw_pattern,
        "block {OV_BLOCK} is the durable authority's"
    );
    let _ = (sc.old_off, &sc.old_key);
    drop(sc.a);
    sc.auth.stop().await;
}

// ===========================================================================
// 4. The fpp anomaly population (2026-09-07, `.benchmarks/2026-09-07-
//    cowriter-claim-anomaly-population.md`): the authority's ASSEMBLY of a
//    co-writer's shipped extent displaces the co-writer's own block through
//    the authority's LOCAL publish — a free no reply to the co-writer names
// ===========================================================================

/// The fleet shape to the moment of truth: the co-writer owns whole blocks
/// `victims` of a striped file (its mints are the durable head, tracked
/// `Some(1)` locally); the authority assembles one shipped slice of each
/// (`assemble_shipped_extent`, the executor `install_extent_assembler`
/// wires) and its fsync force folds and publishes (`flush_shipped_extents`
/// — the co-writer's `FlushExtents`, or the authority's own background
/// fold on the fleet): the fold mints an authority block per victim and
/// the authority's LOCAL recompute frees the co-writer's block through its
/// own ladder. The co-writer side stands down while the authority acts
/// (the process posture latch is global) and is re-armed on return.
struct AssemblerFold {
    auth: Authority,
    a: SideFs,
    cwr: CoWriter,
    w: SideFs,
    ino: u64,
    fh: u64,
    bs: u64,
    cap_blocks: u64,
    /// `(block index, lane block idx)` of the co-writer's displaced mints.
    victims: Vec<(u32, u64)>,
}

const AF_BLOCKS: u64 = 8;

async fn assembler_fold(dir: &Path, tag: &str, victims: &[u64]) -> AssemblerFold {
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    fuse_client::set_patch_max_bytes(0);
    // The write-through pipeline is the co-writer's vehicle (deterministic
    // in-process); the authority's slice parks in the W2 extent overlay.
    squeezefs::device_overlay::set_overlay_overwrite_for_tests(false);
    let vol = fresh_volume(dir, tag).await;
    let dev = data_device(dir, &format!("{tag}.dev"));
    let auth = Authority::start(&vol, &dev, &[NODE_A]).await;
    engage_stamped_keys(&auth);
    let bs = auth.alloc.chunk_size();
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(AF_BLOCKS * bs, bs));
    squeezefs::meta_backend::kv::indirect_map::install_indirect_map_io(
        squeezefs::multi_writer::indirect_map_io_for(Arc::clone(&auth.br)),
    );
    let cap_blocks: u64 = 2 * AF_BLOCKS + 32;
    auth.alloc.set_capacity_bytes(cap_blocks * bs);
    let ino = seed_striped_file(&auth, "shared.bin", AF_BLOCKS).await;
    let a = side_fs(&auth, &auth.alloc, &dev).await;
    a.fs.install_extent_assembler();

    // The co-writer's whole-block writes of the victims: its mints become
    // the durable head at those indices.
    let cwr = CoWriter::join(&auth, &vol, &dev, NODE_A).await;
    cwr.alloc.set_capacity_bytes(cap_blocks * bs);
    let lane_id = u64::from(cwr.client.lane_partition().writer_id());
    let w = side_fs(&auth, &cwr.alloc, &dev).await;
    data_grant::TEST_RANGE_CUSTODY_OVERRIDE.store(1, Ordering::Relaxed);
    let fh =
        w.fs.open(w.req, ino, libc::O_WRONLY as u32, 0)
            .await
            .expect("the co-writer opens the shared file")
            .fh;
    for &b in victims {
        let wr =
            w.fs.write(
                w.req,
                ino,
                fh,
                b * bs,
                bytes::Bytes::from(round_pat(0, b, bs)),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("block {b}: write failed: {e:?}"));
        assert_eq!(wr.written as u64, bs, "block {b}: short write");
    }
    w.fs.fsync(w.req, ino, fh, false)
        .await
        .expect("the co-writer's fsync ships the publish");
    assert!(
        w.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the co-writer's pipeline drains"
    );
    auth.br.reclaim_drain().await;
    let victim_blocks: Vec<(u32, u64)> = durable_blocks(&auth, ino)
        .await
        .into_iter()
        .filter(|(b, _)| victims.contains(&u64::from(*b)))
        .collect();
    assert_eq!(
        victim_blocks.len(),
        victims.len(),
        "premise: every victim is durably mapped"
    );
    for (b, idx) in &victim_blocks {
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

    // The authority acts: one 64 KiB slice of each victim assembled (the
    // shipped-extent shape), then the fsync force folds + publishes —
    // its LOCAL recompute displaces and frees the co-writer's blocks.
    cwr.stand_down();
    let recomputed_before = publish::stats().free_recomputed_blocks;
    for &b in victims {
        a.fs.assemble_shipped_extent(ino, b, OV_REL as u32, bytes::Bytes::from(pat(OV_LEN, 0xA5)))
            .await
            .expect("the authority assembles the shipped slice");
    }
    a.fs.flush_shipped_extents(ino)
        .await
        .expect("the fsync force folds and publishes the assembly");
    auth.br.reclaim_drain().await;
    assert_eq!(
        publish::stats().free_recomputed_blocks - recomputed_before,
        victims.len() as u64,
        "the authority freed each displaced co-writer block by its LOCAL recompute"
    );
    let now = durable_blocks(&auth, ino).await;
    for (b, idx) in &victim_blocks {
        let cur = now
            .iter()
            .find(|(bb, _)| bb == b)
            .map(|(_, i)| *i)
            .expect("block still named");
        assert_ne!(
            cur, *idx,
            "block {b}: the fold displaced the co-writer's mint {idx}"
        );
        assert_eq!(
            lane::block_lane_of(cur, 2),
            0,
            "block {b}: the fold's block is the authority's own"
        );
        assert!(
            auth.free_listed(*idx),
            "block {b}: the co-writer's mint {idx} is on the authority's free list"
        );
        assert_eq!(
            auth.population(*idx).await,
            0,
            "block {b}: the co-writer's mint {idx} holds no durable reference"
        );
        // THE FLEET'S STATE: nothing told the co-writer — its entry lingers.
        assert_eq!(
            cwr.alloc.refcount(idx * bs),
            Some(1),
            "premise: the co-writer still tracks {idx} — no reply named this free"
        );
    }
    cwr.rearm();
    AssemblerFold {
        auth,
        a,
        cwr,
        w,
        ino,
        fh,
        bs,
        cap_blocks,
        victims: victim_blocks,
    }
}

impl Authority {
    async fn population(&self, block_idx: u64) -> usize {
        cowriter::durable_block_refcount(&self.meta, volume_tag(DATA_VOL), block_idx)
            .await
            .expect("the ledger answers")
    }

    fn free_listed(&self, block_idx: u64) -> bool {
        self.alloc.free_block_indices().contains(&block_idx)
    }
}

/// Drain the co-writer's lane through the allocation funnel until every
/// victim offset has come back through the harvest and been re-claimed;
/// returns every offset handed out (the caller recycles them).
async fn reclaim_victims(sc: &AssemblerFold) -> Vec<u64> {
    let mut handed_out: Vec<u64> = Vec::new();
    let mut seen = 0usize;
    for _ in 0..sc.cap_blocks {
        let off = sc
            .cwr
            .alloc
            .allocate_block()
            .await
            .expect("the lane funnel serves (fresh, then harvested)");
        handed_out.push(off);
        if sc.victims.iter().any(|(_, idx)| idx * sc.bs == off) {
            seen += 1;
            if seen == sc.victims.len() {
                break;
            }
        }
    }
    assert_eq!(
        seen,
        sc.victims.len(),
        "every displaced victim came back through the harvest and was re-claimed"
    );
    handed_out
}

async fn recycle(sc: &AssemblerFold, handed_out: Vec<u64>) {
    for off in handed_out {
        sc.cwr
            .alloc
            .abandon_unpublished_offset(off)
            .await
            .expect("the probe mint recycles into its own lane");
    }
}

async fn teardown(sc: AssemblerFold) {
    let AssemblerFold {
        auth,
        a,
        cwr,
        w,
        ino,
        fh,
        ..
    } = sc;
    w.fs.release(w.req, ino, fh, 0, 0, false)
        .await
        .expect("release");
    assert!(
        w.fs.write_pipeline.quiesce(Duration::from_secs(30)).await,
        "the co-writer's pipeline drains"
    );
    drop(w);
    drop(cwr);
    drop(a);
    auth.stop().await;
}

/// Contract (the population, frame-carried): after the authority's fold
/// freed the co-writer's displaced blocks, the co-writer's NEXT publish-
/// plane round trip of any kind — here a whole-block write + fsync of an
/// unrelated block — carries the authority's lane-free notices, and the
/// co-writer releases its local tracking of exactly those blocks at that
/// reply (`cowriter.lane_free_notices` +N); the harvest then hands them
/// back and every re-claim is clean (`block_claim_anomalies` +0). The
/// day-2 rows: `block_claim_anomalies` 1,108 / 1,117 per fpp phase Σ 8
/// co-writers against the authority's `fold_passes` 1,183 / 1,111 — the
/// assembler's folds of the co-writers' first-iteration shipped slices,
/// every one a co-writer block freed by a publish the co-writer never
/// heard of. RED on dev 0bd03455: the two entries linger past the write's
/// reply and both re-claims trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authority_fold_of_shipped_slices_reaches_the_co_writers_tracking_on_its_next_reply() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let sc = assembler_fold(dir.path(), "assembler-fold-frame", &[2, 3]).await;
    let anomalies_before = METRICS.block_claim_anomalies.load(Ordering::Relaxed);
    let notices_before = METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed);
    let untracked_before = METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed);

    // Any round trip on the publish plane: the co-writer writes block 5.
    let wr =
        sc.w.fs
            .write(
                sc.w.req,
                sc.ino,
                sc.fh,
                5 * sc.bs,
                bytes::Bytes::from(round_pat(1, 5, sc.bs)),
                0,
                0,
            )
            .await
            .expect("the co-writer's unrelated write");
    assert_eq!(wr.written as u64, sc.bs);
    sc.w.fs
        .fsync(sc.w.req, sc.ino, sc.fh, false)
        .await
        .expect("the fsync ships a publish frame");
    assert!(
        sc.w.fs
            .write_pipeline
            .quiesce(Duration::from_secs(30))
            .await,
        "the co-writer's pipeline drains"
    );
    for (b, idx) in &sc.victims {
        assert_eq!(
            sc.cwr.alloc.refcount(idx * sc.bs),
            None,
            "block {b}: the reply's lane-free notice released the co-writer's tracking of \
             {idx} (RED: the entry lingers — nothing named the authority's free)"
        );
    }
    assert_eq!(
        METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed) - notices_before,
        sc.victims.len() as u64,
        "cowriter.lane_free_notices names the two releases (RED)"
    );

    // The harvest hands both victims back; the claims are clean.
    let handed_out = reclaim_victims(&sc).await;
    assert_eq!(
        METRICS.block_claim_anomalies.load(Ordering::Relaxed) - anomalies_before,
        0,
        "no re-claim found a lingering entry (CLAIM ANOMALY = 0 — RED: one per victim)"
    );
    recycle(&sc, handed_out).await;
    assert_eq!(
        METRICS
            .block_untracked_free_refusals
            .load(Ordering::Relaxed)
            - untracked_before,
        0,
        "nothing shipped a free for a block the authority already freed"
    );
    teardown(sc).await;
}

/// Contract (the population, harvest-carried — the tightest ordering): with
/// NO round trip between the authority's fold and the co-writer's harvest,
/// the harvest reply frame itself carries the notice — the authority
/// queues the notice BEFORE its ladder free-lists the block, so a reply
/// that hands the offset back was built after the notice was queued and
/// drains it — and the co-writer applies the frame's notices before the
/// grant is adopted: the entry is gone before the claim
/// (`block_claim_anomalies` +0, `cowriter.lane_free_notices` +1). RED on
/// dev 0bd03455: the claim trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_harvest_that_hands_back_an_authority_freed_block_carries_its_notice_first() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let sc = assembler_fold(dir.path(), "assembler-fold-harvest", &[3]).await;
    let anomalies_before = METRICS.block_claim_anomalies.load(Ordering::Relaxed);
    let notices_before = METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed);

    let handed_out = reclaim_victims(&sc).await;
    assert_eq!(
        METRICS.block_claim_anomalies.load(Ordering::Relaxed) - anomalies_before,
        0,
        "the harvest frame carried the notice ahead of its grant (CLAIM ANOMALY = 0 — RED)"
    );
    assert_eq!(
        METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed) - notices_before,
        1,
        "one notice applied, on the harvest's own reply (RED)"
    );
    let (_, idx) = sc.victims[0];
    assert_eq!(
        sc.cwr.alloc.refcount(idx * sc.bs),
        Some(1),
        "the re-claimed lifetime is tracked exactly once"
    );
    recycle(&sc, handed_out).await;
    teardown(sc).await;
}

/// Contract (the notice's INVERSE — a lifetime, never an offset): a notice
/// whose `after_grants` is BELOW the grant sequence this mount re-minted
/// the offset under names the offset's PREVIOUS lifetime (the reply that
/// carried it was reordered behind the grant's, across the ship depth's
/// sessions) and touches nothing — the live entry stays, the tag stays,
/// `cowriter.lane_free_notices_reminted` counts it; a notice at or above
/// the grant sequence names THIS lifetime (the authority freed the re-mint
/// too) and releases it. Driven through the co-writer's apply function on
/// the offsets the harvest tagged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_free_notice_below_the_offsets_grant_sequence_touches_nothing() {
    let _serial = serial();
    let _restore = restore();
    let dir = TempDir::new().unwrap();
    let sc = assembler_fold(dir.path(), "assembler-fold-inverse", &[3]).await;
    let handed_out = reclaim_victims(&sc).await;
    let (_, idx) = sc.victims[0];
    let off = idx * sc.bs;
    let tag = sc
        .cwr
        .alloc
        .harvest_grant_tag(idx)
        .expect("the harvest tagged the re-minted lifetime with its grant sequence");
    assert!(tag >= 1, "a served grant's sequence is 1-based: {tag}");
    assert_eq!(
        sc.cwr.alloc.refcount(off),
        Some(1),
        "premise: the re-mint is tracked"
    );
    let vol_tag = volume_tag(DATA_VOL);
    let applied_before = METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed);
    let reminted_before = METRICS
        .cowriter_lane_free_notices_reminted
        .load(Ordering::Relaxed);

    // A notice from BEFORE the grant (the reordered reply): untouched.
    cowriter::apply_lane_free_notices(&[publish::WireLaneFree {
        vol_tag,
        block_idx: idx,
        after_grants: tag - 1,
    }]);
    assert_eq!(
        sc.cwr.alloc.refcount(off),
        Some(1),
        "the live re-minted lifetime's entry is untouched by a notice older than its grant"
    );
    assert_eq!(
        sc.cwr.alloc.harvest_grant_tag(idx),
        Some(tag),
        "its tag stays"
    );
    assert_eq!(
        METRICS
            .cowriter_lane_free_notices_reminted
            .load(Ordering::Relaxed)
            - reminted_before,
        1,
        "the reorder guard counted the skip"
    );
    assert_eq!(
        METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed) - applied_before,
        0
    );

    // A notice from AFTER the grant (the authority freed the re-mint too):
    // released, the tag pruned with the lifetime.
    cowriter::apply_lane_free_notices(&[publish::WireLaneFree {
        vol_tag,
        block_idx: idx,
        after_grants: tag,
    }]);
    assert_eq!(
        sc.cwr.alloc.refcount(off),
        None,
        "a notice at or above the grant sequence names this lifetime and releases it"
    );
    assert_eq!(
        sc.cwr.alloc.harvest_grant_tag(idx),
        None,
        "the tag died with the lifetime"
    );
    assert_eq!(
        METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed) - applied_before,
        1
    );
    // A repeat is a no-op (untracked), counted on neither gauge.
    cowriter::apply_lane_free_notices(&[publish::WireLaneFree {
        vol_tag,
        block_idx: idx,
        after_grants: tag,
    }]);
    assert_eq!(
        METRICS.cowriter_lane_free_notices.load(Ordering::Relaxed) - applied_before,
        1
    );
    assert_eq!(
        METRICS
            .cowriter_lane_free_notices_reminted
            .load(Ordering::Relaxed)
            - reminted_before,
        1
    );

    // The staged re-mint's entry is gone; hand the rest back and stop
    // (the victim itself was released above — recycling it again would
    // double-list it).
    let rest: Vec<u64> = handed_out.into_iter().filter(|o| *o != off).collect();
    recycle(&sc, rest).await;
    teardown(sc).await;
}
