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
use squeezefs::meta_backend::kv::block_refs::BlockRefOp;
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
        rig.kv()
            .get_block_mapping(ino, 100)
            .await
            .unwrap()
            .is_none(),
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
// 1a'. Finding 46: the WINDOW train — exact-only probes, frame verbatim
// ===========================================================================

/// The whole-map ino's steady-state publish arm (`MapTrainClaims::window`):
/// an EXTEND window over an N-record ino pays exactly one EXACT lookup
/// per window index — no floor probe (a miss with RAM whole-map authority
/// behind it is a fresh index or an index inside a run, and both stage
/// the same superseding point Put under the §2 read law), no whole-map
/// page — and commits the caller's refs frame VERBATIM (no recompute:
/// the caller's RAM merge captured every displacement). Every other
/// record is untouched (delete-by-absence OFF), and a changed take at an
/// existing exact record supersedes in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_train_probes_exact_only_and_commits_the_frame_verbatim() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("window-probes").await;
    let n: u32 = 2048;
    let vol_tag = establish_points(&rig, ino, n).await;

    // The window: 64 fresh indices appended past the map + one changed
    // take at an existing exact record (a same-batch overwrite).
    let window: Vec<(u32, MapEntry)> = (n..n + 64)
        .chain(std::iter::once(7u32))
        .map(|b| {
            (
                b,
                MapEntry::Point {
                    vol_tag,
                    offset: u64::from(b) * BLOCK + 3 * u64::from(n) * BLOCK,
                },
            )
        })
        .collect();
    let mut window = window;
    window.sort_unstable_by_key(|(b, _)| *b);
    let take: std::collections::BTreeSet<u32> = window.iter().map(|(b, _)| *b).collect();
    let claims = MapTrainClaims {
        base_gen: None,
        take,
        release: std::collections::BTreeSet::new(),
        served: false,
        overlay: false,
        window: true,
    };
    // The caller's frame: one take per window index + the displaced
    // release at 7 — exactly what the RAM merge computed. With a
    // recompute the train would REPLACE it; the window law says it
    // commits verbatim.
    let ref_at = |idx: u32, key: &str| squeezefs::meta_backend::kv::block_refs::BlockRef {
        vol_tag,
        block_idx: key.parse::<u64>().unwrap() / BLOCK,
        owner_ino: ino,
        block_index: idx,
    };
    let mut frame: Vec<BlockRefOp> = window
        .iter()
        .map(|(b, e)| BlockRefOp::taken(ref_at(*b, &entry_key_for_tests(e, 0).unwrap())))
        .collect();
    frame.push(BlockRefOp::released(ref_at(7, &(7 * BLOCK).to_string())));
    let staged0 = squeezefs::meta_backend::kv::META_KV_BLOCK_REFS_STAGED.load(Ordering::Relaxed);
    let released0 =
        squeezefs::meta_backend::kv::META_KV_BLOCK_REFS_RELEASED.load(Ordering::Relaxed);
    let exact0 = META_KV_BLOCK_MAP_LOOKUP_EXACT.load(Ordering::Relaxed);
    let floor0 = META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed);
    let range0 = META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed);
    // A resolver that would answer every key: the window law must NOT
    // consult it (a recompute here would double-count the displaced
    // free the caller's stream already owns).
    let resolver_calls = std::sync::atomic::AtomicU64::new(0);
    let outcome = rig
        .kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(n + 64) * BLOCK),
            u64::from(n + 64) * BLOCK,
            &frame,
            &window,
            512,
            Some(&claims),
            ino,
            &entry_key_for_tests,
            0,
            &|key, idx| {
                resolver_calls.fetch_add(1, Ordering::Relaxed);
                Some(ref_at(idx, key))
            },
        )
        .await
        .expect("window train")
        .expect("engaged tree");
    let exact = META_KV_BLOCK_MAP_LOOKUP_EXACT.load(Ordering::Relaxed) - exact0;
    let floor = META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed) - floor0;
    let range = META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed) - range0;
    assert_eq!(
        (exact, floor, range),
        (65, 0, 0),
        "the window train probes exact-only: one lookup per window index, no floor \
         scan, no whole-map page (got exact={exact} floor={floor} range={range})"
    );
    assert!(
        !outcome.recomputed && outcome.released.is_empty() && outcome.released_keys.is_empty(),
        "a window train never recomputes — the caller's frame commits verbatim: {outcome:?}"
    );
    assert_eq!(
        resolver_calls.load(Ordering::Relaxed),
        0,
        "the window train never consults the caller's resolver"
    );
    assert_eq!(
        (
            squeezefs::meta_backend::kv::META_KV_BLOCK_REFS_STAGED.load(Ordering::Relaxed)
                - staged0,
            squeezefs::meta_backend::kv::META_KV_BLOCK_REFS_RELEASED.load(Ordering::Relaxed)
                - released0,
        ),
        (65, 1),
        "the frame's ops staged verbatim (65 takes + 1 release)"
    );
    assert_eq!(outcome.records, 65, "one Put per window index");

    // Correctness: the appended indices bind, the changed take
    // superseded in place, and every other record stands (no
    // delete-by-absence).
    for b in [n, n + 63, 7u32] {
        assert_eq!(
            rig.kv().get_block_mapping(ino, b).await.unwrap().unwrap(),
            (
                b,
                MapEntry::Point {
                    vol_tag,
                    offset: u64::from(b) * BLOCK + 3 * u64::from(n) * BLOCK
                }
            ),
            "window index {b} binds to its new record"
        );
    }
    for b in [0u32, 6, 8, n - 1] {
        assert_eq!(
            rig.kv().get_block_mapping(ino, b).await.unwrap().unwrap(),
            (
                b,
                MapEntry::Point {
                    vol_tag,
                    offset: u64::from(b) * BLOCK
                }
            ),
            "un-windowed index {b} untouched"
        );
    }
    let census = rig
        .kv()
        .block_map_range(ino, 0, 1 << 20)
        .await
        .expect("census");
    assert_eq!(
        census.len(),
        (n + 64) as usize,
        "one record per index — the window neither duplicated nor deleted"
    );
    rig.shutdown().await;
}

