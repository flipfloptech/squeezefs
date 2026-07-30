//! Meta-plane write distribution — the 2026-07-30 field conviction
//! (`.benchmarks/2026-07-30-meta-plane-writes.md`): a 4-node cluster's
//! streaming writes sat at a ~21–25 k meta-device-writes/s ceiling on
//! **one** metadata volume while the second volume of the
//! `--meta-slots 8` pair recorded **0.00 device writes all week**.
//!
//! Root cause under test: regular-file inode placement was
//! **parent-sticky** (`create_with_rdev`: `target_v_idx = parent_v_idx`
//! for non-directories) while only directories striped by health-banded
//! round-robin — and the root directory is pinned to slot 0 → volume 0.
//! Every data-plane meta commit (block publish / size flip / extent
//! spill / destroy) routes by the file's ino, so a workload whose files
//! live under one directory drives 100 % of journal traffic into one
//! volume's journal + conveyor while its siblings idle.
//!
//! Contracts pinned here (red against the parent-sticky bug):
//! - **Per-volume journal write counters exist and count** (the field
//!   adjudication instrument — the global `meta_kv_journal_entries` is
//!   process-wide and cannot see the imbalance).
//! - **Single-directory file storms spread inode placement** across the
//!   volume set (each volume of a symmetric pair hosts ≥ 25 % of the
//!   minted file inos).
//! - **The single-directory block-publish workload's journal entries
//!   land balanced**: each volume of a healthy symmetric 2-volume set
//!   carries ≥ 30 % of the total journal entries (tolerance stated: the
//!   parent volume legitimately carries every dentry-side entry on top
//!   of its share of mints/publishes, so 50/50 is not the contract —
//!   30/70 is the loud-failure boundary; the bug's shape is ~1/99).
//! - **Directory striping keeps working** (the pre-existing good
//!   behavior, pinned so the fix can never regress it).

use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, Metadata};
use std::path::{Path, PathBuf};

const VOL_LEN: u64 = 256 * 1024 * 1024;

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn opts() -> squeezefs::meta_backend::kv::builder::FormatV3Options {
    squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format a stamped 2-volume set the way `format --meta-slots W` does
/// (identity slot distribution, epoch 1) — the exact field shape
/// (`sqmeta:///dev/nvme0n1,/dev/nvme2n1 --meta-slots 8`).
async fn format_stamped_set(metas: &[PathBuf], width: u32) {
    let plan = plan_meta_slot_set(metas.len(), width).expect("plan admits the bounds");
    for (i, m) in metas.iter().enumerate() {
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            m,
            VOL_LEN,
            &opts(),
            plan.stamps[i].clone(),
        )
        .await
        .expect("format stamped meta volume");
    }
}

/// Per-volume journal-entry snapshot (the new instrument under test).
fn journal_entries_per_volume(routed: &squeezefs::meta_backend::RoutedMetaBackend) -> Vec<u64> {
    routed
        .volumes
        .iter()
        .map(|be| be.journal_ring().written_entries())
        .collect()
}

