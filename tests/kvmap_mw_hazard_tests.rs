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
        squeezefs::meta_backend::kv::block_refs::uninstall_block_ref_resolver();
    }
}

/// The production arm's rung-19 refs resolver, mirrored onto the rig's
/// router (the mw_cowriter_free_tests fixture-truth discipline) — the
/// scoped compose's f36b recompute is structurally inert without it.
fn install_rig_refs_resolver(rig: &Rig) {
    let br = std::sync::Arc::clone(&rig.router.backend_router);
    squeezefs::meta_backend::kv::block_refs::install_block_ref_resolver(std::sync::Arc::new(
        move |k: &str, ino: u64, idx: u32| br.block_ref_for(k, ino, idx),
    ));
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
                match entry {
                    MapEntry::String(bytes) => {
                        out.push((idx, String::from_utf8(bytes).expect("utf8 key")))
                    }
                    MapEntry::Point { vol_tag, offset } => {
                        assert_eq!(vol_tag, default_tag, "the rig's one data volume");
                        out.push((idx, offset.to_string()));
                    }
                    // PR 6a: expand runs per-index (the shared surfaces'
                    // arithmetic — this rig's one volume, bare offsets).
                    MapEntry::Run {
                        vol_tag,
                        start_offset,
                        len,
                    } => {
                        assert_eq!(vol_tag, default_tag, "the rig's one data volume");
                        let stride = self.alloc.chunk_size();
                        for d in 0..len {
                            out.push((idx + d, (start_offset + u64::from(d) * stride).to_string()));
                        }
                    }
                    other => panic!("this rig never mints stamped records: {other:?}"),
                }
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
    // to a 1-entry inline map pre-fix). PR 5b: the compose is a committed
    // train on a custody-armed authority, so the head now also carries
    // the §11 map-generation belt — the sticky pin is the PARSED kvmap
    // head, and the minted generation is asserted beside it.
    let head_id = rig.durable_head(ino).await.block_map_id;
    let parsed =
        squeezefs::meta_backend::kv::block_map::parse_kvmap_head(head_id.as_deref().unwrap_or(""))
            .unwrap_or_else(|e| {
                panic!("the composed publish must keep the sticky kvmap head ({head_id:?}): {e}")
            });
    assert_eq!(parsed.sweep_cursor, None);
    assert_eq!(
        parsed.gen, 1,
        "the episode compose is a committed train on the mw plane — the §11 belt bumps"
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
// 4b. §11 row 4 → PR 5b item 4 — a sticky-head LOCAL train under live
//     grants composes CLAIMS-SCOPED (the f34 local screen, lifted)
// ===========================================================================

/// Contract (PR 5b item 4 — replaces PR 5a's local-train refusal): a
/// sticky-head LOCAL whole-map train under live range grants, OUTSIDE the
/// episode-compose window, runs CLAIMS-SCOPED — its own claimed
/// transition lands, a SUBSET map deletes NOTHING by absence (peers'
/// entries survive), and an empty-claims train is a records no-op. The
/// non-kvmap-base refusal STAYS (a new-crossing whole-map train under
/// live grants — the crossing stands down at the caller instead).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sticky_head_local_train_under_live_range_grants_composes_claims_scoped() {
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

    let auth = start_authority(Arc::clone(&rig.routed));
    let (holder, lease) = live_range_grant(&auth.endpoint, NODE, ino).await;
    assert!(auth.owner.ino_has_range_grants(ino));

    // The LOCAL train, outside any compose window, carrying a SUBSET map
    // plus ONE claimed new binding — pre-5b this refused loud; the
    // pre-law diff would have deleted every record the subset omits.
    let new_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(new_off);
    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    let outcome = publish::migrate_block_map(
        &rig.routed,
        ino,
        &kvmap_head_bytes(u64::from(SPILL_BLOCKS + 1) * BLOCK),
        u64::from(SPILL_BLOCKS + 1) * BLOCK,
        vec![entries[0].clone(), (SPILL_BLOCKS, new_off.to_string())],
        vec![squeezefs::meta_backend::kv::block_refs::BlockRefOp::taken(
            squeezefs::meta_backend::kv::block_refs::BlockRef {
                vol_tag,
                block_idx: new_off / BLOCK,
                owner_ino: ino,
                block_index: SPILL_BLOCKS,
            },
        )],
        512,
        // PR 6b: no live sweep cursor in these shapes — the barrier
        // never engages (a claims train refuses instead).
        0,
        &|_key, _idx| None,
        None,
    )
    .await
    .expect("a sticky-head local train under live grants composes claims-scoped (item 4)");
    assert_eq!(outcome.records, 1, "exactly the claimed transition staged");
    assert_eq!(outcome.gen, 1, "a committed mw-plane train bumps the belt");

    let mut expect = entries.clone();
    expect.push((SPILL_BLOCKS, new_off.to_string()));
    assert_eq!(
        rig.tree_records(ino).await,
        expect,
        "the SUBSET map deleted NOTHING by absence — peers' entries survive"
    );

    // An empty-claims subset train is a records NO-OP (nothing adopted,
    // nothing deleted) — the §11 law-b floor.
    let noop = publish::migrate_block_map(
        &rig.routed,
        ino,
        &kvmap_head_bytes(u64::from(SPILL_BLOCKS + 1) * BLOCK),
        u64::from(SPILL_BLOCKS + 1) * BLOCK,
        vec![entries[0].clone()],
        Vec::new(),
        512,
        // PR 6b: no live sweep cursor in these shapes — the barrier
        // never engages (a claims train refuses instead).
        0,
        &|_key, _idx| None,
        None,
    )
    .await
    .expect("an empty-claims subset train is legal");
    assert_eq!(
        noop.records, 0,
        "no claims ⇒ no ops — never delete-by-absence"
    );
    assert_eq!(
        rig.tree_records(ino).await,
        expect,
        "every record survives the no-op train"
    );
    assert!(rig.drift().await.is_empty(), "the accounting stayed exact");

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
        // PR 6b: no live sweep cursor in these shapes — the barrier
        // never engages (a claims train refuses instead).
        0,
        &|_key, _idx| None,
        None,
    )
    .await
    .expect("the shipped crossing lands");
    entries
}

