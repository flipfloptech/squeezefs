//! **Durable block refcounts and free list** — pre-RC engineering spec
//! §6.2 **item 1** ("the largest item"), behind incompat bit 8, ruling
//! **D9** (built, NOT stamped on existing volumes).
//!
//! Before this, `BlockAllocator`'s refcount map and free list were
//! re-derived at every mount by walking the live inode tree and reading
//! each ino's `layout` xattr — the code's own words: *"both are
//! mount-session RAM, rebuilt at mount — pure derived state."* The spec's
//! verdict: *"Without durable shared ownership accounting, no
//! multi-writer data path is expressible"*, because two writers each
//! derive a private answer from the subset of the tree they walked, and a
//! block that node A holds at refcount 2 reads 1 on node B — whose W1
//! sole-owner patch then rewrites, in place, a block A also references
//! (`patch_ineligible_shared` never increments; silent on a passthrough
//! volume).
//!
//! Contracts pinned here:
//!
//! 1. **Compatibility matrix.** A fresh format carries bit 8 and a
//!    block-reference tree root; an UN-stamped volume mounts with its
//!    superblock byte-identical, no tree, no records, and the derived
//!    walk — and stamping the bit engages the machinery on the next
//!    mount.
//! 2. **Durability (acceptance a).** Durable references survive a
//!    crash + remount, seed the allocator with **no inode-tree walk**,
//!    and equal the derived answer exactly.
//! 3. **Sharing (acceptance b).** A clone's shared blocks read refcount 2
//!    DURABLY.
//! 4. **The free window (acceptance c).** A crash mid-`begin_free`
//!    recovers without leaking and without double-freeing.
//! 5. **The oracle (acceptance d).** Durable and derived agree exactly
//!    across a workload mixing striped writes, clones, truncates,
//!    overwrites and reclaims.
//! 6. **One transaction.** The accounting rides the layout commit — a
//!    publish costs the SAME number of journal entries with accounting as
//!    without it (the write-commit-economy collapse is not re-split).
//! 7. **Power cut.** With the data device's volatile cache lost, the
//!    surviving ledger and the surviving layout agree (one tx = one
//!    checksummed journal entry, §4.10).

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_refs::{self, BLOCK_INDEX_MAP_BLOB};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::record::TREE_BLOCK_REFS;
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_refcounts_bit, write_superblock_v3, VolumeFormat,
    FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
/// Sparse on purpose: the indirect-map-spill leg needs > 1000 mapped
/// blocks to push the layout value past the volume's xattr cap, and the
/// tests allocate offsets without writing most of them.
const DATA_LEN: u64 = 8 * 1024 * 1024 * 1024;
/// The data volume's durable id — stable across remounts, which is what
/// makes [`block_refs::volume_tag`] a usable key component (KD-5).
const DATA_VOL_ID: &str = "vol-00000000000000a1";

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// One mounted harness: a real v3 meta volume, a real file-backed data
/// volume, and the router that binds them.
struct Rig {
    router: DataRouter,
    alloc: Arc<BlockAllocator>,
    routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

/// Format a v3 meta volume the way `squeezefs format` does today —
/// **without** incompat bit 8 (ruling D9: build the bit, do not stamp it).
/// A volume formatted this way mounts with DERIVED block accounting.
async fn format_meta(path: &std::path::Path) {
    format_v3(path, META_LEN, &opts())
        .await
        .expect("format v3 meta volume");
    // This suite owns its own stamping (it tests BOTH sides of the D9
    // boundary), so it must be immune to the `SQUEEZEFS_TEST_STAMP_BLOCK_REFS`
    // seam that points OTHER suites at the durable path: strip the bit if the
    // seam set it, leaving the shape production `format` actually writes.
    let VolumeFormat::V3(mut sb) = classify_volume(path).await.unwrap() else {
        panic!("expected v3");
    };
    if sb.features_incompat & FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS != 0 {
        sb.features_incompat &= !FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS;
        write_superblock_v3(path, &sb).await.unwrap();
    }
}

/// [`format_meta`] plus the Phase-8 stamp — the on-disk state every leg
/// that exercises the DURABLE machinery needs. Stamping an empty volume is
/// the trivially safe case (the backfill hazard is
/// `stamping_a_non_empty_volume_backfills_instead_of_freeing_live_blocks`);
/// the first mount mints the missing root.
async fn format_meta_stamped(path: &std::path::Path) {
    format_meta(path).await;
    assert!(
        set_block_refcounts_bit(path).await.expect("stamp bit 8"),
        "a fresh format must NOT already carry bit 8 — the stamp is the \
         Phase-8 window's act, not format's"
    );
}

async fn mount(meta: &std::path::Path, data: &std::path::Path) -> Rig {
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

    /// Allocate a real block and bind it at `block_index` of `ino`
    /// through the shared merge primitive — the one place a striped block
    /// map changes, and therefore the one place the accounting is
    /// computed.
    async fn publish_block(&self, ino: u64, block_index: u32) -> u64 {
        let offset = self.alloc.allocate_block().await.expect("allocate");
        self.alloc.publish_block(offset);
        let key = offset.to_string();
        self.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[(block_index, key)]),
                (block_index as u64 + 1) * 4 * 1024 * 1024,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.token(ino),
            )
            .await
            .expect("merge published block");
        offset
    }

    /// Every durable reference recorded on the (single) meta volume for
    /// this rig's data volume.
    async fn durable(&self) -> Vec<block_refs::BlockRef> {
        let tag = block_refs::volume_tag(DATA_VOL_ID);
        self.routed.volumes[0]
            .block_ref_scan(tag)
            .await
            .expect("durable reference scan")
    }

    /// The durable refcount of one block index — the population of the
    /// `(vol_tag, block_idx)` key prefix.
    async fn durable_refcount(&self, offset: u64) -> u32 {
        let idx = offset / self.alloc.chunk_size();
        self.durable()
            .await
            .iter()
            .filter(|r| r.block_idx == idx)
            .count() as u32
    }

    /// The oracle: durable-vs-derived, exact or the drifting blocks.
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

