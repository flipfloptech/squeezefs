//! **The kvmap multi-writer serve-arm hazards — PR 5a** of the PB-class
//! file ladder (`docs/design-kvmap-block-map-tree.md` §11, Rev 1.5): the
//! three SILENT-WRONG serve arms a kvmap ino can reach on the mw planes,
//! plus the local-train screen gap. The full S11 ∘ kvmap compose,
//! claims-scoped shipped trains and the f36b recompute twin are **PR 5b**
//! — every refusal pinned here must NAME it.
//!
//! Contracts pinned here (one per §11 hazard row):
//!
//! 1. **The authority-local episode compose** (§11 row 1 — the WORST arm,
//!    no wire needed): `fetch_durable_layout_head` read a `kvmap:` head as
//!    an EMPTY map (`block_map: None` by format law), so the finding-35b
//!    claims diff mass-deleted every live tree-7 record on the authority's
//!    own next publish of a range-episode kvmap ino. Fixed for real: the
//!    head resolves through the SHARED tree-7 extraction, and a
//!    range-episode publish keeps every unclaimed live binding.
//! 2. **The S11 scoped Put** (§11 row 2): a kvmap head's base decode is
//!    Err by design, so it fell through the undecodable-base "legacy
//!    verbatim" arm — the shipper's stale inline map overwrote the head
//!    and orphaned every tree record. Screened: refuses loud, nothing
//!    committed, PR 5b named.
//! 3. **The chained shipped merge** (§11 row 3): `head_indirect` gates on
//!    the WORD "indirect" in the decode error, so a kvmap base reached
//!    `use_delta` and staged a versioned delta ONTO the delta-ineligible
//!    kvmap base — delayed read-side poison at the next fold. Screened in
//!    BOTH arms (direct + aggregated pass), refusing before any staging.
//! 4. **f34 parity on the LOCAL train** (§11 further-fact b): the serve
//!    arm refuses whole-map trains under live range grants; the local arm
//!    did not. A NEW crossing under live grants STANDS DOWN to the legacy
//!    blob arm (the crossing simply does not happen yet under S11); a
//!    sticky-head local train refuses loud (a blob fallback would regress
//!    the head).
//!
//! Harness: the kvmap_crossing_tests rig (real v3 meta volume + real
//! file-backed data volume + the router that binds them) and its
//! shipped-verb two-node section (sandbox / start_authority / arm_client),
//! plus the mw_cowriter_free_tests range-custody fixtures
//! (`WriteCustodyClient::acquire_range` — the live-grant probe the f34
//! screen keys on).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::data_custody;
use squeezefs::data_grant::{self, RangeAcquireOutcome, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::METRICS;
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_map::MapEntry;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::kv::META_KV_JOURNAL_ENTRIES;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::meta_ship::{self as ship, publish, OwnerMap, PeerOwner};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
/// Sparse on purpose (the crossing legs allocate > 1000 offsets without
/// writing most of them — the kvmap_crossing fixture's shape).
const DATA_LEN: u64 = 32 * 1024 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-00000000000000c5";
/// Enough mapped blocks to push the encoded map past the 64 KiB-node
/// volume's ~16 KiB xattr cap — the crossing trigger.
const SPILL_BLOCKS: u32 = 1200;
const BLOCK: u64 = 4 * 1024 * 1024;

/// The `job:enroll`-class storage-trust secret both halves prove
/// possession of.
const SECRET: &[u8] = b"kvmap-mw-hazard-storage-trust-secret";
const NODE: &str = "node-kvmap-hazard-a";

// ---------------------------------------------------------------------------
// Serialization + posture restoration (process-global custody state)
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

/// Restores every process-global posture this file can move, so a
/// panicking assertion never leaves the binary armed or range-latched.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
        squeezefs::meta_ship::tokens::test_clear_range_cache();
        squeezefs::dlm::test_clear_range_episodes();
    }
}

// ---------------------------------------------------------------------------
// The single-node rig (the kvmap_crossing_tests fixture: real v3 meta
// volume, real file-backed data volume, the router that binds them)
// ---------------------------------------------------------------------------

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format WITHOUT bit 16 (the crossing owns the stamp) and WITHOUT bit 9
/// unless stamped below — immune to the `SQUEEZEFS_TEST_STAMP_*` seams.
async fn format_meta(path: &Path) {
    format_v3(path, META_LEN, &opts())
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
}

