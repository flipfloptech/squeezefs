//! Symmetric metadata program, PR 11 — **the real bit-17 stamp**:
//! `squeezefs format --symmetric` and the OFFLINE conversion verb
//! `squeezefs volume enable-symmetric` (`docs/design-symmetric-metadata.md`
//! §6.2, §7.1–§7.3, §5.2.1/§5.2.2, §5.1.8, KD-SYM-12).
//!
//! This suite is **layout-blind by construction**: its stamping is the
//! VERB — a PRE-FLIP multi-writer-class volume (nine bits, no forest —
//! built through `format_v3_stamped_multi_writer_flat`, the class every
//! field volume formatted between the rung-10b flip and PR 14 carries)
//! goes in, a forest comes out. Since PR 14 the default `format` IS the
//! forest, so the suite's source volumes are the one class the verb
//! exists for. It rides `tests/run_sym_forest_suites.sh`'s list so both
//! legs run it.
//!
//! Contracts pinned (the PR-11 row of the design's PR plan):
//! - a converted volume's post-fold digest EQUALS the source's, and every
//!   inode / dentry / xattr / block-reference record reads back through
//!   the PR-3 forest mount; the converted volume mounts with
//!   `dlm_rpcs == 0`, survives a create/unlink/rename storm + remount,
//!   and `fsck` is clean;
//! - a CRASHED conversion refuses every writable mount until resumed —
//!   at each of the five windows (after the markers, after the forest
//!   build, after the hybrid ledger, after the stamp, after the old
//!   trees' free) — while readers keep serving the digest; the resume
//!   completes and the digest still equals; a crash mid-build leaks no
//!   extent (the resume reclaims the orphaned build);
//! - refusals: already symmetric, a bit-8 NON-SOLO partition record, an
//!   open cross-volume intent, an in-flight `job:` record, a live client,
//!   a marker without `--resume`, `--resume` with nothing to resume, and
//!   `--symmetric --single-writer` at the CLI;
//! - the default `format` builds the forest (PR 14; the flip's own pins
//!   are `sym_default_flip_tests`); the FLAT images of the fixed
//!   description digest to their pre-PR-11 goldens;
//! - `--dry-run` writes nothing (sector 0 + the ledger extent unchanged);
//! - a 4-volume set converts every volume in one invocation, and a
//!   half-converted set refuses writable mounts naming the volume.

use squeezefs::config_ops::{
    enable_symmetric, enable_symmetric_with, ConversionOutcome, EnableSymCrash, EnableSymHooks,
    EnableSymOptions, SymUpgradeMarker,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{
    digest_backend, digest_backend_kind_set, format_v3_stamped,
    format_v3_stamped_multi_writer_flat, BuilderConfig, FormatV3Options, ImageBuilder, ROOT_INO,
};
use squeezefs::meta_backend::kv::checkpoint::{read_newest_ledger, write_ledger_slot};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::node::residue_seq_ceiling;
use squeezefs::meta_backend::kv::record::{
    guest_forest_slot, xattr_key, XattrValue, HASH56_MAX, KIND_INTERIOR, TREE_BLOCK_MAP,
    TREE_BLOCK_REFS, TREE_CONTROL, TREE_XATTRS,
};
use squeezefs::meta_backend::kv::slot_state::{
    decode_slot_state_key, slot_state_key_range, SlotState,
};
use squeezefs::meta_backend::kv::superblock::{
    backup_offset, classify_volume, read_backup_superblock, sector_generation, ExtentRef,
    SuperblockV3, VolumeFormat, FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, SUPERBLOCK_V3_LEN,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use squeezefs::SYM_UPGRADE_MARKER_XATTR;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Small volumes: 64 KiB nodes, a 1 MiB ring (the kv_backend_tests shape).
const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const TEST_SEED: u64 = 0x5EED_C0DE_0000_0011;
const TEST_UUID: [u8; 16] = *b"sym-convert-test";

/// The golden-digest contract serializes its builds (the builder's
/// determinism holds per description; two builds racing on one
/// description is a harness shape, not a product one).
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// The volume-set format config the first member records (what the
/// offline fsck harness reads back to build its router), naming one
/// file-backed data volume under `dir`.
fn format_config_for(dir: &Path) -> Vec<u8> {
    let oss = dir.join("oss0");
    std::fs::File::create(&oss)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let cfg = squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        block_size: 4096,
        capacity: 1 << 30,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(vec![oss.display().to_string()]),
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    };
    serde_json::to_vec(&cfg).unwrap()
}

/// A WRITABLE open of the pre-flip class — what the pre-PR-14 binary did
/// to every field volume the verb converts; since the flip the mount's
/// door refuses it (presence-required), so the conversion's own
/// process-scoped admission (`admit_pre_flip_writers` — the guard the
/// verb holds around its quiesce) stands in for that binary around
/// exactly these opens.
async fn open_pre_flip_writer(uris: &[String]) -> squeezefs::error::Result<Arc<RoutedMetaBackend>> {
    let _admit = squeezefs::meta_backend::kv::backend::admit_pre_flip_writers();
    open_routed_meta_set(uris).await
}

/// [`open_pre_flip_writer`]'s single-volume form (the plain writer door).
async fn open_pre_flip_backend(
    path: &str,
) -> Result<Arc<KvMetaBackend>, squeezefs::meta_backend::kv::KvError> {
    let _admit = squeezefs::meta_backend::kv::backend::admit_pre_flip_writers();
    KvMetaBackend::open(Path::new(path)).await
}

/// Format an `n`-member derived-width set of the PRE-FLIP multi-writer-
/// capable, bit-17-absent class — the shape every field volume formatted
/// before PR 14 has: this suite's source volumes are FLAT whatever leg of
/// the matrix runs it — the verb is what stamps.
async fn format_flat_set(dir: &Path, n: usize) -> Vec<String> {
    format_flat_set_sized(dir, n, VOL_LEN).await
}

/// [`format_flat_set`] on `vol_len`-byte volumes (the capacity contracts
/// use a small heap).
async fn format_flat_set_sized(dir: &Path, n: usize, vol_len: u64) -> Vec<String> {
    let plan = plan_meta_slot_set(n).expect("derived plan");
    let mut uris = Vec::with_capacity(n);
    for i in 0..n {
        let p = dir.join(format!("meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(vol_len).unwrap();
        let opts = FormatV3Options {
            format_config_xattr: (i == 0).then(|| format_config_for(dir)),
            ..set_opts()
        };
        // The PRE-FLIP multi-writer class (nine bits, no forest) — what the
        // verb converts; since PR 14 the default `format` builds the forest
        // and this class is built by no CLI arm.
        format_v3_stamped_multi_writer_flat(&p, vol_len, &opts, plan.stamps[i].clone())
            .await
            .expect("format flat member");
        uris.push(p.display().to_string());
    }
    uris
}

async fn superblock_of(path: &str) -> SuperblockV3 {
    match classify_volume(Path::new(path)).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("expected a v3 superblock, got {other:?}"),
    }
}

fn is_symmetric(sb: &SuperblockV3) -> bool {
    sb.features_incompat & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST != 0
}

/// The post-fold digest of one volume through a read-only probe.
async fn digest_of(path: &str) -> u64 {
    let be = KvMetaBackend::open_probe(Path::new(path))
        .await
        .expect("probe open");
    digest_backend(&be).await.expect("digest")
}

/// The order-independent oracles for the two kinds the ordered digest
/// does not walk — `(block references, block map)` as `(count, sum)` —
/// through a read-only probe.
async fn kind_sets_of(path: &str) -> ((u64, u64), (u64, u64)) {
    let be = KvMetaBackend::open_probe(Path::new(path))
        .await
        .expect("probe open");
    (
        digest_backend_kind_set(&be, TREE_BLOCK_REFS)
            .await
            .expect("refs set"),
        digest_backend_kind_set(&be, TREE_BLOCK_MAP)
            .await
            .expect("map set"),
    )
}

/// The bitmap-vs-reachability census of one quiesced volume through a
/// probe: `(claimed extents, extents some tree root reaches)`.
async fn extent_census(path: &str) -> (u64, u64) {
    let be = KvMetaBackend::open_probe(Path::new(path))
        .await
        .expect("probe open");
    // The census is exact only over a window the probe's replay folds
    // whole: a flat volume's clean unmount leaves it EMPTY; a forest's
    // armed leave (PR 14's default) leaves exactly its one control entry
    // — the region's `Unleased` release batch, tree-0 records the probe
    // replays before the walk (§5.1.3; no alloc delta rides it).
    assert_eq!(
        be.replay_stats().entries,
        u64::from(be.symmetric_forest()),
        "the window holds nothing but a forest leave's release entry"
    );
    assert_eq!(be.replay_stats().dropped_torn, 0);
    let sb = be.superblock().clone();
    let mut reachable = std::collections::BTreeSet::new();
    for tree in be.all_trees() {
        for addr in tree.reachable_node_addrs().await.expect("walk") {
            reachable.insert((addr - sb.heap.start) / u64::from(sb.node_size));
        }
    }
    let claimed = (0..sb.total_extents())
        .filter(|e| be.allocator().is_allocated(*e))
        .count() as u64;
    (claimed, reachable.len() as u64)
}

/// Whether the volume carries the `sym_upgrade:` marker (probe-read).
async fn marker_present(path: &str) -> bool {
    let be = KvMetaBackend::open_probe(Path::new(path))
        .await
        .expect("probe open");
    be.getxattr(ROOT_INO, SYM_UPGRADE_MARKER_XATTR)
        .await
        .expect("getxattr")
        .is_some()
}

/// What the population looks like after [`churn`]: every path's ino,
/// every ino's xattrs, and the block references published.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Population {
    inos: BTreeMap<String, u64>,
    xattrs: BTreeMap<(u64, String), Vec<u8>>,
    dirs: BTreeMap<u64, Vec<String>>,
    refs: Vec<(u64, u64, u64)>, // (vol_tag, block_idx, expected count)
}

/// Populate a set through the routed backend: directories, files, small
/// and large xattrs, a hard link, a rename, unlinks, and two files whose
/// layouts carry durable block references (kind 6) — every record kind
/// the conversion moves.
async fn churn(routed: &RoutedMetaBackend, files: u32) -> Population {
    let mut pop = Population::default();
    let docs = routed
        .create(ROOT_INO, "docs", libc::S_IFDIR | 0o755, 1000, 1000)
        .await
        .expect("mkdir docs")
        .ino;
    pop.inos.insert("/docs".into(), docs);
    let work = routed
        .create(ROOT_INO, "work", libc::S_IFDIR | 0o750, 0, 0)
        .await
        .expect("mkdir work")
        .ino;
    pop.inos.insert("/work".into(), work);
    let mut work_names = Vec::new();
    for i in 0..files {
        let f = routed
            .create(work, &format!("f{i:05}"), libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .unwrap_or_else(|e| panic!("create f{i}: {e}"))
            .ino;
        routed
            .setxattr(f, "user.tag", format!("t{i}").as_bytes())
            .await
            .expect("setxattr");
        pop.xattrs
            .insert((f, "user.tag".into()), format!("t{i}").into_bytes());
        if i % 7 == 0 {
            let big = vec![0xAB ^ (i as u8); 6000];
            routed
                .setxattr(f, "user.big", &big)
                .await
                .expect("setxattr big");
            pop.xattrs.insert((f, "user.big".into()), big);
        }
        if i % 5 == 4 {
            // Unlink the previous file (its xattrs go with it).
            let gone = routed
                .unlink(work, &format!("f{:05}", i - 1))
                .await
                .expect("unlink");
            pop.xattrs.retain(|(ino, _), _| *ino != gone);
            work_names.retain(|n: &String| n != &format!("f{:05}", i - 1));
        }
        if i % 5 != 3 {
            // (files unlinked at the next step are never recorded)
            pop.inos.insert(format!("/work/f{i:05}"), f);
            work_names.push(format!("f{i:05}"));
        }
    }
    // A hard link and a rename across directories.
    let first = *pop
        .inos
        .iter()
        .find(|(k, _)| k.starts_with("/work/f"))
        .map(|(_, v)| v)
        .expect("a surviving file");
    routed.link(first, docs, "hard.lnk").await.expect("link");
    pop.inos.insert("/docs/hard.lnk".into(), first);
    let moved = routed
        .create(docs, "moving.txt", libc::S_IFREG | 0o600, 0, 0)
        .await
        .expect("create moving")
        .ino;
    routed
        .rename(docs, "moving.txt", work, "moved.txt", 0)
        .await
        .expect("rename");
    pop.inos.insert("/work/moved.txt".into(), moved);
    work_names.push("moved.txt".into());
    // Two files publish layouts with durable block references (bit 9 is
    // stamped by the default format): one shared block, one private.
    let tag = volume_tag("vol-0011223344556677");
    let owners: Vec<u64> = (0..2)
        .map(|i| pop.inos[&format!("/work/f{:05}", 5 * i)])
        .collect();
    for (i, owner) in owners.iter().enumerate() {
        let (vol_idx, local) = routed.route_ino(*owner);
        let vol = &routed.volumes[vol_idx];
        let mut ops = vec![BlockRefOp::taken(BlockRef {
            vol_tag: tag,
            block_idx: 7,
            owner_ino: local,
            block_index: 0,
        })];
        if i == 1 {
            ops.push(BlockRefOp::taken(BlockRef {
                vol_tag: tag,
                block_idx: 8,
                owner_ino: local,
                block_index: 1,
            }));
        }
        vol.set_layout_and_size(local, b"layout", 4096 * (i as u64 + 1), &ops)
            .await
            .expect("publish a layout with block references");
    }
    pop.refs.push((tag, 7, 2));
    pop.refs.push((tag, 8, 1));
    pop.refs.push((tag, 9, 0));
    work_names.sort();
    pop.dirs.insert(work, work_names);
    pop.dirs.insert(docs, vec!["hard.lnk".into()]);
    pop
}

/// Every record of `pop` reads back through `routed` exactly.
async fn assert_population(routed: &RoutedMetaBackend, pop: &Population) {
    for (path, ino) in &pop.inos {
        let mut parent = ROOT_INO;
        let mut resolved = ROOT_INO;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            resolved = routed
                .lookup(parent, comp)
                .await
                .unwrap_or_else(|e| panic!("lookup {path}: {e}"))
                .ino;
            parent = resolved;
        }
        assert_eq!(resolved, *ino, "{path} resolves to its ino");
        routed.getattr(*ino).await.expect("getattr");
    }
    for ((ino, name), value) in &pop.xattrs {
        assert_eq!(
            routed
                .getxattr(*ino, name)
                .await
                .expect("getxattr")
                .as_deref(),
            Some(value.as_slice()),
            "xattr {name} of ino {ino}"
        );
    }
    for (dir, names) in &pop.dirs {
        let mut got: Vec<String> = routed
            .readdir(*dir, 0, usize::MAX)
            .await
            .expect("readdir")
            .into_iter()
            .map(|e| e.name)
            .filter(|n| n != "." && n != "..")
            .collect();
        got.sort();
        assert_eq!(&got, names, "directory {dir} lists its names");
    }
    for (tag, blk, want) in &pop.refs {
        let mut count = 0u64;
        for vol in &routed.volumes {
            count += vol.block_ref_count(*tag, *blk).await.expect("refcount") as u64;
        }
        assert_eq!(count, *want, "refcount(block {blk}) over the set");
    }
}

/// A flat set of `n` volumes populated with `files` files, cleanly shut
/// down: the per-volume digests, the population, the URIs.
async fn populated_flat_set(
    dir: &Path,
    n: usize,
    files: u32,
) -> (Vec<String>, Vec<u64>, Population) {
    let uris = format_flat_set(dir, n).await;
    let routed = open_pre_flip_writer(&uris).await.expect("open flat set");
    let pop = churn(&routed, files).await;
    let mut digests = Vec::with_capacity(n);
    for vol in &routed.volumes {
        digests.push(digest_backend(vol).await.expect("digest"));
    }
    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }
    drop(routed);
    (uris, digests, pop)
}

