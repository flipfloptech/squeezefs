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
        CoWriter { client, alloc }
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
