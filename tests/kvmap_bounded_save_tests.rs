//! **kvmap bounded maps, both sides — PR 6c-i (`feat/kvmap-bounded-save`)**
//! of the PB-class file ladder (`docs/design-kvmap-block-map-tree.md` §14,
//! Rev 1.9).
//!
//! Contracts pinned here:
//!
//! 1. **The train pre-fixes (§14 S2's two breaks).** (a) A claims-scoped
//!    train's current-binding scan is CLAIMS-BOUNDED — point lookups /
//!    bounded floor probes per claimed index, never a whole-map
//!    materialization (asserted via the split lookup counters: a claims
//!    train over an N-record ino performs O(claims) tree reads and ZERO
//!    range pages). (b) The gen-bump criterion is SPLIT: a SOLO mount's
//!    local claims train mints no generation (the solo-dark
//!    byte-identity law); only SERVED and custody-armed trains bump.
//! 2. **The mode heuristic (§14 S1).** `CachedMetadata` for a kvmap ino
//!    whose SIZE-estimated map exceeds the 6a budget share
//!    (`kvmap_write_map_budget_bytes`) carries the PARTIAL store (dirty
//!    overlay + bounded warm windows + tombstones), never the whole map;
//!    under-cap inos keep whole-map RAM verbatim (byte-identical), and
//!    `SQUEEZEFS_KVMAP_OVERLAY=0` restores whole-map RAM at any size.
//! 3. **PartialMiss ≠ Hole.** On a partial-mode ino: an unwritten index
//!    reads zeros (Hole), a written index absent from the overlay reads
//!    THROUGH the tree (never fabricated zeros), and a TOMBSTONED index
//!    reads zeros and never resurrects the tree's stale binding.
//! 4. **Overlay saves.** A partial-mode publish ships ONLY the overlay
//!    through the LOCAL claims train (`kvmap_overlay_saves` accounts for
//!    it); displacement discovery rides the train's recompute (the f36b
//!    law) — the displaced tree binding's block is freed on the publish
//!    tail (the freed-supply probe), with no whole-map diff.
//! 5. **Truncate composition.** Partial-mode truncate never refuses:
//!    over-threshold takes the 6b size-flip handoff verbatim;
//!    under-threshold sweeps via bounded ranges (never whole-map).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::METRICS;
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, MapTrainClaims};
use squeezefs::meta_backend::kv::block_map::{parse_kvmap_head, MapEntry};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::kv::{
    META_KV_BLOCK_MAP_LOOKUP_EXACT, META_KV_BLOCK_MAP_LOOKUP_FLOOR, META_KV_BLOCK_MAP_LOOKUP_RANGE,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
const DATA_LEN: u64 = 32 * 1024 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-00000000000000d6";
const BLOCK: u64 = 4 * 1024 * 1024;
/// Past the 64 KiB-node volume's inline cap — the crossing trigger.
const SPILL_BLOCKS: u32 = 1200;

// ---------------------------------------------------------------------------
// Serialization (process-global METRICS deltas + mem-budget mutation)
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

// ---------------------------------------------------------------------------
// The Rig (the kvmap_run_tests fixture, subset)
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

async fn format_meta_kvmap(path: &Path) {
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
    assert!(set_block_refcounts_bit(path).await.expect("stamp bit 9"));
    assert!(set_block_map_tree_bit(path).await.expect("stamp bit 16"));
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

    fn token(&self, ino: u64) -> u64 {
        self.router.dlm.get_fencing_token_ino(ino)
    }

    /// Allocate `n` sequential blocks and bind them at indices `0..n` in
    /// ONE merge — the crossing trigger.
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

    async fn durable_head(&self, ino: u64) -> LayoutMetadata {
        let bytes = self
            .routed
            .getxattr(ino, "layout")
            .await
            .expect("layout read")
            .expect("layout present");
        bincode::deserialize(&bytes).expect("bincode head")
    }

    async fn shutdown(self) {
        self.routed.volumes[0]
            .shutdown()
            .await
            .expect("clean shutdown");
    }
}

/// The kv-level entry_key closure the direct claims-train calls use:
/// this rig's one volume, bare-offset keys, 4 MiB stride.
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

/// A bincode kvmap flip head at `size` (the direct-train shape).
fn kvmap_head_bytes(size: u64) -> Vec<u8> {
    bincode::serialize(&LayoutMetadata {
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

/// Establish `n` PER-INDEX POINT records for `ino` via the direct
/// whole-map (establishing) train — no run coalescing (the routed seam
/// owns that), so the tree carries exactly `n` records.
async fn establish_points(rig: &Rig, ino: u64, n: u32) -> u64 {
    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    let entries: Vec<(u32, MapEntry)> = (0..n)
        .map(|b| {
            (
                b,
                MapEntry::Point {
                    vol_tag,
                    offset: u64::from(b) * BLOCK,
                },
            )
        })
        .collect();
    rig.kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(n) * BLOCK),
            u64::from(n) * BLOCK,
            &[],
            &entries,
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
    vol_tag
}

// ===========================================================================
// 1a. Pre-fix (a): the claims train's current scan is CLAIMS-BOUNDED
//     (design §14 S2 break 1)
// ===========================================================================

/// A claims train over an N-record ino performs O(claims) tree reads —
/// point lookups (+ bounded floor probes on exact misses), and ZERO
/// whole-map range pages. On the pre-fix tip the claims arm paged the
/// ino's ENTIRE record population into RAM per train (`block_map_range`
/// pages ≈ N/chunk), which is exactly the O(map)-per-publish cost §14's
/// overlay saves exist to delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claims_train_probes_o_claims_not_o_map() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("bounded-probes").await;
    let n: u32 = 2048;
    let vol_tag = establish_points(&rig, ino, n).await;

    // Claims: one changed take at 5, one release at 100 — 2 claimed
    // indices against 2048 records.
    let take: std::collections::BTreeSet<u32> = [5u32].into_iter().collect();
    let release: std::collections::BTreeSet<u32> = [100u32].into_iter().collect();
    let claims = MapTrainClaims {
        base_gen: None,
        take,
        release,
        ..Default::default()
    };
    let exact0 = META_KV_BLOCK_MAP_LOOKUP_EXACT.load(Ordering::Relaxed);
    let floor0 = META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed);
    let range0 = META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed);
    rig.kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(n) * BLOCK),
            u64::from(n) * BLOCK,
            &[],
            &[(
                5u32,
                MapEntry::Point {
                    vol_tag,
                    offset: u64::from(n + 7) * BLOCK,
                },
            )],
            512,
            Some(&claims),
            ino,
            &entry_key_for_tests,
            0,
            &|_key, _idx| None,
        )
        .await
        .expect("claims train")
        .expect("engaged tree");
    let exact = META_KV_BLOCK_MAP_LOOKUP_EXACT.load(Ordering::Relaxed) - exact0;
    let floor = META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed) - floor0;
    let range = META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed) - range0;
    // The bounded-probe law: zero range pages (the whole-map scan is
    // gone), and at most exact+floor per claimed index. On the pre-fix
    // tip `range` is ≥ N/chunk = 4.
    assert_eq!(
        range, 0,
        "a claims train must never page the whole map (§14 S2 pre-fix a): \
         {range} range pages over {n} records for 2 claims"
    );
    assert!(
        exact + floor <= 4,
        "O(claims) probes for 2 claims: exact={exact} floor={floor}"
    );

    // Correctness beside the economy: the take adopted, the release
    // deleted, an unclaimed neighbour is untouched.
    let got5 = rig.kv().get_block_mapping(ino, 5).await.unwrap().unwrap();
    assert_eq!(
        got5,
        (
            5,
            MapEntry::Point {
                vol_tag,
                offset: u64::from(n + 7) * BLOCK
            }
        ),
        "the take claim adopted"
    );
    assert!(
        rig.kv().get_block_mapping(ino, 100).await.unwrap().is_none(),
        "the release claim deleted"
    );
    assert_eq!(
        rig.kv().get_block_mapping(ino, 6).await.unwrap().unwrap(),
        (
            6,
            MapEntry::Point {
                vol_tag,
                offset: 6 * BLOCK
            }
        ),
        "unclaimed indices untouched"
    );
    rig.shutdown().await;
}

