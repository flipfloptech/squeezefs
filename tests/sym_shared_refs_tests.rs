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
    let ghost = rig.mk_file("ghost").await;
    assert_eq!(
        client
            .share_block(rig.tag(), idx, &[(src, 0), (ghost, 0)])
            .await
            .unwrap(),
        (2, 0)
    );
    assert_eq!(index_owners(&rig, offset).await.len(), 2);
    // The wire release of the ghost's entry: one remains (the source's).
    assert_eq!(
        client
            .release_shared(rig.tag(), idx, Some(ghost))
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
