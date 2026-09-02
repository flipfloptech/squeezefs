//! **kvmap RUN records + the stamped compact forms — PR 6a
//! (`perf/kvmap-runs-point2`)** of the PB-class file ladder
//! (`docs/design-kvmap-block-map-tree.md` §12, Rev 1.6; §2/§6 A6).
//!
//! Contracts pinned here:
//!
//! 1. **Run emission + the byte collapse.** A straight sequential
//!    publish coalesces into `ceil(N / RUN_LEN_MAX)` RUN records at the
//!    routed train's post-encode seam; `publish_map_record_bytes` and
//!    the staged-record count collapse ~len×; the C8 oracle reads zero
//!    drift THROUGH the run expansion (both sides ride the shared
//!    extraction).
//! 2. **The read law.** An exact point at N supersedes a covering run
//!    (§2); an exact miss resolves through the bounded A6 floor probe
//!    (`RUN_LEN_MAX` window, owner-prefix-checked, coverage-checked);
//!    the ROUTED `block_map_range` prepends the covering run for a
//!    mid-span `from_index`.
//! 3. **The publish train is the canonicalizer** (§12 — NO separate
//!    coalescer): a one-block rewrite splits the canonical form to
//!    run‖point‖run on its own publish; a shadow point left by a
//!    non-canonical stager is deleted by the next full publish IN the
//!    covering run's own tx group (the A6 coalesce law).
//! 4. **Claims×runs (the §12 conservative law).** A take inside a run
//!    adopts as ONE superseding point (never a hot-path split); a
//!    release inside a run DISSOLVES it to per-index records (never a
//!    silent partial adopt); a desired-side run on a claims train
//!    refuses loud.
//! 5. **POINT2/RUN2.** With incarnation keys engaged (bit 13 — the
//!    default format, Rev 1.4 #1), stamped keys ride the 26-B POINT2 /
//!    30-B RUN2 forms and decode their stamps VERBATIM (never
//!    re-attached from the live map).
//! 6. **The write-map cap (§12 option b).** `kvmap_write_map_bytes`
//!    gauges write-active kvmap RAM maps and an over-budget merge
//!    refuses EFBIG loud.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::METRICS;
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, MapTrainClaims};
use squeezefs::meta_backend::kv::block_map::{MapEntry, RUN_LEN_MAX};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::kv::{META_KV_BLOCK_MAP_LOOKUP_FLOOR, META_KV_BLOCK_MAP_RUN_PUTS};
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
// The Rig (the kvmap_crossing_tests fixture)
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

/// Format with bit 16 (the tree) + bit 9 (the durable-ref ledger, so the
/// C8 oracle grades every run publish), stripped of the `SQUEEZEFS_TEST_
/// STAMP_*` seams first for determinism.
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
    /// ONE merge — the crossing trigger AND the straight-run shape.
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

    /// The RAW tree-7 records — kinds preserved (the record-count/shape
    /// census; `expanded_records` is the per-index view).
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

    /// Per-index expansion of the raw records with the §2 read-law
    /// overlay (exact record supersedes covering run) — the test's own
    /// independent arithmetic, deliberately not the shared surface.
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

    /// The C8 oracle: durable-vs-derived (both sides through the shared
    /// extraction — run expansion included).
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

/// A bincode kvmap flip head at `size` (the shipped-train shape).
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

// ===========================================================================
// 1. Run emission + the byte collapse (design §12; A6)
// ===========================================================================

