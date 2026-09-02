//! **The kvmap crossing — PR 2 (`feat/kvmap-crossing`)** of the PB-class
//! file ladder (`docs/design-kvmap-block-map-tree.md`, Rev 1.1 — §§3, 6
//! A1–A5/A10, and all five §7 addenda): the spill switch, the held-4a
//! migration train, the legacy-blob conversion, the pulled-forward walker
//! safety, the read-minimal/unlink set, and the shipped `MigrateBlockMap`
//! verb.
//!
//! Contracts pinned here:
//!
//! 1. **The crossing end-to-end.** A beyond-inline publish on a bit-16
//!    volume flips the head to `kvmap:1`, the tree carries EVERY mapping,
//!    the counters account for the train, and the C8 oracle reads zero
//!    drift (Rev 1.1 #3).
//! 2. **The crash matrix.** A crashed train's residue (records staged,
//!    head not flipped — the mid-sweep / between-chunks / chunk↔flip
//!    windows all leave exactly this on-disk shape) is INVISIBLE after
//!    remount, and the re-crossing's A1 diff reconciles it — including
//!    the truncate-shrink-recross-extend stale-mapping resurrection.
//! 3. **Byte identity.** An un-stamped volume's crossing takes the legacy
//!    blob arm verbatim (sector 0 never stamped); `SQUEEZEFS_KVMAP=0`
//!    routes NEW crossings to the blob arm on a stamped volume too — and
//!    never disables kvmap-HEAD resolution (A10 / Rev 1.1 #5 sticky).
//! 4. **Legacy conversion.** An `indirect:` ino converts on its first
//!    publish under a stamped volume: records complete, the blob's
//!    MAP_BLOB reference released exactly once, oracle clean.
//! 5. **Pulled-forward safety (Rev 1.1 #2).** The DERIVED mount-recovery
//!    walk owns a kvmap ino's blocks — gap-completion never free-lists
//!    live data on a bit-9-absent volume.
//! 6. **Unlink.** `delete_file` sweeps the records (tree empty, blocks
//!    released) — never silent residue.
//! 7. **The shipped verb.** A foreign-home crossing ships ONE witnessed
//!    `MigrateBlockMap`; the owner's ratchet self-arms (stamp + mint),
//!    a lost-reply replay answers the winner's own outcome, and a dead
//!    era refuses before the window (the FreeBlocks template).
//!
//! All tests currently FAIL (the crossing does not exist yet).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::data_custody;
use squeezefs::data_grant::{self, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::dlm::DlmClient;
use squeezefs::error::SqueezefsError;
use squeezefs::fuse_client::METRICS;
use squeezefs::layout_wire::LayoutMetadata;
use squeezefs::membership::{LeaseClock, LeaseClocks};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_map::{parse_kvmap_head, MapEntry};
use squeezefs::meta_backend::kv::block_refs::BLOCK_INDEX_MAP_BLOB;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_map_tree_bit, set_block_refcounts_bit, write_superblock_v3,
    VolumeFormat, FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::kv::{META_KV_BLOCK_MAP_PUTS, META_KV_JOURNAL_ENTRIES};
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
/// Sparse on purpose (the durable_block_refs fixture's shape): the
/// crossing legs allocate > 1000 offsets without writing most of them —
/// and the two-ino legs allocate two spills' worth.
const DATA_LEN: u64 = 32 * 1024 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-00000000000000c2";
/// Enough mapped blocks to push the encoded map past the 64 KiB-node
/// volume's ~16 KiB xattr cap — the crossing trigger.
const SPILL_BLOCKS: u32 = 1200;

// ---------------------------------------------------------------------------
// Serialization (process-global METRICS deltas + env knob mutation)
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
// The Rig (the durable_block_refs_tests fixture: real v3 meta volume, real
// file-backed data volume, the router that binds them)
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

/// Format WITHOUT bit 16 (PR 2's crossing owns the stamp) and WITHOUT
/// bit 9 unless a leg stamps it — immune to the `SQUEEZEFS_TEST_STAMP_*`
/// seams (this suite tests both sides of the boundary).
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
/// ledger, so the C8 oracle grades the crossing's accounting).
async fn format_meta_kvmap(path: &Path) {
    format_meta(path).await;
    assert!(set_block_refcounts_bit(path).await.expect("stamp bit 9"));
    assert!(
        set_block_map_tree_bit(path).await.expect("stamp bit 16"),
        "a fresh format must NOT already carry bit 16 — the crossing owns the stamp"
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

    fn token(&self, ino: u64) -> u64 {
        self.router.dlm.get_fencing_token_ino(ino)
    }

    /// Allocate `n` blocks and bind them at indices `0..n` in ONE merge —
    /// the durable fixture's spill trigger, kvmap's crossing trigger.
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
                u64::from(n) * 4 * 1024 * 1024,
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

    /// Every tree-7 record of `ino`, decoded to key strings. PR 3 emits
    /// POINT for undecorated keys (this rig's are all default-slot bare
    /// offsets), STRING for decorated shapes — resolve both.
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

    /// The C8 oracle (Rev 1.1 #3): durable-vs-derived, exact or drifting.
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

// ===========================================================================
// 1. The crossing end-to-end (design §3; Rev 1.1 #3/#4)
// ===========================================================================

/// Grow past the inline cap on a bit-16 volume: the head flips to
/// `kvmap:1` (no blob, no inline map), the tree carries EVERY mapping,
/// the ledger counters account for the train, refetch resolves via the
/// TREE (a `block_map: None` head would zero-read — the Rev 1.1 #4
/// class), and the C8 oracle reads zero drift.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crossing_flips_the_head_and_the_tree_carries_every_mapping() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert!(rig.kv().block_map_tree_engaged());
    let ino = rig.mk_file("crossed").await;

    let inos_before = METRICS.map_migrate_inos.load(Ordering::Relaxed);
    let recs_before = METRICS.map_migrate_records.load(Ordering::Relaxed);
    let bytes_before = METRICS.publish_map_record_bytes.load(Ordering::Relaxed);
    let puts_before = META_KV_BLOCK_MAP_PUTS.load(Ordering::Relaxed);

    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);

    // The head: the ~100 B kvmap sentinel — never a blob, never inline.
    let head = rig.durable_head(ino).await;
    assert_eq!(head.block_map_id.as_deref(), Some("kvmap:1"));
    parse_kvmap_head(head.block_map_id.as_deref().unwrap()).expect("canonical head");
    assert!(
        head.block_map.is_none(),
        "a kvmap head carries no inline map"
    );
    assert_eq!(head.size, u64::from(SPILL_BLOCKS) * 4 * 1024 * 1024);

    // The records: complete, in index order, key strings verbatim.
    assert_eq!(rig.tree_records(ino).await, entries);
    assert!(!rig.kv().crossing_in_flight(ino), "the registry drained");

    // The ledger: the train is accounted.
    assert_eq!(
        METRICS.map_migrate_inos.load(Ordering::Relaxed) - inos_before,
        1,
        "one crossing"
    );
    assert!(
        METRICS.map_migrate_records.load(Ordering::Relaxed) - recs_before
            >= u64::from(SPILL_BLOCKS)
    );
    assert!(METRICS.publish_map_record_bytes.load(Ordering::Relaxed) > bytes_before);
    assert!(
        META_KV_BLOCK_MAP_PUTS.load(Ordering::Relaxed) - puts_before >= u64::from(SPILL_BLOCKS)
    );
    // The blob counters must NOT have engaged — this publish never
    // wrote an indirect blob.
    assert!(
        rig.drift().await.is_empty(),
        "the accounting rode the train's own transactions — zero C8 drift"
    );

    // Rev 1.1 #4 (the read-minimal set): evict the RAM entry, then
    // REFETCH — the map must resolve via the tree, byte-for-byte.
    rig.router.metadata_cache.invalidate(&ino);
    let fetched = rig
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("refetch");
    assert_eq!(fetched.block_map_id.as_deref(), Some("kvmap:1"));
    let map = fetched.block_map.as_deref().expect("tree-resolved map");
    assert_eq!(map.len(), entries.len());
    for (b, k) in &entries {
        assert_eq!(map.get(b), Some(k), "index {b} resolved via the tree");
    }

    // …and a post-refetch publish COMPOSES onto the full map (a
    // zero-read refetch would have made this save's diff delete
    // everything but the new entry).
    let extra_off = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(extra_off);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(SPILL_BLOCKS, extra_off.to_string())]),
            u64::from(SPILL_BLOCKS + 1) * 4 * 1024 * 1024,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("post-crossing publish");
    assert_eq!(
        rig.tree_records(ino).await.len(),
        entries.len() + 1,
        "the steady-state save diffs — it never regresses the tree to its own batch"
    );
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

