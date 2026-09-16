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
}

impl Venue {
    async fn stand_up(rig: &DataRig, appenders: &[u32]) -> Self {
        Self::stand_up_on(&rig.routed, appenders).await
    }

    /// [`Self::stand_up`] over a routed set directly (the FUSE-layer rig's).
    async fn stand_up_on(routed: &Arc<RoutedMetaBackend>, appenders: &[u32]) -> Self {
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
            .with_tokens(TokenSetService::new(&routed.volumes))
            .with_manager(ManagerSetService::new(&routed.volumes));
        let host = cw::RpcListener::start_async(listener_cfg(), SECRET.to_vec(), Arc::new(router))
            .expect("holder listener");
        let endpoint = host.endpoint().to_string();
        for vol in &routed.volumes {
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
            routed,
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
        }
    }

    async fn tear_down(self) {
        data_grant::disarm_slot_custody().await;
        data_grant::uninstall_custody_owner();
        self.host.shutdown();
    }
}

impl Drop for Venue {
    /// Failure hygiene: a contract that panics mid-way must not leave its
    /// listener's threads holding the owner — whose arbiter leases live in
    /// the PROCESS-GLOBAL lock map — alive into the next contract (the
    /// next fresh volume mints the same inos, and its legit acquire read
    /// `conflicting custody` off this venue's stranded grant).
    fn drop(&mut self) {
        self.host.shutdown();
        self.owner.revoke_client(WRITER, "test venue dropped");
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
    let (uris, inos) = holders_volume(dir, data, &[(SLOT_B, "foreign")], 0).await;
    (uris, inos[0])
}

/// Forest slot 5 (routing slot 4) — the declared appender 2's, the second
/// holder of the N-holder contracts.
const SLOT_C: ForestSlot = 5;
const THREE_HOLDERS: &str = "1:4;2:5";

/// One `user.big.N` xattr's value: 15 KiB — under the fixture's KV value
/// cap (`min(64 KiB, node_size/4)` = 16 KiB at the 64 KiB test node).
const BIG_XATTR_BYTES: usize = 15 * 1024;

/// [`two_holder_volume`] generalized: one file per `(slot, name)`, each
/// slot released to `Unleased` for the next open's declared regions, and
/// — when `xattr_fill > 0` — that many [`BIG_XATTR_BYTES`] `user.big.N`
/// xattrs on every file (a carried-token xattr set wider than one grant
/// page). Returns `(uris, the files' global inos in `slots` order)`.
async fn holders_volume(
    dir: &Path,
    data: &Path,
    slots: &[(ForestSlot, &str)],
    xattr_fill: usize,
) -> (Vec<String>, Vec<u64>) {
    let uris = vec![format_stamped_member(dir, "meta0").await];
    let rig = mount_data(&uris, data, &Knobs::armed()).await;
    let mut inos = Vec::with_capacity(slots.len());
    for (slot, name) in slots {
        let ino = seed_file_in_slot(&rig, *slot, name).await;
        // A block bound at index 0 while the slot is still the manager's,
        // so the W1 probe has a reference to read.
        rig.publish_block(ino, 0).await;
        for i in 0..xattr_fill {
            rig.routed
                .setxattr(
                    ino,
                    &format!("user.big.{i:02}"),
                    &vec![b'x'; BIG_XATTR_BYTES],
                )
                .await
                .expect("a 15 KiB xattr fits the KV value cap");
        }
        inos.push(ino);
    }
    let vol = Arc::clone(&rig.routed.volumes[0]);
    let mut released: Vec<ForestSlot> = Vec::new();
    for (slot, _) in slots {
        if released.contains(slot) {
            continue;
        }
        vol.release_slot_handover(0, *slot)
            .await
            .expect("release to unleased");
        released.push(*slot);
    }
    drop(vol);
    rig.shutdown().await;
    (uris, inos)
}

/// A TCP endpoint that ACCEPTS and never answers — a holder that is up
/// on the network and dead on the wire (the dial parks to the cluster
/// wire's `DIAL_TIMEOUT`).
fn silent_endpoint() -> (std::net::TcpListener, String) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let ep = l.local_addr().expect("addr").to_string();
    (l, ep)
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
        err.to_string().contains("custody"),
        "the deferral names the custody: {err:?}"
    );
    assert!(
        !matches!(&err, squeezefs::meta_backend::kv::KvError::Busy(_)),
        "the deferral is not the D0 `Busy` refusal class (review round 2, Issue 5): {err:?}"
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

/// Review round 2, Issue 2 — **the arm's clean leave**: a writer holding a
/// foreign file's custody (grant + carried token) that leaves cleanly
/// leaves NOTHING outstanding at the holder — the token released (drain +
/// purge first, the token plane's one release path), the custody grant
/// released, in that order — so the holder's next commit on the file
/// recalls nobody and no dead-client recall runs to its deadline (the
/// class PR 5 closed for readers). `dlm_custody_held` / the holder's
/// `outstanding()` read 0 after the leave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clean_leave_releases_every_carried_token_and_custody_grant_at_the_holder() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let holder_plane = rig.routed.volumes[0].token_holder().unwrap().clone();

    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    assert_eq!(venue.owner.held(), 1);
    assert_eq!(holder_plane.outstanding(), 1);
    // The FUSE layer's cached lease is still alive at unmount (the
    // session ended; nothing dropped it yet) — the leave must release the
    // grant regardless.
    let recalls0 = holder_plane.stats().recalls;
    let expired0 = holder_plane.stats().expired_with_lease;

    data_grant::disarm_slot_custody().await;

    assert!(!data_grant::slot_custody_armed());
    assert_eq!(
        venue.owner.held(),
        0,
        "the clean leave released the writer's custody grant at the holder"
    );
    assert_eq!(
        holder_plane.outstanding(),
        0,
        "the clean leave released the writer's carried token at the holder"
    );
    assert!(
        !lease.is_held().await,
        "the local handle reads the release (the owner's decision)"
    );
    // The holder's next commit on the file: nothing to recall, nobody to
    // wait for.
    rig.publish_block(foreign, 1).await;
    let s = holder_plane.stats();
    assert_eq!(
        s.recalls, recalls0,
        "no recall issued for a departed writer"
    );
    assert_eq!(
        s.expired_with_lease, expired0,
        "no dead-client recall ran to the lease deadline"
    );
    drop(lease);
    data_grant::uninstall_custody_owner();
    venue.host.shutdown();
    rig.shutdown().await;
}

