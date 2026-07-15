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

use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_release, KvMetaBackend, TEST_COMMIT_ADMITTED_STALL_MS,
    TEST_CONVEYOR_HOLD_PRE_DRAIN, TEST_CONVEYOR_HOLD_PRE_FANOUT, TEST_CONVEYOR_HOLD_STAGE,
    TEST_CONVEYOR_POISON_APPLY_INO,
};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::{
    META_COMMIT_GROUP_BYTES, META_COMMIT_GROUP_SIZE, META_CONVEYOR_LEADER_PASSES,
    META_CONVEYOR_PASS_PANICS, META_KV_JOURNAL_ENTRIES,
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
