//! Symmetric metadata program, PR 7 — **the shared-block index and the
//! clone protocol** (`docs/design-symmetric-metadata.md` §5.4.3 law 2,
//! §5.4.4, §5.8.5 C16, §11; KD-SYM-8; risk R6).
//!
//! On an ARMED forest set a clone's two inos live in two slot trees, so
//! the block is shared through the index: `MarkShared` at the source's
//! holder (the SHARED bit on its reference, under its 4a guard),
//! `ShareBlock` at the home (both inos' entries), then the cloner's own
//! publish WITH the bit — the ordering law. A terminal free of a SHARED
//! block is never decided locally (`ReleaseShared` at the home decides
//! from the index), and the W1 patch predicate stands down on the mark
//! whatever the count reads. Unarmed, every path is the shipped one and
//! every gauge stays 0.

mod common;

use common::sym::{
    data_file, format_stamped_member, ino_in_slot, mount_data, open_under, shutdown,
    slot_of_global, DataRig, Knobs, DATA_VOL, SEAM,
};
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::record::ForestSlot;
use squeezefs::meta_backend::kv::shared_refs::{
    self, MarkOutcome, SharedIndexDrift, BLOCK_REF_PROBES, MARK_SHARED_CALLS, RELEASE_SHARED_CALLS,
    SHARE_BLOCK_CALLS,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::tempdir;

type Rig = DataRig;

async fn mount(uris: &[String], data: &std::path::Path, knobs: &Knobs) -> Rig {
    mount_data(uris, data, knobs).await
}

/// The SHARED bit on `ino`'s reference to `offset` at `block_index`
/// (`None` = no record).
async fn flag(rig: &Rig, ino: u64, offset: u64, block_index: u32) -> Option<bool> {
    let (_v, local) = rig.routed.route_ino(ino);
    rig.vol()
        .block_ref_flags(&BlockRef {
            vol_tag: rig.tag(),
            block_idx: offset / rig.alloc.chunk_size(),
            owner_ino: local,
            block_index,
        })
        .await
        .unwrap()
}

/// The index home's entries for `offset` — GLOBAL owner inos.
async fn index_owners(rig: &Rig, offset: u64) -> Vec<u64> {
    let mut v: Vec<u64> = rig
        .vol()
        .shared_index_population(rig.tag(), offset / rig.alloc.chunk_size())
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.owner_ino)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

async fn probe(rig: &Rig, offset: u64, slot: Option<ForestSlot>) -> usize {
    rig.vol()
        .block_ref_probe(rig.tag(), offset / rig.alloc.chunk_size(), slot)
        .await
        .unwrap()
}

async fn c16(rig: &Rig) -> Vec<SharedIndexDrift> {
    shared_refs::shared_index_drift(&rig.routed, rig.tag())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// §5.4.4 — the protocol on an armed set.
// ---------------------------------------------------------------------------

/// A clone across two slot trees marks the source SHARED, indexes BOTH
/// inos at the home, publishes the dest's reference with the bit, and
/// every probe agrees: one reference per slot tree, two on the volume,
/// the RAM mark set, C8 and C16 clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_on_an_armed_forest_marks_the_source_shared_and_indexes_both_inos() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    assert!(rig.router.symmetric_armed());
    assert!(rig.router.backend_router.shared_free_gate_armed());
    let src = rig.mk_file("src").await;
    let src_slot = slot_of_global(&rig.routed, src);
    let dst = rig.mk_file_apart("dst", src_slot).await;
    let dst_slot = slot_of_global(&rig.routed, dst);
    let offset = rig.publish_block(src, 0).await;
    assert_eq!(
        flag(&rig, src, offset, 0).await,
        Some(false),
        "a plain publish is unshared"
    );
    assert!(!rig.alloc.is_shared(offset));
    let (marks, shares, probes) = (
        MARK_SHARED_CALLS.load(Ordering::Relaxed),
        SHARE_BLOCK_CALLS.load(Ordering::Relaxed),
        BLOCK_REF_PROBES.load(Ordering::Relaxed),
    );

    rig.clone(src, dst).await.expect("clone");

    assert_eq!(
        flag(&rig, src, offset, 0).await,
        Some(true),
        "step 1: the source's bit"
    );
    assert_eq!(
        flag(&rig, dst, offset, 0).await,
        Some(true),
        "step 3: the cloner's bit"
    );
    assert_eq!(index_owners(&rig, offset).await, {
        let mut v = vec![src, dst];
        v.sort_unstable();
        v
    });
    assert!(
        rig.alloc.is_shared(offset),
        "the RAM mark the W1 predicate reads"
    );
    assert_eq!(rig.alloc.shared_blocks(), 1);
    assert_eq!(rig.alloc.refcount(offset), Some(2), "the clone's pin");
    assert_eq!(
        probe(&rig, offset, Some(src_slot)).await,
        1,
        "one reference per slot tree"
    );
    assert_eq!(probe(&rig, offset, Some(dst_slot)).await, 1);
    assert_eq!(probe(&rig, offset, None).await, 2, "two on the volume");
    assert!(MARK_SHARED_CALLS.load(Ordering::Relaxed) > marks);
    assert!(SHARE_BLOCK_CALLS.load(Ordering::Relaxed) > shares);
    assert!(BLOCK_REF_PROBES.load(Ordering::Relaxed) > probes);
    assert!(rig.drift().await.is_empty(), "C8: durable == derived");
    assert!(
        c16(&rig).await.is_empty(),
        "C16: a healthy clone drifts nothing"
    );
    // The W1 predicate: never in place on a shared block, whatever the
    // count reads (the mark alone refuses — tested by clearing the pin's
    // count down to 1 through the source's later release below).
    assert!(!rig.alloc.begin_patch_sole_owner(offset));
    rig.alloc.publish_block(offset);
    rig.shutdown().await;
}

/// **The pin of R6**: a truncate (the source dropping its last reference)
/// racing a completed clone neither frees nor patches the shared block —
/// the terminal verdict is the home's (`Held`), the RAM count comes down
/// to 1 for the clone's reference alone, the mark stands and the W1
/// predicate refuses on it at count 1; only the clone's own release
/// reaches `Freed` and the ordinary terminal free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_racing_a_truncate_never_frees_or_patches_a_shared_block() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("src").await;
    let dst = rig
        .mk_file_apart("dst", slot_of_global(&rig.routed, src))
        .await;
    let offset = rig.publish_block(src, 0).await;
    rig.clone(src, dst).await.expect("clone");
    let idx = offset / rig.alloc.chunk_size();
    let releases = RELEASE_SHARED_CALLS.load(Ordering::Relaxed);

    // The source's truncate: its map moves to a fresh block, the shared
    // one is displaced — its release rides `free_block_verdict`.
    let fresh = rig.publish_block(src, 0).await;
    assert_ne!(fresh, offset);
    assert_eq!(
        flag(&rig, src, offset, 0).await,
        None,
        "the source's reference is gone"
    );
    assert!(
        RELEASE_SHARED_CALLS.load(Ordering::Relaxed) > releases,
        "the home decided"
    );
    assert_eq!(
        rig.alloc.refcount(offset),
        Some(1),
        "nonterminal: the clone's reference stands"
    );
    assert!(
        !rig.alloc.free_list_contains(idx),
        "never freed under the clone"
    );
    assert!(
        rig.alloc.is_shared(offset),
        "the mark stands while the index names it"
    );
    assert_eq!(
        index_owners(&rig, offset).await,
        vec![dst],
        "the source's entry was GC'd"
    );
    // At count 1 the W1 predicate reads the MARK and refuses in place.
    assert!(
        !rig.alloc.begin_patch_sole_owner(offset),
        "count 1 with the mark set is not sole ownership"
    );
    rig.alloc.publish_block(offset);
    assert_eq!(probe(&rig, offset, None).await, 1);
    assert!(rig.drift().await.is_empty());
    assert!(c16(&rig).await.is_empty());

    // The clone's own release: the last entry goes, the home says Freed,
    // the ordinary terminal free runs, the mark is cleared.
    let fresh2 = rig.publish_block(dst, 0).await;
    assert_ne!(fresh2, offset);
    assert!(rig.alloc.free_list_contains(idx) || rig.alloc.refcount(offset).is_none());
    assert!(!rig.alloc.is_shared(offset));
    assert!(index_owners(&rig, offset).await.is_empty());
    assert_eq!(rig.alloc.shared_blocks(), 0);
    assert!(rig.drift().await.is_empty());
    assert!(c16(&rig).await.is_empty());
    rig.shutdown().await;
}

