//! Symmetric metadata program, PR 9 — **custody by the slot holder**
//! (`docs/design-symmetric-metadata.md` §5.5 the "S9 custody endpoint"
//! row, §5.1.5 the custody lock class, §5.4.1/§5.4.3 the W1 clause, §5.7.1
//! the holder's implicit Write, §11 the `dlm_custody` family; PR-plan
//! row 9).
//!
//! Under `SQUEEZEFS_SYMMETRIC_META=1` the custody SERVER for a file is its
//! slot's HOLDER, resolved through tree 0's lessee + the `SlotHolderCache`
//! exactly as PR 6's shipped steps are; the custody grant rides PR 5's
//! token wire as ONE round trip that carries the file's records (Lustre's
//! intent lock), so a foreign file costs exactly one grant and an own file
//! costs 0 RPCs; the holder's later commit on the file recalls the writer's
//! token (PR 5's pass hook) and the writer re-fetches. The S9 protocol is
//! unchanged — JOIN / RENEW / RELEASE / `T_self` / the custody epoch — only
//! WHERE its server is moved. Unarmed, and on a bit-17-absent volume, the
//! S9 authority client serves verbatim (the S9 suites pin it).
//!
//! **The multi-holder shape** is PR 6's: ONE process holds region 0 (the
//! manager — the writer's own appender) and the DECLARED regions the seam
//! names (`SQUEEZEFS_TEST_SYM_APPENDER_SLOTS`), each its own ring, lease
//! set and tree-0 lessee record; a file in a declared region's slot is
//! FOREIGN to appender 0 for the custody decision and its grant travels
//! over a real `cluster_wire` session to the endpoint registered for that
//! appender — the S9 custody owner + the token service over the same
//! backend. N daemon processes on one volume is PR 12's join ladder.
//!
//! Also here: PR 7's owed no-re-Put law at the two BACKEND-side recompute
//! translators (the owner's compose of a shipped frame and the kvmap
//! train's resolve — `cancel_same_reference_pairs`), and the adjudication
//! of the PK4 `pack_group` wire (§5.4.3): the UNARMED co-writer ships it
//! today, so it stays; an ARMED symmetric mount never issues one.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, MapTrainClaims};
use squeezefs::meta_backend::kv::block_map::MapEntry;
use squeezefs::meta_backend::kv::block_refs::{
    install_block_ref_resolver, uninstall_block_ref_resolver, volume_tag, BlockRef, BlockRefOp,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::path::Path;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// The seams, the resolver and the custody registry are process-global;
/// every contract serializes on it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ===========================================================================
// Part A — PR 7's owed no-re-Put law at the two backend-side translators
// (`.benchmarks/2026-09-15-sym-pr7-refs.md` §7, review Issue 18b).
// ===========================================================================

fn reference(vol_tag: u64, block_idx: u64, owner: u64, index: u32) -> BlockRef {
    BlockRef {
        vol_tag,
        block_idx,
        owner_ino: owner,
        block_index: index,
    }
}

/// The owner-side translator (`KvMetaBackend::recompute_refs_against_map`
/// — the compose of a co-writer's shipped frame): a decorated clip
/// `bk:off:len → bk:off:len'` of one entry resolves the displaced and the
/// adopted key to ONE reference; the translated frame must stage NOTHING
/// for it (a re-Put would rewrite the record's value from scratch and
/// strip a durable SHARED bit — the C16 tripwire tripped by a legal op).
/// A real move (two references) stays two ops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_owner_side_translator_stages_no_op_for_a_re_described_reference() {
    let _g = SEAM.lock().await;
    const TAG: u64 = 0x5EED;
    const INO: u64 = 4242;
    // The resolver the mount installs: the block index is the key's block
    // number (`bk:off:len` keys the same block whatever the window).
    install_block_ref_resolver(Arc::new(|key: &str, ino: u64, idx: u32| {
        let block = key
            .trim_start_matches("bk")
            .split(':')
            .next()?
            .parse::<u64>()
            .ok()?;
        Some(reference(TAG, block, ino, idx))
    }));
    let head: std::collections::HashMap<u32, String> = [
        (0u32, "7:0:4096".to_string()),
        (1u32, "8:0:4096".to_string()),
    ]
    .into_iter()
    .collect();
    // Entry 0 re-described (the clip); entry 1 moved to another block.
    let entries = vec![
        (0u32, "7:0:2048".to_string()),
        (1u32, "9:0:4096".to_string()),
    ];
    let frame = KvMetaBackend::recompute_refs_against_map(&head, &entries, INO, &[])
        .expect("a resolver is armed");
    uninstall_block_ref_resolver();
    assert_eq!(
        frame.ops,
        vec![
            BlockRefOp::released(reference(TAG, 8, INO, 1)),
            BlockRefOp::taken(reference(TAG, 9, INO, 1)),
        ],
        "the re-described reference stages nothing; the move stays a released + taken pair"
    );
    assert!(frame.ram_only_releases.is_empty());
}