/// The writable set refuses with the sym-upgrade marker's message.
async fn assert_writable_refuses(uris: &[String]) -> String {
    match open_pre_flip_writer(uris).await {
        Ok(routed) => {
            for v in &routed.volumes {
                let _ = v.shutdown().await;
            }
            panic!("a writable mount must refuse while the sym_upgrade marker is present");
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("enable-symmetric") && msg.contains("--resume"),
                "the refusal names the resume: {msg}"
            );
            msg
        }
    }
}

/// A storm of creates / unlinks / renames on the converted set.
async fn storm(routed: &RoutedMetaBackend, rounds: u32) {
    let d = routed
        .create(ROOT_INO, "storm", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir storm")
        .ino;
    for i in 0..rounds {
        routed
            .create(d, &format!("s{i:04}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
        if i % 3 == 2 {
            routed
                .rename(d, &format!("s{i:04}"), d, &format!("r{i:04}"), 0)
                .await
                .expect("rename");
        }
        if i % 4 == 3 {
            let prev = i - 1;
            let name = if prev % 3 == 2 {
                format!("r{prev:04}")
            } else {
                format!("s{prev:04}")
            };
            routed.unlink(d, &name).await.expect("unlink");
        }
    }
}

async fn fsck_clean(uris: &[String]) {
    let mut opts = squeezefs::fsck::FsckOptions::offline();
    opts.settle = std::time::Duration::from_millis(10);
    let report = squeezefs::fsck::run_offline(uris, &opts)
        .await
        .expect("offline fsck runs");
    assert!(
        !report.has_findings(),
        "fsck must be clean after the conversion: {:?}",
        report.findings
    );
}

// ---------------------------------------------------------------------------
// The conversion: digest equality + every record readable + the forest mount.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_converted_volumes_post_fold_digest_equals_the_sources() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 1, 200).await;
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));
    // The kinds the ordered digest does not walk: the references are
    // non-trivial here (three published), the block map whatever the
    // format engaged.
    let (refs_before, map_before) = kind_sets_of(&uris[0]).await;
    assert_eq!(
        refs_before.0, 3,
        "the source publishes three block references"
    );

    // The plan is the build's own claim: what `--dry-run` reports as
    // needed is exactly what the conversion writes (the tie contract of
    // the capacity preflight — the tree writer's chunking, not an
    // estimate), and the census credit reads 0 on a cleanly unmounted
    // volume.
    let plan = enable_symmetric(
        &uris,
        &EnableSymOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .expect("plan");
    assert_eq!(plan.rows[0].outcome, ConversionOutcome::Planned);
    assert!(
        plan.rows[0].extents_needed > 1,
        "the plan claims nodes + the directory"
    );
    assert!(
        plan.rows[0].extents_available >= plan.rows[0].extents_needed,
        "the populated 64 MiB volume holds its forest: needs {} of {}",
        plan.rows[0].extents_needed,
        plan.rows[0].extents_available
    );
    assert_eq!(plan.rows[0].orphans_reclaimed, 0);

    let report = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("enable-symmetric converts a flat set");
    assert_eq!(report.rows.len(), 1);
    println!("conversion row: {:?}", report.rows[0]);
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Converted);
    assert!(report.rows[0].records > 0, "records were moved");
    assert_eq!(
        report.rows[0].records, plan.rows[0].records,
        "the plan's record set is the build's (the marker planned in)"
    );
    assert_eq!(
        report.rows[0].extents_written, plan.rows[0].extents_needed,
        "the capacity preflight's peak claim IS the build's claim (tie)"
    );
    assert_eq!(report.rows[0].extents_needed, plan.rows[0].extents_needed);
    assert_eq!(report.rows[0].slot_trees, plan.rows[0].slot_trees);
    assert!(
        report.rows[0].slot_trees > 1,
        "a derived-width set's mints spread over guest slots: {} slot trees",
        report.rows[0].slot_trees
    );
    assert!(
        report.rows[0].extents_freed > 0,
        "the emptied shared trees returned their extents"
    );
    assert_eq!(
        kind_sets_of(&uris[0]).await,
        (refs_before, map_before),
        "the block-reference and block-map record SETS survive the relayout"
    );
    // The forest the conversion leaves is exactly accounted: every
    // claimed extent is a node some root reaches or the directory extent
    // (the same census the verb runs before it builds — see
    // `a_clean_flat_unmount_leaves_no_claimed_extent_its_roots_do_not_reach`).
    let (claimed, reachable) = extent_census(&uris[0]).await;
    assert_eq!(
        claimed,
        reachable + 1,
        "claimed extents = reachable nodes + the appender directory extent"
    );

    let sb = superblock_of(&uris[0]).await;
    assert!(is_symmetric(&sb), "bit 17 stamped");
    assert!(sb.appender_dir.len != 0, "the appender directory is named");
    assert!(!marker_present(&uris[0]).await, "the marker is gone");
    assert_eq!(
        digest_of(&uris[0]).await,
        digests[0],
        "the forest folds to the flat volume's digest"
    );

    let rpcs_before = squeezefs::dlm_slot::dlm_rpcs();
    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    let vol = &routed.volumes[0];
    assert!(
        vol.symmetric_forest(),
        "the converted volume mounts as a forest"
    );
    assert_eq!(digest_backend(vol).await.unwrap(), digests[0]);
    assert_population(&routed, &pop).await;
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        rpcs_before,
        "a solo forest mount pays no lock RPC"
    );
    let ledger = read_newest_ledger(Path::new(&uris[0]), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    let mut ids: Vec<u8> = ledger.tree_roots.iter().map(|r| r.tree_id).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![KIND_INTERIOR, TREE_CONTROL],
        "the emptied shared trees are folded out of the ledger"
    );

    storm(&routed, 120).await;
    let live = digest_backend(vol).await.unwrap();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let again = open_routed_meta_set(&uris).await.expect("remount");
    assert_eq!(digest_backend(&again.volumes[0]).await.unwrap(), live);
    assert_population(&again, &pop).await;
    for v in &again.volumes {
        v.shutdown().await.unwrap();
    }
    drop(again);
    fsck_clean(&uris).await;
}

