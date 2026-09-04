//! PR M7 conveyor conformance — `docs/design-metadata-throughput.md`
//! §5.5 D5 (group-commit v2, the per-volume commit conveyor).
//!
//! Contracts pinned (each traces to a §5.5 clause):
//!
//! - **Group formation**: concurrent committers arriving while a pass is
//!   in flight drain as ONE batch (the measured baseline failure mode —
//!   strict group size ≈ 1 — must be structurally impossible once
//!   arrivals overlap). Observed via `META_COMMIT_GROUP_SIZE`.
//! - **Caps honored**: `SQUEEZEFS_META_COMMIT_BATCH_TXS` /
//!   `_BATCH_BYTES` bound every drain; no timers anywhere.
//! - **Batch-of-1 ≡ today's pipeline**: serial commits keep today's
//!   entries/op (the degenerate case IS the current code path).
//! - **Per-tx error isolation**: a poisoned batch member fails ALONE —
//!   the rest of its batch commits, survives remount, and the volume
//!   stays healthy (no admission leak, no watermark wedge).
//! - **Cancellation safety at every committer stage** (§5.5 lifecycle,
//!   Issue 13): dropping a committer future pre-enqueue / while parked
//!   on its oneshot / mid-pass / pre-fanout leaks nothing — the detached
//!   pass completes the tx under the queue entry's co-owned DLM guards,
//!   and a second same-key writer BLOCKS on the DLM stripe until the
//!   pass releases at that tx's terminal outcome (never co-queues, never
//!   observes pre-apply state).
//! - **Strict-mode batch barrier**: one coalesced fdatasync per batch
//!   (the G3 mechanism).
//! - **Group commit (D-1c, e2e perf audit §5.3 row 1 — one conveyor group
//!   per shipped frame)**: `commit_tx_group` enqueues N staged txs under
//!   ONE queue-lock acquisition, so a drain can never observe a partial
//!   group — N distinct-ino txs are ONE pass BY CONSTRUCTION (no held
//!   pass, no timing), each tx stays its own checksummed journal entry,
//!   a member that fails admission fails ALONE (its siblings commit), the
//!   byte cap still splits an over-cap group (progress law: the first
//!   entry is always taken), and every member's DLM guards release at
//!   its terminal outcome. Observed via `META_CONVEYOR_GROUP_{COMMITS,TXS}`.
//!
//! Every scenario ends with the **conservation audit**: admitted ring
//! bytes settle to zero, `completed_upto == head` (no watermark wedge),
//! the conveyor queue is empty, and a follow-up commit succeeds.
//!
//! Committers drive the ROUTED backend (the mount shape): regular-file
//! creates take the parent I-stripe SHARED (design §5.8(b)), so same-dir
//! committers can overlap — the arrival concurrency the conveyor turns
//! into group size.
//!
//! Suite runs `--test-threads=1` (process-global stats + env knobs).

use squeezefs::meta_backend::dlm::{DlmGuard, LockMode};
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, KvTx, TEST_COMMIT_ADMITTED_STALL_MS,
    TEST_CONVEYOR_HOLD_PRE_DRAIN, TEST_CONVEYOR_HOLD_PRE_FANOUT, TEST_CONVEYOR_HOLD_STAGE,
    TEST_CONVEYOR_POISON_APPLY_INO,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::{
    KvError, META_COMMIT_GROUP_BYTES, META_COMMIT_GROUP_SIZE, META_CONVEYOR_GROUP_COMMITS,
    META_CONVEYOR_GROUP_TXS, META_CONVEYOR_LEADER_PASSES, META_CONVEYOR_PASS_PANICS,
    META_KV_JOURNAL_ENTRIES,
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

/// Fresh formatted volume + mounted routed backend (single volume: global
/// inos == local inos).
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

/// Scoped env override (the mount_writer_guard_tests pattern; knobs are
/// read at `open`, so the guard must wrap the sandbox construction).
struct EnvVarGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// RAII: seams never leak across tests.
struct SeamGuard;
impl Drop for SeamGuard {
    fn drop(&mut self) {
        TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
        test_conveyor_hold_release();
        TEST_CONVEYOR_POISON_APPLY_INO.store(0, Ordering::SeqCst);
        TEST_COMMIT_ADMITTED_STALL_MS.store(0, Ordering::SeqCst);
    }
}

/// Poll a sync observable until it holds or the deadline trips (bounded
/// condition-poll, not sleep-synchronization: the condition IS the
/// contract under test).
async fn poll_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + POLL_DEADLINE;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("poll_until({what}) deadline: the observable never held — conveyor contract broken");
}