/// A straight sequential spill emits ONE run record (`SPILL_BLOCKS <
/// RUN_LEN_MAX`), record bytes collapse ~len×, the per-index expansion
/// equals the published entries exactly, the C8 oracle reads zero drift,
/// and a crash-less remount refetch resolves the same map — the walkers
/// see identical per-index truth before and after runs engage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sequential_publish_emits_coalesced_run_records() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("straight").await;

    let bytes_before = METRICS.publish_map_record_bytes.load(Ordering::Relaxed);
    let run_puts_before = META_KV_BLOCK_MAP_RUN_PUTS.load(Ordering::Relaxed);

    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);

    // The record census: ceil(N / RUN_LEN_MAX) runs — one, here.
    let raw = rig.raw_records(ino).await;
    assert_eq!(
        raw.len() as u32,
        SPILL_BLOCKS.div_ceil(RUN_LEN_MAX),
        "a straight sequential map is ceil(N/RUN_LEN_MAX) run records: {raw:?}"
    );
    assert!(
        matches!(raw[0].1, MapEntry::Run { len, .. } if len == SPILL_BLOCKS),
        "the one record covers the whole span: {:?}",
        raw[0]
    );
    assert!(
        META_KV_BLOCK_MAP_RUN_PUTS.load(Ordering::Relaxed) > run_puts_before,
        "the run-emission gauge engaged"
    );
    // The byte collapse: one 22-B value (+12-B key) vs N points.
    let staged_bytes = METRICS.publish_map_record_bytes.load(Ordering::Relaxed) - bytes_before;
    assert!(
        staged_bytes < u64::from(SPILL_BLOCKS) * 20 / 100,
        "record bytes must collapse ~len× under runs, got {staged_bytes}"
    );

    // Expansion equality: the per-index view is the published truth.
    let expanded = rig.expanded_records(ino).await;
    assert_eq!(expanded.len(), entries.len());
    for (b, k) in &entries {
        assert_eq!(expanded.get(b), Some(k), "index {b} expands correctly");
    }
    // The C8 oracle rides the SHARED expansion — zero drift.
    assert!(rig.drift().await.is_empty(), "oracle clean over runs");

    // Refetch resolves via the tree, byte-for-byte (Rev 1.1 #4).
    rig.router.metadata_cache.invalidate(&ino);
    let fetched = rig
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("refetch");
    let map = fetched.block_map.as_deref().expect("tree-resolved map");
    assert_eq!(map.len(), entries.len());
    for (b, k) in &entries {
        assert_eq!(map.get(b), Some(k), "index {b} resolved via the run");
    }
    rig.shutdown().await;
}

/// A single-block rewrite re-canonicalizes on ITS OWN publish (the
/// publish train is the coalescer — §12): the canonical record shape
/// becomes run‖point‖run, reads resolve the point exactly, and the
/// oracle stays clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_block_rewrite_recanonicalizes_to_run_point_run() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("rewrite").await;
    let entries = rig.publish_spill(ino, SPILL_BLOCKS).await;

    // Rewrite index 700 to a FRESH (non-adjacent) offset.
    let k = 700u32;
    // Burn one offset so the replacement is not arithmetically adjacent.
    let burn = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(burn);
    let fresh = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(fresh);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(k, fresh.to_string())]),
            u64::from(SPILL_BLOCKS) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("rewrite publish");

    let raw = rig.raw_records(ino).await;
    assert_eq!(
        raw.len(),
        3,
        "canonical form after a mid-span rewrite is run‖point‖run: {raw:?}"
    );
    assert!(matches!(&raw[0], (0, MapEntry::Run { len, .. }) if *len == k));
    assert!(matches!(&raw[1], (idx, MapEntry::Point { offset, .. })
        if *idx == k && *offset == fresh));
    assert!(matches!(&raw[2], (idx, MapEntry::Run { len, .. })
            if *idx == k + 1 && *len == SPILL_BLOCKS - k - 1));

    // Reads: the point at k, the runs elsewhere (floor law).
    let hit = rig.kv().get_block_mapping(ino, k).await.unwrap().unwrap();
    assert_eq!(hit.0, k);
    assert!(matches!(hit.1, MapEntry::Point { offset, .. } if offset == fresh));
    let floor_before = META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed);
    let covered = rig
        .kv()
        .get_block_mapping(ino, k + 5)
        .await
        .unwrap()
        .expect("covered by the tail run");
    assert_eq!(covered.0, k + 1, "the floor probe answers the tail run");
    assert!(META_KV_BLOCK_MAP_LOOKUP_FLOOR.load(Ordering::Relaxed) > floor_before);

    // The expansion still equals the rewritten truth.
    let expanded = rig.expanded_records(ino).await;
    for (b, key) in &entries {
        let want = if *b == k {
            fresh.to_string()
        } else {
            key.clone()
        };
        assert_eq!(expanded.get(b), Some(&want), "index {b}");
    }
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ===========================================================================
// 2. The read law (§2/§6 A6/§12)
// ===========================================================================