/// **The clean-unmount law, and the shipped defect PR 11 found under it
/// (every flat volume): a clean unmount leaves NO claimed extent its
/// ledger roots do not reach.** Before the fix, the checkpoint cycle's
/// coverage barrier released the pending frees its tail covers
/// (`after_durable_barrier` → `advance_durable`) AFTER that cycle had
/// written its bitmap pages — dirtying them for the NEXT cycle — while
/// the shutdown fixpoint converged on ring coverage alone (`head ==
/// reusable_upto`), so the FINAL cycle's releases were never written:
/// the retired images read CLAIMED at the next mount, nothing in the
/// covered window freed them again, and every clean unmount whose last
/// pass retired images leaked them (the populated volume below leaked 6,
/// an idle mount/unmount pair 1 — proportional to the final pass's
/// SMOs). The fixpoint now also converges on "no dirty bitmap page"; the
/// conversion's pre-build census (`orphans_reclaimed`) is the instrument
/// that found the class and reads 0 on a volume this binary unmounted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_flat_unmount_leaves_no_claimed_extent_its_roots_do_not_reach() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 60).await;
    let (claimed0, reachable0) = extent_census(&uris[0]).await;
    assert_eq!(
        claimed0, reachable0,
        "the populated volume's clean unmount left every claimed extent reachable"
    );
    // Idle mount cycles (the pre-flip binary's, under the admission): no
    // records change, and the claimed count holds.
    for _ in 0..3 {
        let routed = open_pre_flip_writer(&uris).await.expect("mount");
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
    }
    let (claimed3, reachable3) = extent_census(&uris[0]).await;
    assert_eq!(reachable3, reachable0, "the trees hold the same population");
    assert_eq!(
        claimed3, claimed0,
        "idle mount cycles leak no claimed extent: {claimed0} → {claimed3}"
    );
    // The conversion's census finds nothing to reclaim on such a volume,
    // and leaves one whose claimed set is its reachable set plus the
    // directory extent.
    let report = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("convert");
    assert_eq!(
        report.rows[0].orphans_reclaimed, 0,
        "nothing leaked before the verb ran"
    );
    let (claimed, reachable) = extent_census(&uris[0]).await;
    assert_eq!(claimed, reachable + 1);
}

// ---------------------------------------------------------------------------
// The crash-window matrix.
// ---------------------------------------------------------------------------

/// Crash the conversion at `window`, prove the set refuses writers and
/// serves readers, resume, and prove the result equals the source.
/// Returns the converted volume's free-extent count (the leak pin).
async fn crash_then_resume(window: EnableSymCrash) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 1, 150).await;
    let kind_sets = kind_sets_of(&uris[0]).await;

    let hooks = EnableSymHooks {
        crash_after: Some(window),
    };
    let err = enable_symmetric_with(&uris, &EnableSymOptions::default(), &hooks)
        .await
        .expect_err("the injected crash aborts the verb");
    assert!(
        err.to_string().contains("crash injection"),
        "the abort is the seam's: {err}"
    );
    assert!(
        marker_present(&uris[0]).await,
        "the marker outlives every window before the last act ({window:?})"
    );
    let refusal = assert_writable_refuses(&uris).await;
    assert!(
        refusal.contains(&uris[0]),
        "the refusal names the volume: {refusal}"
    );
    // Readers keep serving on whichever layout is current. (The digest
    // is not compared here: the marker is itself an ino-1 xattr the walk
    // hashes — equality is asserted once the resume removes it.)
    let reader = open_routed_meta_set_read_only(&uris)
        .await
        .expect("a read-only mount proceeds under the marker");
    assert_population(&reader, &pop).await;
    drop(reader);

    // A plain re-run refuses: a crashed run must be acknowledged.
    let plain = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("a marker refuses a plain re-run");
    assert!(plain.to_string().contains("--resume"), "{plain}");

    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            resume: true,
            ..Default::default()
        },
    )
    .await
    .expect("the resume completes the conversion");
    println!("resume row after {window:?}: {:?}", report.rows[0]);
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Resumed);
    if matches!(
        window,
        EnableSymCrash::AfterBuild { .. } | EnableSymCrash::AfterLedger { .. }
    ) {
        assert!(
            report.rows[0].orphans_reclaimed > 0,
            "the resume reclaims the crashed build's extents before rebuilding"
        );
    }
    assert!(is_symmetric(&superblock_of(&uris[0]).await));
    assert!(!marker_present(&uris[0]).await);
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
    assert_eq!(kind_sets_of(&uris[0]).await, kind_sets);

    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    assert!(routed.volumes[0].symmetric_forest());
    assert_population(&routed, &pop).await;
    let free = routed.volumes[0].allocator().free_extents();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    fsck_clean(&uris).await;
    free
}