// ---------------------------------------------------------------------------
// 1. The compatibility matrix (ruling D9: built, NOT stamped).
// ---------------------------------------------------------------------------

/// **Ruling D9, at the format boundary**: a fresh format must NOT carry
/// bit 8, and must mount with DERIVED accounting.
///
/// This is a safety property, not only discipline. The durable ledger is
/// only as complete as the set of write-path sites that stage into it, and
/// while that wiring is incomplete a partially-populated ledger is the
/// DANGEROUS state — it is non-empty, so the "an empty population is never
/// authoritative" rule does not fire, and every reference an unwired site
/// failed to stage reads back as a free block, which `recover_block` then
/// hands to the next writer. Derived accounting cannot fail that way: it
/// re-reads the layouts, which are always complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_format_does_not_carry_bit8_and_mounts_derived() {
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;

    let VolumeFormat::V3(sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
        0,
        "format must not stamp bit 8 — the batched Phase-8 reformat window owns \
         that act (ruling D9), and a fresh format mounting derived is what keeps a \
         partially-wired ledger from ever being trusted"
    );
    assert_eq!(
        FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
        1 << 8,
        "the execution plan's §6.2 item-1 bit is 8"
    );
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
        0,
        "this binary must understand bit 8"
    );
    assert_eq!(TREE_BLOCK_REFS, 6, "the §4.2 tree table pins the id");

    let kv = KvMetaBackend::open(meta.path()).await.expect("mount");
    assert!(
        !kv.block_refs_engaged(),
        "a fresh format mounts with DERIVED accounting — no tree, no records"
    );
    kv.shutdown().await.unwrap();

    // …and the stamp is what engages it, on an EMPTY volume (the trivially
    // safe case; the non-empty one is the backfill leg below).
    format_meta_stamped(meta.path()).await;
    let kv = KvMetaBackend::open(meta.path())
        .await
        .expect("mount post-stamp");
    assert!(kv.block_refs_engaged(), "the stamp engages the machinery");
    assert_eq!(
        kv.block_ref_scan(block_refs::volume_tag(DATA_VOL_ID))
            .await
            .unwrap()
            .len(),
        0,
        "an empty volume references no blocks"
    );
    kv.shutdown().await.unwrap();
}

