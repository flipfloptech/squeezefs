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

use squeezefs::dlm::{DlmClient, LockMode, RangeAcquired};
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
    let regressions = Arc::new(AtomicU64::new(0));
    let uncovered = Arc::new(AtomicU64::new(0));

    let mut set = tokio::task::JoinSet::new();
    for w in 0..WORKERS {
        let d = dlm();
        let path = path.clone();
        let tokens = tokens.clone();
        let regressions = regressions.clone();
        let uncovered = uncovered.clone();
        set.spawn(async move {
            // Monotonicity is only OBSERVABLE in one observer's program
            // order: the retired global fetch-max floor raced the read it
            // recorded (worker X reads 100, deschedules; Y reads-and-
            // records 101; X records 100 → a false "regression" of two
            // correctly-ordered reads — the 2026-08-10 gate flake, 7/10
            // red pre-fix). Each worker's successive reads of the
            // monotone generation must be non-decreasing; cross-worker
            // coverage is the `uncovered` check below (read ≥ the
            // worker's own LIVE token) plus global mint uniqueness.
            let mut my_floor = 0u64;
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
                // ... and it must never regress in this observer's order.
                if read < my_floor {
                    regressions.fetch_add(1, Ordering::Relaxed);
                }
                my_floor = my_floor.max(read);
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
        // `expect_err` since S9 gave `LockLease` a `Debug` impl (the lease is
        // the audit surface a custody refusal is read beside).
        .expect_err("CW must be refused while it ships disabled");
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

// ===========================================================================
// DLM stage **S11 rung 15** — KD-MW-7: distributed byte-range custody ON THE
// WIRE (`docs/design-full-multi-writer.md` §9.2 + PR-plan row 15).
//
// The local algebra above is S11's single-node half; this section takes it
// to the wire: range grants ride the S9 custody lease, the client caches its
// granted spans in the S8 token cache, and the authority's admit gains the
// §9.2 bounds law — required/desired, admit-time coalescing, the
// geometry-derived per-file span cap `max(16, ceil(size / block_size))`, and
// the `dlm_grant_table_bytes` R5 byte-budget ceiling with refuse-loud (no
// free span constants — Issue-19's law).
//
// Contracts (numbering continues the local half's):
//
//  8. **Required/desired admit** — the grant is the largest desired-subset
//     that conflicts with nothing, NEVER less than required: a foreign
//     grant inside desired TRIMS desired (`range_custody_desired_trims`);
//     a foreign grant overlapping REQUIRED refuses (never a silent trim).
//  9. **Admit-time coalescing convergence** (the adversarial tiny-ranges
//     shape — charter red-first pin b): N tiny ADJACENT asks from one
//     holder converge to O(1) live spans under ONE surviving token, and
//     the merged custody retires as a unit.
// 10. **At-budget refusal names the arithmetic** (charter pin a): a
//     new-span admit past the `dlm_grant_table_bytes` R5 share refuses
//     LOUD, counted `range_custody_cap_refusals`, converging by release —
//     required is never silently trimmed to fit.
// 11. **The block-cyclic shape** (charter pin c — rung 18's acceptance
//     shape, pinned now): K holders, holder i takes blocks i, i+K, i+2K…
//     of one large file — grants ≈ blocks-in-file with ZERO refusals below
//     budget (the Issue-19 class: a constant refusing the workload S11
//     exists for).
// 12. **The geometry cap** — `max(16, ceil(size/block_size))` spans per
//     file: floor 16 is transient pre-coalesce headroom for sub-16-block
//     files (refuses the 17th non-coalescible span, loud); a larger file's
//     cap is its own block count (the 17th span admits).
// 13. **Pull-revocation + T_self** — the custody-lease machinery verbatim:
//     a revoked holder learns at its next renewal, every adopted range
//     lease reads dead, the client range cache empties, and the custody
//     generation advances (never poisons); T_self stays strictly earlier
//     than the owner's deadline.
// 14. **Era fencing** — a stale lease epoch's range acquire refuses
//     UNKNOWN_LEASE; a successor era's grants dominate every prior era's
//     tokens (ranges share the file's generator — no new token algebra).
// 15. **The range vector on the lease** — the renewal reply carries the
//     client's live range grants, and the client cache REBUILDS from it
//     (the revalidation surface).
// 16. **The client range cache** (S8 token cache extension) — covering
//     probes serve the granted token locally; `dlm_token_cache_bytes`
//     accounts range-span state.
// 17. **Dark posture** — `SQUEEZEFS_RANGE_CUSTODY` is registered (ENG-10:
//     Kind::Bool, static default OFF until rungs 16/17 — the 2026-08-17
//     adjudication) and read ONLY when the mw plane is
//     armed (and defaults OFF until rungs 16/17 land the publish
//     composition — adjudicated 2026-08-17): on an unarmed process the
//     lever answers false even when forced (the `delegation_enabled` law).
// 18. **R5 registration** — `dlm_grant_table_bytes` (authority-side) and
//     `dlm_token_cache_bytes` (client-side) are registered R5 components.
// ===========================================================================

use squeezefs::cluster_wire as cw;
use squeezefs::data_grant::{self, RangeAcquireOutcome, WriteCustodyClient, WriteCustodyOwner};
use squeezefs::membership::{LeaseClock, LeaseClocks};

/// The wire tests' storage-trust secret (S3's root of trust).
const RANGE_SECRET: &[u8] = b"s11-range-wire-storage-trust-secret";

/// 4 MiB — the shipped block size the desired-rounding doctrine aligns to.
const BLK: u64 = 4 * MIB;

/// Process-global range-custody state (the grant-table gauge, the budget
/// seam, the range counters, the client range cache) forces the counting
/// tests to serialize even in parallel dev runs (the gate runs
/// `--test-threads=1`; this keeps dev runs deterministic too).
static RANGE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Deterministic lease clocks (the S9 suite's law: T_self strictly earlier
/// than the owner's TTL by construction).
fn range_clocks() -> (LeaseClocks, LeaseClock) {
    let clocks = LeaseClocks::with_params(
        Duration::from_millis(3_000),
        Duration::from_millis(200),
        Duration::from_millis(400),
    )
    .expect("2*skew + purge < TTL");
    (clocks, LeaseClock::monotonic())
}

/// A fixed-geometry source: every ino reads as one `size`-byte file of
/// `BLK`-byte blocks — the injectable seam the mount arm fills with the
/// real metadata lookup.
fn fixed_geometry(size: u64) -> Arc<dyn data_grant::RangeGeometry> {
    data_grant::fixed_range_geometry(size, BLK)
}

/// One armed authority + listener (no meta backend, no quarantine — the
/// custody plane alone), geometry installed.
fn range_authority(size: u64) -> (Arc<cw::RpcListener>, Arc<WriteCustodyOwner>, String) {
    let (clocks, clock) = range_clocks();
    let owner = WriteCustodyOwner::arm(
        "s11-owner",
        squeezefs::dlm::durable_term() + 1,
        squeezefs::dlm::durable_term(),
        clocks,
        clock,
        None,
    )
    .expect("the custody authority arms");
    owner.install_range_geometry(fixed_geometry(size));
    let router = data_grant::AsyncVerbRouter::new().with_custody(Arc::clone(&owner));
    let cfg = cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    };
    let listener = cw::RpcListener::start_async(cfg, RANGE_SECRET.to_vec(), Arc::new(router))
        .expect("the S11 authority listens");
    let endpoint = listener.endpoint().to_string();
    (listener, owner, endpoint)
}

async fn range_client(endpoint: &str, id: &str) -> Arc<WriteCustodyClient> {
    WriteCustodyClient::connect(endpoint, RANGE_SECRET, id)
        .await
        .expect("a co-writer dials the S11 authority")
}

// ---------------------------------------------------------------------------
// 8. Required/desired admit (local algebra half — the wire rides it)
// ---------------------------------------------------------------------------

/// Contract 8: the grant is the largest desired-subset conflicting with
/// nothing, never less than required. A foreign grant inside desired trims
/// desired (counted); a foreign grant overlapping REQUIRED refuses loud —
/// a silent trim of required is the §9.2 forbidden answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn required_desired_admit_never_trims_required() {
    let _g = RANGE_SERIAL.lock().await;
    let path = "inode_72000001";
    let d = dlm();
    let e = dlm();
    let geometry = Some((64 * BLK, BLK));

    // (a) an uncontended ask gets its full block-aligned desired.
    let required = (5 * MIB, 6 * MIB);
    let desired = squeezefs::dlm::block_align_out(required, BLK);
    assert_eq!(desired, (4 * MIB, 8 * MIB), "outward block alignment");
    let a = d
        .acquire_lock_range(path, required, desired, Duration::from_secs(3), geometry)
        .await
        .expect("uncontended required/desired acquire");
    let (a_lease, a_span) = match a {
        RangeAcquired::New { lease, span } => (lease, span),
        other => panic!("first ask on a fresh file must be a NEW grant, got {other:?}"),
    };
    assert_eq!(
        a_span, desired,
        "an uncontended ask is granted its full desired window"
    );

    // (b) a FOREIGN ask whose desired overlaps the live grant is TRIMMED to
    // its conflict-free subset — but never below required.
    let trims_before = squeezefs::dlm::range_custody_stats().desired_trims;
    let b = e
        .acquire_lock_range(
            path,
            (9 * MIB, 10 * MIB),
            (4 * MIB, 12 * MIB),
            Duration::from_secs(3),
            geometry,
        )
        .await
        .expect("a foreign ask whose REQUIRED is free must grant");
    let (b_lease, b_span) = match b {
        RangeAcquired::New { lease, span } => (lease, span),
        other => panic!("the foreign ask must be a NEW grant, got {other:?}"),
    };
    assert!(
        b_span.0 >= 8 * MIB,
        "desired must be trimmed off the live foreign grant [4M,8M): got {b_span:?}"
    );
    assert!(
        b_span.0 <= 9 * MIB && b_span.1 >= 10 * MIB,
        "the granted span must still cover required [9M,10M): got {b_span:?}"
    );
    assert!(
        squeezefs::dlm::range_custody_stats().desired_trims > trims_before,
        "a trimmed desired must be counted (range_custody_desired_trims)"
    );

    // (c) a foreign ask whose REQUIRED overlaps live custody REFUSES —
    // never a silent trim of required.
    let err = e
        .acquire_lock_range(
            path,
            (7 * MIB, 9 * MIB),
            (4 * MIB, 12 * MIB),
            Duration::from_millis(200),
            geometry,
        )
        .await
        .expect_err("required overlapping foreign custody must refuse, never trim");
    let msg = format!("{err}");
    assert!(
        msg.contains("required") || msg.contains("held") || msg.contains("conflict"),
        "the refusal must name the conflict: {msg}"
    );

    a_lease.release().await.expect("release a");
    b_lease.release().await.expect("release b");
}

// ---------------------------------------------------------------------------
// 9. Admit-time coalescing (the adversarial tiny-ranges shape) — pin (b)
// ---------------------------------------------------------------------------

/// Contract 9 (charter red-first pin b): 512 byte-granular ADJACENT asks
/// from one holder converge to ONE live span under ONE surviving token —
/// the admit-time merge rides the existing O(n) admit, the table stays
/// O(1), and the merged custody retires as a unit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adversarial_tiny_adjacent_asks_coalesce_to_one_span() {
    let _g = RANGE_SERIAL.lock().await;
    let ino = 72_000_002u64;
    let path = format!("inode_{ino}");
    let d = dlm();
    let geometry = Some((64 * BLK, BLK));

    let first = d
        .acquire_lock_range(
            &path,
            (0, 4096),
            (0, 4096),
            Duration::from_secs(3),
            geometry,
        )
        .await
        .expect("first tiny ask");
    let (lease, mut span, token) = match first {
        RangeAcquired::New { lease, span } => {
            let t = lease.fencing_token();
            (lease, span, t)
        }
        other => panic!("first tiny ask must be NEW, got {other:?}"),
    };

    let mut extensions = 0u64;
    for i in 1..512u64 {
        let req = (i * 4096, (i + 1) * 4096);
        match d
            .acquire_lock_range(&path, req, req, Duration::from_secs(3), geometry)
            .await
            .unwrap_or_else(|e| panic!("adjacent tiny ask {i} refused: {e:?}"))
        {
            RangeAcquired::Extended { token: t, span: s } => {
                assert_eq!(
                    t, token,
                    "an admit-time merge extends the SURVIVING grant — same token"
                );
                assert!(s.1 >= req.1 && s.0 == 0, "the union widens: {s:?}");
                span = s;
                extensions += 1;
            }
            other => panic!(
                "adjacent same-holder ask {i} must COALESCE (extend), got {other:?} — \
                 the table is filling with per-ask spans (the Issue-19 bounds failure)"
            ),
        }
    }
    assert_eq!(extensions, 511);
    assert_eq!(span, (0, 512 * 4096), "the converged union");
    assert_eq!(
        squeezefs::dlm::live_range_records(ino),
        1,
        "512 tiny adjacent asks must converge to ONE live span (admit-time coalescing)"
    );

    // The merged custody is real custody: whole-file conflicts while held…
    assert!(
        d.acquire_lock(&path, None, Duration::from_millis(150))
            .await
            .is_err(),
        "whole-file custody must conflict with the coalesced span"
    );
    // …and retires AS A UNIT with the one surviving lease.
    lease.release().await.expect("release the merged grant");
    assert_eq!(
        squeezefs::dlm::live_range_records(ino),
        0,
        "the merged grant must retire completely with its one lease"
    );
    let whole = timeout(
        Duration::from_secs(3),
        d.acquire_lock(&path, None, Duration::from_secs(2)),
    )
    .await
    .expect("whole-file acquire hung after the merged release")
    .expect("no residue after the merged release");
    whole.release().await.expect("release whole");
}

// ---------------------------------------------------------------------------
// 10. The R5 byte-budget ceiling — refuse-loud naming the arithmetic (pin a)
// ---------------------------------------------------------------------------

/// Contract 10 (charter red-first pin a): a NEW-span admit past the
/// `dlm_grant_table_bytes` R5 share refuses LOUD — the message names the
/// live bytes, the record cost and the share (the fleet-share refusal
/// precedent) — counted `range_custody_cap_refusals`, and the table
/// converges by release, never by trimming required.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_budget_new_span_refuses_loud_naming_the_arithmetic() {
    let _g = RANGE_SERIAL.lock().await;
    let ino = 72_000_003u64;
    let path = format!("inode_{ino}");
    let d = dlm();
    let geometry = Some((256 * BLK, BLK));

    // Clamp the share to the live table + 2 records (the seam — production
    // derives from the R5 budget).
    let rec = squeezefs::dlm::RANGE_GRANT_RECORD_BYTES;
    let budget = squeezefs::dlm::grant_table_bytes() + 2 * rec;
    let prev = squeezefs::dlm::test_swap_range_table_budget(Some(budget));

    let refusals_before = squeezefs::dlm::range_custody_stats().cap_refusals;
    let mut held = Vec::new();
    let mut refusal = None;
    for i in 0..4u64 {
        // Disjoint, non-adjacent spans (never coalescible): 4 KiB asks a
        // block apart.
        let req = (i * BLK, i * BLK + 4096);
        match d
            .acquire_lock_range(&path, req, req, Duration::from_millis(300), geometry)
            .await
        {
            Ok(RangeAcquired::New { lease, .. }) => held.push(lease),
            Ok(other) => panic!("non-adjacent spans cannot coalesce, got {other:?}"),
            Err(e) => {
                refusal = Some(format!("{e}"));
                break;
            }
        }
    }
    let msg =
        refusal.expect("the at-budget new-span admit must REFUSE — it granted past the share");
    assert!(
        held.len() <= 2,
        "at most 2 records fit the clamped share, {} were granted",
        held.len()
    );
    assert!(
        msg.contains("dlm_grant_table_bytes"),
        "the refusal must name the gauge: {msg}"
    );
    assert!(
        msg.contains(&rec.to_string()),
        "the refusal must name the per-record cost ({rec} B): {msg}"
    );
    assert!(
        msg.contains(&budget.to_string()),
        "the refusal must name the share ({budget} B) — the budget arithmetic: {msg}"
    );
    assert!(
        squeezefs::dlm::range_custody_stats().cap_refusals > refusals_before,
        "the refusal must be counted (range_custody_cap_refusals)"
    );
    // Required was refused whole, never trimmed: nothing partial is live.
    assert_eq!(
        squeezefs::dlm::live_range_records(ino),
        held.len(),
        "a refused acquire must leave NO partial custody (never a silent trim)"
    );

    // Converge by RELEASE: retiring one grant makes the refused span
    // admittable again.
    if let Some(lease) = held.pop() {
        lease.release().await.expect("release one");
    }
    let again = d
        .acquire_lock_range(
            &path,
            (3 * BLK, 3 * BLK + 4096),
            (3 * BLK, 3 * BLK + 4096),
            Duration::from_millis(500),
            geometry,
        )
        .await
        .expect("the budget converges by release");
    match again {
        RangeAcquired::New { lease, .. } => lease.release().await.expect("release"),
        other => panic!("expected a NEW grant post-release, got {other:?}"),
    }
    for lease in held {
        lease.release().await.expect("release");
    }
    squeezefs::dlm::test_swap_range_table_budget(prev);
}