// --- the kvmap train's resolve (a FLAT kvmap volume: layout-blind) -------

const META_LEN: u64 = 256 * 1024 * 1024;
const DATA_LEN: u64 = 32 * 1024 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-00000000000000d9";
const BLOCK: u64 = 4 * 1024 * 1024;

fn kvmap_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn format_meta_kvmap(path: &Path) {
    format_v3(path, META_LEN, &kvmap_opts())
        .await
        .expect("format v3 meta volume");
    let VolumeFormat::V3(mut sb) = classify_volume(path).await.expect("classify") else {
        panic!("expected v3");
    };
    let strip = FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS | FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE;
    if sb.features_incompat & strip != 0 {
        sb.features_incompat &= !strip;
        write_superblock_v3(path, &sb).await.expect("strip seams");
    }
    assert!(set_block_refcounts_bit(path).await.expect("stamp bit 9"));
    assert!(set_block_map_tree_bit(path).await.expect("stamp bit 16"));
}

struct KvmapRig {
    routed: Arc<RoutedMetaBackend>,
    _router: DataRouter,
    _staging: TempDir,
}

async fn mount_kvmap(meta: &Path, data: &Path) -> KvmapRig {
    let kv = KvMetaBackend::open(meta)
        .await
        .expect("open v3 meta volume");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv]));
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
    alloc.set_capacity_bytes(DATA_LEN);
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, alloc, nvme);
    router.set_meta_backend(routed.clone());
    KvmapRig {
        routed,
        _router: router,
        _staging: staging,
    }
}

fn data_file() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(DATA_LEN)
        .unwrap();
    f
}

/// The kv-level `entry_key` closure the direct train calls use: bare-offset
/// keys, 4 MiB stride.
fn entry_key_for_tests(entry: &MapEntry, delta: u32) -> Option<String> {
    match entry {
        MapEntry::String(b) if delta == 0 => String::from_utf8(b.clone()).ok(),
        MapEntry::Point { offset, .. } if delta == 0 => Some(offset.to_string()),
        MapEntry::Run {
            start_offset, len, ..
        } if delta < *len => Some((start_offset + u64::from(delta) * BLOCK).to_string()),
        _ => None,
    }
}

fn kvmap_head_bytes(size: u64) -> Vec<u8> {
    bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "striped".to_string(),
        size,
        block_map_id: Some("kvmap:1".to_string()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: None,
    })
    .expect("head bytes")
}

