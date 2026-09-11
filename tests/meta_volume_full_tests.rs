//! A FULL metadata volume answers `ENOSPC` — never a fail-stop
//! (`.benchmarks/2026-09-09-inline-raise-sweep-local.md` §3, P1;
//! design-cow-kv-metadata §4.7 "ENOSPC semantics" + the wedged-tail bound's
//! two classes).
//!
//! The field shape: 11 k × 32 KiB inline files filled a 1 GiB metadata
//! volume; the checkpoint's flush pass answered `metadata heap exhausted
//! (free=0, reserve=80) … compaction deferred` on twelve nodes, the §4.7
//! wedged-tail audit read the space standstill as the corruption class it
//! was built for, marked the volume FAILED, and every later op returned
//! EIO (`writeback error latched`). A full volume is neither corruption
//! nor fencing: the outcome is `ENOSPC` on the metadata plane.
//!
//! Contracts pinned here (the `KvMetaBackend` sandbox shape the KV suites
//! use — a small file-backed v3 volume driven through the `Metadata`
//! trait):
//!
//! - **(a) the refusal is ENOSPC**: filling the heap with commits refuses
//!   with the no-space class → `libc::ENOSPC` through the errno mapping the
//!   FUSE layer uses (`SqueezefsError::to_errno`), never EIO/EINVAL;
//! - **(b) never FAILED**: the fail-stop latch stays clear across the fill,
//!   the refusals, the deletes and the remount — the wedged-tail terminal
//!   never fires on a space standstill;
//! - **(c) reads keep working, deletes make progress**: every existing
//!   file resolves and its payload reads back; unlinks + destroys commit on
//!   the full volume; after deletes and a checkpoint, new creates succeed
//!   again (the record space the deletes freed inside the leaves is
//!   reusable — the v1 tree never shrinks, so extents do not return, see
//!   §4.7);
//! - **(d) the ledger closes**: `pending_free` drains to 0 and the
//!   checkpoint keeps advancing after the deletes;
//! - **(e) a remount of the full volume mounts, reads and can delete.**
//!
//! Plus the audit-split contract: a flush pass that defers nodes because
//! the allocator answered `NoSpace` is the SPACE class — counted on
//! `heap_full_cycles`, loud, non-terminal — and clears when the budget
//! returns; the FAILED terminal is reserved for the wedge class (nothing
//! deferred for space, retirements parked, tail not advancing).

use squeezefs::error::SqueezefsError;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder, ROOT_INO};
use squeezefs::meta_backend::kv::record::TREE_XATTRS;
use squeezefs::meta_backend::kv::tree::{
    test_smo_build_pause_release, TEST_SMO_BUILD_PAUSED, TEST_SMO_BUILD_PAUSE_TREE,
};
use squeezefs::meta_backend::kv::KvError;
use squeezefs::meta_backend::Metadata;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

/// Small nodes and a small ring keep the fill in the seconds range: a
/// 24 MiB heap of 64 KiB extents is ≈ 380 leaves.
const VOL_LEN: u64 = 24 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
/// Under the 64 KiB node's `node_size/4` value cap (16 KiB): ~4 per leaf,
/// so every few files split the xattr tree's rightmost leaf.
const PAYLOAD_LEN: usize = 12 * 1024;
const TEST_SEED: u64 = 0x0F1F_2F3F_4F5F_6F7F;
const TEST_UUID: [u8; 16] = *b"meta-volume-full";
/// Wall bound on the fill (the red shape wedged behind ring parks).
const FILL_BOUND: Duration = Duration::from_secs(240);

async fn fresh_volume() -> (Arc<KvMetaBackend>, NamedTempFile) {
    let _ = env_logger::builder().is_test(true).try_init();
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    let cfg = BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    };
    ImageBuilder::new(cfg)
        .unwrap()
        .build(file.path(), VOL_LEN)
        .await
        .expect("build empty v3 image");
    let be = KvMetaBackend::open(file.path()).await.expect("mount v3");
    (be, file)
}

fn name(i: u32) -> String {
    format!("f{i:06}")
}

/// One "inline file": a create plus its payload xattr — two commits, the
/// shape the sweep's 32 KiB inline row drove.
async fn put_file(be: &KvMetaBackend, i: u32) -> Result<(), SqueezefsError> {
    let f = be
        .create(ROOT_INO, &name(i), libc::S_IFREG | 0o644, 0, 0)
        .await?;
    be.setxattr(f.ino, "user.payload", &vec![(i & 0xFF) as u8; PAYLOAD_LEN])
        .await
}