/// The crossing survives a crash + remount through the journal alone
/// (no clean shutdown), and the remounted read path serves the tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crossed_head_survives_a_crash_remount() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let (ino, entries) = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("crash_survivor").await;
        let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
        entries.sort_unstable_by_key(|&(b, _)| b);
        // CRASH: drop without shutdown — nothing checkpointed on purpose.
        drop(rig);
        (ino, entries)
    };
    let rig = mount(meta.path(), data.path()).await;
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1")
    );
    assert_eq!(rig.tree_records(ino).await, entries);
    rig.shutdown().await;
}

// ===========================================================================
// 2. The crash-window matrix (A1): residue invisible, re-cross idempotent
// ===========================================================================

/// Every mid-train crash window — mid-sweep, between Put chunks, and
/// chunk↔flip — leaves the SAME on-disk class: tree-7 records staged,
/// head NOT flipped. Planted here through the one-tx staging seam (the
/// exact state a killed train leaves), the contract is: (a) the residue
/// is INVISIBLE to reads after remount (the head is the truth); (b) the
/// next crossing's A1 diff reconciles it — including a stale record at
/// an index BEYOND the final map (the truncate-shrink-recross-extend
/// resurrection, the silent-wrong-data class A1 exists to kill).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_residue_is_invisible_and_the_recross_sweeps_it() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let (ino, inline_key) = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("residue").await;
        // A small INLINE map first (below the cap — no crossing).
        let off = rig.alloc.allocate_block().await.unwrap();
        rig.alloc.publish_block(off);
        rig.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[(0, off.to_string())]),
                4 * 1024 * 1024,
                LayoutFlip::ToStripedKeepStagedIdentity,
                rig.token(ino),
            )
            .await
            .expect("inline publish");
        // Plant the crashed-train residue: map records staged (the
        // committed chunk txs) while the head still names the INLINE
        // map — the flip never ran. `stale-resurrect` sits at an index
        // far beyond the final map: pre-A1 it would resurrect.
        let inline_head = rig
            .kv()
            .getxattr(ino, "layout")
            .await
            .unwrap()
            .expect("inline head");
        rig.kv()
            .set_layout_and_size_with_map(
                ino,
                &inline_head,
                4 * 1024 * 1024,
                &[],
                &[
                    squeezefs::meta_backend::kv::block_map::BlockMapOp::Put {
                        owner_ino: ino,
                        block_index: 7,
                        entry: MapEntry::String(b"999999999".to_vec()),
                    },
                    squeezefs::meta_backend::kv::block_map::BlockMapOp::Put {
                        owner_ino: ino,
                        block_index: 4_000_000,
                        entry: MapEntry::String(b"888888888".to_vec()),
                    },
                ],
            )
            .await
            .expect("plant residue (the crashed train's committed chunks)");
        // CRASH.
        drop(rig);
        (ino, off.to_string())
    };

    let rig = mount(meta.path(), data.path()).await;
    // (a) Residue exists on disk but is INVISIBLE: the head still names
    // the inline map, and the read path serves exactly it.
    assert_eq!(rig.tree_records(ino).await.len(), 2, "residue on disk");
    let head = rig.durable_head(ino).await;
    assert!(
        head.block_map_id.as_deref() != Some("kvmap:1"),
        "the flip never committed — the head is not kvmap"
    );
    rig.router.metadata_cache.invalidate(&ino);
    let fetched = rig
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(ino))
        .await
        .expect("refetch");
    assert_eq!(
        fetched
            .block_map
            .as_deref()
            .and_then(|m| m.get(&0))
            .cloned(),
        Some(inline_key.clone()),
        "reads resolve the INLINE map, never the residue"
    );
    assert!(fetched
        .block_map
        .as_deref()
        .is_none_or(|m| !m.contains_key(&7) && !m.contains_key(&4_000_000)));

    // (b) The re-crossing reconciles: residue deleted (the far index
    // GONE — no resurrection), the tree is exactly the current map, and
    // the resumed counter names the reconciliation.
    let resumed_before = METRICS.map_migrate_resumed.load(Ordering::Relaxed);
    let mut entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
    entries.sort_unstable_by_key(|&(b, _)| b);
    // The re-published map re-binds index 0 too — fold the earlier
    // inline binding into the expectation only if the spill left it.
    let records = rig.tree_records(ino).await;
    assert!(
        records.iter().all(|(b, _)| *b < SPILL_BLOCKS),
        "no record survives outside the current map (the A1 sweep): {records:?}"
    );
    assert!(
        !records
            .iter()
            .any(|(_, k)| k == "888888888" || k == "999999999"),
        "the planted residue is GONE"
    );
    assert_eq!(
        METRICS.map_migrate_resumed.load(Ordering::Relaxed) - resumed_before,
        1,
        "a first crossing that found residue is counted as RESUMED"
    );
    assert_eq!(
        rig.durable_head(ino).await.block_map_id.as_deref(),
        Some("kvmap:1")
    );
    rig.shutdown().await;
}