/// The kvmap train's resolve: a RUN's start re-adopted as a POINT at the
/// same block (a legal per-index re-description — the run dissolves, the
/// start's binding is unchanged) resolves the displaced run start and the
/// adopted point to ONE reference. The pair stages nothing — the record
/// keeps its durable SHARED bit — and the block leaves the train's
/// post-commit free stream (a released reference the record still holds
/// would be freed under the record).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kvmap_trains_resolve_keeps_the_shared_bit_across_a_re_description() {
    let _g = SEAM.lock().await;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount_kvmap(meta.path(), data.path()).await;
    let kv = Arc::clone(&rig.routed.volumes[0]);
    let ino = rig
        .routed
        .create(1, "shared-run", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create")
        .ino;
    let vol_tag = volume_tag(DATA_VOL_ID);
    // One run of 4 blocks at index 0 through the establishing train.
    let run = MapEntry::Run {
        vol_tag,
        start_offset: 0,
        len: 4,
    };
    kv.migrate_block_map_train(
        ino,
        &kvmap_head_bytes(4 * BLOCK),
        4 * BLOCK,
        &[],
        &[(0u32, run)],
        512,
        None,
        ino,
        &entry_key_for_tests,
        0,
        &|_key, _idx| None,
    )
    .await
    .expect("establishing train")
    .expect("engaged tree");
    // The run start's reference, durably SHARED (the clone protocol's mark).
    let r0 = reference(vol_tag, 0, ino, 0);
    rig.routed
        .commit_block_refs(ino, &[BlockRefOp::taken_shared(r0)])
        .await
        .expect("stage the shared reference");
    let before = kv
        .block_ref_probe_flags(vol_tag, 0, None)
        .await
        .expect("probe");
    assert_eq!((before.count, before.shared), (1, true));

    // The claims train re-adopts the run's start as a point at the SAME
    // block: the dissolve's displaced run start and the adopted point
    // resolve to `r0`.
    let ref_for = |key: &str, idx: u32| -> Option<BlockRef> {
        let offset = key.parse::<u64>().ok()?;
        Some(reference(vol_tag, offset / BLOCK, ino, idx))
    };
    let claims = MapTrainClaims {
        base_gen: None,
        take: [0u32].into_iter().collect(),
        release: std::collections::BTreeSet::new(),
        served: false,
        overlay: true,
        window: false,
    };
    let outcome = kv
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(4 * BLOCK),
            4 * BLOCK,
            &[],
            &[(0u32, MapEntry::Point { vol_tag, offset: 0 })],
            512,
            Some(&claims),
            ino,
            &entry_key_for_tests,
            0,
            &ref_for,
        )
        .await
        .expect("claims train")
        .expect("engaged tree");
    assert!(outcome.recomputed, "the overlay claims train recomputes");
    assert!(
        outcome.released.is_empty(),
        "a re-described reference never enters the free stream: {:?}",
        outcome.released
    );
    let after = kv
        .block_ref_probe_flags(vol_tag, 0, None)
        .await
        .expect("probe");
    assert_eq!(
        (after.count, after.shared),
        (1, true),
        "the re-described reference keeps its SHARED bit"
    );
    // The run dissolved: the start binds as a point, the tail survives.
    assert_eq!(
        kv.get_block_mapping(ino, 0).await.unwrap().unwrap(),
        (0, MapEntry::Point { vol_tag, offset: 0 })
    );
    assert_eq!(
        entry_key_for_tests(&kv.get_block_mapping(ino, 3).await.unwrap().unwrap().1, 0),
        Some((3 * BLOCK).to_string())
    );
    kv.shutdown().await.expect("clean shutdown");
}

// ===========================================================================
// Part B — custody by the slot holder (design §5.5, §5.1.5, §5.1.4).
// ===========================================================================

mod common;

use common::sym::{
    data_file as sym_data_file, format_stamped_member, mount_data, slot_of_global, DataRig, Knobs,
};
use squeezefs::cluster_wire as cw;
use squeezefs::data_custody;
use squeezefs::data_grant::{self, AsyncVerbRouter, SlotCustodyArm, WriteCustodyOwner};
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::builder::ROOT_INO;
use squeezefs::meta_backend::kv::record::ForestSlot;
use squeezefs::meta_backend::{make_global_ino_width, IntentCreatePreset};
use squeezefs::meta_ship::manager::ManagerSetService;
use squeezefs::meta_ship::token_plane::{
    RecallDataSink, RecalledObject, TokenSetService, TokenWants,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const SECRET: &[u8] = b"sym-custody-tests-enroll-secret";
/// Forest slot 4 (routing slot 3) — the declared appender 1's.
const SLOT_B: ForestSlot = 4;
const TWO_HOLDERS: &str = "1:4";
/// This mount's KD-MW-2 member id at every holder.
const WRITER: &str = "node-w";

/// Restores every process-global custody posture, so a panicking contract
/// can never leave the binary armed for the next one.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        data_grant::uninstall_slot_custody();
        data_grant::uninstall_custody_owner();
        data_grant::uninstall_custody_client();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
    }
}

/// Deterministic lease clocks for the holder's custody owner: a short
/// TTL on a manual clock (the S9 suite's discipline — seams, never
/// sleeps).
fn clocks(ms: &Arc<AtomicU64>) -> (LeaseClocks, LeaseClock) {
    let clock = LeaseClock::manual(Arc::clone(ms));
    let c = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("2*skew + purge < TTL");
    (c, clock)
}

fn listener_cfg() -> cw::RpcListenerConfig {
    cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    }
}

/// The writer's recall sink: counts the data-plane drains a recall runs
/// before its ack travels.
struct ProbeSink {
    calls: AtomicU64,
}

impl RecallDataSink for ProbeSink {
    fn drain_and_purge<'a>(
        &'a self,
        _objects: &'a [RecalledObject],
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
        })
    }
}

/// The holders' venue: the S9 custody owner + the token service + the
/// manager verbs over `rig`'s set on one listener (what PR 12's join
/// ladder stands up on every writer), every declared appender's endpoint
/// registered on the plane, and this mount's slot-custody arm installed.
struct Venue {
    host: Arc<cw::RpcListener>,
    endpoint: String,
    owner: Arc<WriteCustodyOwner>,
    arm: Arc<SlotCustodyArm>,
    sink: Arc<ProbeSink>,
    ms: Arc<AtomicU64>,
}