// ===========================================================================
// 2. §11 row 2 → PR 5b item 3 — the S11 ∘ kvmap scoped compose
// ===========================================================================

/// Contract (PR 5b item 3 — replaces PR 5a's row-2 refusal with the
/// positive S11 ∘ kvmap compose): a range holder's full Put served over a
/// kvmap-headed ino composes over the TREE-RESOLVED map under the claims
/// law and persists via the claims-scoped train — the scoped Put lands
/// its range's CLAIMED binding, every UNCLAIMED binding survives (the
/// shipper's stale garbage entry never adopts), an out-of-custody claim
/// is span-filtered, the head stays sticky-kvmap, the f36b recompute
/// stands the caller's frame down (`PutDone{recomputed: true}` +
/// `map_recomputed_releases`), and the C8 oracle reads zero drift.
///
/// The REMAINING refusal (item 3's stated residue, pinned): a Put that
/// SHIPS a kvmap head over a non-kvmap durable base refuses retried-class
/// (sticky heads never regress, so the frame is stale/foreign), counted
/// on `map_refused`, journal-entry equality.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scoped_put_served_over_a_kvmap_head_composes_claims_scoped() {
    let _serial = serial();
    let _restore = Restore;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file_at("scoped.bin", 12).await;

    // The crossing first (no custody plane yet).
    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);

    let auth = start_authority(Arc::clone(&rig.routed));
    install_rig_refs_resolver(&rig);
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(
            u64::from(SPILL_BLOCKS) * BLOCK,
            BLOCK,
        ));
    // The holder's LIVE range grant covers block index 0 only.
    let (holder, lease) = live_range_grant(&auth.endpoint, NODE, ino).await;
    assert!(auth.owner.ino_has_range_grants(ino));
    let pc = publish::PublishClient::new(NODE, SECRET.to_vec());

    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    let wire_ref = |off: u64, idx: u32, take: bool| publish::WireBlockRefOp {
        vol_tag,
        block_idx: off / BLOCK,
        owner_ino: ino,
        block_index: idx,
        take,
    };
    let old0: u64 = entries[0].1.parse().expect("plain offset key");
    let old2: u64 = entries[2].1.parse().expect("plain offset key");
    let new0 = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(new0);
    let out2 = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(out2);

    // The holder's full Put: its RANGE's rewrite at idx 0 (claimed), a
    // stale GARBAGE entry at idx 1 (unclaimed — its view of a block it
    // never wrote), and an OUT-OF-CUSTODY claimed rewrite at idx 2 (the
    // span filter's row).
    let mut put_map = entries.clone();
    put_map[0] = (0, new0.to_string());
    put_map[1] = (1, "999999999".to_string());
    put_map[2] = (2, out2.to_string());
    let recomputed_before = publish::stats().map_recomputed_releases;
    let reply = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: inline_head_bytes(u64::from(SPILL_BLOCKS) * BLOCK, put_map),
                size: u64::from(SPILL_BLOCKS) * BLOCK,
                refs: vec![
                    wire_ref(old0, 0, false),
                    wire_ref(new0, 0, true),
                    wire_ref(old2, 2, false),
                    wire_ref(out2, 2, true),
                ],
                lease_epoch: holder.lease_epoch(),
                request_id: 0x53A,
            },
        )
        .await
        .expect("the S11 ∘ kvmap scoped Put composes (item 3's positive contract)");
    assert_eq!(
        reply,
        publish::PublishReply::PutDone { recomputed: true },
        "the compose recomputed the accounting — the caller's frame stream stands down"
    );

    // The claimed in-custody binding landed; everything unclaimed (and
    // the out-of-custody claim) survived verbatim.
    let mut expect = entries.clone();
    expect[0] = (0, new0.to_string());
    assert_eq!(
        rig.tree_records(ino).await,
        expect,
        "adopt under claim ∩ custody; never the shipper's stale view (§11 row 2's \
         clobber is exactly this assertion failing)"
    );
    let head_id = rig.durable_head(ino).await.block_map_id;
    let parsed =
        squeezefs::meta_backend::kv::block_map::parse_kvmap_head(head_id.as_deref().unwrap_or(""))
            .unwrap_or_else(|e| panic!("the head stays sticky-kvmap ({head_id:?}): {e}"));
    assert_eq!(parsed.gen, 1, "the compose is a committed mw-plane train");
    assert!(
        rig.drift().await.is_empty(),
        "zero C8 drift — the recomputed swap diff matches the tree diff"
    );
    assert_eq!(
        publish::stats().map_recomputed_releases - recomputed_before,
        1,
        "the displaced in-custody binding (old0) is the recompute's released set"
    );

    // The remaining refusal: SHIPPING a kvmap head over a non-kvmap
    // durable base (a stale/foreign frame — sticky heads never regress).
    let ino2 = rig.mk_file_at("nonkvmap.bin", 0).await;
    let refused_before = publish::stats().map_refused;
    let journal_before = journal_entries();
    let err = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino: ino2,
                layout: kvmap_head_bytes(BLOCK),
                size: BLOCK,
                refs: Vec::new(),
                lease_epoch: holder.lease_epoch(),
                request_id: 0x53B,
            },
        )
        .await
        .expect_err("a shipped kvmap head over a non-kvmap durable base refuses");
    assert!(
        format!("{err}").contains("layout delta base unusable"),
        "retried-class (the shipper refetches): {err}"
    );
    assert_eq!(publish::stats().map_refused, refused_before + 1);
    assert_eq!(
        journal_entries(),
        journal_before,
        "NOTHING committed — the refusal fires before any staging"
    );

    drop(lease);
    holder.drain_releases().await;
    auth.listener.shutdown();
    rig.shutdown().await;
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