// ===========================================================================
// 3. Byte identity: un-stamped volumes and the SQUEEZEFS_KVMAP=0 lever
// ===========================================================================

/// An UN-stamped volume's crossing takes the legacy blob arm verbatim —
/// the head spills to `indirect:`, bit 16 is never stamped (a local
/// crossing never self-arms a volume — the D9 caution), and the kvmap
/// counters stay flat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_volume_self_arms_at_its_first_crossing() {
    // Finding 43 (the kvmap reachability bug, caught by the first
    // live-mount smoke): PR 2's gate pre-probed `block_map_tree_engaged`
    // — true only after the tree exists — while the ONLY thing that ever
    // stamps bit 16 and mints the tree is the train BEHIND that gate,
    // and no offline stamping verb exists. Every real mount's crossings
    // took the legacy blob arm forever (the smoke: 1,324 indirect full
    // saves, zero map records) — the posture-dispatch reachability
    // class. The design's law (§2, the bit-5 KV_LAYOUT_DELTAS
    // precedent): the ratchet SELF-ARMS at first use — stamped+barriered
    // before the volume's first map record, never on untouched volumes
    // (a volume that never crosses stays byte-identical; that pin lives
    // in kvmap_tree_tests).
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    assert!(set_block_refcounts_bit(meta.path()).await.unwrap());
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert!(
        !rig.kv().block_map_tree_engaged(),
        "fixture: the volume starts un-stamped"
    );
    let ino = rig.mk_file("selfarm").await;

    let inos_before = METRICS.map_migrate_inos.load(Ordering::Relaxed);
    let entries = rig.publish_spill(ino, SPILL_BLOCKS).await;

    let head = rig.durable_head(ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("kvmap:1"),
        "the first crossing on a fresh volume must SELF-ARM and take the \
         tree (finding 43: the engaged-only gate made kvmap unreachable \
         on every real mount)"
    );
    assert_eq!(
        METRICS.map_migrate_inos.load(Ordering::Relaxed),
        inos_before + 1,
        "the train ran"
    );
    let records = rig.tree_records(ino).await;
    assert_eq!(
        records.len(),
        entries.len(),
        "the tree carries every mapping"
    );
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;

    let VolumeFormat::V3(sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    assert!(
        sb.block_map_tree_stamped(),
        "bit 16 is durable after the self-arming crossing"
    );
}