// ---------------------------------------------------------------------------
// 11. The block-cyclic shape (pin c — the Issue-19 class, adjudicated now)
// ---------------------------------------------------------------------------

/// Contract 11 (charter red-first pin c): the MPI-IO block-cyclic
/// decomposition — K holders, holder i takes blocks i, i+K, i+2K… — is the
/// legitimate NON-coalescible shape: spans are never adjacent per holder by
/// construction, so the table legitimately approaches one span per block.
/// It must grant ≈ blocks-in-file with ZERO cap refusals below the byte
/// budget — any refusal here is the Issue-19 class (a constant refusing the
/// workload S11 exists for).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn block_cyclic_shape_grants_one_span_per_block_with_zero_refusals() {
    let _g = RANGE_SERIAL.lock().await;
    let ino = 72_000_004u64;
    let path = format!("inode_{ino}");
    const BLOCKS: u64 = 256;
    const K: u64 = 4;
    let geometry = Some((BLOCKS * BLK, BLK));

    let refusals_before = squeezefs::dlm::range_custody_stats().cap_refusals;
    let grants_before = squeezefs::dlm::range_custody_stats().grants;
    let bytes_before = squeezefs::dlm::grant_table_bytes();

    let mut set = tokio::task::JoinSet::new();
    for holder in 0..K {
        let d = dlm();
        let path = path.clone();
        set.spawn(async move {
            let mut leases = Vec::new();
            let mut block = holder;
            while block < BLOCKS {
                let req = (block * BLK, (block + 1) * BLK);
                let got = d
                    .acquire_lock_range(&path, req, req, Duration::from_secs(10), geometry)
                    .await
                    .unwrap_or_else(|e| {
                        panic!(
                            "block-cyclic holder {holder} block {block} REFUSED below budget \
                             — the Issue-19 class: {e:?}"
                        )
                    });
                match got {
                    RangeAcquired::New { lease, span } => {
                        assert_eq!(span, req, "block-aligned required == granted");
                        leases.push(lease);
                    }
                    other => panic!(
                        "holder {holder} block {block}: round-robin spans are never \
                         adjacent per holder — nothing may coalesce, got {other:?}"
                    ),
                }
                block += K;
            }
            leases
        });
    }
    let mut all = Vec::new();
    while let Some(joined) = set.join_next().await {
        all.extend(joined.expect("block-cyclic holder panicked"));
    }

    assert_eq!(all.len() as u64, BLOCKS, "grants ≈ blocks-in-file, exactly");
    assert_eq!(
        squeezefs::dlm::live_range_records(ino),
        BLOCKS as usize,
        "one live span per block (the legitimate non-coalescible population)"
    );
    let stats = squeezefs::dlm::range_custody_stats();
    assert_eq!(
        stats.cap_refusals, refusals_before,
        "ZERO cap refusals below budget on the block-cyclic shape (Issue-19)"
    );
    assert_eq!(
        stats.grants - grants_before,
        BLOCKS,
        "grants delta accounts"
    );
    let span_bytes = BLOCKS * squeezefs::dlm::RANGE_GRANT_RECORD_BYTES;
    assert!(
        squeezefs::dlm::grant_table_bytes() >= bytes_before + span_bytes,
        "dlm_grant_table_bytes must account ≈ spans × record bytes"
    );
    for lease in all {
        lease.release().await.expect("release");
    }
    assert_eq!(
        squeezefs::dlm::live_range_records(ino),
        0,
        "the block-cyclic population retires completely"
    );
}