/// Review round 2, Issue 5 — **the deferral's class**: a handover deferred
/// for a live custody grant is the RETRYABLE class (`EAGAIN` — the wire's
/// `Deferred`, the requester's `AcquireSlot` retries), never the `Busy` /
/// `EINVAL` refusal a wire requester reads as terminal; the deferral is
/// counted on `slot_handover_custody_deferrals`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handover_deferred_for_custody_is_the_retryable_class_the_requester_retries() {
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
    let err = vol
        .release_slot_handover(1, SLOT_B)
        .await
        .expect_err("a slot with live custody does not move yet");
    let surfaced: squeezefs::error::SqueezefsError = err.into();
    assert!(
        matches!(
            &surfaced,
            squeezefs::error::SqueezefsError::Refused { errno, .. } if *errno == libc::EAGAIN
        ),
        "the deferral is the retryable EAGAIN class, not a refusal: {surfaced:?}"
    );
    assert!(lease.is_held().await);
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// Review round 2, Issue 4 — a custody grant whose carried xattr set is
/// WIDER than one grant page (`xattrs_complete == false`) is never
/// installed as a complete token: the writer pages the remainder from the
/// holder and installs the COMPLETE set (a `listxattr` off the token then
/// names every xattr), counted `dlm_custody_token_carried` once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_carried_grant_whose_xattrs_exceed_one_page_is_paged_to_completion() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    // 40 × 15 KiB = 600 KiB > the 512 KiB grant page budget: two xattr
    // pages.
    const FILL: usize = 40;
    let (uris, inos) = holders_volume(dir.path(), data.path(), &[(SLOT_B, "wide")], FILL).await;
    let foreign = inos[0];
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let (_v, local) = rig.routed.route_ino(foreign);
    let s0 = data_grant::stats();
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(5))
        .await
        .expect("custody from the slot holder");
    let s1 = data_grant::stats();
    assert_eq!(s1.token_carried, s0.token_carried + 1, "the token landed");
    let tokens = venue.arm.token_plane(&venue.endpoint, 0).await.unwrap();
    assert!(tokens.holds(local), "the carried token is resident");
    wait_until(
        "the writer's recall channel completes its first round",
        || tokens.stats().channel_fresh,
    )
    .await;
    let grants_before_serve = tokens.stats().grants;
    let serve = tokens
        .serve(local, TokenWants::default())
        .await
        .expect("serve")
        .expect("the file exists");
    assert_eq!(
        tokens.stats().grants,
        grants_before_serve,
        "the serve is a RAM hit — the carried token was complete, no re-fetch"
    );
    let big = serve
        .entry()
        .xattrs
        .iter()
        .filter(|(n, _)| n.starts_with(b"user.big."))
        .count();
    assert_eq!(
        big, FILL,
        "the installed token carries the COMPLETE xattr set, not the first page"
    );
    assert!(
        serve
            .entry()
            .xattrs
            .iter()
            .any(|(n, _)| n.as_slice() == b"layout"),
        "and the layout"
    );
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// Review round 2, Issue 3 — **the wire word `object` is screened before
/// the arbiter sees it** (PR 3's bounded-execution law): a slot word past
/// the forest codec's bound, a control record, a slot this volume's
/// forest does not name, or an ino with no durable record is REJECTED
/// (the buggy/hostile-peer class — its own status, never a grant, never
/// a panic), and the holder's arbiter never takes a lease on an ino the
/// peer did not name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wire_word_object_is_screened_before_the_arbiter_sees_it() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let (_v, local) = rig.routed.route_ino(foreign);
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("the legit grant dials the holder");
    let granted0 = venue.owner.stats().granted;
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    let holder_plane = rig.routed.volumes[0].token_holder().unwrap().clone();
    let rejected0 = holder_plane.stats().custody_rejected;

    // (a) a slot word past FOREST_SLOT_MAX — the codec's debug_assert /
    // release truncation; (b) a control record; (c) a slot the forest does
    // not name (2^16 - 1 is never minted); (d) a nameable slot, no record.
    let past_bound = ((u64::from(u16::MAX) + 2) << 40) | 5;
    let unnamed_slot = (u64::from(u16::MAX) << 40) | 5;
    let no_record = (local & !((1u64 << 40) - 1)) | 0x00ff_ffff_ffff;
    for (what, object) in [
        ("slot past the codec's bound", past_bound),
        ("a control record", 1),
        ("a slot the forest never named", unnamed_slot),
        ("a nameable slot with no durable record", no_record),
    ] {
        let err = client
            .acquire_carrying_token(
                0,
                object,
                None,
                squeezefs::dlm::LockMode::Exclusive,
                Duration::from_secs(1),
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{what}: object {object:#x} was GRANTED"));
        assert!(
            matches!(
                &err,
                squeezefs::error::SqueezefsError::Refused { errno, msg }
                    if *errno == libc::EIO && msg.contains("rejected")
            ),
            "{what}: a deterministic REJECTION, never a transport error or a retry class: {err:?}"
        );
    }
    assert_eq!(
        venue.owner.stats().granted,
        granted0,
        "the arbiter never took a lease for a screened word"
    );
    assert_eq!(venue.owner.held(), 1, "only the legit grant is held");
    assert_eq!(
        holder_plane.stats().custody_rejected,
        rejected0 + 4,
        "every screened word counted on the rejected class (dlm_token_custody_rejected)"
    );
    assert!(lease.is_held().await, "the legit grant is untouched");
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// Review round 2, Issue 6 — **a dead holder never delays an acquire
/// against a live one**: the dial of a holder is single-flight PER HOLDER
/// (joiners of one holder park on ITS dial; no process-wide lock spans
/// any I/O), so a holder that accepts and never answers (its dial parks
/// to the wire's 10 s bound) costs the files of OTHER holders nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_holder_never_delays_an_acquire_against_a_live_one() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, inos) = holders_volume(
        dir.path(),
        data.path(),
        &[(SLOT_B, "live-holders-file"), (SLOT_C, "dead-holders-file")],
        0,
    )
    .await;
    let (live_file, dead_file) = (inos[0], inos[1]);
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(THREE_HOLDERS)).await;
    assert_eq!(slot_of_global(&rig.routed, dead_file), SLOT_C);
    let venue = Venue::stand_up(&rig, &[1]).await;
    let (_silent, dead_endpoint) = silent_endpoint();
    rig.routed.volumes[0]
        .slot_leases()
        .unwrap()
        .holders
        .set_endpoint(2, &dead_endpoint);
    assert!(matches!(
        data_grant::slot_holder_home(dead_file),
        Some(data_grant::CustodyHome::Holder { holder: 2, .. })
    ));

    // The dead holder's acquire parks in its dial.
    let dlm = rig.router.dlm.clone();
    let path = lock_path(dead_file);
    let parked =
        tokio::spawn(async move { dlm.acquire_lock(&path, None, Duration::from_secs(2)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !parked.is_finished(),
        "premise: the dead holder's dial is parked"
    );

    // The live holder's acquire must not wait behind it.
    let started = std::time::Instant::now();
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(live_file), None, Duration::from_secs(2))
        .await
        .expect("custody from the LIVE holder");
    let wall = started.elapsed();
    assert!(
        wall < Duration::from_secs(3),
        "the live holder's acquire waited {wall:?} — behind the dead holder's dial"
    );
    assert!(lease.is_held().await);
    assert_eq!(venue.owner.held(), 1);
    drop(lease);
    // The dead dial resolves on its own bound (an error, never a grant).
    let dead = tokio::time::timeout(Duration::from_secs(20), parked)
        .await
        .expect("the dead dial gave up inside the wire's bound")
        .expect("task");
    assert!(
        dead.is_err(),
        "no custody from a holder that never answered"
    );
    venue.tear_down().await;
    rig.shutdown().await;
}