/// A clone of a clone inherits the bit (round-3 Issue 7): the second
/// cloner's reference is SHARED, the index names all three, and the
/// source's `MarkShared` is answered `Already`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_of_a_clone_inherits_the_shared_bit() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let a = rig.mk_file("a").await;
    let b = rig.mk_file_apart("b", slot_of_global(&rig.routed, a)).await;
    let c = rig.mk_file_apart("c", slot_of_global(&rig.routed, b)).await;
    let offset = rig.publish_block(a, 0).await;
    rig.clone(a, b).await.expect("clone a→b");
    rig.clone(b, c).await.expect("clone b→c");
    assert_eq!(flag(&rig, c, offset, 0).await, Some(true));
    let mut want = vec![a, b, c];
    want.sort_unstable();
    assert_eq!(index_owners(&rig, offset).await, want);
    assert_eq!(rig.alloc.refcount(offset), Some(3));
    assert_eq!(probe(&rig, offset, None).await, 3);
    // Idempotent step 1: marking an already-marked reference writes nothing.
    let (_v, local_b) = rig.routed.route_ino(b);
    let out = rig
        .vol()
        .mark_block_ref_shared(&BlockRef {
            vol_tag: rig.tag(),
            block_idx: offset / rig.alloc.chunk_size(),
            owner_ino: local_b,
            block_index: 0,
        })
        .await
        .unwrap();
    assert_eq!(out, MarkOutcome::Already);
    assert!(rig.drift().await.is_empty());
    assert!(c16(&rig).await.is_empty());
    rig.shutdown().await;
}

// ---------------------------------------------------------------------------
// §5.4.4 — the three crash windows, and C16 on each.
// ---------------------------------------------------------------------------

/// C dies after step 1: a SHARED flag with no clone. C16 reports the flag
/// (report-only), the source's release finds the index empty and the
/// block frees — harmless, as the design says.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cloner_dying_after_mark_shared_leaves_a_harmless_flag_c16_reports() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("src").await;
    let offset = rig.publish_block(src, 0).await;
    let (_v, local) = rig.routed.route_ino(src);
    let r = BlockRef {
        vol_tag: rig.tag(),
        block_idx: offset / rig.alloc.chunk_size(),
        owner_ino: local,
        block_index: 0,
    };
    assert_eq!(
        rig.vol().mark_block_ref_shared(&r).await.unwrap(),
        MarkOutcome::Marked
    );
    rig.alloc.mark_shared(offset);
    let drift = c16(&rig).await;
    assert_eq!(drift.len(), 1);
    assert!(matches!(drift[0], SharedIndexDrift::FlagWithoutEntry(x) if x.owner_ino == src));
    // The mark alone refuses the W1 patch at count 1.
    assert!(!rig.alloc.begin_patch_sole_owner(offset));
    rig.alloc.publish_block(offset);
    // The source's release: the index never named the block → the local
    // verdict — terminal, the block frees, the mark clears.
    let fresh = rig.publish_block(src, 0).await;
    assert_ne!(fresh, offset);
    let idx = offset / rig.alloc.chunk_size();
    assert!(rig.alloc.free_list_contains(idx) || rig.alloc.refcount(offset).is_none());
    assert!(!rig.alloc.is_shared(offset));
    assert!(c16(&rig).await.is_empty(), "the flag went with the record");
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

