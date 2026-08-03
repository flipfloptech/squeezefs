//! DLM byte-range custody — spec §6.9 stage **S11**'s single-node half
//! (`docs/pre-rc-engineering-spec.md` §6.7 lock modes, §6.3 W1 paragraph;
//! execution-plan ruling **D8**: *"a file especially a large one could be
//! getting read/written to different blocks by different applications and
//! want locks on them"*).
//!
//! `acquire_lock(path, range, ttl)` has carried a `range` parameter since
//! S0, but there was **no range conflict logic**: every range folded into
//! the file's single lock entry, so two DISJOINT ranges on one file
//! serialized against each other. This suite defines the arbitration.
//!
//! Contracts:
//!
//! 1. **Disjoint ranges are genuinely concurrent** — proven by a barrier
//!    both tasks must reach *while both leases are held*. Serialization
//!    shows up as a barrier that never completes (RED pre-S11).
//! 2. **Overlapping ranges arbitrate** — bounded loud refusal while held
//!    (the wait budget is time, never wakeup counts), then a long-budget
//!    waiter is woken by the release.
//! 3. **Whole-file (`range: None`) conflicts with every range and vice
//!    versa** — whole-file custody is the strongest custody, so it can
//!    neither be granted under a live range nor coexist with one (RED
//!    pre-S11: ranges were a *different lock object* and did not conflict).
//! 4. **End-exclusive spans**: `[a, b)` — adjacent ranges never conflict,
//!    identical ranges always do, and a malformed span refuses loud.
//! 5. **The fencing semantics S1/S2 built survive**: ranges keep sharing
//!    the FILE's generator (the ~24-site census reads file identities),
//!    every mint stays globally unique and monotone, the file read is
//!    exact while a whole-file lease is held, covers every live range
//!    grant, and never regresses across release.
//! 6. **Lock modes** (§6.7): EX is what every shipped verb issues; **CW
//!    ships DISABLED** until a verb issues it — refused loud through the
//!    public API and reachable only through the documented test seam.
//! 7. **The W1 seventh ineligibility clause's custody source**: a span
//!    under byte-range custody this writer does not solely own classifies
//!    range-shared (`patch_ineligible_range_shared`'s predicate).
//!
//! Every test owns a private inode band: the lock table and the fencing
//! mint are process-global statics shared across this binary.

use squeezefs::dlm::{DlmClient, LockMode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

fn dlm() -> DlmClient {
    DlmClient::new().expect("local dlm")
}

/// CW arming is process-global: the CW tests take this.
static CW_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const MIB: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// 1. Disjoint ranges are genuinely concurrent
// ---------------------------------------------------------------------------

/// Contract 1 (RED pre-S11): two applications writing DIFFERENT regions of
/// one large file hold their leases at the same time.
///
/// The proof is a two-party barrier both tasks must reach **while holding**
/// their lease. Pre-S11 both ranges fold into one file entry, so the second
/// acquire waits out its whole budget and fails, the first task blocks in
/// the barrier, and the outer timeout fires. No sleeps: the only clocks are
/// the acquire budget and the failure timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disjoint_ranges_are_held_concurrently() {
    let path = "inode_71000001";
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut set = tokio::task::JoinSet::new();

    for (i, (start, end)) in [(0, 4 * MIB), (4 * MIB, 8 * MIB)].into_iter().enumerate() {
        let d = dlm(); // distinct client nonces: no owner-based exemption
        let barrier = barrier.clone();
        set.spawn(async move {
            let lease = d
                .acquire_lock(path, Some((start, end)), Duration::from_secs(3))
                .await
                .unwrap_or_else(|e| {
                    panic!("disjoint range {i} [{start},{end}) refused — ranges still serialize: {e:?}")
                });
            // BOTH leases are live here. If the manager serialized them the
            // other party never arrives and the outer timeout fires.
            barrier.wait().await;
            assert!(
                lease.is_held().await,
                "range {i} lease must still be held at the barrier"
            );
            lease.release().await.expect("release");
        });
    }

    timeout(Duration::from_secs(20), async {
        while let Some(joined) = set.join_next().await {
            joined.expect("range task panicked");
        }
    })
    .await
    .expect("disjoint byte ranges could not be held at once (they serialized)");
}