/// [`format_meta`] + bit 16 (the kvmap tree) + bit 9 (the durable-ref
/// ledger, so the C8 oracle grades every leg's accounting).
async fn format_meta_kvmap(path: &Path) {
    format_meta(path).await;
    assert!(set_block_refcounts_bit(path).await.expect("stamp bit 9"));
    assert!(
        set_block_map_tree_bit(path).await.expect("stamp bit 16"),
        "a fresh format must NOT already carry bit 16"
    );
}

struct Rig {
    router: DataRouter,
    alloc: Arc<BlockAllocator>,
    routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

async fn mount(meta: &Path, data: &Path) -> Rig {
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
    let router = DataRouter::new(dlm, cache, alloc.clone(), nvme);
    router.set_meta_backend(routed.clone());
    Rig {
        router,
        alloc,
        routed,
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

impl Rig {
    fn kv(&self) -> &Arc<KvMetaBackend> {
        &self.routed.volumes[0]
    }

    async fn mk_file(&self, name: &str) -> u64 {
        self.routed
            .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    /// [`Self::mk_file`] at a per-test DISTINCT ino: the range-custody
    /// lock objects key on `inode_{ino}` in a process-global table, and a
    /// PANICKED sibling test's un-drained grant on the same low ino would
    /// cascade "conflicting custody" refusals into every later fixture
    /// (fresh volumes re-mint ino 2 forever).
    async fn mk_file_at(&self, name: &str, fillers: u32) -> u64 {
        for i in 0..fillers {
            self.mk_file(&format!("filler-{name}-{i}")).await;
        }
        self.mk_file(name).await
    }

    fn token(&self, ino: u64) -> u64 {
        self.router.dlm.get_fencing_token_ino(ino)
    }

    /// Allocate `n` blocks and bind them at indices `0..n` in ONE merge —
    /// the crossing trigger.
    async fn publish_spill(&self, ino: u64, n: u32) -> Vec<(u32, String)> {
        let mut entries: Vec<(u32, String)> = Vec::new();
        for b in 0..n {
            let offset = self.alloc.allocate_block().await.expect("allocate");
            self.alloc.publish_block(offset);
            entries.push((b, offset.to_string()));
        }
        self.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&entries),
                u64::from(n) * BLOCK,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.token(ino),
            )
            .await
            .expect("merge a crossing map");
        entries
    }

    /// The durable layout head, decoded (bincode — kvmap/inline heads).
    async fn durable_head(&self, ino: u64) -> LayoutMetadata {
        let bytes = self
            .kv()
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout exists");
        bincode::deserialize(&bytes).expect("bincode head")
    }

    /// Every tree-7 record of `ino`, decoded to key strings.
    async fn tree_records(&self, ino: u64) -> Vec<(u32, String)> {
        let default_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
        let mut out = Vec::new();
        let mut cursor = 0u32;
        loop {
            let page = self
                .kv()
                .block_map_range(ino, cursor, 512)
                .await
                .expect("tree scan");
            let Some(last) = page.last().map(|(i, _)| *i) else {
                break;
            };
            for (idx, entry) in page {
                let key = match entry {
                    MapEntry::String(bytes) => String::from_utf8(bytes).expect("utf8 key"),
                    MapEntry::Point { vol_tag, offset } => {
                        assert_eq!(vol_tag, default_tag, "the rig's one data volume");
                        offset.to_string()
                    }
                };
                out.push((idx, key));
            }
            cursor = match last.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }
        out
    }

    /// The C8 oracle: durable-vs-derived, exact or drifting.
    async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.router
            .backend_router
            .verify_durable_block_refs(&self.routed)
            .await
            .expect("verification pass")
    }

    async fn shutdown(self) {
        self.routed.volumes[0]
            .shutdown()
            .await
            .expect("clean shutdown");
    }
}

fn journal_entries() -> u64 {
    META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The custody authority (the kvmap_crossing shipped-verb harness's
// start_authority, reused for both the single-node and two-node legs)
// ---------------------------------------------------------------------------

struct Authority {
    listener: Arc<squeezefs::cluster_wire::RpcListener>,
    owner: Arc<WriteCustodyOwner>,
    endpoint: String,
}

fn start_authority(inner: Arc<RoutedMetaBackend>) -> Authority {
    let ms = Arc::new(AtomicU64::new(1_000));
    let clock = LeaseClock::manual(Arc::clone(&ms));
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("positive T_self");
    let owner = WriteCustodyOwner::arm(
        "kvmap-hazard-authority",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("the custody authority arms");
    data_grant::install_custody_owner(Arc::clone(&owner));
    let router = data_grant::AsyncVerbRouter::new()
        .with_custody(Arc::clone(&owner))
        .with_publish(publish::PublishService::new(inner));
    let listener = squeezefs::cluster_wire::RpcListener::start_async(
        squeezefs::cluster_wire::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            service_threads: 2,
            ..squeezefs::cluster_wire::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(router),
    )
    .expect("the authority listens");
    let endpoint = listener.endpoint().to_string();
    Authority {
        listener,
        owner,
        endpoint,
    }
}

/// A peer holder's LIVE range grant on `ino` (the mw_cowriter_free_tests
/// range-custody fixture): the connected client and the held lease — the
/// owner's `ino_has_range_grants` probe (the f34 screen) reads true while
/// the lease lives, and the authority-side dlm's sticky range-episode
/// latch (the finding-35b compose gate) is set by the grant's own
/// arbitration.
async fn live_range_grant(
    endpoint: &str,
    node: &str,
    ino: u64,
) -> (Arc<WriteCustodyClient>, squeezefs::dlm::LockLease) {
    let client = WriteCustodyClient::connect(endpoint, SECRET, node)
        .await
        .expect("the holder dials the custody authority");
    let lease = match client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(3))
        .await
        .expect("the holder's ranged grant")
    {
        RangeAcquireOutcome::New { lease, .. } => lease,
        other => panic!("a fresh ino's first ranged ask must be NEW, got {other:?}"),
    };
    (client, lease)
}

// ===========================================================================
// 1. §11 row 1 — the authority-local episode compose (the WORST arm)
// ===========================================================================

/// Contract (§11 row 1, RED pre-fix): the AUTHORITY's own range-episode
/// publish of a kvmap ino COMPOSES its claims onto the durable head — and
/// the durable head of a `kvmap:` ino is the TREE, not the head record's
/// `block_map: None`. Pre-fix, `fetch_durable_layout_head` read the head
/// as an EMPTY map, the claims diff composed exactly this save's claim
/// set, and the publish mass-deleted every live unclaimed tree-7 record
/// (on the pre-fix tip the composed 1-entry map even regressed the head
/// to INLINE). The fixed shape: every unclaimed live binding survives,
/// the head stays `kvmap:1`, the new claim lands, and the C8 oracle reads
/// zero drift — WITH the grant still LIVE, which also pins the
/// compose-window exemption of the §11 row-4 local-train screen (the
/// compose-fed train is the one legal whole-map train under live grants).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_episode_publish_on_a_kvmap_ino_keeps_every_unclaimed_live_binding() {
    let _serial = serial();
    let _restore = Restore;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file_at("episode.bin", 0).await;

    // The crossing first — no custody plane yet (a plain local crossing).
    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1")
    );

    // The custody plane arms and a peer takes a LIVE range grant — the
    // ino is now a range-episode ino on the authority.
    let auth = start_authority(Arc::clone(&rig.routed));
    let (holder, lease) = live_range_grant(&auth.endpoint, NODE, ino).await;
    assert!(
        auth.owner.ino_has_range_grants(ino),
        "fixture: the grant is LIVE on the owner"
    );
    assert!(
        squeezefs::dlm::ino_has_range_custody(ino),
        "fixture: the grant's arbitration latched the sticky episode"
    );

    // The authority's own next publish: ONE claimed transition (a fresh
    // block at index SPILL_BLOCKS).
    let extra_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(extra_off);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(SPILL_BLOCKS, extra_off.to_string())]),
            u64::from(SPILL_BLOCKS + 1) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("the range-episode publish composes onto the tree");

    // The head is STILL kvmap (a compose fed an empty head regressed it
    // to a 1-entry inline map pre-fix)…
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1"),
        "the composed publish must keep the sticky kvmap head"
    );
    // …every unclaimed live binding survives, plus the claimed one…
    let mut expect = entries.clone();
    expect.push((SPILL_BLOCKS, extra_off.to_string()));
    assert_eq!(
        rig.tree_records(ino).await,
        expect,
        "the compose must keep every UNCLAIMED live tree-7 record (§11 row 1's \
         mass-delete is exactly this assertion failing)"
    );
    // …and the accounting stayed exact.
    assert!(
        rig.drift().await.is_empty(),
        "zero C8 drift — the composed publish's swap diff matches the tree diff"
    );

    drop(lease);
    holder.drain_releases().await;
    auth.listener.shutdown();
    rig.shutdown().await;
}