/// The uninterrupted conversion of an identical set — the leak pin's
/// reference.
async fn straight_conversion_free_extents() -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _digests, _pop) = populated_flat_set(dir.path(), 1, 150).await;
    enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("convert");
    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    let free = routed.volumes[0].allocator().free_extents();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    free
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_the_marker_refuses_writers_until_resumed() {
    crash_then_resume(EnableSymCrash::AfterMarker).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_mid_pass_refuses_writers_until_resumed_and_leaks_no_extent() {
    let resumed = crash_then_resume(EnableSymCrash::AfterBuild { volume: 0 }).await;
    let straight = straight_conversion_free_extents().await;
    assert_eq!(
        resumed, straight,
        "the resume reclaims the orphaned build: same free extents as a straight conversion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_the_hybrid_ledger_refuses_writers_until_resumed() {
    crash_then_resume(EnableSymCrash::AfterLedger { volume: 0 }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_the_stamp_before_the_marker_removal_refuses_writers_until_resumed() {
    let resumed = crash_then_resume(EnableSymCrash::AfterStamp { volume: 0 }).await;
    let straight = straight_conversion_free_extents().await;
    assert_eq!(
        resumed, straight,
        "the old trees are freed by the resume: same free extents as a straight conversion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_the_old_trees_free_refuses_writers_until_resumed() {
    crash_then_resume(EnableSymCrash::AfterFree { volume: 0 }).await;
}

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_already_symmetric_set_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).unwrap();
    format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("the default (forest) format");
    let uris = vec![p.display().to_string()];
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("already symmetric"), "{err}");
}

/// A bit-8 NON-SOLO partition record (the shape a multi-appender era
/// leaves behind): a dry run names the quiesce and writes nothing; the
/// real run's quiesce IS the solo mount the pre-flip remedy named (PR
/// 14) — its checkpoint writes the solo-form record — and the verb
/// converts (Issue 12's law, kept: `…_and_the_verb_converts` below).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bit_8_non_solo_partition_record_is_quiesced_solo_by_the_verb() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 20).await;
    let sb = superblock_of(&uris[0]).await;
    // A 2-way partitioned record from writer 0, newer than the solo one:
    // the shape a multi-appender era leaves behind.
    let mut rec = read_newest_ledger(Path::new(&uris[0]), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    rec.seq += 1;
    rec.append_partition = Some(AppendPartition::new(2, 0).expect("legal partition"));
    write_ledger_slot(Path::new(&uris[0]), sb.root_ledger.start, &rec)
        .await
        .expect("write");
    squeezefs::uring_fs::fdatasync(Path::new(&uris[0]))
        .await
        .unwrap();
    let before = fixed_region_of(&uris[0]).await;
    let err = enable_symmetric(
        &uris,
        &EnableSymOptions {
            dry_run: true,
            ..EnableSymOptions::default()
        },
    )
    .await
    .expect_err("a dry run cannot plan against a non-solo record");
    let msg = err.to_string();
    assert!(
        msg.contains("non-solo partition record") && msg.contains("QUIESCES"),
        "names the bit-8 record and the quiesce: {msg}"
    );
    assert!(
        before == fixed_region_of(&uris[0]).await,
        "a dry run writes nothing"
    );
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_cross_volume_intent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 20).await;
    // Plant an intent record on the reserved intent ino (what a crashed
    // cross-volume transaction leaves behind), through the guarded open.
    let be = open_pre_flip_backend(&uris[0]).await.expect("open");
    let key = xattr_key(
        squeezefs::meta_backend::crossvol_tx::XV_INTENT_INO,
        0x1234 & HASH56_MAX,
        0,
    );
    let value = XattrValue {
        name: b"xv".to_vec(),
        value: b"intent".to_vec(),
    }
    .encode()
    .unwrap();
    be.insert_kind(TREE_XATTRS, &key, value)
        .await
        .expect("plant");
    be.checkpoint_now().await.unwrap();
    be.shutdown().await.unwrap();
    drop(be);
    // The verb's quiesce (PR 14) runs the routed open, whose bring-up
    // rolls open intents forward — an unreadable one refuses that open,
    // and the verb refuses with it: nothing converted, no marker.
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("cross-volume intent"), "{err}");
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));
    assert!(!marker_present(&uris[0]).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_flight_job_record_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 20).await;
    let be = open_pre_flip_backend(&uris[0]).await.expect("open");
    let (name, bytes) = squeezefs::jobs::durable_queued_job_xattr(
        &squeezefs::jobs::JobType::Noop {
            tasks: 4,
            task_ms: 1,
        },
        50,
    );
    be.setxattr_internal(ROOT_INO, &name, &bytes)
        .await
        .expect("plant a queued job");
    be.checkpoint_now().await.unwrap();
    be.shutdown().await.unwrap();
    drop(be);
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("job"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_client_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 20).await;
    let live = open_pre_flip_writer(&uris).await.expect("live writer");
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused under a live client");
    assert!(err.to_string().contains("mounted"), "{err}");
    for v in &live.volumes {
        v.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_with_nothing_to_resume_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 10).await;
    let err = enable_symmetric(
        &uris,
        &EnableSymOptions {
            resume: true,
            ..Default::default()
        },
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("nothing to resume"), "{err}");
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));
}

#[test]
fn the_cli_refuses_symmetric_together_with_single_writer() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_squeezefs"))
        .args([
            "format",
            "--symmetric",
            "--single-writer",
            "--meta-lv",
            "/dev/null",
            "--data-lv",
            "/dev/null",
            "sqmeta:///dev/null",
        ])
        .output()
        .expect("run the binary");
    assert!(!out.status.success(), "the contradictory pair is refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--symmetric") && stderr.contains("--single-writer"),
        "names both flags: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// The marker record.
// ---------------------------------------------------------------------------

#[test]
fn the_marker_round_trips_and_refuses_a_torn_image() {
    let m = SymUpgradeMarker {
        volumes: vec!["/dev/a".into(), "/dev/b".into()],
    };
    let img = m.encode().expect("two short paths fit the u16 fields");
    assert_eq!(SymUpgradeMarker::decode(&img).unwrap(), m);
    let mut torn = img.clone();
    torn[3] ^= 0xFF;
    assert!(SymUpgradeMarker::decode(&torn).is_err());
    assert!(SymUpgradeMarker::decode(&img[..img.len() - 1]).is_err());
    assert!(SymUpgradeMarker::decode(&[]).is_err());
}

// ---------------------------------------------------------------------------
// `format --symmetric` and the untouched default.
// ---------------------------------------------------------------------------

/// The fixed image description every byte-identity contract builds
/// (`TEST_UUID` / `TEST_SEED`: the builder is deterministic over them).
fn describe() -> ImageBuilder {
    let cfg = BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    };
    let mut b = ImageBuilder::new(cfg).unwrap();
    let docs = b.add_dir(ROOT_INO, "docs", 0o750, 1000, 1000).unwrap();
    let readme = b
        .add_file(docs, "readme.txt", 0o644, 1000, 1000, 4096)
        .unwrap();
    b.set_xattr(readme, "user.color", b"blue").unwrap();
    b.add_link(readme, ROOT_INO, "hard.lnk").unwrap();
    b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_public_symmetric_formatter_mounts_as_a_forest() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).unwrap();
    // The DEFAULT class since PR 14 (`--symmetric` is its no-op spelling).
    format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("the default format");
    let uris = vec![p.display().to_string()];
    let sb = superblock_of(&uris[0]).await;
    assert!(is_symmetric(&sb));
    assert!(
        sb.features_incompat & squeezefs::meta_backend::kv::superblock::MULTI_WRITER_FORMAT_BITS
            == squeezefs::meta_backend::kv::superblock::MULTI_WRITER_FORMAT_BITS,
        "--symmetric is the multi-writer-capable class plus bit 17"
    );
    let routed = open_routed_meta_set(&uris).await.expect("mount");
    assert!(routed.volumes[0].symmetric_forest());
    let pop = churn(&routed, 40).await;
    assert_population(&routed, &pop).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_format_stamps_no_bit_and_names_no_directory() {
    let dir = tempfile::tempdir().unwrap();
    let uris = format_flat_set(dir.path(), 1).await;
    let sb = superblock_of(&uris[0]).await;
    assert!(!is_symmetric(&sb), "a default format never carries bit 17");
    assert_eq!(sb.appender_dir.len, 0);
    assert_eq!(sb.appender_dir.start, 0);
    let ledger = read_newest_ledger(Path::new(&uris[0]), sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    assert!(
        ledger
            .tree_roots
            .iter()
            .all(|r| r.tree_id != TREE_CONTROL && r.tree_id != KIND_INTERIOR),
        "a default format names no forest root"
    );
}

// ---------------------------------------------------------------------------
// `--dry-run`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, _pop) = populated_flat_set(dir.path(), 1, 30).await;
    let sb = superblock_of(&uris[0]).await;
    let fixed_len = (sb.heap.start) as usize; // superblock + ledger + ring + bitmap
    let before = squeezefs::uring_fs::read_at(Path::new(&uris[0]), 0, fixed_len)
        .await
        .unwrap();
    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .expect("a dry run plans");
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Planned);
    assert!(report.rows[0].records > 0, "the plan counts the records");
    let after = squeezefs::uring_fs::read_at(Path::new(&uris[0]), 0, fixed_len)
        .await
        .unwrap();
    assert!(
        before == after,
        "sector 0, the ledger, the ring and the bitmap are byte-identical after a dry run"
    );
    assert!(!marker_present(&uris[0]).await);
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
}

// ---------------------------------------------------------------------------
// Sets: every volume in one invocation; a half-converted set refuses.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_four_volume_set_converts_every_volume_in_one_invocation() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 4, 160).await;
    let report = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("convert the set");
    assert_eq!(report.rows.len(), 4);
    for (i, uri) in uris.iter().enumerate() {
        assert!(
            is_symmetric(&superblock_of(uri).await),
            "volume {i} stamped"
        );
        assert!(!marker_present(uri).await, "volume {i} marker gone");
        assert_eq!(
            report.rows[i].outcome,
            ConversionOutcome::Converted,
            "volume {i}"
        );
    }
    let routed = open_routed_meta_set(&uris).await.expect("forest set");
    for (i, vol) in routed.volumes.iter().enumerate() {
        assert!(vol.symmetric_forest());
        assert_eq!(
            digest_backend(vol).await.unwrap(),
            digests[i],
            "volume {i} digest"
        );
    }
    assert_population(&routed, &pop).await;
    storm(&routed, 64).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    fsck_clean(&uris).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_half_converted_set_refuses_writable_mounts_naming_the_volume() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 4, 80).await;
    let hooks = EnableSymHooks {
        crash_after: Some(EnableSymCrash::AfterStamp { volume: 1 }),
    };
    enable_symmetric_with(&uris, &EnableSymOptions::default(), &hooks)
        .await
        .expect_err("the injected crash aborts the verb");
    // Volumes 0 and 1 are stamped (1 still under its marker); 2 and 3 are
    // flat under theirs.
    assert!(is_symmetric(&superblock_of(&uris[0]).await));
    assert!(is_symmetric(&superblock_of(&uris[1]).await));
    assert!(!is_symmetric(&superblock_of(&uris[2]).await));
    assert!(!marker_present(&uris[0]).await);
    assert!(marker_present(&uris[1]).await);
    assert!(marker_present(&uris[3]).await);
    let refusal = assert_writable_refuses(&uris).await;
    assert!(
        refusal.contains(&uris[1]),
        "the refusal names the first volume still under its marker: {refusal}"
    );
    let reader = open_routed_meta_set_read_only(&uris)
        .await
        .expect("readers proceed");
    // Volume 0 finished (marker gone): its digest is the source's; the
    // others still carry the marker xattr the walk hashes — the population
    // check is the layout-blind assertion for them.
    assert_eq!(
        digest_backend(&reader.volumes[0]).await.unwrap(),
        digests[0]
    );
    assert_population(&reader, &pop).await;
    drop(reader);

    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            resume: true,
            ..Default::default()
        },
    )
    .await
    .expect("resume");
    assert_eq!(report.rows[0].outcome, ConversionOutcome::AlreadySymmetric);
    assert_eq!(report.rows[1].outcome, ConversionOutcome::Resumed);
    assert_eq!(report.rows[2].outcome, ConversionOutcome::Resumed);
    let routed = open_routed_meta_set(&uris).await.expect("forest set");
    for (i, vol) in routed.volumes.iter().enumerate() {
        assert!(vol.symmetric_forest());
        assert_eq!(digest_backend(vol).await.unwrap(), digests[i]);
    }
    assert_population(&routed, &pop).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    fsck_clean(&uris).await;
}

