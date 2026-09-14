//! Small-file PACKING on a CO-WRITER — PR PK4's contracts
//! (`docs/design-small-file-packing.md` §5.3, §5.6, §10; PR plan PK4).
//!
//! On the authority the open pack block is pinned by a refcount that IS
//! the truth the free ladder reads (PK2). On a co-writer the pin is a
//! private word and the authority frees from the DURABLE population, so
//! two hazards reopen — a DMA, or a PUBLISH, into a block a tenant delete
//! freed under the packer. §5.6 closes them structurally:
//!
//! 1. **ONE atomically-enqueued conveyor group per co-writer pack, on the
//!    pack's ONE home meta volume, under ONE owner** — the batch driver
//!    partitions a promotion batch by `(owner endpoint, route_ino(ino).0)`,
//!    each partition is its own pack (its own block), every tenant's DMA
//!    precedes every publish, and the partition's `SetLayoutAndSize`
//!    calls travel as ONE frame flagged `pack_group` (publish schema 17)
//!    that the owner serves as ONE `set_layout_and_size_group` on ONE
//!    volume — or REFUSES: `PUBLISH_PACK_GROUP_UNAVAILABLE` when its D-1c
//!    lever is `0`, `PUBLISH_PACK_GROUP_SPLIT` when its post-gate route
//!    re-derivation finds two home volumes. The co-writer never resends
//!    the same block's tenants as two frames: **at most ONE `pack_group`
//!    frame ever names a given pack block** — a refused pack is abandoned
//!    (the lane recycle) and its tenants re-`prepare`d into fresh packs.
//! 2. **The authority's served-publish screen** (belt): a served publish
//!    adopting a block on the authority's free list / grace ring / S7
//!    quarantine is refused loud (`served_publish_free_block_refusals`).
//! 3. **`release_pack_reference`'s untracked ⇒ no-op row**: the last
//!    committed tenant's shipped `Freed` retires the co-writer's private
//!    entry; the later seal must not take the terminal arm on it.
//!
//! Every arm keys on the TYPED publish outcome
//! (`SqueezefsError::PublishFailure { class }` — `TransportOutcomeUnknown |
//! FrameRefused(status) | CallRefused(status) | Protocol`); no arm parses a
//! message string (a source pin below). The membership grant advertises
//! the set authority's posture (`Grant::pack_group_available`,
//! `CLUSTER_WIRE_SCHEMA` 4).
//!
//! The thirteen contracts (PR plan PK4), lever ON through the seam:
//!  1. a co-writer's tenants land in blocks of its OWN lane (`b % W`),
//!     `alloc_lane_raise_refusals = 0`;
//!  2. DMA-before-publish — the trace seam records every pack-base DMA and
//!     every frame departure; no DMA on a base follows a publish naming it;
//!  3. the per-volume group law — a 2-meta-volume batch splits into ≥ 2
//!     packs (`pack_batch_volume_splits`), each pack's tenants land in ONE
//!     conveyor group (`META_CONVEYOR_GROUP_{COMMITS,TXS}` deltas,
//!     `pack_batch_frames = packs`); a partition larger than the frame cap
//!     splits into successive packs; two owner ENDPOINTS never share a pack;
//!  4. the pack seals at the frame reply (`pack_blocks_sealed_batch`); a
//!     zero-committed pack with a KNOWN refusal recycles into the lane
//!     (`cowriter_unpublished_recycles`);
//!  5. a tenant deleted after the group lands, before the seal, makes the
//!     seal's release a counted no-op (`pack_release_untracked_noops`), and
//!     a batch-end census hands out no offset a live layout names;
//!  6. a tenant delete ships `FreeBlocks` → `NonTerminal`
//!     (`free_refused_blocks = 0`); the last → `Freed`, the authority reclaims;
//!  7. the screen refuses a served publish adopting a free-listed block;
//!  8. the lever dependency (UNAVAILABLE → one-block-per-file, the grant
//!     without the flag → no packed arm) and the split check (SPLIT →
//!     abandon + re-`prepare` once, a second SPLIT → one-block-per-file;
//!     the frame seam asserts no two frames ever name one pack base);
//!  9. outcome unknown (FIND-PK-4): a lost reply after the owner applied
//!     surfaces as `TransportOutcomeUnknown`, the seal abandons WITHOUT
//!     recycle, the authority reads every tenant byte-exact, no double
//!     owner; the other classes are typed; the arms parse no string;
//! 10. `stage` never publishes the RAM layout entry — `finish` does;
//! 11. the authority reads the co-writer's packed files byte-exact after
//!     the co-writer is gone;
//! 12. `local_commit_refusals = 0`, `accounting_refusals = 0` throughout;
//! 13. ≤ frame-cap 3.5 guards held per group and none across a free.
//!
//! Venue caveat (stated, not hidden): one process plays both nodes over
//! ONE file-backed set and ONE sparse data device — the
//! `mw_authority_recycled_binding_tests` shape. Suite runs
//! `--test-threads=1` (process-global posture, ownership map, seams).

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
use squeezefs::error::{PublishFailureClass, SqueezefsError};
use squeezefs::free_grace;
use squeezefs::fuse_client::{self, MountPosture, SqueezefsFilesystem, METRICS};
use squeezefs::membership::{
    self, ClaimSet, ClaimSetMember, Grant, LeaseClock, LeaseClocks, MemberIdentity, MemberRole,
    MemberSession,
};
use squeezefs::meta_backend::kv::backend::WriterClaim;
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::kv::{META_CONVEYOR_GROUP_COMMITS, META_CONVEYOR_GROUP_TXS};
use squeezefs::meta_backend::{plan_meta_slot_set, Metadata as _, RoutedMetaBackend};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{
    self, BackendRouter, DataRouter, PackTrace, PromotedInto, StagedPromotionItem,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, TempDir};

const KIB: usize = 1024;
const VOL_LEN: u64 = 64 * 1024 * 1024;
/// Sparse data-device backing: the packs land a handful of blocks.
const DEV_LEN: u64 = 16 * 1024 * 1024 * 1024;
const SECRET: &[u8] = b"pk4-cowriter-pack-storage-trust-secret";
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
        routing::test_set_small_file_packing(None);
        routing::set_inline_max_bytes_override(None);
        routing::test_arm_pack_trace(false);
        routing::test_install_pack_pre_seal_hook(None);
        publish::test_install_pack_group_serve_hook(None);
        publish::TEST_LOSE_LAYOUT_PUBLISH_REPLIES.store(0, Ordering::Relaxed);
        std::env::remove_var(publish::CONVEYOR_GROUP_ENV);
        std::env::remove_var(squeezefs::meta_ship::router::BATCH_MAX_ENV);
        lane::test_reset_mount_partition();
        squeezefs::meta_backend::kv::block_refs::uninstall_block_ref_resolver();
        grant::uninstall_frontier_source();
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        publish::uninstall_free_executor();
        publish::uninstall_harvest_executor();
        publish::uninstall_binding_witness();
        publish::uninstall_released_block_probe();
        publish::uninstall_served_layout_invalidation();
        publish::uninstall_served_displacement_sink();
        ship::disarm_ownership();
        ship::uninstall_daemon_verb_router();
        ship::tokens::TEST_DELEGATION_OVERRIDE.store(0, Ordering::SeqCst);
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