/// The window law's run faces: a window index INSIDE an existing run
/// stages a superseding point (the §2 read law — one Put, no run split,
/// no floor scan), and a changed take AT a run's own key dissolves the
/// run (its other indices survive as records) — both with the exact
/// probe alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_train_supersedes_inside_a_run_and_dissolves_at_its_key() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("window-runs").await;
    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    // Establish: a 16-run at 0 and a 16-run at 16 (the direct train
    // accepts pre-coalesced runs verbatim).
    let runs: Vec<(u32, MapEntry)> = vec![
        (
            0,
            MapEntry::Run {
                vol_tag,
                start_offset: 0,
                len: 16,
            },
        ),
        (
            16,
            MapEntry::Run {
                vol_tag,
                start_offset: 16 * BLOCK,
                len: 16,
            },
        ),
    ];
    rig.kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(32 * BLOCK),
            32 * BLOCK,
            &[],
            &runs,
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

    // Window: index 5 (inside run 0 — a superseding point) and index 16
    // (run 1's OWN key — a dissolve).
    let fresh = |b: u32| MapEntry::Point {
        vol_tag,
        offset: 1000 * BLOCK + u64::from(b) * BLOCK,
    };
    let window: Vec<(u32, MapEntry)> = vec![(5, fresh(5)), (16, fresh(16))];
    let claims = MapTrainClaims {
        base_gen: None,
        take: [5u32, 16].into_iter().collect(),
        release: std::collections::BTreeSet::new(),
        served: false,
        overlay: false,
        window: true,
    };
    let floor0 = META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed);
    let outcome = rig
        .kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(32 * BLOCK),
            32 * BLOCK,
            &[],
            &window,
            512,
            Some(&claims),
            ino,
            &entry_key_for_tests,
            0,
            &|_key, _idx| None,
        )
        .await
        .expect("window train")
        .expect("engaged tree");
    assert_eq!(
        META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed) - floor0,
        0,
        "no floor probe on the window arm"
    );
    assert!(!outcome.recomputed);
    // Index 5: the exact record now supersedes the covering run; run 0
    // still covers its other indices.
    assert_eq!(
        rig.kv().get_block_mapping(ino, 5).await.unwrap().unwrap(),
        (5, fresh(5)),
        "a window index inside a run superseded as a point"
    );
    assert_eq!(
        rig.kv().get_block_mapping(ino, 4).await.unwrap().unwrap(),
        (
            0,
            MapEntry::Run {
                vol_tag,
                start_offset: 0,
                len: 16
            }
        ),
        "run 0 still covers its other indices"
    );
    // Index 16: the run dissolved — 16 binds to the new point, 17..32
    // survive as records.
    assert_eq!(
        rig.kv().get_block_mapping(ino, 16).await.unwrap().unwrap(),
        (16, fresh(16)),
        "a changed take at a run's key rebinds it"
    );
    for b in [17u32, 24, 31] {
        let (ridx, entry) = rig.kv().get_block_mapping(ino, b).await.unwrap().unwrap();
        assert_eq!(ridx, b, "run 1's index {b} survives as its own record");
        assert_eq!(
            entry_key_for_tests(&entry, 0).as_deref(),
            Some((u64::from(b) * BLOCK).to_string().as_str()),
            "run 1's index {b} keeps its binding through the dissolve"
        );
    }
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
// Shared partial-mode helpers
// ===========================================================================