// ===========================================================================
// 4a. §11 row 4 — a NEW crossing under live range grants stands down
// ===========================================================================

/// Contract (§11 further-fact b, the NEW-crossing half; RED pre-fix): a
/// beyond-inline publish on an ino OTHER writers hold live ranges on must
/// NOT cross to kvmap — S11 scoped Puts on a kvmap head refuse until
/// PR 5b builds the S11 ∘ kvmap compose, so flipping the head mid-episode
/// would wedge every ranged peer's publishes. The crossing STANDS DOWN to
/// the legacy blob arm (today's shipped S11 behavior): an `indirect:`
/// head, ZERO tree-7 records, the crossing counter untouched, and the C8
/// oracle clean. Pre-fix the crossing ran and flipped the head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_crossing_under_live_range_grants_stands_down_to_the_blob_arm() {
    let _serial = serial();
    let _restore = Restore;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert!(rig.kv().block_map_tree_engaged());
    let ino = rig.mk_file_at("standdown.bin", 4).await;

    let auth = start_authority(Arc::clone(&rig.routed));
    let (holder, lease) = live_range_grant(&auth.endpoint, NODE, ino).await;
    assert!(auth.owner.ino_has_range_grants(ino));

    let inos_before = METRICS.map_migrate_inos.load(Ordering::Relaxed);
    rig.publish_spill(ino, SPILL_BLOCKS).await;

    let head = rig.durable_head(ino).await;
    assert!(
        head.block_map_id
            .as_deref()
            .is_some_and(|id| id.starts_with("indirect:")),
        "a new crossing under live range grants takes the LEGACY BLOB arm \
         (§11 row 4's stand-down) — got head {:?}",
        head.block_map_id
    );
    assert!(
        rig.tree_records(ino).await.is_empty(),
        "no tree-7 records — the crossing did not happen"
    );
    assert_eq!(
        METRICS.map_migrate_inos.load(Ordering::Relaxed),
        inos_before,
        "the crossing counter must not move on a stood-down publish"
    );
    assert!(
        rig.drift().await.is_empty(),
        "the blob arm's accounting is exact"
    );

    drop(lease);
    holder.drain_releases().await;
    auth.listener.shutdown();
    rig.shutdown().await;
}