/// The exact-point-supersedes-covering-run law + the bounded floor probe
/// at the kv level: a shadow point staged OVER a live run resolves
/// exactly; covered misses resolve through the floor; beyond-span and
/// foreign-ino misses stay absent; and the next FULL publish deletes the
/// shadow in the covering run's own tx group (the A6 coalesce law).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exact_point_supersedes_a_covering_run_until_the_publish_recanonicalizes() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("shadowed").await;
    let entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    assert_eq!(rig.raw_records(ino).await.len(), 1, "one covering run");

    // Stage a SHADOW point over index 40 through the kv staging surface
    // (the non-canonical stager class — a claims adoption's shape).
    let k = 40u32;
    let shadow_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(shadow_off);
    let head = rig
        .kv()
        .getxattr(ino, "layout")
        .await
        .unwrap()
        .expect("head");
    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);
    // The staged shadow carries its own reference swap so the C8 ledger
    // stays truth-consistent with the record it stages.
    let old_off: u64 = entries[k as usize].1.parse().unwrap();
    let bref = |off: u64| squeezefs::meta_backend::kv::block_refs::BlockRef {
        vol_tag,
        block_idx: off / BLOCK,
        owner_ino: ino,
        block_index: k,
    };
    rig.kv()
        .set_layout_and_size_with_map(
            ino,
            &head,
            u64::from(SPILL_BLOCKS) * BLOCK,
            &[
                squeezefs::meta_backend::kv::block_refs::BlockRefOp::released(bref(old_off)),
                squeezefs::meta_backend::kv::block_refs::BlockRefOp::taken(bref(shadow_off)),
            ],
            &[squeezefs::meta_backend::kv::block_map::BlockMapOp::Put {
                owner_ino: ino,
                block_index: k,
                entry: MapEntry::Point {
                    vol_tag,
                    offset: shadow_off,
                },
            }],
        )
        .await
        .expect("stage the shadow point");

    // Exact supersedes.
    let hit = rig.kv().get_block_mapping(ino, k).await.unwrap().unwrap();
    assert_eq!(
        hit,
        (
            k,
            MapEntry::Point {
                vol_tag,
                offset: shadow_off
            }
        )
    );
    // Neighbors still resolve through the covering run's floor.
    let covered = rig
        .kv()
        .get_block_mapping(ino, k + 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(covered.0, 0, "the run at 0 covers k+1");
    assert!(matches!(covered.1, MapEntry::Run { .. }));
    // Beyond the span: absent.
    assert_eq!(
        rig.kv()
            .get_block_mapping(ino, SPILL_BLOCKS + 7)
            .await
            .unwrap(),
        None
    );
    // Foreign-ino isolation (the A6 owner-prefix check): a neighbor ino
    // sees nothing of this ino's run.
    let other = rig.mk_file("neighbor").await;
    assert_eq!(rig.kv().get_block_mapping(other, 5).await.unwrap(), None);

    // The ROUTED range prepends the covering run for a mid-span start —
    // record-true (keyed at its real start), overlay points following.
    let page = rig.routed.block_map_range(ino, k + 3, 16).await.unwrap();
    assert!(
        matches!(page.first(), Some((0, MapEntry::Run { .. }))),
        "mid-run from_index prepends the covering run: {page:?}"
    );

    // The next FULL publish canonicalizes: the fetch-based RMW adopts
    // the durable truth (shadow included), so the map splits into
    // run‖point‖run — no covered shadow survives as a shadow.
    rig.router.metadata_cache.invalidate(&ino);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(0u32, entries[0].1.clone())]),
            u64::from(SPILL_BLOCKS) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("re-publish");
    let raw = rig.raw_records(ino).await;
    assert_eq!(
        raw.len(),
        3,
        "the publish train made the shadow record-true canonical (run‖point‖run): {raw:?}"
    );

    // …and rebinding the shadowed index BACK to the run's arithmetic
    // coalesces the whole span again: the desired run's Put rides with
    // the covered point's Delete (the A6 one-tx law) and the stale tail
    // run deletes after coverage — ONE record.
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(k, entries[k as usize].1.clone())]),
            u64::from(SPILL_BLOCKS) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("rebind publish");
    let raw = rig.raw_records(ino).await;
    assert_eq!(
        raw.len(),
        1,
        "a re-straightened span coalesces back to one run: {raw:?}"
    );
    assert!(matches!(raw[0].1, MapEntry::Run { len, .. } if len == SPILL_BLOCKS));
    let expanded = rig.expanded_records(ino).await;
    for (b, key) in &entries {
        assert_eq!(
            expanded.get(b),
            Some(key),
            "index {b} back to the run truth"
        );
    }
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ===========================================================================
// 3. Claims × runs (§12's conservative law)
// ===========================================================================