/// **The D9 contract**: an UN-stamped volume (every volume formatted
/// before the bit existed) mounts and behaves exactly as it did — derived
/// accounting, no tree, no records — and its superblock is **byte-identical
/// after the mount**. Mount never stamps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstamped_volume_is_unchanged_by_mount_and_stays_derived() {
    let meta = NamedTempFile::new().unwrap();
    // No stripping needed: today's `format` IS the un-stamped shape (D9).
    format_meta(meta.path()).await;

    let before = std::fs::read(meta.path()).unwrap()[..4096].to_vec();

    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert!(
        !rig.routed.volumes[0].block_refs_engaged(),
        "an un-stamped volume must not engage durable accounting"
    );

    // A real publish must still work and must record NOTHING durably.
    let ino = rig.mk_file("unstamped").await;
    let offset = rig.publish_block(ino, 0).await;
    assert_eq!(
        rig.durable().await.len(),
        0,
        "an un-stamped volume must never stage an accounting record"
    );
    assert_eq!(
        rig.alloc.refcount(offset),
        Some(1),
        "RAM accounting is unchanged on an un-stamped volume"
    );
    // The durable recovery path must decline, so the caller falls back to
    // the derived walk (pre-item-1 behavior verbatim).
    assert!(
        rig.router
            .backend_router
            .recover_durable_block_refs(&rig.routed)
            .await
            .unwrap()
            .is_none(),
        "with no engaged volume the durable path must decline, not fabricate"
    );
    rig.shutdown().await;

    let after = std::fs::read(meta.path()).unwrap()[..4096].to_vec();
    assert_eq!(
        before, after,
        "mounting an un-stamped volume must leave sector 0 byte-identical \
         (the batched reformat window owns the stamp — ruling D9)"
    );
}

/// The upgrade path: stamping the bit engages the machinery on the next
/// mount, which mints the missing root itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stamping_the_bit_engages_accounting_on_the_next_mount() {
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    assert!(
        set_block_refcounts_bit(meta.path()).await.unwrap(),
        "stamping a fresh bit reports the write"
    );
    assert!(
        !set_block_refcounts_bit(meta.path()).await.unwrap(),
        "re-stamping is a no-op"
    );

    // The stamped volume's ledger names no block-reference root: the mount
    // must mint one rather than refuse.
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert!(
        rig.routed.volumes[0].block_refs_engaged(),
        "the stamp engages accounting"
    );
    let ino = rig.mk_file("post_stamp").await;
    let offset = rig.publish_block(ino, 0).await;
    assert_eq!(
        rig.durable_refcount(offset).await,
        1,
        "the freshly minted tree accepts records"
    );
    assert!(rig.drift().await.is_empty(), "durable == derived");
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. Acceptance (a): durability across a crash, with no inode walk.
// ---------------------------------------------------------------------------

/// Publish blocks, **kill the mount without a clean shutdown**, remount,
/// and seed the allocator from durable records ONLY. The seeded refcounts,
/// free list and cursor must equal the derived answer exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_refs_survive_a_crash_and_equal_the_derived_answer() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();

    let (ino, offsets) = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("crash_survivor").await;
        let mut offsets = Vec::new();
        for b in 0..4u32 {
            offsets.push(rig.publish_block(ino, b).await);
        }
        assert_eq!(rig.durable().await.len(), 4);
        assert!(rig.drift().await.is_empty());
        // CRASH: no `shutdown()`, so nothing is checkpointed on purpose —
        // the records survive only through the journal, which is exactly
        // the §4.10 whole-transaction-atomicity claim under test.
        drop(rig);
        (ino, offsets)
    };

    let rig = mount(meta.path(), data.path()).await;
    let seeded = rig
        .router
        .backend_router
        .recover_durable_block_refs(&rig.routed)
        .await
        .expect("durable recovery")
        .expect("bit 8 is stamped, so the durable path must run");
    assert_eq!(seeded, 4, "every reference replayed out of the journal");

    // The seeded RAM state, block for block.
    for off in &offsets {
        assert_eq!(
            rig.alloc.refcount(*off),
            Some(1),
            "block {off} must be tracked from durable records alone"
        );
    }
    assert_eq!(
        rig.alloc.highest_block_index(),
        4,
        "the allocation cursor derives from the durable reference set"
    );
    assert_eq!(
        rig.alloc.free_blocks_count(),
        0,
        "no gaps: every block below the cursor is referenced"
    );
    // A fresh allocation must not alias a durably-referenced offset — the
    // property the whole structure exists to guarantee.
    let fresh = rig.alloc.allocate_block().await.expect("fresh allocation");
    assert!(
        !offsets.contains(&fresh),
        "allocator handed out a durably-referenced offset {fresh}"
    );

    // The oracle, on a mount that never walked the tree.
    assert!(
        rig.drift().await.is_empty(),
        "durable seed must equal the derived census exactly"
    );
    let _ = ino;
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. Acceptance (b): a clone's shared blocks are DURABLY shared.
// ---------------------------------------------------------------------------