/// `SQUEEZEFS_KVMAP=0` (A10): NEW crossings take the legacy blob arm even
/// on a STAMPED volume — but an existing `kvmap:` head stays FORCE-kvmap
/// (the knob never disables head resolution), and the head is STICKY
/// (Rev 1.1 #5): a map shrunk below the cap never collapses back inline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_knob_governs_new_crossings_only_and_heads_stay_sticky() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let crossed = rig.mk_file("sticky").await;
    let fresh = rig.mk_file("knob_off").await;

    // Cross `crossed` with the knob ON (default).
    rig.publish_spill(crossed, SPILL_BLOCKS).await;
    assert_eq!(
        rig.durable_head(crossed).await.block_map_id.as_deref(),
        Some("kvmap:1")
    );

    // Knob OFF: a NEW crossing takes the blob arm on the same volume…
    // (restored on EVERY exit — a panicking leg must not poison the
    // suite's environment).
    struct KnobGuard;
    impl Drop for KnobGuard {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_KVMAP");
        }
    }
    let _knob = KnobGuard;
    std::env::set_var("SQUEEZEFS_KVMAP", "0");
    let out = async {
        rig.publish_spill(fresh, SPILL_BLOCKS).await;
        let head = rig.durable_head(fresh).await;
        assert!(
            head.block_map_id
                .as_deref()
                .is_some_and(|id| id.starts_with("indirect:")),
            "knob off ⇒ the legacy blob arm: {:?}",
            head.block_map_id
        );

        // …while the crossed ino's next save stays kvmap (FORCE — A10)
        // even though its map now SHRINKS below the inline cap (sticky —
        // Rev 1.1 #5).
        let keep: Vec<u32> = (0..4).collect();
        rig.router
            .merge_block_mappings(
                crossed,
                BlockMapOp::TruncateFrom {
                    new_size: 4 * 4 * 1024 * 1024,
                },
                0,
                LayoutFlip::KeepLayout,
                rig.token(crossed),
            )
            .await
            .expect("shrink");
        let head = rig.durable_head(crossed).await;
        assert_eq!(
            head.block_map_id.as_deref(),
            Some("kvmap:1"),
            "sticky: a shrunk map never collapses back inline"
        );
        assert!(head.block_map.is_none());
        let records = rig.tree_records(crossed).await;
        assert_eq!(
            records.iter().map(|(b, _)| *b).collect::<Vec<_>>(),
            keep,
            "the shrink's save reconciled the tree to the shrunk map"
        );
    };
    out.await;
    drop(_knob);
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ===========================================================================
// 4. Legacy `indirect:` conversion on first publish (design §2)
// ===========================================================================