// ===========================================================================
// 4b. §11 row 4 — a sticky-head LOCAL train under live grants refuses
// ===========================================================================

/// Contract (§11 further-fact b, the STICKY half; RED pre-fix): the
/// serve arm refuses a whole-map train under live range grants (the f34
/// screen); the LOCAL train must refuse the same way — a stood-down blob
/// fallback is WRONG here (the ino is already kvmap-headed and sticky, a
/// blob save would regress the head), and running the train would revert
/// peers' entries by delete-by-absence. Refuses loud, names PR 5b,
/// counted on `map_refused`, NOTHING committed (journal-entry equality;
/// head and records byte-identical). Pre-fix the local arm ran the train
/// and mass-deleted the un-shipped records.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sticky_head_local_train_under_live_range_grants_refuses_loud() {
    let _serial = serial();
    let _restore = Restore;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file_at("sticky.bin", 8).await;

    // The crossing first (no grants yet).
    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);
    let head_bytes = rig
        .kv()
        .getxattr(ino, "layout")
        .await
        .expect("layout read")
        .expect("kvmap head");

    let auth = start_authority(Arc::clone(&rig.routed));
    let (holder, lease) = live_range_grant(&auth.endpoint, NODE, ino).await;
    assert!(auth.owner.ino_has_range_grants(ino));

    // The LOCAL train, outside any compose window, carrying a SUBSET map
    // — pre-fix its diff deletes every record the subset does not name.
    let refused_before = publish::stats().map_refused;
    let journal_before = journal_entries();
    let out = publish::migrate_block_map(
        &rig.routed,
        ino,
        &head_bytes,
        u64::from(SPILL_BLOCKS) * BLOCK,
        vec![entries[0].clone()],
        Vec::new(),
        512,
    )
    .await;
    let err = out.expect_err(
        "a sticky-head LOCAL whole-map train under live range grants must refuse \
         loud (the f34 screen's local twin) — running it reverts peers' entries",
    );
    assert!(
        format!("{err}").contains("PR 5b"),
        "the refusal names PR 5b (the S11 ∘ kvmap compose): {err}"
    );
    assert_eq!(
        publish::stats().map_refused,
        refused_before + 1,
        "the refusal is counted (map_refused)"
    );
    assert_eq!(
        journal_entries(),
        journal_before,
        "NOTHING committed — the refusal fires before the train stages"
    );
    assert_eq!(
        rig.kv()
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("kvmap head"),
        head_bytes,
        "the head is byte-identical"
    );
    assert_eq!(
        rig.tree_records(ino).await,
        entries,
        "every tree-7 record survives the refused train"
    );

    drop(lease);
    holder.drain_releases().await;
    auth.listener.shutdown();
    rig.shutdown().await;
}