// ---------------------------------------------------------------------------
// 12. The geometry-derived per-file span cap
// ---------------------------------------------------------------------------

/// Contract 12: the per-file cap is the file's OWN geometry —
/// `max(16, ceil(size / block_size))` spans. On a sub-16-block file the
/// floor (16 — transient pre-coalesce headroom) governs and the 17th
/// non-coalescible span refuses LOUD naming the geometry; on a larger file
/// the block count governs and the 17th span admits. No free constants.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn geometry_cap_refuses_the_seventeenth_span_on_a_small_file() {
    let _g = RANGE_SERIAL.lock().await;
    let d = dlm();

    assert_eq!(squeezefs::dlm::range_span_cap(8 * BLK, BLK), 16, "floor");
    assert_eq!(
        squeezefs::dlm::range_span_cap(256 * BLK, BLK),
        256,
        "geometry"
    );
    assert_eq!(
        squeezefs::dlm::range_span_cap(256 * BLK + 1, BLK),
        257,
        "ceil, never floor-divide"
    );

    // (a) an 8-block file: cap = max(16, 8) = 16.
    let path = "inode_72000005";
    let small = Some((8 * BLK, BLK));
    let mut held = Vec::new();
    for i in 0..16u64 {
        // 4 KiB asks 8 KiB apart: disjoint, non-adjacent, never coalescible.
        let req = (i * 8192, i * 8192 + 4096);
        match d
            .acquire_lock_range(path, req, req, Duration::from_secs(3), small)
            .await
            .unwrap_or_else(|e| panic!("span {i} of 16 refused under the floor: {e:?}"))
        {
            RangeAcquired::New { lease, .. } => held.push(lease),
            other => panic!("non-adjacent spans cannot coalesce, got {other:?}"),
        }
    }
    let req17 = (16 * 8192, 16 * 8192 + 4096);
    let err = d
        .acquire_lock_range(path, req17, req17, Duration::from_millis(300), small)
        .await
        .expect_err("the 17th non-coalescible span on a 16-cap file must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("16") && (msg.contains("cap") || msg.contains("geometry")),
        "the refusal must name the geometry cap: {msg}"
    );
    let refusals = squeezefs::dlm::range_custody_stats().cap_refusals;
    assert!(refusals > 0, "the cap refusal is counted");

    // (b) the same 17th span on a 256-block file admits — the cap is the
    // file's own geometry, never a constant.
    for lease in held.drain(..) {
        lease.release().await.expect("release");
    }
    let big = Some((256 * BLK, BLK));
    for i in 0..17u64 {
        let req = (i * 8192, i * 8192 + 4096);
        match d
            .acquire_lock_range(path, req, req, Duration::from_secs(3), big)
            .await
            .unwrap_or_else(|e| panic!("span {i} of 17 refused under a 256 cap: {e:?}"))
        {
            RangeAcquired::New { lease, .. } => held.push(lease),
            other => panic!("non-adjacent spans cannot coalesce, got {other:?}"),
        }
    }
    for lease in held {
        lease.release().await.expect("release");
    }
}