/// A volume with live `indirect:` blob inos gets bit 16 stamped offline;
/// the FIRST publish under the new posture converts: head `kvmap:1`,
/// records complete, the blob's MAP_BLOB reference released exactly once
/// (in the flip tx), and the oracle stays clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_legacy_indirect_ino_converts_on_its_first_publish() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    assert!(set_block_refcounts_bit(meta.path()).await.unwrap());
    let data = data_file();
    let ino = {
        // Build the LEGACY head under `SQUEEZEFS_KVMAP=0` — since the
        // finding-43 self-arm fix, an un-stamped volume's crossing would
        // otherwise engage the tree; the knob is the sanctioned lever
        // that keeps the blob arm (A10).
        struct KnobGuard;
        impl Drop for KnobGuard {
            fn drop(&mut self) {
                std::env::remove_var("SQUEEZEFS_KVMAP");
            }
        }
        let _knob = KnobGuard;
        std::env::set_var("SQUEEZEFS_KVMAP", "0");
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("converted").await;
        rig.publish_spill(ino, SPILL_BLOCKS).await;
        let head = rig.durable_head(ino).await;
        assert!(head
            .block_map_id
            .as_deref()
            .is_some_and(|id| id.starts_with("indirect:")));
        let blobs = rig
            .kv()
            .block_ref_scan(squeezefs::meta_backend::kv::block_refs::volume_tag(
                DATA_VOL_ID,
            ))
            .await
            .expect("scan")
            .into_iter()
            .filter(|r| r.block_index == BLOCK_INDEX_MAP_BLOB)
            .count();
        assert_eq!(blobs, 1, "the blob carries its own durable reference");
        rig.shutdown().await;
        ino
    };

    // The offline upgrade act (the Phase-8 shape).
    assert!(set_block_map_tree_bit(meta.path()).await.unwrap());

    let rig = mount(meta.path(), data.path()).await;
    let inos_before = METRICS.map_migrate_inos.load(Ordering::Relaxed);
    // The first publish under the stamped posture: ONE more block.
    let extra = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(extra);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(SPILL_BLOCKS, extra.to_string())]),
            u64::from(SPILL_BLOCKS + 1) * 4 * 1024 * 1024,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("converting publish");

    let head = rig.durable_head(ino).await;
    assert_eq!(
        head.block_map_id.as_deref(),
        Some("kvmap:1"),
        "the conversion flipped the head"
    );
    assert_eq!(
        rig.tree_records(ino).await.len() as u32,
        SPILL_BLOCKS + 1,
        "the whole rehydrated map migrated"
    );
    assert_eq!(
        METRICS.map_migrate_inos.load(Ordering::Relaxed) - inos_before,
        1
    );
    let blobs = rig
        .kv()
        .block_ref_scan(squeezefs::meta_backend::kv::block_refs::volume_tag(
            DATA_VOL_ID,
        ))
        .await
        .expect("scan")
        .into_iter()
        .filter(|r| r.block_index == BLOCK_INDEX_MAP_BLOB)
        .count();
    assert_eq!(
        blobs, 0,
        "the displaced blob's MAP_BLOB reference released exactly once, in the flip tx"
    );
    assert!(
        rig.drift().await.is_empty(),
        "durable == derived across the conversion (the blob freed once, never twice)"
    );
    rig.shutdown().await;
}