/// A stamped `n`-volume meta set (the dynamic-routing plan, every
/// capability bit set) — round-robin minting spreads regular-file creates
/// across the volumes, which is what the per-volume partition law is for.
async fn fresh_set(dir: &Path, tag: &str, n: usize) -> Vec<PathBuf> {
    let plan = plan_meta_slot_set(n).expect("derived plan");
    let mut out = Vec::with_capacity(n);
    for (i, stamp) in plan.stamps.iter().enumerate() {
        let p = dir.join(format!("{tag}-meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        format_v3_stamped(&p, VOL_LEN, &opts(), stamp.clone())
            .await
            .expect("format meta volume");
        stamp_capabilities(&p).await;
        out.push(p);
    }
    out
}

fn uris(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
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

/// A membership lease grant carrying the set authority's pack-group
/// posture — what the co-writer's `MemberSession` adopts (the production
/// join), installed so `membership::pack_group_available()` reads it.
fn install_member_grant(pack_group_available: bool) {
    let grant = Grant {
        epoch: 3,
        term: 7,
        t_owner_ms: 3_000,
        skew_max_ms: 200,
        d_purge_ms: 400,
        renew_ms: 1_000,
        granted_at_owner_ms: 1_000,
        lane_supply_blocks: 0,
        checkpoint_ceiling_ms: 0,
        lane_supply_volumes: Vec::new(),
        pack_group_available,
        slot_leases_ack: Default::default(),
        slot_release_notices: Vec::new(),
        offered_slots: Vec::new(),
    };
    let session = MemberSession::adopt(
        NODE_A,
        MemberRole::Writer,
        &grant,
        1_000,
        LeaseClock::manual(Arc::new(AtomicU64::new(1_000))),
    );
    membership::install_member(Arc::new(session));
}

// ---------------------------------------------------------------------------
// The rig: one authority (custody + publish + a live data plane whose
// ladder executes the shipped frees) with one or two listener ENDPOINTS,
// and one co-writer with the production FUSE write path over its own
// routed set
// ---------------------------------------------------------------------------

struct Authority {
    listeners: Vec<Arc<cw::RpcListener>>,
    /// Held: the custody authority lives as long as the rig (the lane
    /// assignment and the lease epochs the co-writer presents are its).
    _owner: Arc<WriteCustodyOwner>,
    meta: Arc<RoutedMetaBackend>,
    endpoints: Vec<String>,
    alloc: Arc<BlockAllocator>,
    br: Arc<BackendRouter>,
}

impl Authority {
    /// `endpoints` listeners over ONE publish service — two endpoints make
    /// the co-writer's owner map name two OWNERS for the partition law
    /// without a partial-authority fixture (the frames still land).
    async fn start(vols: &[PathBuf], dev: &Path, members: &[&str], endpoints: usize) -> Authority {
        let meta = squeezefs::meta_backend::open_routed_meta_set(&uris(vols))
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
        // The production arm's data-plane installs, mirrored: the
        // shipped-free executor, the lane harvest, the finding-28 binding
        // probe, the finding-51 binding witness, the rung-19 resolver — and
        // PK4's released-block screen probe beside them.
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
        publish::install_released_block_probe(cowriter::router_released_block_probe(Arc::clone(
            &br,
        )));

        // ONE listener carrying the S8 metadata block (the co-writer's
        // creates/unlinks ship through it) beside custody + publish —
        // `arm_multi_writer`'s composition.
        let router: Arc<dyn cw::RpcAsyncService> = Arc::new(
            data_grant::AsyncVerbRouter::new()
                .with_custody(Arc::clone(&owner))
                .with_publish(publish::PublishService::new(Arc::clone(&meta)))
                .with_meta(ship::MetaShipService::new(Arc::clone(&meta))),
        );
        let mut listeners = Vec::with_capacity(endpoints);
        let mut eps = Vec::with_capacity(endpoints);
        for _ in 0..endpoints {
            let listener = cw::RpcListener::start_async(
                cw::RpcListenerConfig {
                    bind_addr: "127.0.0.1:0".parse().unwrap(),
                    service_threads: 2,
                    ..cw::RpcListenerConfig::default()
                },
                SECRET.to_vec(),
                Arc::clone(&router),
            )
            .expect("the authority listens");
            eps.push(listener.endpoint().to_string());
            listeners.push(listener);
        }
        Authority {
            listeners,
            _owner: owner,
            meta,
            endpoints: eps,
            alloc,
            br,
        }
    }

    /// Create empty regular files ON THE AUTHORITY, before the co-writer
    /// arms the process's ownership map: the authority's round-robin mint
    /// spreads them across the set's meta volumes (`pick_mint_volume` —
    /// which, in this one-process venue, sees the CO-WRITER's all-foreign
    /// map once armed and turns parent-sticky). Returns the inos.
    async fn pre_create(&self, names: &[String]) -> Vec<u64> {
        let mut inos = Vec::with_capacity(names.len());
        for name in names {
            inos.push(
                self.meta
                    .create_with_rdev(1, name, libc::S_IFREG | 0o644, 0, 0, 0)
                    .await
                    .unwrap_or_else(|e| panic!("authority create {name}: {e:?}"))
                    .ino,
            );
        }
        inos
    }

    /// The durable reference population of one block.
    async fn population(&self, block_idx: u64) -> usize {
        cowriter::durable_block_refcount(&self.meta, volume_tag(DATA_VOL), block_idx)
            .await
            .expect("the ledger answers")
    }

    fn free_listed(&self, block_idx: u64) -> bool {
        self.alloc.free_block_indices().contains(&block_idx)
    }

    async fn stop(self) {
        for l in &self.listeners {
            l.shutdown();
        }
        for v in &self.meta.volumes {
            v.shutdown().await.expect("the authority unmounts clean");
        }
    }
}

/// One co-writer: posture latched, routed set opened through the
/// admission, ownership armed all-foreign (volume `v` → endpoint
/// `endpoints[v % len]`), custody + publish clients installed, its granted
/// lane engaged, its own data plane with the reclaim queue CEASED, and the
/// production FUSE write path over that set.
struct CoWriter {
    meta: Arc<RoutedMetaBackend>,
    client: Arc<WriteCustodyClient>,
    alloc: Arc<BlockAllocator>,
    fs: Arc<SqueezefsFilesystem>,
    dlm: DlmClient,
    _stage: TempDir,
}

impl CoWriter {
    async fn join(auth: &Authority, vols: &[PathBuf], dev: &Path, node_id: &str) -> CoWriter {
        fuse_client::set_mount_posture(MountPosture::CoWriter);
        // Delegations pinned OFF: every create ships to the owner, whose
        // round-robin mint spreads the population across the volumes.
        ship::tokens::TEST_DELEGATION_OVERRIDE.store(2, Ordering::SeqCst);
        let admission = cowriter::classify_admission(&full_request(vols, node_id))
            .expect("the five-rung ladder admits");
        let meta = squeezefs::meta_backend::open_routed_meta_set_co_writer(&uris(vols), &admission)
            .await
            .expect("the co-writer's routed set");
        let owners: Vec<(usize, PeerOwner)> = (0..meta.volumes.len())
            .map(|v| {
                (
                    v,
                    PeerOwner::new(
                        format!("mw-authority-{}", v % auth.endpoints.len()),
                        auth.endpoints[v % auth.endpoints.len()].clone(),
                    ),
                )
            })
            .collect();
        let map = OwnerMap::for_volumes(&meta, owners).expect("an all-foreign owner map");
        ship::arm_ownership(map);
        // `cowriter::arm`'s S8 half: the daemon verb router over the
        // co-writer's own backend (its creates / unlinks / destroys SHIP).
        ship::install_daemon_verb_router(ship::MetaShipRouter::new(
            Arc::clone(&meta),
            node_id,
            SECRET.to_vec(),
        ));
        publish::install_client(publish::PublishClient::new(node_id, SECRET.to_vec()));
        let client = WriteCustodyClient::connect(&auth.endpoints[0], SECRET, node_id)
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

        let dlm = DlmClient::new().expect("dlm");
        let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
        let stage = tempdir().unwrap();
        let cache = TieredCache::new(
            vec![stage.path().to_path_buf()],
            Some("64MB"),
            Some("64MB"),
            Some("16MB"),
            Some("32MB"),
            Arc::clone(&alloc),
            Arc::clone(&nvme),
            None,
        )
        .await
        .expect("tiered cache");
        let router = DataRouter::new(dlm.clone(), cache, Arc::clone(&alloc), nvme);
        let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
        fs.router.set_meta_backend(Arc::clone(&meta));
        fs.meta_backend = Some(Arc::clone(&meta));
        CoWriter {
            meta,
            client,
            alloc,
            fs: Arc::new(fs),
            dlm,
            _stage: stage,
        }
    }

    fn lane(&self) -> (u64, u64) {
        let p = self.client.lane_partition();
        (u64::from(p.writer_id()), u64::from(p.writers()))
    }

    async fn create(&self, name: &str) -> u64 {
        self.fs
            .create(req(), 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap_or_else(|e| panic!("create {name} failed: {e:?}"))
            .attr
            .ino
    }

    /// A staged-layout file of `len` bytes (resident in this co-writer's
    /// ring, no map): returns `(ino, file_id)`.
    async fn staged_file(&self, name: &str, len: usize, tag: usize) -> (u64, String) {
        let ino = self.create(name).await;
        let fid = self.stage_into(ino, len, tag).await;
        (ino, fid)
    }

    /// Stage `len` bytes into an existing (empty) ino through the
    /// production write path: returns its `file_id`.
    async fn stage_into(&self, ino: u64, len: usize, tag: usize) -> String {
        let written = self
            .fs
            .write(
                req(),
                ino,
                0,
                0,
                bytes::Bytes::from(pattern(tag, len)),
                0,
                0,
            )
            .await
            .unwrap_or_else(|e| panic!("write ino {ino} failed: {e:?}"))
            .written;
        assert_eq!(written as usize, len, "short write");
        let m = self.fs.router.metadata_cache.get(&ino).expect("RAM layout");
        assert_eq!(m.file_type, "staged", "fixture premise: staged layout");
        let fid = m.file_id.as_deref().expect("file_id").to_string();
        assert!(
            self.fs.router.cache.nvme.read_staged(&fid).is_some(),
            "fixture premise: ring-resident"
        );
        fid
    }

    fn item(&self, ino: u64, fid: &str) -> StagedPromotionItem {
        StagedPromotionItem {
            file_path: squeezefs::keys::inode_path(ino),
            file_id: fid.to_string(),
            fencing_token: self.dlm.get_fencing_token_ino(ino),
        }
    }

    /// The tenant mapping `block_map[0]` of a promoted file, decoded:
    /// `(base key, off, len)`.
    fn mapping(&self, ino: u64) -> (String, u64, usize) {
        let m = self.fs.router.metadata_cache.get(&ino).expect("layout");
        let s = m
            .block_map
            .as_ref()
            .and_then(|bm| bm.get(&0).cloned())
            .unwrap_or_else(|| panic!("ino {ino} has no block_map[0]: {m:?}"));
        let (prefix, rest) = match s.find("://") {
            Some(p) => s.split_at(p + 3),
            None => ("", s.as_str()),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        assert_eq!(parts.len(), 3, "size-carrying mapping: {s}");
        (
            format!("{prefix}{}", parts[0]),
            parts[1].parse().unwrap(),
            parts[2].parse().unwrap(),
        )
    }

    fn block_idx(&self, base_key: &str) -> u64 {
        self.fs
            .router
            .backend_router
            .parse_block_offset(base_key)
            .expect("base key parses")
            / self.alloc.chunk_size()
    }

    /// Unlink + reclaim on the co-writer — the FUSE forget path's batch
    /// entry (the release ships every terminal free).
    async fn unlink(&self, name: &str, ino: u64) {
        let _ = self.fs.release(req(), ino, 0, 0, 0, true).await;
        self.fs
            .unlink(req(), 1, OsStr::new(name))
            .await
            .unwrap_or_else(|e| panic!("unlink {name} failed: {e:?}"));
        self.fs.reclaim_orphaned_batch(vec![ino]).await;
    }
}

/// The authority's READER: its own data plane, the shared backend's
/// records — a client A's staging root is invisible to.
struct AuthorityReader {
    fs: SqueezefsFilesystem,
    _stage: TempDir,
}

async fn authority_reader(auth: &Authority, dev: &Path) -> AuthorityReader {
    let dlm = DlmClient::new().expect("dlm");
    let nvme = Arc::new(NvmeBlockDev::new(dev.to_str().unwrap()));
    let stage = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![stage.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        Arc::clone(&auth.alloc),
        Arc::clone(&nvme),
        None,
    )
    .await
    .expect("tiered cache");
    let router = DataRouter::new(dlm.clone(), cache, Arc::clone(&auth.alloc), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.router.set_meta_backend(Arc::clone(&auth.meta));
    fs.meta_backend = Some(Arc::clone(&auth.meta));
    AuthorityReader { fs, _stage: stage }
}

impl AuthorityReader {
    async fn read(&self, ino: u64, len: usize) -> Vec<u8> {
        self.fs.router.discard_layout_cache(ino);
        self.fs
            .read(req(), ino, 0, 0, len as u32, 0)
            .await
            .unwrap_or_else(|e| panic!("authority read of ino {ino} failed: {e:?}"))
            .data
            .to_vec()
    }
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    }
}

/// Deterministic per-file content, salted by the index.
fn pattern(idx: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (idx.wrapping_mul(131)
                .wrapping_add(i.wrapping_mul(7))
                .wrapping_add(i >> 8)
                % 251) as u8
        })
        .collect()
}

fn metric(a: &fuse_client::Align64<AtomicU64>) -> u64 {
    a.load(Ordering::Relaxed)
}

/// Lever seams: one-page inline ceiling (the 8–64 KiB population stays
/// staged), packing ON, the trace armed.
fn arm_levers() {
    routing::set_inline_max_bytes_override(Some(routing::INLINE_MAX_FLOOR));
    routing::test_set_small_file_packing(Some(true));
    routing::test_arm_pack_trace(true);
}

/// Scoped env override (the owner reads its levers per served frame).
struct EnvVarGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// The co-writer counters every leg must leave flat (contract 12).
struct Flat {
    local_commit_refusals: u64,
    accounting_refusals: u64,
    raise_refusals: u64,
    screen_refusals: u64,
}

fn flat() -> Flat {
    Flat {
        local_commit_refusals: metric(&METRICS.cowriter_local_commit_refusals),
        accounting_refusals: metric(&METRICS.cowriter_accounting_refusals),
        raise_refusals: metric(&METRICS.alloc_lane_raise_refusals),
        screen_refusals: metric(&METRICS.served_publish_free_block_refusals),
    }
}

fn assert_flat(before: &Flat) {
    let now = flat();
    assert_eq!(
        now.local_commit_refusals, before.local_commit_refusals,
        "local_commit_refusals stays 0 through the leg (contract 12)"
    );
    assert_eq!(
        now.accounting_refusals, before.accounting_refusals,
        "accounting_refusals stays 0 through the leg (contract 12)"
    );
    assert_eq!(
        now.raise_refusals, before.raise_refusals,
        "alloc_lane_raise_refusals = 0 (contract 1)"
    );
    assert_eq!(
        now.screen_refusals, before.screen_refusals,
        "the served-publish screen refused nothing on a healthy leg (contract 7)"
    );
}

/// The frame-departure records of the trace: `(base key, inos)`.
fn frames(trace: &[PackTrace]) -> Vec<(String, Vec<u64>)> {
    trace
        .iter()
        .filter_map(|t| match t {
            PackTrace::FrameDeparture { base_key, inos, .. } => {
                Some((base_key.clone(), inos.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The frame departures keyed by pack LIFETIME: `(pack seq, base key)`.
fn frame_packs(trace: &[PackTrace]) -> Vec<(u64, String)> {
    trace
        .iter()
        .filter_map(|t| match t {
            PackTrace::FrameDeparture { pack, base_key, .. } => Some((*pack, base_key.clone())),
            _ => None,
        })
        .collect()
}

/// KD-4's invariant over a trace: no two frame departures name one pack
/// LIFETIME (a recycled offset re-opened as a fresh pack is a new one —
/// these fixture keys carry no incarnation stamp, so the open sequence is
/// the identity), and no DMA into a pack follows a frame naming it.
fn assert_frame_invariants(trace: &[PackTrace]) {
    let mut named: BTreeSet<u64> = BTreeSet::new();
    for t in trace {
        match t {
            PackTrace::FrameDeparture { pack, base_key, .. } => {
                assert!(
                    named.insert(*pack),
                    "pack {pack} ({base_key}) named by a SECOND pack_group frame — KD-4's \
                     invariant"
                );
            }
            PackTrace::Dma {
                pack,
                base_key,
                ino,
            } => {
                assert!(
                    !named.contains(pack),
                    "DMA of ino {ino} into pack {pack} ({base_key}) AFTER a frame named it — \
                     DMA-before-publish broken"
                );
            }
            _ => {}
        }
    }
}

/// Contract 10 over a trace: no tenant's RAM layout publish precedes the
/// departure of the frame that names it.
fn assert_stage_never_publishes_ram(trace: &[PackTrace]) {
    let mut departed: BTreeSet<u64> = BTreeSet::new();
    let mut dma_seen: BTreeSet<u64> = BTreeSet::new();
    for t in trace {
        match t {
            PackTrace::Dma { ino, .. } => {
                dma_seen.insert(*ino);
            }
            PackTrace::FrameDeparture { inos, .. } => departed.extend(inos.iter().copied()),
            PackTrace::RamPublish { ino } if dma_seen.contains(ino) => {
                assert!(
                    departed.contains(ino),
                    "tenant ino {ino}'s RAM layout entry published BEFORE its frame departed — \
                     `stage` published what only `finish` may"
                );
            }
            _ => {}
        }
    }
}

/// Contract 13 over a trace: every guard set is ≤ the frame cap wide, and
/// every pin/tenant release runs with no guard held.
fn assert_guard_discipline(trace: &[PackTrace], cap: usize) {
    let mut held = false;
    for t in trace {
        match t {
            PackTrace::GuardsHeld { stripes } => {
                assert!(
                    *stripes <= cap,
                    "{stripes} 3.5 guards held > frame cap {cap}"
                );
                held = true;
            }
            PackTrace::GuardsDropped => held = false,
            PackTrace::PinRelease { base_key } => {
                assert!(
                    !held,
                    "the pack reference release of {base_key} ran UNDER a held 3.5 guard (RES-1)"
                );
            }
            _ => {}
        }
    }
}

/// A rig: `n` meta volumes, `endpoints` authority endpoints, one co-writer.
struct Rig {
    auth: Authority,
    cwr: CoWriter,
    dev: PathBuf,
    _dir: TempDir,
}

async fn rig(tag: &str, volumes: usize, endpoints: usize, pack_group_available: bool) -> Rig {
    rig_with_files(tag, volumes, endpoints, pack_group_available, &[])
        .await
        .0
}

/// [`rig`] with `names` pre-created on the authority (spread across the
/// volumes) before the co-writer joins; returns their inos beside the rig.
async fn rig_with_files(
    tag: &str,
    volumes: usize,
    endpoints: usize,
    pack_group_available: bool,
    names: &[String],
) -> (Rig, Vec<u64>) {
    let dir = TempDir::new().unwrap();
    let vols = fresh_set(dir.path(), tag, volumes).await;
    let dev = data_device(dir.path(), &format!("{tag}.dev"));
    let auth = Authority::start(&vols, &dev, &[NODE_A], endpoints).await;
    let inos = auth.pre_create(names).await;
    let cwr = CoWriter::join(&auth, &vols, &dev, NODE_A).await;
    install_member_grant(pack_group_available);
    arm_levers();
    (
        Rig {
            auth,
            cwr,
            dev,
            _dir: dir,
        },
        inos,
    )
}

// ===========================================================================
// 1/2/4/10/11/12/13 — the headline: one pack, one lane, one frame, one seal
// ===========================================================================

/// Contracts 1, 2, 4, 10, 11, 12, 13: six 16 KiB staged files promoted as
/// ONE co-writer batch land in ONE block of the co-writer's own lane, every
/// DMA precedes the one frame naming the block, the RAM entries publish
/// only after the frame departed, the pack seals at the reply
/// (`pack_blocks_sealed_batch + 1`), the guards held per group are ≤ the
/// frame cap and none across the release, and the authority reads every
/// tenant byte-exact once the co-writer is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_writers_batch_packs_into_one_lane_block_as_one_frame() {
    let _serial = serial();
    let _restore = restore();
    let r = rig("headline", 1, 1, true).await;
    let (lane_id, writers) = r.cwr.lane();
    const N: usize = 6;
    let len = 16 * KIB;
    let mut files = Vec::new();
    for i in 0..N {
        files.push(r.cwr.staged_file(&format!("t{i}.bin"), len, 10 + i).await);
    }
    let before = flat();
    let packed0 = metric(&METRICS.layout_promoted_packed);
    let sealed0 = metric(&METRICS.pack_blocks_sealed_batch);
    let frames0 = metric(&METRICS.pack_batch_frames);
    let tenants0 = metric(&METRICS.pack_batch_tenants);
    let batches0 = metric(&METRICS.pack_cowriter_batches);
    let groups0 = META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed);
    let group_txs0 = META_CONVEYOR_GROUP_TXS.load(Ordering::Relaxed);
    routing::test_take_pack_trace();

    let items: Vec<StagedPromotionItem> = files
        .iter()
        .map(|(ino, fid)| r.cwr.item(*ino, fid))
        .collect();
    let outcomes = r.cwr.fs.router.promote_staged_batch(items).await;
    for (i, out) in outcomes.iter().enumerate() {
        assert!(
            matches!(out, Ok(Some(PromotedInto::Packed))),
            "tenant {i} promoted PACKED: {out:?}"
        );
    }
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, N as u64);
    assert_eq!(metric(&METRICS.pack_cowriter_batches) - batches0, 1);
    assert_eq!(
        metric(&METRICS.pack_blocks_sealed_batch) - sealed0,
        1,
        "the pack sealed at the frame reply (contract 4)"
    );
    assert_eq!(metric(&METRICS.pack_batch_frames) - frames0, 1, "one frame");
    assert_eq!(
        metric(&METRICS.pack_batch_tenants) - tenants0,
        N as u64,
        "tenants ÷ frames is the live pack width"
    );
    assert_eq!(
        META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed) - groups0,
        1,
        "ONE conveyor group on the owner"
    );
    assert_eq!(
        META_CONVEYOR_GROUP_TXS.load(Ordering::Relaxed) - group_txs0,
        N as u64,
        "the group carried every tenant"
    );

    // Contract 1: one block, in the co-writer's lane.
    let (base, off0, len0) = r.cwr.mapping(files[0].0);
    assert_eq!((off0, len0), (0, len));
    let idx = r.cwr.block_idx(&base);
    assert_eq!(
        idx % writers,
        lane_id,
        "the pack block is in the co-writer's lane (b % W)"
    );
    let mut offs = BTreeSet::new();
    for (ino, fid) in &files {
        let (b, off, l) = r.cwr.mapping(*ino);
        assert_eq!(b, base, "every tenant names the one pack block");
        assert_eq!(l, len);
        assert_eq!(off % 4096, 0);
        assert!(offs.insert(off), "distinct slots");
        assert!(
            r.cwr.fs.router.cache.nvme.read_staged(fid).is_none(),
            "the ring entry released at the commit"
        );
    }
    assert_eq!(
        r.auth.population(idx).await,
        N,
        "N tenants = N durable references"
    );
    assert!(!r.auth.free_listed(idx));
    assert_eq!(
        r.cwr.alloc.refcount(idx * r.cwr.alloc.chunk_size()),
        Some(N as u32),
        "the seal released the private pin: refcount = tenants"
    );

    // Contracts 2, 10, 13 over the trace.
    let trace = routing::test_take_pack_trace();
    assert_eq!(frames(&trace).len(), 1, "one frame departure: {trace:?}");
    assert_eq!(frames(&trace)[0].1.len(), N);
    assert_frame_invariants(&trace);
    assert_stage_never_publishes_ram(&trace);
    assert_guard_discipline(&trace, publish::pack_group_tenant_cap());
    assert!(
        trace
            .iter()
            .filter(|t| matches!(t, PackTrace::Dma { .. }))
            .count()
            == N,
        "one DMA per tenant"
    );
    assert_flat(&before);

    // Contract 11: the co-writer is gone; the authority reads byte-exact.
    let Rig {
        auth,
        cwr,
        dev,
        _dir,
    } = r;
    drop(cwr);
    let reader = authority_reader(&auth, &dev).await;
    for (i, (ino, _)) in files.iter().enumerate() {
        let got = reader.read(*ino, len).await;
        assert_eq!(
            got,
            pattern(10 + i, len),
            "tenant {i} byte-exact from the authority"
        );
    }
    assert_eq!(metric(&METRICS.staged_payload_lost_reads), 0);
    auth.stop().await;
}

// ===========================================================================
// 3 — the per-volume and per-owner partition law, and the frame-cap split
// ===========================================================================

/// Contract 3 (volumes): on a 2-meta-volume set the round-robin-minted
/// population partitions into 2 packs (`pack_batch_volume_splits`), each
/// pack's tenants land in ONE conveyor group (`frames = packs`, one group
/// commit per frame, group txs = tenants), and no pack names tenants of two
/// home volumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_partitions_by_home_meta_volume_one_group_per_pack() {
    let _serial = serial();
    let _restore = restore();
    const N: usize = 8;
    let names: Vec<String> = (0..N).map(|i| format!("v{i}.bin")).collect();
    let (r, inos) = rig_with_files("volumes", 2, 1, true, &names).await;
    let len = 8 * KIB;
    let mut files = Vec::new();
    for (i, ino) in inos.into_iter().enumerate() {
        files.push((ino, r.cwr.stage_into(ino, len, 20 + i).await));
    }
    let homes: BTreeSet<usize> = files
        .iter()
        .map(|(ino, _)| r.cwr.meta.route_ino(*ino).0)
        .collect();
    assert_eq!(
        homes.len(),
        2,
        "fixture premise: the population spans both volumes"
    );
    let before = flat();
    let splits0 = metric(&METRICS.pack_batch_volume_splits);
    let frames0 = metric(&METRICS.pack_batch_frames);
    let groups0 = META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed);
    let group_txs0 = META_CONVEYOR_GROUP_TXS.load(Ordering::Relaxed);
    routing::test_take_pack_trace();

    let items: Vec<StagedPromotionItem> = files
        .iter()
        .map(|(ino, fid)| r.cwr.item(*ino, fid))
        .collect();
    for out in r.cwr.fs.router.promote_staged_batch(items).await {
        assert!(matches!(out, Ok(Some(PromotedInto::Packed))), "{out:?}");
    }
    assert_eq!(
        metric(&METRICS.pack_batch_volume_splits) - splits0,
        2,
        "the batch was split into two (owner, home volume) partitions"
    );
    assert_eq!(
        metric(&METRICS.pack_batch_frames) - frames0,
        2,
        "frames = packs"
    );
    assert_eq!(
        META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed) - groups0,
        2,
        "one conveyor group per pack"
    );
    assert_eq!(
        META_CONVEYOR_GROUP_TXS.load(Ordering::Relaxed) - group_txs0,
        N as u64
    );
    // Every pack's tenants share ONE home volume.
    let mut by_base: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (ino, _) in &files {
        let (base, _, _) = r.cwr.mapping(*ino);
        by_base
            .entry(base)
            .or_default()
            .insert(r.cwr.meta.route_ino(*ino).0);
    }
    assert_eq!(by_base.len(), 2, "two pack blocks");
    for (base, vols) in &by_base {
        assert_eq!(
            vols.len(),
            1,
            "pack {base} names tenants of one home volume only"
        );
    }
    let trace = routing::test_take_pack_trace();
    assert_eq!(frames(&trace).len(), 2);
    assert_frame_invariants(&trace);
    assert_stage_never_publishes_ram(&trace);
    assert_flat(&before);
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

/// Contract 3 (the frame cap): a partition larger than the derived frame
/// cap splits into successive packs — each its own block, its own frame,
/// sealed at its frame boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partition_past_the_frame_cap_splits_into_successive_packs() {
    let _serial = serial();
    let _restore = restore();
    let _cap = EnvVarGuard::set(squeezefs::meta_ship::router::BATCH_MAX_ENV, "2");
    let r = rig("framecap", 1, 1, true).await;
    assert_eq!(
        publish::pack_group_tenant_cap(),
        2,
        "the cap derives from the frame cap"
    );
    const N: usize = 5;
    let len = 8 * KIB;
    let mut files = Vec::new();
    for i in 0..N {
        files.push(r.cwr.staged_file(&format!("c{i}.bin"), len, 30 + i).await);
    }
    let frames0 = metric(&METRICS.pack_batch_frames);
    let sealed0 = metric(&METRICS.pack_blocks_sealed_batch);
    routing::test_take_pack_trace();
    let items: Vec<StagedPromotionItem> = files
        .iter()
        .map(|(ino, fid)| r.cwr.item(*ino, fid))
        .collect();
    for out in r.cwr.fs.router.promote_staged_batch(items).await {
        assert!(matches!(out, Ok(Some(PromotedInto::Packed))), "{out:?}");
    }
    assert_eq!(
        metric(&METRICS.pack_batch_frames) - frames0,
        3,
        "⌈5/2⌉ frames"
    );
    assert_eq!(
        metric(&METRICS.pack_blocks_sealed_batch) - sealed0,
        3,
        "each sealed"
    );
    let bases: BTreeSet<String> = files.iter().map(|(ino, _)| r.cwr.mapping(*ino).0).collect();
    assert_eq!(bases.len(), 3, "three pack blocks");
    let trace = routing::test_take_pack_trace();
    assert_eq!(frames(&trace).len(), 3);
    for (_, inos) in frames(&trace) {
        assert!(inos.len() <= 2, "a frame never exceeds the cap");
    }
    assert_frame_invariants(&trace);
    assert_guard_discipline(&trace, 2);
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

/// Contract 3 (owners): with the co-writer's map naming a DIFFERENT owner
/// endpoint per volume, no pack names tenants of two owners — one frame
/// per endpoint, both landing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_never_packs_tenants_of_two_owner_endpoints_together() {
    let _serial = serial();
    let _restore = restore();
    const N: usize = 6;
    let names: Vec<String> = (0..N).map(|i| format!("o{i}.bin")).collect();
    let (r, inos) = rig_with_files("owners", 2, 2, true, &names).await;
    let len = 8 * KIB;
    let mut files = Vec::new();
    for (i, ino) in inos.into_iter().enumerate() {
        files.push((ino, r.cwr.stage_into(ino, len, 40 + i).await));
    }
    let homes: BTreeSet<usize> = files
        .iter()
        .map(|(ino, _)| r.cwr.meta.route_ino(*ino).0)
        .collect();
    assert_eq!(
        homes.len(),
        2,
        "fixture premise: the population spans both volumes"
    );
    let frames0 = metric(&METRICS.pack_batch_frames);
    routing::test_take_pack_trace();
    let items: Vec<StagedPromotionItem> = files
        .iter()
        .map(|(ino, fid)| r.cwr.item(*ino, fid))
        .collect();
    for out in r.cwr.fs.router.promote_staged_batch(items).await {
        assert!(matches!(out, Ok(Some(PromotedInto::Packed))), "{out:?}");
    }
    assert_eq!(metric(&METRICS.pack_batch_frames) - frames0, 2);
    let mut by_base: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (ino, _) in &files {
        let (base, _, _) = r.cwr.mapping(*ino);
        let v = r.cwr.meta.route_ino(*ino).0;
        let ep = ship::owner_of_volume(v)
            .expect("all-foreign")
            .endpoint
            .clone();
        by_base.entry(base).or_default().insert(ep);
    }
    assert_eq!(by_base.len(), 2);
    for (base, eps) in &by_base {
        assert_eq!(
            eps.len(),
            1,
            "pack {base} names tenants of one owner endpoint only"
        );
    }
    assert_frame_invariants(&routing::test_take_pack_trace());
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 5/6 — a tenant deleted between the group landing and the seal
// ===========================================================================

/// Contracts 5 and 6: a ONE-tenant pack whose tenant is deleted after the
/// group lands and BEFORE the seal (the pre-seal hook): the delete ships
/// `FreeBlocks` → `Freed` (population 0), the co-writer's private entry is
/// retired, and the seal's `release_pack_reference` is a counted no-op
/// (`pack_release_untracked_noops + 1`) — never a second handout. With TWO
/// tenants the first delete answers `NonTerminal` (`free_refused_blocks =
/// 0`) and the last `Freed`, the authority reclaims. A batch-end census:
/// no lane harvest ever hands out an offset a live layout names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_deleted_before_the_seal_makes_the_release_a_counted_noop() {
    let _serial = serial();
    let _restore = restore();
    let r = rig("preseal", 1, 1, true).await;
    let len = 8 * KIB;
    let (ino, fid) = r.cwr.staged_file("lone.bin", len, 50).await;
    let noops0 = metric(&METRICS.pack_release_untracked_noops);
    let shipped0 = publish::stats().free_shipped_blocks;
    let refused0 = publish::stats().free_refused_blocks;

    // The hook: delete the tenant while the pack is landed-but-unsealed.
    let fs = Arc::clone(&r.cwr.fs);
    let hook_ran = Arc::new(AtomicBool::new(false));
    let ran = Arc::clone(&hook_ran);
    routing::test_install_pack_pre_seal_hook(Some(Arc::new(move |_base: String| {
        let fs = Arc::clone(&fs);
        let ran = Arc::clone(&ran);
        Box::pin(async move {
            let _ = fs.release(req(), ino, 0, 0, 0, true).await;
            fs.unlink(req(), 1, OsStr::new("lone.bin"))
                .await
                .expect("unlink under the hook");
            fs.reclaim_orphaned_batch(vec![ino]).await;
            ran.store(true, Ordering::SeqCst);
        })
    })));
    let out = r
        .cwr
        .fs
        .router
        .promote_staged_batch(vec![r.cwr.item(ino, &fid)])
        .await;
    assert!(
        matches!(out[0], Ok(Some(PromotedInto::Packed))),
        "{:?}",
        out[0]
    );
    assert!(hook_ran.load(Ordering::SeqCst), "the pre-seal hook ran");
    routing::test_install_pack_pre_seal_hook(None);
    assert_eq!(
        publish::stats().free_shipped_blocks - shipped0,
        1,
        "the tenant's delete SHIPPED its free"
    );
    assert_eq!(publish::stats().free_refused_blocks - refused0, 0);
    assert_eq!(
        metric(&METRICS.pack_release_untracked_noops) - noops0,
        1,
        "the seal's release found the entry retired by the Freed verdict: a counted no-op"
    );
    // The block is the authority's again (free list / grace / reclaim), and
    // NOT on the co-writer's lane free list a second time.
    let trace = routing::test_take_pack_trace();
    let base = frames(&trace)[0].0.clone();
    let idx = r.cwr.block_idx(&base);
    r.auth.br.reclaim_drain().await;
    assert_eq!(r.auth.population(idx).await, 0);
    assert!(
        !r.cwr.alloc.free_block_indices().contains(&idx),
        "no double handout into the co-writer's lane list"
    );
    assert_eq!(metric(&METRICS.block_double_frees), 0);

    // Two tenants: NonTerminal then Freed.
    let (a, fa) = r.cwr.staged_file("pair_a.bin", len, 51).await;
    let (b, fb) = r.cwr.staged_file("pair_b.bin", len, 52).await;
    let outs = r
        .cwr
        .fs
        .router
        .promote_staged_batch(vec![r.cwr.item(a, &fa), r.cwr.item(b, &fb)])
        .await;
    assert!(outs
        .iter()
        .all(|o| matches!(o, Ok(Some(PromotedInto::Packed)))));
    let (base2, _, _) = r.cwr.mapping(a);
    assert_eq!(r.cwr.mapping(b).0, base2, "one pack");
    let idx2 = r.cwr.block_idx(&base2);
    let refused1 = publish::stats().free_refused_blocks;
    r.cwr.unlink("pair_a.bin", a).await;
    assert_eq!(
        r.auth.population(idx2).await,
        1,
        "NonTerminal: the sibling's reference stays"
    );
    assert!(!r.auth.free_listed(idx2));
    assert_eq!(
        publish::stats().free_refused_blocks - refused1,
        0,
        "free_refused_blocks = 0"
    );
    r.cwr.unlink("pair_b.bin", b).await;
    r.auth.br.reclaim_drain().await;
    assert_eq!(r.auth.population(idx2).await, 0, "Freed at population 0");
    assert!(
        r.auth.free_listed(idx2) || r.auth.alloc.grace_holds(idx2 * r.auth.alloc.chunk_size()),
        "the authority reclaimed the block (free list or grace ring)"
    );

    // Batch-end census: a lane harvest hands out nothing a live layout names.
    let (lane_id, writers) = r.cwr.lane();
    let epoch = r.cwr.client.lease_epoch();
    let grant = publish::ship_harvest_lane_free(
        &r.auth.endpoints[0],
        volume_tag(DATA_VOL),
        lane_id as u16,
        writers as u16,
        64,
        epoch,
        0xF00D_0001,
    )
    .await
    .expect("the harvest ships");
    for &harvested_idx in &grant.blocks {
        assert_eq!(
            r.auth.population(harvested_idx).await,
            0,
            "harvested block {harvested_idx} is named by a live layout"
        );
    }
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 7 — the authority's served-publish screen
// ===========================================================================

/// Contract 7: a hand-built served publish whose refs ADOPT a block on the
/// authority's free list is REFUSED loud — `served_publish_free_block_
/// refusals + 1`, the client sees a typed `CallRefused(PUBLISH_FREE_BLOCK_
/// REFUSED)`, nothing is staged (population 0, the block still free).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_served_publish_screen_refuses_a_publish_adopting_a_released_block() {
    let _serial = serial();
    let _restore = restore();
    // The authority first (the process posture is the writer's here): a
    // block it minted and RELEASED — on its free list — then the co-writer.
    let dir = TempDir::new().unwrap();
    let vols = fresh_set(dir.path(), "screen", 1).await;
    let dev = data_device(dir.path(), "screen.dev");
    let auth = Authority::start(&vols, &dev, &[NODE_A], 1).await;
    let off = auth.alloc.allocate_block().await.expect("mint");
    let idx = off / auth.alloc.chunk_size();
    auth.alloc.publish_block(off);
    auth.br.free_block(&off.to_string()).await.expect("free");
    auth.br.reclaim_drain().await;
    assert!(auth.free_listed(idx), "fixture premise: free-listed");
    let cwr = CoWriter::join(&auth, &vols, &dev, NODE_A).await;
    install_member_grant(true);
    arm_levers();
    let r = Rig {
        auth,
        cwr,
        dev,
        _dir: dir,
    };

    let ino = r.cwr.create("adopter.bin").await;
    let mut map = std::collections::HashMap::new();
    map.insert(0u32, format!("{off}:0:4096"));
    let layout = bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "staged".into(),
        size: 4096,
        block_map_id: None,
        block_prefix: None,
        file_id: Some("hand-built".into()),
        data_key: None,
        block_map: Some(map),
    })
    .expect("layout bytes");
    let refs = [BlockRefOp::taken(BlockRef {
        vol_tag: volume_tag(DATA_VOL),
        block_idx: idx,
        owner_ino: ino,
        block_index: 0,
    })];
    let refusals0 = metric(&METRICS.served_publish_free_block_refusals);
    let out = publish::set_layout_and_size(&r.cwr.meta, ino, &layout, 4096, &refs).await;
    match out {
        Err(SqueezefsError::PublishFailure { class, .. }) => assert_eq!(
            class,
            PublishFailureClass::CallRefused(publish::PUBLISH_FREE_BLOCK_REFUSED),
            "the typed per-call refusal"
        ),
        other => panic!("expected the screen's refusal, got {other:?}"),
    }
    assert_eq!(
        metric(&METRICS.served_publish_free_block_refusals) - refusals0,
        1
    );
    assert_eq!(r.auth.population(idx).await, 0, "nothing staged");
    assert!(
        r.auth.free_listed(idx),
        "the block stays free — never re-claimed"
    );
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 8 — the lever dependency and the split check
// ===========================================================================

/// Contract 8 (UNAVAILABLE + the grant): an owner on the D-1c control arm
/// (`SQUEEZEFS_PUBLISH_CONVEYOR_GROUP=0`) refuses the `pack_group` frame
/// (`served_pack_group_unavailable + 1`); the co-writer abandons the
/// never-published pack (KNOWN → the lane recycle, `cowriter_unpublished_
/// recycles + 1`, `pack_blocks_abandoned + 1`), promotes those tenants
/// one-block-per-file (`pack_cowriter_group_refusals + N`,
/// `layout_promoted_block + N`), and every file reads byte-exact from the
/// authority. A grant without `pack_group_available` skips the packed arm
/// outright (`pack_cowriter_group_unavailable + N`, no frame ever shipped).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_on_the_control_arm_refuses_the_frame_and_the_co_writer_falls_back() {
    let _serial = serial();
    let _restore = restore();
    let r = rig("unavailable", 1, 1, true).await;
    let len = 8 * KIB;
    const N: usize = 3;
    let mut files = Vec::new();
    for i in 0..N {
        files.push(r.cwr.staged_file(&format!("u{i}.bin"), len, 60 + i).await);
    }
    let before = flat();
    let unavailable0 = metric(&METRICS.served_pack_group_unavailable);
    let refusals0 = metric(&METRICS.pack_cowriter_group_refusals);
    let recycles0 = metric(&METRICS.cowriter_unpublished_recycles);
    let abandoned0 = metric(&METRICS.pack_blocks_abandoned);
    let block0 = metric(&METRICS.layout_promoted_block);
    let packed0 = metric(&METRICS.layout_promoted_packed);
    routing::test_take_pack_trace();
    {
        let _lever = EnvVarGuard::set(publish::CONVEYOR_GROUP_ENV, "0");
        let items: Vec<StagedPromotionItem> = files
            .iter()
            .map(|(ino, fid)| r.cwr.item(*ino, fid))
            .collect();
        for out in r.cwr.fs.router.promote_staged_batch(items).await {
            assert!(
                matches!(out, Ok(Some(PromotedInto::Block))),
                "the fallback is one-block-per-file: {out:?}"
            );
        }
    }
    assert_eq!(
        metric(&METRICS.served_pack_group_unavailable) - unavailable0,
        1
    );
    assert_eq!(
        metric(&METRICS.pack_cowriter_group_refusals) - refusals0,
        N as u64
    );
    assert_eq!(
        metric(&METRICS.cowriter_unpublished_recycles) - recycles0,
        1
    );
    assert_eq!(metric(&METRICS.pack_blocks_abandoned) - abandoned0, 1);
    assert_eq!(metric(&METRICS.layout_promoted_block) - block0, N as u64);
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, 0);
    let trace = routing::test_take_pack_trace();
    let fr = frames(&trace);
    assert_eq!(
        fr.len(),
        1,
        "exactly one (refused) pack_group frame departed"
    );
    // The abandoned pack block went back to the co-writer's lane free list
    // (the KNOWN recycle) — where the fallback's own mints may have taken
    // it again as a FRESH lifetime: either it is still free, or exactly one
    // fallback file's own block IS it (population 1, `bk:0:len`).
    let abandoned_idx = r.cwr.block_idx(&fr[0].0);
    let mut fallback_blocks = Vec::new();
    for (ino, _) in &files {
        let (base, off, _) = r.cwr.mapping(*ino);
        assert_eq!(off, 0, "one-block-per-file: bk:0:len");
        fallback_blocks.push(r.cwr.block_idx(&base));
    }
    let reminted = fallback_blocks
        .iter()
        .filter(|b| **b == abandoned_idx)
        .count();
    if reminted == 0 {
        assert!(
            r.cwr.alloc.free_block_indices().contains(&abandoned_idx),
            "the abandoned pack block is back on the co-writer's lane free list"
        );
        assert_eq!(
            r.auth.population(abandoned_idx).await,
            0,
            "no layout ever named it"
        );
    } else {
        assert_eq!(reminted, 1, "a recycled block is minted to ONE new owner");
        assert_eq!(r.auth.population(abandoned_idx).await, 1);
    }
    assert_frame_invariants(&trace);
    assert_flat(&before);
    // The authority reads the fallback files byte-exact.
    let reader = authority_reader(&r.auth, &r.dev).await;
    for (i, (ino, _)) in files.iter().enumerate() {
        assert_eq!(reader.read(*ino, len).await, pattern(60 + i, len));
    }
    drop(reader);

    // The grant without the flag: no packed arm, no frame.
    install_member_grant(false);
    let grant_unavail0 = metric(&METRICS.pack_cowriter_group_unavailable);
    let frames0 = metric(&METRICS.pack_cowriter_frames);
    let (g, gf) = r.cwr.staged_file("grantless.bin", len, 70).await;
    let out = r
        .cwr
        .fs
        .router
        .promote_staged_batch(vec![r.cwr.item(g, &gf)])
        .await;
    assert!(
        matches!(out[0], Ok(Some(PromotedInto::Block))),
        "{:?}",
        out[0]
    );
    assert_eq!(
        metric(&METRICS.pack_cowriter_group_unavailable) - grant_unavail0,
        1
    );
    assert_eq!(
        metric(&METRICS.pack_cowriter_frames) - frames0,
        0,
        "no frame shipped"
    );
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