/// Contract 1 at a population: 16 disjoint 1 MiB ranges of one file, all
/// held simultaneously (16-party barrier), then all released. This is the
/// MPI-IO shape D8 names — and the live-range population the interval
/// structure is sized for.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixteen_disjoint_ranges_are_all_held_at_once() {
    let path = "inode_71000002";
    const N: u64 = 16;
    let barrier = Arc::new(tokio::sync::Barrier::new(N as usize));
    let mut set = tokio::task::JoinSet::new();

    for i in 0..N {
        let d = dlm();
        let barrier = barrier.clone();
        set.spawn(async move {
            let lease = d
                .acquire_lock(path, Some((i * MIB, (i + 1) * MIB)), Duration::from_secs(3))
                .await
                .unwrap_or_else(|e| panic!("range {i} refused: {e:?}"));
            barrier.wait().await;
            lease.release().await.expect("release");
        });
    }

    timeout(Duration::from_secs(30), async {
        while let Some(joined) = set.join_next().await {
            joined.expect("range task panicked");
        }
    })
    .await
    .expect("16 disjoint ranges of one file could not be held at once");
}

/// Contract 4: spans are `[start, end)` — **end exclusive**. Two ranges
/// that merely touch (`[0,4096)` and `[4096,8192)`) share no byte and must
/// not conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adjacent_ranges_do_not_conflict() {
    let path = "inode_71000003";
    let a = dlm();
    let b = dlm();
    let left = a
        .acquire_lock(path, Some((0, 4096)), Duration::from_secs(3))
        .await
        .expect("left range");
    let right = timeout(
        Duration::from_secs(5),
        b.acquire_lock(path, Some((4096, 8192)), Duration::from_secs(3)),
    )
    .await
    .expect("adjacent range acquire hung")
    .expect("adjacent (end-exclusive) ranges must not conflict");
    left.release().await.expect("release left");
    right.release().await.expect("release right");
}

// ---------------------------------------------------------------------------
// 2. Overlapping ranges arbitrate
// ---------------------------------------------------------------------------