/// The §6.3 W1 hazard, closed durably: a cloned block must read refcount
/// **2** from the durable ledger, so a remounted (or second) writer cannot
/// conclude it is the sole owner and patch it in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_records_durable_refcount_two_and_survives_remount() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();

    let offset = {
        let rig = mount(meta.path(), data.path()).await;
        let src = rig.mk_file("clone_src").await;
        let dst = rig.mk_file("clone_dst").await;
        let offset = rig.publish_block(src, 0).await;
        assert_eq!(rig.durable_refcount(offset).await, 1);

        rig.router
            .clone_file(
                &squeezefs::keys::inode_path(src),
                &squeezefs::keys::inode_path(dst),
                Some(rig.token(src)),
                Some(rig.token(dst)),
            )
            .await
            .expect("clone");

        assert_eq!(
            rig.alloc.refcount(offset),
            Some(2),
            "the RAM refcount reflects the clone's pin"
        );
        assert_eq!(
            rig.durable_refcount(offset).await,
            2,
            "the DURABLE ledger must record the clone's shared ownership"
        );
        let refs = rig.durable().await;
        let owners: std::collections::BTreeSet<u64> = refs.iter().map(|r| r.owner_ino).collect();
        assert_eq!(owners.len(), 2, "one record per OWNER: {refs:?}");
        assert!(rig.drift().await.is_empty(), "durable == derived");
        rig.shutdown().await;
        offset
    };

    // Remount from durable records only: sharing must survive.
    let rig = mount(meta.path(), data.path()).await;
    let seeded = rig
        .router
        .backend_router
        .recover_durable_block_refs(&rig.routed)
        .await
        .unwrap()
        .expect("durable path");
    assert_eq!(seeded, 2);
    assert_eq!(
        rig.alloc.refcount(offset),
        Some(2),
        "a remount that never walked the tree still knows the block is SHARED — \
         this is the state whose absence makes the §6.3 W1 patch corrupt data"
    );
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. Acceptance (c): the begin_free → reclaim → finish_free window.
// ---------------------------------------------------------------------------

/// A crash inside the free window recovers to one of exactly two states —
/// referenced or free — never a leak and never a double free.
///
/// Both halves are checked:
///
/// * **After the publish that dropped the reference** (the durable
///   `Delete` committed): the block recovers FREE, is reallocatable, and
///   the derived walk agrees — so nothing leaked.
/// * **Before it** (the reference still recorded, the reclaimer's
///   `finish_free` lost): the block recovers ALLOCATED — conservative, so
///   the offset can never be minted to a second owner, and a `begin_free`
///   of an untracked offset is refused rather than double-freeing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_in_the_free_window_neither_leaks_nor_double_frees() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();

    // --- Half 1: the durable Delete landed. -----------------------------
    let freed = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("freewindow").await;
        let keep = rig.publish_block(ino, 0).await;
        // The doomed block is a MIDDLE one, so its recovery lands on the
        // free list proper rather than in the virgin tail above the cursor.
        let doomed = rig.publish_block(ino, 1).await;
        let _tail = rig.publish_block(ino, 2).await;
        assert_eq!(rig.durable().await.len(), 3);

        // Punch block 1: the merge removes it from the map and stages the
        // durable release in the SAME commit, then the blocks are freed.
        rig.router
            .punch_striped_blocks(ino, &[1], 3 * 4 * 1024 * 1024, rig.token(ino))
            .await
            .expect("punch");
        assert_eq!(
            rig.durable_refcount(doomed).await,
            0,
            "the punched block's durable reference is gone"
        );
        assert_eq!(rig.durable_refcount(keep).await, 1);
        assert!(rig.drift().await.is_empty());
        // CRASH here — the reclaimer's device work and `finish_free` are
        // exactly what a power loss drops.
        drop(rig);
        doomed
    };

    {
        let rig = mount(meta.path(), data.path()).await;
        rig.router
            .backend_router
            .recover_durable_block_refs(&rig.routed)
            .await
            .unwrap()
            .expect("durable path");
        assert_eq!(
            rig.alloc.refcount(freed),
            None,
            "an unreferenced block must recover UNTRACKED (free), never leaked"
        );
        assert!(
            rig.alloc
                .free_block_indices()
                .contains(&(freed / rig.alloc.chunk_size())),
            "the freed block must be back on the free list — the free list IS the \
             complement of the durable reference set below the cursor"
        );
        // Not double-freeable: a release of an untracked offset is refused.
        assert!(
            !rig.alloc.begin_free(freed),
            "begin_free of an unreferenced offset must be REFUSED (the double-release \
             lineage that mints one offset to two owners)"
        );
        // …and it is genuinely reusable: the next claim takes the gap.
        assert_eq!(
            rig.alloc.allocate_block().await.expect("reuse the gap"),
            freed,
            "the recovered free list must hand the freed offset back out"
        );
        assert!(rig.drift().await.is_empty());
        rig.shutdown().await;
    }

    // --- Half 2: the crash preceded the release commit. -----------------
    let meta2 = NamedTempFile::new().unwrap();
    format_meta_stamped(meta2.path()).await;
    let data2 = data_file();
    let pinned = {
        let rig = mount(meta2.path(), data2.path()).await;
        let ino = rig.mk_file("halffreed").await;
        let offset = rig.publish_block(ino, 0).await;
        // The reclaimer's half of the window WITHOUT the meta commit: the
        // RAM release happened, the durable record did not (a crash between
        // the two is the window under test).
        assert!(rig.alloc.begin_free(offset), "terminal RAM release");
        assert_eq!(
            rig.durable_refcount(offset).await,
            1,
            "the durable record is untouched by a RAM-only release"
        );
        drop(rig);
        offset
    };
    let rig = mount(meta2.path(), data2.path()).await;
    rig.router
        .backend_router
        .recover_durable_block_refs(&rig.routed)
        .await
        .unwrap()
        .expect("durable path");
    assert_eq!(
        rig.alloc.refcount(pinned),
        Some(1),
        "a still-recorded reference recovers ALLOCATED — conservative, so the offset \
         can never be minted to a second owner"
    );
    assert!(
        !rig.alloc
            .free_block_indices()
            .contains(&(pinned / rig.alloc.chunk_size())),
        "a recorded block must not be on the free list"
    );
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5. Acceptance (d): the oracle across a mixed workload.
// ---------------------------------------------------------------------------