// ===========================================================================
// 5. PR 5b item 1 — claims-scoped SHIPPED trains + the map-generation belt
// ===========================================================================

/// Contract (design §11 law b, RED pre-5b): two co-writers ALTERNATE
/// shipped sticky-head saves on one kvmap ino, and writer B's RAM map
/// lags writer A's committed bindings. The pre-law served train's diff
/// was delete-by-absence — B's whole-map ship mass-deleted A's fresh
/// binding (and minted NO ref release for it: the f36b leak class). The
/// law: the served train adopts a Put only under B's take claims and
/// deletes only under an explicit release-without-take — A's bindings
/// survive B's ship, both writers' own claims land, and the C8 oracle
/// reads zero drift. The BELT: a ship whose carried base generation lags
/// the durable head refuses retried-class ("layout delta base unusable")
/// before anything stages, counted on `map_refused`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn alternating_shipped_saves_keep_peer_bindings_and_the_belt_refuses_lag() {
    let _serial = serial();
    let _restore = Restore;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file_at("alternating.bin", 24).await;

    // The crossing (local, pre-arm): the sticky kvmap base both writers
    // then ship onto.
    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1"),
        "a solo crossing mints no generation (byte-identity)"
    );

    let auth = start_authority(Arc::clone(&rig.routed));
    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    let writer_a = WriteCustodyClient::connect(&auth.endpoint, SECRET, "node-kvmap-alt-a")
        .await
        .expect("writer A joins");
    let writer_b = WriteCustodyClient::connect(&auth.endpoint, SECRET, "node-kvmap-alt-b")
        .await
        .expect("writer B joins");
    let pc_a = publish::PublishClient::new("node-kvmap-alt-a", SECRET.to_vec());
    let pc_b = publish::PublishClient::new("node-kvmap-alt-b", SECRET.to_vec());

    let take_ref = |off: u64, idx: u32| publish::WireBlockRefOp {
        vol_tag,
        block_idx: off / BLOCK,
        owner_ino: ino,
        block_index: idx,
        take: true,
    };

    // Writer A's shipped save: the current map plus ITS OWN new binding
    // at index SPILL_BLOCKS, claimed by its refs frame.
    let a_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(a_off);
    let mut a_map = entries.clone();
    a_map.push((SPILL_BLOCKS, a_off.to_string()));
    let a_reply = pc_a
        .ship(
            &auth.endpoint,
            publish::PublishCall::MigrateBlockMap {
                ino,
                layout: kvmap_head_bytes(u64::from(SPILL_BLOCKS + 1) * BLOCK),
                size: u64::from(SPILL_BLOCKS + 1) * BLOCK,
                entries: a_map,
                refs: vec![take_ref(a_off, SPILL_BLOCKS)],
                base_gen: 0,
                lease_epoch: writer_a.lease_epoch(),
                request_id: 0xA1,
            },
        )
        .await
        .expect("writer A's sticky-head ship lands");
    let publish::PublishReply::MapMigrated { records, gen, .. } = a_reply else {
        panic!("the train answers its accounting: {a_reply:?}");
    };
    assert_eq!(
        records, 1,
        "A's claims-scoped diff stages exactly its claim"
    );
    assert_eq!(gen, 1, "the first mw-plane train mints the belt");

    // Writer B's shipped save: a CURRENT base generation but a STALE map
    // — it lags A's committed binding at SPILL_BLOCKS and carries its own
    // new one at SPILL_BLOCKS + 1.
    let b_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(b_off);
    let mut b_map = entries.clone();
    b_map.push((SPILL_BLOCKS + 1, b_off.to_string()));
    let served_before = publish::stats().map_served;
    let b_reply = pc_b
        .ship(
            &auth.endpoint,
            publish::PublishCall::MigrateBlockMap {
                ino,
                layout: kvmap_head_bytes(u64::from(SPILL_BLOCKS + 2) * BLOCK),
                size: u64::from(SPILL_BLOCKS + 2) * BLOCK,
                entries: b_map.clone(),
                refs: vec![take_ref(b_off, SPILL_BLOCKS + 1)],
                base_gen: 1,
                lease_epoch: writer_b.lease_epoch(),
                request_id: 0xB1,
            },
        )
        .await
        .expect("writer B's sticky-head ship lands");
    let publish::PublishReply::MapMigrated {
        records,
        gen,
        preexisting,
        ..
    } = b_reply
    else {
        panic!("the train answers its accounting: {b_reply:?}");
    };
    assert_eq!(
        records, 1,
        "B's claims-scoped diff stages exactly its claim"
    );
    assert_eq!(gen, 2, "every committed mw-plane train bumps the belt");
    assert_eq!(
        preexisting, 0,
        "PR 6c-i (§14 S2 pre-fix a): a claims train probes CLAIMED indices \
         only — B claimed the unmapped index SPILL_BLOCKS + 1, so its \
         bounded probes observed zero preexisting records (the whole-map \
         census stays the establishing train's)"
    );
    assert_eq!(publish::stats().map_served - served_before, 1);

    // §11 law (b)'s whole point: A's binding SURVIVED B's stale
    // whole-map ship, and both writers' claims landed.
    let mut expect = entries.clone();
    expect.push((SPILL_BLOCKS, a_off.to_string()));
    expect.push((SPILL_BLOCKS + 1, b_off.to_string()));
    assert_eq!(
        rig.tree_records(ino).await,
        expect,
        "delete-by-absence on a SHIPPED train is §11 law (b)'s hazard — exactly this \
         assertion failing"
    );
    assert!(
        rig.drift().await.is_empty(),
        "zero C8 drift — both claims landed and nothing was diff-deleted unaccounted"
    );

    // The BELT: B re-ships with a LAGGING base generation (1, durable 2)
    // — refused retried-class before anything stages.
    let refused_before = publish::stats().map_refused;
    let journal_before = journal_entries();
    let err = pc_b
        .ship(
            &auth.endpoint,
            publish::PublishCall::MigrateBlockMap {
                ino,
                layout: kvmap_head_bytes(u64::from(SPILL_BLOCKS + 2) * BLOCK),
                size: u64::from(SPILL_BLOCKS + 2) * BLOCK,
                entries: b_map,
                refs: Vec::new(),
                base_gen: 1,
                lease_epoch: writer_b.lease_epoch(),
                request_id: 0xB2,
            },
        )
        .await
        .expect_err("a lagging base generation refuses (§11's belt)");
    let text = format!("{err}");
    assert!(
        text.contains("layout delta base unusable") && text.contains("kvmap map generation"),
        "retried-class, named: {err}"
    );
    assert_eq!(
        publish::stats().map_refused,
        refused_before + 1,
        "the belt refusal is counted (map_refused)"
    );
    assert_eq!(
        journal_entries(),
        journal_before,
        "NOTHING committed — the belt fires under the train's 4a before any staging"
    );
    assert_eq!(
        rig.tree_records(ino).await,
        expect,
        "the refused ship changed nothing"
    );

    drop(writer_a);
    drop(writer_b);
    auth.listener.shutdown();
    rig.shutdown().await;
}