/// C dies after step 2: index entries for F and G with no reference of
/// G. C16 reports G's entry (report-only); the source's release GC's it
/// at the home and the block frees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cloner_dying_after_share_block_is_reported_by_c16_and_gcd_at_the_release() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("src").await;
    let ghost = rig
        .mk_file_apart("ghost", slot_of_global(&rig.routed, src))
        .await;
    let offset = rig.publish_block(src, 0).await;
    let idx = offset / rig.alloc.chunk_size();
    let (_v, local) = rig.routed.route_ino(src);
    let src_ref = BlockRef {
        vol_tag: rig.tag(),
        block_idx: idx,
        owner_ino: local,
        block_index: 0,
    };
    rig.vol().mark_block_ref_shared(&src_ref).await.unwrap();
    rig.alloc.mark_shared(offset);
    let (inserted, already) = rig
        .vol()
        .share_block(&[
            BlockRef {
                owner_ino: src,
                ..src_ref
            },
            BlockRef {
                owner_ino: ghost,
                ..src_ref
            },
        ])
        .await
        .unwrap();
    assert_eq!((inserted, already), (2, 0));
    // A replay of step 2 writes nothing.
    assert_eq!(
        rig.vol()
            .share_block(&[BlockRef {
                owner_ino: src,
                ..src_ref
            }])
            .await
            .unwrap(),
        (0, 1)
    );
    let drift = c16(&rig).await;
    assert_eq!(drift.len(), 1, "{drift:?}");
    assert!(matches!(drift[0], SharedIndexDrift::EntryWithoutFlag(x) if x.owner_ino == ghost));
    // The source's release: its entry goes with the release, the ghost's
    // is GC'd (no reference behind it) → Freed → the block frees.
    let fresh = rig.publish_block(src, 0).await;
    assert_ne!(fresh, offset);
    assert!(rig.alloc.free_list_contains(idx) || rig.alloc.refcount(offset).is_none());
    assert!(
        index_owners(&rig, offset).await.is_empty(),
        "both entries gone"
    );
    assert!(c16(&rig).await.is_empty());
    assert!(rig.drift().await.is_empty());
    rig.shutdown().await;
}

/// O dies mid-step-1: the cloner retries against the successor lessee and
/// the durable witness (the flag) answers `Already`; a block whose
/// reference is gone answers `Gone`, and a clone whose source map went
/// stale aborts ENOENT-class with its pins undone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mark_shared_is_idempotent_against_the_durable_witness_and_gone_aborts_the_clone() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("src").await;
    let offset = rig.publish_block(src, 0).await;
    let (_v, local) = rig.routed.route_ino(src);
    let r = BlockRef {
        vol_tag: rig.tag(),
        block_idx: offset / rig.alloc.chunk_size(),
        owner_ino: local,
        block_index: 0,
    };
    assert_eq!(
        rig.vol().mark_block_ref_shared(&r).await.unwrap(),
        MarkOutcome::Marked
    );
    assert_eq!(
        rig.vol().mark_block_ref_shared(&r).await.unwrap(),
        MarkOutcome::Already
    );
    let gone = BlockRef {
        block_idx: r.block_idx + 1000,
        ..r
    };
    assert_eq!(
        rig.vol().mark_block_ref_shared(&gone).await.unwrap(),
        MarkOutcome::Gone
    );
    // A batch over one owner answers per reference, in order.
    let outs = rig.vol().mark_block_refs_shared(&[r, gone]).await.unwrap();
    assert_eq!(outs, vec![MarkOutcome::Already, MarkOutcome::Gone]);
    rig.shutdown().await;
}

/// The unarmed forest and the flat mount are the shipped clone verbatim:
/// no mark, no index entry, no gate, every gauge 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unarmed_mount_clones_plain_and_every_gauge_stays_zero() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::unarmed()).await;
    assert!(!rig.router.symmetric_armed());
    assert!(!rig.router.backend_router.shared_free_gate_armed());
    let (marks, shares, releases) = (
        MARK_SHARED_CALLS.load(Ordering::Relaxed),
        SHARE_BLOCK_CALLS.load(Ordering::Relaxed),
        RELEASE_SHARED_CALLS.load(Ordering::Relaxed),
    );
    let src = rig.mk_file("src").await;
    let dst = rig.mk_file("dst").await;
    let offset = rig.publish_block(src, 0).await;
    rig.clone(src, dst).await.expect("clone");
    assert_eq!(flag(&rig, src, offset, 0).await, Some(false));
    assert_eq!(flag(&rig, dst, offset, 0).await, Some(false));
    assert!(!rig.alloc.is_shared(offset));
    assert_eq!(rig.alloc.shared_blocks(), 0);
    assert_eq!(rig.alloc.refcount(offset), Some(2));
    assert!(index_owners(&rig, offset).await.is_empty());
    assert_eq!(MARK_SHARED_CALLS.load(Ordering::Relaxed), marks);
    assert_eq!(SHARE_BLOCK_CALLS.load(Ordering::Relaxed), shares);
    assert_eq!(RELEASE_SHARED_CALLS.load(Ordering::Relaxed), releases);
    assert!(rig.drift().await.is_empty());
    assert_eq!(rig.router.pack_scope_of(src), 0, "PK2's one scope");
    rig.shutdown().await;
}

/// The RAM marks survive a remount: the index is the durable home, the
/// arm seeds the marks from it, and a fresh mount's W1 predicate stands
/// down on the shared block before any clone touches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shared_marks_are_seeded_from_the_index_at_the_arm() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let (offset, src) = {
        let rig = mount(&uris, data.path(), &Knobs::armed()).await;
        let src = rig.mk_file("src").await;
        let dst = rig
            .mk_file_apart("dst", slot_of_global(&rig.routed, src))
            .await;
        let offset = rig.publish_block(src, 0).await;
        rig.clone(src, dst).await.expect("clone");
        rig.shutdown().await;
        (offset, src)
    };
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    assert!(
        rig.alloc.is_shared(offset),
        "seeded from the index at the arm"
    );
    assert_eq!(rig.alloc.shared_blocks(), 1);
    // The durable seed of the RAM count sees both references.
    let seeded = rig
        .router
        .backend_router
        .recover_durable_block_refs(&rig.routed)
        .await
        .unwrap()
        .expect("durable path");
    assert_eq!(seeded, 2);
    assert!(!rig.alloc.begin_patch_sole_owner(offset));
    rig.alloc.publish_block(offset);
    assert_eq!(flag(&rig, src, offset, 0).await, Some(true));
    assert!(rig.drift().await.is_empty());
    assert!(c16(&rig).await.is_empty());
    rig.shutdown().await;
}