/// Poll until a name resolves under the routed root (async observable).
async fn poll_lookup(routed: &RoutedMetaBackend, name: &str, what: &str) {
    let deadline = Instant::now() + POLL_DEADLINE;
    while Instant::now() < deadline {
        if routed.lookup(1, name).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("poll_lookup({what}): {name} never became visible — the detached pass lost the tx");
}

/// The §5.5 conservation audit every scenario ends with: budget settled,
/// watermark caught up, queue empty, volume alive.
async fn conservation_audit(routed: &Arc<RoutedMetaBackend>, kv: &Arc<KvMetaBackend>, ctx: &str) {
    let ctx_owned = ctx.to_string();
    poll_until(&format!("{ctx_owned}: queue drains"), || {
        kv.conveyor_pending_len() == 0
    })
    .await;
    poll_until(&format!("{ctx_owned}: watermark covers head"), || {
        kv.journal_ring().completed_upto() >= kv.journal_ring().core().head()
    })
    .await;
    // Admission settles once no pass is in flight; a leaked Admission
    // never settles (the §5.5 "leaks ring budget forever" hazard).
    poll_until(&format!("{ctx_owned}: admitted budget settles"), || {
        kv.journal_ring().core().admitted() == 0
    })
    .await;
    let probe_name = format!("audit_probe_{}", unique_suffix());
    routed
        .create(1, &probe_name, libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap_or_else(|e| {
            panic!("{ctx_owned}: volume must accept commits after the scenario: {e}")
        });
}

fn unique_suffix() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
}

fn snap_delta(before: &[u64; 12], after: &[u64; 12]) -> [u64; 12] {
    std::array::from_fn(|i| after[i] - before[i])
}

/// Weighted tx total across a group-size histogram delta (exact buckets
/// 1–8 carry exact weights; range buckets use their lower bound, so the
/// assertion direction stays conservative).
fn txs_at_least(delta: &[u64; 12]) -> u64 {
    const LOWER: [u64; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 17, 33, 65];
    delta.iter().zip(LOWER).map(|(&c, w)| c * w).sum()
}

// ===========================================================================
// Group formation + caps (§5.5 batching policy).
// ===========================================================================

/// Committers arriving while the pass is held drain as ONE batch: hold
/// the pass pre-drain, enqueue 16 concurrent creates (SHARED parent —
/// co-queueable by design), release, and the histogram must record one
/// 16-tx batch (leader passes stay O(1), never one per tx).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_forms_under_held_pass() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;

    let hist_before = META_COMMIT_GROUP_SIZE.snapshot();
    let passes_before = META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed);
    let bytes_before = META_COMMIT_GROUP_BYTES.load(Ordering::Relaxed);

    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);

    let mut tasks = Vec::new();
    for i in 0..16usize {
        let routed = routed.clone();
        tasks.push(tokio::spawn(async move {
            routed
                .create(1, &format!("grp_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
        }));
        // Sequence the enqueues so the batch composition is exact.
        let want = i + 1;
        poll_until("committer enqueued behind the held pass", || {
            kv.conveyor_pending_len() >= want
        })
        .await;
    }

    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();

    for t in tasks {
        t.await
            .expect("committer task")
            .expect("held-then-drained create must succeed");
    }

    let delta = snap_delta(&hist_before, &META_COMMIT_GROUP_SIZE.snapshot());
    assert_eq!(
        delta[8], 1,
        "a single 16-tx batch must form behind the held pass (<=16 bucket); got {delta:?}"
    );
    assert!(
        delta.iter().sum::<u64>() <= 2,
        "16 held txs must not fragment into many batches (≤ 1 ambient singleton \
         tolerated); got {delta:?}"
    );
    let passes = META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed) - passes_before;
    assert!(
        (1..=2).contains(&passes),
        "16 held-then-released txs must drain in 1–2 passes, not {passes}"
    );
    assert!(
        META_COMMIT_GROUP_BYTES.load(Ordering::Relaxed) > bytes_before,
        "batch byte accounting must move with the batch"
    );

    for i in 0..16 {
        routed
            .lookup(1, &format!("grp_{i}"))
            .await
            .unwrap_or_else(|e| panic!("grp_{i} must be visible after its batch: {e}"));
    }
    conservation_audit(&routed, &kv, "group_forms_under_held_pass").await;
}