// ---------------------------------------------------------------------------
// 8/9/16 on the WIRE: required/desired + coalescing + the client range cache
// ---------------------------------------------------------------------------

/// Contracts 8+9+16, wire face: a co-writer's `acquire_range` rides the S9
/// custody lease — the first ask is a NEW grant (adopted locally, cached in
/// the S8 token cache's range extension), adjacent asks EXTEND it (same
/// grant_id, same token, widened span, ONE grant on the authority), and the
/// client range cache serves covering probes locally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wire_required_desired_rides_the_custody_lease_and_coalesces() {
    let _g = RANGE_SERIAL.lock().await;
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let (_l, owner, endpoint) = range_authority(64 * BLK);
    let client = range_client(&endpoint, "s11-cw-a").await;
    let ino = 72_000_010u64;

    // First ask: required is one sub-block window, desired its block.
    let required = (MIB, 2 * MIB);
    let desired = squeezefs::dlm::block_align_out(required, BLK);
    let held0 = owner.held();
    let got = client
        .acquire_range(ino, required, desired, Duration::from_secs(3))
        .await
        .expect("the first wire range acquire");
    let (lease, span0, grant_id, token) = match got {
        RangeAcquireOutcome::New {
            lease,
            span,
            grant_id,
        } => {
            let t = lease.fencing_token();
            (lease, span, grant_id, t)
        }
        other => panic!("first ask must be NEW, got {other:?}"),
    };
    assert_eq!(span0, (0, BLK), "block-aligned desired granted whole");
    assert!(lease.is_held().await, "the adopted range lease is held");
    assert_eq!(owner.held(), held0 + 1, "one grant on the authority");

    // The client range cache serves a covering probe locally (the
    // ≥99.5%-local law's mechanism).
    assert_eq!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, MIB, 2 * MIB),
        Some(token),
        "a covering probe must serve the granted token from the client cache"
    );
    assert_eq!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, 0, 2 * BLK),
        None,
        "a probe past the granted span must miss"
    );

    // Adjacent asks EXTEND the same grant: same id, same token, wider span,
    // still ONE grant on the authority (the adversarial shape's wire face).
    for i in 1..8u64 {
        let req = (i * BLK, i * BLK + 4096);
        let des = squeezefs::dlm::block_align_out(req, BLK);
        match client
            .acquire_range(ino, req, des, Duration::from_secs(3))
            .await
            .unwrap_or_else(|e| panic!("adjacent wire ask {i} refused: {e:?}"))
        {
            RangeAcquireOutcome::Extended {
                token: t,
                span,
                grant_id: g,
            } => {
                assert_eq!(t, token, "the extension keeps the surviving token");
                assert_eq!(g, grant_id, "the extension keeps the grant identity");
                assert_eq!(span, (0, (i + 1) * BLK), "the union widens block-wise");
            }
            other => panic!("adjacent wire ask {i} must EXTEND, got {other:?}"),
        }
    }
    assert_eq!(
        owner.held(),
        held0 + 1,
        "8 adjacent asks converge to ONE grant on the authority"
    );
    assert_eq!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, 0, 8 * BLK),
        Some(token),
        "the client cache widened with the extension"
    );

    // The widened custody classifies for W1's clause 7: the whole span is
    // solely owned, a foreign sub-span is not shared.
    assert!(
        !squeezefs::dlm::span_range_shared(ino, 2 * BLK, 3 * BLK, token),
        "the holder's widened grant covers the block — not range-shared"
    );

    lease.release().await.expect("release");
    client.drain_releases().await;
    assert_eq!(owner.held(), held0, "the merged grant retires as a unit");
}