impl Venue {
    async fn stand_up(rig: &DataRig, appenders: &[u32]) -> Self {
        let ms = Arc::new(AtomicU64::new(1_000));
        let (c, clock) = clocks(&ms);
        let owner = WriteCustodyOwner::arm(
            "slot-holder",
            squeezefs::dlm::durable_term() + 1,
            squeezefs::dlm::durable_term(),
            c,
            clock,
            None,
        )
        .expect("the holder's custody authority arms");
        data_grant::install_custody_owner(Arc::clone(&owner));
        let router = AsyncVerbRouter::new()
            .with_custody(Arc::clone(&owner))
            .with_tokens(TokenSetService::new(&rig.routed.volumes))
            .with_manager(ManagerSetService::new(&rig.routed.volumes));
        let host = cw::RpcListener::start_async(listener_cfg(), SECRET.to_vec(), Arc::new(router))
            .expect("holder listener");
        let endpoint = host.endpoint().to_string();
        for vol in &rig.routed.volumes {
            let plane = vol.slot_leases().expect("armed");
            for id in appenders {
                plane.holders.set_endpoint(*id, &endpoint);
            }
        }
        let sink = Arc::new(ProbeSink {
            calls: AtomicU64::new(0),
        });
        let for_arm = Arc::clone(&sink);
        let arm = data_grant::arm_slot_custody(
            &rig.routed,
            WRITER,
            SECRET.to_vec(),
            0,
            Arc::new(move |_volume| Arc::clone(&for_arm) as Arc<dyn RecallDataSink>),
        );
        Self {
            host,
            endpoint,
            owner,
            arm,
            sink,
            ms,
        }
    }

    async fn tear_down(self) {
        data_grant::disarm_slot_custody().await;
        data_grant::uninstall_custody_owner();
        self.host.shutdown();
    }
}

/// Mint a REGULAR FILE under the root whose ino routes to forest slot
/// `slot` while the slot is still the manager's (a preset ino routes
/// itself).
async fn seed_file_in_slot(rig: &DataRig, slot: ForestSlot, name: &str) -> u64 {
    let vol = &rig.routed.volumes[0];
    let width = rig.routed.routing_width();
    let routing = u64::from(slot) - 1;
    let local = vol
        .allocate_guest_ino(routing as u16)
        .expect("a guest cursor");
    let global = make_global_ino_width(local, routing, width);
    let ino = rig
        .routed
        .create_with_rdev_preset(
            ROOT_INO,
            name,
            libc::S_IFREG | 0o644,
            1000,
            1000,
            0,
            0,
            Some(IntentCreatePreset {
                global_ino: global,
                ts_ns: KvMetaBackend::now_ns_pub(),
            }),
        )
        .await
        .expect("seed file")
        .ino;
    assert_eq!(ino, global);
    ino
}