/// The probe ≡ the derived census on every block a layout publish
/// touched, on the armed set: for each block, the sum over slot trees of
/// the one-slot probe equals the whole-volume count equals the oracle's
/// derived count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_one_slot_probe_agrees_with_the_derived_census_on_every_published_block() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let mut inos = Vec::new();
    for i in 0..6 {
        inos.push(rig.mk_file(&format!("f{i}")).await);
    }
    let mut offsets = Vec::new();
    for (i, ino) in inos.iter().enumerate() {
        for b in 0..=(i as u32 % 3) {
            offsets.push(rig.publish_block(*ino, b).await);
        }
    }
    let fresh = rig.mk_file("clone-dst").await;
    rig.clone(inos[0], fresh).await.expect("clone");
    let derived = rig
        .alloc
        .derived_block_census(rig.vol(), &rig.router.backend_router)
        .await
        .unwrap();
    for offset in offsets {
        let idx = offset / rig.alloc.chunk_size();
        let whole = probe(&rig, offset, None).await;
        let mut per_slot = 0usize;
        for (slot, _tree) in rig.vol().slot_trees() {
            per_slot += probe(&rig, offset, Some(slot)).await;
        }
        assert_eq!(
            per_slot, whole,
            "block {idx}: Σ slot probes ≡ the volume count"
        );
        assert_eq!(
            whole as u32,
            derived.get(&idx).copied().unwrap_or(0),
            "block {idx}: the probe ≡ the derived census"
        );
    }
    assert!(rig.drift().await.is_empty());
    assert!(c16(&rig).await.is_empty());
    rig.shutdown().await;
}

/// **The W1 predicate's durable clause** (§5.4.3 law 2): on an armed set
/// the sole-owner patch confirms the RAM verdict with ONE probe of the
/// patching ino's slot tree — a second durable reference the RAM map
/// never saw (the shape a handed-over tree leaves behind: its records
/// were committed by a predecessor lessee) refuses the patch; unarmed
/// the clause is absent and the gauge never moves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_w1_predicate_confirms_sole_ownership_in_the_inos_slot_tree_on_an_armed_set() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    {
        let rig = mount(&uris, data.path(), &Knobs::armed()).await;
        let f = rig.mk_file("f").await;
        let offset = rig.publish_block(f, 0).await;
        assert_eq!(rig.alloc.refcount(offset), Some(1));
        let probes = BLOCK_REF_PROBES.load(Ordering::Relaxed);
        assert!(
            rig.router.sole_owner_durably(f, &rig.alloc, offset).await,
            "one durable reference in f's slot tree: the RAM verdict stands"
        );
        assert_eq!(BLOCK_REF_PROBES.load(Ordering::Relaxed), probes + 1);
        // A predecessor's record: another ino of f's SLOT, committed at
        // the backend — the allocator's count is untouched.
        let (_v, local_f) = rig.routed.route_ino(f);
        let slot = slot_of_global(&rig.routed, f);
        assert!(
            slot >= 1,
            "a child of `/` rides the rotor, never the native slot"
        );
        let ghost = ino_in_slot(slot, (local_f & 0xFF_FFFF_FFFF) + 7_777);
        let block_idx = offset / rig.alloc.chunk_size();
        rig.vol()
            .commit_block_refs(
                ghost,
                &[BlockRefOp::taken(BlockRef {
                    vol_tag: rig.tag(),
                    block_idx,
                    owner_ino: ghost,
                    block_index: 0,
                })],
            )
            .await
            .unwrap();
        assert_eq!(rig.alloc.refcount(offset), Some(1), "RAM never saw it");
        assert_eq!(probe(&rig, offset, Some(slot)).await, 2);
        let probes = BLOCK_REF_PROBES.load(Ordering::Relaxed);
        assert!(
            !rig.router.sole_owner_durably(f, &rig.alloc, offset).await,
            "two durable references: the patch is refused whatever RAM reads"
        );
        assert_eq!(BLOCK_REF_PROBES.load(Ordering::Relaxed), probes + 1);
        // **The SHARED-flag clause** (review round 1, Issue 1): a SOLE
        // reference that carries the durable SHARED bit — marked at the
        // BACKEND so the RAM mark this mount's W1 predicate reads stays
        // ABSENT, the shape a mark that landed elsewhere leaves — refuses
        // the patch: the durable flag is the authority, the RAM mark an
        // accelerator.
        let g = rig.mk_file("g").await;
        let g_off = rig.publish_block(g, 0).await;
        let (_v, g_local) = rig.routed.route_ino(g);
        let g_ref = BlockRef {
            vol_tag: rig.tag(),
            block_idx: g_off / rig.alloc.chunk_size(),
            owner_ino: g_local,
            block_index: 0,
        };
        assert!(rig.router.sole_owner_durably(g, &rig.alloc, g_off).await);
        assert_eq!(
            rig.vol().mark_block_ref_shared(&g_ref).await.unwrap(),
            MarkOutcome::Marked
        );
        assert!(!rig.alloc.is_shared(g_off), "premise: no RAM mark");
        assert_eq!(rig.alloc.refcount(g_off), Some(1), "premise: RAM count 1");
        assert!(
            rig.alloc.begin_patch_sole_owner(g_off),
            "premise: the RAM predicate alone would patch"
        );
        rig.alloc.publish_block(g_off);
        assert!(
            !rig.router.sole_owner_durably(g, &rig.alloc, g_off).await,
            "one reference under a durable SHARED bit is not sole ownership"
        );
        rig.shutdown().await;
    }
    {
        // The unarmed half's law is the POSTURE's, on a never-armed
        // volume: a set the plane has stamped takes no writer without the
        // plane (PR 5 review round 3, Issue 25).
        let uris = vec![format_stamped_member(dir.path(), "meta-unarmed").await];
        let rig = mount(&uris, data.path(), &Knobs::unarmed()).await;
        let f = rig.mk_file("u").await;
        let offset = rig.publish_block(f, 0).await;
        let probes = BLOCK_REF_PROBES.load(Ordering::Relaxed);
        assert!(rig.router.sole_owner_durably(f, &rig.alloc, offset).await);
        assert_eq!(
            BLOCK_REF_PROBES.load(Ordering::Relaxed),
            probes,
            "unarmed: no clause, no probe"
        );
        rig.shutdown().await;
    }
}