/// Contract 8 (SPLIT): a slot cutover injected between the co-writer's
/// partition and the owner's serve (the owner's slot map flipped for the
/// frame) draws `PUBLISH_PACK_GROUP_SPLIT` (`served_pack_group_splits +
/// 1`); the co-writer ABANDONS the refused pack (`pack_blocks_abandoned +
/// 1`, X back on the lane free list, no layout names X), re-`prepare`s
/// every tenant of that frame (a fresh reservation and a fresh DMA per
/// tenant) into fresh packs on the now-current routes, and each retry
/// frame lands as one sub-group. A SECOND split falls back
/// one-block-per-file (`pack_cowriter_group_splits + N`). The frame seam
/// asserts no two frames ever name one pack base.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slot_cutover_between_partition_and_serve_draws_split_and_re_prepares_once() {
    let _serial = serial();
    let _restore = restore();
    const N: usize = 4;
    let names: Vec<String> = (0..N).map(|i| format!("s{i}.bin")).collect();
    let (r, inos) = rig_with_files("split", 2, 1, true, &names).await;
    let len = 8 * KIB;
    let mut files = Vec::new();
    for (i, ino) in inos.into_iter().enumerate() {
        files.push((ino, r.cwr.stage_into(ino, len, 80 + i).await));
    }
    // The cutover: flip the FIRST tenant's slot onto the other volume on
    // the OWNER's table for the duration of the next pack_group serve (a
    // migration whose flip lands between partition and serve), then
    // restore — `flips` frames in a row.
    let auth_meta = Arc::clone(&r.auth.meta);
    let victim = files[0].0;
    let victim_slot = auth_meta.slot_of_ino(victim) as usize;
    let base_map = auth_meta.slot_map_snapshot();
    let flips = Arc::new(AtomicU64::new(1));
    {
        let flips = Arc::clone(&flips);
        let auth_meta = Arc::clone(&auth_meta);
        let base_map = base_map.clone();
        publish::test_install_pack_group_serve_hook(Some(Arc::new(move || {
            if flips.load(Ordering::SeqCst) == 0 {
                return None;
            }
            flips.fetch_sub(1, Ordering::SeqCst);
            let mut flipped = base_map.clone();
            flipped[victim_slot] = (flipped[victim_slot] + 1) % 2;
            auth_meta.publish_slot_map(flipped).expect("the flip");
            let auth_meta = Arc::clone(&auth_meta);
            let base_map = base_map.clone();
            Some(Box::new(move || {
                auth_meta.publish_slot_map(base_map).expect("the restore");
            }) as Box<dyn FnOnce() + Send>)
        })));
    }
    let before = flat();
    let splits0 = metric(&METRICS.served_pack_group_splits);
    let cw_splits0 = metric(&METRICS.pack_cowriter_group_splits);
    let abandoned0 = metric(&METRICS.pack_blocks_abandoned);
    let packed0 = metric(&METRICS.layout_promoted_packed);
    let groups0 = META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed);
    routing::test_take_pack_trace();

    let items: Vec<StagedPromotionItem> = files
        .iter()
        .map(|(ino, fid)| r.cwr.item(*ino, fid))
        .collect();
    for out in r.cwr.fs.router.promote_staged_batch(items).await {
        assert!(matches!(out, Ok(Some(PromotedInto::Packed))), "{out:?}");
    }
    assert_eq!(
        metric(&METRICS.served_pack_group_splits) - splits0,
        1,
        "one SPLIT refusal"
    );
    assert_eq!(
        metric(&METRICS.pack_cowriter_group_splits) - cw_splits0,
        0,
        "no fallback"
    );
    assert_eq!(metric(&METRICS.layout_promoted_packed) - packed0, N as u64);
    let trace = routing::test_take_pack_trace();
    let fr = frames(&trace);
    let packs = frame_packs(&trace);
    // The first frame carried the victim and was refused; its pack was
    // abandoned (the lane recycle) — the retry frames landed on FRESH packs
    // (new lifetimes; the recycled offset may be one of them, re-minted).
    let (refused_pack, refused_base) = packs[0].clone();
    assert_eq!(
        packs.iter().filter(|(p, _)| *p == refused_pack).count(),
        1,
        "the refused pack lifetime is named by ONE frame only"
    );
    assert!(
        metric(&METRICS.pack_blocks_abandoned) - abandoned0 >= 1,
        "the refused pack was abandoned"
    );
    let refused_idx = r.cwr.block_idx(&refused_base);
    let reminted = packs[1..].iter().any(|(_, b)| *b == refused_base);
    if !reminted {
        assert!(
            r.cwr.alloc.free_block_indices().contains(&refused_idx),
            "X is back on the lane free list"
        );
        assert_eq!(r.auth.population(refused_idx).await, 0, "no layout names X");
        for (ino, _) in &files {
            let (base, _, _) = r.cwr.mapping(*ino);
            assert_ne!(
                base, refused_base,
                "no tenant's layout names the refused block"
            );
        }
    }
    // Every tenant of the refused frame was DMA'd TWICE (fresh reservation,
    // fresh DMA) — once into X, once into its retry pack.
    for &ino in &fr[0].1 {
        let dmas = trace
            .iter()
            .filter(|t| matches!(t, PackTrace::Dma { ino: i, .. } if *i == ino))
            .count();
        assert_eq!(dmas, 2, "tenant {ino}: a fresh DMA on the retry");
    }
    assert!(
        META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed) - groups0 >= fr.len() as u64 - 1,
        "every landed frame was one owner group"
    );
    assert_frame_invariants(&trace);
    assert_stage_never_publishes_ram(&trace);
    assert_flat(&before);

    // A second SPLIT (a migration still in flight): fall back one-block-per-file.
    flips.store(u64::MAX, Ordering::SeqCst);
    let mut second = Vec::new();
    for i in 0..2 {
        second.push(r.cwr.staged_file(&format!("s2_{i}.bin"), len, 90 + i).await);
    }
    // Both tenants on the victim's home volume make every frame split.
    let victim_home = auth_meta.route_ino(victim).0;
    let second: Vec<(u64, String)> = second
        .into_iter()
        .filter(|(ino, _)| auth_meta.route_ino(*ino).0 == victim_home)
        .collect();
    if !second.is_empty() {
        let victim_slot_hits = second
            .iter()
            .filter(|(ino, _)| auth_meta.slot_of_ino(*ino) as usize == victim_slot)
            .count();
        let cw_splits1 = metric(&METRICS.pack_cowriter_group_splits);
        let items: Vec<StagedPromotionItem> = second
            .iter()
            .map(|(ino, fid)| r.cwr.item(*ino, fid))
            .collect();
        let outs = r.cwr.fs.router.promote_staged_batch(items).await;
        if victim_slot_hits > 0 && victim_slot_hits < second.len() {
            for out in &outs {
                assert!(
                    matches!(out, Ok(Some(PromotedInto::Block))),
                    "a second SPLIT falls back one-block-per-file: {out:?}"
                );
            }
            assert_eq!(
                metric(&METRICS.pack_cowriter_group_splits) - cw_splits1,
                second.len() as u64
            );
        }
        assert_frame_invariants(&routing::test_take_pack_trace());
    }
    publish::test_install_pack_group_serve_hook(None);
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