/// One LIVE slot holder among N in this process (the N-holder contracts —
/// review round 2, Issues 7 and 10): its own S9 custody authority (its own
/// era and lease-epoch counter), its own listener whose token service
/// arbitrates custody on THAT authority, registered as appender
/// `appender`'s endpoint on the plane. Shares the venue's manual clock.
struct Holder {
    host: Arc<cw::RpcListener>,
    endpoint: String,
    owner: Arc<WriteCustodyOwner>,
}

impl Holder {
    /// `term_bump` sets the holder's era above the process's; `prejoin`
    /// throwaway JOINs raise its lease-epoch counter (a "busier" holder).
    async fn stand_up(
        rig: &DataRig,
        appender: u32,
        ms: &Arc<AtomicU64>,
        term_bump: u64,
        prejoin: usize,
    ) -> Self {
        let (c, clock) = clocks(ms);
        let owner = WriteCustodyOwner::arm(
            &format!("slot-holder-{appender}"),
            squeezefs::dlm::durable_term() + term_bump,
            squeezefs::dlm::durable_term(),
            c,
            clock,
            None,
        )
        .expect("the holder's custody authority arms");
        for i in 0..prejoin {
            owner
                .join(&data_grant::JoinFrame {
                    schema: data_grant::CUSTODY_SCHEMA,
                    client: format!("filler-{appender}-{i}"),
                    pr_key: 0,
                    prior_epoch: None,
                })
                .expect("a filler join");
        }
        let router = AsyncVerbRouter::new()
            .with_custody(Arc::clone(&owner))
            .with_tokens(TokenSetService::with_custody_owner(
                &rig.routed.volumes,
                Arc::clone(&owner),
            ))
            .with_manager(ManagerSetService::new(&rig.routed.volumes));
        let host = cw::RpcListener::start_async(listener_cfg(), SECRET.to_vec(), Arc::new(router))
            .expect("holder listener");
        let endpoint = host.endpoint().to_string();
        for vol in &rig.routed.volumes {
            vol.slot_leases()
                .expect("armed")
                .holders
                .set_endpoint(appender, &endpoint);
        }
        Self {
            host,
            endpoint,
            owner,
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        self.host.shutdown();
        self.owner.revoke_client(WRITER, "test holder dropped");
    }
}

/// The two-live-holder venue: holder A (appender 1, slot 4) at the
/// process's era, holder B (appender 2, slot 5) two eras up and six joins
/// busier; the arm installed on a MANUAL clock shared with both owners.
async fn two_live_holders(rig: &DataRig) -> (Holder, Holder, Arc<AtomicU64>, Arc<SlotCustodyArm>) {
    let ms = Arc::new(AtomicU64::new(1_000));
    let a = Holder::stand_up(rig, 1, &ms, 1, 0).await;
    let b = Holder::stand_up(rig, 2, &ms, 3, 6).await;
    let arm = data_grant::arm_slot_custody_with_clock(
        &rig.routed,
        WRITER,
        SECRET.to_vec(),
        0,
        Arc::new(|_v| {
            Arc::new(ProbeSink {
                calls: AtomicU64::new(0),
            }) as Arc<dyn RecallDataSink>
        }),
        LeaseClock::manual(Arc::clone(&ms)),
    );
    (a, b, ms, arm)
}

/// Review round 2, Issue 7 — **the custody generation is per holder**: a
/// grant adopted from a BUSIER holder (a higher lease-epoch counter, a
/// higher era) never moves this process's custody epoch, so a DMA
/// authorized under another holder's grant — captured at write-pipeline
/// admission, submitted later — is never refused (`data_dma_epoch_refusals`
/// stays 0; a refusal there is a `FenceDrop`, acked data lost). The
/// process's durable term is untouched too (the holder's era is its
/// volume's, one of N).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_from_a_busier_holder_never_voids_dma_in_flight_under_another() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, inos) = holders_volume(
        dir.path(),
        data.path(),
        &[(SLOT_B, "a-file"), (SLOT_C, "b-file")],
        0,
    )
    .await;
    let (file_a, file_b) = (inos[0], inos[1]);
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(THREE_HOLDERS)).await;
    let (a, b, _ms, _arm) = two_live_holders(&rig).await;
    let refusals = || {
        squeezefs::fuse_client::METRICS
            .data_dma_epoch_refusals
            .load(Ordering::Relaxed)
    };
    let gen0 = data_custody::custody_generation();
    let term0 = squeezefs::dlm::durable_term();

    // Custody of A's file: the write pipeline captures the epoch at
    // admission (the permit is the carrier) — here, the same read.
    let lease_a = rig
        .router
        .dlm
        .acquire_lock(&lock_path(file_a), None, Duration::from_secs(2))
        .await
        .expect("custody from holder A");
    let in_flight = data_custody::current_epoch();
    assert_eq!(a.owner.held(), 1);

    // Custody of B's file — B's lease epoch for this writer is 7 (six
    // fillers joined first) against A's 1, and B's era is two up.
    let lease_b = rig
        .router
        .dlm
        .acquire_lock(&lock_path(file_b), None, Duration::from_secs(2))
        .await
        .expect("custody from holder B");
    assert_eq!(b.owner.held(), 1);
    assert!(
        b.owner.stats().clients > 6,
        "premise: B is the busier holder (its lease-epoch counter is higher)"
    );

    // The DMA under A's grant submits now.
    let refusals0 = refusals();
    assert!(
        data_custody::authorize_dma(Some(in_flight)).is_ok(),
        "a DMA authorized under holder A's grant is refused after adopting holder B's — \
         the two holders' epochs were folded into one process word"
    );
    assert_eq!(refusals(), refusals0, "zero epoch refusals");
    assert_eq!(
        data_custody::custody_generation(),
        gen0,
        "no holder's lease epoch moved the process generation"
    );
    assert_eq!(
        squeezefs::dlm::durable_term(),
        term0,
        "no holder's era moved the process's durable term"
    );
    assert!(lease_a.is_held().await && lease_b.is_held().await);
    drop(lease_a);
    drop(lease_b);
    data_grant::disarm_slot_custody().await;
    assert_eq!(a.owner.held(), 0);
    assert_eq!(b.owner.held(), 0);
    rig.shutdown().await;
}

