//! Symmetric metadata program, PR 11 — **the real bit-17 stamp**:
//! `squeezefs format --symmetric` and the OFFLINE conversion verb
//! `squeezefs volume enable-symmetric` (`docs/design-symmetric-metadata.md`
//! §6.2, §7.1–§7.3, §5.2.1/§5.2.2, §5.1.8, KD-SYM-12).
//!
//! This suite is **layout-blind by construction**: its stamping is the
//! VERB (or the `--symmetric` format arm), never the test seam — a
//! flat volume goes in, a forest comes out, and the seam's presence or
//! absence in the environment changes nothing it asserts. It rides
//! `tests/run_sym_forest_suites.sh`'s list so both legs run it.
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
//! - `format --symmetric` builds the image the seam builds, byte for
//!   byte; a default `format` is untouched (no bit, no directory);
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
    digest_backend, format_v3_stamped, format_v3_stamped_symmetric, BuilderConfig, FormatV3Options,
    ImageBuilder, ROOT_INO,
};
use squeezefs::meta_backend::kv::checkpoint::{read_newest_ledger, write_ledger_slot};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::record::{
    xattr_key, XattrValue, HASH56_MAX, KIND_INTERIOR, TREE_CONTROL, TREE_XATTRS,
};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, SuperblockV3, VolumeFormat, FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use squeezefs::SYM_UPGRADE_MARKER_XATTR;
use std::collections::BTreeMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Small volumes: 64 KiB nodes, a 1 MiB ring (the kv_backend_tests shape).
const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const TEST_SEED: u64 = 0x5EED_C0DE_0000_0011;
const TEST_UUID: [u8; 16] = *b"sym-convert-test";

/// The seam is process-global; the one contract that compares the seam's
/// image against the flag's serializes its two builds through it.
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

/// Format an `n`-member derived-width set of the DEFAULT (multi-writer-
/// capable, bit-17-absent) class — the shape every field volume has. The
/// seam is cleared for the build (and restored): this suite's source
/// volumes are FLAT whatever leg of the matrix runs it — the verb is what
/// stamps.
async fn format_flat_set(dir: &Path, n: usize) -> Vec<String> {
    let plan = plan_meta_slot_set(n).expect("derived plan");
    let mut uris = Vec::with_capacity(n);
    let _g = SEAM.lock().await;
    let prior = std::env::var_os("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    for i in 0..n {
        let p = dir.join(format!("meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        let opts = FormatV3Options {
            format_config_xattr: (i == 0).then(|| format_config_for(dir)),
            ..set_opts()
        };
        let r = format_v3_stamped(&p, VOL_LEN, &opts, plan.stamps[i].clone()).await;
        if r.is_err() {
            if let Some(v) = &prior {
                std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", v);
            }
        }
        r.expect("format flat member");
        uris.push(p.display().to_string());
    }
    if let Some(v) = prior {
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", v);
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

/// The bitmap-vs-reachability census of one quiesced volume through a
/// probe: `(claimed extents, extents some tree root reaches)`.
async fn extent_census(path: &str) -> (u64, u64) {
    let be = KvMetaBackend::open_probe(Path::new(path))
        .await
        .expect("probe open");
    assert_eq!(
        be.replay_stats().entries,
        0,
        "the census is exact only over an empty replay window"
    );
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
    let routed = open_routed_meta_set(&uris).await.expect("open flat set");
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
    match open_routed_meta_set(uris).await {
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

    let report = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect("enable-symmetric converts a flat set");
    assert_eq!(report.rows.len(), 1);
    println!("conversion row: {:?}", report.rows[0]);
    assert_eq!(report.rows[0].outcome, ConversionOutcome::Converted);
    assert!(report.rows[0].records > 0, "records were moved");
    assert!(
        report.rows[0].slot_trees > 1,
        "a derived-width set's mints spread over guest slots: {} slot trees",
        report.rows[0].slot_trees
    );
    assert!(
        report.rows[0].extents_freed > 0,
        "the emptied shared trees returned their extents"
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
    // Idle mount cycles: no records change, and the claimed count holds.
    for _ in 0..3 {
        let routed = open_routed_meta_set(&uris).await.expect("mount");
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
    format_v3_stamped_symmetric(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("format --symmetric");
    let uris = vec![p.display().to_string()];
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("already symmetric"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bit_8_non_solo_partition_record_is_refused_naming_the_solo_mount() {
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
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused");
    let msg = err.to_string();
    assert!(
        msg.contains("partition") && msg.contains("solo"),
        "names the bit-8 record and the solo-mount remedy: {msg}"
    );
    assert!(!is_symmetric(&superblock_of(&uris[0]).await));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_cross_volume_intent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 20).await;
    // Plant an intent record on the reserved intent ino (what a crashed
    // cross-volume transaction leaves behind), through the guarded open.
    let be = KvMetaBackend::open(Path::new(&uris[0]))
        .await
        .expect("open");
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
    let err = enable_symmetric(&uris, &EnableSymOptions::default())
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("cross-volume intent"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_flight_job_record_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (uris, _d, _p) = populated_flat_set(dir.path(), 1, 20).await;
    let be = KvMetaBackend::open(Path::new(&uris[0]))
        .await
        .expect("open");
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
    let live = open_routed_meta_set(&uris).await.expect("live writer");
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
    let img = m.encode();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_symmetric_builds_the_image_the_seam_builds_byte_for_byte() {
    let cfg = BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    };
    let describe = || {
        let mut b = ImageBuilder::new(cfg.clone()).unwrap();
        let docs = b.add_dir(ROOT_INO, "docs", 0o750, 1000, 1000).unwrap();
        let readme = b
            .add_file(docs, "readme.txt", 0o644, 1000, 1000, 4096)
            .unwrap();
        b.set_xattr(readme, "user.color", b"blue").unwrap();
        b.add_link(readme, ROOT_INO, "hard.lnk").unwrap();
        b
    };
    let flag = tempfile::NamedTempFile::new().unwrap();
    flag.as_file().set_len(VOL_LEN).unwrap();
    let seam = tempfile::NamedTempFile::new().unwrap();
    seam.as_file().set_len(VOL_LEN).unwrap();
    {
        let _g = SEAM.lock().await;
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        let mut b = describe();
        b.set_symmetric();
        b.build(flag.path(), VOL_LEN)
            .await
            .expect("build --symmetric");
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
        let r = describe().build(seam.path(), VOL_LEN).await;
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        r.expect("build under the seam");
    }
    let a = std::fs::read(flag.path()).unwrap();
    let b = std::fs::read(seam.path()).unwrap();
    assert!(a == b, "the flag and the seam build byte-identical images");
    let sb = superblock_of(flag.path().to_str().unwrap()).await;
    assert!(is_symmetric(&sb));
    assert!(sb.appender_dir.len != 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_public_symmetric_formatter_mounts_as_a_forest() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).unwrap();
    format_v3_stamped_symmetric(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("format --symmetric");
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