/// Budget clamp: 64 KiB flag budget ⇒ `kvmap_write_map_budget_bytes()` ≤
/// 4 KiB ⇒ any map estimate past 64 entries flips PARTIAL. Restores on
/// drop (the kvmap_run_tests BudgetGuard pattern).
struct BudgetGuard;
impl Drop for BudgetGuard {
    fn drop(&mut self) {
        squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(0);
        squeezefs::mem_budget::MEM_BUDGET.tick();
    }
}

fn shrink_budget() -> BudgetGuard {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(64 * 1024);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    assert!(squeezefs::routing::kvmap_write_map_budget_bytes() <= 4 * 1024);
    BudgetGuard
}

impl Rig {
    /// The RAW tree-7 records (kinds preserved).
    async fn raw_records(&self, ino: u64) -> Vec<(u32, MapEntry)> {
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
            out.extend(page);
            cursor = match last.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }
        out
    }

    /// Per-index expansion with the §2 read-law overlay — the test's own
    /// independent arithmetic (the kvmap_run_tests helper).
    async fn expanded_records(&self, ino: u64) -> std::collections::BTreeMap<u32, String> {
        let stride = self.alloc.chunk_size();
        let mut out = std::collections::BTreeMap::new();
        for (idx, entry) in self.raw_records(ino).await {
            match entry {
                MapEntry::String(bytes) => {
                    out.insert(idx, String::from_utf8(bytes).expect("utf8 key"));
                }
                MapEntry::Point { offset, .. } => {
                    out.insert(idx, offset.to_string());
                }
                MapEntry::PointStamped {
                    offset,
                    incarnation,
                    ..
                } => {
                    out.insert(
                        idx,
                        squeezefs::routing::block_key_with_incarnation(
                            &offset.to_string(),
                            incarnation,
                        ),
                    );
                }
                MapEntry::Run {
                    start_offset, len, ..
                } => {
                    for d in 0..len {
                        out.insert(idx + d, (start_offset + u64::from(d) * stride).to_string());
                    }
                }
                MapEntry::RunStamped {
                    start_offset,
                    len,
                    start_incarnation,
                    ..
                } => {
                    for d in 0..len {
                        out.insert(
                            idx + d,
                            squeezefs::routing::block_key_with_incarnation(
                                &(start_offset + u64::from(d) * stride).to_string(),
                                start_incarnation + u64::from(d),
                            ),
                        );
                    }
                }
            }
        }
        out
    }

    /// The C8 oracle: durable-vs-derived, both sides through the shared
    /// extraction.
    async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.router
            .backend_router
            .verify_durable_block_refs(&self.routed)
            .await
            .expect("verification pass")
    }

    /// Evict + refetch, answering the fresh entry (the read path's view).
    async fn refetched(&self, ino: u64) -> squeezefs::routing::CachedMetadata {
        self.router.metadata_cache.invalidate(&ino);
        self.router
            .fetch_metadata(&squeezefs::keys::inode_path(ino))
            .await
            .expect("refetch")
    }

    /// The partial ladder's answers over `[0, upto)` via the authoritative
    /// span resolve.
    async fn resolved_span(
        &self,
        ino: u64,
        meta: &squeezefs::routing::CachedMetadata,
        upto: u32,
    ) -> std::collections::BTreeMap<u32, String> {
        let path = squeezefs::keys::inode_path(ino);
        let keys = self
            .router
            .load_striped_block_keys(&path, meta, 0, upto - 1)
            .await
            .expect("span resolve");
        keys.into_iter()
            .filter_map(|(b, k)| k.map(|k| (b, k)))
            .collect()
    }
}