/// Review round 2, Issues 10 + 16 — **a dead holder's `T_self` fence is
/// scoped to that holder's custody**, and it is the renewal CADENCE that
/// fires it (the listener down, the manual clock past `T_self`): holder
/// A's grants are dead, this mount's generation advances, A's token plane
/// serves nothing, the arm forgets A — and the mount LIVES: nothing is
/// poisoned, no park, holder B's file keeps its custody and a fresh
/// acquire against B succeeds. A's TTL sweep retires the dead lease. What
/// remains for PR 10 (the death ledger re-leasing A's slots) and PR 12
/// (the per-object epoch capture) is stated in the note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_holders_t_self_fence_is_scoped_to_its_own_custody_and_driven_by_the_cadence() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, inos) = holders_volume(
        dir.path(),
        data.path(),
        &[(SLOT_B, "a-file"), (SLOT_C, "b-file")],
        0,
    )
    .await;
    let (file_a, file_b) = (inos[0], inos[1]);
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(THREE_HOLDERS)).await;
    let (a, b, ms, arm) = two_live_holders(&rig).await;
    let (_v, local_a) = rig.routed.route_ino(file_a);
    let lease_a = rig
        .router
        .dlm
        .acquire_lock(&lock_path(file_a), None, Duration::from_secs(2))
        .await
        .expect("custody from holder A");
    let lease_b = rig
        .router
        .dlm
        .acquire_lock(&lock_path(file_b), None, Duration::from_secs(2))
        .await
        .expect("custody from holder B");
    let client_a = arm.holder_client(&a.endpoint).await.expect("A dialed");
    let tokens_a = arm.token_plane(&a.endpoint, 0).await.expect("A's plane");
    assert!(tokens_a.holds(local_a));
    let t_self_a = client_a.t_self_deadline_ms();
    let t_owner_a = a
        .owner
        .lease_deadline_ms(WRITER)
        .expect("A tracks the writer");
    assert!(
        t_self_a < t_owner_a,
        "premise: T_self is strictly before the holder's TTL"
    );
    let s0 = data_grant::stats();
    let gen0 = data_custody::custody_generation();

    // Holder A dies: its listener is gone, and the clock passes T_self.
    // The renewal cadence's failed renewal past T_self is what fences —
    // this contract calls no fence itself. `fenced()` is the fence's
    // COMPLETION word (review round 3, Issue 27: set after its work — the
    // grants marked dead, the generation advanced, the planes stopped,
    // the holder forgotten — never before it).
    a.host.shutdown();
    ms.store(t_self_a + 1, Ordering::SeqCst);
    wait_until("holder A's custody client fences itself at T_self", || {
        client_a.fenced()
    })
    .await;

    // The fence is SCOPED: A's custody is gone, the mount lives.
    assert!(
        !data_custody::poisoned(),
        "one dead holder never poisons the mount"
    );
    assert!(
        !squeezefs::park_gate::is_parked(),
        "the RemoteCustody class never parks"
    );
    assert!(
        data_custody::authorize_dma(None).is_ok(),
        "fresh DMA authorizations are still minted"
    );
    let s1 = data_grant::stats();
    assert_eq!(
        s1.holder_fences,
        s0.holder_fences + 1,
        "counted on its own gauge"
    );
    assert_eq!(
        s1.self_fences, s0.self_fences,
        "never on the poison's gauge"
    );
    assert!(
        data_custody::custody_generation() > gen0,
        "the S9 law for a lost lease: the generation advanced (in-flight DMA under A's grants \
         is refused at the device gate — the one-word carrier's cost, stated in the note)"
    );
    assert!(!lease_a.is_held().await, "A's grant is dead on this side");
    assert!(
        !tokens_a.holds(local_a),
        "A's token plane serves nothing after its holder died"
    );
    assert!(
        arm.holder_client(&a.endpoint).await.is_none(),
        "the arm forgot the dead holder"
    );
    // Holder B is untouched: its grant stands and a fresh acquire lands.
    assert!(lease_b.is_held().await, "B's custody is untouched");
    let via0 = data_grant::stats().via_slot_holder;
    drop(lease_b);
    let client_b = arm.holder_client(&b.endpoint).await.expect("B dialed");
    client_b.drain_releases().await;
    let lease_b2 = rig
        .router
        .dlm
        .acquire_lock(&lock_path(file_b), None, Duration::from_secs(2))
        .await
        .expect("a fresh acquire against the LIVE holder");
    assert!(lease_b2.is_held().await);
    assert_eq!(data_grant::stats().via_slot_holder, via0 + 1);
    // A's file: the dead holder is re-dialed and refused (its slots are
    // re-leased by PR 10's recovery) — an EAGAIN-class refusal, never a
    // poison, never a `LockFailed` the POSIX ladder retries for its
    // whole budget.
    let err = rig
        .router
        .dlm
        .acquire_lock(&lock_path(file_a), None, Duration::from_millis(500))
        .await
        .expect_err("no custody from a dead holder");
    assert!(
        matches!(
            &err,
            squeezefs::error::SqueezefsError::Refused { errno, .. } if *errno == libc::EAGAIN
        ),
        "the dead holder's refusal is typed EAGAIN: {err:?}"
    );
    assert!(!data_custody::poisoned());
    // A's TTL passes: its sweep retires the dead writer's lease.
    ms.store(t_owner_a + 1, Ordering::SeqCst);
    let expired = a.owner.expire_due();
    assert_eq!(expired.len(), 1, "holder A swept the fenced writer's lease");
    assert_eq!(a.owner.held(), 0);
    drop(lease_a);
    drop(lease_b2);
    data_grant::disarm_slot_custody().await;
    assert_eq!(b.owner.held(), 0, "the clean leave released B's grants");
    rig.shutdown().await;
}