/// Fill until the first refusal; returns `(files landed, the refusal)`.
async fn fill_until_refused(be: &KvMetaBackend, from: u32) -> (u32, SqueezefsError) {
    let mut i = from;
    loop {
        match put_file(be, i).await {
            Ok(()) => i += 1,
            Err(e) => return (i - from, e),
        }
        assert!(
            i - from < 100_000,
            "a 24 MiB metadata volume absorbed 100k × 12 KiB payloads — harness bug"
        );
        assert!(
            !be.is_failed(),
            "the volume fail-stopped mid-fill (a full heap must never mark the volume \
             FAILED — §4.7)"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_metadata_volume_is_enospc_not_a_failstop() {
    let (be, file) = fresh_volume().await;
    let checkpoints0 = squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed);

    // ---- (a) the refusal class.
    let (landed, first_refusal) = tokio::time::timeout(FILL_BOUND, fill_until_refused(&be, 0))
        .await
        .expect(
            "the fill must reach a refusal (or ENOSPC) within the wall bound — a \
                 wedged-not-failed volume parks its committers forever",
        );
    assert!(
        landed > 50,
        "a 24 MiB heap must absorb more than {landed} files"
    );
    assert_eq!(
        first_refusal.to_errno(),
        libc::ENOSPC,
        "a full metadata volume must refuse with ENOSPC, got {first_refusal:?} (errno {})",
        first_refusal.to_errno()
    );
    assert!(
        be.heap_full(),
        "the heap-full posture word must be set once a growth commit was refused for space"
    );
    assert!(
        be.enospc_refusals() >= 1,
        "every ENOSPC refusal is counted on the volume"
    );

    // ---- (b) never FAILED — and more refusals stay ENOSPC, not EIO. (A
    // probe may land when a cycle returned a promise's surplus; those
    // files are the newest and go with the deletes below.)
    assert!(!be.is_failed(), "a full heap is not a fail-stop class");
    let mut probes_landed: Vec<u32> = Vec::new();
    for i in 0..8 {
        match put_file(&be, 900_000 + i).await {
            Ok(()) => probes_landed.push(900_000 + i),
            Err(e) => assert_eq!(
                e.to_errno(),
                libc::ENOSPC,
                "repeated refusals on a full volume stay ENOSPC (got {e:?})"
            ),
        }
        assert!(!be.is_failed());
    }

    // ---- (c) reads keep working.
    for i in [0u32, landed / 3, landed - 1] {
        let f = be
            .lookup(ROOT_INO, &name(i))
            .await
            .expect("landed files resolve on a full volume");
        let attr = be.getattr(f.ino).await.expect("getattr on a full volume");
        assert_eq!(attr.ino, f.ino);
        let v = be
            .getxattr(f.ino, "user.payload")
            .await
            .expect("getxattr on a full volume")
            .expect("payload present");
        assert_eq!(v.len(), PAYLOAD_LEN);
        assert!(v.iter().all(|b| *b == (i & 0xFF) as u8));
    }

    // ---- (c) deletes make progress on the full volume: the newest third
    // (their records sit in the rightmost leaves, where growth lands).
    let delete_from = landed - landed / 3;
    for i in (delete_from..landed).chain(probes_landed) {
        let ino = be
            .unlink(ROOT_INO, &name(i))
            .await
            .unwrap_or_else(|e| panic!("unlink must commit on a full volume: {e:?}"));
        be.destroy_inode(ino)
            .await
            .unwrap_or_else(|e| panic!("destroy must commit on a full volume: {e:?}"));
        assert!(!be.is_failed());
    }
    // The compaction that turns the deletes into leaf room rides the
    // checkpoint's flush pass; the next barrier releases the retirements.
    for _ in 0..3 {
        be.checkpoint_now()
            .await
            .expect("checkpoint cycles run on a full volume");
    }

    // ---- (d) the ledger closes: retirements drained, promises consumed.
    assert_eq!(
        be.pending_free_extents(),
        0,
        "pending-free retirements must drain once the deletes' compactions barriered"
    );
    assert_eq!(
        be.heap_promised(),
        0,
        "every promised SMO ran (or its remainder fit in place) — the ledger is 0 at quiesce"
    );
    assert!(
        squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed) > checkpoints0,
        "the checkpoint keeps advancing on a full volume"
    );
    assert!(!be.is_failed());

    // ---- (c) new creates succeed again: the room the deletes freed in the
    // rightmost leaves is reusable (extents do not return — the tree never
    // shrinks — so the SECOND refusal is again ENOSPC, never a fail-stop).
    let (relanded, second_refusal) =
        tokio::time::timeout(FILL_BOUND, fill_until_refused(&be, 500_000))
            .await
            .expect("the refill must reach a refusal within the wall bound");
    assert!(
        relanded >= 1,
        "after deleting {} files and checkpointing, at least one create+payload must land \
         (got {relanded}); the deletes' record space was not recovered",
        landed - delete_from
    );
    assert_eq!(second_refusal.to_errno(), libc::ENOSPC);
    assert!(!be.is_failed());
    // Reads still serve after the second fill.
    let f = be.lookup(ROOT_INO, &name(500_000)).await.unwrap();
    assert_eq!(
        be.getxattr(f.ino, "user.payload")
            .await
            .unwrap()
            .map(|v| v.len()),
        Some(PAYLOAD_LEN)
    );

    // ---- (e) remount of the full volume.
    be.shutdown()
        .await
        .expect("clean shutdown of a full volume");
    drop(be);
    let re = KvMetaBackend::open(file.path())
        .await
        .expect("a full metadata volume mounts");
    assert!(!re.is_failed());
    let f = re
        .lookup(ROOT_INO, &name(0))
        .await
        .expect("remount: landed files resolve");
    assert_eq!(
        re.getxattr(f.ino, "user.payload")
            .await
            .unwrap()
            .map(|v| v.len()),
        Some(PAYLOAD_LEN),
        "remount: payloads read back"
    );
    let ino = re
        .unlink(ROOT_INO, &name(1))
        .await
        .expect("remount: unlink commits on a full volume");
    re.destroy_inode(ino)
        .await
        .expect("remount: destroy commits on a full volume");
    re.checkpoint_now().await.expect("remount: checkpoint");
    assert!(
        !re.is_failed(),
        "the remounted full volume is not a fail-stop class"
    );
    re.shutdown().await.unwrap();
}

/// The wedged-tail audit's two classes (design-smo-replay-currency PR 4
/// clause b, amended): a barriered cycle whose flush pass DEFERRED a node
/// for `NoSpace` is a SPACE standstill — counted, loud, never terminal —
/// and the FAILED terminal is reserved for the wedge class.
///
/// Driven deterministically through the SMO build-pause seam: an SMO on
/// the xattr tree is parked in its build window (the SMO mutex held, so
/// no flush can run), growth commits keep landing in RAM behind it, then
/// the heap is drained to zero through the allocator itself — a foreign
/// claimant taking the budget those commits were admitted against, the
/// residual the audit split exists for. Every later flush pass needs SMOs
/// it cannot claim; more than `PENDING_FREE_FORCE_CYCLES` barriered
/// cycles run; the volume must stay un-failed, count the cycles, and
/// recover once the budget returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn space_standstill_is_counted_not_terminal_and_clears_when_budget_returns() {
    // Slow the cadence (the kv_smo_crash_completeness precedent): the
    // cycles below are the ones this test drives; the shutdown's final
    // tick bounds the wait.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
            test_smo_build_pause_release();
            *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = None;
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "2000");
    let _cleanup = Cleanup;

    let (be, _file) = fresh_volume().await;
    for i in 0..8 {
        put_file(&be, i).await.expect("warm-up creates");
    }
    be.checkpoint_now().await.expect("warm-up checkpoint");

    // Park the next xattr-tree SMO in its build window.
    TEST_SMO_BUILD_PAUSE_TREE.store(u64::from(TREE_XATTRS), Ordering::SeqCst);
    let mut i = 8u32;
    let mut parked = false;
    while !parked && i < 400 {
        put_file(&be, i)
            .await
            .expect("creates while arming the seam");
        i += 1;
        for _ in 0..50 {
            if TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex").is_some() {
                parked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    assert!(parked, "an xattr-tree SMO must park in its build window");
    // Behind the parked SMO: growth admitted into RAM with the budget
    // still present (the leaves it lands on will need SMOs at the flush).
    for j in 0..24 {
        put_file(&be, 10_000 + j)
            .await
            .expect("growth commits land in RAM behind the parked SMO");
    }
    // The foreign claimant: drain every claimable extent.
    let alloc = Arc::clone(be.allocator());
    let mut held: Vec<u64> = Vec::new();
    loop {
        match alloc.claim_internal() {
            Ok(ext) => held.push(ext),
            Err(KvError::NoSpace { .. }) => break,
            Err(e) => panic!("unexpected claim error: {e:?}"),
        }
    }
    assert!(
        held.len() > 8,
        "the drain must take the whole heap ({} claimed)",
        held.len()
    );
    assert_eq!(alloc.free_extents(), 0);
    test_smo_build_pause_release();
    *TEST_SMO_BUILD_PAUSED.lock().expect("seam mutex") = None;

    // Well past the wedge bound: every cycle defers for space.
    let cycles0 = be.heap_full_cycles();
    for _ in 0..12 {
        be.checkpoint_now()
            .await
            .expect("a space-deferring cycle completes (skip-and-defer, then barrier)");
        assert!(
            !be.is_failed(),
            "a space standstill (heap drained, compactions deferred) must never fire the \
             wedged-tail FAILED terminal"
        );
    }
    assert!(
        be.heap_full_cycles() > cycles0,
        "cycles that deferred a node for NoSpace are counted as the space class"
    );
    assert!(
        be.heap_full(),
        "a deferred-for-space cycle latches the heap-full posture"
    );
    // Growth is refused ENOSPC meanwhile — never EIO.
    match put_file(&be, 20_000).await {
        Ok(()) => {}
        Err(e) => assert_eq!(e.to_errno(), libc::ENOSPC, "got {e:?}"),
    }
    assert!(!be.is_failed());

    // Return the budget: the next cycles flush what was deferred, the
    // retirements drain and the posture clears.
    for ext in held {
        alloc.release_unpublished(ext);
    }
    for _ in 0..3 {
        be.checkpoint_now()
            .await
            .expect("cycles with the budget returned");
    }
    assert!(!be.is_failed());
    assert!(
        !be.heap_full(),
        "the heap-full posture clears once the budget returned and a cycle flushed"
    );
    assert_eq!(be.pending_free_extents(), 0, "the ledger closes");
    put_file(&be, 30_000)
        .await
        .expect("growth resumes once the budget returned");
    be.shutdown().await.unwrap();
}

/// The heap admission's derivations, tie-tested so drift is red
/// (`tests/derivation_sweep_tests.rs`'s convention): the compaction floor
/// is HALF the §4.7 reserve — never a constant of its own — and the SMO
/// extent projection follows the SMO's own geometry: one for a fold that
/// fits, else the greedy ¾-fill part count plus the packer's slack.
#[test]
fn heap_admission_floors_derive_from_the_reserve_and_the_split_geometry() {
    use squeezefs::meta_backend::kv::alloc_ext::{
        compaction_floor_extents, compaction_reserve_extents,
    };
    use squeezefs::meta_backend::kv::node::{NodeLayout, DEFAULT_NODE_SIZE};
    // The field shape: 1 GiB volume of 256 KiB extents ⇒ 4096-extent heap,
    // 2 % reserve = 81, compaction floor 40. The floor shape: the 8-extent
    // reserve floor ⇒ compaction floor 4.
    assert_eq!(compaction_reserve_extents(4096), 81);
    assert_eq!(
        compaction_floor_extents(compaction_reserve_extents(4096)),
        40
    );
    assert_eq!(compaction_floor_extents(8), 4);
    assert_eq!(compaction_floor_extents(0), 0);
    for total in [64u64, 4096, 65_536, 1 << 20] {
        let reserve = compaction_reserve_extents(total);
        assert_eq!(compaction_floor_extents(reserve), reserve / 2);
    }

    let layout = NodeLayout::new(DEFAULT_NODE_SIZE).unwrap();
    let cap = layout.fold_capacity();
    assert_eq!(cap, DEFAULT_NODE_SIZE - 4096 - 32 - 32);
    assert_eq!(layout.split_part_capacity(), cap * 3 / 4);
    // A fold that fits is one compaction extent whatever the packer
    // says; one byte over is the smallest split: its two parts plus the
    // rounding and cascade extents.
    assert_eq!(layout.smo_extents_for_parts(0, 1), 1);
    assert_eq!(layout.smo_extents_for_parts(cap, 1), 1);
    assert_eq!(layout.smo_extents_for_parts(cap + 1, 2), 4);
    assert_eq!(layout.smo_extents_for_parts(cap * 3, 14), 16);
    // The growth window a split promise absorbs without a re-walk is what
    // a NEW greedy part needs at minimum: the part budget less the largest
    // record the layout admits.
    assert_eq!(
        layout.split_growth_window(),
        layout.split_part_capacity() - layout.record_value_cap()
    );
    assert!(layout.split_growth_window() > layout.record_value_cap());
    // The append frame is the bset frame's geometry: headers + bytes,
    // page-aligned; nothing pending costs nothing.
    assert_eq!(layout.append_frame_len(0), 0);
    assert_eq!(layout.append_frame_len(1), 4096);
    assert_eq!(layout.append_frame_len(4096 - 64), 4096);
    assert_eq!(layout.append_frame_len(4096 - 63), 8192);
}

/// The no-space class carries `ENOSPC` structurally through every mapping
/// the FUSE layer uses (POSIX-6): the typed KV refusal and the crate
/// error it converts to.
#[test]
fn kv_no_space_maps_to_enospc_structurally() {
    let e: SqueezefsError = KvError::NoSpace {
        free: 3,
        reserve: 8,
    }
    .into();
    assert_eq!(e.to_errno(), libc::ENOSPC);
    assert!(
        e.to_string().contains("no space"),
        "the message stays prose ({e})"
    );
}