/// A stamped volume with one file in `SLOT_B`, the slot released to
/// `Unleased` so the NEXT open's declared region 1 takes it: the two-
/// holder shape (appender 0 = the writer, appender 1 = the file's holder).
/// Returns `(uris, the foreign file's global ino)`.
async fn two_holder_volume(dir: &Path, data: &Path) -> (Vec<String>, u64) {
    let uris = vec![format_stamped_member(dir, "meta0").await];
    let rig = mount_data(&uris, data, &Knobs::armed()).await;
    let foreign = seed_file_in_slot(&rig, SLOT_B, "foreign").await;
    // A block bound at index 0 while the slot is still the manager's, so
    // the W1 probe has a reference to read.
    rig.publish_block(foreign, 0).await;
    let vol = Arc::clone(&rig.routed.volumes[0]);
    vol.release_slot_handover(0, SLOT_B)
        .await
        .expect("release to unleased");
    drop(vol);
    rig.shutdown().await;
    (uris, foreign)
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn lock_path(ino: u64) -> String {
    squeezefs::keys::inode_path(ino)
}

/// §5.10 rows 1 and 9 / gate 1's `dlm_rpcs == 0`: an OWN file's custody is
/// the local arbiter — no RPC, no holder dialed, the `dlm_custody` family
/// flat — while a file in a slot ANOTHER appender leases costs exactly ONE
/// grant from that holder, whose reply carried the file's records: the
/// writer's token cache holds them, the holder registered the token, and
/// no S9 authority client was ever installed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_own_file_costs_no_rpc_and_a_foreign_file_exactly_one_grant_with_its_token() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    assert_eq!(slot_of_global(&rig.routed, foreign), SLOT_B);
    let venue = Venue::stand_up(&rig, &[1]).await;
    assert!(data_grant::slot_custody_armed());
    assert!(
        data_grant::custody_client().is_none(),
        "no S9 authority client — the holder is the server"
    );

    // The own file: the local arbiter, nothing counted.
    let own = rig.mk_file("own").await;
    assert_ne!(slot_of_global(&rig.routed, own), SLOT_B);
    assert!(data_grant::slot_holder_home(own).is_none());
    let rpcs0 = squeezefs::dlm_slot::dlm_rpcs();
    let s0 = data_grant::stats();
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(own), None, Duration::from_secs(1))
        .await
        .expect("own custody is local");
    assert!(lease.is_held().await);
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        rpcs0,
        "dlm_rpcs == 0 for an own file"
    );
    let s1 = data_grant::stats();
    assert_eq!(
        (s1.grants, s1.via_slot_holder, s1.token_carried),
        (s0.grants, s0.via_slot_holder, s0.token_carried),
        "the custody family is flat for an own file"
    );
    assert_eq!(venue.owner.held(), 0, "the holder's authority saw nothing");
    drop(lease);

    // The foreign file: ONE grant from its slot holder, the token carried.
    let (_v, local) = rig.routed.route_ino(foreign);
    assert!(matches!(
        data_grant::slot_holder_home(foreign),
        Some(data_grant::CustodyHome::Holder { holder: 1, .. })
    ));
    let holder_plane = rig.routed.volumes[0].token_holder().unwrap().clone();
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    assert!(lease.is_held().await);
    assert_ne!(lease.fencing_token(), 0);
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        rpcs0 + 1,
        "exactly one lock round trip"
    );
    let s2 = data_grant::stats();
    assert_eq!(s2.grants, s1.grants + 1, "exactly one grant");
    assert_eq!(s2.via_slot_holder, s1.via_slot_holder + 1);
    assert_eq!(s2.token_carried, s1.token_carried + 1);
    assert_eq!(
        venue.owner.held(),
        1,
        "the grant lives on the holder's authority"
    );
    assert_eq!(venue.owner.stats().granted, 1);
    assert_eq!(venue.owner.stats().clients, 1, "one JOIN at this holder");
    let tokens = venue
        .arm
        .token_plane(&venue.endpoint, 0)
        .await
        .expect("the holder's token plane was dialed");
    assert!(
        tokens.holds(local),
        "the carried records are this writer's token"
    );
    assert_eq!(
        tokens.stats().grants,
        1,
        "the token came with the grant — no second RPC"
    );
    assert_eq!(
        holder_plane.outstanding(),
        1,
        "the holder registered the writer's token"
    );
    assert_eq!(holder_plane.stats().grants_served, 1);
    // A serve needs the recall channel FRESH (the reader's fail-closed
    // gate); the channel started at the dial and completes its first
    // round inside the holder's park bound.
    wait_until(
        "the writer's recall channel completes its first round",
        || tokens.stats().channel_fresh,
    )
    .await;
    let serve = tokens
        .serve(local, TokenWants::default())
        .await
        .expect("serve")
        .expect("the file exists");
    assert_eq!(serve.entry().attrs.mode & libc::S_IFMT, libc::S_IFREG);
    assert!(
        serve
            .entry()
            .xattrs
            .iter()
            .any(|(n, _)| n.as_slice() == b"layout"),
        "the carried token names the file's layout"
    );
    assert_eq!(tokens.stats().hits, 1, "the second read is a RAM hit");

    // A second acquire of the same file is a second grant at the same
    // holder: one JOIN, two grants, `already` for the token.
    drop(lease);
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    client.drain_releases().await;
    assert_eq!(venue.owner.held(), 0, "the release landed");
    let lease2 = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("re-acquire");
    assert_eq!(
        venue.owner.stats().clients,
        1,
        "still one lease at the holder"
    );
    assert_eq!(data_grant::stats().via_slot_holder, s2.via_slot_holder + 1);
    drop(lease2);
    client.drain_releases().await;
    venue.tear_down().await;
    rig.shutdown().await;
}