/// Striped writes, a clone, overwrites (displacement), a truncate, a
/// punch, and a reclaim — durable and derived must agree **exactly** at
/// every step, and again after a remount that never walks the tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_matches_derived_across_a_mixed_workload() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;

    let a = rig.mk_file("mixed_a").await;
    let b = rig.mk_file("mixed_b").await;

    // (i) striped writes
    let mut a_blocks = Vec::new();
    for i in 0..6u32 {
        a_blocks.push(rig.publish_block(a, i).await);
    }
    assert!(rig.drift().await.is_empty(), "after striped writes");

    // (ii) clone (shared ownership)
    rig.router
        .clone_file(
            &squeezefs::keys::inode_path(a),
            &squeezefs::keys::inode_path(b),
            Some(rig.token(a)),
            Some(rig.token(b)),
        )
        .await
        .expect("clone");
    assert!(rig.drift().await.is_empty(), "after clone");
    for off in &a_blocks {
        assert_eq!(rig.durable_refcount(*off).await, 2, "shared block {off}");
    }

    // (iii) overwrite = displacement: a new offset replaces block 2 of `a`
    let replacement = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(replacement);
    let displaced = rig
        .router
        .merge_block_mappings(
            a,
            BlockMapOp::Merge(&[(2, replacement.to_string())]),
            6 * 4 * 1024 * 1024,
            LayoutFlip::KeepLayout,
            rig.token(a),
        )
        .await
        .expect("overwrite merge");
    assert_eq!(displaced.len(), 1, "one displaced key");
    assert!(rig.drift().await.is_empty(), "after displacement");
    assert_eq!(
        rig.durable_refcount(replacement).await,
        1,
        "the replacement is referenced once (by `a`)"
    );
    assert_eq!(
        rig.durable_refcount(a_blocks[2]).await,
        1,
        "the displaced block keeps the CLONE's reference — freeing it here would \
         destroy the clone's bytes"
    );
    // The displaced key's block is still shared, so the free is non-terminal.
    for key in &displaced {
        rig.router.backend_router.free_block(key).await.unwrap();
    }
    assert!(rig.drift().await.is_empty(), "after the non-terminal free");

    // (iv) truncate `a` down to two blocks
    rig.router
        .truncate_layout(a, 2 * 4 * 1024 * 1024, rig.token(a))
        .await
        .expect("truncate");
    assert!(rig.drift().await.is_empty(), "after truncate");

    // (v) punch a hole in `b`
    rig.router
        .punch_striped_blocks(b, &[4], 6 * 4 * 1024 * 1024, rig.token(b))
        .await
        .expect("punch");
    assert!(rig.drift().await.is_empty(), "after punch");

    // (vi) reclaim `b` entirely (unlink → reclaim's data teardown)
    rig.routed.unlink(1, "mixed_b").await.expect("unlink");
    rig.router
        .delete_file(&squeezefs::keys::inode_path(b))
        .await
        .expect("delete_file");
    assert!(rig.drift().await.is_empty(), "after reclaim");

    rig.shutdown().await;

    // (vii) and the same after a remount seeded ONLY from durable records
    let rig = mount(meta.path(), data.path()).await;
    rig.router
        .backend_router
        .recover_durable_block_refs(&rig.routed)
        .await
        .unwrap()
        .expect("durable path");
    assert!(
        rig.drift().await.is_empty(),
        "durable and derived must still agree exactly after a remount with no walk"
    );
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 6. One transaction: the accounting must not add a commit.
// ---------------------------------------------------------------------------