/// **The staged-source clone under a FULL staging ring** (review round
/// 1, Issue 2 — the shipped FIND-RW5-A arm on an armed set): the clone's
/// refused destination stage escalates to a durable spill — a FRESH
/// block the SOURCE never referenced — so there is nothing to share and
/// the protocol must not run over it (a `MarkShared` on it answered `Gone`
/// and aborted every such clone `ENOENT`, then double-released the landed
/// site). The clone succeeds, reads back exact, the spilled block carries
/// ONE reference (the dest's, unshared), no mark, no index, C8 and C16
/// clean, and every gauge of the protocol is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_staged_source_clone_under_a_full_ring_spills_and_succeeds_on_an_armed_set() {
    use common::sym::{mount_fuse, FuseRig};
    use squeezefs::fuse_client::METRICS;
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    // A 4 MiB ring: one 700 KiB resident source + fillers fill it.
    let rig = mount_fuse(&uris, data.path(), &Knobs::armed(), "4MB").await;
    let img_len = 700 * 1024;
    let (src, src_fid) = rig.resident_staged("spill_src", img_len, 3).await;
    rig.fill_ring(6, img_len, 50).await;
    assert!(
        rig.fs.router.cache.nvme.read_staged(&src_fid).is_some(),
        "premise: the rider keeps the source ring-resident under the fill"
    );
    let dst = rig.create("spill_dst").await;
    let spills = METRICS.staged_spill_escalations.load(Ordering::Relaxed);
    let marks = MARK_SHARED_CALLS.load(Ordering::Relaxed);
    let shares = SHARE_BLOCK_CALLS.load(Ordering::Relaxed);
    rig.clone_whole(src, dst, img_len)
        .await
        .expect("a staged clone never surfaces StorageFull, armed or not");
    assert_eq!(
        METRICS.staged_spill_escalations.load(Ordering::Relaxed),
        spills + 1,
        "premise: the clone took the durable-spill escalation"
    );
    assert_eq!(rig.read(dst, img_len).await, FuseRig::composed(3, img_len));
    let mapping = rig.mapping0(dst).await.expect("the spill landed a mapping");
    let (_be, offset) = rig
        .fs
        .router
        .backend_router
        .split_block_key(squeezefs::routing::clean_block_key_ref(&mapping))
        .expect("the spilled block's offset");
    let alloc = &rig.alloc;
    let block_idx = offset / alloc.chunk_size();
    assert_eq!(
        rig.vol()
            .block_ref_count(rig.tag(), block_idx)
            .await
            .unwrap(),
        1,
        "one durable reference: the dest's (the open pack's own pin is RAM-only)"
    );
    assert!(
        !alloc.is_shared(offset),
        "a fresh copy is nobody's shared block"
    );
    let (_v, dst_local) = rig.routed.route_ino(dst);
    assert_eq!(
        rig.vol()
            .block_ref_flags(&BlockRef {
                vol_tag: rig.tag(),
                block_idx,
                owner_ino: dst_local,
                block_index: 0,
            })
            .await
            .unwrap(),
        Some(false),
        "the dest's reference stands, unshared"
    );
    assert_eq!(MARK_SHARED_CALLS.load(Ordering::Relaxed), marks, "no mark");
    assert_eq!(
        SHARE_BLOCK_CALLS.load(Ordering::Relaxed),
        shares,
        "no index"
    );
    assert!(rig.drift().await.is_empty(), "C8 clean");
    assert!(
        shared_refs::shared_index_drift(&rig.routed, rig.tag())
            .await
            .unwrap()
            .is_empty(),
        "C16 clean"
    );
    rig.shutdown().await;
}

/// **A same-slot clone of an UNSHARED block is the shipped clone** (review
/// round 1, Issue 5): both references live in ONE slot tree, so the
/// population is exact there and the index has nothing to say — no mark,
/// no index entry, no SHARED bit, and the W1 predicate keeps its RAM
/// count law (2 → refuse; after the clone's release, 1 → patch again). A
/// same-slot clone of a SHARED source (a clone of a cross-slot clone) is
/// the other case: the block's population is already cross-slot, so the
/// protocol runs and the new reference inherits the bit and its entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_slot_clone_of_an_unshared_block_runs_no_protocol_and_a_shared_source_does() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    // The static 1 MiB affinity ceiling (PR 4's fixture): a fresh
    // directory's children bind to its slot.
    let rig = mount(&uris, data.path(), &Knobs::armed().affinity_mb("1")).await;
    let d = rig.mk_dir("d").await;
    let a = rig.mk_file_in(d, "a").await;
    let b = rig.mk_file_in(d, "b").await;
    assert_eq!(
        slot_of_global(&rig.routed, a),
        slot_of_global(&rig.routed, b),
        "premise: a fresh directory's children share its slot"
    );
    let offset = rig.publish_block(a, 0).await;
    let marks = MARK_SHARED_CALLS.load(Ordering::Relaxed);
    let shares = SHARE_BLOCK_CALLS.load(Ordering::Relaxed);
    rig.clone(a, b).await.expect("same-slot clone");
    assert_eq!(MARK_SHARED_CALLS.load(Ordering::Relaxed), marks, "no mark");
    assert_eq!(
        SHARE_BLOCK_CALLS.load(Ordering::Relaxed),
        shares,
        "no index"
    );
    assert_eq!(flag(&rig, a, offset, 0).await, Some(false));
    assert_eq!(flag(&rig, b, offset, 0).await, Some(false));
    assert!(index_owners(&rig, offset).await.is_empty());
    assert!(!rig.alloc.is_shared(offset));
    assert_eq!(rig.alloc.refcount(offset), Some(2));
    assert_eq!(
        probe(&rig, offset, Some(slot_of_global(&rig.routed, a))).await,
        2
    );
    // The W1 law on the same-slot pair: two references refuse; the
    // clone's truncate-to-zero releases its reference (the layout tx) and
    // frees its RAM pin — the durable count reads 1 and W1 is back.
    assert!(!rig.router.sole_owner_durably(a, &rig.alloc, offset).await);
    rig.router
        .truncate_layout(b, 0, rig.token(b))
        .await
        .expect("truncate the clone to zero");
    assert_eq!(rig.alloc.refcount(offset), Some(1));
    assert!(
        rig.router.sole_owner_durably(a, &rig.alloc, offset).await,
        "the surviving same-slot owner regains W1 — nothing was ever marked"
    );
    assert!(rig.drift().await.is_empty());
    // A cross-slot clone marks a; a further SAME-slot clone of a (a → c,
    // c beside a) inherits the sharing: the block's population is already
    // cross-slot, so c is marked, indexed and flagged.
    let far = rig
        .mk_file_apart("far", slot_of_global(&rig.routed, a))
        .await;
    rig.clone(a, far).await.expect("cross-slot clone");
    assert_eq!(flag(&rig, a, offset, 0).await, Some(true));
    let c = rig.mk_file_in(d, "c").await;
    assert_eq!(
        slot_of_global(&rig.routed, c),
        slot_of_global(&rig.routed, a)
    );
    let shares = SHARE_BLOCK_CALLS.load(Ordering::Relaxed);
    rig.clone(a, c)
        .await
        .expect("same-slot clone of a shared source");
    assert!(
        SHARE_BLOCK_CALLS.load(Ordering::Relaxed) > shares,
        "indexed"
    );
    assert_eq!(flag(&rig, c, offset, 0).await, Some(true));
    assert_eq!(index_owners(&rig, offset).await, {
        let mut v = vec![a, far, c];
        v.sort_unstable();
        v
    });
    assert!(rig.drift().await.is_empty());
    assert!(c16(&rig).await.is_empty());
    rig.shutdown().await;
}