/// The marker's own refusal is one check at the D0 gate, BEFORE the
/// claim: a refused writable open writes nothing — the ledger it found
/// is the ledger it leaves (what makes every intermediate state of the
/// conversion resumable).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_writable_open_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 30).await;
    let hooks = EnableSymHooks {
        crash_after: Some(EnableSymCrash::AfterLedger { volume: 0 }),
    };
    enable_symmetric_with(&uris, &EnableSymOptions::default(), &hooks)
        .await
        .expect_err("crash");
    let sb = superblock_of(&uris[0]).await;
    let fixed_len = sb.heap.start as usize;
    let before = squeezefs::uring_fs::read_at(Path::new(&uris[0]), 0, fixed_len)
        .await
        .unwrap();
    assert!(KvMetaBackend::open(Path::new(&uris[0])).await.is_err());
    let after = squeezefs::uring_fs::read_at(Path::new(&uris[0]), 0, fixed_len)
        .await
        .unwrap();
    assert!(
        before == after,
        "a refused writable open leaves the fixed structures byte-identical"
    );
    // The marker's value names the act.
    let be = KvMetaBackend::open_probe(Path::new(&uris[0]))
        .await
        .unwrap();
    let raw = be
        .getxattr(ROOT_INO, SYM_UPGRADE_MARKER_XATTR)
        .await
        .unwrap()
        .expect("marker");
    let m = SymUpgradeMarker::decode(&raw).expect("decodes");
    assert_eq!(m.volumes, uris);
}

// ---------------------------------------------------------------------------
// Round 2 — everything that can refuse runs BEFORE the first marker: the
// capacity preflight, the verb's D0 hold, `--abort`, the census-reclaim
// seq floor, the emptied slot's cursor in tree 0, the golden flat image.
// ---------------------------------------------------------------------------

/// A small heap for the capacity contracts: 16 MiB at 64 KiB nodes with a
/// 1 MiB ring ≈ 236 heap extents.
const SMALL_VOL_LEN: u64 = 16 * 1024 * 1024;

/// Grow the shared trees by whole extents fast: `count` files each
/// carrying one `value_len`-byte xattr (near the 64 KiB-node value cap, so
/// a leaf holds a handful).
async fn fill_with_large_xattrs(routed: &RoutedMetaBackend, count: u32, value_len: usize) {
    let d = routed
        .create(ROOT_INO, "bulk", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir bulk")
        .ino;
    for i in 0..count {
        let f = routed
            .create(d, &format!("b{i:05}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        let v = vec![(i % 251) as u8; value_len];
        routed.setxattr(f, "user.bulk", &v).await.expect("setxattr");
        if i % 50 == 49 {
            for vol in &routed.volumes {
                vol.checkpoint_now().await.expect("checkpoint");
            }
        }
    }
}

/// The fixed region `[0, heap.start)` — sector 0, the ledger, the ring,
/// the bitmap — of one volume.
async fn fixed_region_of(path: &str) -> Vec<u8> {
    let sb = superblock_of(path).await;
    squeezefs::uring_fs::read_at(Path::new(path), 0, sb.heap.start as usize)
        .await
        .unwrap()
        .to_vec()
}

/// **Issue 1 (the bug).** A volume that cannot hold the forest beside its
/// shared trees refuses BEFORE any marker — the capacity preflight is
/// computed from the write-free inspection — naming the volume, the
/// extents needed and available, and the remedy; the fixed region is
/// byte-identical, the digest unchanged, and a writer mounts it. Before
/// round 2 the same volume took its marker first and failed `NoSpace`
/// mid-build, with `--resume` failing identically and no abort: a set
/// only `format --force` could exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_capacity_preflight_refuses_a_volume_that_cannot_hold_the_forest_before_any_marker() {
    let dir = tempfile::tempdir().unwrap();
    let uris = format_flat_set_sized(dir.path(), 1, SMALL_VOL_LEN).await;
    let routed = open_pre_flip_writer(&uris).await.expect("open");
    let pop = churn(&routed, 20).await;
    fill_with_large_xattrs(&routed, 480, 15_000).await;
    let digest = digest_backend(&routed.volumes[0]).await.unwrap();
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let before = fixed_region_of(&uris[0]).await;

    // The dry run reports the shortfall instead of refusing — the
    // operator's instrument for the remedy.
    let plan = enable_symmetric(
        &uris,
        &EnableSymOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .expect("a dry run plans what it would refuse");
    let row = &plan.rows[0];
    println!("capacity plan: {row:?}");
    assert!(
        row.extents_needed > row.extents_available,
        "the small volume cannot hold the forest beside its trees: needs {} of {}",
        row.extents_needed,
        row.extents_available
    );

    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused at the capacity preflight");
    let msg = err.to_string();
    assert!(msg.contains("cannot hold the forest"), "{msg}");
    assert!(msg.contains(&uris[0]), "names the volume: {msg}");
    assert!(
        msg.contains(&format!("claims {} fresh extents", row.extents_needed))
            && msg.contains(&format!("only {} are claimable", row.extents_available)),
        "names the needed and the available extents: {msg}"
    );
    assert!(
        msg.contains("--meta-node-kib") && msg.contains("defrag --meta"),
        "names the remedy: {msg}"
    );

    assert!(
        before == fixed_region_of(&uris[0]).await,
        "refused BEFORE any write: sector 0, the ledger, the ring and the bitmap are \
         byte-identical"
    );
    assert!(!marker_present(&uris[0]).await, "no marker was written");
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));
    assert_eq!(
        digest_of(&uris[0]).await,
        digest,
        "the records are untouched"
    );
    let routed = open_pre_flip_writer(&uris)
        .await
        .expect("a writer mounts the refused volume");
    assert_population(&routed, &pop).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// **Issue 2.** The verb holds the D0 Layer-A flock on every volume for
/// its duration: a concurrent holder — another invocation, a mount that
/// began after the live-client gate — refuses the verb loud before any
/// write, naming the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_holder_of_the_writer_lock_refuses_the_verb_before_any_write() {
    use std::os::unix::io::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, _pop) = populated_flat_set(dir.path(), 1, 20).await;
    let before = fixed_region_of(&uris[0]).await;
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&uris[0])
        .unwrap();
    // SAFETY: valid owned fd; LOCK_NB never blocks.
    let rc = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "the test takes the device flock");
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused on the writer lock");
    let msg = err.to_string();
    assert!(
        msg.contains("writer lock") && msg.contains("concurrent"),
        "names the lock and the concurrent class: {msg}"
    );
    assert!(before == fixed_region_of(&uris[0]).await, "nothing written");
    assert!(!marker_present(&uris[0]).await);
    drop(holder);
    // Released, the same set converts.
    enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("converts once the lock is free");
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
}

/// **Issue 2, the race itself.** Two invocations started together on one
/// set: exactly one converts, the other refuses loud (on the lock, or —
/// if it took the lock inside the winner's own guarded-open window — on
/// the marker the winner had already written), and the set ends
/// symmetric with the source's digest, never double-built.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_invocations_never_double_build() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 1, 60).await;
    let opts = EnableSymOptions::default();
    let (a, b) = tokio::join!(
        enable_symmetric(&uris, &opts),
        enable_symmetric(&uris, &opts)
    );
    let (won, lost) = match (a, b) {
        (Ok(r), Err(e)) | (Err(e), Ok(r)) => (r, e),
        (Ok(_), Ok(_)) => panic!("both invocations converted — the lock did not exclude"),
        (Err(a), Err(b)) => panic!("neither invocation converted: {a} / {b}"),
    };
    assert_eq!(won.rows[0].outcome, ConversionOutcome::Converted);
    let msg = lost.to_string();
    assert!(
        msg.contains("writer lock") || msg.contains("--resume") || msg.contains("mounted"),
        "the loser refuses on the lock or the marker: {msg}"
    );
    assert!(is_symmetric(&superblock_of(&uris[0]).await));
    assert!(!marker_present(&uris[0]).await);
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    assert_population(&routed, &pop).await;
    let (claimed, reachable) = {
        let free_before = routed.volumes[0].allocator().free_extents();
        for v in &routed.volumes {
            v.shutdown().await.unwrap();
        }
        drop(routed);
        let _ = free_before;
        extent_census(&uris[0]).await
    };
    assert_eq!(claimed, reachable + 1, "one forest, exactly accounted");
    fsck_clean(&uris).await;
}