/// Review round 2, Issue 8 — a carried token whose INSTALL fails after
/// custody was granted keeps the custody: the caller adopts the lease it
/// was granted (the holder holds exactly that grant), the failure is
/// counted on `dlm_custody_token_carry_failures`, and the token is fetched
/// at the next serve.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_token_install_never_throws_away_granted_custody() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let (_v, local) = rig.routed.route_ino(foreign);
    let s0 = data_grant::stats();
    squeezefs::meta_ship::token_plane::TEST_INSTALL_CARRIED_FAIL_ONCE.store(true, Ordering::SeqCst);
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody is the caller's whatever became of the token");
    assert!(
        !squeezefs::meta_ship::token_plane::TEST_INSTALL_CARRIED_FAIL_ONCE.load(Ordering::SeqCst),
        "premise: the seam fired"
    );
    assert!(lease.is_held().await);
    assert_eq!(
        venue.owner.held(),
        1,
        "the holder holds exactly the grant the caller adopted"
    );
    let s1 = data_grant::stats();
    assert_eq!(s1.via_slot_holder, s0.via_slot_holder + 1);
    assert_eq!(s1.token_carried, s0.token_carried, "no token landed");
    assert_eq!(s1.token_carry_failures, s0.token_carry_failures + 1);
    let tokens = venue.arm.token_plane(&venue.endpoint, 0).await.unwrap();
    assert!(!tokens.holds(local), "nothing installed");
    // The next serve fetches the token the ordinary way.
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
    assert!(tokens.holds(local), "fetched at the next serve");
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// Review round 2, Issue 15 — a `NotHolder` answer re-resolves through
/// tree 0: when the slot was handed to THIS mount under the acquire, the
/// acquire answers `NowLocal` and the lock manager falls to its local
/// arbiter (never an `Unbound` refusal naming appender 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_not_holder_answer_for_a_slot_now_ours_falls_to_the_local_arbiter() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let vol = Arc::clone(&rig.routed.volumes[0]);
    // The stale view: resolved while appender 1 still leased the slot.
    let stale = data_grant::slot_holder_home(foreign).expect("foreign while leased to 1");
    assert!(matches!(
        &stale,
        data_grant::CustodyHome::Holder { holder: 1, .. }
    ));
    // The slot moves to THIS mount (the manager) under the writer's view.
    vol.release_slot_handover(1, SLOT_B)
        .await
        .expect("no custody yet — the slot moves");
    assert!(
        data_grant::slot_holder_home(foreign).is_none(),
        "premise: the slot is ours now"
    );
    // One process shares tree 0 between the holder's plane and the
    // writer, so the holder's own view cannot lag the writer's: the seam
    // answers the cross-process `NotHolder { holder: 0 }` once.
    squeezefs::meta_ship::token_plane::TEST_CUSTODY_NOT_HOLDER_ONCE.store(0, Ordering::SeqCst);
    let granted0 = venue.owner.stats().granted;
    let out = data_grant::acquire_at_slot_holder(
        stale,
        foreign,
        None,
        squeezefs::dlm::LockMode::Exclusive,
        Duration::from_secs(2),
    )
    .await
    .expect("a NotHolder for a slot now ours is not a refusal");
    assert!(
        matches!(out, data_grant::HolderAcquire::NowLocal),
        "the acquire answers NowLocal — the caller's local arbiter serves"
    );
    assert_eq!(
        venue.owner.stats().granted,
        granted0,
        "the old holder granted nothing"
    );
    // Through the lock manager end to end: local custody, no RPC counted
    // past the redirect's own.
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("local custody");
    assert!(lease.is_held().await);
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// Review round 2, Issues 5 + 9 — **the deferral is BOUNDED by a recall**
/// (design §5.1.4 flush-then-transfer): the first deferred handover
/// RECALLS the slot's live grants through the S9 pull channel; the writer
/// absorbs the recall on its standing notice poll, marks the grant dead
/// locally (its next write re-acquires) and releases once quiescent; a
/// re-acquire at the old holder meanwhile is DEFERRED (the recall cannot be
/// undone); the requester's next attempt finds the grants gone and the
/// slot moves — within one renewal beat of the recall, whatever the
/// writer's open-file lifetime. Nothing was voided: no DMA refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_handover_recalls_the_custody_and_completes_within_a_beat() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let vol = Arc::clone(&rig.routed.volumes[0]);
    let refusals = || {
        squeezefs::fuse_client::METRICS
            .data_dma_epoch_refusals
            .load(Ordering::Relaxed)
    };
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    let in_flight = data_custody::current_epoch();
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    let s0 = data_grant::stats();
    let deferrals0 = data_grant::HANDOVER_CUSTODY_DEFERRALS.load(Ordering::Relaxed);
    let refusals0 = refusals();

    // Attempt 1: deferred, the grant recalled.
    let err = vol
        .release_slot_handover(1, SLOT_B)
        .await
        .expect_err("deferred while the writer holds custody");
    assert!(
        matches!(
            &err,
            squeezefs::meta_backend::kv::KvError::HandoverDeferred(_)
        ),
        "the typed deferral: {err:?}"
    );
    assert_eq!(
        data_grant::HANDOVER_CUSTODY_DEFERRALS.load(Ordering::Relaxed),
        deferrals0 + 1
    );
    assert_eq!(venue.owner.recalled(), 1, "the holder recalled the grant");
    assert_eq!(
        data_grant::handover_recalls_pending(),
        1,
        "the slot is mid-handover"
    );

    // The writer's standing notice poll absorbs the recall and releases.
    wait_until("the recalled writer releases at the holder", || {
        venue.owner.held() == 0
    })
    .await;
    assert_eq!(
        data_grant::stats().recalls_absorbed,
        s0.recalls_absorbed + 1
    );
    assert!(!lease.is_held().await, "the local handle reads the recall");
    // A re-acquire at the OLD holder mid-handover is deferred, never
    // granted (the recall cannot be undone); the acquire's own retry loop
    // answers EAGAIN inside its budget.
    let err = client
        .acquire_carrying_token(
            0,
            rig.routed.route_ino(foreign).1,
            None,
            squeezefs::dlm::LockMode::Exclusive,
            Duration::from_millis(100),
        )
        .await;
    assert!(
        matches!(err, Ok(data_grant::CarriedAcquire::Deferred { .. })),
        "the old holder defers a grant on a slot mid-handover"
    );
    assert_eq!(venue.owner.held(), 0, "nothing granted");

    // Attempt 2 (the cadence's next tick): the grants are gone, the slot
    // moves; the mid-handover mark clears with it.
    vol.release_slot_handover(1, SLOT_B)
        .await
        .expect("the handover completes once the recalled custody is released");
    assert_eq!(data_grant::handover_recalls_pending(), 0);
    assert!(
        data_grant::slot_holder_home(foreign).is_none(),
        "the slot is the manager's now"
    );
    // Nothing was voided by the recall: the recall is not the
    // `dead_grants` revocation — the process generation did not move, so
    // no DMA authorized under ANY other grant is refused (round 3, Issue
    // 20: a straggler under the RECALLED grant cannot exist — the FUSE
    // layer drains every in-flight custody use before the release
    // departs, pinned by the FUSE-layer contract below — so no epoch
    // retire is needed for it).
    assert_eq!(refusals(), refusals0, "no DMA refusal for a recall");
    assert_eq!(
        data_custody::current_epoch(),
        in_flight,
        "the recall moved no process word"
    );
    // The file's next custody comes from the NEW holder — local.
    let lease2 = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the new holder");
    assert!(lease2.is_held().await);
    drop(lease);
    drop(lease2);
    venue.tear_down().await;
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

