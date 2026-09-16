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
        let block = key.trim_start_matches("bk").split(':').next()?.parse::<u64>().ok()?;
        Some(reference(TAG, block, ino, idx))
    }));
    let head: std::collections::HashMap<u32, String> = [
        (0u32, "7:0:4096".to_string()),
        (1u32, "8:0:4096".to_string()),
    ]
    .into_iter()
    .collect();
    // Entry 0 re-described (the clip); entry 1 moved to another block.
    let entries = vec![(0u32, "7:0:2048".to_string()), (1u32, "9:0:4096".to_string())];
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