/// `SQUEEZEFS_META_COMMIT_BATCH_TXS` bounds every drain (read at open,
/// like every backend knob): 10 held committers under a cap of 4 drain
/// in ≥ 3 passes with no batch above the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_tx_cap_bounds_group_size() {
    let _seams = SeamGuard;
    let _cap = EnvVarGuard::set("SQUEEZEFS_META_COMMIT_BATCH_TXS", "4");
    let (routed, kv, _f) = sandbox().await;

    let hist_before = META_COMMIT_GROUP_SIZE.snapshot();
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);

    let mut tasks = Vec::new();
    for i in 0..10usize {
        let routed = routed.clone();
        tasks.push(tokio::spawn(async move {
            routed
                .create(1, &format!("cap_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
        }));
        let want = i + 1;
        poll_until("committer enqueued under tx cap", || {
            kv.conveyor_pending_len() >= want
        })
        .await;
    }
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    for t in tasks {
        t.await.expect("task").expect("create");
    }

    let delta = snap_delta(&hist_before, &META_COMMIT_GROUP_SIZE.snapshot());
    assert_eq!(txs_at_least(&delta), 10, "10 txs total; got {delta:?}");
    for (i, &c) in delta.iter().enumerate() {
        assert!(
            i < 4 || c == 0,
            "no batch may exceed the 4-tx cap; histogram delta {delta:?}"
        );
    }
    assert!(
        delta[3] >= 2,
        "a 10-tx backlog under a 4-cap must produce at least two full batches; got {delta:?}"
    );
    conservation_audit(&routed, &kv, "batch_tx_cap").await;
}

/// `SQUEEZEFS_META_COMMIT_BATCH_BYTES` bounds drains the same way: a cap
/// smaller than one create entry forces batches of 1 even with a held
/// backlog (the first entry is always taken — progress over the cap).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_byte_cap_bounds_group_size() {
    let _seams = SeamGuard;
    // A same-dir create tx (inode Put + dentry Put + parent Δtime) is a
    // few hundred entry bytes; 64 admits exactly one per drain.
    let _cap = EnvVarGuard::set("SQUEEZEFS_META_COMMIT_BATCH_BYTES", "64");
    let (routed, kv, _f) = sandbox().await;

    let hist_before = META_COMMIT_GROUP_SIZE.snapshot();
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);

    let mut tasks = Vec::new();
    for i in 0..4usize {
        let routed = routed.clone();
        tasks.push(tokio::spawn(async move {
            routed
                .create(1, &format!("bcap_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
        }));
        let want = i + 1;
        poll_until("committer enqueued under byte cap", || {
            kv.conveyor_pending_len() >= want
        })
        .await;
    }
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    for t in tasks {
        t.await.expect("task").expect("create");
    }

    let delta = snap_delta(&hist_before, &META_COMMIT_GROUP_SIZE.snapshot());
    assert_eq!(
        delta[0], 4,
        "a byte cap below one entry must degrade every drain to batch-of-1 \
         (first-entry progress rule); got {delta:?}"
    );
    conservation_audit(&routed, &kv, "batch_byte_cap").await;
}

// ===========================================================================
// Batch-of-1 degenerate equivalence (§5.5: "a batch of 1 is byte-for-byte
// today's pipeline").
// ===========================================================================

/// Serial (never-overlapping) commits keep today's shape exactly: one
/// journal entry per create, every batch recorded at size 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_one_keeps_todays_entry_shape() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;

    let hist_before = META_COMMIT_GROUP_SIZE.snapshot();
    let entries_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);

    for i in 0..8 {
        routed
            .create(1, &format!("solo_{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("serial create");
    }

    let entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - entries_before;
    assert_eq!(
        entries, 8,
        "serial creates stay one ordinary checksummed entry each (zero format change)"
    );
    let delta = snap_delta(&hist_before, &META_COMMIT_GROUP_SIZE.snapshot());
    assert_eq!(
        delta[0], 8,
        "serial commits ride the conveyor as batches of 1 (the degenerate case IS \
         today's pipeline); got {delta:?}"
    );
    conservation_audit(&routed, &kv, "batch_of_one").await;
}

// ===========================================================================
// Per-tx error isolation (§5.5: one poisoned tx fails alone).
// ===========================================================================

/// A batch member whose RAM apply fails is rolled out of the batch and
/// fails ALONE: its neighbors commit, survive remount, and the volume
/// neither leaks budget nor wedges the watermark.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn poisoned_tx_fails_alone_batch_survives() {
    let _seams = SeamGuard;
    let (routed, kv, file) = sandbox().await;

    // The poison target must exist first so its ino is knowable.
    let victim = routed
        .create(1, "poison_me", libc::S_IFREG | 0o600, 0, 0)
        .await
        .expect("victim create");

    let hist_before = META_COMMIT_GROUP_SIZE.snapshot();
    TEST_CONVEYOR_POISON_APPLY_INO.store(victim.ino, Ordering::SeqCst);
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);

    let t_a = {
        let routed = routed.clone();
        tokio::spawn(async move { routed.create(1, "iso_a", libc::S_IFREG | 0o644, 0, 0).await })
    };
    poll_until("iso_a enqueued", || kv.conveyor_pending_len() >= 1).await;
    let t_poison = {
        let routed = routed.clone();
        let ino = victim.ino;
        tokio::spawn(async move {
            // atime present ⇒ NOT the M6 echo-absorb shape: stages a full
            // inode Put on the poisoned ino, which the armed seam fails
            // at apply.
            routed
                .setattr(ino, None, None, None, None, Some(1), Some(2), Some(3))
                .await
        })
    };
    poll_until("poisoned setattr enqueued", || {
        kv.conveyor_pending_len() >= 2
    })
    .await;
    let t_b = {
        let routed = routed.clone();
        tokio::spawn(async move { routed.create(1, "iso_b", libc::S_IFREG | 0o644, 0, 0).await })
    };
    poll_until("iso_b enqueued", || kv.conveyor_pending_len() >= 3).await;

    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();

    t_a.await
        .expect("task")
        .expect("iso_a must commit despite the poisoned neighbor");
    let poisoned = t_poison.await.expect("task");
    assert!(
        poisoned.is_err(),
        "the poisoned tx must fail alone (armed apply fault); got {poisoned:?}"
    );
    t_b.await
        .expect("task")
        .expect("iso_b must commit despite the poisoned neighbor");
    TEST_CONVEYOR_POISON_APPLY_INO.store(0, Ordering::SeqCst);

    // The batch really formed (this is isolation INSIDE one batch, not
    // three serial commits).
    let delta = snap_delta(&hist_before, &META_COMMIT_GROUP_SIZE.snapshot());
    assert!(
        delta[2] >= 1,
        "the three txs must have drained as one batch; got {delta:?}"
    );

    // The poisoned mutation must NOT be visible…
    let v = routed.getattr(victim.ino).await.expect("victim getattr");
    assert_ne!(
        (v.atime, v.mtime, v.ctime),
        (1, 2, 3),
        "the failed member's records must be rolled out of RAM"
    );
    // …and the survivors must be durable across a clean remount.
    conservation_audit(&routed, &kv, "poisoned_tx").await;
    kv.shutdown().await.expect("clean shutdown");
    drop(routed);
    drop(kv);
    let re = KvMetaBackend::open(file.path()).await.expect("remount");
    re.lookup(1, "iso_a").await.expect("iso_a durable");
    re.lookup(1, "iso_b").await.expect("iso_b durable");
    let v = Metadata::getattr(re.as_ref(), victim.ino)
        .await
        .expect("victim getattr");
    assert_ne!(
        (v.atime, v.mtime, v.ctime),
        (1, 2, 3),
        "the failed member must not resurrect at replay"
    );
    re.shutdown().await.expect("shutdown");
}

