//! Write-commit-economy campaign (2026-07-30) — the backend-level
//! **block-publish economy contract** (lever 2,
//! `.benchmarks/2026-07-30-write-commit-economy.md`).
//!
//! The conviction (field, e076db7): per-file publish cycles of ~19 ms
//! (rewrite) with `set_layout_and_size` re-serializing the WHOLE block
//! map into every journal entry — O(file_size) meta bytes per published
//! block, 18.2 KiB mean journal entry, the standing 10.3/6.7 GB/s write
//! wall while the meta-IOPS-ceiling hypothesis was falsified by the
//! balanced-journals experiment.
//!
//! The contract: [`RoutedMetaBackend::merge_layout_and_size`] publishes
//! a batch as ONE two-record transaction whose layout record is an
//! **O(batch) delta** whenever a live inline base exists — journal
//! bytes per publish must stop tracking the map size. RED at the
//! contract commit (the skeleton stages the full `Put` every time).
//!
//! [`RoutedMetaBackend::merge_layout_and_size`]:
//!     squeezefs::meta_backend::RoutedMetaBackend::merge_layout_and_size

use squeezefs::layout_wire::{LayoutDelta, LayoutMetadata};
use squeezefs::meta_backend::kv::superblock::{
    SuperblockV3, FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_LAYOUT_DELTAS, SUPERBLOCK_V3_LEN,
};
use squeezefs::meta_backend::kv::{META_KV_LAYOUT_DELTA_COMMITS, META_KV_LAYOUT_FULL_COMMITS};
use squeezefs::meta_backend::{open_routed_meta_set, Metadata, RoutedMetaBackend};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const VOL_LEN: u64 = 256 * 1024 * 1024;
const BLOCK: u64 = 4 * 1024 * 1024;

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

async fn format_meta(path: &Path) {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        VOL_LEN,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
}

/// One writer-shaped publish round: grow the expected layout by one
/// block, then commit it through the delta-bearing publish call exactly
/// as the coalescing merge pass will (delta + always-correct full
/// fallback + absolute size).
async fn publish_block(
    routed: &RoutedMetaBackend,
    ino: u64,
    layout: &mut LayoutMetadata,
    b: u32,
) -> bool {
    let key = format!("oss0://{}", b as u64 * BLOCK);
    layout
        .block_map
        .as_mut()
        .expect("map")
        .insert(b, key.clone());
    layout.size = (b as u64 + 1) * BLOCK;
    let full = bincode::serialize(layout).expect("serialize layout");
    let delta = LayoutDelta::from_final_state(
        &layout.file_type,
        layout.size,
        layout.block_map_id.as_deref(),
        layout.block_prefix.as_deref(),
        layout.file_id.as_deref(),
        layout.data_key.as_deref(),
        vec![(b, key)],
    );
    routed
        .merge_layout_and_size(ino, &delta, &full, layout.size)
        .await
        .expect("publish")
}

fn read_superblock(path: &Path) -> SuperblockV3 {
    let mut buf = vec![0u8; SUPERBLOCK_V3_LEN];
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    f.read_exact(&mut buf).unwrap();
    SuperblockV3::decode_sector(&buf).expect("live superblock decodes")
}

async fn persisted_layout(routed: &RoutedMetaBackend, ino: u64) -> LayoutMetadata {
    let bytes = routed
        .getxattr(ino, "layout")
        .await
        .expect("getxattr")
        .expect("layout present");
    bincode::deserialize::<LayoutMetadata>(&bytes).expect("folded layout decodes as bincode")
}

fn assert_layout_eq(got: &LayoutMetadata, want: &LayoutMetadata, ctx: &str) {
    assert_eq!(got.file_type, want.file_type, "{ctx}: file_type");
    assert_eq!(got.size, want.size, "{ctx}: size");
    assert_eq!(got.block_map_id, want.block_map_id, "{ctx}: block_map_id");
    assert_eq!(got.file_id, want.file_id, "{ctx}: file_id");
    assert_eq!(got.data_key, want.data_key, "{ctx}: data_key");
    assert_eq!(got.block_map, want.block_map, "{ctx}: block_map");
}