/// A take-claim INSIDE a live run adopts as one superseding point (the
/// §2 law — never a hot-path split); a release-claim inside a run
/// DISSOLVES it to per-index records with the released index absent; a
/// desired-side RUN on a claims train refuses loud. Every arm leaves
/// per-index truth intact for the unclaimed indices.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_never_partial_adopt_a_run_silently() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("claimed").await;
    // Past the inline cap (a 64-entry map never crosses).
    let n = SPILL_BLOCKS;
    let entries = rig.publish_spill(ino, n).await;
    assert_eq!(rig.raw_records(ino).await.len(), 1);
    let vol_tag = squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL_ID);

    // (a) A take at index 9 with a CHANGED binding: one superseding point.
    let adopt_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(adopt_off);
    let take: std::collections::BTreeSet<u32> = [9u32].into_iter().collect();
    let claims = MapTrainClaims {
        base_gen: Some(0),
        take,
        release: std::collections::BTreeSet::new(),
    };
    rig.kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(n) * BLOCK),
            u64::from(n) * BLOCK,
            &[],
            &[(
                9u32,
                MapEntry::Point {
                    vol_tag,
                    offset: adopt_off,
                },
            )],
            512,
            Some(&claims),
            ino,
            &entry_key_for_tests,
            // PR 6b: claims trains never barrier (no live cursor here).
            0,
            &|_key, _idx| None,
        )
        .await
        .expect("claims adopt")
        .expect("engaged tree");
    let raw = rig.raw_records(ino).await;
    assert_eq!(
        raw.len(),
        2,
        "adopt-inside-run = run + superseding point, never a split: {raw:?}"
    );
    let hit = rig.kv().get_block_mapping(ino, 9).await.unwrap().unwrap();
    assert_eq!(
        hit,
        (
            9,
            MapEntry::Point {
                vol_tag,
                offset: adopt_off
            }
        )
    );

    // (b) A release at index 20 DISSOLVES the run: per-index records for
    // every still-bound index, index 20 absent, adopted index 9 intact.
    let release: std::collections::BTreeSet<u32> = [20u32].into_iter().collect();
    let claims = MapTrainClaims {
        base_gen: Some(1),
        take: std::collections::BTreeSet::new(),
        release,
    };
    rig.kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(n) * BLOCK),
            u64::from(n) * BLOCK,
            &[],
            &[],
            512,
            Some(&claims),
            ino,
            &entry_key_for_tests,
            // PR 6b: claims trains never barrier (no live cursor here).
            0,
            &|_key, _idx| None,
        )
        .await
        .expect("claims release")
        .expect("engaged tree");
    let expanded = rig.expanded_records(ino).await;
    assert_eq!(expanded.get(&20), None, "the released index is unbound");
    assert_eq!(
        expanded.get(&9),
        Some(&adopt_off.to_string()),
        "the adopted binding survives the dissolve"
    );
    for (b, key) in entries.iter().filter(|(b, _)| *b != 20 && *b != 9) {
        assert_eq!(expanded.get(b), Some(key), "index {b} survives per-index");
    }
    assert!(
        rig.raw_records(ino)
            .await
            .iter()
            .all(|(_, e)| e.run_len() == 1),
        "the dissolve left per-index records only (the next full local \
         publish re-coalesces)"
    );

    // (c) A desired-side RUN on a claims train refuses loud.
    let refused = rig
        .kv()
        .migrate_block_map_train(
            ino,
            &kvmap_head_bytes(u64::from(n) * BLOCK),
            u64::from(n) * BLOCK,
            &[],
            &[(
                0u32,
                MapEntry::Run {
                    vol_tag,
                    start_offset: 0,
                    len: 8,
                },
            )],
            512,
            Some(&MapTrainClaims {
                base_gen: None,
                take: [0u32].into_iter().collect(),
                release: std::collections::BTreeSet::new(),
            }),
            ino,
            &entry_key_for_tests,
            // PR 6b: claims trains never barrier (no live cursor here).
            0,
            &|_key, _idx| None,
        )
        .await;
    assert!(
        matches!(refused, Err(SqueezefsError::InvalidOperation(ref m)) if m.contains("per-index")),
        "a claims train carrying a run must refuse, got {refused:?}"
    );

    // The full LOCAL publish re-coalesces the dissolved span (the
    // publish train is the canonicalizer).
    let mut current: Vec<(u32, String)> = rig.expanded_records(ino).await.into_iter().collect();
    current.sort_unstable_by_key(|&(b, _)| b);
    rig.router.metadata_cache.invalidate(&ino);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&current),
            u64::from(n) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("re-canonicalizing publish");
    let raw = rig.raw_records(ino).await;
    assert!(
        raw.iter().any(|(_, e)| e.run_len() > 1),
        "the local publish re-coalesced the dissolved span: {raw:?}"
    );
    rig.shutdown().await;
}

