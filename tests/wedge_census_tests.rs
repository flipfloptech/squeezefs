//! The **wedge census** (zc-bridge-cqe-wedge campaign, 2026-08-07):
//! when the D1.b op watchdog names overdue ops, the census line must
//! name WHAT they park on — the zcws-9 W4 field wedge produced 16k
//! watchdog lines, a 3-entry lock census, and NO way to tell whether
//! the stall was the commit conveyor, the journal, the write pipeline,
//! or the transport (the `.stats` read itself hung, so the gauges were
//! unreachable exactly when they mattered). The census is one log line
//! built from PROCESS-GLOBAL atomics only — readable from the watchdog
//! task no matter what is wedged.
//!
//! Contracts (red-first):
//! 1. `wedge_census_line()` exports every named field as `key=value`
//!    (the field list is the contract — a wedge capture reads it, so a
//!    missing field is a blind spot found at the worst time).
//! 2. The conveyor pair MOVES: a commit parked behind a HELD conveyor
//!    (the `TEST_CONVEYOR_HOLD_STAGE` seam) reads
//!    `meta_commit_parked ≥ 1` and `meta_conveyor_queued ≥ 1` while
//!    `meta_conveyor_passes` stays flat — the exact stalled-conveyor
//!    signature; release drains both gauges back to 0 and the commit
//!    lands.
//!
//! Suite runs `--test-threads=1` (process-global stats + seams).

use squeezefs::fuse_client::wedge_census_line;
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_CONVEYOR_HOLD_PRE_DRAIN,
    TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::{
    META_COMMIT_PARKED, META_CONVEYOR_LEADER_PASSES, META_CONVEYOR_QUEUED,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const POLL_DEADLINE: Duration = Duration::from_secs(15);

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

/// RAII: the hold seam never leaks across tests.
struct SeamGuard;
impl Drop for SeamGuard {
    fn drop(&mut self) {
        TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
        test_conveyor_hold_release();
    }
}

async fn poll_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

/// Contract 1 — the field list. Every field a wedge capture reads must
/// export as `key=value` (u64s), always (zero-valued on a quiet
/// process).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn census_line_exports_every_field() {
    let line = wedge_census_line();
    for key in [
        // The M7 conveyor: passes flat + queued/parked growing = the
        // stalled-conveyor signature.
        "meta_conveyor_passes=",
        "meta_conveyor_queued=",
        "meta_commit_parked=",
        "meta_publish_parked=",
        // The journal/checkpoint plane (a stalled checkpoint starves
        // ring admission).
        "meta_journal_entries=",
        "meta_checkpoints=",
        // The write pipeline (permit exhaustion = invisible in-flight).
        "pipeline_inflight_blocks=",
        "pipeline_admission_waits=",
        // The reclaim queue (at-cap parks feed back into writers).
        "reclaim_queue_bytes=",
        // Rewrite epochs (the zcws-9 onset correlate).
        "rewrite_open_epochs=",
        // The transport: leases gate COMMIT re-arms; parked − unparked
        // at quiesce is the wedge count (the §5.4 park ledger).
        "transport_leases_outstanding=",
        "transport_parked_commits=",
        "transport_unparked_commits=",
    ] {
        assert!(
            line.contains(key),
            "wedge census must export `{key}` — a missing field is a \
             blind spot found mid-wedge; line was: {line}"
        );
    }
}

/// Contract 2 — the conveyor pair moves and the census names a live
/// stall: a commit parked behind a held conveyor reads
/// commit_parked ≥ 1 ∧ conveyor_queued ≥ 1 with passes flat; the
/// release drains both and the commit lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn census_names_a_stalled_conveyor() {
    let _seam = SeamGuard;
    let (routed, _kv, _file) = sandbox().await;

    let parked0 = META_COMMIT_PARKED.load(Ordering::SeqCst);
    let queued0 = META_CONVEYOR_QUEUED.load(Ordering::SeqCst);
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
    let passes0 = META_CONVEYOR_LEADER_PASSES.load(Ordering::SeqCst);

    let be = Arc::clone(&routed);
    let committer = tokio::spawn(async move {
        be.create(1, "wedge_census_probe", libc::S_IFREG | 0o644, 0, 0)
            .await
    });

    assert!(
        poll_until(POLL_DEADLINE, || {
            META_COMMIT_PARKED.load(Ordering::SeqCst) > parked0
                && META_CONVEYOR_QUEUED.load(Ordering::SeqCst) > queued0
        })
        .await,
        "a commit behind a held conveyor must show on BOTH gauges \
         (parked {} queued {})",
        META_COMMIT_PARKED.load(Ordering::SeqCst),
        META_CONVEYOR_QUEUED.load(Ordering::SeqCst),
    );
    assert_eq!(
        META_CONVEYOR_LEADER_PASSES.load(Ordering::SeqCst),
        passes0,
        "the held conveyor must run NO pass — queued-grows-passes-flat \
         is the stalled signature the census exists for"
    );
    let line = wedge_census_line();
    assert!(
        !line.contains("meta_commit_parked=0"),
        "the census line must carry the live parked count: {line}"
    );
    assert!(
        !line.contains("meta_conveyor_queued=0"),
        "the census line must carry the live queued count: {line}"
    );

    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    committer
        .await
        .expect("committer task")
        .expect("create lands after release");
    assert!(
        poll_until(POLL_DEADLINE, || {
            META_COMMIT_PARKED.load(Ordering::SeqCst) == parked0
                && META_CONVEYOR_QUEUED.load(Ordering::SeqCst) == queued0
        })
        .await,
        "gauges must drain to their floors after the release \
         (parked {} queued {})",
        META_COMMIT_PARKED.load(Ordering::SeqCst),
        META_CONVEYOR_QUEUED.load(Ordering::SeqCst),
    );
}