// ===========================================================================
// Cancellation safety (§5.5 lifecycle + Issue 13) — drop the COMMITTER
// future at each protocol stage under the conservation audit.
// ===========================================================================

/// Stage: pre-enqueue. A constructed-but-never-polled commit future
/// acquires nothing; dropping it leaks nothing and leaves the name free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_pre_enqueue_is_inert() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;

    {
        let fut = routed.create(1, "pre_enq", libc::S_IFREG | 0o644, 0, 0);
        drop(fut); // never polled: no guards, no queue entry, no budget
    }
    assert_eq!(kv.conveyor_pending_len(), 0, "nothing may be queued");
    routed
        .create(1, "pre_enq", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("the name must be free after a pre-enqueue drop");
    conservation_audit(&routed, &kv, "cancel_pre_enqueue").await;
}

/// Stage: post-enqueue, committer parked on its oneshot (the Issue-13
/// case). Dropping the committer must NOT release its queue entry's DLM
/// guards: a second same-key create BLOCKS on the stripe until the pass
/// reaches the tx's terminal outcome, then observes the committed state
/// (EEXIST) — never co-queues, never sees pre-apply state. The dropped
/// committer's tx still commits (detached pass).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_post_enqueue_same_key_blocks_until_terminal() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;

    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);

    // X enqueues {records, guards, oneshot} and parks on the oneshot.
    let x = {
        let routed = routed.clone();
        tokio::spawn(async move {
            routed
                .create(1, "dup_key", libc::S_IFREG | 0o644, 0, 0)
                .await
        })
    };
    poll_until("X enqueued behind the held pass", || {
        kv.conveyor_pending_len() >= 1
    })
    .await;

    // Drop the committer future mid-await: only its oneshot receiver and
    // its own Arc ref on the guard set die (§5.5 revision 2).
    x.abort();
    let _ = x.await; // observe the abort

    // Y = same (parent, name). Its D-stripe acquisition must BLOCK while
    // the queue entry's guards live.
    let y = {
        let routed = routed.clone();
        tokio::spawn(async move {
            routed
                .create(1, "dup_key", libc::S_IFREG | 0o644, 0, 0)
                .await
        })
    };
    // Y must not finish, and must NOT co-queue, while the pass is held:
    // structural exclusion, not scheduling luck.
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(
            !y.is_finished(),
            "same-key Y must block on the DLM stripe while X's queue entry holds its guards"
        );
        assert!(
            kv.conveyor_pending_len() <= 1,
            "same-key Y must never co-queue behind X's live guards"
        );
    }

    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();

    // X's tx commits under its still-held guards; Y then sees EEXIST.
    let y_out = y.await.expect("Y task");
    let err = y_out.expect_err("Y must observe X's committed dentry, never pre-apply state");
    assert!(
        err.to_string().contains("already exists"),
        "Y's refusal must be the EEXIST shape (committed-state visibility): {err}"
    );
    routed
        .lookup(1, "dup_key")
        .await
        .expect("the dropped committer's tx must still commit (detached pass)");
    conservation_audit(&routed, &kv, "cancel_post_enqueue").await;
}