/// Drain ring 0's USER admission budget into `held` (every byte a user
/// commit or an index entry would need is taken), returning the holds.
fn hold_user_budget(
    vol: &squeezefs::meta_backend::kv::backend::KvMetaBackend,
) -> Vec<squeezefs::meta_backend::kv::journal_core::Admission> {
    use squeezefs::meta_backend::kv::journal_core::AdmissionClass;
    let ring = vol.journal_ring();
    let mut held = Vec::new();
    for chunk in [256 * 1024u64, 16 * 1024, 1024, 64] {
        while let Some(a) = ring.try_admit(chunk, AdmissionClass::User) {
            held.push(a);
        }
    }
    held
}

/// **A full ring 0 PARKS the index writers, never fails them** (review
/// round 1, Issue 3): `ShareBlock` on the USER clone path and
/// `ReleaseShared` on the FREE path admit their control entry PARKING,
/// before the verb mutex (PR 4's door law) — under a ring whose user
/// budget is entirely held, a clone waits for the budget and completes,
/// and a shared block's release waits and lands (the reference released,
/// the closure held), where a `Try` admission failed the clone and leaked
/// the reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_ring_0_parks_the_index_writers_and_never_fails_or_leaks() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("src").await;
    let dst = rig
        .mk_file_apart("dst", slot_of_global(&rig.routed, src))
        .await;
    let offset = rig.publish_block(src, 0).await;
    let stalls = rig.vol().journal_full_stalls();
    // The clone under a held-full ring: its `ShareBlock` (and its layout
    // commit) park; the budget returns 400 ms later; the clone completes.
    let held = hold_user_budget(rig.vol());
    assert!(!held.is_empty(), "premise: the user budget was drainable");
    let releaser = {
        let vol = Arc::clone(rig.vol());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            for a in held {
                vol.journal_ring().core().release(a);
            }
        })
    };
    let t0 = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(30), rig.clone(src, dst))
        .await
        .expect("a parked clone is never stranded")
        .expect("a clone under a busy ring completes after admission — never fails");
    releaser.await.unwrap();
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(300),
        "the clone waited for the budget (parked), not failed fast"
    );
    assert!(
        rig.vol().journal_full_stalls() > stalls,
        "premise: ring 0 parked"
    );
    assert_eq!(flag(&rig, src, offset, 0).await, Some(true));
    assert_eq!(flag(&rig, dst, offset, 0).await, Some(true));
    assert_eq!(index_owners(&rig, offset).await.len(), 2);
    assert_eq!(rig.alloc.refcount(offset), Some(2));
    // The index writer ITSELF under a held-full ring (the clone above
    // reaches it only after its own parked commit returned the budget):
    // `ShareBlock` parks on its admission and lands — never a `Try`
    // refusal.
    let third = rig.mk_file("third").await;
    let held = hold_user_budget(rig.vol());
    assert!(!held.is_empty());
    let releaser = {
        let vol = Arc::clone(rig.vol());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            for a in held {
                vol.journal_ring().core().release(a);
            }
        })
    };
    let t1 = std::time::Instant::now();
    let shared = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        rig.vol().share_block(&[BlockRef {
            vol_tag: rig.tag(),
            block_idx: offset / rig.alloc.chunk_size(),
            owner_ino: third,
            block_index: 0,
        }]),
    )
    .await
    .expect("a parked ShareBlock is never stranded")
    .expect("ShareBlock under a full ring parks, never fails");
    releaser.await.unwrap();
    assert_eq!(shared, (1, 0));
    assert!(
        t1.elapsed() >= std::time::Duration::from_millis(300),
        "the index write waited for the budget (parked), not failed fast"
    );
    assert_eq!(index_owners(&rig, offset).await.len(), 3);
    // The FREE path under a held-full ring: the release parks and lands —
    // the reference is released (`Held { remaining }` at the home), never
    // leaked, and the closure `RAM count == entries the home keeps` holds.
    let held = hold_user_budget(rig.vol());
    assert!(!held.is_empty());
    let releaser = {
        let vol = Arc::clone(rig.vol());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            for a in held {
                vol.journal_ring().core().release(a);
            }
        })
    };
    let releases = RELEASE_SHARED_CALLS.load(Ordering::Relaxed);
    let failures = shared_refs::SHARED_RELEASE_FAILURES.load(Ordering::Relaxed);
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        rig.router
            .backend_router
            .free_block_verdict(&offset.to_string()),
    )
    .await
    .expect("a parked release is never stranded")
    .expect("a shared block's release under a busy ring lands — never fails");
    releaser.await.unwrap();
    assert!(!terminal, "the clone still stands: nonterminal");
    assert_eq!(RELEASE_SHARED_CALLS.load(Ordering::Relaxed), releases + 1);
    assert_eq!(
        shared_refs::SHARED_RELEASE_FAILURES.load(Ordering::Relaxed),
        failures,
        "no discarded failure"
    );
    assert_eq!(
        rig.alloc.refcount(offset),
        Some(1),
        "the reference was released"
    );
    assert!(rig.drift().await.is_empty());
    assert!(c16(&rig).await.is_empty());
    rig.shutdown().await;
}