/// The write-commit-economy campaign collapsed the block publish into one
/// commit precisely so the journal-entry-per-publish term would stop
/// growing. Durable accounting **rides** that commit: a publish costs the
/// same number of journal entries with the ledger as without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accounting_rides_the_publish_transaction_and_adds_no_commit() {
    use squeezefs::meta_backend::kv::{META_KV_BLOCK_REFS_STAGED, META_KV_JOURNAL_ENTRIES};
    use std::sync::atomic::Ordering;

    const PUBLISHES: u32 = 8;

    // Leg A: an UN-stamped volume (today's `format` default) — the
    // pre-item-1 entry cost.
    let meta_a = NamedTempFile::new().unwrap();
    format_meta(meta_a.path()).await;
    let data_a = data_file();
    let rig = mount(meta_a.path(), data_a.path()).await;
    let ino = rig.mk_file("entries_plain").await;
    let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    for b in 0..PUBLISHES {
        rig.publish_block(ino, b).await;
    }
    let plain_entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - before;
    rig.shutdown().await;

    // Leg B: the same op sequence WITH durable accounting engaged.
    let meta_b = NamedTempFile::new().unwrap();
    format_meta_stamped(meta_b.path()).await;
    let data_b = data_file();
    let rig = mount(meta_b.path(), data_b.path()).await;
    let ino = rig.mk_file("entries_accounted").await;
    let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let staged_before = META_KV_BLOCK_REFS_STAGED.load(Ordering::Relaxed);
    for b in 0..PUBLISHES {
        rig.publish_block(ino, b).await;
    }
    let accounted_entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - before;
    let staged = META_KV_BLOCK_REFS_STAGED.load(Ordering::Relaxed) - staged_before;

    assert_eq!(
        staged, PUBLISHES as u64,
        "one durable reference staged per published block (the engagement instrument)"
    );
    assert_eq!(
        accounted_entries,
        plain_entries,
        "durable accounting must ride the layout commit — it added {} journal \
         entr(ies) over the un-accounted leg, which would re-split the \
         write-commit-economy collapse",
        accounted_entries as i64 - plain_entries as i64
    );
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 7. Power cut on the data device.
// ---------------------------------------------------------------------------

/// With the data device's volatile cache lost mid-workload, the surviving
/// durable ledger and the surviving layouts must still agree: they ride
/// ONE checksummed journal entry, so no crash prefix can separate them
/// (§4.10).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_data_device_power_cut_leaves_ledger_and_layout_agreeing() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();
    let data_path = data.path().to_str().unwrap().to_string();

    squeezefs::dev_power_cut::arm_power_cut(&data_path);
    let expect_refs = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("powercut").await;
        for b in 0..3u32 {
            rig.publish_block(ino, b).await;
        }
        let refs = rig.durable().await.len();
        // Quiesce, then cut: the harness models a volatile cache, so the
        // caller must have no I/O in flight (its documented contract).
        rig.shutdown().await;
        refs
    };
    let reverted = squeezefs::dev_power_cut::power_cut(&data_path);
    squeezefs::dev_power_cut::clear_faults_for(&data_path);

    let rig = mount(meta.path(), data.path()).await;
    let seeded = rig
        .router
        .backend_router
        .recover_durable_block_refs(&rig.routed)
        .await
        .unwrap()
        .expect("durable path");
    assert_eq!(
        seeded, expect_refs as u64,
        "the ledger is metadata-plane state: a DATA-device cache loss ({reverted} \
         write(s) reverted) cannot lose references the layouts still name"
    );
    assert!(
        rig.drift().await.is_empty(),
        "ledger and layouts must agree after the cut"
    );
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 8. The indirect-map blob's own reference.
// ---------------------------------------------------------------------------