// ===========================================================================
// The two-node harness (the kvmap_crossing_tests shipped-verb fixture: one
// authority backend + listener, one all-foreign client backend, a real
// custody join)
// ===========================================================================

const VOL_LEN: u64 = 256 * 1024 * 1024;

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

async fn sandbox(dir: &Path, tag: &str) -> Arc<RoutedMetaBackend> {
    let plan = squeezefs::meta_backend::plan_meta_slot_set(1).expect("derived plan");
    let p = make_file(dir, &format!("{tag}-meta0"), VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &p,
        VOL_LEN,
        &FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
        plan.stamps[0].clone(),
    )
    .await
    .expect("format meta volume");
    squeezefs::meta_backend::open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("open routed set")
}

async fn shutdown_set(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

async fn arm_client(
    auth: &Authority,
    client_be: &Arc<RoutedMetaBackend>,
) -> (Arc<WriteCustodyClient>, Arc<publish::PublishClient>) {
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("kvmap-hazard-authority", &auth.endpoint)))
        .collect();
    ship::arm_ownership(OwnerMap::for_volumes(client_be, foreign).expect("owner map"));
    let pc = publish::PublishClient::new(NODE, SECRET.to_vec());
    publish::install_client(Arc::clone(&pc));
    let client = WriteCustodyClient::connect(&auth.endpoint, SECRET, NODE)
        .await
        .expect("the co-writer joins the custody plane");
    data_grant::install_custody_client(Arc::clone(&client));
    (client, pc)
}

fn kvmap_head_bytes(size: u64) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: Some("kvmap:1".into()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: None,
    })
    .expect("serialize kvmap head")
}

fn inline_head_bytes(size: u64, map: Vec<(u32, String)>) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map.into_iter().collect()),
    })
    .expect("serialize inline head")
}

/// A shipped create at a per-test DISTINCT ino (the `mk_file_at` law for
/// the two-node harness).
async fn shipped_file_at(client_be: &Arc<RoutedMetaBackend>, name: &str, fillers: u32) -> u64 {
    for i in 0..fillers {
        publish::create_with_rdev_size(
            client_be,
            1,
            &format!("filler-{name}-{i}"),
            libc::S_IFREG | 0o644,
            0,
            0,
            0,
            0,
        )
        .await
        .expect("filler create ships");
    }
    publish::create_with_rdev_size(client_be, 1, name, libc::S_IFREG | 0o644, 0, 0, 0, 0)
        .await
        .expect("create ships")
        .ino
}