/// Stage: mid-pass (admission held, nothing reserved — the historical
/// budget-leak window). Dropping the committer mid-pass leaks nothing:
/// the detached pass finishes the batch, the tx commits, budget settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_pass_conserves_and_commits() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;

    TEST_COMMIT_ADMITTED_STALL_MS.store(1200, Ordering::SeqCst);
    let x = {
        let routed = routed.clone();
        tokio::spawn(async move {
            routed
                .create(1, "mid_pass", libc::S_IFREG | 0o644, 0, 0)
                .await
        })
    };
    // The pass has admitted (Σ > 0) and is stalling inside the hazard
    // window the M4 seam pins.
    poll_until("pass admitted the batch", || {
        kv.journal_ring().core().admitted() > 0
    })
    .await;
    x.abort();
    let _ = x.await;
    TEST_COMMIT_ADMITTED_STALL_MS.store(0, Ordering::SeqCst);

    // The detached pass must complete the commit anyway.
    poll_lookup(&routed, "mid_pass", "cancel_mid_pass").await;
    conservation_audit(&routed, &kv, "cancel_mid_pass").await;
}

/// Stage: pre-fanout (batch written, acked, barriered; results not yet
/// sent). The oneshot send hits a dead receiver — harmless; everything
/// else already happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_pre_fanout_is_harmless() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;

    let entries_before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_FANOUT, Ordering::SeqCst);

    let x = {
        let routed = routed.clone();
        tokio::spawn(async move {
            routed
                .create(1, "pre_fan", libc::S_IFREG | 0o644, 0, 0)
                .await
        })
    };
    // The batch's entry has landed but X is still parked on its oneshot
    // (the pass is held pre-fanout).
    poll_until("batch written while pass held pre-fanout", || {
        META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) > entries_before
    })
    .await;
    assert!(
        !x.is_finished(),
        "X must still be parked on its oneshot while the pass is held pre-fanout"
    );
    x.abort();
    let _ = x.await;

    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();

    poll_lookup(&routed, "pre_fan", "cancel_pre_fanout").await;
    assert_eq!(
        META_CONVEYOR_PASS_PANICS.load(Ordering::Relaxed),
        0,
        "a dead oneshot receiver is harmless, never a pass panic"
    );
    conservation_audit(&routed, &kv, "cancel_pre_fanout").await;
}

// ===========================================================================
// Strict mode: ONE coalesced barrier per batch (the G3 mechanism).
// ===========================================================================