// ===========================================================================
// 5. Pulled-forward safety (Rev 1.1 #2): the DERIVED recovery walk
// ===========================================================================

/// On a bit-9-ABSENT volume the mount walk derives ownership from layout
/// heads. A `kvmap:` head must yield its tree-7 blocks — pre-PR the walk
/// saw zero owned blocks and gap-completion free-listed LIVE data.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_derived_recovery_walk_owns_a_kvmap_inos_blocks() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    // Bit 16 WITHOUT bit 9: the derived-walk-only shape.
    format_meta(meta.path()).await;
    assert!(set_block_map_tree_bit(meta.path()).await.unwrap());
    let data = data_file();
    let (ino, entries) = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("derived").await;
        let entries = rig.publish_spill(ino, SPILL_BLOCKS).await;
        assert_eq!(
            rig.durable_head(ino).await.block_map_id.as_deref(),
            Some("kvmap:1")
        );
        rig.shutdown().await;
        (ino, entries)
    };
    let _ = ino;

    // A FRESH allocator (remount): the walk must claim every mapped
    // offset — refcounted, never free-listed.
    let rig = mount(meta.path(), data.path()).await;
    rig.alloc
        .recover_active_blocks_v3(rig.kv(), &rig.router.backend_router)
        .await
        .expect("derived recovery walk");
    let chunk = rig.alloc.chunk_size();
    for (b, key) in &entries {
        let offset: u64 = key.parse().unwrap();
        assert_eq!(
            rig.alloc.refcount(offset),
            Some(1),
            "block {b} (offset {offset}) is OWNED after the walk — a kvmap head must \
             never read as zero owned blocks (gap-completion would free-list live data)"
        );
        assert!(
            !rig.alloc.free_block_indices().contains(&(offset / chunk)),
            "offset {offset} must not be free-listed"
        );
    }
    rig.shutdown().await;
}

// ===========================================================================
// 6. Unlink sweeps the records (Rev 1.1 #4)
// ===========================================================================