/// Contract 2: a partially overlapping range must arbitrate — a bounded,
/// loud refusal while the holder holds (the budget is TIME: with no churn
/// anywhere a wakeup-counting waiter would hang forever), and a
/// long-budget waiter must be woken by the release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_ranges_serialize_and_wake() {
    let path = "inode_71000004";
    let holder = dlm();
    let contender = dlm();

    let held = holder
        .acquire_lock(path, Some((0, 8192)), Duration::from_secs(3))
        .await
        .expect("holder acquire");

    // (a) bounded refusal while held.
    match timeout(
        Duration::from_secs(3),
        contender.acquire_lock(path, Some((4096, 12288)), Duration::from_millis(300)),
    )
    .await
    {
        Err(_) => panic!("overlapping acquire hung unbounded (300 ms budget, no churn)"),
        Ok(Ok(_)) => panic!("overlapping ranges must arbitrate, not both grant"),
        Ok(Err(_)) => {}
    }

    // (b) a waiter with a real budget is woken by the release.
    let waiter = tokio::spawn({
        let contender = contender.clone();
        async move {
            contender
                .acquire_lock(path, Some((4096, 12288)), Duration::from_secs(10))
                .await
        }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    held.release().await.expect("holder release");
    let got = timeout(Duration::from_secs(8), waiter)
        .await
        .expect("overlapping waiter was never woken by the release")
        .expect("waiter task panicked")
        .expect("waiter acquire failed after the release");
    got.release().await.expect("waiter release");
}

/// Contract 4: identical ranges are the same span — always a conflict
/// (the pre-S11 behavior this suite must not weaken).
#[tokio::test]
async fn identical_ranges_conflict() {
    let path = "inode_71000005";
    let d = dlm();
    let e = dlm();
    let held = d
        .acquire_lock(path, Some((0, 4096)), Duration::from_secs(3))
        .await
        .expect("first acquire");
    match timeout(
        Duration::from_secs(3),
        e.acquire_lock(path, Some((0, 4096)), Duration::from_millis(300)),
    )
    .await
    {
        Err(_) => panic!("identical-range acquire hung unbounded"),
        Ok(Ok(_)) => panic!("an identical range must conflict while held"),
        Ok(Err(_)) => {}
    }
    held.release().await.expect("release");
}

/// Contract 2, containment face: a range wholly INSIDE a held range
/// conflicts (and the reverse — a superset of a held range).
#[tokio::test]
async fn contained_and_containing_ranges_conflict() {
    let path = "inode_71000006";
    let d = dlm();
    let e = dlm();

    let outer = d
        .acquire_lock(path, Some((0, 64 * 1024)), Duration::from_secs(3))
        .await
        .expect("outer acquire");
    assert!(
        e.acquire_lock(path, Some((4096, 8192)), Duration::from_millis(300))
            .await
            .is_err(),
        "a contained range must conflict with the enclosing holder"
    );
    outer.release().await.expect("release outer");

    let inner = d
        .acquire_lock(path, Some((4096, 8192)), Duration::from_secs(3))
        .await
        .expect("inner acquire");
    assert!(
        e.acquire_lock(path, Some((0, 64 * 1024)), Duration::from_millis(300))
            .await
            .is_err(),
        "a containing range must conflict with the contained holder"
    );
    inner.release().await.expect("release inner");
}

// ---------------------------------------------------------------------------
// 3. Whole-file vs range (RED pre-S11 — they were distinct lock objects)
// ---------------------------------------------------------------------------

/// Contract 3: a whole-file lease (`range: None`) conflicts with EVERY
/// range. Pre-S11 a range lock was a different key and was granted
/// straight through a held whole-file lease — the hole this closes.
#[tokio::test]
async fn whole_file_conflicts_with_every_range() {
    let path = "inode_71000007";
    let d = dlm();
    let e = dlm();

    let whole = d
        .acquire_lock(path, None, Duration::from_secs(3))
        .await
        .expect("whole-file acquire");

    for (start, end) in [(0, 4096), (7 * MIB, 9 * MIB), (u64::MAX - 1, u64::MAX)] {
        match timeout(
            Duration::from_secs(3),
            e.acquire_lock(path, Some((start, end)), Duration::from_millis(300)),
        )
        .await
        {
            Err(_) => panic!("range [{start},{end}) acquire hung unbounded"),
            Ok(Ok(_)) => panic!(
                "range [{start},{end}) was granted under a held WHOLE-FILE \
                 lease: whole-file custody must conflict with every range"
            ),
            Ok(Err(_)) => {}
        }
    }

    // Released whole-file custody frees every range.
    whole.release().await.expect("release whole");
    let range = timeout(
        Duration::from_secs(5),
        e.acquire_lock(path, Some((0, 4096)), Duration::from_secs(3)),
    )
    .await
    .expect("range acquire hung after the whole-file release")
    .expect("range acquire failed after the whole-file release");
    range.release().await.expect("release range");
}

/// Contract 3, reverse: a live range blocks whole-file acquisition, and
/// the range release wakes the whole-file waiter (the waiter stripe must
/// be keyed on the FILE identity, not on the range key).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_conflicts_with_whole_file_and_release_wakes_it() {
    let path = "inode_71000008";
    let d = dlm();
    let e = dlm();

    let range = d
        .acquire_lock(path, Some((MIB, 2 * MIB)), Duration::from_secs(3))
        .await
        .expect("range acquire");

    match timeout(
        Duration::from_secs(3),
        e.acquire_lock(path, None, Duration::from_millis(300)),
    )
    .await
    {
        Err(_) => panic!("whole-file acquire hung unbounded under a live range"),
        Ok(Ok(_)) => panic!("a whole-file lease must not be granted under a live range lease"),
        Ok(Err(_)) => {}
    }

    let waiter = tokio::spawn({
        let e = e.clone();
        async move { e.acquire_lock(path, None, Duration::from_secs(10)).await }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    range.release().await.expect("release range");
    let got = timeout(Duration::from_secs(8), waiter)
        .await
        .expect("whole-file waiter was never woken by the range release")
        .expect("waiter panicked")
        .expect("whole-file acquire failed after the range release");
    got.release().await.expect("release");
}

/// Contract 3/1 composed: whole-file custody is exclusive against a
/// POPULATION of live ranges — it is granted only after the last one
/// drops, and the ranges themselves never blocked each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn whole_file_waits_for_the_last_live_range() {
    let path = "inode_71000009";
    let d = dlm();
    let mut leases = Vec::new();
    for i in 0..8u64 {
        leases.push(
            d.acquire_lock(path, Some((i * MIB, (i + 1) * MIB)), Duration::from_secs(3))
                .await
                .unwrap_or_else(|e| panic!("range {i} refused: {e:?}")),
        );
    }

    let w = dlm();
    // Drop them one at a time. Every bounded whole-file attempt taken while
    // ANY range is still live must be refused — a deterministic probe, not a
    // "did the spawned waiter get polled yet" race.
    while !leases.is_empty() {
        assert!(
            w.acquire_lock(path, None, Duration::from_millis(150))
                .await
                .is_err(),
            "whole-file custody was granted while {} range(s) were still live",
            leases.len()
        );
        leases.pop().unwrap().release().await.expect("release");
    }

    let got = timeout(
        Duration::from_secs(5),
        w.acquire_lock(path, None, Duration::from_secs(3)),
    )
    .await
    .expect("whole-file acquire hung after the last range released")
    .expect("whole-file acquire failed after the last range released");
    got.release().await.expect("release");
}

// ---------------------------------------------------------------------------
// 4. Malformed spans
// ---------------------------------------------------------------------------

/// Contract 4: an empty (`start == end`) or inverted (`start > end`) span
/// protects nothing — a caller bug. Refuse loud instead of granting a lock
/// that arbitrates zero bytes.
#[tokio::test]
async fn malformed_spans_refuse_loud() {
    let path = "inode_71000010";
    let d = dlm();
    for (start, end) in [(4096, 4096), (8192, 4096), (u64::MAX, 0)] {
        let err = d
            .acquire_lock(path, Some((start, end)), Duration::from_secs(1))
            .await
            .err()
            .unwrap_or_else(|| {
                panic!("malformed span [{start},{end}) must refuse loud, not grant")
            });
        let msg = format!("{err}");
        assert!(
            msg.contains("range") || msg.contains("span"),
            "the refusal must name the malformed span: {msg}"
        );
    }
    // The malformed attempts must leave NO custody behind.
    let whole = timeout(
        Duration::from_secs(3),
        d.acquire_lock(path, None, Duration::from_secs(2)),
    )
    .await
    .expect("whole-file acquire hung after malformed span attempts")
    .expect("a refused malformed span must leave no residual custody");
    whole.release().await.expect("release");
}

// ---------------------------------------------------------------------------
// 5. Fencing semantics (S1/S2 preserved)
// ---------------------------------------------------------------------------

/// Contract 5: the file-identity read is EXACT while a whole-file lease is
/// held (S1's live-writer property — byte-identical), covers every live
/// range grant while ranges are held (ranges share the FILE's generator),
/// and never regresses after release (the `presented < current` stale-arm).
#[tokio::test]
async fn file_read_is_exact_while_whole_held_and_covers_range_grants() {
    let ino = 71_000_011u64;
    let path = format!("inode_{ino}");
    let d = dlm();

    // (a) whole-file held ⇒ exact.
    let whole = d
        .acquire_lock(&path, None, Duration::from_secs(3))
        .await
        .expect("whole acquire");
    assert_eq!(
        d.get_fencing_token_ino(ino),
        whole.fencing_token(),
        "a held whole-file lease's read must be its own token exactly \
         (S1: live writers are never fenced by a shared read surface)"
    );
    whole.release().await.expect("release whole");

    // (b) two live ranges ⇒ the file read covers both (shared generator).
    let r1 = d
        .acquire_lock(&path, Some((0, MIB)), Duration::from_secs(3))
        .await
        .expect("range 1");
    let r2 = d
        .acquire_lock(&path, Some((MIB, 2 * MIB)), Duration::from_secs(3))
        .await
        .expect("range 2");
    assert!(
        r2.fencing_token() > r1.fencing_token(),
        "the global mint is strictly monotone across range grants"
    );
    let read = d.get_fencing_token_ino(ino);
    assert_eq!(
        read,
        r2.fencing_token(),
        "with ranges live the file read is the file's NEWEST grant \
         (ranges share the file's generator — the ~24-site census reads \
         this surface): got {read}"
    );

    // (c) release must never lower it.
    let newest = r2.fencing_token();
    r2.release().await.expect("release r2");
    assert!(
        d.get_fencing_token_ino(ino) >= newest,
        "the file read regressed below a granted token after release: \
         the stale-reject arm dies"
    );
    r1.release().await.expect("release r1");
    assert!(
        d.get_fencing_token_ino(ino) >= newest,
        "the file read regressed below a granted token after the last release"
    );
    assert!(
        newest - 1 < d.get_fencing_token_ino(ino),
        "`presented < current` must still classify a stale token"
    );
}

/// Contract 5: strict global uniqueness + per-object monotonicity under
/// CONCURRENT range minting on one file — the S1 mint property that the
/// whole fencing census rests on, re-verified under range custody (where
/// grants on one object are no longer serialized by the lock itself).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn fencing_monotone_and_unique_under_concurrent_range_minting() {
    let ino = 71_000_012u64;
    let path = format!("inode_{ino}");
    const WORKERS: u64 = 8;
    const ROUNDS: u64 = 32;

    let tokens = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let read_floor = Arc::new(AtomicU64::new(0));
    let regressions = Arc::new(AtomicU64::new(0));
    let uncovered = Arc::new(AtomicU64::new(0));

    let mut set = tokio::task::JoinSet::new();
    for w in 0..WORKERS {
        let d = dlm();
        let path = path.clone();
        let tokens = tokens.clone();
        let read_floor = read_floor.clone();
        let regressions = regressions.clone();
        let uncovered = uncovered.clone();
        set.spawn(async move {
            for r in 0..ROUNDS {
                let start = (w * ROUNDS + r) * 4096;
                let lease = d
                    .acquire_lock(&path, Some((start, start + 4096)), Duration::from_secs(15))
                    .await
                    .expect("concurrent range acquire");
                let tok = lease.fencing_token();
                tokens.lock().unwrap().push(tok);
                // The file read must COVER this live grant (the stale-record
                // detection property: a record stamped `tok` can never look
                // newer than the file's current generation).
                let read = d.get_fencing_token_ino(ino);
                if read < tok {
                    uncovered.fetch_add(1, Ordering::Relaxed);
                }
                // ... and it must never regress.
                let prev = read_floor.fetch_max(read, Ordering::AcqRel);
                if read < prev {
                    regressions.fetch_add(1, Ordering::Relaxed);
                }
                lease.release().await.expect("release");
            }
        });
    }
    while let Some(joined) = set.join_next().await {
        joined.expect("minting worker panicked");
    }

    let all = tokens.lock().unwrap().clone();
    let unique: std::collections::HashSet<u64> = all.iter().copied().collect();
    assert_eq!(
        unique.len(),
        all.len(),
        "range grants must stay globally unique (the S1 single mint)"
    );
    assert_eq!(
        uncovered.load(Ordering::Relaxed),
        0,
        "the file read failed to cover a live range grant"
    );
    assert_eq!(
        regressions.load(Ordering::Relaxed),
        0,
        "the file read regressed under concurrent range minting"
    );
}