/// Under strict durability a held-then-released batch of 8 pays ONE
/// batch barrier (± the ambient checkpoint tick), not 8 — the exact
/// baseline pathology (effective group size ≈ 1, ~70 % of wall in
/// fdatasync) the conveyor exists to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_batch_pays_one_barrier() {
    let _seams = SeamGuard;
    let _strict = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "0");
    let (routed, kv, _f) = sandbox().await;

    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for i in 0..8usize {
        let routed = routed.clone();
        tasks.push(tokio::spawn(async move {
            routed
                .create(1, &format!("strict_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
        }));
        let want = i + 1;
        poll_until("strict committer enqueued", || {
            kv.conveyor_pending_len() >= want
        })
        .await;
    }

    let syncs_before = squeezefs::fuse_client::METRICS
        .meta_device_syncs
        .load(Ordering::Relaxed);
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    for t in tasks {
        t.await.expect("task").expect("strict create");
    }
    let syncs = squeezefs::fuse_client::METRICS
        .meta_device_syncs
        .load(Ordering::Relaxed)
        - syncs_before;

    assert!(
        (1..=3).contains(&syncs),
        "8 strict txs in one batch must pay ONE coalesced barrier (+ ambient \
         checkpoint ticks at most), got {syncs}"
    );
    conservation_audit(&routed, &kv, "strict_batch_barrier").await;
}

// ===========================================================================
// D-1c — group commit: N staged txs enqueued as ONE conveyor group
// (docs/design-e2e-perf-audit.md §5.3 row 1; the owner-side mechanism
// behind "one conveyor group per shipped frame").
// ===========================================================================

/// Mint `n` regular files under the root (distinct inos — the shape a
/// shipped frame's independent publishes name).
async fn mint_files(routed: &Arc<RoutedMetaBackend>, n: usize, prefix: &str) -> Vec<u64> {
    let mut inos = Vec::with_capacity(n);
    for i in 0..n {
        inos.push(
            routed
                .create(1, &format!("{prefix}_{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("mint")
                .ino,
        );
    }
    inos
}

/// A layout value of `len` bytes tagged by `tag` (opaque to the backend:
/// `set_layout_and_size` stores the xattr bytes verbatim; the read-back
/// compares them).
fn layout_value(tag: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[..8].copy_from_slice(&tag.to_le_bytes());
    v
}

/// The group's 4a acquisition: ONE canonical `lock_many` over every
/// member's `I{ino}` (deduped by stripe, ascending — two distinct inos
/// may share a stripe, and per-tx guards taken one after another would
/// self-deadlock on a collision), co-owned by every member tx.
async fn lock_group(kv: &KvMetaBackend, inos: &[u64]) -> Arc<[DlmGuard]> {
    let want: Vec<(u64, LockMode)> = inos.iter().map(|&i| (i, LockMode::Exclusive)).collect();
    Arc::from(kv.dlm().lock_many(&want, &[]).await)
}

/// Stage one layout publish per `(ino, layout, size)` under the shared
/// guard set — the stage half of `set_layout_and_size`.
async fn stage_group(
    kv: &KvMetaBackend,
    guards: &Arc<[DlmGuard]>,
    items: &[(u64, Vec<u8>, u64)],
) -> Vec<KvTx> {
    let mut txs = Vec::with_capacity(items.len());
    for (ino, layout, size) in items {
        txs.push(
            kv.stage_layout_and_size_holding(*ino, layout, *size, &[], Arc::clone(guards))
                .await
                .expect("stage"),
        );
    }
    txs
}

/// Every member's `I{ino}` is free again: an exclusive re-acquisition
/// completes promptly (a leaked group guard would park it forever).
async fn assert_inode_locks_free(kv: &KvMetaBackend, inos: &[u64]) {
    for &ino in inos {
        let g = tokio::time::timeout(Duration::from_secs(5), kv.dlm().lock_inode_exclusive(ino))
            .await
            .unwrap_or_else(|_| panic!("I{{{ino}}} still held after the group's terminal outcome"));
        drop(g);
    }
}

struct GroupCounters {
    passes: u64,
    entries: u64,
    groups: u64,
    group_txs: u64,
    hist: [u64; 12],
}

fn group_counters() -> GroupCounters {
    GroupCounters {
        passes: META_CONVEYOR_LEADER_PASSES.load(Ordering::Relaxed),
        entries: META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed),
        groups: META_CONVEYOR_GROUP_COMMITS.load(Ordering::Relaxed),
        group_txs: META_CONVEYOR_GROUP_TXS.load(Ordering::Relaxed),
        hist: META_COMMIT_GROUP_SIZE.snapshot(),
    }
}

/// (a) 16 distinct-ino staged txs committed as one group are ONE apply
/// pass BY CONSTRUCTION — no held pass, no seam: the group is enqueued
/// under one queue-lock acquisition, so the drain that takes its head
/// takes it all. 16 journal entries (one tx = one entry, unchanged),
/// every slot `Ok`, every layout landed, every guard released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_of_distinct_inos_is_one_pass_by_construction() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;
    let inos = mint_files(&routed, 16, "grp16").await;
    let items: Vec<(u64, Vec<u8>, u64)> = inos
        .iter()
        .enumerate()
        .map(|(i, &ino)| (ino, layout_value(ino, 64), 4096 * (i as u64 + 1)))
        .collect();

    let before = group_counters();
    let guards = lock_group(&kv, &inos).await;
    let txs = stage_group(&kv, &guards, &items).await;
    drop(guards); // the txs co-own the set from here — the committer's ref is not needed
    let results = kv.commit_tx_group(txs).await;
    let after = group_counters();

    assert_eq!(results.len(), 16, "one outcome per member, in input order");
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("member {i} must commit: {e}"));
    }
    assert_eq!(
        after.entries - before.entries,
        16,
        "one tx = one checksummed journal entry — a group never collapses entries"
    );
    let passes = after.passes - before.passes;
    assert!(
        (1..=2).contains(&passes),
        "a 16-tx group is ONE apply pass (≤ 1 ambient singleton tolerated), not {passes}"
    );
    let delta = snap_delta(&before.hist, &after.hist);
    assert_eq!(
        delta[8], 1,
        "the group drained as one 16-tx batch (the <=16 bucket); got {delta:?}"
    );
    assert_eq!(after.groups - before.groups, 1, "one group commit");
    assert_eq!(after.group_txs - before.group_txs, 16, "sixteen member txs");
    for (i, &ino) in inos.iter().enumerate() {
        assert_eq!(
            Metadata::getattr(kv.as_ref(), ino)
                .await
                .expect("getattr")
                .size,
            4096 * (i as u64 + 1),
            "member {i}'s size landed on member {i}'s ino"
        );
        assert_eq!(
            KvMetaBackend::getxattr(&kv, ino, "layout")
                .await
                .expect("layout read")
                .expect("layout present"),
            layout_value(ino, 64),
            "member {i}'s layout landed"
        );
    }
    assert_inode_locks_free(&kv, &inos).await;
    conservation_audit(&routed, &kv, "group_one_pass").await;
}