/// `delete_file` on a kvmap ino releases its durable references AND
/// sweeps its tree-7 records — the tree reads empty, never silent
/// residue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlink_sweeps_the_records() {
    let _serial = serial();
    let meta = NamedTempFile::new().unwrap();
    format_meta_kvmap(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("doomed").await;
    rig.publish_spill(ino, SPILL_BLOCKS).await;
    assert_eq!(rig.tree_records(ino).await.len() as u32, SPILL_BLOCKS);

    // Production reclaim order: unlink → delete_file → destroy.
    rig.routed.unlink(1, "doomed").await.expect("unlink");
    rig.router
        .delete_file(&squeezefs::keys::inode_path(ino))
        .await
        .expect("delete_file");
    assert!(
        rig.tree_records(ino).await.is_empty(),
        "the bounded synchronous sweep leaves NO records"
    );
    rig.routed.destroy_inodes(&[ino]).await.expect("destroy");
    assert!(rig.drift().await.is_empty(), "after reclaim");
    rig.shutdown().await;
}

// ===========================================================================
// 7. The shipped verb (A4 + the finding-36b whole-claim-set law)
// ===========================================================================
// The mw_publish_era_gate_tests harness: one authority backend + listener,
// one all-foreign client backend, a real custody join.

const SECRET: &[u8] = b"kvmap-crossing-storage-trust-secret";
const NODE: &str = "node-kvmap-crossing-a";
const VOL_LEN: u64 = 256 * 1024 * 1024;

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        data_grant::uninstall_custody_client();
        data_grant::uninstall_custody_owner();
        publish::uninstall_client();
        ship::disarm_ownership();
        data_custody::test_reset_custody_generation();
        data_custody::test_clear_poison();
    }
}

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

async fn sandbox(dir: &Path, tag: &str) -> (Arc<RoutedMetaBackend>, PathBuf) {
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
    // The OWNER's volume deliberately carries NO bit 16: the shipped
    // crossing is the ratchet's one live un-engaged caller (the owner
    // cannot be probed by the co-writer, so it self-arms).
    let routed = squeezefs::meta_backend::open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("open routed set");
    (routed, p)
}

