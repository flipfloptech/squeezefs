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
    data_file, format_flat_member, format_stamped_member, format_stamped_set, ino_in_slot,
    mount_data, open_under, shutdown, slot_of_global, Knobs, SEAM,
};
use squeezefs::fuse_client::METRICS;
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

// ---------------------------------------------------------------------------
// §5.4.3 law 1 — pack per `(writer, slot, data volume)`.
// ---------------------------------------------------------------------------

/// On an ARMED set the packer keys its open packs by the tenant's slot:
/// two tenants of two slots reserve in two DIFFERENT pack blocks, two of
/// one slot in ONE; the dismount seal reports one sealed block per open
/// scope (`pack_blocks_sealed_dismount` — the design's seal residue, ≤
/// the leased slots being written). Unarmed, every tenant's scope is 0 and
/// PK2's one open pack per data volume stands byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_set_packs_per_slot_and_an_unarmed_mount_keeps_pk2s_one_scope() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    // Armed: per-slot scopes.
    {
        let rig = mount_data(&uris, data.path(), &Knobs::armed()).await;
        let a = rig.mk_file("a").await;
        let b = rig.mk_file_apart("b", slot_of_global(&rig.routed, a)).await;
        let a2 = {
            // A second tenant of a's slot: the affinity policy puts a
            // child of a directory in a's slot there; here the simplest
            // same-slot tenant is a itself at a second block index — the
            // packer's scope is the ino's slot, so one ino's two tenants
            // share a scope by construction.
            a
        };
        let sa = rig.router.pack_scope_of(a);
        let sb = rig.router.pack_scope_of(b);
        assert_eq!(
            sa,
            squeezefs::routing::pack_scope_key(0, slot_of_global(&rig.routed, a)),
            "the scope is (meta volume, forest slot)"
        );
        assert_ne!(sa, sb, "two slots, two scopes");
        assert_eq!(rig.router.pack_scope_of(a2), sa);
        let slot = squeezefs::routing::pack_slot_len(16 * 1024);
        let t_a = rig
            .router
            .packer
            .reserve(&rig.router.backend_router, slot, sa)
            .await
            .unwrap()
            .expect("the arm is open");
        let t_a2 = rig
            .router
            .packer
            .reserve(&rig.router.backend_router, slot, sa)
            .await
            .unwrap()
            .expect("the arm is open");
        let t_b = rig
            .router
            .packer
            .reserve(&rig.router.backend_router, slot, sb)
            .await
            .unwrap()
            .expect("the arm is open");
        assert_eq!(
            t_a.base_key(),
            t_a2.base_key(),
            "one slot's tenants share one pack block"
        );
        assert_ne!(
            t_a.base_key(),
            t_b.base_key(),
            "two slots' tenants never share a block (every reference of a pack block lives in \
             ONE slot tree)"
        );
        assert_eq!(rig.router.packer.open_scopes(), 2);
        let before = METRICS.pack_blocks_sealed_dismount.load(Ordering::Relaxed);
        drop((t_a, t_a2, t_b));
        let sealed = rig.router.seal_open_packs().await;
        assert_eq!(sealed, 2, "one sealed block per open scope");
        assert_eq!(
            METRICS.pack_blocks_sealed_dismount.load(Ordering::Relaxed),
            before + 2
        );
        assert_eq!(rig.router.packer.open_scopes(), 0);
        rig.shutdown().await;
    }
    // Unarmed — the `--single-writer` class, the one unarmed writable
    // posture since PR 14: ONE scope — two inos' tenants share one block
    // (PK2's law verbatim).
    {
        let uris = vec![format_flat_member(dir.path(), "meta-flat").await];
        let rig = mount_data(&uris, data.path(), &Knobs::unarmed()).await;
        let a = rig.mk_file("ua").await;
        let b = rig.mk_file("ub").await;
        assert_eq!(rig.router.pack_scope_of(a), 0);
        assert_eq!(rig.router.pack_scope_of(b), 0);
        let slot = squeezefs::routing::pack_slot_len(16 * 1024);
        let t_a = rig
            .router
            .packer
            .reserve(&rig.router.backend_router, slot, 0)
            .await
            .unwrap()
            .expect("open");
        let t_b = rig
            .router
            .packer
            .reserve(&rig.router.backend_router, slot, 0)
            .await
            .unwrap()
            .expect("open");
        assert_eq!(
            t_a.base_key(),
            t_b.base_key(),
            "PK2: one open pack per data volume"
        );
        assert_eq!(rig.router.packer.open_scopes(), 1);
        let before = METRICS.pack_blocks_sealed_dismount.load(Ordering::Relaxed);
        drop((t_a, t_b));
        assert_eq!(rig.router.seal_open_packs().await, 1);
        assert_eq!(
            METRICS.pack_blocks_sealed_dismount.load(Ordering::Relaxed),
            before + 1
        );
        rig.shutdown().await;
    }
}

