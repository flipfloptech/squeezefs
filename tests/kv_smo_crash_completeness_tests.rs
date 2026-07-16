//! FIND-VS-A (part 2, acked-create loss): crash-replay completeness under
//! SMO churn — tests-first contract for the fix.
//!
//! The 2026-07-16 forensics (`.benchmarks/2026-07-16-find-vs-a-fix.md`)
//! caught the v3 backend losing **acked, committed** creates across a
//! process crash with a *clean* replay (`replay_dropped_torn == 0`):
//! under a create storm, leaf SMOs (compact/split) retire nodes whose
//! `dirty_floor` still names un-checkpoint-covered record seqs; the
//! checkpoint tail rule (`§4.6 pt 2`) then only sees LIVE nodes' floors,
//! so the next ledger record's `journal_tail_seq` can pass records whose
//! only durable copy rides successor images + in-RAM routing that the
//! mounted ledger record does not name. A kill between that ledger write
//! and full coverage loses the records (observed: ENOENT on 1–4 % of
//! acked creates; scoreboard FIND-VS-A R3 rows INVALID).
//!
//! Contract (design §4.10, "every ledger-acked op present and whole"):
//! **an acked commit followed by ANY crash must be served after reopen** —
//! whatever mix of checkpoints and SMOs ran in between. The reopen here is
//! the process-crash equivalent: every device write is buffered (page
//! cache), so a fresh `open` of the same file sees exactly the bytes a
//! post-kill remount would.
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::Metadata;
use squeezefs::meta_backend::RoutedMetaBackend;
use std::sync::Arc;
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 256 * 1024 * 1024;
/// Smallest legal node: leaf logs fill fast, so compact/split SMO churn
/// (the loss precondition) fires hundreds of times within one test.
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 8 * 1024 * 1024;

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

/// The storm shape from the forensics, shrunk: bursts of acked creates
/// with checkpoint cycles interleaved (the cadence tick's job), enough to
/// drive leaf compactions/splits with dirty floors dying at retire —
/// then a crash-equivalent reopen that must serve every acked name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acked_creates_survive_crash_across_smo_churn() {
    let (routed, kv, file) = sandbox().await;

    let mut acked: Vec<String> = Vec::new();
    // ~24 burst/checkpoint rounds × 400 creates ≈ 9.6k dentries: a 64 KiB
    // leaf folds ~1k records, so this drives dozens of compactions and
    // splits with checkpoints (and their ledger writes) interleaved at
    // storm-realistic points — the exact retire-with-floor overlap the
    // forensics caught losing records.
    for round in 0..24 {
        for i in 0..400 {
            let name = format!("f{round:02}_{i:04}");
            routed
                .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create acked");
            acked.push(name);
        }
        kv.checkpoint_now().await.expect("checkpoint cycle");
    }

    // Process-crash equivalent: drop RAM state without shutdown. The
    // buffered device writes (page cache) are exactly what a post-kill
    // remount reads. In-process, the single-writer flock releases only
    // when the detached checkpoint/pass tasks drop their last Arc —
    // poll-retry the reopen (a real crash releases the flock instantly;
    // this is harness plumbing, not the contract under test).
    drop(routed);
    drop(kv);

    let mut reopened = None;
    for _ in 0..200 {
        match KvMetaBackend::open(file.path()).await {
            Ok(be) => {
                reopened = Some(be);
                break;
            }
            Err(squeezefs::meta_backend::kv::KvError::Busy(_)) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => panic!("reopen: {e:?}"),
        }
    }
    let kv2 = reopened.expect("writer guard must release once the old backend is dropped");
    let routed2 = Arc::new(RoutedMetaBackend::new(vec![kv2.clone()]));

    let mut lost: Vec<String> = Vec::new();
    for name in &acked {
        if routed2.lookup(1, name).await.is_err() {
            lost.push(name.clone());
        }
    }
    assert!(
        lost.is_empty(),
        "{} of {} ACKED creates vanished across a crash-equivalent reopen \
         (clean replay, no torn writes — the FIND-VS-A loss class): {:?} …",
        lost.len(),
        acked.len(),
        &lost[..lost.len().min(12)]
    );
}