// ===========================================================================
// 6. PR 5b item 4 — the s11-shaped composition contract (the f34 lift's
//    gate): a range-granted kvmap ino under TWO scoped writers plus the
//    authority's own episode compose
// ===========================================================================

/// Contract (PR 5b item 4 — the lift's acceptance): on a RANGE-GRANTED
/// kvmap ino, writer A's scoped Put (item 3's path), writer B's
/// sticky-head shipped TRAIN under live grants (the lifted f34 serve —
/// claims custody-scoped to B's span), and the authority's own episode
/// compose ALL land: each writer's claimed binding sticks, every peer
/// binding survives every stale ship, the belt counts three committed
/// trains, and the C8 oracle reads zero drift.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_granted_kvmap_ino_composes_two_scoped_writers_and_the_episode_publish() {
    let _serial = serial();
    let _restore = Restore;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file_at("s11shape.bin", 32).await;

    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);

    let auth = start_authority(Arc::clone(&rig.routed));
    install_rig_refs_resolver(&rig);
    auth.owner
        .install_range_geometry(data_grant::fixed_range_geometry(
            u64::from(SPILL_BLOCKS) * BLOCK,
            BLOCK,
        ));
    // Writer A holds block 0; writer B holds block 1 — disjoint ranges.
    let (holder_a, lease_a) = live_range_grant(&auth.endpoint, "node-s11-a", ino).await;
    let holder_b = WriteCustodyClient::connect(&auth.endpoint, SECRET, "node-s11-b")
        .await
        .expect("writer B joins");
    let lease_b = match holder_b
        .acquire_range(
            ino,
            (BLOCK, 2 * BLOCK),
            (BLOCK, 2 * BLOCK),
            Duration::from_secs(3),
        )
        .await
        .expect("writer B's ranged grant")
    {
        RangeAcquireOutcome::New { lease, .. } => lease,
        other => panic!("B's first ranged ask must be NEW, got {other:?}"),
    };
    let pc_a = publish::PublishClient::new("node-s11-a", SECRET.to_vec());
    let pc_b = publish::PublishClient::new("node-s11-b", SECRET.to_vec());

    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    let wire_ref = |off: u64, idx: u32, take: bool| publish::WireBlockRefOp {
        vol_tag,
        block_idx: off / BLOCK,
        owner_ino: ino,
        block_index: idx,
        take,
    };
    let old0: u64 = entries[0].1.parse().expect("plain offset key");
    let old1: u64 = entries[1].1.parse().expect("plain offset key");
    let recomputed_before = publish::stats().map_recomputed_releases;
    let refused_before = publish::stats().map_refused;
    let unscoped_before = publish::stats().unscoped_put_refusals;

    // Writer A: the scoped Put (item 3's path) rewriting ITS block 0.
    let a_new = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(a_new);
    let mut a_map = entries.clone();
    a_map[0] = (0, a_new.to_string());
    let reply = pc_a
        .ship(
            &auth.endpoint,
            publish::PublishCall::SetLayoutAndSize {
                ino,
                layout: inline_head_bytes(u64::from(SPILL_BLOCKS) * BLOCK, a_map),
                size: u64::from(SPILL_BLOCKS) * BLOCK,
                refs: vec![wire_ref(old0, 0, false), wire_ref(a_new, 0, true)],
                lease_epoch: holder_a.lease_epoch(),
                request_id: 0x511,
            },
        )
        .await
        .expect("writer A's scoped Put lands");
    assert_eq!(reply, publish::PublishReply::PutDone { recomputed: true });

    // Writer B: the sticky-head shipped TRAIN under LIVE grants (the
    // lifted f34 serve) — a STALE whole map (it lags A's rewrite) plus
    // its own claimed rewrite of ITS block 1, base gen refetched current.
    let b_new = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(b_new);
    let mut b_map = entries.clone();
    b_map[1] = (1, b_new.to_string());
    let b_reply = pc_b
        .ship(
            &auth.endpoint,
            publish::PublishCall::MigrateBlockMap {
                ino,
                layout: kvmap_head_bytes(u64::from(SPILL_BLOCKS) * BLOCK),
                size: u64::from(SPILL_BLOCKS) * BLOCK,
                entries: b_map,
                refs: vec![wire_ref(old1, 1, false), wire_ref(b_new, 1, true)],
                base_gen: 1,
                lease_epoch: holder_b.lease_epoch(),
                request_id: 0x512,
            },
        )
        .await
        .expect("writer B's sticky-head train lands under live grants (the f34 lift)");
    let publish::PublishReply::MapMigrated {
        records,
        recomputed,
        released,
        gen,
        ..
    } = b_reply
    else {
        panic!("the train answers its accounting: {b_reply:?}");
    };
    assert_eq!(
        records, 1,
        "B's claims-scoped diff stages exactly its claim"
    );
    assert!(recomputed, "the f36b recompute rides the lifted serve");
    assert_eq!(released, 1, "old1 is the recompute's released set");
    assert_eq!(gen, 2);

    // The authority's OWN episode compose (the §11 row-1 arm, unchanged):
    // a fresh claimed binding at index SPILL_BLOCKS.
    let c_new = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(c_new);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(SPILL_BLOCKS, c_new.to_string())]),
            u64::from(SPILL_BLOCKS + 1) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("the authority's episode compose lands beside both writers");

    // Everything landed; nobody's stale view erased anybody's binding.
    let mut expect = entries.clone();
    expect[0] = (0, a_new.to_string());
    expect[1] = (1, b_new.to_string());
    expect.push((SPILL_BLOCKS, c_new.to_string()));
    assert_eq!(
        rig.tree_records(ino).await,
        expect,
        "two scoped writers + the episode compose all landed, claims-scoped"
    );
    let head_id = rig.durable_head(ino).await.block_map_id;
    let parsed =
        squeezefs::meta_backend::kv::block_map::parse_kvmap_head(head_id.as_deref().unwrap_or(""))
            .unwrap_or_else(|e| panic!("the head stays sticky-kvmap ({head_id:?}): {e}"));
    assert_eq!(
        parsed.gen, 3,
        "three committed trains — the belt counted every one"
    );
    assert!(
        rig.drift().await.is_empty(),
        "zero C8 drift across the whole composition"
    );
    assert_eq!(
        publish::stats().map_recomputed_releases - recomputed_before,
        2,
        "old0 (A's Put) and old1 (B's train) are the recomputes' released sets"
    );
    assert_eq!(
        publish::stats().map_refused,
        refused_before,
        "no refusal anywhere — the screens lifted for kvmap inos"
    );
    assert_eq!(
        publish::stats().unscoped_put_refusals,
        unscoped_before,
        "every ship carried custody — the f34 class never fired"
    );

    drop(lease_a);
    drop(lease_b);
    holder_a.drain_releases().await;
    holder_b.drain_releases().await;
    auth.listener.shutdown();
    rig.shutdown().await;
}