async fn shutdown_set(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

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
        "kvmap-authority",
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

async fn arm_client(
    auth: &Authority,
    client_be: &Arc<RoutedMetaBackend>,
) -> (Arc<WriteCustodyClient>, Arc<publish::PublishClient>) {
    let foreign: Vec<(usize, PeerOwner)> = (0..client_be.volumes.len())
        .map(|v| (v, PeerOwner::new("kvmap-authority", &auth.endpoint)))
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

fn journal_entries() -> u64 {
    META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed)
}

/// The shipped-crossing contracts (the FreeBlocks template): the owner's
/// ratchet SELF-ARMS its un-stamped volume (stamp + mint — the one live
/// un-engaged caller), the train executes under the witness window (a
/// verbatim replay answers the winner's own outcome and stages nothing),
/// the routed helper is idempotent (a second logical ship diffs to zero
/// ops), and a DEAD era refuses before the window with nothing applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shipped_crossing_is_witnessed_owner_ratcheted_and_era_gated() {
    let _serial = serial();
    let _restore = Restore;
    let dir = TempDir::new().unwrap();
    let (owner_be, owner_path) = sandbox(dir.path(), "own").await;
    let (client_be, _p2) = sandbox(dir.path(), "cli").await;
    let auth = start_authority(Arc::clone(&owner_be));
    let (client, pc) = arm_client(&auth, &client_be).await;
    let epoch = client.lease_epoch();

    assert!(
        !owner_be.volumes[0].block_map_tree_engaged(),
        "the owner's volume starts UN-stamped — the shipped crossing is the ratchet's caller"
    );

    let ino = publish::create_with_rdev_size(
        &client_be,
        1,
        "crossing.bin",
        libc::S_IFREG | 0o644,
        0,
        0,
        0,
        0,
    )
    .await
    .expect("create ships")
    .ino;

    let entries: Vec<(u32, String)> = (0..3u32)
        .map(|b| (b, (u64::from(b) * 4194304).to_string()))
        .collect();
    let layout = kvmap_head_bytes(3 * 4194304);

    // P1: the crossing, shipped with an explicit witness key.
    let p1 = publish::PublishCall::MigrateBlockMap {
        ino,
        layout: layout.clone(),
        size: 3 * 4194304,
        entries: entries.clone(),
        refs: Vec::new(),
        base_gen: 0,
        lease_epoch: epoch,
        request_id: 0xD1,
    };
    let first = pc.ship(&auth.endpoint, p1.clone()).await.expect("P1 lands");
    let publish::PublishReply::MapMigrated {
        records,
        preexisting,
        ..
    } = first
    else {
        panic!("the crossing answers its accounting: {first:?}");
    };
    assert_eq!(records, 3);
    assert_eq!(preexisting, 0);

    // The ratchet self-armed: bit 16 durable, tree engaged, records live.
    assert!(owner_be.volumes[0].block_map_tree_engaged());
    let VolumeFormat::V3(sb) = classify_volume(&owner_path).await.unwrap() else {
        panic!("expected v3");
    };
    assert!(
        sb.block_map_tree_stamped(),
        "the owner stamped bit 16 BEFORE its first record (the ratchet's ordering law)"
    );
    let head_bytes = owner_be.volumes[0]
        .getxattr(ino, "layout")
        .await
        .unwrap()
        .expect("owner head");
    let head: LayoutMetadata = bincode::deserialize(&head_bytes).unwrap();
    assert_eq!(head.block_map_id.as_deref(), Some("kvmap:1"));
    assert_eq!(
        owner_be.volumes[0]
            .block_map_range(ino, 0, 16)
            .await
            .unwrap()
            .len(),
        3
    );

    // The replay: SAME frame, SAME witness — answered from the window,
    // nothing staged (journal-entry equality), counted as a replay.
    let replays_before = publish::stats().replays;
    let journal_before = journal_entries();
    let replayed = pc
        .ship(&auth.endpoint, p1)
        .await
        .expect("the duplicate is ANSWERED, not re-run");
    assert_eq!(replayed, first, "the winner's own cached outcome");
    assert_eq!(publish::stats().replays - replays_before, 1);
    assert_eq!(
        journal_entries(),
        journal_before,
        "a replay stages NOTHING — the train never re-runs"
    );

    // The routed helper (a SECOND logical publish of the same map): the
    // ledger row moves and the train's diff is empty — idempotent.
    let shipped_before = publish::stats().map_shipped;
    let served_before = publish::stats().map_served;
    let outcome = publish::migrate_block_map(
        &client_be,
        ino,
        &layout,
        3 * 4194304,
        entries.clone(),
        Vec::new(),
        512,
    )
    .await
    .expect("the helper ships");
    assert_eq!(outcome.records, 0, "an identical map diffs to zero ops");
    assert_eq!(outcome.preexisting, 3);
    assert_eq!(publish::stats().map_shipped - shipped_before, 1);
    assert_eq!(publish::stats().map_served - served_before, 1);

    // The era gate (the FreeBlocks template, refused BEFORE the window):
    // after the sweep, a fresh frame under the dead epoch refuses loud,
    // nothing is applied, and the refusal is the client's fence signal.
    auth.owner.revoke_client(NODE, "test: swept past its TTL");
    let stale_before = publish::stats().stale_refusals;
    let journal_before = journal_entries();
    let err = pc
        .ship(
            &auth.endpoint,
            publish::PublishCall::MigrateBlockMap {
                ino,
                layout: kvmap_head_bytes(9 * 4194304),
                size: 9 * 4194304,
                entries: vec![(8, "777777777".to_string())],
                refs: Vec::new(),
                base_gen: 1,
                lease_epoch: epoch,
                request_id: 0xD2,
            },
        )
        .await
        .expect_err("a swept era's crossing refuses");
    assert!(
        matches!(err, SqueezefsError::WriterGuardFenced),
        "the refusal surfaces in the fence class: {err:?}"
    );
    assert_eq!(publish::stats().stale_refusals - stale_before, 1);
    assert_eq!(journal_entries(), journal_before, "nothing applied");
    assert_eq!(
        owner_be.volumes[0]
            .block_map_range(ino, 0, 16)
            .await
            .unwrap()
            .len(),
        3,
        "the dead era's entries never landed"
    );

    auth.listener.shutdown();
    shutdown_set(&owner_be).await;
    shutdown_set(&client_be).await;
}