/// `--abort` after a crash at `window` (a FLAT volume under its marker):
/// the marker is removed, the aborted build's extents reclaimed, and the
/// volume is the flat volume it was before the verb — the pre-verb
/// digest, every claimed extent reachable, writer-mountable, and a fresh
/// conversion of it succeeds.
async fn abort_after(window: EnableSymCrash) {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 1, 120).await;
    let sector0_before = squeezefs::uring_fs::read_at(Path::new(&uris[0]), 0, 4096)
        .await
        .unwrap();
    enable_symmetric_with(
        &uris,
        &EnableSymOptions::default(),
        &EnableSymHooks {
            crash_after: Some(window),
        },
    )
    .await
    .expect_err("the injected crash aborts the verb");
    assert!(marker_present(&uris[0]).await);
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));

    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            abort: true,
            ..Default::default()
        },
    )
    .await
    .expect("--abort undoes the crashed run on a flat volume");
    println!("abort row after {window:?}: {:?}", report.rows[0]);
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Aborted);
    if matches!(
        window,
        EnableSymCrash::AfterBuild { .. } | EnableSymCrash::AfterLedger { .. }
    ) {
        assert!(
            report.rows[0].orphans_reclaimed > 0,
            "the abort reclaims the crashed build's extents"
        );
    } else {
        assert_eq!(report.rows[0].orphans_reclaimed, 0);
    }
    assert!(!marker_present(&uris[0]).await, "the marker is gone");
    assert!(!is_symmetric(&superblock_of(&uris[0]).await), "still flat");
    let sector0_after = squeezefs::uring_fs::read_at(Path::new(&uris[0]), 0, 4096)
        .await
        .unwrap();
    assert!(sector0_before == sector0_after, "sector 0 never changed");
    assert_eq!(
        digest_of(&uris[0]).await,
        digests[0],
        "the pre-verb digest — the same records, nothing else"
    );
    let (claimed, reachable) = extent_census(&uris[0]).await;
    assert_eq!(
        claimed, reachable,
        "every claimed extent is reachable again — the aborted build left nothing"
    );

    let routed = open_pre_flip_writer(&uris)
        .await
        .expect("a writer mounts the aborted volume");
    assert_population(&routed, &pop).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    // A plain conversion of the restored volume succeeds.
    let report = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("convert after the abort");
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Converted);
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
    fsck_clean(&uris).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_after_the_markers_restores_the_flat_volume() {
    abort_after(EnableSymCrash::AfterMarker).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_after_a_crashed_build_restores_the_flat_volume() {
    abort_after(EnableSymCrash::AfterBuild { volume: 0 }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_after_the_hybrid_ledger_restores_the_flat_volume() {
    abort_after(EnableSymCrash::AfterLedger { volume: 0 }).await;
}

/// `--abort` on a volume past its flip refuses naming `--resume`, touches
/// nothing (the marker stays, the stamp stays), and the resume then
/// finishes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_on_a_stamped_volume_refuses_naming_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 1, 60).await;
    enable_symmetric_with(
        &uris,
        &EnableSymOptions::default(),
        &EnableSymHooks {
            crash_after: Some(EnableSymCrash::AfterStamp { volume: 0 }),
        },
    )
    .await
    .expect_err("crash");
    assert!(is_symmetric(&superblock_of(&uris[0]).await));
    assert!(marker_present(&uris[0]).await);
    let before = fixed_region_of(&uris[0]).await;
    let err = enable_symmetric(
        &uris,
        &EnableSymOptions {
            abort: true,
            ..Default::default()
        },
    )
    .await
    .expect_err("refused past the flip");
    let msg = err.to_string();
    assert!(
        msg.contains("--resume") && msg.contains("past its flip") && msg.contains(&uris[0]),
        "names the volume and the only exit: {msg}"
    );
    assert!(before == fixed_region_of(&uris[0]).await, "nothing undone");
    assert!(marker_present(&uris[0]).await);
    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            resume: true,
            ..Default::default()
        },
    )
    .await
    .expect("resume finishes");
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Resumed);
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    assert_population(&routed, &pop).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_with_nothing_to_abort_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 10).await;
    let err = enable_symmetric(
        &uris,
        &EnableSymOptions {
            abort: true,
            ..Default::default()
        },
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("nothing to abort"), "{err}");
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));
}

/// **Issue 1 (the half-converted set).** `--abort` on a set with one
/// volume past its flip refuses the WHOLE set — the flat marked volumes
/// keep their markers (nothing partial), and `--resume` finishes all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_on_a_half_converted_set_refuses_whole_and_undoes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, _pop) = populated_flat_set(dir.path(), 2, 40).await;
    enable_symmetric_with(
        &uris,
        &EnableSymOptions::default(),
        &EnableSymHooks {
            crash_after: Some(EnableSymCrash::AfterStamp { volume: 0 }),
        },
    )
    .await
    .expect_err("crash");
    assert!(is_symmetric(&superblock_of(&uris[0]).await));
    assert!(!is_symmetric(&superblock_of(&uris[1]).await));
    assert!(marker_present(&uris[0]).await && marker_present(&uris[1]).await);
    let err = enable_symmetric(
        &uris,
        &EnableSymOptions {
            abort: true,
            ..Default::default()
        },
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains(&uris[0]), "{err}");
    assert!(
        marker_present(&uris[1]).await,
        "the flat volume's marker stays — nothing was undone on any volume"
    );
    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            resume: true,
            ..Default::default()
        },
    )
    .await
    .expect("resume");
    assert!(report
        .rows
        .iter()
        .all(|r| r.outcome == ConversionOutcome::Resumed));
    for (i, uri) in uris.iter().enumerate() {
        assert_eq!(digest_of(uri).await, digests[i]);
    }
}

/// **Issue 4.** A crashed build's images carry node seqs ABOVE the mounted
/// ledger's watermark (they were never freed under a record — the
/// watermark law does not cover them). The resume's census reclaims those
/// extents and floors its writer above every stamp their residue carries,
/// so no forest node can ever share a seq with a frame left in a
/// reclaimed extent (the §4.1 tail-chain hazard): every node the
/// converted volume reaches is stamped above the crashed build's highest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_resume_floors_its_node_seqs_above_every_reclaimed_extents_residue() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, _pop) = populated_flat_set(dir.path(), 1, 120).await;
    let sb = superblock_of(&uris[0]).await;
    let p = Path::new(&uris[0]);
    let ledger_before = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    enable_symmetric_with(
        &uris,
        &EnableSymOptions::default(),
        &EnableSymHooks {
            crash_after: Some(EnableSymCrash::AfterBuild { volume: 0 }),
        },
    )
    .await
    .expect_err("crash after the build");
    // The heap's highest residue stamp — the crashed build's images — vs
    // the watermark the flat ledger still carries.
    let node_size = sb.node_size as usize;
    let mut residue_max = 0u64;
    for extent in 0..sb.total_extents() {
        let img =
            squeezefs::uring_fs::read_at(p, sb.heap.start + extent * node_size as u64, node_size)
                .await
                .unwrap();
        residue_max = residue_max.max(residue_seq_ceiling(&img));
    }
    let ledger_crashed = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    assert!(
        residue_max > ledger_crashed.node_seq_watermark,
        "the orphaned build's images sit ABOVE the mounted watermark ({} > {}) — the hazard \
         the floor exists for is real here",
        residue_max,
        ledger_crashed.node_seq_watermark
    );
    assert!(ledger_crashed.node_seq_watermark >= ledger_before.node_seq_watermark);

    enable_symmetric(
        &uris,
        &EnableSymOptions {
            resume: true,
            ..Default::default()
        },
    )
    .await
    .expect("resume");
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
    let be = KvMetaBackend::open_probe(p).await.expect("probe");
    let ledger_after = be.mounted_ledger().clone();
    assert!(
        ledger_after.node_seq_watermark > residue_max,
        "the ledger's watermark passed every reclaimed stamp: {} > {}",
        ledger_after.node_seq_watermark,
        residue_max
    );
    let layout = squeezefs::meta_backend::kv::node::NodeLayout::new(node_size).unwrap();
    let mut nodes = 0u64;
    for tree in be.all_trees() {
        for addr in tree.reachable_node_addrs().await.expect("walk") {
            let node = squeezefs::meta_backend::kv::node::load_node(p, &layout, addr, u64::MAX)
                .await
                .expect("a reachable node loads");
            assert!(
                node.header().node_seq > residue_max,
                "node {addr:#x} seq {} ≤ the reclaimed residue's {} — a fresh node could adopt \
                 a dead frame",
                node.header().node_seq,
                residue_max
            );
            nodes += 1;
        }
    }
    assert!(nodes > 1);
}