// ===========================================================================
// 9 — outcome unknown (FIND-PK-4) and the typed class
// ===========================================================================

/// Contract 9: with the frame's replies LOST after the owner applied it
/// (the seam), the failure surfaces as `TransportOutcomeUnknown`, the
/// co-writer's seal abandons WITHOUT recycle (`cowriter_unpublished_
/// abandons + 1`, the block absent from the lane free list), the
/// authority's layouts name the tenants, the authority reads every tenant
/// byte-exact, and no double owner is ever minted (the next mint is a
/// different block). The per-file block arm takes the same disposition.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_reply_after_the_owner_applied_abandons_without_recycle() {
    let _serial = serial();
    let _restore = restore();
    let r = rig("unknown", 1, 1, true).await;
    let len = 8 * KIB;
    const N: usize = 3;
    let mut files = Vec::new();
    for i in 0..N {
        files.push(r.cwr.staged_file(&format!("k{i}.bin"), len, 100 + i).await);
    }
    let abandons0 = metric(&METRICS.cowriter_unpublished_abandons);
    let recycles0 = metric(&METRICS.cowriter_unpublished_recycles);
    routing::test_take_pack_trace();
    publish::TEST_LOSE_LAYOUT_PUBLISH_REPLIES.store(u64::MAX, Ordering::Relaxed);
    let items: Vec<StagedPromotionItem> = files
        .iter()
        .map(|(ino, fid)| r.cwr.item(*ino, fid))
        .collect();
    let outs = r.cwr.fs.router.promote_staged_batch(items).await;
    publish::TEST_LOSE_LAYOUT_PUBLISH_REPLIES.store(0, Ordering::Relaxed);
    for out in &outs {
        match out {
            Err(SqueezefsError::PublishFailure { class, .. }) => assert_eq!(
                *class,
                PublishFailureClass::TransportOutcomeUnknown,
                "the lost reply IS the outcome-unknown class"
            ),
            other => panic!("expected TransportOutcomeUnknown, got {other:?}"),
        }
    }
    assert_eq!(
        metric(&METRICS.cowriter_unpublished_abandons) - abandons0,
        1,
        "the seal abandoned WITHOUT recycle"
    );
    assert_eq!(
        metric(&METRICS.cowriter_unpublished_recycles) - recycles0,
        0
    );
    let trace = routing::test_take_pack_trace();
    let fr = frames(&trace);
    assert_eq!(
        fr.len(),
        1,
        "one frame (its resends re-send the same request ids)"
    );
    let base = fr[0].0.clone();
    let idx = r.cwr.block_idx(&base);
    assert!(
        !r.cwr.alloc.free_block_indices().contains(&idx),
        "the block is NOT on the co-writer's lane free list"
    );
    assert_eq!(
        r.auth.population(idx).await,
        N,
        "the authority APPLIED the frame: every tenant's reference is durable"
    );
    // No double owner: the co-writer's next mint is a different block.
    let next = r.cwr.alloc.allocate_block().await.expect("mint");
    assert_ne!(next / r.cwr.alloc.chunk_size(), idx, "never re-minted");
    let _ = r.cwr.alloc.abandon_unpublished_offset(next).await;
    // The authority reads every tenant byte-exact.
    let reader = authority_reader(&r.auth, &r.dev).await;
    for (i, (ino, _)) in files.iter().enumerate() {
        assert_eq!(
            reader.read(*ino, len).await,
            pattern(100 + i, len),
            "tenant {i}"
        );
    }
    drop(reader);

    // FIND-PK-4 on the per-file block arm: the same disposition.
    install_member_grant(false);
    let (b, fb) = r.cwr.staged_file("block_unknown.bin", len, 110).await;
    let abandons1 = metric(&METRICS.cowriter_unpublished_abandons);
    let recycles1 = metric(&METRICS.cowriter_unpublished_recycles);
    publish::TEST_LOSE_LAYOUT_PUBLISH_REPLIES.store(u64::MAX, Ordering::Relaxed);
    let out = r
        .cwr
        .fs
        .router
        .promote_staged_batch(vec![r.cwr.item(b, &fb)])
        .await;
    publish::TEST_LOSE_LAYOUT_PUBLISH_REPLIES.store(0, Ordering::Relaxed);
    assert!(
        matches!(
            out[0],
            Err(SqueezefsError::PublishFailure {
                class: PublishFailureClass::TransportOutcomeUnknown,
                ..
            })
        ),
        "{:?}",
        out[0]
    );
    assert_eq!(
        metric(&METRICS.cowriter_unpublished_abandons) - abandons1,
        1
    );
    assert_eq!(
        metric(&METRICS.cowriter_unpublished_recycles) - recycles1,
        0
    );
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