// ===========================================================================
// 4. POINT2 / RUN2 (§12; Rev 1.4 #1 — stamps engaged, never re-attached)
// ===========================================================================

/// With incarnation keys engaged (the default format's bit-13 posture),
/// a sequential publish of stamped keys rides RUN2 — the stamps decode
/// VERBATIM through expansion and survive a remount — and a stamped
/// non-adjacent key rides the 26-B POINT2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stamped_keys_ride_point2_and_run2_with_verbatim_stamps() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("stamped").await;

    // Engage item-6 incarnation keys on this rig's router (era 3 — any
    // legal durable term; SOLO appender lane).
    rig.router
        .backend_router
        .engage_incarnation_keys(
            3,
            squeezefs::meta_backend::kv::journal::AppendPartition::SOLO,
        )
        .expect("engage incarnations");

    // Mint sequential blocks WITH stamps (persist_block_key attaches the
    // live lifetime — consecutive mints, consecutive lane_seqs).
    let mut entries: Vec<(u32, String)> = Vec::new();
    for b in 0..SPILL_BLOCKS {
        let offset = rig.alloc.allocate_block().await.expect("allocate");
        rig.alloc.publish_block(offset);
        let key = rig
            .router
            .backend_router
            .persist_block_key("backend_0", offset);
        assert!(
            key.contains('@'),
            "an engaged mint must stamp its key: {key}"
        );
        entries.push((b, key));
    }
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&entries),
            u64::from(SPILL_BLOCKS) * BLOCK,
            LayoutFlip::ToStripedKeepStagedIdentity,
            rig.token(ino),
        )
        .await
        .expect("stamped crossing");

    let raw = rig.raw_records(ino).await;
    assert_eq!(
        raw.len() as u32,
        SPILL_BLOCKS.div_ceil(RUN_LEN_MAX),
        "stamped sequential mints coalesce into RUN2: {raw:?}"
    );
    assert!(
        matches!(raw[0].1, MapEntry::RunStamped { len, .. } if len == SPILL_BLOCKS),
        "the stamped run form: {:?}",
        raw[0]
    );

    // Expansion reproduces every stamped key VERBATIM.
    let expanded = rig.expanded_records(ino).await;
    for (b, k) in &entries {
        assert_eq!(expanded.get(b), Some(k), "stamped key {b} verbatim");
    }

    // A stamped NON-adjacent single key rides POINT2 (26 B, stamp
    // carried — never re-attached).
    let burn = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(burn);
    let lone = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(lone);
    let lone_key = rig
        .router
        .backend_router
        .persist_block_key("backend_0", lone);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(SPILL_BLOCKS + 10, lone_key.clone())]),
            u64::from(SPILL_BLOCKS + 11) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("stamped point publish");
    let raw = rig.raw_records(ino).await;
    let point2 = raw
        .iter()
        .find(|(i, _)| *i == SPILL_BLOCKS + 10)
        .expect("the lone record");
    assert!(
        matches!(point2.1, MapEntry::PointStamped { .. }),
        "a stamped lone key rides POINT2: {point2:?}"
    );

    // Remount (no incarnation engagement — a fresh mount with no live
    // stamps): the RECORDED stamps still decode verbatim, proving the
    // decode never consults the live map.
    rig.shutdown().await;
    let rig = mount(meta.path(), data.path()).await;
    let expanded = rig.expanded_records(ino).await;
    for (b, k) in &entries {
        assert_eq!(
            expanded.get(b),
            Some(k),
            "stamp {b} survives remount verbatim (never re-attached)"
        );
    }
    assert_eq!(expanded.get(&(SPILL_BLOCKS + 10)), Some(&lone_key));
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ===========================================================================
// 5. The write-map cap (§12 option b)
// ===========================================================================