// ===========================================================================
// 1b. Pre-fix (b): a SOLO local claims train mints no generation
//     (design §14 S2 break 2 — the solo-dark byte-identity law)
// ===========================================================================

/// The gen-bump criterion is SPLIT: presence of claims alone never mints
/// a generation — a solo mount's LOCAL claims trains (6c overlay saves)
/// keep the head gen-dark exactly like its whole-map trains (the
/// `a_solo_crossing_mints_no_generation` pin's law), while a train on a
/// custody-armed authority or with an already-minted gen still bumps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_solo_local_claims_train_mints_no_generation() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("solo-dark").await;
    let n: u32 = 128;
    let vol_tag = establish_points(&rig, ino, n).await;
    let head0 = rig.durable_head(ino).await;
    let parsed0 = parse_kvmap_head(head0.block_map_id.as_deref().unwrap()).expect("kvmap head");
    assert_eq!(parsed0.gen, 0, "the establishing train mints nothing");

    // A LOCAL claims train (base_gen None, un-served, no custody owner):
    // the §14 split — no generation mints.
    let claims = MapTrainClaims {
        base_gen: None,
        take: [3u32].into_iter().collect(),
        release: std::collections::BTreeSet::new(),
        ..Default::default()
    };
    let outcome = rig
        .kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(n) * BLOCK),
            u64::from(n) * BLOCK,
            &[],
            &[(
                3u32,
                MapEntry::Point {
                    vol_tag,
                    offset: u64::from(n + 1) * BLOCK,
                },
            )],
            512,
            Some(&claims),
            ino,
            &entry_key_for_tests,
            0,
            &|_key, _idx| None,
        )
        .await
        .expect("local claims train")
        .expect("engaged tree");
    assert_eq!(
        outcome.gen, 0,
        "a SOLO local claims train mints no generation (§14 S2 pre-fix b)"
    );
    let head = rig.durable_head(ino).await;
    let id = head.block_map_id.as_deref().unwrap();
    assert!(
        !id.contains(";gen:"),
        "the solo head stays gen-dark (byte-identity): {id}"
    );
    rig.shutdown().await;
}

// ===========================================================================
// Later items' contracts (mode heuristic, PartialMiss ≠ Hole, overlay
// saves, truncate composition) land with their own red/impl pairs below.
// ===========================================================================

/// Placeholder anchor so the suite names the ladder even before the later
/// rungs' contracts land — deliberately trivially green.
#[test]
fn suite_anchor() {
    let _ = METRICS.kvmap_write_map_bytes.load(Ordering::Relaxed);
}