/// (b) A member that fails ADMISSION (its record crosses the per-volume
/// value cap) fails ALONE before the group is enqueued: its slot is the
/// `ValueTooLarge` error, its 15 siblings commit in one pass with 15
/// entries, and its guard releases with everyone else's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inadmissible_member_fails_alone_and_its_siblings_commit() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;
    let inos = mint_files(&routed, 16, "grpbad").await;
    // Node size 64 KiB ⇒ record value cap 16 KiB + envelope; 32 KiB is
    // over it by construction.
    let over_cap = 32 * 1024;
    let items: Vec<(u64, Vec<u8>, u64)> = inos
        .iter()
        .enumerate()
        .map(|(i, &ino)| {
            let len = if i == 7 { over_cap } else { 64 };
            (ino, layout_value(ino, len), 8192)
        })
        .collect();

    let before = group_counters();
    let guards = lock_group(&kv, &inos).await;
    let txs = stage_group(&kv, &guards, &items).await;
    drop(guards);
    let results = kv.commit_tx_group(txs).await;
    let after = group_counters();

    assert_eq!(results.len(), 16);
    for (i, r) in results.iter().enumerate() {
        if i == 7 {
            assert!(
                matches!(r, Err(KvError::ValueTooLarge { .. })),
                "the over-cap member's slot carries ITS refusal, got {r:?}"
            );
        } else {
            r.as_ref()
                .unwrap_or_else(|e| panic!("sibling {i} must commit despite member 7: {e}"));
        }
    }
    assert_eq!(
        after.entries - before.entries,
        15,
        "15 siblings, 15 entries"
    );
    let passes = after.passes - before.passes;
    assert!(
        (1..=2).contains(&passes),
        "the 15 siblings are one pass, not {passes}"
    );
    assert_eq!(after.groups - before.groups, 1);
    assert_eq!(
        after.group_txs - before.group_txs,
        15,
        "the refused member never joined the group"
    );
    assert_eq!(
        Metadata::getattr(kv.as_ref(), inos[7])
            .await
            .expect("getattr")
            .size,
        0,
        "the refused member applied nothing"
    );
    assert_inode_locks_free(&kv, &inos).await;
    conservation_audit(&routed, &kv, "group_inadmissible_member").await;
}

/// (c) The degenerate groups: an empty group answers an empty Vec and
/// touches nothing; a group of EMPTY txs answers `Ok(())` per slot inline
/// — no pass, no entry, no group counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_groups_and_empty_members_commit_nothing() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;
    let before = group_counters();

    let none = kv.commit_tx_group(Vec::new()).await;
    assert!(none.is_empty(), "an empty group answers an empty Vec");

    let empties = kv
        .commit_tx_group(vec![KvTx::empty(), KvTx::empty(), KvTx::empty()])
        .await;
    assert_eq!(empties.len(), 3);
    for r in &empties {
        assert!(r.is_ok(), "an empty tx is Ok(()) inline");
    }

    let after = group_counters();
    assert_eq!(after.passes, before.passes, "no pass ran");
    assert_eq!(after.entries, before.entries, "no entry was written");
    assert_eq!(
        after.groups, before.groups,
        "nothing was enqueued, nothing counted"
    );
    assert_eq!(after.group_txs, before.group_txs);
    conservation_audit(&routed, &kv, "group_empty").await;
}

/// (d) Two groups from two racing tasks: every tx commits exactly once,
/// entries == the total, and the two groups ride at most two passes
/// (one when the second's enqueue lands before the first's drain). The
/// interleaving law itself is the loom model
/// (`conveyor_group_never_drains_partially`); this is its integration face.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_racing_groups_commit_every_member_exactly_once() {
    let _seams = SeamGuard;
    let (routed, kv, _f) = sandbox().await;
    let inos_a = mint_files(&routed, 8, "race_a").await;
    let inos_b = mint_files(&routed, 8, "race_b").await;
    let items = |inos: &[u64], base: u64| -> Vec<(u64, Vec<u8>, u64)> {
        inos.iter()
            .enumerate()
            .map(|(i, &ino)| (ino, layout_value(ino, 64), base + i as u64))
            .collect()
    };
    let items_a = items(&inos_a, 1_000);
    let items_b = items(&inos_b, 2_000);

    let before = group_counters();
    let run = |kv: Arc<KvMetaBackend>, inos: Vec<u64>, items: Vec<(u64, Vec<u8>, u64)>| {
        tokio::spawn(async move {
            let guards = lock_group(&kv, &inos).await;
            let txs = stage_group(&kv, &guards, &items).await;
            drop(guards);
            kv.commit_tx_group(txs).await
        })
    };
    let (ra, rb) = tokio::join!(
        run(kv.clone(), inos_a.clone(), items_a),
        run(kv.clone(), inos_b.clone(), items_b)
    );
    let (ra, rb) = (ra.expect("task a"), rb.expect("task b"));
    let after = group_counters();

    for (label, res) in [("a", &ra), ("b", &rb)] {
        assert_eq!(res.len(), 8);
        for (i, r) in res.iter().enumerate() {
            r.as_ref()
                .unwrap_or_else(|e| panic!("group {label} member {i}: {e}"));
        }
    }
    assert_eq!(
        after.entries - before.entries,
        16,
        "16 txs, 16 entries, each exactly once"
    );
    let passes = after.passes - before.passes;
    assert!(
        (1..=3).contains(&passes),
        "two groups ride ≤ 2 passes (+ 1 ambient), not {passes}"
    );
    assert_eq!(after.groups - before.groups, 2);
    assert_eq!(after.group_txs - before.group_txs, 16);
    for (i, &ino) in inos_a.iter().enumerate() {
        assert_eq!(
            Metadata::getattr(kv.as_ref(), ino).await.unwrap().size,
            1_000 + i as u64
        );
    }
    for (i, &ino) in inos_b.iter().enumerate() {
        assert_eq!(
            Metadata::getattr(kv.as_ref(), ino).await.unwrap().size,
            2_000 + i as u64
        );
    }
    assert_inode_locks_free(&kv, &inos_a).await;
    assert_inode_locks_free(&kv, &inos_b).await;
    conservation_audit(&routed, &kv, "group_race").await;
}

