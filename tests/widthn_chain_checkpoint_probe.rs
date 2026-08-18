//! DLM S11 rung 19 — the chain-durability contract the width-N rows
//! exercise at fleet speed, deterministic: a deep chained-merge chain
//! (bare + volume-prefixed key forms, the live wire's mix) with forced
//! checkpoints between links must never lose a map entry, and the
//! durable block-reference ledger must mirror the final composed map
//! exactly. The checkpoint interleave rides the same compaction plane
//! the lineage rule (`compact_fold`'s retained link) protects.

use squeezefs::layout_wire::LayoutDelta;
use squeezefs::meta_backend::kv::block_refs::{self, BlockRef};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, RoutedMetaBackend};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const VOL_LEN: u64 = 256 * 1024 * 1024;
const TEST_TAG: u64 = 0x5157_4944_5448_4e02;

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

async fn sandbox(dir: &Path) -> (Arc<RoutedMetaBackend>, PathBuf) {
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let p = make_file(dir, "probe-meta0", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &p,
        VOL_LEN,
        &opts(),
        plan.stamps[0].clone(),
    )
    .await
    .expect("format meta volume");
    sb::set_layout_versions_bit(&p).await.expect("bit 15");
    sb::set_block_refcounts_bit(&p)
        .await
        .expect("bit 9 (ledger)");
    let routed = open_routed_meta_set(&[p.display().to_string()])
        .await
        .expect("open routed set");
    (routed, p)
}

fn base_layout_bytes(size: u64, map: &[(u32, &str)]) -> Vec<u8> {
    bincode::serialize(&squeezefs::layout_wire::LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map.iter().map(|(b, k)| (*b, k.to_string())).collect()),
    })
    .expect("serialize base layout")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deep_chained_chain_with_checkpoints_loses_nothing() {
    let dir = TempDir::new().unwrap();
    let (be, _p) = sandbox(dir.path()).await;
    block_refs::install_block_ref_resolver(Arc::new(|key: &str, ino: u64, idx: u32| {
        Some(BlockRef {
            vol_tag: TEST_TAG,
            block_idx: block_refs::volume_tag(key),
            owner_ino: ino,
            block_index: idx,
        })
    }));
    let inode = be
        .create(1, "probe.bin", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    let ino = inode.ino;

    // The bare base (a truncate-created striped file: empty map).
    be.set_layout_and_size(ino, &base_layout_bytes(0, &[]), 0, &[])
        .await
        .expect("base Put");

    // 64 chained links, one entry each, mirroring the live key forms
    // (bare + volume-prefixed alternating), a forced checkpoint every 8.
    let mut want: Vec<(u32, String)> = Vec::new();
    for i in 0u32..64 {
        let key = if i % 2 == 0 {
            format!("{}", 4194304u64 * u64::from(i))
        } else {
            format!("volB://{}", 4194304u64 * u64::from(i))
        };
        let mut d = LayoutDelta::from_final_state(
            "striped",
            u64::from(i + 1) * 4194304,
            None,
            None,
            None,
            None,
            vec![(i, key.clone())],
        );
        d.set_versions(0, squeezefs::dlm::mint_layout_version());
        let full = base_layout_bytes(
            u64::from(i + 1) * 4194304,
            &want
                .iter()
                .map(|(b, k)| (*b, k.as_str()))
                .chain([(i, key.as_str())])
                .collect::<Vec<_>>(),
        );
        let refs = vec![squeezefs::meta_backend::kv::block_refs::BlockRefOp::taken(
            BlockRef {
                vol_tag: TEST_TAG,
                block_idx: block_refs::volume_tag(&key),
                owner_ino: ino,
                block_index: i,
            },
        )];
        let (_used, _v) = be
            .merge_layout_and_size_chained(ino, &d, bytes::Bytes::from(full), 0, refs)
            .await
            .expect("chained link lands");
        want.push((i, key));
        if i % 8 == 7 {
            for vol in &be.volumes {
                vol.checkpoint_now().await.expect("checkpoint");
            }
        }
    }

    // The composed map holds EVERY link's entry.
    use squeezefs::meta_backend::Metadata as _;
    let raw = be
        .getxattr(ino, "layout")
        .await
        .expect("layout read")
        .expect("layout present");
    let map = squeezefs::layout_wire::decode_base_layout(&raw)
        .expect("decodable layout")
        .block_map
        .expect("striped map");
    let mut missing: Vec<u32> = Vec::new();
    for (b, k) in &want {
        if map.get(b).map(String::as_str) != Some(k.as_str()) {
            missing.push(*b);
        }
    }
    assert!(
        missing.is_empty(),
        "the composed map lost entries across checkpointed chain growth: {missing:?} \
         (map has {} of {})",
        map.len(),
        want.len()
    );

    // The ledger mirrors the map exactly.
    let mut ledger: Vec<(u64, u32)> = Vec::new();
    for kv in &be.volumes {
        for r in kv.block_ref_scan(TEST_TAG).await.expect("scan") {
            ledger.push((r.block_idx, r.block_index));
        }
    }
    ledger.sort_unstable();
    let mut expect: Vec<(u64, u32)> = want
        .iter()
        .map(|(b, k)| (block_refs::volume_tag(k), *b))
        .collect();
    expect.sort_unstable();
    assert_eq!(ledger, expect, "the ledger mirrors the composed map");

    block_refs::uninstall_block_ref_resolver();
    for vol in &be.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}