/// The honest write-map cap: `kvmap_write_map_bytes` gauges the
/// write-active kvmap RAM maps, and a merge whose map estimate exceeds
/// the derived `mem_budget/16` share refuses EFBIG loud (never ENOSPC —
/// storage exists; the FILE's class is what this mount cannot hold).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_budget_kvmap_write_map_refuses_efbig() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("giant").await;
    let _entries = rig.publish_spill(ino, SPILL_BLOCKS).await;

    // The gauge sees the write-active map (the merge above upserted it).
    let gauge = rig.router.kvmap_write_map_gauge();
    assert!(
        gauge >= u64::from(SPILL_BLOCKS) * 64,
        "the write-map gauge accounts the dirty kvmap map, got {gauge}"
    );

    // Shrink the budget so the standing map is over the cap, then try to
    // grow: the merge must refuse EFBIG with zero side effects on the
    // records. Restore the budget on every exit.
    struct BudgetGuard;
    impl Drop for BudgetGuard {
        fn drop(&mut self) {
            squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(0);
            squeezefs::mem_budget::MEM_BUDGET.tick();
        }
    }
    let _budget = BudgetGuard;
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(64 * 1024);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    assert!(squeezefs::routing::kvmap_write_map_budget_bytes() <= 4 * 1024);

    let records_before = rig.raw_records(ino).await;
    let grow = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(grow);
    let refused = rig
        .router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(SPILL_BLOCKS, grow.to_string())]),
            u64::from(SPILL_BLOCKS + 1) * BLOCK,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await;
    match refused {
        Err(SqueezefsError::Io(e)) => {
            assert_eq!(
                e.raw_os_error(),
                Some(libc::EFBIG),
                "the cap's errno is EFBIG (the file-class refusal), got {e:?}"
            );
        }
        other => panic!("an over-budget kvmap merge must refuse EFBIG, got {other:?}"),
    }
    assert_eq!(
        rig.raw_records(ino).await,
        records_before,
        "a refused merge stages nothing"
    );
    rig.shutdown().await;
}