/// Review round 3, Issue 20 — **the recall reaches the FUSE layer's cached
/// lease**: a writer holding a foreign file's custody through the FUSE
/// layer (`active_leases`, held from the first write to the last close)
/// keeps WRITING; the slot's handover recalls the grant; the FUSE layer's
/// cached lease is REVOKED (the one accessor refuses it — the busy writer's
/// next write re-acquires: `CUSTODY_DEFERRED` at the old holder, the NEW
/// holder after the move), the in-flight custody uses drain, the acked
/// bytes flush, THEN the grant is released — within the bound, whatever
/// the writer's cadence — and the handover completes. No write is lost or
/// refused, no write lands under the released grant (the fencing token
/// after the move is a fresh local custody's, never the recalled grant's),
/// and a second file in the slot written during the window is DELAYED,
/// never failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recalled_writers_cached_lease_is_revoked_and_its_writes_reacquire_at_the_new_holder() {
    use fuse3::raw::Filesystem;
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, inos) = holders_volume(
        dir.path(),
        data.path(),
        &[(SLOT_B, "busy"), (SLOT_B, "other")],
        0,
    )
    .await;
    let (busy, other) = (inos[0], inos[1]);
    let rig = common::sym::mount_fuse(
        &uris,
        data.path(),
        &Knobs::armed().partition(TWO_HOLDERS),
        "32MB",
    )
    .await;
    let venue = Venue::stand_up_on(&rig.routed, &[1]).await;
    rig.fs.install_slot_custody_hooks();
    let vol = Arc::clone(&rig.routed.volumes[0]);
    let refusals = || {
        squeezefs::fuse_client::METRICS
            .data_dma_epoch_refusals
            .load(Ordering::Relaxed)
    };
    let refusals0 = refusals();
    let s0 = data_grant::stats();

    // The first write acquires custody from the slot holder and CACHES
    // the lease in the FUSE layer.
    rig.write_at(busy, 0, &common::sym::pattern(1, 4096)).await;
    assert_eq!(venue.owner.held(), 1, "custody from the slot holder");
    let grant_token = rig.fs.router.dlm.get_fencing_token_ino(busy);
    assert_eq!(
        rig.fs.cached_lease_token(busy),
        Some(grant_token),
        "the FUSE layer caches the grant's lease"
    );
    assert_eq!(rig.fs.custody_uses(busy), 0, "no op in flight");

    // The BUSY writer: a write every 10 ms until told to stop; every
    // write must succeed (a blocked write is fine, a failed one is not).
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let last = Arc::new(AtomicU64::new(1));
    let errors = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(1));
    let writer = {
        let fs = Arc::clone(&rig.fs);
        let (stop, last, errors, writes) = (
            Arc::clone(&stop),
            Arc::clone(&last),
            Arc::clone(&errors),
            Arc::clone(&writes),
        );
        tokio::spawn(async move {
            let mut i = 2usize;
            while !stop.load(Ordering::SeqCst) {
                let payload = bytes::Bytes::from(common::sym::pattern(i, 4096));
                match fs
                    .write(common::sym::req(), busy, 0, 0, payload, 0, 0)
                    .await
                {
                    Ok(_) => {
                        last.store(i as u64, Ordering::SeqCst);
                        writes.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => {
                        eprintln!("busy writer: write {i} failed: {e:?}");
                        errors.fetch_add(1, Ordering::SeqCst);
                    }
                }
                i += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    wait_until("the busy writer is writing", || {
        writes.load(Ordering::SeqCst) >= 5
    })
    .await;

    // The handover: deferred, the grant recalled.
    let err = vol
        .release_slot_handover(1, SLOT_B)
        .await
        .expect_err("deferred while the writer holds custody");
    assert!(matches!(
        &err,
        squeezefs::meta_backend::kv::KvError::HandoverDeferred(_)
    ));
    assert_eq!(venue.owner.recalled(), 1, "the grant was recalled");

    // The writer half: the cached lease is revoked, the in-flight uses
    // drain, the release lands — the busy writer notwithstanding.
    wait_until("the FUSE layer's cached lease is revoked", || {
        rig.fs.cached_lease_token(busy) != Some(grant_token)
    })
    .await;
    wait_until("the recalled grant's release lands at the holder", || {
        venue.owner.held() == 0
    })
    .await;
    assert_eq!(
        rig.fs.recalled_leases_pending(),
        0,
        "the parked lease was settled (its uses drained) before the release"
    );
    assert!(
        data_grant::handover_recalls_pending() >= 1,
        "the slot is mid-handover until the move"
    );
    // A second file of the slot written inside the window: DELAYED to
    // the move, never failed.
    let other_write = {
        let fs = Arc::clone(&rig.fs);
        tokio::spawn(async move {
            fs.write(
                common::sym::req(),
                other,
                0,
                0,
                bytes::Bytes::from(common::sym::pattern(77, 4096)),
                0,
                0,
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !other_write.is_finished(),
        "premise: the second file's grant is deferred while the slot is mid-handover"
    );

    // The requester's next tick: the grants are gone, the slot moves.
    vol.release_slot_handover(1, SLOT_B)
        .await
        .expect("the handover completes once the recalled custody is released");
    wait_until("the slot is this mount's", || {
        data_grant::slot_holder_home(busy).is_none()
    })
    .await;
    let other_written = tokio::time::timeout(Duration::from_secs(20), other_write)
        .await
        .expect("the second file's write completes after the move")
        .expect("task");
    assert!(
        other_written.is_ok(),
        "the second file's write landed at the new holder: {other_written:?}"
    );
    assert_eq!(data_grant::handover_recalls_pending(), 0);

    // The busy writer kept writing throughout; stop it and audit.
    let writes_before_stop = writes.load(Ordering::SeqCst);
    wait_until("the writer lands writes at the new holder", || {
        writes.load(Ordering::SeqCst) > writes_before_stop + 3
    })
    .await;
    stop.store(true, Ordering::SeqCst);
    writer.await.expect("writer task");
    assert_eq!(
        errors.load(Ordering::SeqCst),
        0,
        "no write failed across the recall and the handover"
    );
    let token_after = rig.fs.router.dlm.get_fencing_token_ino(busy);
    assert_ne!(
        token_after, grant_token,
        "every write after the move runs under a FRESH local custody — never the released grant"
    );
    assert_eq!(
        rig.fs.cached_lease_token(busy),
        Some(token_after),
        "the FUSE layer caches the new custody"
    );
    assert_eq!(venue.owner.held(), 0, "nothing held at the old holder");
    assert_eq!(
        data_grant::stats().recalls_absorbed,
        s0.recalls_absorbed + 1
    );
    assert_eq!(refusals(), refusals0, "no DMA refusal: nothing was voided");
    // The last acked write is what the file reads.
    let want = common::sym::pattern(last.load(Ordering::SeqCst) as usize, 4096);
    assert_eq!(
        rig.read(busy, 4096).await,
        want,
        "the last write is durable"
    );
    venue.tear_down().await;
    data_grant::uninstall_recall_hooks();
    data_grant::uninstall_release_gate();
    rig.shutdown().await;
}

/// Round 3's ×10 stamped finding (run 4 of the FUSE-layer contract:
/// `held() == 1` at the OLD holder after the move) — **a grant never lands
/// at the old holder INSIDE the flush-then-transfer**: the completing tick
/// finds no live grant and proceeds; a recalled writer's retry (or any
/// first acquire of a file in the slot) arriving inside the transfer
/// window must be DEFERRED to the slot's next holder, never granted at
/// this one — else the grant spans the move. The handover is parked
/// mid-transfer (after its page, before tree 0) by the PR 4 seam while a
/// foreign acquire arrives; the mark stands until the handover's terminal
/// outcome and clears with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_inside_the_flush_then_transfer_is_deferred_to_the_next_holder() {
    use squeezefs::meta_backend::kv::backend::{
        test_handover_park_release, test_handover_parked, TEST_HANDOVER_PARK_AFTER_PAGE,
    };
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let vol = Arc::clone(&rig.routed.volumes[0]);
    // Dial the holder through one acquire, released before the handover.
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    drop(lease);
    wait_until("the release lands at the holder", || {
        venue.owner.held() == 0
    })
    .await;

    // The handover parks mid-transfer: the page names the slot Releasing,
    // tree 0 does not yet — the exact window the ×10 run's grant landed in.
    let parked0 = test_handover_parked();
    TEST_HANDOVER_PARK_AFTER_PAGE.store(true, Ordering::Relaxed);
    let handover = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move { vol.release_slot_handover(1, SLOT_B).await })
    };
    wait_until("the handover parks after its page", || {
        test_handover_parked() > parked0
    })
    .await;
    assert!(
        data_grant::handover_recalls_pending() >= 1,
        "the completing handover HOLDS the slot's mark through the transfer"
    );

    // A foreign acquire inside the window: deferred at the old holder,
    // nothing granted there.
    let inside = client
        .acquire_carrying_token(
            0,
            rig.routed.route_ino(foreign).1,
            None,
            squeezefs::dlm::LockMode::Exclusive,
            Duration::from_millis(100),
        )
        .await;
    assert!(
        matches!(inside, Ok(data_grant::CarriedAcquire::Deferred { .. })),
        "a grant inside the transfer is deferred to the next holder (got {})",
        match &inside {
            Ok(data_grant::CarriedAcquire::Granted { .. }) => "Granted".to_string(),
            Ok(data_grant::CarriedAcquire::NotHolder { .. }) => "NotHolder".to_string(),
            Ok(data_grant::CarriedAcquire::Deferred { .. }) => "Deferred".to_string(),
            Err(e) => format!("Err({e})"),
        }
    );
    assert_eq!(
        venue.owner.held(),
        0,
        "the old holder granted nothing inside the transfer"
    );

    // The handover completes; the mark clears with its terminal outcome.
    test_handover_park_release();
    handover
        .await
        .expect("task")
        .expect("the handover completes");
    assert_eq!(data_grant::handover_recalls_pending(), 0);
    assert!(
        data_grant::slot_holder_home(foreign).is_none(),
        "the slot is the manager's now"
    );
    // The blocked write's re-acquire lands LOCALLY at the new holder.
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody at the new holder");
    assert!(lease.is_held().await);
    assert_eq!(venue.owner.held(), 0, "nothing at the old holder");
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// Review round 3, Issue 21 — **the recall is STATE, re-gathered on every
/// carrier**: a carrier whose reply the writer loses (the seam drops one
/// absorbed batch) does not orphan the recall — it lands on the NEXT
/// carrier (the standing poll's next round / the renewal), counted once
/// at the holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_recall_carrier_re_travels_the_recall_on_the_next_one() {
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
    let s0 = data_grant::stats();
    // The next carrier's recalls are DROPPED at the writer (a lost reply).
    data_grant::TEST_DROP_RECALL_CARRIER_ONCE.store(true, Ordering::SeqCst);
    let err = vol
        .release_slot_handover(1, SLOT_B)
        .await
        .expect_err("deferred while the writer holds custody");
    assert!(matches!(
        &err,
        squeezefs::meta_backend::kv::KvError::HandoverDeferred(_)
    ));
    wait_until("the seam fired (one carrier lost)", || {
        !data_grant::TEST_DROP_RECALL_CARRIER_ONCE.load(Ordering::SeqCst)
    })
    .await;
    // The recall re-travels on the next carrier (the standing poll's next
    // round — within milliseconds; the renewal at the latest) — no second
    // recall issued at the holder, one absorbed at the writer.
    wait_until("the recall lands on the next carrier", || {
        venue.owner.held() == 0
    })
    .await;
    assert_eq!(venue.owner.recalled(), 1, "recalled ONCE at the holder");
    assert_eq!(
        data_grant::stats().recalls_absorbed,
        s0.recalls_absorbed + 1,
        "absorbed once at the writer"
    );
    assert!(!lease.is_held().await);
    vol.release_slot_handover(1, SLOT_B)
        .await
        .expect("the handover completes");
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// Review round 3, Issue 22 — **the holder's clean leave recalls the
/// grants it issued** before its slots go `Unleased`: a writer holding a
/// foreign file's custody from this holder sees the recall (the leave's
/// own recall — the handover's one mechanism), releases, and only then do
/// the slots move; the next lessee (the manager, here) grants the file
/// afresh — no window in which two custodies of one file exist. The leave
/// is bounded by `T_owner + renew` (the S9 sweep); a grant that survives
/// it keeps its slot leased.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holders_clean_leave_recalls_its_grants_before_its_slots_go_unleased() {
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
    assert_eq!(venue.owner.held(), 1);
    let s0 = data_grant::stats();
    let recalled0 = venue.owner.recalled();

    // The HOLDER (declared region 1 of this set) leaves cleanly while the
    // writer holds its grant: the leave recalls, the writer releases, the
    // slot goes Unleased.
    let started = std::time::Instant::now();
    rig.shutdown().await;
    let wall = started.elapsed();
    assert_eq!(
        venue.owner.recalled(),
        recalled0 + 1,
        "the leave recalled the grant it issued"
    );
    assert_eq!(
        venue.owner.held(),
        0,
        "the release landed BEFORE the slot went Unleased (the leave waited for it)"
    );
    assert_eq!(
        data_grant::stats().recalls_absorbed,
        s0.recalls_absorbed + 1
    );
    assert!(
        !lease.is_held().await,
        "the writer's handle reads the recall"
    );
    assert!(
        wall < Duration::from_secs(15),
        "the leave waited for the release, not for T_owner: {wall:?}"
    );
    drop(lease);
    data_grant::disarm_slot_custody().await;
    data_grant::uninstall_custody_owner();
    venue.host.shutdown();

    // The next open: the slot is Unleased — the manager takes it and the
    // file's next custody is local (the new holder), nothing at the old.
    let rig = mount_data(&uris, data.path(), &Knobs::armed()).await;
    assert!(
        data_grant::slot_holder_home(foreign).is_none(),
        "the slot is the manager's now"
    );
    let lease2 = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the new holder — local");
    assert!(lease2.is_held().await);
    drop(lease2);
    rig.shutdown().await;
}

/// Review round 3, Issues 24/25 — the handover's parks and bounds are
/// DERIVED from the S9 lease clocks and published: the writer's retry park
/// is one twentieth of its renewal beat, the mid-handover mark stands two
/// beats (`slot_handover_recall_bound_ms`), the leave waits `T_owner +
/// renew`; every one tie-tested here against the clocks in force.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_handovers_parks_and_bounds_derive_from_the_lease_clocks() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let renew = venue.owner.clocks().renew_interval;
    let t_owner = venue.owner.clocks().t_owner;
    assert_eq!(
        data_grant::handover_recall_bound_ms(),
        (renew * 2).as_millis() as u64,
        "the published mark bound is 2 × the authority's renewal beat"
    );
    assert_eq!(data_grant::handover_retry_park_for(renew), renew / 20);
    assert_eq!(data_grant::handover_recall_bound_for(renew), renew * 2);
    assert_eq!(
        data_grant::leave_custody_bound_for(t_owner, renew),
        t_owner + renew
    );
    // The writer derives the same words from its lease at the holder.
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .expect("custody from the slot holder");
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    // The writer's words are millisecond-granular (the lease frame's).
    let renew_ms = Duration::from_millis(renew.as_millis() as u64);
    assert_eq!(
        client.handover_retry_park(),
        data_grant::handover_retry_park_for(renew_ms)
    );
    assert_eq!(
        client.handover_recall_bound(),
        data_grant::handover_recall_bound_for(renew_ms)
    );
    drop(lease);
    venue.tear_down().await;
    rig.shutdown().await;
}

/// **The scoping instrument** (`#[ignore]`d — PR 6's precedent; the note's
/// §6 row, dev box = SCOPING): the wall of N foreign-file acquires with the
/// token CARRIED (one round trip: custody + records) against N plain S9
/// acquires on the custody wire PLUS N separate token grants (the two
/// round trips the carriage folds into one), and the handover-with-custody
/// cost: the deferral's refusal, then the release + the handover itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "scoping instrument — run once for the evidence note"]
async fn scoping_row_grant_rtt_with_and_without_the_carried_token() {
    let _g = SEAM.lock().await;
    let _restore = Restore;
    let dir = tempdir().unwrap();
    let data = sym_data_file();
    let (uris, foreign) = two_holder_volume(dir.path(), data.path()).await;
    let rig = mount_data(&uris, data.path(), &Knobs::armed().partition(TWO_HOLDERS)).await;
    let venue = Venue::stand_up(&rig, &[1]).await;
    let (_v, local) = rig.routed.route_ino(foreign);
    const N: u32 = 200;

    // Carried: custody + records in one round trip (the first dial JOINs).
    let t0 = std::time::Instant::now();
    for _ in 0..N {
        let lease = rig
            .router
            .dlm
            .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
            .await
            .expect("carried acquire");
        drop(lease);
        venue
            .arm
            .holder_client(&venue.endpoint)
            .await
            .unwrap()
            .drain_releases()
            .await;
    }
    let carried = t0.elapsed();
    let client = venue.arm.holder_client(&venue.endpoint).await.unwrap();
    let tokens = venue.arm.token_plane(&venue.endpoint, 0).await.unwrap();
    wait_until("channel fresh", || tokens.stats().channel_fresh).await;

    // Uncarried: the plain S9 acquire on the custody wire, then the token
    // as its own Grant (the reader plane's fetch after a drop).
    let t1 = std::time::Instant::now();
    for _ in 0..N {
        let lease = client
            .acquire(
                foreign,
                None,
                squeezefs::dlm::LockMode::Exclusive,
                Duration::from_secs(2),
            )
            .await
            .expect("plain acquire");
        tokens.test_drop_entry(local);
        let _ = tokens
            .serve(local, TokenWants::default())
            .await
            .expect("token grant")
            .expect("exists");
        drop(lease);
        client.drain_releases().await;
    }
    let uncarried = t1.elapsed();

    // Handover with custody: the deferral, the release, the handover.
    let vol = Arc::clone(&rig.routed.volumes[0]);
    let lease = rig
        .router
        .dlm
        .acquire_lock(&lock_path(foreign), None, Duration::from_secs(2))
        .await
        .unwrap();
    let t2 = std::time::Instant::now();
    assert!(vol.release_slot_handover(1, SLOT_B).await.is_err());
    let deferral = t2.elapsed();
    drop(lease);
    client.drain_releases().await;
    let t3 = std::time::Instant::now();
    vol.release_slot_handover(1, SLOT_B).await.unwrap();
    let handover = t3.elapsed();
    eprintln!(
        "SCOPING carried {N} acquires: {:?} ({:.1} µs/op); uncarried (acquire + token grant): \
         {:?} ({:.1} µs/op); handover deferral {:?}; release+handover {:?}; custody phases {}",
        carried,
        carried.as_micros() as f64 / f64::from(N),
        uncarried,
        uncarried.as_micros() as f64 / f64::from(N),
        deferral,
        handover,
        data_grant::phase_json()
    );
    venue.tear_down().await;
    rig.shutdown().await;
}
