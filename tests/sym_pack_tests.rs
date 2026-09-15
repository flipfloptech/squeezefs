//! Symmetric metadata program, PR 7 — **pack per `(writer, slot, data
//! volume)` and the on-demand refcount probe** (`docs/design-symmetric-
//! metadata.md` §5.4.3 laws 1 and 2, §8 gate 1's packing row, §11;
//! KD-SYM-8).
//!
//! Under the slot-tree forest a block reference lives in its OWNER ino's
//! slot tree; a pack block's tenants are inos of ONE slot, so every
//! reference of the block lives in one slot tree and `refcount(block)`
//! there is exact — one range probe, no seed walk, nothing proportional
//! to a handed-over tree. On a flat or unarmed mount the packer keeps
//! PK2's ONE scope per data volume byte-identical, and the probe answers
//! the whole-volume count exactly as `block_ref_count` does.

mod common;

use common::sym::{
    format_flat_member, format_stamped_member, ino_in_slot, open_under, shutdown, slot_of_global,
    Knobs, SEAM,
};
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::record::{ForestSlot, NATIVE_FOREST_SLOT};
use squeezefs::meta_backend::kv::shared_refs::BLOCK_REF_PROBES;
use squeezefs::meta_backend::Metadata;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const DATA_VOL: &str = "vol-00000000000000a7";

fn taken(tag: u64, block_idx: u64, owner: u64, idx: u32) -> BlockRefOp {
    BlockRefOp::taken(BlockRef {
        vol_tag: tag,
        block_idx,
        owner_ino: owner,
        block_index: idx,
    })
}

/// A minimal striped layout naming `blocks` whole-block keys on the
/// rig's data volume (the wire form the census decodes; the block keys
/// are `vol://offset` on a 4 MiB chunk).
fn layout_bytes(size: u64, keys: &[(u32, u64)]) -> Vec<u8> {
    let mut map = std::collections::HashMap::new();
    for (idx, block_idx) in keys {
        map.insert(
            *idx,
            format!("{DATA_VOL}://{}", block_idx * 4 * 1024 * 1024),
        );
    }
    let layout = squeezefs::layout_wire::LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map: Some(map),
        ..Default::default()
    };
    bincode::serialize(&layout).unwrap()
}

// ---------------------------------------------------------------------------
// §5.4.3 law 1's precondition — the reference lives in the OWNER's tree.
// ---------------------------------------------------------------------------

/// **A PR-1 defect, pinned red on `6f8d44e6`**: the forest codec routes a
/// block reference by the top 24 bits of the owner ino at offset 17 (the
/// LOCAL KEY form `(s+1) << 40 | local`), but the product keys every
/// reference by the owner's GLOBAL ino (the rung-19 law, `refs_owner`),
/// whose top bits are zero for any realistic ino — so through the routed
/// layer every reference landed in the NATIVE slot tree (the manager's),
/// whatever slot its owner lived in. Complete as a census (the union over
/// every tree), wrong for the plane: a publish from any other appender
/// would name the manager's slot (the cross-region refusal / the `Lease`
/// class), and the pack law's per-slot probe reads 0. The routed layer,
/// which owns `route_ino`, now rewrites a forest volume's reference
/// owners to their local key form before the volume stages them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_layout_publish_through_the_routed_layer_lands_its_reference_in_the_owners_slot_tree() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let tag = volume_tag(DATA_VOL);
    let ino = routed
        .create(1, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let slot = slot_of_global(&routed, ino);
    assert_ne!(
        slot, NATIVE_FOREST_SLOT,
        "a child of / mints into a rotor slot"
    );
    routed
        .set_layout_and_size(
            ino,
            &layout_bytes(4 * 1024 * 1024, &[(0, 17)]),
            4 * 1024 * 1024,
            &[taken(tag, 17, ino, 0)],
        )
        .await
        .unwrap();
    let probes_before = BLOCK_REF_PROBES.load(Ordering::Relaxed);
    assert_eq!(
        vol.block_ref_probe(tag, 17, Some(slot)).await.unwrap(),
        1,
        "the owner's slot tree holds the reference"
    );
    assert_eq!(
        vol.block_ref_probe(tag, 17, Some(NATIVE_FOREST_SLOT))
            .await
            .unwrap(),
        0,
        "the manager's native tree holds none of it"
    );
    assert_eq!(vol.block_ref_count(tag, 17).await.unwrap(), 1);
    assert_eq!(BLOCK_REF_PROBES.load(Ordering::Relaxed), probes_before + 2);
    // The record's owner reads back as the LOCAL KEY ino on the forest —
    // the key form the kind-routed contracts (PR 2–4) drive the backend
    // with — so both worlds key one way.
    let (_v, local) = routed.route_ino(ino);
    let refs = vol.block_ref_scan(tag).await.unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].owner_ino, local);
    // A displacing publish releases through the SAME translation: the
    // reference is gone from the owner's tree, nothing strays.
    routed
        .set_layout_and_size(
            ino,
            &layout_bytes(4 * 1024 * 1024, &[(0, 18)]),
            4 * 1024 * 1024,
            &[
                BlockRefOp::released(BlockRef {
                    vol_tag: tag,
                    block_idx: 17,
                    owner_ino: ino,
                    block_index: 0,
                }),
                taken(tag, 18, ino, 0),
            ],
        )
        .await
        .unwrap();
    assert_eq!(vol.block_ref_probe(tag, 17, Some(slot)).await.unwrap(), 0);
    assert_eq!(vol.block_ref_probe(tag, 18, Some(slot)).await.unwrap(), 1);
    assert_eq!(vol.block_ref_count(tag, 17).await.unwrap(), 0);
    shutdown(&routed).await;
}