/// An indirect block map lives in a DATA block that the layout record
/// references, and the mount-time walk counts it. So must the ledger —
/// under the `BLOCK_INDEX_MAP_BLOB` sentinel — or a remount would hand the
/// live map's block to the next writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_indirect_map_blob_carries_its_own_durable_reference() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("blobbed").await;

    // Force the inline → indirect spill: a map larger than the volume's
    // xattr value cap. The rig's 64 KiB nodes give a 16 KiB cap, so a few
    // hundred entries spill.
    let mut entries: Vec<(u32, String)> = Vec::new();
    for b in 0..1200u32 {
        let offset = rig.alloc.allocate_block().await.expect("allocate");
        rig.alloc.publish_block(offset);
        entries.push((b, offset.to_string()));
    }
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&entries),
            1200 * 4 * 1024 * 1024,
            LayoutFlip::ToStripedKeepStagedIdentity,
            rig.token(ino),
        )
        .await
        .expect("merge a spilling map");

    let refs = rig.durable().await;
    let blobs: Vec<_> = refs
        .iter()
        .filter(|r| r.block_index == BLOCK_INDEX_MAP_BLOB)
        .collect();
    assert_eq!(
        blobs.len(),
        1,
        "exactly one indirect-map blob reference (found {}: {:?})",
        blobs.len(),
        refs.iter().take(4).collect::<Vec<_>>()
    );
    assert!(blobs[0].is_map_blob());
    assert_eq!(
        refs.len(),
        entries.len() + 1,
        "every map entry plus the blob itself"
    );
    assert!(
        rig.drift().await.is_empty(),
        "the walk counts the blob too — the census must match it"
    );

    // A second publish CoWs the blob: the old blob's reference must be
    // released in the same transaction that stops naming it.
    let old_blob = blobs[0].block_idx;
    let extra = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(extra);
    rig.router
        .merge_block_mappings(
            ino,
            BlockMapOp::Merge(&[(1200, extra.to_string())]),
            1201 * 4 * 1024 * 1024,
            LayoutFlip::KeepLayout,
            rig.token(ino),
        )
        .await
        .expect("second spilling merge");
    let refs = rig.durable().await;
    let blobs: Vec<_> = refs
        .iter()
        .filter(|r| r.block_index == BLOCK_INDEX_MAP_BLOB)
        .collect();
    assert_eq!(blobs.len(), 1, "the CoW blob replaced its predecessor");
    assert_ne!(
        blobs[0].block_idx, old_blob,
        "DUR-6: every indirect publish allocates a FRESH blob"
    );
    assert!(rig.drift().await.is_empty(), "durable == derived");
    rig.shutdown().await;
}

/// **Stamping an EXISTING, non-empty volume must be safe.**
///
/// The hazard this pins (found while writing the design record, and the
/// reason `recover_durable_block_refs` refuses to trust an empty ledger):
/// a volume the Phase-8 window stamps has layouts that reference blocks
/// and a ledger with nothing in it. A mount that treated that ledger as
/// authoritative would read every live block as FREE and hand it straight
/// to the next writer — one device offset, two owners, which is the exact
/// failure class the whole structure exists to prevent.
///
/// Contract: the empty ledger DECLINES (so the caller walks), the backfill
/// persists what the walk found, and the mount after that seeds from
/// records alone and still agrees with the derived answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stamping_a_non_empty_volume_backfills_instead_of_freeing_live_blocks() {
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    // Un-stamp, so the data below is written with NO accounting at all —
    // exactly the pre-item-1 on-disk state the reformat window meets.
    let VolumeFormat::V3(mut sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    sb.features_incompat &= !FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS;
    write_superblock_v3(meta.path(), &sb).await.unwrap();

    let data = data_file();
    let live = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("legacy").await;
        let mut live = Vec::new();
        for b in 0..3u32 {
            live.push(rig.publish_block(ino, b).await);
        }
        assert_eq!(
            rig.durable().await.len(),
            0,
            "an un-stamped volume records nothing (that IS the legacy state)"
        );
        rig.shutdown().await;
        live
    };

    // The Phase-8 stamp.
    assert!(set_block_refcounts_bit(meta.path()).await.unwrap());

    // First mount after the stamp: the ledger is engaged but EMPTY.
    let rig = mount(meta.path(), data.path()).await;
    assert!(rig.routed.volumes[0].block_refs_engaged());
    assert!(
        rig.router
            .backend_router
            .recover_durable_block_refs(&rig.routed)
            .await
            .unwrap()
            .is_none(),
        "an EMPTY ledger must never be treated as authoritative — trusting it here \
         would read every live block as free"
    );
    // The caller's fallback: the derived walk, then the backfill.
    rig.alloc
        .recover_active_blocks_v3(&rig.routed.volumes[0], &rig.router.backend_router)
        .await
        .expect("derived walk");
    let written = rig
        .router
        .backend_router
        .backfill_durable_block_refs(&rig.routed)
        .await
        .expect("backfill");
    assert_eq!(written, 3, "one record per live reference");
    assert!(rig.drift().await.is_empty(), "the backfill is exact");
    rig.shutdown().await;

    // Every later mount seeds from records alone — and still must not hand
    // out a live offset.
    let rig = mount(meta.path(), data.path()).await;
    let seeded = rig
        .router
        .backend_router
        .recover_durable_block_refs(&rig.routed)
        .await
        .unwrap()
        .expect("the backfilled ledger IS authoritative");
    assert_eq!(seeded, 3);
    for off in &live {
        assert_eq!(
            rig.alloc.refcount(*off),
            Some(1),
            "block {off} must be tracked after a backfilled-ledger mount"
        );
    }
    let fresh = rig.alloc.allocate_block().await.expect("fresh");
    assert!(
        !live.contains(&fresh),
        "allocator handed out live block {fresh} after the stamp — the hazard"
    );
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// 9. The oracle inside fsck — the permanent C8 pin.
// ---------------------------------------------------------------------------