/// The lever-2 economy + engagement + correctness contract, one story:
/// a 256-block streamed publish sequence pays O(batch) journal bytes
/// per publish after the first (full-Put) commit, folds back to the
/// exact layout, stamps the incompat bit before the first delta, and
/// survives BOTH a clean-shutdown reopen (bset fold) and a
/// drop-without-shutdown reopen (journal-replay fold).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_layout_and_size_delta_economy_and_equivalence() {
    let _s = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "economy", VOL_LEN);
    format_meta(&meta).await;
    let paths = vec![meta.display().to_string()];
    let routed = open_routed_meta_set(&paths).await.expect("open");

    let inode = routed
        .create(1, "streamed", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    let ino = inode.ino;

    let mut layout = LayoutMetadata {
        file_type: "striped".into(),
        size: 0,
        block_map_id: Some(format!("block_map_{ino}")),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(HashMap::new()),
    };

    const K: u32 = 256;
    let ring = routed.volumes[0].journal_ring();
    let delta_commits_before = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed);
    let full_commits_before = META_KV_LAYOUT_FULL_COMMITS.load(Ordering::Relaxed);

    // First publish: no persisted base exists — MUST be the full Put.
    let used = publish_block(&routed, ino, &mut layout, 0).await;
    assert!(!used, "first-ever persist has no base to fold onto");

    // Streamed publishes: window the journal bytes exactly like the
    // meta-plane audit did.
    let mut windows: Vec<(u32, u64, u64)> = Vec::new(); // (end, entries, bytes)
    let mut prev = (ring.written_entries(), ring.written_bytes());
    for b in 1..K {
        let used = publish_block(&routed, ino, &mut layout, b).await;
        assert!(
            used,
            "publish of block {b} onto a live inline base must stage the \
             O(batch) delta record (lever 2 engagement) — full-Put fallback taken instead"
        );
        if (b + 1) % 64 == 0 {
            let now = (ring.written_entries(), ring.written_bytes());
            windows.push((b + 1, now.0 - prev.0, now.1 - prev.1));
            prev = now;
        }
    }

    let delta_commits = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed) - delta_commits_before;
    let full_commits = META_KV_LAYOUT_FULL_COMMITS.load(Ordering::Relaxed) - full_commits_before;
    assert_eq!(
        delta_commits,
        (K - 1) as u64,
        "every post-base publish must engage the delta path"
    );
    assert_eq!(full_commits, 1, "exactly the first publish goes full");

    println!("== delta-publish journal economy (K={K}) ==");
    println!("window        entries  bytes      bytes/publish");
    let mut lo = 63u32; // first window starts after the full-Put publish
    for (end, entries, bytes) in &windows {
        let n = end - lo;
        println!(
            "({lo:>3},{end:>4}]   {entries:>7}  {bytes:>9}  {:>13.0}",
            *bytes as f64 / n as f64
        );
        lo = *end;
    }

    // THE economy contract: per-publish journal bytes are O(batch) —
    // flat across the stream, never tracking the map size. The last
    // window must stay within 1.5× of the first AND under an absolute
    // 1 KiB/publish bound (delta record + inode Put + entry framing;
    // the pre-campaign representation pays ~5-8 KiB here and grows).
    let first = windows.first().expect("windows");
    let last = windows.last().expect("windows");
    let first_per = first.2 as f64 / 64.0;
    let last_per = last.2 as f64 / 64.0;
    assert!(
        last_per <= 1024.0,
        "per-publish journal bytes must be O(batch): last window {last_per:.0} B/publish \
         (the O(block_map) representation conviction — lever 2 not engaged)"
    );
    assert!(
        last_per <= first_per * 1.5 + 128.0,
        "per-publish journal bytes must not grow with the map: first window \
         {first_per:.0} B/publish, last {last_per:.0}"
    );

    // Correctness: the folded layout equals the writer's intent exactly.
    let got = persisted_layout(&routed, ino).await;
    assert_layout_eq(&got, &layout, "live fold");

    // The incompat ratchet: the bit is on the superblock (stamped before
    // the first delta record could become durable), and a pre-campaign
    // binary refuses the volume loud.
    let sb = read_superblock(&meta);
    assert_ne!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_LAYOUT_DELTAS,
        0,
        "KV_LAYOUT_DELTAS must be stamped once delta records exist"
    );
    let mut raw = vec![0u8; SUPERBLOCK_V3_LEN];
    use std::io::Read;
    std::fs::File::open(&meta)
        .unwrap()
        .read_exact(&mut raw)
        .unwrap();
    let refused = SuperblockV3::decode_sector_with_known(
        &raw,
        FEATURES_INCOMPAT_KNOWN & !FEATURE_INCOMPAT_KV_LAYOUT_DELTAS,
    );
    assert!(
        refused.is_err(),
        "a pre-campaign binary (known mask without bit 5) must refuse the mount loud"
    );

    // Clean-shutdown reopen: the bset-resident chain folds identically.
    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
    drop(routed);
    let reopened = open_routed_meta_set(&paths).await.expect("reopen");
    let got = persisted_layout(&reopened, ino).await;
    assert_layout_eq(&got, &layout, "clean-shutdown remount fold");

    // Replay face: publish more blocks, then drop WITHOUT shutdown — the
    // reopen must replay the delta-bearing journal entries and fold to
    // the same state (whole-tx atomicity: size never leads its map).
    for b in K..K + 16 {
        publish_block(&reopened, ino, &mut layout, b).await;
    }
    drop(reopened);
    let replayed = open_routed_meta_set(&paths).await.expect("replay reopen");
    let got = persisted_layout(&replayed, ino).await;
    assert_layout_eq(&got, &layout, "journal-replay remount fold");
    for vol in &replayed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// Volumes that never stage a delta stay bit-identical: the plain
/// full-`Put` path (`set_layout_and_size`) must NOT ratchet the
/// incompat bit — pre-campaign binaries keep mounting untouched sets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_delta_no_incompat_bit() {
    let _s = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "nobit", VOL_LEN);
    format_meta(&meta).await;
    let paths = vec![meta.display().to_string()];
    let routed = open_routed_meta_set(&paths).await.expect("open");
    let inode = routed
        .create(1, "plain", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    let layout = LayoutMetadata {
        file_type: "striped".into(),
        size: BLOCK,
        block_map_id: Some(format!("block_map_{}", inode.ino)),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(HashMap::from([(0u32, "oss0://0".to_string())])),
    };
    let bytes = bincode::serialize(&layout).unwrap();
    routed
        .set_layout_and_size(inode.ino, &bytes, BLOCK)
        .await
        .expect("full save");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
    let sb = read_superblock(&meta);
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_LAYOUT_DELTAS,
        0,
        "no delta record was staged — the volume must stay bit-identical"
    );
}