/// The flat volume takes the ops VERBATIM: the reference keys its GLOBAL
/// owner exactly as before (byte-identical ledger), and the probe on a
/// flat volume is the whole-volume count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flat_volumes_ledger_keys_the_global_owner_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_flat_member(dir.path(), "meta0").await];
    std::env::set_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS", "1");
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_BLOCK_REFS");
    let vol = Arc::clone(&routed.volumes[0]);
    let tag = volume_tag(DATA_VOL);
    let ino = routed
        .create(1, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    if !vol.block_refs_engaged() {
        // A flat volume without bit 9 has no ledger to key (the fresh
        // default format stamps it; the seam engages it in isolation).
        shutdown(&routed).await;
        return;
    }
    routed
        .set_layout_and_size(
            ino,
            &layout_bytes(4 * 1024 * 1024, &[(0, 17)]),
            4 * 1024 * 1024,
            &[taken(tag, 17, ino, 0)],
        )
        .await
        .unwrap();
    let refs = vol.block_ref_scan(tag).await.unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(
        refs[0].owner_ino, ino,
        "the flat ledger keys the GLOBAL ino"
    );
    assert_eq!(vol.block_ref_probe(tag, 17, None).await.unwrap(), 1);
    assert_eq!(
        vol.block_ref_probe(tag, 17, Some(ForestSlot::MAX))
            .await
            .unwrap(),
        1,
        "a slot means nothing on a flat volume: the whole-volume count"
    );
    shutdown(&routed).await;
}

/// A reference committed by the kind-routed contracts' own convention (a
/// LOCAL key owner, straight at the backend) probes in that slot — the
/// two worlds key one way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backend_committed_reference_probes_in_the_slot_its_owner_names() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let tag = volume_tag(DATA_VOL);
    let owner = ino_in_slot(4, 5);
    vol.commit_block_refs(owner, &[taken(tag, 40, owner, 0), taken(tag, 41, owner, 1)])
        .await
        .unwrap();
    assert_eq!(vol.block_ref_probe(tag, 40, Some(4)).await.unwrap(), 1);
    assert_eq!(vol.block_ref_probe(tag, 41, Some(4)).await.unwrap(), 1);
    assert_eq!(vol.block_ref_probe(tag, 40, Some(5)).await.unwrap(), 0);
    assert_eq!(vol.block_ref_probe(tag, 40, None).await.unwrap(), 1);
    shutdown(&routed).await;
}