/// Contract 11, wire face (rung 18's acceptance shape pinned at the wire):
/// 2 clients take alternating blocks of one file over the wire — every
/// block grants, zero refusals, the authority's table carries one span per
/// block, and the two custodies never conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn wire_block_cyclic_shape_grants_every_block_with_zero_refusals() {
    let _g = RANGE_SERIAL.lock().await;
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    const BLOCKS: u64 = 32;
    let (_l, owner, endpoint) = range_authority(BLOCKS * BLK);
    let ino = 72_000_011u64;
    let refusals_before = squeezefs::dlm::range_custody_stats().cap_refusals;

    let a = range_client(&endpoint, "s11-cyclic-a").await;
    let b = range_client(&endpoint, "s11-cyclic-b").await;
    let held0 = owner.held();

    let mut leases = Vec::new();
    for block in 0..BLOCKS {
        let client = if block % 2 == 0 { &a } else { &b };
        let req = (block * BLK, (block + 1) * BLK);
        match client
            .acquire_range(ino, req, req, Duration::from_secs(5))
            .await
            .unwrap_or_else(|e| panic!("wire block-cyclic block {block} refused: {e:?}"))
        {
            RangeAcquireOutcome::New { lease, span, .. } => {
                assert_eq!(span, req);
                leases.push(lease);
            }
            other => panic!("alternating blocks are never same-holder-adjacent — got {other:?}"),
        }
    }
    assert_eq!(owner.held(), held0 + BLOCKS as usize);
    assert_eq!(
        squeezefs::dlm::range_custody_stats().cap_refusals,
        refusals_before,
        "zero cap refusals below budget (Issue-19, wire face)"
    );
    for lease in leases {
        lease.release().await.expect("release");
    }
    a.drain_releases().await;
    b.drain_releases().await;
    assert_eq!(owner.held(), held0, "the wire population retires");
}