/// Contract 5: a range acquire that FAILS must burn no fencing token
/// (AGENTS.md lock-order law: "never burn fencing tokens on failed lock
/// acquisition") and must leave no custody residue.
#[tokio::test]
async fn refused_range_burns_no_token_and_leaves_no_residue() {
    let ino = 71_000_013u64;
    let path = format!("inode_{ino}");
    let d = dlm();
    let e = dlm();

    let held = d
        .acquire_lock(&path, Some((0, 8192)), Duration::from_secs(3))
        .await
        .expect("holder acquire");
    let before = d.get_fencing_token_ino(ino);
    assert!(
        e.acquire_lock(&path, Some((4096, 12288)), Duration::from_millis(200))
            .await
            .is_err(),
        "premise: the overlapping acquire is refused"
    );
    assert_eq!(
        d.get_fencing_token_ino(ino),
        before,
        "a refused acquire must not bump the object's generation"
    );
    held.release().await.expect("release");

    // No residue: the span is immediately acquirable.
    let after = timeout(
        Duration::from_secs(3),
        e.acquire_lock(&path, Some((4096, 12288)), Duration::from_secs(2)),
    )
    .await
    .expect("hung acquiring a span whose refused waiter left residue")
    .expect("refused attempts must leave no custody");
    after.release().await.expect("release");
}

