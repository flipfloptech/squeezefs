//! Leaf merge — the §4.6a underfull-sibling SMO
//! (`docs/design-cow-kv-metadata.md` §4.6a; the motivating defect is the
//! 2026-09-11 metadata-full fix's stated limitation, `.benchmarks/
//! 2026-09-11-post-123-board-items-2-4.md` §1: the v1 tree never merged,
//! so deletes returned record space INSIDE leaves, never extents — a
//! filled volume resumed creates into the room its deletes freed and hit
//! `ENOSPC` again the moment a new leaf was needed, with most leaves
//! nearly empty).
//!
//! Contracts pinned here:
//!
//! 1. **the motivating one** — fill a small volume to `ENOSPC`, delete
//!    90 % of the files SPREAD across every leaf (no leaf empties on its
//!    own), run checkpoints; creates MUST resume for a population
//!    proportional to the deleted share, and `meta_kv_node_merges` moves.
//!    RED on `a5b2d5fc`: the refill lands the handful of files the
//!    rightmost xattr leaf has room for, then `ENOSPC` once a new leaf is
//!    needed — every other leaf 90 % empty.
//!
//! The sandbox is the `KvMetaBackend` shape of `tests/
//! meta_volume_full_tests.rs`: a small file-backed v3 volume with 64 KiB
//! nodes driven through the `Metadata` trait, so a fill takes seconds.

use squeezefs::error::SqueezefsError;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder, ROOT_INO};
use squeezefs::meta_backend::Metadata;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Backend sandbox (the meta_volume_full_tests shape).
// ---------------------------------------------------------------------------

/// A 24 MiB heap of 64 KiB extents is ≈ 360 leaves; the ring is small so
/// the fill's checkpoints are frequent.
const VOL_LEN: u64 = 24 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
/// Under the 64 KiB node's `node_size/4` value cap (16 KiB): ~4 per leaf,
/// so the xattr tree is the extent-dominant one and every few files split
/// its rightmost leaf.
const PAYLOAD_LEN: usize = 12 * 1024;
const TEST_SEED: u64 = 0x4C4E_4D52_4745_0001;
const TEST_UUID: [u8; 16] = *b"kv-leaf-merge-01";
/// Wall bound on a fill (a wedged-not-failed volume parks its committers).
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

/// One "inline file": a create plus its payload xattr — two commits.
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

/// Unlink + destroy one file on a full volume. A delete whose leaf needs a
/// compaction is admitted down to the compaction floor (§4.7 amended); a
/// burst of them can exhaust that half of the reserve, in which case the
/// refusal is `ENOSPC` and one checkpoint cycle returns the budget —
/// bounded retries, exactly the operator's shape.
async fn delete_file(be: &KvMetaBackend, i: u32) {
    for attempt in 0..16 {
        let unlinked = match be.unlink(ROOT_INO, &name(i)).await {
            Ok(ino) => ino,
            Err(e) if e.to_errno() == libc::ENOSPC => {
                be.checkpoint_now()
                    .await
                    .expect("cycle behind a refused unlink");
                continue;
            }
            Err(e) => panic!("unlink {} failed: {e:?}", name(i)),
        };
        match be.destroy_inode(unlinked).await {
            Ok(()) => return,
            Err(e) if e.to_errno() == libc::ENOSPC && attempt < 15 => {
                be.checkpoint_now()
                    .await
                    .expect("cycle behind a refused destroy");
                // The unlink committed; only the destroy is retried.
                for _ in 0..15 {
                    match be.destroy_inode(unlinked).await {
                        Ok(()) => return,
                        Err(e) if e.to_errno() == libc::ENOSPC => {
                            be.checkpoint_now().await.expect("cycle");
                        }
                        Err(e) => panic!("destroy {} failed: {e:?}", name(i)),
                    }
                }
                panic!("destroy {} refused ENOSPC past the retry bound", name(i));
            }
            Err(e) => panic!("destroy {} failed: {e:?}", name(i)),
        }
    }
    panic!("unlink {} refused ENOSPC past the retry bound", name(i));
}

/// Run checkpoint cycles until the claimable extent count stops growing
/// for `quiet` consecutive cycles (the merge waves return extents one to
/// two barriered cycles after each sweep — §4.6a (d)). Returns the peak
/// free-extent count observed.
async fn cycle_until_quiescent(be: &KvMetaBackend, quiet: u32, max_cycles: u32) -> u64 {
    let mut peak = be.free_extents();
    let mut flat = 0u32;
    for _ in 0..max_cycles {
        be.checkpoint_now().await.expect("checkpoint cycle");
        assert!(
            !be.is_failed(),
            "a recovery cycle must never fail-stop the volume"
        );
        let free = be.free_extents();
        if free > peak {
            peak = free;
            flat = 0;
        } else {
            flat += 1;
            if flat >= quiet {
                break;
            }
        }
    }
    peak
}