/// §5.7.1 / gate 5 through the custody path: the HOLDER's own commit on
/// the file recalls the writer's carried token before it lands (the
/// writer's recall channel acks after its data drain), and the writer's
/// next resolve re-fetches — exact, never bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_holders_publish_recalls_the_writers_token_and_the_writer_refetches() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let (_v, local) = rig.routed.route_ino(foreign);
    let holder_plane = rig.routed.volumes[0].token_holder().unwrap().clone();

    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    let tokens = venue.arm.token_plane(&venue.endpoint, 0).await.unwrap();
    assert!(tokens.holds(local));
    wait_until(
        "the writer's recall channel completes its first round",
        || tokens.stats().channel_fresh,
    )
    .await;
    let before = holder_plane.stats();

    // The holder's commit on the file (its own region's ring): the pass
    // recalls the writer's token and proceeds on the ack.
    Metadata::setattr(
        rig.routed.as_ref(),
        foreign,
        Some(0o600),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("the holder's setattr");
    let after = holder_plane.stats();
    assert!(
        after.recalls > before.recalls,
        "the commit recalled the token"
    );
    assert_eq!(after.recall_acks, after.recalls, "every recall acked");
    assert_eq!(after.timeouts_live, 0);
    assert_eq!(after.expired_with_lease, 0);
    let rs = tokens.stats();
    assert_eq!(rs.recalls_received, rs.recalls_acked);
    assert!(rs.recalls_received >= 1);
    assert!(
        venue.sink.calls.load(Ordering::SeqCst) >= 1,
        "the data drain ran before the ack"
    );
    assert!(!tokens.holds(local), "the recalled records are gone");

    // The writer's next resolve re-fetches — exact.
    let grants0 = tokens.stats().grants;
    let serve = tokens
        .serve(local, TokenWants::default())
        .await
        .expect("serve")
        .expect("the file exists");
    assert_eq!(
        serve.entry().attrs.mode & 0o777,
        0o600,
        "exact at the next resolve"
    );
    assert_eq!(tokens.stats().grants, grants0 + 1, "one re-fetch");
    assert!(lease.is_held().await, "custody was untouched by the recall");
    drop(lease);
    venue
        .arm
        .holder_client(&venue.endpoint)
        .await
        .unwrap()
        .drain_releases()
        .await;
    venue.tear_down().await;
    rig.shutdown().await;
}

/// §5.4.3 law 2 from the custody path: a file whose custody this mount
/// holds from its slot HOLDER never patches in place — the block's
/// references live in the holder's tree — whatever the probe would read
/// (a sole, unshared reference here); an OWN file keeps PR 7's durable
/// probe (sole ⇒ patch; a durable SHARED bit ⇒ CoW).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w1_declines_a_foreign_custody_file_and_keeps_the_durable_probe_for_own_files() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    assert!(rig.router.symmetric_armed());
    let tag = rig.tag();
    let (_v, local_f) = rig.routed.route_ino(foreign);
    // The foreign file's block: exactly one unshared durable reference.
    let refs = rig.vol().block_ref_scan(tag).await.unwrap();
    let r = refs
        .iter()
        .find(|r| r.owner_ino == local_f)
        .copied()
        .expect("the seeded reference");
    let off = r.block_idx * rig.alloc.chunk_size();
    let probe = rig
        .vol()
        .block_ref_probe_flags(tag, r.block_idx, Some(SLOT_B))
        .await
        .unwrap();
    assert_eq!(
        (probe.count, probe.shared),
        (1, false),
        "premise: sole and unshared"
    );
    assert!(
        !rig.router
            .sole_owner_durably(foreign, &rig.alloc, off)
            .await,
        "a foreign-custody file never patches in place"
    );
    // Holding the custody changes nothing: the file is still the holder's.
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    assert!(
        !rig.router
            .sole_owner_durably(foreign, &rig.alloc, off)
            .await
    );
    drop(lease);

    // An own file: PR 7's probe decides — sole ⇒ patch, SHARED ⇒ CoW.
    let own = rig.mk_file("own").await;
    let own_off = rig.publish_block(own, 0).await;
    assert!(
        rig.router
            .sole_owner_durably(own, &rig.alloc, own_off)
            .await
    );
    let (_v, own_local) = rig.routed.route_ino(own);
    rig.vol()
        .mark_block_ref_shared(&reference(
            tag,
            own_off / rig.alloc.chunk_size(),
            own_local,
            0,
        ))
        .await
        .unwrap();
    assert!(
        !rig.router
            .sole_owner_durably(own, &rig.alloc, own_off)
            .await
    );
    venue
        .arm
        .holder_client(&venue.endpoint)
        .await
        .unwrap()
        .drain_releases()
        .await;
    venue.tear_down().await;
    rig.shutdown().await;
}