/// Contract 10, wire face: the at-budget refusal TRAVELS — the co-writer's
/// acquire fails loud with the budget arithmetic in the refusal text (never
/// a trimmed grant), and a peer's release converges it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wire_at_budget_refusal_travels_loud_and_converges_by_release() {
    let _g = RANGE_SERIAL.lock().await;
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let (_l, _owner, endpoint) = range_authority(256 * BLK);
    let client = range_client(&endpoint, "s11-budget-cw").await;
    let ino = 72_000_012u64;

    let rec = squeezefs::dlm::RANGE_GRANT_RECORD_BYTES;
    let budget = squeezefs::dlm::grant_table_bytes() + rec;
    let prev = squeezefs::dlm::test_swap_range_table_budget(Some(budget));

    // One span fits…
    let req0 = (0, 4096);
    let first = client
        .acquire_range(ino, req0, req0, Duration::from_secs(3))
        .await
        .expect("one record fits the clamped share");
    let lease = match first {
        RangeAcquireOutcome::New { lease, .. } => lease,
        other => panic!("expected NEW, got {other:?}"),
    };
    // …the second (non-coalescible) refuses with the arithmetic.
    let req1 = (BLK, BLK + 4096);
    let err = client
        .acquire_range(ino, req1, req1, Duration::from_millis(500))
        .await
        .expect_err("the at-budget wire acquire must refuse loud");
    let msg = format!("{err}");
    assert!(
        msg.contains("dlm_grant_table_bytes"),
        "the wire refusal must carry the budget arithmetic: {msg}"
    );

    // Converge by release.
    lease.release().await.expect("release");
    client.drain_releases().await;
    let again = client
        .acquire_range(ino, req1, req1, Duration::from_secs(3))
        .await
        .expect("the budget converges by release, wire face");
    if let RangeAcquireOutcome::New { lease, .. } = again {
        lease.release().await.expect("release");
        client.drain_releases().await;
    }
    squeezefs::dlm::test_swap_range_table_budget(prev);
}

// ---------------------------------------------------------------------------
// 13/14. Pull-revocation + T_self; era fencing
// ---------------------------------------------------------------------------

/// Contract 13: revocation is PULL — a revoked holder learns at its next
/// renewal (the custody-lease machinery verbatim): every adopted range
/// lease reads dead, the client range cache empties, the custody generation
/// ADVANCES (never poisons), and T_self stays strictly earlier than the
/// owner's deadline (the S6 law the range plane composes with).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_revocation_retires_range_grants_and_composes_with_t_self() {
    let _g = RANGE_SERIAL.lock().await;
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let (_l, owner, endpoint) = range_authority(64 * BLK);
    let client = range_client(&endpoint, "s11-revoked-cw").await;
    let ino = 72_000_013u64;

    // T_self composition: the client's own deadline is strictly earlier
    // than the owner's re-grant instant — the bound pull-revocation rides.
    let owner_deadline = owner
        .lease_deadline_ms(client.id())
        .expect("the joined client has an owner-side deadline");
    assert!(
        client.t_self_deadline_ms() < owner_deadline,
        "T_self must be strictly earlier than the owner's deadline (S6 law): \
         {} vs {owner_deadline}",
        client.t_self_deadline_ms()
    );

    let r1 = client
        .acquire_range(ino, (0, BLK), (0, BLK), Duration::from_secs(3))
        .await
        .expect("range 1");
    let r2 = client
        .acquire_range(
            ino,
            (2 * BLK, 3 * BLK),
            (2 * BLK, 3 * BLK),
            Duration::from_secs(3),
        )
        .await
        .expect("range 2");
    let (l1, l2) = match (r1, r2) {
        (
            RangeAcquireOutcome::New { lease: l1, .. },
            RangeAcquireOutcome::New { lease: l2, .. },
        ) => (l1, l2),
        other => panic!("two disjoint non-adjacent asks must be NEW grants: {other:?}"),
    };
    assert!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, 0, BLK).is_some(),
        "the cache serves before the revocation"
    );

    let gen_before = squeezefs::data_custody::custody_generation();
    let dead = owner.revoke_client(client.id(), "operator revoke (contract 13)");
    assert_eq!(
        dead.len(),
        1,
        "one dead custody cohort for the whole client"
    );

    // The pull channel: the next renewal answers "not custody".
    client
        .renew_all()
        .await
        .expect_err("a revoked lease's renewal must refuse (the pull channel)");
    assert!(!l1.is_held().await, "revoked range lease 1 reads dead");
    assert!(!l2.is_held().await, "revoked range lease 2 reads dead");
    assert!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, 0, BLK).is_none(),
        "the client range cache empties with the lease (a dead grant must \
         never serve a covering probe)"
    );
    assert!(
        squeezefs::data_custody::custody_generation() > gen_before,
        "losing custody ADVANCES the generation (never poisons)"
    );

    // Contract 14 (era fencing): the stale lease epoch's next acquire
    // refuses UNKNOWN_LEASE — the range plane needs no new token algebra.
    let err = client
        .acquire_range(ino, (BLK, 2 * BLK), (BLK, 2 * BLK), Duration::from_secs(1))
        .await
        .expect_err("a dead lease epoch's range acquire must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("UNKNOWN_LEASE") || msg.contains("unknown") || msg.contains("not custody"),
        "the refusal must be the unknown-lease class: {msg}"
    );
}