/// Contract 5 (whole-file no-regression pin): with no range ever passed,
/// the whole-file path behaves exactly as before — self-exclusive,
/// strictly monotone across re-acquisition, exact read while held, and no
/// residue after release.
#[tokio::test]
async fn whole_file_only_behavior_is_unchanged() {
    let ino = 71_000_014u64;
    let path = format!("inode_{ino}");
    let d = dlm();
    let e = dlm();

    let l1 = d
        .acquire_lock(&path, None, Duration::from_secs(3))
        .await
        .expect("acquire 1");
    let t1 = l1.fencing_token();
    assert_eq!(d.get_fencing_token(&path), t1);
    assert!(
        e.acquire_lock(&path, None, Duration::from_millis(200))
            .await
            .is_err(),
        "whole-file locks stay mutually exclusive"
    );
    l1.release().await.expect("release 1");

    let l2 = d
        .acquire_lock(&path, None, Duration::from_secs(3))
        .await
        .expect("acquire 2");
    assert!(l2.fencing_token() > t1, "monotone across re-acquisition");
    assert_eq!(d.get_fencing_token_ino(ino), l2.fencing_token());
    drop(l2); // drop-release
    let l3 = timeout(
        Duration::from_secs(3),
        d.acquire_lock(&path, None, Duration::from_secs(2)),
    )
    .await
    .expect("reacquire hung after drop-release")
    .expect("reacquire failed after drop-release");
    l3.release().await.expect("release 3");
}