/// The NATIVE forest slot is `0` on EVERY meta volume (a converted set's
/// pre-arm inos sit there on each), so a scope of the slot alone would
/// pool two volumes' natives into one pack block whose references live in
/// two slot trees — law 1 broken. The scope carries the meta volume: two
/// native inos of two volumes take two scopes, two native inos of one
/// volume one (review round 1, Issue 7).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_meta_volumes_native_inos_never_share_a_pack_scope() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = format_stamped_set(dir.path(), &["meta0", "meta1"]).await;
    let data = data_file();
    let rig = mount_data(&uris, data.path(), &Knobs::armed()).await;
    assert_eq!(rig.routed.volumes.len(), 2);
    // A raw local ino (no guest bits) IS a volume's native-slot ino.
    let n0 = rig.routed.make_global_ino(9, 0);
    let n0b = rig.routed.make_global_ino(10, 0);
    let n1 = rig.routed.make_global_ino(9, 1);
    assert_eq!(rig.routed.route_ino(n0).0, 0);
    assert_eq!(rig.routed.route_ino(n1).0, 1);
    assert_eq!(slot_of_global(&rig.routed, n0), NATIVE_FOREST_SLOT);
    assert_eq!(slot_of_global(&rig.routed, n1), NATIVE_FOREST_SLOT);
    let (s0, s0b, s1) = (
        rig.router.pack_scope_of(n0),
        rig.router.pack_scope_of(n0b),
        rig.router.pack_scope_of(n1),
    );
    assert_eq!(s0, s0b, "one volume's natives: one scope");
    assert_ne!(s0, s1, "two volumes' natives: two scopes");
    assert_eq!(
        s0,
        squeezefs::routing::pack_scope_key(0, NATIVE_FOREST_SLOT)
    );
    assert_eq!(
        s1,
        squeezefs::routing::pack_scope_key(1, NATIVE_FOREST_SLOT)
    );
    rig.shutdown().await;
}

/// **The design's pin** (`a_pack_block_whose_slot_is_handed_over_keeps_its_
/// whole_population_in_one_tree`): a pack block's tenants are inos of ONE
/// slot, so its references all live in that slot's tree — the one-slot
/// probe reads the whole population before a handover, after it (the tree
/// moved with the lease, the references with the tree), and across a
/// crash-remount; no other slot tree holds a reference to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pack_block_whose_slot_is_handed_over_keeps_its_whole_population_in_one_tree() {
    use squeezefs::meta_backend::kv::backend::AcquireSlotReply;
    const PARTITION: &str = "1:4";
    const SLOT4: ForestSlot = 4;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag(DATA_VOL);
    let block = 4242u64;
    let tenants = [
        ino_in_slot(SLOT4, 5),
        ino_in_slot(SLOT4, 6),
        ino_in_slot(SLOT4, 7),
    ];
    {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        assert!(vol.slot_leases().unwrap().gate.is_leased(SLOT4));
        // Three tenants of one pack block — one reference each, by the
        // tenants' own commits (the packed publish's C8 record shape).
        for (i, t) in tenants.iter().enumerate() {
            vol.commit_block_refs(*t, &[taken(tag, block, *t, i as u32)])
                .await
                .unwrap();
        }
        assert_eq!(
            vol.block_ref_probe(tag, block, Some(SLOT4)).await.unwrap(),
            3
        );
        assert_eq!(vol.block_ref_count(tag, block).await.unwrap(), 3);
        for (slot, _) in vol.slot_trees() {
            if slot != SLOT4 {
                assert_eq!(
                    vol.block_ref_probe(tag, block, Some(slot)).await.unwrap(),
                    0,
                    "slot {slot} holds none of the pack's references"
                );
            }
        }
        // The handover: appender 1 offers slot 4 to the manager, which
        // accepts — the tree (and every reference in it) moves with the
        // lease (PR 4's flush-then-transfer).
        vol.manager_offer_slot(1, SLOT4, 0).await.unwrap();
        let AcquireSlotReply::Granted(grant) = vol.manager_acquire_slot(0, SLOT4).await.unwrap()
        else {
            panic!("the offer's accept grants");
        };
        assert_eq!(grant.g, 2);
        assert_eq!(
            vol.block_ref_probe(tag, block, Some(SLOT4)).await.unwrap(),
            3,
            "the whole population, in the one tree, after the handover"
        );
        assert_eq!(vol.block_ref_count(tag, block).await.unwrap(), 3);
        // A tenant's release after the handover rides the new lessee's
        // ring and the same tree.
        vol.commit_block_refs(
            tenants[0],
            &[BlockRefOp::released(BlockRef {
                vol_tag: tag,
                block_idx: block,
                owner_ino: tenants[0],
                block_index: 0,
            })],
        )
        .await
        .unwrap();
        assert_eq!(
            vol.block_ref_probe(tag, block, Some(SLOT4)).await.unwrap(),
            2
        );
        vol.sync_device().await.unwrap();
        drop(vol);
        drop(routed); // the crash: no leave
    }
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        vol.block_ref_probe(tag, block, Some(SLOT4)).await.unwrap(),
        2,
        "the population survives the crash in the one tree"
    );
    assert_eq!(vol.block_ref_count(tag, block).await.unwrap(), 2);
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// §5.7.5 — gather mode.
// ---------------------------------------------------------------------------