// ===========================================================================
// 2. The mode heuristic — SIZE-based, both directions, bounded fetch
//    (design §14 S1)
// ===========================================================================

/// Over the budget share the ino flips PARTIAL at its post-publish
/// republish and the refetch builds the BOUNDED store (no whole-map
/// materialization — the S1 read-open debt); resolution reads THROUGH the
/// tree byte-for-byte; restoring the budget flips a CLEAN ino back DOWN
/// to whole-map RAM verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mode_flip_is_size_based_both_directions_with_bounded_fetch() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("flip").await;
    let mut expect: std::collections::BTreeMap<u32, String> = rig
        .publish_spill(ino, SPILL_BLOCKS)
        .await
        .into_iter()
        .collect();
    assert!(
        !rig.router.kvmap_partial_mode(ino),
        "under the real budget the ino keeps whole-map RAM (byte-identity)"
    );

    let _budget = shrink_budget();
    // A growth merge publishes; the CLEAN republish is the §14 flip point.
    let grow = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(grow);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(SPILL_BLOCKS, grow.to_string())]),
            u64::from(SPILL_BLOCKS + 1) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("over-budget growth merges (EFBIG deleted)");
    expect.insert(SPILL_BLOCKS, grow.to_string());
    assert!(
        rig.router.kvmap_partial_mode(ino),
        "the post-publish republish flips an over-budget ino PARTIAL"
    );

    // The bounded fetch: no whole-map materialization…
    let fetched = rig.refetched(ino).await;
    assert!(
        fetched.block_map.is_none(),
        "a partial fetch materializes NO map (§14 S1)"
    );
    assert!(rig.router.kvmap_partial_mode(ino));
    // …and the ladder reads THROUGH the tree, window-retained.
    let fills0 = METRICS.block_map_window_fills.load(Ordering::Relaxed);
    let got = rig.resolved_span(ino, &fetched, SPILL_BLOCKS + 1).await;
    assert_eq!(
        got,
        rig.expanded_records(ino).await,
        "partial resolution ≡ the tree's own truth"
    );
    assert_eq!(got.len() as u32, SPILL_BLOCKS + 1);
    assert!(
        METRICS.block_map_window_fills.load(Ordering::Relaxed) > fills0,
        "the read-through retained warm windows"
    );
    assert!(
        METRICS.block_map_window_bytes.load(Ordering::Relaxed) > 0,
        "the R5 gauge carries the window bytes"
    );

    // Both directions: a CLEAN under-budget refetch flips DOWN and
    // rehydrates whole-map verbatim.
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(0);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    let fetched = rig.refetched(ino).await;
    assert!(
        !rig.router.kvmap_partial_mode(ino),
        "a clean under-budget fetch flips back to whole-map"
    );
    let whole = fetched.block_map.as_deref().expect("whole map rehydrated");
    assert_eq!(whole.len() as u32, SPILL_BLOCKS + 1);
    for (b, k) in &expect {
        assert_eq!(whole.get(b), Some(k), "whole-map rehydration at {b}");
    }
    rig.shutdown().await;
}