/// **A legal passthrough truncate-shrink keeps the durable SHARED bit**
/// (review round 1, Issue 4): the clip re-describes the SAME reference
/// (`bk:0:len → bk:0:len'`), which arrived as a released + a taken op
/// over one record and re-Put it from scratch — flags 0 — so a healthy
/// clone-then-shrink tripped C16 (`EntryWithoutFlag`) for ever. Now the
/// pair cancels: the record survives with its bit, both inos stay SHARED
/// and indexed, C8/C16 clean, and the source's later release still ships
/// to the home (`Held`, never a local terminal free).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_truncate_shrink_of_a_clone_shared_tenant_keeps_the_durable_shared_bit() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("tenant").await;
    let dst = rig
        .mk_file_apart("clone", slot_of_global(&rig.routed, src))
        .await;
    // A promoted tenant: a staged layout whose `block_map[0]` is the
    // size-carrying mapping `offset:0:65536` of a block this ino owns.
    let offset = rig.alloc.allocate_block().await.expect("allocate");
    rig.alloc.publish_block(offset);
    let block_idx = offset / rig.alloc.chunk_size();
    let len = 64 * 1024u64;
    let layout = squeezefs::layout_wire::LayoutMetadata {
        file_type: "staged".into(),
        size: len,
        file_id: Some("tenant-src".into()),
        block_map: Some(std::collections::HashMap::from([(
            0u32,
            format!("{offset}:0:{len}"),
        )])),
        ..Default::default()
    };
    rig.routed
        .set_layout_and_size(
            src,
            &bincode::serialize(&layout).unwrap(),
            len,
            &[BlockRefOp::taken(BlockRef {
                vol_tag: rig.tag(),
                block_idx,
                owner_ino: src,
                block_index: 0,
            })],
        )
        .await
        .unwrap();
    rig.clone(src, dst)
        .await
        .expect("clone the promoted tenant");
    assert_eq!(flag(&rig, src, offset, 0).await, Some(true));
    assert_eq!(flag(&rig, dst, offset, 0).await, Some(true));
    assert_eq!(rig.alloc.refcount(offset), Some(2));
    // The passthrough shrink: one re-description, no reference moves.
    rig.router
        .truncate_layout(src, len / 2, rig.token(src))
        .await
        .expect("truncate-shrink");
    let clipped = rig
        .router
        .fetch_metadata(&squeezefs::keys::inode_path(src))
        .await
        .unwrap();
    assert_eq!(
        clipped.block_map.as_ref().and_then(|m| m.get(&0).cloned()),
        Some(format!("{offset}:0:{}", len / 2)),
        "premise: the mapping was re-described in place"
    );
    assert_eq!(
        flag(&rig, src, offset, 0).await,
        Some(true),
        "the re-described reference keeps its SHARED bit"
    );
    assert_eq!(flag(&rig, dst, offset, 0).await, Some(true));
    assert_eq!(index_owners(&rig, offset).await, {
        let mut v = vec![src, dst];
        v.sort_unstable();
        v
    });
    assert!(rig.drift().await.is_empty(), "C8 clean");
    assert!(c16(&rig).await.is_empty(), "C16 clean after a legal shrink");
    assert_eq!(rig.alloc.refcount(offset), Some(2), "no reference moved");
    // The source's release ships to the home: `Held` (the clone stands),
    // the block stays allocated and marked.
    let releases = RELEASE_SHARED_CALLS.load(Ordering::Relaxed);
    let terminal = rig
        .router
        .backend_router
        .free_block_verdict(&format!("{offset}:0:{}", len / 2))
        .await
        .unwrap();
    assert!(
        !terminal,
        "a shared block's release is never locally terminal"
    );
    assert_eq!(RELEASE_SHARED_CALLS.load(Ordering::Relaxed), releases + 1);
    assert_eq!(rig.alloc.refcount(offset), Some(1));
    assert!(rig.alloc.is_shared(offset));
    rig.shutdown().await;
}

/// The three verbs ride the manager wire (the S8 listener, the volume
/// ordinal dispatch) and answer the same durable outcomes the local
/// executors do — `ManagerClient::{mark_shared, share_block,
/// release_shared}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_clone_verbs_ride_the_manager_wire() {
    use squeezefs::cluster_wire as cw;
    use squeezefs::data_grant::AsyncVerbRouter;
    use squeezefs::meta_ship::manager::{ManagerClient, ManagerReply, ManagerSetService};
    const SECRET: &[u8] = b"pr7-wire-secret";
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let src = rig.mk_file("src").await;
    let offset = rig.publish_block(src, 0).await;
    let idx = offset / rig.alloc.chunk_size();
    let (_v, local) = rig.routed.route_ino(src);
    let host = cw::RpcListener::start_async(
        cw::RpcListenerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
            service_threads: 2,
            ..cw::RpcListenerConfig::default()
        },
        SECRET.to_vec(),
        Arc::new(AsyncVerbRouter::new().with_manager(ManagerSetService::new(&rig.routed.volumes))),
    )
    .unwrap();
    let endpoint = host.endpoint().to_string();
    let mut client = ManagerClient::connect(&endpoint, SECRET, "pr7-cloner", 0)
        .await
        .expect("dial");
    let reply = client
        .mark_shared(BlockRef {
            vol_tag: rig.tag(),
            block_idx: idx,
            owner_ino: local,
            block_index: 0,
        })
        .await
        .unwrap();
    assert_eq!(reply, ManagerReply::Marked { already: false });
    // The served mark sets the RAM mark on the serving mount beside the
    // durable bit (the W1 accelerator; the durable flag is the authority).
    assert!(
        rig.alloc.is_shared(offset),
        "the wire mark reaches the RAM mark"
    );
    let reply = client
        .mark_shared(BlockRef {
            vol_tag: rig.tag(),
            block_idx: idx + 999,
            owner_ino: local,
            block_index: 0,
        })
        .await
        .unwrap();
    assert_eq!(reply, ManagerReply::SharedGone);
    // The served `ShareBlock` SCREENS its words against durable state
    // (review round 1, Issue 17): an ino without a SHARED reference to the
    // block rejects the WHOLE frame — nothing indexed, counted on
    // `manager_verb_rejected`.
    let ghost = rig.mk_file("ghost").await;
    let rejected = |rig: &Rig| {
        rig.vol()
            .appender_stats()
            .expect("armed")
            .manager_verb_rejected
    };
    let rejected_before = rejected(&rig);
    assert!(
        client
            .share_block(rig.tag(), idx, &[(src, 0), (ghost, 0)])
            .await
            .is_err(),
        "an ino holding no reference to the block is rejected"
    );
    assert_eq!(rejected(&rig), rejected_before + 1);
    assert!(
        index_owners(&rig, offset).await.is_empty(),
        "nothing written"
    );
    // The ghost takes an UNSHARED reference: still rejected (MarkShared
    // precedes ShareBlock).
    let (_gv, ghost_local) = rig.routed.route_ino(ghost);
    let ghost_ref = BlockRef {
        vol_tag: rig.tag(),
        block_idx: idx,
        owner_ino: ghost_local,
        block_index: 0,
    };
    rig.vol()
        .commit_block_refs(ghost_local, &[BlockRefOp::taken(ghost_ref)])
        .await
        .unwrap();
    assert!(client
        .share_block(rig.tag(), idx, &[(src, 0), (ghost, 0)])
        .await
        .is_err());
    // Marked over the wire too: the frame's every word confirmed, indexed.
    assert_eq!(
        client.mark_shared(ghost_ref).await.unwrap(),
        ManagerReply::Marked { already: false }
    );
    assert_eq!(
        client
            .share_block(rig.tag(), idx, &[(src, 0), (ghost, 0)])
            .await
            .unwrap(),
        (2, 0)
    );
    assert_eq!(index_owners(&rig, offset).await.len(), 2);
    // The wire release of the ghost's ONE entry — keyed `(ino, block_index)`
    // (Issue 9): one remains (the source's).
    assert_eq!(
        client
            .release_shared(rig.tag(), idx, Some((ghost, 0)))
            .await
            .unwrap(),
        (true, 1)
    );
    // An unindexed block answers "not shared".
    assert_eq!(
        client
            .release_shared(rig.tag(), idx + 5, None)
            .await
            .unwrap(),
        (false, 0)
    );
    host.shutdown();
    rig.shutdown().await;
}