/// **Issue 5.** A hosted slot whose stamp carries a cursor but whose inos
/// were all deleted gets an EMPTY slot tree and a tree-0 `Unleased`
/// record carrying that cursor: tree 0 is the cursor's durable home
/// without the stamp (§5.1.8), so a lease that reads tree 0 alone can
/// never re-mint a deleted ino. Pinned twice: tree 0 names every stamped
/// guest slot at or above the stamp's cursor, and no ino minted after the
/// conversion is one that was deleted before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_emptied_before_conversion_never_remints_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let uris = format_flat_set(dir.path(), 1).await;
    let routed = open_pre_flip_writer(&uris).await.expect("open");
    let gone = routed
        .create(ROOT_INO, "gone", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir")
        .ino;
    let mut deleted = std::collections::BTreeSet::new();
    for i in 0..300 {
        let f = routed
            .create(gone, &format!("g{i:04}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        deleted.insert(f);
    }
    for i in 0..300 {
        routed
            .unlink(gone, &format!("g{i:04}"))
            .await
            .expect("unlink");
    }
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let sb = superblock_of(&uris[0]).await;
    let p = Path::new(&uris[0]);
    let stamp = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger")
        .membership_stamp
        .expect("stamp");
    let guests: Vec<(u16, u64)> = stamp
        .slot_cursors
        .iter()
        .copied()
        .filter(|(s, _)| stamp.resolved_native_slot() != Some(*s))
        .collect();
    assert!(
        guests.len() > 1,
        "the mint spread left cursors on several guest slots: {}",
        guests.len()
    );

    let report = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("convert");
    assert!(
        report.rows[0].slot_trees as usize > guests.len(),
        "one slot tree per stamped guest slot plus the native: {} > {}",
        report.rows[0].slot_trees,
        guests.len()
    );

    // Tree 0 carries every stamped guest slot's cursor.
    let be = KvMetaBackend::open_probe(p).await.expect("probe");
    let control = be
        .all_trees()
        .into_iter()
        .find(|t| t.tree_id() == TREE_CONTROL)
        .expect("tree 0");
    let (start, end) = slot_state_key_range();
    let mut named: BTreeMap<u32, u64> = BTreeMap::new();
    for (k, v) in control.range(&start, &end, 100_000).await.expect("range") {
        let slot = decode_slot_state_key(&k).expect("slot key");
        match SlotState::decode(&v).expect("slot state") {
            SlotState::Unleased { cursor, .. } => {
                named.insert(slot, cursor);
            }
            other => panic!("PR 11 writes Unleased only: {other:?}"),
        }
    }
    for (slot, cursor) in &guests {
        let fs = guest_forest_slot(*slot);
        let got = named
            .get(&fs)
            .unwrap_or_else(|| panic!("tree 0 names no record for stamped guest slot {slot}"));
        assert!(
            *got >= *cursor,
            "slot {slot}: tree 0's cursor {got} is below the stamp's {cursor}"
        );
    }
    drop(be);

    // Nothing minted after the conversion is an ino deleted before it.
    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    for i in 0..600 {
        let f = routed
            .create(gone, &format!("n{i:04}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        assert!(
            !deleted.contains(&f),
            "ino {f} was deleted before the conversion and re-minted after it"
        );
    }
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// **Issue 12.** The non-solo bit-8 remedy — "mount the set solo once" —
/// is the verb's OWN quiesce since PR 14 (the mount's door refuses the
/// pre-flip class): its solo open re-checkpoints the ledger in solo form,
/// and the verb converts in the same invocation; the converted volume's
/// digest is the source's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_solo_mount_of_a_non_solo_record_volume_leaves_a_solo_record_and_the_verb_converts() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, _p) = populated_flat_set(dir.path(), 1, 20).await;
    let sb = superblock_of(&uris[0]).await;
    let p = Path::new(&uris[0]);
    let mut rec = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    rec.seq += 1;
    rec.append_partition = Some(AppendPartition::new(2, 0).expect("legal partition"));
    write_ledger_slot(p, sb.root_ledger.start, &rec)
        .await
        .expect("write");
    squeezefs::uring_fs::fdatasync(p).await.unwrap();
    let report = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("the verb's quiesce is the solo mount; then it converts");
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Converted);
    assert!(is_symmetric(&superblock_of(&uris[0]).await));
    let after = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    assert!(
        after.append_partition.is_none_or(|part| part.is_solo()),
        "the quiesce's checkpoint wrote a solo-form record: {:?}",
        after.append_partition
    );
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
}

/// A flat volume whose ring holds entries past its checkpoint tail (a
/// crashed writer whose claim has aged out — within the 45 s TTL the
/// live-client gate refuses it first) is QUIESCED by the verb itself
/// before any marker (PR 14: the mount that used to be the remedy
/// refuses the pre-flip class presence-required, so the verb runs the
/// mount's own crash recovery under its admission — the census the
/// conversion runs is exact only over an empty window, and the remedy
/// must be reachable); a dry run names the quiesce and writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_volume_not_cleanly_unmounted_is_quiesced_by_the_verb_before_any_marker() {
    use squeezefs::meta_backend::kv::journal::{
        checkpoint_reserve_bytes, entry_len_for, JournalRing, JOURNAL_PAGE_LEN,
    };
    use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
    use squeezefs::meta_backend::kv::record::Record;
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 1, 30).await;
    let p = Path::new(&uris[0]);
    // A committed-but-not-checkpointed record past the tail — the shape
    // a kill -9 after an ack leaves — written through the ring's own
    // primitive at the recovered head (the claim record itself is gone:
    // the clean unmount removed it, as an aged-out crash's is ignored).
    let sb = superblock_of(&uris[0]).await;
    let ledger = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    let (ring, recovery) = JournalRing::recover(
        p,
        sb.journal.start,
        sb.journal.len / JOURNAL_PAGE_LEN,
        checkpoint_reserve_bytes(sb.journal.len),
        ledger.journal_tail_seq,
    )
    .await
    .expect("recover the ring");
    assert!(
        recovery.entries.is_empty(),
        "cleanly unmounted: empty window"
    );
    let key = xattr_key(
        ROOT_INO,
        squeezefs::meta_backend::kv::record::xattr_name_hash56(
            b"user.uncheckpointed",
            sb.hash_seed,
        ),
        0,
    );
    let value = XattrValue {
        name: b"user.uncheckpointed".to_vec(),
        value: b"x".to_vec(),
    }
    .encode()
    .unwrap();
    let probe = vec![(TREE_XATTRS, Record::put(key.to_vec(), 0, value.clone()))];
    let need = entry_len_for(&probe).unwrap();
    let adm = ring
        .try_admit(need, AdmissionClass::User)
        .expect("room in the ring");
    let res = ring.core().reserve(adm);
    ring.write_entry(
        &res,
        &[(TREE_XATTRS, Record::put(key.to_vec(), res.seq(), value))],
    )
    .await
    .expect("write the entry");
    squeezefs::uring_fs::fdatasync(p).await.unwrap();
    drop(ring);
    // A dry run writes nothing and names the quiesce it would run (the
    // remedy the pre-flip binary's mount used to be: since PR 14 the
    // mount's writer door refuses the class, so the verb IS the remedy).
    let before = fixed_region_of(&uris[0]).await;
    let err = enable_symmetric(
        &uris,
        &EnableSymOptions {
            dry_run: true,
            ..EnableSymOptions::default()
        },
    )
    .await
    .expect_err("a dry run cannot plan against a window");
    let msg = err.to_string();
    assert!(
        msg.contains("not cleanly unmounted") && msg.contains("QUIESCES"),
        "names the state and what the real run does: {msg}"
    );
    assert!(before == fixed_region_of(&uris[0]).await, "nothing written");
    assert!(!marker_present(&uris[0]).await);
    // The real run quiesces the set itself — the window's record folded
    // into the trees by the replay, checkpointed, the clean leave — then
    // converts: the uncheckpointed xattr is in the forest.
    enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("converts after its own quiesce");
    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    assert_population(&routed, &pop).await;
    assert_eq!(
        routed
            .getxattr(ROOT_INO, "user.uncheckpointed")
            .await
            .unwrap(),
        Some(b"x".to_vec()),
        "the window's record survived the quiesce into the forest"
    );
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let _ = digests;
}

/// **Issue 13.** The default `format` is untouched, as a TEST: the flat
/// image of the fixed description digests to the value the pre-PR-11
/// builder produced (computed on `dev` @ `5eca0e12` for the same
/// description), for both the `--single-writer` class and the default
/// multi-writer-capable class.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flat_image_of_the_fixed_description_digests_to_the_pre_pr_golden() {
    const GOLDEN_SINGLE_WRITER: u64 = 0xd132_9394_4543_ef28;
    const GOLDEN_MULTI_WRITER: u64 = 0x4358_b34f_3970_f9c9;
    let _g = SEAM.lock().await;
    let single = tempfile::NamedTempFile::new().unwrap();
    single.as_file().set_len(VOL_LEN).unwrap();
    let multi = tempfile::NamedTempFile::new().unwrap();
    multi.as_file().set_len(VOL_LEN).unwrap();
    let r1 = describe().build(single.path(), VOL_LEN).await;
    let mut mw = describe();
    mw.set_multi_writer();
    let r2 = mw.build(multi.path(), VOL_LEN).await;
    r1.expect("build the single-writer flat image");
    r2.expect("build the multi-writer flat image");
    let d1 = xxhash_rust::xxh3::xxh3_64(&std::fs::read(single.path()).unwrap());
    let d2 = xxhash_rust::xxh3::xxh3_64(&std::fs::read(multi.path()).unwrap());
    assert_eq!(
        d1, GOLDEN_SINGLE_WRITER,
        "the single-writer flat image changed: {d1:#018x}"
    );
    assert_eq!(
        d2, GOLDEN_MULTI_WRITER,
        "the multi-writer flat image changed: {d2:#018x}"
    );
}