/// §5.1.4 — custody across a handover: a slot whose file a writer holds
/// custody of from this holder does not move (the handover is DEFERRED —
/// the `Busy` class the cadence retries); once the writer releases, the
/// slot moves and the file's custody comes from the NEW holder — here the
/// manager itself, so the next acquire is local and counts nothing. A
/// grant never spans a handover: no write lands under a stale holder's
/// custody and no acked write is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custody_defers_a_handover_of_its_slot_and_moves_with_it_once_released() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let vol = Arc::clone(&rig.routed.volumes[0]);

    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    assert!(data_grant::slot_custody_live(
        u128::from_le_bytes(vol.superblock().uuid),
        SLOT_B
    ));
    let err = vol
        .release_slot_handover(1, SLOT_B)
        .await
        .expect_err("a slot with live custody does not move");
    assert!(
        matches!(&err, squeezefs::meta_backend::kv::KvError::Busy(why) if why.contains("custody")),
        "the deferral names the custody: {err:?}"
    );
    assert!(lease.is_held().await, "the writer's custody is untouched");
    assert_eq!(venue.owner.held(), 1);

    // The writer releases: the release lands at the holder, the slot moves.
    drop(lease);
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    client.drain_releases().await;
    assert_eq!(venue.owner.held(), 0);
    assert!(!data_grant::slot_custody_live(
        u128::from_le_bytes(vol.superblock().uuid),
        SLOT_B
    ));
    vol.release_slot_handover(1, SLOT_B)
        .await
        .expect("the handover proceeds once custody is released");
    assert!(
        data_grant::slot_holder_home(foreign).is_none(),
        "the slot is the manager's now — the file is OWN"
    );
    let rpcs0 = squeezefs::dlm_slot::dlm_rpcs();
    let via0 = data_grant::stats().via_slot_holder;
    let lease2 = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the new holder — local");
    assert!(lease2.is_held().await);
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        rpcs0,
        "the new holder is this mount: 0 RPCs"
    );
    assert_eq!(data_grant::stats().via_slot_holder, via0);
    assert_eq!(
        venue.owner.held(),
        0,
        "no grant at the old holder's authority"
    );
    drop(lease2);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// A dead HOLDER's custody: the writer's lease at it runs the S9 law —
/// past its own `T_self` (strictly before the holder's TTL) the writer
/// POISONS its custody (PR 8's `FenceClass::RemoteCustody`, never the
/// appender park), so no DMA lands after the holder may have re-granted;
/// the holder's sweep at its TTL retires the grant. The holder side of a
/// dead holder — its slots re-leased — is PR 10's recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_writer_poisons_at_t_self_when_its_holder_stops_answering_and_never_parks() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    let t_owner = venue
        .owner
        .lease_deadline_ms(WRITER)
        .expect("the holder tracks the writer's lease");
    assert!(
        !client.self_fence_due(),
        "premise: the writer's own deadline (T_self, strictly before the holder's TTL by the \
         S6 formula) has not passed"
    );
    // The holder stops answering: its listener is gone, so no renewal
    // can complete and the writer's cadence reaches T_self (the S9 loop's
    // act, pinned on its own clock in the S9 suite); the ACT is what this
    // pin classifies.
    venue.host.shutdown();
    assert!(
        !squeezefs::park_gate::is_parked(),
        "premise: nothing parked"
    );
    let fence = client.self_fence("test: the holder stopped answering past T_self");
    assert!(
        fence.poisoned_data_custody,
        "a WRITER's fence poisons custody"
    );
    assert!(data_custody::poisoned());
    assert!(
        !squeezefs::park_gate::is_parked(),
        "the RemoteCustody class never parks"
    );
    assert_eq!(squeezefs::park_gate::parks(), 0);
    assert!(
        data_custody::authorize_dma(None).is_err(),
        "no DMA lands after the writer fenced itself"
    );
    assert!(
        rig.router
            .dlm
            .acquire_lock(&lock_path(foreign), None, Duration::from_millis(200))
            .await
            .is_err(),
        "a poisoned writer acquires nothing"
    );
    // The holder's TTL passes: its sweep retires the grant.
    venue.ms.store(t_owner + 1, Ordering::SeqCst);
    let expired = venue.owner.expire_due();
    assert_eq!(expired.len(), 1, "the holder swept the dead writer's lease");
    assert_eq!(expired[0].client, WRITER);
    assert_eq!(venue.owner.held(), 0);
    drop(lease);
    data_grant::disarm_slot_custody().await;
    data_grant::uninstall_custody_owner();
    rig.shutdown().await;
}