// ===========================================================================
// 3. PartialMiss ≠ Hole (design §14's silent-wrong bomb rows) +
//    tombstones never resurrect
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_miss_reads_through_holes_read_zeros_tombstones_never_resurrect() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("sparse").await;

    let _budget = shrink_budget();
    // A SPARSE over-budget map: [0,600) and [800,1400) mapped, a real
    // hole between (1,200 entries — past the 64 KiB-node inline cap, the
    // crossing trigger).
    let mut entries: Vec<(u32, String)> = Vec::new();
    for b in (0..600).chain(800..1400) {
        let off = rig.alloc.allocate_block().await.unwrap();
        rig.alloc.publish_block(off);
        entries.push((b, off.to_string()));
    }
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&entries),
            1400 * BLOCK,
            LayoutFlip::ToStripedKeepStagedIdentity,
            rig.token(ino),
        )
        .await
        .expect("sparse crossing");
    assert!(rig.router.kvmap_partial_mode(ino), "over-budget ⇒ partial");
    let fetched = rig.refetched(ino).await;
    assert!(fetched.block_map.is_none());

    let path = squeezefs::keys::inode_path(ino);
    let span = rig
        .router
        .load_striped_block_keys(&path, &fetched, 0, 1399)
        .await
        .expect("span");
    let by_idx: std::collections::BTreeMap<u32, Option<String>> = span.into_iter().collect();
    let want: std::collections::BTreeMap<u32, String> = entries.iter().cloned().collect();
    for b in 0..1400u32 {
        match want.get(&b) {
            Some(k) => assert_eq!(
                by_idx.get(&b).cloned().flatten().as_ref(),
                Some(k),
                "a WRITTEN index the overlay lacks reads THROUGH the tree ({b})"
            ),
            None => assert_eq!(
                by_idx.get(&b).cloned().flatten(),
                None,
                "an unwritten index is a definitive HOLE ({b})"
            ),
        }
    }

    // The tombstone law: a punch on a partial ino kills the binding —
    // reads answer zeros and NOTHING (window refill, refetch) resurrects
    // the displaced tree binding.
    let punched_off: u64 = want.get(&1000).unwrap().parse().unwrap();
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::RemoveBlocks(&[1000]),
            1400 * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("partial punch never refuses");
    let fetched = rig.refetched(ino).await;
    let after = rig.resolved_span(ino, &fetched, 1400).await;
    assert!(
        !after.contains_key(&1000),
        "a tombstoned index reads zeros after publish + refetch (never resurrects)"
    );
    assert_eq!(
        after.get(&999),
        want.get(&999),
        "the punch displaced ONLY its own index"
    );
    assert!(
        matches!(rig.alloc.refcount(punched_off), None | Some(0)),
        "the punched binding's block left the reference ledger"
    );
    assert_eq!(rig.drift().await, Vec::new(), "C8 oracle clean");
    rig.shutdown().await;
}

