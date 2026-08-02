//! DLM S1 — the single per-process fencing mint (spec §6.7 decision 4,
//! §6.9 stage S1; closes RES-2 by deletion).
//!
//! Today's `FENCING_MAP` keeps one `Arc<AtomicU64>` generator per
//! distinct object ever write-locked, forever (~105 B each, no removal
//! path, R5-invisible — ~10.5 GB at the 100 M-inode cap). S1 replaces it
//! with ONE process-global `grant_seq: AtomicU64`: global monotonicity
//! implies per-object monotonicity, and every consumer comparison is
//! `<`, `==` or `.max()` — all monotone-safe (the site census rides the
//! implementation commit).
//!
//! Contracts:
//!
//! 1. **RSS gate (RED pre-S1)** — the §6.9 S1 gate, in-process scale
//!    version: an acquire+release walk across millions of DISTINCT inos
//!    must leave RSS flat. Red today: the walk plants one immortal
//!    generator per ino (~100 B × 3 M ≈ 300 MB).
//! 2. **Global mint uniqueness (RED pre-S1)** — the S1 contract itself:
//!    tokens are unique ACROSS objects (one generator). Red today:
//!    per-object counters all start at 1, so two objects' first grants
//!    collide.
//! 3. **Per-object strict monotonicity under concurrent minting (pin,
//!    green both sides)** — successive grants on one object are strictly
//!    increasing, under multi-thread contention across many objects.
//! 4. **Read-surface pins (green both sides)** — the reads S1 must not
//!    regress: held-lease read equals the lease snapshot; a range mint
//!    is visible through the file identity's read (the wt_fencing
//!    pattern); a released object's read stays ≥ its own last grant
//!    (the stale-reject arm).

use squeezefs::dlm::DlmClient;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn vm_rss_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().unwrap();
        }
    }
    panic!("VmRSS not found");
}

/// 1. The S1 gate (spec §6.9: "RSS flat across a 10⁷-inode walk" —
/// in-process scale version at 3 M): fencing state must be O(1) in the
/// number of distinct objects ever locked, not O(n).
///
/// RED pre-S1: FENCING_MAP retains ~100 B per walked ino ⇒ ≈ 300 MB.
/// GREEN post-S1: one AtomicU64 mint + lock entries removed at release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rss_stays_flat_across_distinct_ino_write_walk() {
    let dlm = DlmClient::new().unwrap();

    // Warm allocator arenas / lazy statics before the baseline.
    for i in 0..10_000u64 {
        let lease = dlm
            .acquire_lock(
                &format!("inode_{}", 20_000_000 + i),
                None,
                Duration::from_secs(5),
            )
            .await
            .expect("warmup acquire");
        lease.release().await.expect("warmup release");
    }
    let rss_before = vm_rss_kb();

    const WALK: u64 = 3_000_000;
    for i in 0..WALK {
        let lease = dlm
            .acquire_lock(
                &format!("inode_{}", 30_000_000 + i),
                None,
                Duration::from_secs(5),
            )
            .await
            .expect("walk acquire");
        lease.release().await.expect("walk release");
    }

    let rss_after = vm_rss_kb();
    let delta_mb = rss_after.saturating_sub(rss_before) / 1024;
    assert!(
        delta_mb < 96,
        "fencing state must be O(1) in distinct objects (RES-2): a {WALK}-ino \
         acquire+release walk grew RSS by {delta_mb} MiB (≥ 96 MiB = the \
         per-object-generator signature, ~100 B/ino retained forever)"
    );
}

/// 2. The S1 mint contract: ONE generator ⇒ tokens are unique across ALL
/// objects, not merely monotone within one. RED pre-S1 (per-object
/// counters both mint 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grants_are_globally_unique_across_objects() {
    let dlm = DlmClient::new().unwrap();
    let mut seen = std::collections::HashSet::new();
    for ino in 40_000_000u64..40_000_064 {
        let lease = dlm
            .acquire_lock(&format!("inode_{ino}"), None, Duration::from_secs(5))
            .await
            .expect("acquire");
        assert!(
            seen.insert(lease.fencing_token()),
            "token {} minted twice (per-object generators — the pre-S1 \
             signature; S1's single grant_seq makes every grant unique)",
            lease.fencing_token()
        );
        lease.release().await.expect("release");
    }
}