/// Contract 15: the renewal reply carries the client's live RANGE VECTOR,
/// and the client cache REBUILDS from it — the revalidation surface that
/// keeps a long-lived co-writer's cached spans honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renewal_range_vector_rebuilds_the_client_cache() {
    let _g = RANGE_SERIAL.lock().await;
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    let (_l, _owner, endpoint) = range_authority(64 * BLK);
    let client = range_client(&endpoint, "s11-vector-cw").await;
    let ino = 72_000_014u64;

    let got = client
        .acquire_range(ino, (0, BLK), (0, BLK), Duration::from_secs(3))
        .await
        .expect("range");
    let (lease, token) = match got {
        RangeAcquireOutcome::New { lease, .. } => {
            let t = lease.fencing_token();
            (lease, t)
        }
        other => panic!("expected NEW, got {other:?}"),
    };

    // Drop the client-side cache (the seam), then renew: the reply's range
    // vector must rebuild it.
    squeezefs::meta_ship::tokens::test_clear_range_cache();
    assert!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, 0, BLK).is_none(),
        "premise: the cache is cold"
    );
    client.renew_all().await.expect("renewal");
    assert_eq!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, 0, BLK),
        Some(token),
        "the renewal's range vector must rebuild the client cache"
    );
    lease.release().await.expect("release");
    client.drain_releases().await;
    // A released grant leaves the vector at the NEXT renewal.
    client.renew_all().await.expect("renewal after release");
    assert!(
        squeezefs::meta_ship::tokens::range_token_covering(ino, 0, BLK).is_none(),
        "a released grant must leave the revalidated cache"
    );
}

// ---------------------------------------------------------------------------
// 17/18. Dark posture + R5 registration
// ---------------------------------------------------------------------------

/// Contract 17: `SQUEEZEFS_RANGE_CUSTODY` is a registered ENG-10 knob
/// (Kind::Bool, static default OFF until rungs 16/17 land the publish
/// composition — the 2026-08-17 adjudication) and is read ONLY when the mw plane is
/// armed — on this unarmed process the lever answers false even when
/// forced on (the `delegation_enabled` law), which is what keeps every
/// shipped mount's whole-file path structurally untouched (KD-MW-12).
#[tokio::test]
async fn range_custody_lever_is_registered_and_inert_unarmed() {
    let knob = squeezefs::env_knobs::lookup("SQUEEZEFS_RANGE_CUSTODY")
        .expect("SQUEEZEFS_RANGE_CUSTODY must be a registered knob (ENG-10)");
    assert!(
        matches!(knob.kind, squeezefs::env_knobs::Kind::Bool),
        "the lever is Kind::Bool"
    );
    assert_eq!(
        knob.default, "off",
        "static default OFF until rungs 16/17 land the concurrent same-ino \
         publish composition (adjudicated 2026-08-17 — the never-lossy law \
         outranks §11's provisional default-on; the s11-range leg arms it \
         explicitly)"
    );

    // Unarmed: false, even forced (read only when the mw plane is armed).
    assert!(
        !squeezefs::data_grant::range_custody_enabled(),
        "an unarmed process's range-custody lever answers false"
    );
    let prev = squeezefs::data_grant::TEST_RANGE_CUSTODY_OVERRIDE
        .swap(1, std::sync::atomic::Ordering::Relaxed);
    assert!(
        !squeezefs::data_grant::range_custody_enabled(),
        "forced ON while unarmed stays inert — the knob is read only past \
         the armed gate (announced-inert, never a refusal)"
    );
    squeezefs::data_grant::TEST_RANGE_CUSTODY_OVERRIDE
        .store(prev, std::sync::atomic::Ordering::Relaxed);
}

/// Contract 18: the §9.2 R5 pair is REGISTERED — `dlm_grant_table_bytes`
/// (authority-side custody records) and `dlm_token_cache_bytes`
/// (client-side token + range-span state) are both `MEM_BUDGET`
/// components, floor 0 (custody admission refuses instead of shedding;
/// the cache re-earns instead of answering wrong).
#[tokio::test]
async fn r5_registers_the_grant_table_and_token_cache_components() {
    squeezefs::dlm::ensure_grant_table_r5();
    squeezefs::meta_ship::tokens::ensure_token_cache_r5();
    let comps = squeezefs::mem_budget::MEM_BUDGET.stats_components();
    let names: Vec<&str> = comps.iter().map(|(n, ..)| *n).collect();
    assert!(
        names.contains(&"dlm_grant_table_bytes"),
        "dlm_grant_table_bytes must be an R5 component (§9.2): {names:?}"
    );
    assert!(
        names.contains(&"dlm_token_cache_bytes"),
        "dlm_token_cache_bytes must be an R5 component (§9.2): {names:?}"
    );
    for (name, _cur, floor, ..) in comps {
        if name == "dlm_grant_table_bytes" || name == "dlm_token_cache_bytes" {
            assert_eq!(floor, 0, "{name}: floor 0 — nothing here may pin RAM");
        }
    }
}