/// (e) The byte cap still governs: a group whose summed entry bytes exceed
/// `SQUEEZEFS_META_COMMIT_BATCH_BYTES` splits at the cap (here: a cap
/// below one entry ⇒ batches of 1 by the first-entry progress rule) and
/// STILL commits every member — grouping is queue-side atomicity, never a
/// license to exceed the drain caps.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_over_cap_group_splits_at_the_byte_cap_and_commits_all() {
    let _seams = SeamGuard;
    let _cap = EnvVarGuard::set("SQUEEZEFS_META_COMMIT_BATCH_BYTES", "64");
    let (routed, kv, _f) = sandbox().await;
    let inos = mint_files(&routed, 4, "grpcap").await;
    let items: Vec<(u64, Vec<u8>, u64)> = inos
        .iter()
        .map(|&ino| (ino, layout_value(ino, 128), 512))
        .collect();

    let before = group_counters();
    let guards = lock_group(&kv, &inos).await;
    let txs = stage_group(&kv, &guards, &items).await;
    drop(guards);
    let results = kv.commit_tx_group(txs).await;
    let after = group_counters();

    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("member {i} must commit across the split: {e}"));
    }
    assert_eq!(after.entries - before.entries, 4);
    let delta = snap_delta(&before.hist, &after.hist);
    assert_eq!(
        delta[0], 4,
        "a byte cap below one entry drains the group as four batches of 1; got {delta:?}"
    );
    assert_eq!(after.groups - before.groups, 1, "still ONE group enqueue");
    assert_eq!(after.group_txs - before.group_txs, 4);
    assert_inode_locks_free(&kv, &inos).await;
    conservation_audit(&routed, &kv, "group_byte_cap").await;
}

/// (f) The crash contract is untouched: a 24-tx group is 24 ordinary
/// checksummed journal entries — the same count 24 single commits write —
/// and a replay (drop WITHOUT shutdown, checkpoint cadence parked) rebuilds
/// every one of the 24 layouts and sizes. The group is queue-side only;
/// nothing about it reaches the ring or the replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_replays_identically_to_single_commits() {
    let _seams = SeamGuard;
    // Park the cadence: no checkpoint covers the group, so the whole set
    // is the replay window by construction (the kv_scale replay pattern).
    let _cadence = EnvVarGuard::set("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    const N: usize = 24;

    // Leg 1: 24 single commits — the entry count the group must match.
    let single_entries = {
        let (routed, kv, _f) = sandbox().await;
        let inos = mint_files(&routed, N, "single").await;
        let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
        for (i, &ino) in inos.iter().enumerate() {
            kv.set_layout_and_size(ino, &layout_value(ino, 96), 100 + i as u64, &[])
                .await
                .expect("single commit");
        }
        let entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - before;
        kv.shutdown().await.expect("shutdown");
        entries
    };
    assert_eq!(single_entries, N as u64);

    // Leg 2: the same 24 as ONE group, then crash-replay.
    let (routed, kv, file) = sandbox().await;
    let inos = mint_files(&routed, N, "grouped").await;
    let items: Vec<(u64, Vec<u8>, u64)> = inos
        .iter()
        .enumerate()
        .map(|(i, &ino)| (ino, layout_value(ino, 96), 100 + i as u64))
        .collect();
    let before = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let guards = lock_group(&kv, &inos).await;
    let txs = stage_group(&kv, &guards, &items).await;
    drop(guards);
    for (i, r) in kv.commit_tx_group(txs).await.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("group member {i}: {e}"));
    }
    let group_entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - before;
    assert_eq!(
        group_entries, single_entries,
        "a group writes exactly the entries its members would have written alone"
    );
    poll_until("group settles before the crash", || {
        kv.conveyor_pending_len() == 0
            && kv.journal_ring().completed_upto() >= kv.journal_ring().core().head()
    })
    .await;

    // Drop WITHOUT shutdown: no final checkpoint — the group is the
    // replay window.
    drop(routed);
    drop(kv);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let re = KvMetaBackend::open(file.path())
        .await
        .expect("replaying a grouped window must never fail the mount");
    for (i, &ino) in inos.iter().enumerate() {
        assert_eq!(
            Metadata::getattr(re.as_ref(), ino)
                .await
                .expect("getattr")
                .size,
            100 + i as u64,
            "member {i}'s size survives replay"
        );
        assert_eq!(
            KvMetaBackend::getxattr(&re, ino, "layout")
                .await
                .expect("layout read")
                .expect("layout present after replay"),
            layout_value(ino, 96),
            "member {i}'s layout survives replay"
        );
    }
    re.shutdown().await.expect("shutdown");
}