/// 3. Pin (green both sides): per-object strict monotonicity under
/// concurrent minting — 8 workers × 16 objects × 64 rounds; each
/// object's grant sequence, ordered by acquisition, is strictly
/// increasing, and grants never go missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn per_object_monotonicity_under_concurrent_minting() {
    const OBJECTS: u64 = 16;
    const WORKERS: u64 = 8;
    const ROUNDS: u64 = 64;

    let dlm = DlmClient::new().unwrap();
    // Per-object last-seen word: checked/advanced UNDER the exclusive
    // lease, so ordering is by real acquisition order (no test-side lock).
    let last_seen: Arc<Vec<AtomicU64>> =
        Arc::new((0..OBJECTS).map(|_| AtomicU64::new(0)).collect());
    let violations = Arc::new(AtomicU64::new(0));

    let mut tasks = tokio::task::JoinSet::new();
    for w in 0..WORKERS {
        let dlm = dlm.clone();
        let last_seen = last_seen.clone();
        let violations = violations.clone();
        tasks.spawn(async move {
            for r in 0..ROUNDS {
                let obj = (w * ROUNDS + r) % OBJECTS;
                let key = format!("inode_{}", 50_000_000 + obj);
                let lease = dlm
                    .acquire_lock(&key, None, Duration::from_secs(30))
                    .await
                    .expect("contended acquire");
                let tok = lease.fencing_token();
                let prev = last_seen[obj as usize].swap(tok, Ordering::AcqRel);
                if tok <= prev {
                    violations.fetch_add(1, Ordering::Relaxed);
                    eprintln!("object {obj}: token {tok} after {prev} (NOT strictly monotone)");
                }
                lease.release().await.expect("release");
            }
        });
    }
    while let Some(res) = tasks.join_next().await {
        res.expect("worker panicked");
    }
    assert_eq!(
        violations.load(Ordering::Relaxed),
        0,
        "every object's grant sequence must be strictly monotone in \
         acquisition order under concurrent minting"
    );
}

/// 4a. Pin (green both sides): a HELD lease's generator read equals the
/// lease snapshot — the fence-check identity the write path lives on
/// (presented == current while the era is live; a stripe-shared or
/// global read here would spuriously fence live writers).
#[tokio::test]
async fn held_read_equals_lease_snapshot() {
    let dlm = DlmClient::new().unwrap();
    let lease = dlm
        .acquire_lock("inode_60000001", None, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(dlm.get_fencing_token_ino(60000001), lease.fencing_token());
    assert_eq!(
        dlm.get_fencing_token("inode_60000001"),
        lease.fencing_token()
    );
    lease.release().await.unwrap();
}

/// 4b. Pin (green both sides): a byte-range mint shares the file's
/// fencing generator and is visible through the whole-file read WHILE
/// HELD — the wt_fencing / merge-primitive stale-token pattern
/// (tests/write_through_tests.rs) constructs `stale = range_token - 1`
/// and expects the router's whole-file fence read to reject it.
#[tokio::test]
async fn range_mint_visible_through_file_identity_while_held() {
    let dlm = DlmClient::new().unwrap();
    let whole = dlm
        .acquire_lock("inode_60000002", None, Duration::from_secs(5))
        .await
        .unwrap();
    let range = dlm
        .acquire_lock("inode_60000002", Some((0, 1)), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(range.fencing_token() > whole.fencing_token());
    let read = dlm.get_fencing_token_ino(60000002);
    assert!(
        read >= range.fencing_token(),
        "the whole-file read must see the range mint while it is held \
         (got {read}, range minted {})",
        range.fencing_token()
    );
    range.release().await.unwrap();
    whole.release().await.unwrap();
}

/// 4c. Pin (green both sides): after every lease is released, the
/// object's read stays ≥ its own last grant — the stale-reject arm
/// (`presented < current` must still fire for genuinely superseded
/// tokens after release; regression here silently weakens the fence).
#[tokio::test]
async fn released_read_stays_at_least_last_grant() {
    let dlm = DlmClient::new().unwrap();
    let lease = dlm
        .acquire_lock("inode_60000003", None, Duration::from_secs(5))
        .await
        .unwrap();
    let t = lease.fencing_token();
    lease.release().await.unwrap();
    let read = dlm.get_fencing_token_ino(60000003);
    assert!(
        read >= t,
        "released-object read regressed below its own last grant \
         ({read} < {t}): the stale-reject arm dies"
    );
    // ... and the stale shape the router tests construct still classifies:
    assert!(t - 1 < read, "t-1 must remain rejectable against the read");
}