/// Contract 5: a large live-range population is retired completely —
/// acquire+release cycles leave the file with no custody at all (bounded
/// by CONCURRENTLY HELD leases, never by spans ever locked).
#[tokio::test]
async fn range_churn_leaves_no_custody_behind() {
    let path = "inode_71000015";
    let d = dlm();
    for i in 0..4096u64 {
        let lease = d
            .acquire_lock(
                path,
                Some((i * 4096, (i + 1) * 4096)),
                Duration::from_secs(3),
            )
            .await
            .expect("churn acquire");
        lease.release().await.expect("churn release");
    }
    // If a single span leaked, whole-file custody can never be granted.
    let whole = timeout(
        Duration::from_secs(3),
        d.acquire_lock(path, None, Duration::from_secs(2)),
    )
    .await
    .expect("whole-file acquire hung after range churn (leaked span)")
    .expect("whole-file acquire failed after range churn (leaked span)");
    whole.release().await.expect("release");
}

// ---------------------------------------------------------------------------
// 6. Lock modes — CW ships DISABLED (§6.7)
// ---------------------------------------------------------------------------

/// Contract 6: `LockMode::ConcurrentWrite` exists so S9/S11 can issue it,
/// but **no verb issues it yet**, so the public API refuses it loud. The
/// documented test seam (`test_arm_cw_mode`) is the only way in — the
/// no-dead-code exception shape the spec asks for.
#[tokio::test]
async fn cw_mode_is_refused_until_armed() {
    let _g = CW_SERIAL.lock().await;
    let path = "inode_71000016";
    let d = dlm();

    let err = d
        .acquire_lock_mode(
            path,
            Some((0, 4096)),
            LockMode::ConcurrentWrite,
            Duration::from_secs(1),
        )
        .await
        .err()
        .expect("CW must be refused while it ships disabled");
    let msg = format!("{err}");
    assert!(
        msg.contains("CW") || msg.contains("concurrent-write"),
        "the refusal must name CW: {msg}"
    );

    // EX through the same entry point is unaffected.
    let ex = d
        .acquire_lock_mode(
            path,
            Some((0, 4096)),
            LockMode::Exclusive,
            Duration::from_secs(2),
        )
        .await
        .expect("EX through acquire_lock_mode must work");
    ex.release().await.expect("release");

    // Armed: CW is grantable, and disarming restores the refusal.
    let prev = squeezefs::dlm::test_arm_cw_mode(true);
    assert!(!prev, "CW must ship disarmed");
    let cw = d
        .acquire_lock_mode(
            path,
            Some((0, 4096)),
            LockMode::ConcurrentWrite,
            Duration::from_secs(2),
        )
        .await
        .expect("armed CW must be grantable");
    cw.release().await.expect("release");
    assert!(
        squeezefs::dlm::test_arm_cw_mode(false),
        "disarm returns the armed state"
    );
    assert!(
        d.acquire_lock_mode(
            path,
            Some((0, 4096)),
            LockMode::ConcurrentWrite,
            Duration::from_millis(200)
        )
        .await
        .is_err(),
        "disarming must restore the refusal"
    );
}

