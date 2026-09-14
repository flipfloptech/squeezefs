//! **The `rename` lock hole** (orchestrator-routed from PR 11 §5b; found
//! on every layout — SHIPPED, layout-independent; fixed under the flat
//! law's shipped-bug-fix exception in PR 4 review round 2, Issue 1).
//!
//! `RoutedMetaBackend::rename` locked exactly the two parents' `I{}` and
//! `D{}` keys, THEN discovered the moved child and the overwrite victim
//! and staged a `Delta` on `I{moved}` / a `Put` on `I{dest}` under guards
//! that never named them. A concurrent `set_layout_and_size` on the moved
//! file — which holds `I{ino}` and stages a `Put` of the same inode key —
//! therefore CO-QUEUED with the rename in one conveyor batch: the pass's
//! same-key exclusion `debug_assert!` fired (the panic sentinel failed the
//! batch — EIO to both callers in a debug build), and in release the two
//! records applied in queue order, a `Put` behind the rename's `Delta`
//! overwriting the rename's ctime (a lost ctime — POSIX-visible).
//!
//! The lock law (`src/stripe_locks.rs`, the unlink path's two-phase
//! shape): a guard-less lookup of both children FIRST, then ONE canonical
//! `lock_many` per volume over `{I{old_parent}, I{new_parent}, I{moved},
//! I{dest}}` + the two `D{}` keys (deduped, ascending), then both dentries
//! re-read under the guards and the plan RETRIED when either child moved.
//!
//! The pin holds the conveyor pass pre-drain so the two txs land in ONE
//! batch by construction, on both layouts (the flat volume and the bit-17
//! forest — the suite is in `tests/run_sym_forest_suites.sh`'s matrix).
//! Runs `--test-threads=1` (process-global conveyor seams).

use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_PRE_DRAIN,
    TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::META_CONVEYOR_QUEUED;
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 128 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 8 * 1024 * 1024;
const ROOT: u64 = 1;

async fn sandbox() -> (Arc<RoutedMetaBackend>, Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        file.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: NODE_SIZE,
            journal_len_override: Some(RING_LEN),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
    let kv = KvMetaBackend::open(file.path()).await.expect("open");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv.clone()]));
    (routed, kv, file)
}

/// RAII: the conveyor seam never leaks across tests.
struct SeamGuard;
impl Drop for SeamGuard {
    fn drop(&mut self) {
        TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
        test_conveyor_hold_release();
    }
}

async fn poll_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("poll_until({what}): the observable never held");
}

/// The §5b recipe: a `set_layout_and_size` on the moved file and a
/// `rename` of it, forced into ONE conveyor batch. Both must land — the
/// rename's guard set names `I{moved}`, so the two can never co-queue:
/// the layout publish parks on the rename's guard (or the rename on the
/// publish's) and runs in the NEXT batch. The moved file keeps the
/// layout and reads the rename's ctime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_layout_publish_never_co_queues_with_a_rename_of_the_same_inode() {
    let _seam = SeamGuard;
    let (routed, _kv, _file) = sandbox().await;
    let file = routed
        .create(ROOT, "moved", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let ino = file.ino;
    let ctime_before = routed.getattr(ino).await.unwrap().ctime;

    // Hold the pass pre-drain: everything queued until the release lands
    // in one batch.
    let queued0 = META_CONVEYOR_QUEUED.load(Ordering::SeqCst);
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
    let publish = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move {
            routed
                .set_layout_and_size(ino, b"layout-under-rename", 4096, &[])
                .await
        })
    };
    // The publish is queued (its I{ino} guard alive inside the queue).
    poll_until("the layout publish queued", || {
        META_CONVEYOR_QUEUED.load(Ordering::SeqCst) >= queued0 + 1
    })
    .await;
    let rename = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { routed.rename(ROOT, "moved", ROOT, "renamed", 0).await })
    };
    // Give the rename every chance to co-queue — with the hole it does
    // (its guard set never named I{ino}); with the fix it PARKS on the
    // publish's guard and the queue stays at one member.
    let settle = Instant::now() + Duration::from_millis(300);
    while Instant::now() < settle {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        META_CONVEYOR_QUEUED.load(Ordering::SeqCst),
        queued0 + 1,
        "the rename must not co-queue with the layout publish of the same inode"
    );
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    publish
        .await
        .unwrap()
        .expect("the layout publish lands (never the same-key sentinel's EIO)");
    rename.await.unwrap().expect("the rename lands");

    // Both effects stand: the name moved, the layout is the publish's,
    // the ctime is the rename's (at or after the publish's).
    assert!(routed.lookup_dentry(ROOT, "moved").await.unwrap().is_none());
    assert_eq!(
        routed
            .lookup_dentry(ROOT, "renamed")
            .await
            .unwrap()
            .map(|(i, _)| i),
        Some(ino)
    );
    let after = routed.getattr(ino).await.unwrap();
    assert_eq!(after.size, 4096, "the layout publish's size stands");
    assert!(after.ctime >= ctime_before, "the rename's ctime stands");
}

/// The overwrite victim is the other unlocked participant: a `rename`
/// over an existing name stages a `Put` on `I{dest}`; a concurrent
/// layout publish of the VICTIM must not co-queue with it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_layout_publish_of_the_overwrite_victim_never_co_queues_with_the_rename() {
    let _seam = SeamGuard;
    let (routed, _kv, _file) = sandbox().await;
    routed
        .create(ROOT, "src", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let victim = routed
        .create(ROOT, "dst", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let queued0 = META_CONVEYOR_QUEUED.load(Ordering::SeqCst);
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
    let publish = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move {
            routed
                .set_layout_and_size(victim, b"victim-layout", 8192, &[])
                .await
        })
    };
    poll_until("the victim's publish queued", || {
        META_CONVEYOR_QUEUED.load(Ordering::SeqCst) >= queued0 + 1
    })
    .await;
    let rename = {
        let routed = Arc::clone(&routed);
        tokio::spawn(async move { routed.rename(ROOT, "src", ROOT, "dst", 0).await })
    };
    let settle = Instant::now() + Duration::from_millis(300);
    while Instant::now() < settle {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        META_CONVEYOR_QUEUED.load(Ordering::SeqCst),
        queued0 + 1,
        "the rename must not co-queue with the victim's layout publish"
    );
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    publish.await.unwrap().expect("the publish lands");
    rename.await.unwrap().expect("the rename lands");
    assert!(routed.lookup_dentry(ROOT, "src").await.unwrap().is_none());
    let (now_at_dst, _) = routed.lookup_dentry(ROOT, "dst").await.unwrap().unwrap();
    assert_ne!(now_at_dst, victim, "the victim was replaced");
    let v = routed.getattr(victim).await.unwrap();
    assert_eq!(v.nlink, 0, "the overwritten file lost its name");
}