/// Cross `ino` to a kvmap head on the OWNER through the shipped train
/// (the A4 verb — 3 records at indices 0..3), returning the entries.
async fn shipped_crossing(client_be: &Arc<RoutedMetaBackend>, ino: u64) -> Vec<(u32, String)> {
    let entries: Vec<(u32, String)> = (0..3u32)
        .map(|b| (b, (u64::from(b) * BLOCK).to_string()))
        .collect();
    publish::migrate_block_map(
        client_be,
        ino,
        &kvmap_head_bytes(3 * BLOCK),
        3 * BLOCK,
        entries.clone(),
        Vec::new(),
        512,
    )
    .await
    .expect("the shipped crossing lands");
    entries
}

// ===========================================================================
// 2. §11 row 2 — the S11 scoped Put over a kvmap head
// ===========================================================================

/// Contract (§11 row 2, RED pre-fix): a range holder's full Put served
/// over a kvmap-headed ino must REFUSE — a `kvmap:` head's base decode is
/// Err BY DESIGN (its map lives in tree 7), so pre-fix it fell through
/// `custody_scoped_layout`'s undecodable-base "legacy verbatim" arm: the
/// shipper's stale inline map overwrote the head and orphaned every
/// tree-7 record. The fixed shape: refused loud naming PR 5b, counted on
/// `map_refused`, the head and the tree records byte-identical, and
/// NOTHING committed (journal-entry equality).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scoped_put_served_over_a_kvmap_head_refuses_and_stages_nothing() {
    let _serial = serial();
    let _restore = Restore;
    let dir = TempDir::new().unwrap();
    let owner_be = sandbox(dir.path(), "sp-own").await;
    let client_be = sandbox(dir.path(), "sp-cli").await;
    let auth = start_authority(Arc::clone(&owner_be));
    let (client, pc) = arm_client(&auth, &client_be).await;

    let ino = shipped_file_at(&client_be, "scoped.bin", 12).await;
    shipped_crossing(&client_be, ino).await;

    // The §9.2 geometry source (the scoped compose refuses without it —
    // the production mount arm installs the router-backed one), then the
    // shipper takes LIVE range custody: the Put below is the SCOPED
    // shape, not finding 34's custody-less one.
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(3 * BLOCK, BLOCK));
    let lease = match client
        .acquire_range(ino, (0, BLOCK), (0, BLOCK), Duration::from_secs(3))
        .await
        .expect("the holder's ranged grant")
    {
        RangeAcquireOutcome::New { lease, .. } => lease,
        other => panic!("a fresh ino's first ranged ask must be NEW, got {other:?}"),
    };

    let head_before = owner_be
        .getxattr(ino, "layout")
        .await
        .expect("layout read")
        .expect("kvmap head");
    let refused_before = publish::stats().map_refused;
    let journal_before = journal_entries();

    // The holder's full Put: a stale inline map naming only idx 0 —
    // applied verbatim it regresses the head and orphans the records.
    let err = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: inline_head_bytes(3 * BLOCK, vec![(0u32, "0".to_string())]),
                size: 3 * BLOCK,
                refs: Vec::new(),
                lease_epoch: client.lease_epoch(),
                request_id: 0x52A,
            },
        )
        .await
        .expect_err(
            "a scoped Put served over a kvmap-headed ino must refuse (§11 row 2) — \
             the verbatim arm regresses the head to the shipper's stale inline map",
        );
    assert!(
        format!("{err}").contains("PR 5b"),
        "the refusal names PR 5b (the S11 ∘ kvmap compose): {err}"
    );
    assert_eq!(
        publish::stats().map_refused,
        refused_before + 1,
        "the refusal is counted (map_refused)"
    );
    assert_eq!(
        journal_entries(),
        journal_before,
        "NOTHING committed — the screen fires before any staging"
    );
    assert_eq!(
        owner_be
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("kvmap head"),
        head_before,
        "the head is byte-identical"
    );
    assert_eq!(
        owner_be
            .block_map_range(ino, 0, 16)
            .await
            .expect("tree scan")
            .len(),
        3,
        "every tree-7 record survives the refused Put"
    );

    drop(lease);
    client.drain_releases().await;
    auth.listener.shutdown();
    shutdown_set(&owner_be).await;
    shutdown_set(&client_be).await;
}

// ===========================================================================
// 3. §11 row 3 — the chained shipped merge onto a kvmap base
// ===========================================================================