/// Contract 6: the §6.7 compatibility matrix over the shipped modes —
/// CW‖CW may cover the SAME span concurrently (proven by a barrier both
/// hold across), while EX conflicts with CW in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_cw_grants_overlap_while_ex_still_conflicts() {
    let _g = CW_SERIAL.lock().await;
    let path = "inode_71000017";
    squeezefs::dlm::test_arm_cw_mode(true);

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut set = tokio::task::JoinSet::new();
    for i in 0..2 {
        let d = dlm();
        let barrier = barrier.clone();
        set.spawn(async move {
            let lease = d
                .acquire_lock_mode(
                    path,
                    Some((0, 64 * 1024)),
                    LockMode::ConcurrentWrite,
                    Duration::from_secs(3),
                )
                .await
                .unwrap_or_else(|e| panic!("CW holder {i} refused: {e:?}"));
            barrier.wait().await;
            lease.release().await.expect("release");
        });
    }
    let outcome = timeout(Duration::from_secs(20), async {
        while let Some(joined) = set.join_next().await {
            joined.expect("CW holder panicked");
        }
    })
    .await;
    assert!(
        outcome.is_ok(),
        "two CW grants over the same span must be compatible (§6.7 matrix)"
    );

    // EX vs CW, both directions.
    let a = dlm();
    let b = dlm();
    let cw = a
        .acquire_lock_mode(
            path,
            Some((0, 4096)),
            LockMode::ConcurrentWrite,
            Duration::from_secs(3),
        )
        .await
        .expect("CW acquire");
    assert!(
        b.acquire_lock(path, Some((0, 4096)), Duration::from_millis(200))
            .await
            .is_err(),
        "EX must conflict with a live CW grant"
    );
    cw.release().await.expect("release cw");

    let ex = a
        .acquire_lock(path, Some((0, 4096)), Duration::from_secs(3))
        .await
        .expect("EX acquire");
    assert!(
        b.acquire_lock_mode(
            path,
            Some((0, 4096)),
            LockMode::ConcurrentWrite,
            Duration::from_millis(200)
        )
        .await
        .is_err(),
        "CW must conflict with a live EX grant"
    );
    ex.release().await.expect("release ex");
    squeezefs::dlm::test_arm_cw_mode(false);
}