/// Contract 9 (the typed classes): a frame-level refusal surfaces as
/// `FrameRefused(status)` and a per-call refusal as `CallRefused(status)`
/// — on the landed per-call lane too (`fail_all` / the per-call arm no
/// longer erase the class into `InvalidOperation`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frame_and_call_refusals_surface_typed() {
    let _serial = serial();
    let _restore = restore();
    let r = rig("typed", 1, 1, true).await;
    // A per-call refusal: a publish naming an ino this authority routes
    // to a volume it holds no authority over is refused NOT_OWNER — build
    // it by shipping to a service with an EMPTY authority set.
    let svc = publish::PublishService::with_authority(Arc::clone(&r.auth.meta), &[]);
    let router = Arc::new(data_grant::AsyncVerbRouter::new().with_publish(svc));
    let listener = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 1,
            ..cw::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        router,
    )
    .expect("the no-authority listener");
    let client = publish::PublishClient::new(NODE_A, SECRET.to_vec());
    let call = publish::PublishCall::CommitBlockRefs {
        ino: 2,
        refs: Vec::new(),
        lease_epoch: r.cwr.client.lease_epoch(),
        request_id: 0xF00D_0002,
    };
    match client
        .ship(&listener.endpoint().to_string(), call.clone())
        .await
    {
        Err(SqueezefsError::PublishFailure { class, .. }) => assert_eq!(
            class,
            PublishFailureClass::CallRefused(publish::PUBLISH_NOT_OWNER)
        ),
        other => panic!("expected CallRefused(NOT_OWNER), got {other:?}"),
    }
    // A frame-level refusal: a pack_group frame at an owner on the control arm.
    let _lever = EnvVarGuard::set(publish::CONVEYOR_GROUP_ENV, "0");
    let group_call = publish::PublishCall::SetLayoutAndSize {
        ino: 2,
        layout: Vec::new(),
        size: 0,
        refs: Vec::new(),
        lease_epoch: r.cwr.client.lease_epoch(),
        request_id: 0xF00D_0003,
    };
    match client
        .ship_group(&r.auth.endpoints[0], vec![group_call])
        .await
    {
        Err(SqueezefsError::PublishFailure { class, .. }) => assert_eq!(
            class,
            PublishFailureClass::FrameRefused(publish::PUBLISH_PACK_GROUP_UNAVAILABLE)
        ),
        other => panic!("expected FrameRefused(PACK_GROUP_UNAVAILABLE), got {other:?}"),
    }
    listener.shutdown();
    let Rig { auth, cwr, .. } = r;
    drop(cwr);
    auth.stop().await;
}

/// Contract 9 (the pin): the outcome-class arms of the batch driver and the
/// release primitive key on the TYPED class — a scan of the marked region
/// finds no `to_string()` and no `.contains(`.
#[test]
fn the_outcome_class_arms_never_parse_a_message_string() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/routing.rs"))
        .expect("read src/routing.rs");
    const BEGIN: &str = "// PK4: outcome-class arms begin";
    const END: &str = "// PK4: outcome-class arms end";
    let mut regions = 0;
    let mut cursor = 0;
    while let Some(b) = src[cursor..].find(BEGIN) {
        let begin = cursor + b;
        let end = begin
            + src[begin..]
                .find(END)
                .expect("every begin marker has its end marker");
        let region = &src[begin..end];
        assert!(
            !region.contains("to_string()") && !region.contains(".contains("),
            "an outcome-class arm parses a message string:\n{region}"
        );
        assert!(
            region.contains("PublishFailureClass::TransportOutcomeUnknown"),
            "the arms key on the typed class:\n{region}"
        );
        regions += 1;
        cursor = end + END.len();
    }
    assert!(
        regions >= 2,
        "the helper and the driver's frame arm are both marked ({regions})"
    );
}