// ===========================================================================
// 4. Overlay saves: only the overlay ships; the train recompute owns
//    displacement (design §14 S2, option ii)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_overlay_save_ships_the_overlay_and_the_recompute_frees_the_displaced_block() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("overlay-save").await;

    let _budget = shrink_budget();
    let entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    assert!(rig.router.kvmap_partial_mode(ino));
    let old_key = entries[5].1.clone();
    let old_off: u64 = old_key.parse().unwrap();
    assert_eq!(
        rig.alloc.refcount(old_off),
        Some(1),
        "the spill's own reference"
    );

    let saves0 = METRICS.kvmap_overlay_saves.load(Ordering::Relaxed);
    let range0 = META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed);
    let new_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(new_off);
    let displaced = rig
        .router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(5, new_off.to_string())]),
            u64::from(SPILL_BLOCKS) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("partial overwrite merges");
    // §14 S2 option ii: the merge stopped capturing tree prev-bindings —
    // displacement discovery moved WHOLLY to the train's recompute, and
    // the freed supply rides the publish tail.
    assert_eq!(
        displaced,
        Vec::<String>::new(),
        "no caller-frame displaced key for a tree binding"
    );
    assert!(
        METRICS.kvmap_overlay_saves.load(Ordering::Relaxed) > saves0,
        "the publish rode the overlay-save claims train (engagement)"
    );
    let expanded = rig.expanded_records(ino).await;
    assert_eq!(
        expanded.get(&5),
        Some(&new_off.to_string()),
        "the overlay binding adopted durably"
    );
    assert!(
        matches!(rig.alloc.refcount(old_off), None | Some(0)),
        "the displaced tree binding's block was freed on the publish tail"
    );
    // The bounded-probe law held on the steady-state publish path: the
    // overlay save probed its claims, never the 300-record population
    // (≤ 1 range page — the routed floor-prepend probe).
    assert!(
        META_KV_BLOCK_MAP_LOOKUP_RANGE.load(Ordering::Relaxed) - range0 <= 2,
        "an overlay save stays claims-bounded"
    );
    assert_eq!(rig.drift().await, Vec::new(), "C8 oracle clean");
    rig.shutdown().await;
}

// ===========================================================================
// 5. Truncate composition (design §14 item 4)
// ===========================================================================

/// A partial-mode truncate NEVER refuses: the under-threshold shrink
/// takes the 6b size-flip handoff and drains its sweep synchronously via
/// bounded ranges (records removed, references released, cursor cleared);
/// a subsequent extend write publishes cleanly past the old cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_truncate_never_refuses_and_sweeps_via_bounded_ranges() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("partial-trunc").await;

    let _budget = shrink_budget();
    let entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    assert!(rig.router.kvmap_partial_mode(ino));
    let removed_off: u64 = entries[200].1.parse().unwrap();

    rig.router
        .truncate_layout(ino, 100 * BLOCK, rig.token(ino))
        .await
        .expect("partial-mode truncate never refuses (§14 item 4)");
    // The sync bounded-range drain ran to terminal: records above the cut
    // gone, cursor cleared, references released.
    let head = rig.durable_head(ino).await;
    assert_eq!(head.size, 100 * BLOCK, "size-flip-first");
    let parsed = parse_kvmap_head(head.block_map_id.as_deref().unwrap()).unwrap();
    assert_eq!(
        parsed.sweep_cursor, None,
        "the under-threshold sweep drained synchronously to terminal"
    );
    let max_idx = rig.raw_records(ino).await.into_iter().map(|(i, _)| i).max();
    assert!(
        max_idx.is_some_and(|m| m < 100),
        "no record survives at/above the cut: {max_idx:?}"
    );
    assert!(
        matches!(rig.alloc.refcount(removed_off), None | Some(0)),
        "a removed binding's block was released"
    );

    // Write-during/after-sweep composition: an extend publish past the
    // old cut lands cleanly (the cursor/extend-barrier law).
    let grow = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(grow);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(150, grow.to_string())]),
            151 * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("post-truncate extend publishes");
    let fetched = rig.refetched(ino).await;
    let got = rig.resolved_span(ino, &fetched, 151).await;
    assert_eq!(got.get(&150), Some(&grow.to_string()));
    assert_eq!(
        got.get(&50),
        Some(&entries[50].1),
        "below-cut bindings intact"
    );
    assert!(
        !got.contains_key(&120),
        "the truncated hole between cut and extend stays a hole"
    );
    assert_eq!(rig.drift().await, Vec::new(), "C8 oracle clean");
    rig.shutdown().await;
}