/// `SQUEEZEFS_SYMMETRIC_META=0` is the shipped posture exactly, and a
/// bit-17-absent volume takes it verbatim: no slot-custody arm, every
/// acquire the local arbiter, the `dlm_custody` family's two PR-9 gauges
/// 0, `dlm_rpcs` 0, and an armed symmetric mount ships NO `pack_group`
/// frame (§5.4.3 — its promotions pack in its own slot's scope; the PK4
/// wire is the unarmed co-writer's, which is why it stays).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unarmed_and_flat_mounts_arm_nothing_and_count_nothing() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    // Every gauge is process-global (other contracts of this binary moved
    // them): the law is read as DELTAS across each mount's life.
    let frames = || {
        squeezefs::fuse_client::METRICS
            .pack_cowriter_frames
            .load(Ordering::Relaxed)
    };
    for (name, stamped) in [("flat", false), ("dark", true)] {
        let uris = vec![if stamped {
            format_stamped_member(dir.path(), name).await
        } else {
            common::sym::format_flat_member(dir.path(), name).await
        }];
        let rig = mount_data(&uris, data.path(), &Knobs::unarmed()).await;
        assert!(
            !data_grant::slot_custody_armed(),
            "{name}: nothing armed the slot-custody plane"
        );
        let f = rig.mk_file("f").await;
        assert!(data_grant::slot_holder_home(f).is_none());
        let (rpcs0, s0, frames0) = (
            squeezefs::dlm_slot::dlm_rpcs(),
            data_grant::stats(),
            frames(),
        );
        let lease = rig
            .router
            .dlm
            .acquire_lock(&lock_path(f), None, Duration::from_secs(1))
            .await
            .expect("local custody");
        assert!(lease.is_held().await);
        let _ = rig.publish_block(f, 0).await;
        assert_eq!(
            squeezefs::dlm_slot::dlm_rpcs(),
            rpcs0,
            "{name}: dlm_rpcs flat"
        );
        let s = data_grant::stats();
        assert_eq!(
            (s.via_slot_holder, s.token_carried, s.grants),
            (s0.via_slot_holder, s0.token_carried, s0.grants),
            "{name}: the custody family flat"
        );
        let json = data_grant::stats_json();
        assert_eq!(json["dlm_custody_via_slot_holder"], s.via_slot_holder);
        assert_eq!(json["dlm_custody_token_carried"], s.token_carried);
        assert_eq!(
            frames(),
            frames0,
            "{name}: no pack_group frame left this mount"
        );
        drop(lease);
        rig.shutdown().await;
    }
    // The armed SOLO mount: every slot its own — no holder dialed, no
    // pack_group frame, `dlm_rpcs` flat (gate 1's law).
    let uris = vec![format_stamped_member(dir.path(), "solo").await];
    let rig = mount_data(&uris, data.path(), &Knobs::armed()).await;
    let (rpcs0, s0, frames0) = (
        squeezefs::dlm_slot::dlm_rpcs(),
        data_grant::stats(),
        frames(),
    );
    let f = rig.mk_file("f").await;
    assert!(data_grant::slot_holder_home(f).is_none());
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(f), None, Duration::from_secs(1))
        .await
        .expect("local custody");
    let _ = rig.publish_block(f, 0).await;
    assert_eq!(frames(), frames0);
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        rpcs0,
        "gate 1: dlm_rpcs flat on a solo armed mount"
    );
    assert_eq!(data_grant::stats().via_slot_holder, s0.via_slot_holder);
    drop(lease);
    rig.shutdown().await;
}

/// The mount path's arm on a set without a cluster secret arms nothing
/// (loud), and the round-trip arm/disarm leaves the process clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_arm_needs_a_cluster_secret_and_disarms_clean() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let rig = mount_data(&uris, data.path(), &Knobs::armed()).await;
    let armed = data_grant::arm_mount_slot_custody(&rig.routed, &rig.router)
        .await
        .expect("the arm decides");
    assert!(
        !armed,
        "no job:enroll record — nothing to dial holders with"
    );
    assert!(!data_grant::slot_custody_armed());
    let _arm = data_grant::arm_slot_custody(
        &rig.routed,
        WRITER,
        SECRET.to_vec(),
        0,
        Arc::new(|_v| {
            Arc::new(ProbeSink {
                calls: AtomicU64::new(0),
            }) as Arc<dyn RecallDataSink>
        }),
    );
    assert!(data_grant::slot_custody_armed());
    data_grant::disarm_slot_custody().await;
    assert!(!data_grant::slot_custody_armed());
    rig.shutdown().await;
}