/// A backend-driven reference in a guest slot and the index home's
/// records share one volume: the index never appears in a slot tree's
/// refs probe and a refs probe never counts an index entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_index_lives_in_tree0_and_never_pollutes_a_slot_trees_probe() {
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let tag = volume_tag(DATA_VOL);
    let owner = ino_in_slot(4, 9);
    vol.commit_block_refs(
        owner,
        &[BlockRefOp::taken(BlockRef {
            vol_tag: tag,
            block_idx: 77,
            owner_ino: owner,
            block_index: 0,
        })],
    )
    .await
    .unwrap();
    vol.share_block(&[BlockRef {
        vol_tag: tag,
        block_idx: 77,
        owner_ino: 12345,
        block_index: 0,
    }])
    .await
    .unwrap();
    assert_eq!(vol.block_ref_probe(tag, 77, Some(4)).await.unwrap(), 1);
    assert_eq!(vol.block_ref_probe(tag, 77, None).await.unwrap(), 1);
    assert_eq!(vol.shared_index_population(tag, 77).await.unwrap().len(), 1);
    assert_eq!(vol.shared_index_scan(tag).await.unwrap().len(), 1);
    shutdown(&routed).await;
}

/// **PR 13 — the W1 ladders decline a NON-HOLDER's patch as the counted
/// posture DECISION** (the 2026-08-19 co-writer clause's joiner face): a
/// JOINED appender's allocator is grant-armed for a data volume whose
/// ALLOCATION LEASE this mount does not hold, so the ownership plane the
/// patch's incarnation retire accounts is another daemon's —
/// `BlockAllocator::holds_ownership_plane` answers `false`, and the
/// durable clause answers `SoleOwnerVerdict::NonHolder` BEFORE any probe
/// and before the incarnation word is retired. Before it, the ladders ran
/// on into `begin_patch_sole_owner`, whose `plane_gate` refused with one
/// ERROR line + one `cowriter_accounting_refusals` per eligible overwrite
/// (the sym-walls rewrite row: a burst per joiner). The shipped
/// allocator (no grant arm) and the holder keep the `Sole` verdict.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_w1_ladders_decline_a_non_holders_patch_as_a_counted_posture_decision() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::meta_backend::kv::alloc_lease;
    use squeezefs::meta_ship::manager::WireIdentity;
    use squeezefs::routing::SoleOwnerVerdict;
    let dir = tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let data = data_file();
    let rig = mount(&uris, data.path(), &Knobs::armed()).await;
    let f = rig.mk_file("f").await;
    let offset = rig.publish_block(f, 0).await;
    assert!(
        rig.alloc.holds_ownership_plane(),
        "an allocator without a grant arm takes the shipped arms: it holds its plane"
    );
    assert_eq!(
        rig.router.sole_owner_verdict(f, &rig.alloc, offset).await,
        SoleOwnerVerdict::Sole
    );
    // A JOINED appender's allocator for a data volume nobody in this
    // process holds the allocation lease of: grant-armed through the
    // wire sink (the venue is never dialed — the verdict is decided
    // before any ask).
    let joiner_vol = "vol-pr13-joiner-data";
    let joiner_tag = volume_tag(joiner_vol);
    assert!(
        alloc_lease::holding(joiner_tag).is_none(),
        "premise: no allocation lease for the joiner's volume here"
    );
    let non_holder = Arc::new(BlockAllocator::new(joiner_vol).await.unwrap());
    non_holder.set_capacity_bytes(4096 * non_holder.chunk_size());
    assert!(non_holder.install_block_grant_arm(
        joiner_tag,
        alloc_lease::wire_block_grant_sink(
            alloc_lease::HolderVenue::fixed("127.0.0.1:1".to_string()),
            vec![7u8; 32],
            WireIdentity {
                node_token: 0x5150_1313,
                mount_slot: 3,
                writer_id: 0x13,
            },
            0,
            joiner_tag,
        ),
    ));
    assert!(
        !non_holder.holds_ownership_plane(),
        "grant-armed, lease held elsewhere: the plane is another daemon's"
    );
    let probes = BLOCK_REF_PROBES.load(Ordering::Relaxed);
    let refusals = squeezefs::fuse_client::METRICS
        .cowriter_accounting_refusals
        .load(Ordering::Relaxed);
    assert_eq!(
        rig.router.sole_owner_verdict(f, &non_holder, offset).await,
        SoleOwnerVerdict::NonHolder,
        "the ladder's counted decision, not the gate's refusal"
    );
    assert_eq!(
        BLOCK_REF_PROBES.load(Ordering::Relaxed),
        probes,
        "decided before any durable probe"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .cowriter_accounting_refusals
            .load(Ordering::Relaxed),
        refusals,
        "the allocator's ERROR-logging gate was never reached"
    );
    // The gate itself stays the defense-in-depth arm: reached directly it
    // still refuses (and counts) — which is exactly why the ladders decide
    // upstream.
    assert!(!non_holder.begin_patch_sole_owner(offset));
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .cowriter_accounting_refusals
            .load(Ordering::Relaxed),
        refusals + 1
    );
    shutdown(&rig.routed).await;
}