// ===========================================================================
// 6. The A/B lever + byte-identity (design §14: `SQUEEZEFS_KVMAP_OVERLAY=0`
//    = whole-map RAM at any size)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn knob_off_keeps_whole_map_ram_at_any_size() {
    let _serial = serial();
    std::env::set_var("SQUEEZEFS_KVMAP_OVERLAY", "0");
    struct KnobGuard;
    impl Drop for KnobGuard {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_KVMAP_OVERLAY");
        }
    }
    let _knob = KnobGuard;
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("knob-off").await;

    let _budget = shrink_budget();
    let entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    assert!(
        !rig.router.kvmap_partial_mode(ino),
        "knob off: never partial, at any size (the acceptance bracket control)"
    );
    let fetched = rig.refetched(ino).await;
    let whole = fetched
        .block_map
        .as_deref()
        .expect("whole-map RAM verbatim");
    assert_eq!(whole.len() as u32, SPILL_BLOCKS);
    for (b, k) in &entries {
        assert_eq!(whole.get(b), Some(k));
    }
    rig.shutdown().await;
}

// ===========================================================================
// 7. The f44 mirror: over-cap REMOUNT content law at the binding level
//    (design §14 S1 read-open boundedness across sessions)
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_cap_remount_resolves_identical_bindings_without_materializing() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let _budget = shrink_budget();

    // Session 1: cross + overwrite (the f44 two-session shape's write
    // half), snapshot the tree truth, unmount clean.
    let (ino, want) = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("remount").await;
        let _ = rig.publish_spill(ino, SPILL_BLOCKS).await;
        assert!(rig.router.kvmap_partial_mode(ino));
        let new_off = rig.alloc.allocate_block().await.unwrap();
        rig.alloc.publish_block(new_off);
        rig.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[(7, new_off.to_string())]),
                u64::from(SPILL_BLOCKS) * BLOCK,
                LayoutFlip::KeepLayout,
                rig.token(ino),
            )
            .await
            .expect("session-1 rewrite");
        let want = rig.expanded_records(ino).await;
        assert_eq!(want.len() as u32, SPILL_BLOCKS);
        rig.shutdown().await;
        (ino, want)
    };

    // Session 2: the remount's fetch stays BOUNDED and the ladder answers
    // the same bindings (the content law at the binding level).
    let rig = mount(meta.path(), data.path()).await;
    let fetched = rig.refetched(ino).await;
    assert!(
        fetched.block_map.is_none(),
        "the over-cap remount fetch materializes NO map"
    );
    assert!(rig.router.kvmap_partial_mode(ino));
    let got = rig.resolved_span(ino, &fetched, SPILL_BLOCKS).await;
    assert_eq!(got, want, "remount binding truth ≡ session 1's tree");
    assert_eq!(rig.drift().await, Vec::new(), "C8 oracle clean");
    rig.shutdown().await;
}

/// The gauges named by §14 item 5 exist on the stats surface (compile-time
/// pin: the fields are read here exactly as the stats JSON reads them).
#[test]
fn suite_anchor() {
    let _ = METRICS.kvmap_write_map_bytes.load(Ordering::Relaxed);
    let _ = METRICS.kvmap_overlay_saves.load(Ordering::Relaxed);
    let _ = METRICS.block_map_window_bytes.load(Ordering::Relaxed);
    let _ = METRICS.block_map_window_fills.load(Ordering::Relaxed);
    let _ = METRICS.block_map_window_hits.load(Ordering::Relaxed);
    let _ = METRICS.block_map_window_evictions.load(Ordering::Relaxed);
}