/// `setfattr -n user.squeezefs.gather -v 1 <dir>`: the directory's
/// children mint into ITS slot — past the affinity ceiling too, where a
/// plain directory's children spill to the rotor — counted on
/// `dir_gather_mints`; the mint law `affinity + rotor + gather ≡ mints`
/// closes; the xattr crosses the FUSE screen by name and nothing else in
/// the reserved family does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gather_mode_directorys_children_live_in_its_slot_past_the_cap_and_never_automatically() {
    use squeezefs::meta_backend::kv::backend::xattr_name_allowed;
    assert!(xattr_name_allowed(squeezefs::GATHER_XATTR));
    assert!(!xattr_name_allowed("user.squeezefs.gatherX"));
    assert!(!xattr_name_allowed("user.squeezefs.format_config"));
    assert!(squeezefs::gather_xattr_opts_in(b"1"));
    assert!(squeezefs::gather_xattr_opts_in(b" true\n"));
    assert!(!squeezefs::gather_xattr_opts_in(b"0"));
    assert!(!squeezefs::gather_xattr_opts_in(b"maybe"));

    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    // The static 1 MiB ceiling (PR 4's spill fixture): affinity binds fast.
    let routed = open_under(&uris, &Knobs::armed().affinity_mb("1")).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    let stats = |v: &squeezefs::meta_backend::kv::backend::KvMetaBackend| {
        v.slot_lease_stats().expect("armed")
    };
    let mut mints = 0u64;
    let gather = routed
        .create(1, "gather", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    mints += 1;
    let plain = routed
        .create(1, "plain", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    mints += 1;
    let gather_slot = slot_of_global(&routed, gather);
    let plain_slot = slot_of_global(&routed, plain);
    assert_eq!(stats(&vol).dir_gather_mints, 0);
    routed
        .setxattr(gather, squeezefs::GATHER_XATTR, b"1")
        .await
        .unwrap();
    assert_eq!(
        routed
            .getxattr(gather, squeezefs::GATHER_XATTR)
            .await
            .unwrap()
            .as_deref(),
        Some(&b"1"[..])
    );
    // Both trees past the cap: a plain child spills, a gather child stays.
    plane.extents.set(gather_slot, 17);
    plane.extents.set(plain_slot, 17);
    let spills_before = stats(&vol).ceiling_spills;
    for i in 0..5 {
        let c = routed
            .create(gather, &format!("g{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        mints += 1;
        assert_eq!(
            slot_of_global(&routed, c),
            gather_slot,
            "a gather child lives in the directory's slot, cap or no cap"
        );
    }
    let p = routed
        .create(plain, "p0", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    mints += 1;
    assert_ne!(
        slot_of_global(&routed, p),
        plain_slot,
        "never automatic: a directory without the opt-in spills past the cap"
    );
    let s = stats(&vol);
    assert_eq!(s.dir_gather_mints, 5);
    assert_eq!(s.ceiling_spills, spills_before + 1);
    assert_eq!(
        s.affinity_mints + s.rotor_mints + s.dir_gather_mints,
        mints,
        "the mint law closes: affinity {} + rotor {} + gather {} ≡ {mints}",
        s.affinity_mints,
        s.rotor_mints,
        s.dir_gather_mints
    );
    // The opt-out: remove the xattr and the ordinary policy decides again.
    routed
        .removexattr(gather, squeezefs::GATHER_XATTR)
        .await
        .unwrap();
    let c = routed
        .create(gather, "after", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    mints += 1;
    assert_ne!(
        slot_of_global(&routed, c),
        gather_slot,
        "past the cap, the rotor"
    );
    let s = stats(&vol);
    assert_eq!(s.dir_gather_mints, 5);
    assert_eq!(s.affinity_mints + s.rotor_mints + s.dir_gather_mints, mints);
    shutdown(&routed).await;
}

/// An unarmed mount (the `--single-writer` class since PR 14) never reads
/// the opt-in: children of a gather directory take the shared rotor
/// exactly as before and the gauge stays 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unarmed_mount_ignores_the_gather_xattr_and_the_gauge_stays_zero() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_flat_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(vol.slot_lease_stats().is_none(), "no plane unarmed");
    let gather = routed
        .create(1, "gather", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    routed
        .setxattr(gather, squeezefs::GATHER_XATTR, b"1")
        .await
        .unwrap();
    let gather_slot = slot_of_global(&routed, gather);
    let mut slots = std::collections::BTreeSet::new();
    for i in 0..8 {
        let c = routed
            .create(gather, &format!("g{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        slots.insert(slot_of_global(&routed, c));
    }
    assert!(
        slots.len() > 1 || !slots.contains(&gather_slot),
        "the shared rotor spreads the children: {slots:?}"
    );
    shutdown(&routed).await;
}