/// **The §5b finding, as a test** (`.benchmarks/2026-09-14-sym-pr11-convert.md`
/// §5b — SHIPPED, every layout; NOT fixed by PR 11): `RoutedMetaBackend::rename`
/// (`src/meta_backend/mod.rs`, the `lock_many` over the two parents and
/// the two names) → `KvMetaBackend::routed_rename_local` stages the MOVED
/// inode's ctime `Delta` without holding `I{moved}`, while a concurrent
/// layout publish of the same ino holds `I{ino}` and stages a `Put` — so
/// both land in one conveyor batch on one key and the pass's same-key
/// co-queue exclusion fires (a debug binary fails the batch, a release
/// binary can lose the rename's ctime). Ignored until the rename takes
/// `I{moved}` in its `lock_many` (lookup → one `lock_many` incl. the
/// moved and the overwritten ino → revalidate); un-ignore with the fix.
#[ignore = "the shipped rename guard hole (note §5b): rename never locks I{moved}; un-ignore with the fix"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_and_a_layout_publish_of_one_ino_never_co_queue_a_delta_and_a_put_unguarded() {
    let dir = tempfile::tempdir().unwrap();
    let uris = format_flat_set(dir.path(), 1).await;
    let routed = open_pre_flip_writer(&uris).await.expect("open");
    let d = routed
        .create(ROOT_INO, "race", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .expect("mkdir")
        .ino;
    for round in 0..200u32 {
        let f = routed
            .create(d, &format!("f{round}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        let (vi, local) = routed.route_ino(f);
        let vol = routed.volumes[vi].clone();
        let (from, to) = (format!("f{round}"), format!("g{round}"));
        let (renamed, published) = tokio::join!(
            routed.rename(d, &from, d, &to, 0),
            vol.set_layout_and_size(local, b"layout", 4096, &[])
        );
        renamed.unwrap_or_else(|e| panic!("round {round}: rename failed: {e}"));
        published.unwrap_or_else(|e| panic!("round {round}: publish failed: {e}"));
    }
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// Round 3 — the torn-stamp window under `--abort`; the abort's seq floor.
// ---------------------------------------------------------------------------

/// **Issue 14.** The stamp writes the DUR-5 backup copy FIRST, so a kill
/// between its two sector writes leaves sector 0 FLAT under a STAMPED
/// copy. Every reader honours the primary — the volume is flat and an
/// abort would be admitted — but the abort writes no superblock, so the
/// stale stamped copy would stay for a later sector-0 failure to fall
/// back onto. `--abort` refuses that window naming `--resume`, touching
/// nothing; `--resume` finishes and rewrites BOTH copies consistent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abort_under_a_stamped_backup_copy_refuses_naming_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, pop) = populated_flat_set(dir.path(), 1, 60).await;
    let p = Path::new(&uris[0]);
    enable_symmetric_with(
        &uris,
        &EnableSymOptions::default(),
        &EnableSymHooks {
            crash_after: Some(EnableSymCrash::AfterLedger { volume: 0 }),
        },
    )
    .await
    .expect_err("crash after the hybrid ledger");
    // The torn stamp: a stamped image at a newer generation in the backup
    // slot, sector 0 still flat — exactly what a kill between the two
    // writes of `set_symmetric_forest` leaves.
    let primary = superblock_of(&uris[0]).await;
    assert!(!is_symmetric(&primary));
    let primary_sector = squeezefs::uring_fs::read_at(p, 0, SUPERBLOCK_V3_LEN)
        .await
        .unwrap();
    let mut stamped = primary.clone();
    stamped.features_incompat |= FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST;
    stamped.appender_dir = ExtentRef {
        start: primary.heap.start,
        len: u64::from(primary.node_size),
    };
    let img = stamped
        .encode_sector_at_generation(sector_generation(&primary_sector) + 1)
        .expect("a stamped image");
    let off = backup_offset(VOL_LEN).expect("the volume reserves the backup slot");
    squeezefs::uring_fs::write_at(p, off, bytes::Bytes::from(img))
        .await
        .unwrap();
    squeezefs::uring_fs::fdatasync(p).await.unwrap();
    assert!(
        read_backup_superblock(p)
            .await
            .unwrap()
            .expect("the backup slot holds a superblock")
            .symmetric_forest_stamped(),
        "the planted backup copy carries bit 17"
    );
    assert!(
        !is_symmetric(&superblock_of(&uris[0]).await),
        "sector 0 is authoritative and still flat"
    );

    let before = fixed_region_of(&uris[0]).await;
    let err = enable_symmetric(
        &uris,
        &EnableSymOptions {
            abort: true,
            ..Default::default()
        },
    )
    .await
    .expect_err("the torn-stamp window refuses the abort");
    let msg = err.to_string();
    assert!(
        msg.contains("redundant superblock copy")
            && msg.contains("--resume")
            && msg.contains(&uris[0]),
        "names the copy, the volume and the only exit: {msg}"
    );
    assert!(before == fixed_region_of(&uris[0]).await, "nothing undone");
    assert!(marker_present(&uris[0]).await, "the marker stays");
    assert!(
        read_backup_superblock(p)
            .await
            .unwrap()
            .unwrap()
            .symmetric_forest_stamped(),
        "the backup copy is untouched by the refusal"
    );

    // `--resume` finishes the conversion and leaves the two copies
    // agreeing on the layout (both stamped, one directory).
    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            resume: true,
            ..Default::default()
        },
    )
    .await
    .expect("resume");
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Resumed);
    let primary = superblock_of(&uris[0]).await;
    let backup = read_backup_superblock(p).await.unwrap().expect("backup");
    assert!(is_symmetric(&primary) && backup.symmetric_forest_stamped());
    assert_eq!(
        backup.appender_dir, primary.appender_dir,
        "the resume's stamp rewrote both copies from one image"
    );
    assert_eq!(digest_of(&uris[0]).await, digests[0]);
    let routed = open_routed_meta_set(&uris).await.expect("forest mount");
    assert_population(&routed, &pop).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

/// **Issue 15 — Issue 4's abort-path twin.** The abort returns the
/// crashed build's images (node seqs ABOVE the flat ledger's watermark)
/// to the FLAT free list; its ledger record raises the watermark above
/// every stamp their residue carries (`residue_seq_ceiling`), so the
/// flat volume's later SMOs — minted lowest-free into exactly those
/// extents — never share a seq with a dead frame there. Pinned: after the
/// abort the ledger's watermark is at or above the residue's ceiling, and
/// every node a later flat mount writes is stamped above it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_abort_raises_the_flat_watermark_above_every_reclaimed_extents_residue() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, digests, _pop) = populated_flat_set(dir.path(), 1, 120).await;
    let sb = superblock_of(&uris[0]).await;
    let p = Path::new(&uris[0]);
    enable_symmetric_with(
        &uris,
        &EnableSymOptions::default(),
        &EnableSymHooks {
            crash_after: Some(EnableSymCrash::AfterBuild { volume: 0 }),
        },
    )
    .await
    .expect_err("crash after the build");
    let node_size = sb.node_size as usize;
    let mut residue_max = 0u64;
    for extent in 0..sb.total_extents() {
        let img =
            squeezefs::uring_fs::read_at(p, sb.heap.start + extent * node_size as u64, node_size)
                .await
                .unwrap();
        residue_max = residue_max.max(residue_seq_ceiling(&img));
    }
    let ledger_crashed = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    assert!(
        residue_max > ledger_crashed.node_seq_watermark,
        "the orphaned build's images sit ABOVE the flat watermark ({residue_max} > {}) — the \
         abort returns them to the flat free list",
        ledger_crashed.node_seq_watermark
    );
    // The flat trees' nodes before the abort (they keep their old seqs).
    let reachable_before: std::collections::BTreeSet<u64> = {
        let be = KvMetaBackend::open_probe(p).await.expect("probe");
        let mut set = std::collections::BTreeSet::new();
        for tree in be.all_trees() {
            set.extend(tree.reachable_node_addrs().await.expect("walk"));
        }
        set
    };

    let report = enable_symmetric(
        &uris,
        &EnableSymOptions {
            abort: true,
            ..Default::default()
        },
    )
    .await
    .expect("abort");
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Aborted);
    assert!(report.rows[0].orphans_reclaimed > 0);
    let ledger_after = read_newest_ledger(p, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("ledger");
    assert!(
        ledger_after.node_seq_watermark >= residue_max,
        "the abort's ledger record carries the raised watermark: {} ≥ {residue_max}",
        ledger_after.node_seq_watermark
    );
    assert!(
        ledger_after
            .tree_roots
            .iter()
            .all(|r| r.tree_id != TREE_CONTROL && r.tree_id != KIND_INTERIOR),
        "the abort's record names the flat roots alone"
    );
    assert_eq!(digest_of(&uris[0]).await, digests[0]);

    // A later flat mount mints its SMOs into the reclaimed extents: every
    // node it writes is stamped above the residue's ceiling.
    let routed = open_pre_flip_writer(&uris).await.expect("flat mount");
    fill_with_large_xattrs(&routed, 120, 15_000).await;
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
    drop(routed);
    let be = KvMetaBackend::open_probe(p).await.expect("probe");
    let layout = squeezefs::meta_backend::kv::node::NodeLayout::new(node_size).unwrap();
    let mut new_nodes = 0u64;
    for tree in be.all_trees() {
        for addr in tree.reachable_node_addrs().await.expect("walk") {
            if reachable_before.contains(&addr) {
                continue;
            }
            let node = squeezefs::meta_backend::kv::node::load_node(p, &layout, addr, u64::MAX)
                .await
                .expect("a reachable node loads");
            assert!(
                node.header().node_seq > residue_max,
                "new node {addr:#x} seq {} ≤ the reclaimed residue's {residue_max}",
                node.header().node_seq
            );
            new_nodes += 1;
        }
    }
    assert!(
        new_nodes > 0,
        "the fill minted new nodes into the reclaimed extents"
    );
}