// ---------------------------------------------------------------------------
// (1) The motivating contract.
// ---------------------------------------------------------------------------

/// Fill → `ENOSPC` → delete 90 % spread across every leaf → checkpoints →
/// creates resume for a population proportional to the deleted share.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mostly_deleted_full_volume_keeps_creating() {
    let (be, _file) = fresh_volume().await;
    let merges0 = squeezefs::meta_backend::kv::META_KV_NODE_MERGES.load(Ordering::Relaxed);

    let (landed, refusal) = tokio::time::timeout(FILL_BOUND, fill_until_refused(&be, 0))
        .await
        .expect("the fill must reach a refusal within the wall bound");
    assert_eq!(refusal.to_errno(), libc::ENOSPC, "got {refusal:?}");
    assert!(
        landed > 100,
        "a 24 MiB heap must absorb more than {landed} files"
    );
    let free_at_full = be.free_extents();

    // Delete 90 %, SPREAD: every file whose index is not a multiple of 10.
    // Keys are creation-ordered (monotonic inos), so this empties 90 % of
    // EVERY leaf and leaves no leaf empty on its own.
    let mut deleted = 0u32;
    for i in 0..landed {
        if i % 10 == 0 {
            continue;
        }
        delete_file(&be, i).await;
        deleted += 1;
    }
    assert!(!be.is_failed());

    // The recovery: checkpoint cycles run the flush pass, the heap-full
    // merge sweep, and release the merged extents as their entries are
    // covered (§4.6a (d) — waves).
    let peak_free = cycle_until_quiescent(&be, 6, 400).await;

    // Creates resume for a population proportional to the deleted share:
    // the deleted files' leaves were 90 % empty — merging them 3–4:1 into
    // ¾-fill successors returns most of their extents, so at least half
    // the deleted population lands again (a 5 % rightmost-leaf residue is
    // the pre-merge shape).
    let (relanded, second_refusal) =
        tokio::time::timeout(FILL_BOUND, fill_until_refused(&be, 500_000))
            .await
            .expect("the refill must reach a refusal within the wall bound");
    assert_eq!(second_refusal.to_errno(), libc::ENOSPC);
    assert!(!be.is_failed());
    assert!(
        relanded >= deleted / 2,
        "after deleting {deleted} of {landed} files (spread across every leaf) and \
         checkpointing, creates resumed for only {relanded} files — the deletes' \
         extents were not returned (the v1 'never merges' shape: the refill fits the \
         rightmost leaf's room, then ENOSPC at the first new leaf; free extents at \
         full = {free_at_full}, peak after the recovery cycles = {peak_free})"
    );
    let merges = squeezefs::meta_backend::kv::META_KV_NODE_MERGES.load(Ordering::Relaxed) - merges0;
    assert!(
        merges > 0,
        "deleting 90 % of a full volume's files must merge underfull leaves \
         (meta_kv_node_merges is the engagement gauge)"
    );
    assert!(
        peak_free > free_at_full + 8,
        "merges must RETURN extents: free at full {free_at_full}, peak after the \
         recovery cycles {peak_free}"
    );

    // The survivors and the refilled files read back.
    for i in (0..landed).step_by(10).take(20) {
        let f = be
            .lookup(ROOT_INO, &name(i))
            .await
            .expect("survivor resolves");
        let v = be
            .getxattr(f.ino, "user.payload")
            .await
            .expect("getxattr")
            .expect("payload present");
        assert_eq!(v.len(), PAYLOAD_LEN);
        assert!(v.iter().all(|b| *b == (i & 0xFF) as u8));
    }
    for i in [500_000u32, 500_000 + relanded / 2, 500_000 + relanded - 1] {
        let f = be
            .lookup(ROOT_INO, &name(i))
            .await
            .expect("refilled file resolves");
        assert_eq!(
            be.getxattr(f.ino, "user.payload")
                .await
                .unwrap()
                .map(|v| v.len()),
            Some(PAYLOAD_LEN)
        );
    }
    // Every deleted name is gone.
    for i in (1..landed).step_by(97) {
        if i % 10 != 0 {
            assert!(
                be.lookup(ROOT_INO, &name(i)).await.is_err(),
                "deleted {} must not resurface across the merges",
                name(i)
            );
        }
    }
    be.shutdown().await.expect("clean shutdown");
}