/// **fsck class C8 must report ZERO findings on a healthy stamped volume.**
///
/// This is the pin for the *deferred-accounting* class, and it lives here
/// rather than in `fsck_tests` because a fresh format no longer carries bit
/// 8 (ruling D9), so those fixtures' volumes are not engaged and their C8
/// arm never runs.
///
/// The class it pins, which the oracle caught: a site that mutates the RAM
/// block map and leaves the layout DIRTY defers its accounting to whichever
/// save persists the map — and that save is handed a map which already
/// contains the change, so it stages nothing. The rewrite-shadow ACK path
/// does exactly that. The fix is structural (a per-ino deferred-op
/// accumulator drained by the persisting save), so it covers every future
/// deferring site too; this leg is what keeps it honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsck_reports_no_durable_reference_drift_on_a_healthy_volume() {
    use squeezefs::fsck::{FsckCtx, FsckOptions};

    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert!(rig.routed.volumes[0].block_refs_engaged());

    // A population that touches the accounting from several directions:
    // fresh publishes, a displacement, a clone's sharing, a truncate and a
    // punch — the shapes whose sites stage at different places.
    let a = rig.mk_file("fsck_a").await;
    let b = rig.mk_file("fsck_b").await;
    for i in 0..4u32 {
        rig.publish_block(a, i).await;
    }
    let replacement = rig.alloc.allocate_block().await.unwrap();
    rig.alloc.publish_block(replacement);
    rig.router
        .merge_block_mappings(
            a,
            BlockMapOp::Merge(&[(1, replacement.to_string())]),
            4 * 4 * 1024 * 1024,
            LayoutFlip::KeepLayout,
            rig.token(a),
        )
        .await
        .expect("displacement");
    rig.router
        .clone_file(
            &squeezefs::keys::inode_path(a),
            &squeezefs::keys::inode_path(b),
            Some(rig.token(a)),
            Some(rig.token(b)),
        )
        .await
        .expect("clone");
    rig.router
        .punch_striped_blocks(b, &[2], 4 * 4 * 1024 * 1024, rig.token(b))
        .await
        .expect("punch");
    rig.router
        .truncate_layout(a, 2 * 4 * 1024 * 1024, rig.token(a))
        .await
        .expect("truncate");

    let report = squeezefs::fsck::run(
        &FsckCtx {
            meta: rig.routed.clone(),
            router: rig.router.clone(),
            staging_dirs: Vec::new(),
            expected_generation: None,
        },
        &FsckOptions {
            settle: std::time::Duration::from_millis(0),
            ..FsckOptions::online()
        },
    )
    .await
    .expect("fsck run");

    let c8: Vec<_> = report.findings.iter().filter(|f| f.class == "C8").collect();
    assert!(
        c8.is_empty(),
        "fsck C8 (durable-vs-derived block-reference drift) must be EMPTY on a \
         healthy volume — the ledger and the layouts that justify it diverged: {c8:?}"
    );
    // And the process tripwire agrees (the class increments it).
    assert!(
        rig.drift().await.is_empty(),
        "the comparison itself must be exact"
    );
    rig.shutdown().await;
}