async fn shutdown(routed: &squeezefs::meta_backend::RoutedMetaBackend) {
    for vol in &routed.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

/// A synthetic striped-layout value shaped like the write path's block
/// publish (`save_metadata_to_backend` → `set_layout_and_size`): the
/// serialized layout grows with the block map, so per-publish journal
/// bytes are realistic without a data plane in the fixture.
fn layout_bytes(blocks: usize) -> Vec<u8> {
    let mut map = std::collections::HashMap::new();
    for b in 0..blocks as u32 {
        map.insert(b, format!("backend_0://{}", b as u64 * 4 * 1024 * 1024));
    }
    let layout = squeezefs::routing::LayoutMetadata {
        file_type: "striped".to_string(),
        size: blocks as u64 * 4 * 1024 * 1024,
        block_map_id: Some("block_map_test".to_string()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map),
    };
    bincode::serialize(&layout).expect("serialize layout")
}

// ---------------------------------------------------------------------------
// Instrument: per-volume journal write counters
// ---------------------------------------------------------------------------

/// The per-volume counters exist and count: on a single-volume set, N
/// creates land ≥ N journal entries on THAT volume's ring counter, and
/// the byte counter moves with them. (The process-global
/// `meta_kv_journal_entries` cannot attribute traffic to a volume —
/// the exact blindness that let the field imbalance hide.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_per_volume_journal_write_counters_count() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "solo", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3(&meta, VOL_LEN, &opts())
        .await
        .expect("format v3");
    let paths = vec![meta.display().to_string()];
    let routed = open_routed_meta_set(&paths).await.expect("open");

    let before = journal_entries_per_volume(&routed);
    let bytes_before = routed.volumes[0].journal_ring().written_bytes();
    const N: usize = 16;
    for i in 0..N {
        routed
            .create(1, &format!("c{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
    }
    let after = journal_entries_per_volume(&routed);
    let bytes_after = routed.volumes[0].journal_ring().written_bytes();
    assert!(
        after[0] - before[0] >= N as u64,
        "{N} creates must land >= {N} journal entries on volume 0's per-volume \
         counter — got {} -> {}",
        before[0],
        after[0]
    );
    assert!(
        bytes_after > bytes_before,
        "per-volume journal byte counter must move with the entries"
    );
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// Placement: single-directory file storms must spread inodes
// ---------------------------------------------------------------------------

/// Regular files created in ONE directory (the root — pinned to slot 0 →
/// volume 0) must stripe their inode placement across the healthy volume
/// set exactly like directories do. Parent-sticky placement (the bug)
/// puts 100 % on volume 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_dir_file_storm_spreads_inode_placement() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 8).await;
    let paths: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();
    let routed = open_routed_meta_set(&paths).await.expect("open");

    const N: usize = 32;
    let mut per_volume = vec![0usize; routed.volumes.len()];
    for i in 0..N {
        let inode = routed
            .create(1, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
        let (v_idx, _local) = routed.route_ino(inode.ino);
        per_volume[v_idx] += 1;
    }
    for (v, &n) in per_volume.iter().enumerate() {
        assert!(
            n * 4 >= N,
            "volume {v} hosts {n}/{N} file inos — single-directory file \
             storms must stripe across the healthy set (each symmetric \
             volume >= 25 %); parent-sticky placement starves it \
             (distribution: {per_volume:?})"
        );
    }
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// THE field contract: single-directory block-publish journal balance
// ---------------------------------------------------------------------------

/// The field workload's shape: files under one directory, each taking a
/// stream of block-publish commits (`set_layout_and_size` — the exact
/// commit `save_metadata_to_backend` issues per published block). The
/// per-volume journal-entry distribution must come out balanced: each
/// volume of the healthy symmetric pair carries >= 30 % of the total
/// (the bug's shape is ~1 % — writer-claim heartbeats only — vs 99 %).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_dir_workload_journal_distribution_balanced() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 8).await;
    let paths: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();
    let routed = open_routed_meta_set(&paths).await.expect("open");

    let before = journal_entries_per_volume(&routed);

    const FILES: usize = 32;
    const PUBLISHES_PER_FILE: usize = 8;
    let mut inos = Vec::with_capacity(FILES);
    for i in 0..FILES {
        let inode = routed
            .create(1, &format!("s{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create");
        inos.push(inode.ino);
    }
    // The streaming phase: per-block publishes on every file (the field's
    // 2,600 blocks/s all-through-one-volume conviction, miniaturized).
    for (i, &ino) in inos.iter().enumerate() {
        for b in 1..=PUBLISHES_PER_FILE {
            let bytes = layout_bytes(b);
            routed
                .set_layout_and_size(ino, &bytes, (b as u64) * 4 * 1024 * 1024)
                .await
                .unwrap_or_else(|e| panic!("publish {b} on file {i}: {e}"));
        }
    }

    let after = journal_entries_per_volume(&routed);
    let deltas: Vec<u64> = after
        .iter()
        .zip(&before)
        .map(|(a, b)| a.saturating_sub(*b))
        .collect();
    let total: u64 = deltas.iter().sum();
    assert!(
        total >= (FILES * (1 + PUBLISHES_PER_FILE)) as u64,
        "engagement: the workload must have journaled at least one entry \
         per create+publish — got {total} ({deltas:?})"
    );
    for (v, &d) in deltas.iter().enumerate() {
        assert!(
            d * 10 >= total * 3,
            "volume {v} carried {d}/{total} journal entries (< 30 %) — the \
             single-directory workload must spread the meta plane across \
             the healthy set (distribution: {deltas:?}; the field shape \
             of this failure is one volume at ~21-25k writes/s and its \
             sibling at 0.00)"
        );
    }
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// Pin: directory striping (pre-existing good behavior)
// ---------------------------------------------------------------------------

/// Directories already striped by health-banded round-robin before this
/// campaign — pinned so the file-placement fix can never regress it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_dir_placement_stripes_across_volumes_pin() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 8).await;
    let paths: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();
    let routed = open_routed_meta_set(&paths).await.expect("open");

    const N: usize = 8;
    let mut per_volume = vec![0usize; routed.volumes.len()];
    for i in 0..N {
        let inode = routed
            .create(1, &format!("d{i}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir");
        let (v_idx, _local) = routed.route_ino(inode.ino);
        per_volume[v_idx] += 1;
    }
    for (v, &n) in per_volume.iter().enumerate() {
        assert!(
            n >= 2,
            "volume {v} hosts {n}/{N} directories — health-banded RR dir \
             striping regressed (distribution: {per_volume:?})"
        );
    }
    shutdown(&routed).await;
}