/// Contract 6: compatible WHOLE-FILE grants coexist and each retires
/// exactly itself. The custody table keeps whole-inode grants in a list
/// precisely so a second compatible grant cannot overwrite (and thereby
/// strand) the first — releasing one must leave the other held, and the
/// entry must be gone once both are.
#[tokio::test]
async fn armed_cw_whole_file_grants_coexist_and_both_retire() {
    let _g = CW_SERIAL.lock().await;
    let path = "inode_71000019";
    squeezefs::dlm::test_arm_cw_mode(true);

    let a = dlm();
    let b = dlm();
    let first = a
        .acquire_lock_mode(
            path,
            None,
            LockMode::ConcurrentWrite,
            Duration::from_secs(3),
        )
        .await
        .expect("first CW whole-file grant");
    let second = b
        .acquire_lock_mode(
            path,
            None,
            LockMode::ConcurrentWrite,
            Duration::from_secs(3),
        )
        .await
        .expect("second CW whole-file grant must be compatible");

    first.release().await.expect("release first");
    assert!(
        second.is_held().await,
        "retiring one whole-file grant must not strand the other"
    );
    // EX must still be refused while the survivor holds.
    assert!(
        a.acquire_lock(path, None, Duration::from_millis(200))
            .await
            .is_err(),
        "EX must conflict with the surviving CW whole-file grant"
    );
    second.release().await.expect("release second");

    squeezefs::dlm::test_arm_cw_mode(false);
    let ex = timeout(
        Duration::from_secs(3),
        a.acquire_lock(path, None, Duration::from_secs(2)),
    )
    .await
    .expect("EX acquire hung after both CW grants retired")
    .expect("both CW grants must have retired completely");
    ex.release().await.expect("release");
}

// ---------------------------------------------------------------------------
// 7. The W1 seventh clause's custody source
// ---------------------------------------------------------------------------

/// Contract 7: `span_range_shared` is what the W1 patch predicate's
/// seventh clause consults (`patch_ineligible_range_shared`).
///
/// - no custody at all ⇒ not range-shared (nothing to violate);
/// - a live WHOLE-FILE lease ⇒ not range-shared (whole-inode custody is
///   exactly what W1 requires — today's shipped shape, so the clause is
///   inert and the patch path is unchanged);
/// - a FOREIGN live range overlapping the span ⇒ range-shared;
/// - the writer's OWN range, fully covering the span ⇒ not shared;
/// - the writer's OWN range, only partially covering ⇒ shared (the patch
///   would mutate bytes outside its custody);
/// - a foreign range that does NOT overlap ⇒ not shared (this is what
///   makes S11's shared-file parallel write fast, not merely correct).
#[tokio::test]
async fn span_range_shared_classifies_custody() {
    let ino = 71_000_018u64;
    let path = format!("inode_{ino}");
    let d = dlm();
    let block = (0u64, 4 * MIB); // one 4 MiB block's span

    assert!(
        !squeezefs::dlm::span_range_shared(ino, block.0, block.1, 0),
        "an object with no live custody is not range-shared"
    );

    let whole = d
        .acquire_lock(&path, None, Duration::from_secs(3))
        .await
        .expect("whole acquire");
    assert!(
        !squeezefs::dlm::span_range_shared(ino, block.0, block.1, whole.fencing_token()),
        "a whole-file lease IS whole-inode custody: never range-shared"
    );
    whole.release().await.expect("release whole");

    // Foreign overlapping range.
    let foreign = d
        .acquire_lock(&path, Some((MIB, 2 * MIB)), Duration::from_secs(3))
        .await
        .expect("foreign range");
    assert!(
        squeezefs::dlm::span_range_shared(ino, block.0, block.1, 0),
        "a foreign range overlapping the block makes it range-shared"
    );
    // ... and non-overlapping spans of the same file stay unshared.
    assert!(
        !squeezefs::dlm::span_range_shared(ino, 8 * MIB, 12 * MIB, 0),
        "a range must only shadow the spans it actually overlaps"
    );
    foreign.release().await.expect("release foreign");

    // Own fully-covering range vs own partial range.
    let full = d
        .acquire_lock(&path, Some((0, 4 * MIB)), Duration::from_secs(3))
        .await
        .expect("own covering range");
    assert!(
        !squeezefs::dlm::span_range_shared(ino, block.0, block.1, full.fencing_token()),
        "the writer's own range fully covering the span is sole custody"
    );
    full.release().await.expect("release full");

    let partial = d
        .acquire_lock(&path, Some((0, MIB)), Duration::from_secs(3))
        .await
        .expect("own partial range");
    assert!(
        squeezefs::dlm::span_range_shared(ino, block.0, block.1, partial.fencing_token()),
        "a partial own range does not license mutating the whole block"
    );
    partial.release().await.expect("release partial");
}