/// **The s11-range leg's second live finding (2026-08-17), repro-ported**:
/// a required window PARTIALLY OVERLAPPING the holder's OWN grant — the
/// frontier-crossing shape every non-block-aligned writeback chunk
/// produces (write [15.7M,16.6M) against a held [0,16M)) — must EXTEND
/// the grant, and a fully-covered re-ask must answer COVERED. The shipped
/// plan classified the holder's own EX grant as FOREIGN custody
/// (`!compatible_with` — EX‖EX is incompatible BY THE MATRIX, but the
/// matrix governs two DIFFERENT holders; one holder's own grant is
/// mergeable custody), so a lone streaming co-writer waited out its whole
/// budget ON ITSELF and died EIO — with no peer anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn own_grant_partial_overlap_extends_and_covered_reask_serves() {
    let _g = RANGE_SERIAL.lock().await;
    let ino = 72_000_020u64;
    let path = format!("inode_{ino}");
    let d = dlm();
    let geometry = Some((64 * BLK, BLK));

    // The stream's first chunk: grant [0, 4M).
    let first = d
        .acquire_lock_range(&path, (0, MIB), (0, BLK), Duration::from_secs(3), geometry)
        .await
        .expect("first chunk");
    let (lease, token) = match first {
        RangeAcquired::New { lease, span } => {
            assert_eq!(span, (0, BLK));
            let t = lease.fencing_token();
            (lease, t)
        }
        other => panic!("expected NEW, got {other:?}"),
    };

    // The frontier-crossing chunk: [3.7M, 4.6M) overlaps [0,4M) AND
    // extends past it — the holder's own custody must EXTEND, never read
    // as a foreign holder to wait on (the live EIO shape).
    let crossing = (BLK - 300 * 1024, BLK + 400 * 1024);
    let desired = squeezefs::dlm::block_align_out(crossing, BLK);
    match d
        .acquire_lock_range(
            &path,
            crossing,
            desired,
            Duration::from_millis(800),
            geometry,
        )
        .await
        .expect("a frontier-crossing chunk must not starve on the holder's OWN grant")
    {
        RangeAcquired::Extended { token: t, span } => {
            assert_eq!(t, token, "the extension keeps the surviving token");
            assert_eq!(span, (0, 2 * BLK), "the union covers the crossing chunk");
        }
        other => panic!("a same-scope partial overlap must EXTEND, got {other:?}"),
    }
    assert_eq!(
        squeezefs::dlm::live_range_records(ino),
        1,
        "still ONE grant after the frontier crossing"
    );

    // A fully-covered re-ask (the cache-cold idempotent shape) answers
    // COVERED — same token, no mutation, no mint.
    match d
        .acquire_lock_range(
            &path,
            (MIB, 2 * MIB),
            (MIB, 2 * MIB),
            Duration::from_millis(800),
            geometry,
        )
        .await
        .expect("a covered re-ask must serve, not starve")
    {
        RangeAcquired::Covered { token: t, span } => {
            assert_eq!(t, token);
            assert_eq!(span, (0, 2 * BLK), "the covering grant's own span");
        }
        other => panic!("a covered re-ask must answer COVERED, got {other:?}"),
    }
    let stats = squeezefs::dlm::range_custody_stats();
    assert!(stats.covered_serves > 0, "covered serves must be countable");
    lease.release().await.expect("release");
    assert_eq!(squeezefs::dlm::live_range_records(ino), 0);
}

// ---------------------------------------------------------------------------
// DLM S11 **rung 16** — KD-MW-12: the two fast-path clause FACES
// (`docs/design-full-multi-writer.md` §9.3 items 3/4 + PR-plan row 16).
// ---------------------------------------------------------------------------

/// Rung 16: the W1 patch clause (`BlockAllocator::patch_range_shared`)
/// and the B4 overlay clause (`device_overlay::overlay_range_shared`)
/// consult ONE custody core (`span_range_shared`) and count in SPLIT
/// ledgers — plus the **multi-grant fencing corollary** (rung 15
/// residual #2): custody is classified by the PRESENTED token snapshot,
/// so a STALE token — even an older mint against the mount's own live
/// grant — refuses BOTH fast paths (conservative refusal; the KD-6
/// retry ladder converges the write through the CoW-rewrite path, never
/// through an in-place mutation under a snapshot the custody core does
/// not recognize).
#[tokio::test]
async fn range_clause_faces_share_the_core_and_refuse_stale_tokens() {
    use squeezefs::block_allocator::BlockAllocator;
    use squeezefs::fuse_client::METRICS;
    let ino = 71_016_001u64;
    let path = format!("inode_{ino}");
    let d = dlm();
    let block = (0u64, 4 * MIB);
    let p = || {
        METRICS
            .patch_ineligible_range_shared
            .load(Ordering::Relaxed)
    };
    let o = || {
        METRICS
            .overlay_ineligible_range_shared
            .load(Ordering::Relaxed)
    };

    // No custody at all: neither face refuses, neither ledger moves.
    let (p0, o0) = (p(), o());
    assert!(
        !BlockAllocator::patch_range_shared(ino, block.0, block.1, 0),
        "no custody: the patch face passes"
    );
    assert!(
        !squeezefs::device_overlay::overlay_range_shared(ino, block.0, block.1, 0),
        "no custody: the overlay face passes"
    );
    assert_eq!(p() - p0, 0, "no refusal, no count");
    assert_eq!(o() - o0, 0, "no refusal, no count");

    // Own covering grant, CURRENT token: own custody never refuses
    // itself (finding #2's lesson) on EITHER face.
    let own = d
        .acquire_lock(&path, Some((0, 4 * MIB)), Duration::from_secs(3))
        .await
        .expect("own covering grant");
    let t = own.fencing_token();
    assert!(
        !BlockAllocator::patch_range_shared(ino, block.0, block.1, t),
        "own covering custody with the CURRENT token: patch passes"
    );
    assert!(
        !squeezefs::device_overlay::overlay_range_shared(ino, block.0, block.1, t),
        "own covering custody with the CURRENT token: overlay passes"
    );
    assert_eq!(p() - p0, 0);
    assert_eq!(o() - o0, 0);

    // The corollary: a STALE token is NOT custody — a snapshot the core
    // does not recognize as the live grant's refuses on BOTH faces, each
    // counted in ITS OWN ledger (the buckets must not merge —
    // predicate-rot detection depends on the split).
    assert!(
        BlockAllocator::patch_range_shared(ino, block.0, block.1, t - 1),
        "a stale token must refuse the patch face"
    );
    assert!(
        squeezefs::device_overlay::overlay_range_shared(ino, block.0, block.1, t - 1),
        "a stale token must refuse the overlay face"
    );
    assert_eq!(p() - p0, 1, "patch ledger counted exactly its refusal");
    assert_eq!(o() - o0, 1, "overlay ledger counted exactly its refusal");

    own.release().await.expect("release");
    // Custody released: both faces pass again with ANY token (nothing
    // left to violate) and the ledgers stay put.
    assert!(!BlockAllocator::patch_range_shared(
        ino, block.0, block.1, 0
    ));
    assert!(!squeezefs::device_overlay::overlay_range_shared(
        ino, block.0, block.1, 0
    ));
    assert_eq!(p() - p0, 1);
    assert_eq!(o() - o0, 1);
}