/// Contract (§11 row 3, RED pre-fix): a chained shipped merge onto a
/// kvmap base must REFUSE — the `head_indirect` gate keys on the WORD
/// "indirect" in the base-decode error, and a kvmap head's refusal text
/// deliberately does not carry it, so the merge reached `use_delta` and
/// staged a versioned delta ONTO the delta-ineligible kvmap base: every
/// later fold of the key refuses forever (delayed read-side poison). The
/// fixed shape, on BOTH arms (the aggregated conveyor pass — the shipped
/// default — and the `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1` direct
/// twin): refused loud in the retried class ("layout delta base
/// unusable" — the shipper's reset provenance refetches the kvmap head
/// and re-ships the A4 train), naming PR 5b, NOTHING staged
/// (journal-entry equality), and the base still folds clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chained_shipped_merge_onto_a_kvmap_base_refuses_before_staging() {
    let _serial = serial();
    let _restore = Restore;
    let dir = TempDir::new().unwrap();
    let owner_be = sandbox(dir.path(), "cm-own").await;
    let client_be = sandbox(dir.path(), "cm-cli").await;
    let auth = start_authority(Arc::clone(&owner_be));
    let (_client, _pc) = arm_client(&auth, &client_be).await;

    let ino = shipped_file_at(&client_be, "chained.bin", 16).await;
    shipped_crossing(&client_be, ino).await;
    let head_before = owner_be
        .getxattr(ino, "layout")
        .await
        .expect("layout read")
        .expect("kvmap head");
    let journal_before = journal_entries();

    let delta = squeezefs::layout_wire::LayoutDelta {
        file_type: "striped".into(),
        size: 4 * BLOCK,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: None,
        entries: vec![(3u32, (3 * BLOCK).to_string())],
        base_version: 0,
        version: 0,
    };
    let full = bytes::Bytes::from(inline_head_bytes(
        4 * BLOCK,
        vec![(3u32, (3 * BLOCK).to_string())],
    ));

    // Arm 1: the aggregated conveyor pass (the shipped default).
    let out = publish::merge_layout_and_size(
        &client_be,
        ino,
        &delta,
        full.clone(),
        4 * BLOCK,
        Vec::new(),
    )
    .await;
    let err = out.expect_err(
        "a chained shipped merge onto a kvmap base must refuse (§11 row 3, the \
         aggregated arm) — staging a delta onto it poisons every later fold",
    );
    assert!(
        format!("{err}").contains("layout delta base unusable"),
        "retried-class marker (the shipper refetches and re-ships the train): {err}"
    );
    assert!(
        format!("{err}").contains("PR 5b"),
        "the refusal names PR 5b: {err}"
    );

    // Arm 2: the direct twin (`SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1`) —
    // override set around the ONE call, restored before any assertion
    // can panic past it.
    squeezefs::routing::set_publish_commit_group_override(Some(1));
    let direct = publish::merge_layout_and_size(
        &client_be,
        ino,
        &delta,
        full.clone(),
        4 * BLOCK,
        Vec::new(),
    )
    .await;
    squeezefs::routing::set_publish_commit_group_override(None);
    let err = direct.expect_err(
        "a chained shipped merge onto a kvmap base must refuse (§11 row 3, the \
         direct arm)",
    );
    assert!(
        format!("{err}").contains("layout delta base unusable")
            && format!("{err}").contains("PR 5b"),
        "same class, same PR-5b naming, on the direct arm: {err}"
    );

    assert_eq!(
        journal_entries(),
        journal_before,
        "NOTHING staged on either arm — journal-entry equality"
    );
    // The base still folds clean: a staged delta would make this read
    // refuse FOREVER (the delayed poison §11 row 3 names).
    assert_eq!(
        owner_be
            .getxattr(ino, "layout")
            .await
            .expect("the kvmap base still folds clean")
            .expect("kvmap head"),
        head_before,
        "the head is byte-identical"
    );
    assert_eq!(
        owner_be
            .block_map_range(ino, 0, 16)
            .await
            .expect("tree scan")
            .len(),
        3,
        "every tree-7 record survives the refused merges"
    );

    auth.listener.shutdown();
    shutdown_set(&owner_be).await;
    shutdown_set(&client_be).await;
}
