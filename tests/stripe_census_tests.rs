//! D-3 (e2e perf audit DLM board #4, `perf/dlm-stripe-derivation`) — the
//! stripe-collision CENSUS on every striped lock table, and the width
//! derivation that census convicts.
//!
//! A striped table maps many keys onto a fixed lock population, so a
//! contended acquire has two causes the wait histograms cannot tell
//! apart: the stripe is held by the SAME key (a true wait — the workload
//! asked for it) or by a DIFFERENT key that merely hashes alongside (a
//! false-sharing wait — the table's width did). On the 4a `DlmLockManager`
//! tables the guard is held across the whole commit park (D5), so a
//! false-sharing wait there costs a full commit. The census splits them
//! per table: `*_stripe_collisions` vs `*_key_waits`, exported flat on the
//! stats inode beside `lock_phase_ns`.
//!
//! Contracts (each pinned per table through its ONE acquisition door):
//!
//! - two distinct keys that hash to the same stripe, the second acquiring
//!   while the first holds ⇒ exactly 1 collision, 0 key waits;
//! - the same key twice ⇒ 1 key wait, 0 collisions;
//! - unrelated stripes ⇒ neither (an uncontended acquire records nothing
//!   — the fast path pays one relaxed store, no counter);
//! - `lock_phase_ns.dlm_guard_wait.count ≡ Σ 4a (collisions + key_waits)`
//!   — the contended 4a acquire's parked span is recorded exactly once per
//!   contended acquire, in the same arm the census classifies in;
//! - the stats inode carries every table's `{width, collisions, waits}`.
//!
//! Suite runs `--test-threads=1` (process-global counters); every test
//! takes the serial guard and reads DELTAS.

use squeezefs::stripe_locks::{
    StripeCensusCounters, BLOCK_FLUSH_CENSUS, DLM_DENTRY_CENSUS, DLM_INODE_CENSUS,
    INODE_META_CENSUS, LEASE_WAITER_CENSUS, SERVE_INO_CENSUS,
};
use std::sync::OnceLock;
use std::time::Duration;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn snap(c: &StripeCensusCounters) -> (u64, u64) {
    c.snapshot()
}

fn delta(before: (u64, u64), after: (u64, u64)) -> (u64, u64) {
    (after.0 - before.0, after.1 - before.1)
}

/// Find a key `!= base` in `[base+1, base+limit)` mapping onto `base`'s
/// stripe under `stripe_of`, and one mapping onto a DIFFERENT stripe.
fn colliding_and_foreign(
    base: u64,
    stripe_of: impl Fn(u64) -> usize,
) -> (u64 /* same stripe */, u64 /* other stripe */) {
    let want = stripe_of(base);
    let same = (base + 1..base + 1_000_000)
        .find(|&k| stripe_of(k) == want)
        .expect("a stripe-mate exists within a million keys on any width ≤ 2^20");
    let other = (base + 1..base + 1_000_000)
        .find(|&k| stripe_of(k) != want)
        .expect("a foreign stripe exists");
    (same, other)
}

/// One HOLD-then-CONTEND scenario: `hold` acquires, raises [`HELD`], and
/// keeps its guard 2 ms; the caller's `contend` acquires once the latch
/// is up (so the contention is certain, never timing-dependent). Returns
/// the census delta for `counters` and the `dlm_guard_wait` count delta.
async fn contend<H, C>(
    counters: &'static StripeCensusCounters,
    hold: H,
    contend: C,
) -> ((u64, u64), u64)
where
    H: std::future::Future<Output = ()> + Send + 'static,
    C: std::future::Future<Output = ()>,
{
    use squeezefs::fuse_client::lock_phase_json;
    let c0 = snap(counters);
    let w0 = wait_count(&lock_phase_json());
    let holder = tokio::spawn(hold);
    HELD.notified().await;
    contend.await;
    holder.await.unwrap();
    let c1 = snap(counters);
    let w1 = wait_count(&lock_phase_json());
    (delta(c0, c1), w1 - w0)
}

/// Latch the holder raises once its guard is held (`notify_one` stores a
/// permit, so the raise never races the wait).
static HELD: tokio::sync::Notify = tokio::sync::Notify::const_new();

fn wait_count(fam: &serde_json::Value) -> u64 {
    fam["dlm_guard_wait"]["count"]
        .as_u64()
        .expect("lock_phase_ns.dlm_guard_wait.count")
}

// ---------------------------------------------------------------------------
// 4a — DlmLockManager, inode class
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dlm_inode_stripe_mate_is_a_collision_same_ino_is_a_key_wait() {
    use squeezefs::meta_backend::dlm::DlmLockManager;
    use std::sync::Arc;
    let _g = serial().await;
    let dlm = Arc::new(DlmLockManager::new());
    let base = 0xD3_0001u64;
    let (mate, foreign) = colliding_and_foreign(base, |k| dlm.inode_stripe(k));
    assert_ne!(mate, base);
    assert_eq!(dlm.inode_stripe(mate), dlm.inode_stripe(base));
    assert_ne!(dlm.inode_stripe(foreign), dlm.inode_stripe(base));

    // Stripe-mate under an EXCLUSIVE holder ⇒ 1 collision, 0 key waits,
    // and exactly one dlm_guard_wait sample.
    let d = dlm.clone();
    let (census, waits) = contend(
        &DLM_INODE_CENSUS,
        async move {
            let g = d.lock_inode_exclusive(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(dlm.lock_inode_exclusive(mate).await);
        },
    )
    .await;
    assert_eq!(census, (1, 0), "stripe-mate ⇒ one collision, no key wait");
    assert_eq!(
        waits, 1,
        "one contended acquire ⇒ one dlm_guard_wait sample"
    );

    // Same ino ⇒ 1 key wait, 0 collisions.
    let d = dlm.clone();
    let (census, waits) = contend(
        &DLM_INODE_CENSUS,
        async move {
            let g = d.lock_inode_exclusive(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(dlm.lock_inode_shared(base).await);
        },
    )
    .await;
    assert_eq!(census, (0, 1), "same ino ⇒ one key wait, no collision");
    assert_eq!(waits, 1);

    // A foreign stripe ⇒ neither, and no wait sample.
    let d = dlm.clone();
    let (census, waits) = contend(
        &DLM_INODE_CENSUS,
        async move {
            let g = d.lock_inode_exclusive(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(dlm.lock_inode_exclusive(foreign).await);
        },
    )
    .await;
    assert_eq!(census, (0, 0), "unrelated stripes never touch the census");
    assert_eq!(waits, 0, "an uncontended acquire records no wait sample");
}

// ---------------------------------------------------------------------------
// 4a — DlmLockManager, dentry class
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dlm_dentry_stripe_mate_is_a_collision_same_name_is_a_key_wait() {
    use squeezefs::meta_backend::dlm::DlmLockManager;
    use std::sync::Arc;
    let _g = serial().await;
    let dlm = Arc::new(DlmLockManager::new());
    let parent = 7u64;
    let want = dlm.dentry_stripe(parent, "f0");
    let mate = (1..1_000_000u64)
        .map(|i| format!("f{i}"))
        .find(|n| dlm.dentry_stripe(parent, n) == want)
        .expect("a dentry stripe-mate exists");
    let foreign = (1..1_000_000u64)
        .map(|i| format!("g{i}"))
        .find(|n| dlm.dentry_stripe(parent, n) != want)
        .expect("a foreign dentry stripe exists");

    let d = dlm.clone();
    let (census, _) = contend(
        &DLM_DENTRY_CENSUS,
        async move {
            let g = d.lock_dentry_exclusive(parent, "f0").await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(dlm.lock_dentry_exclusive(parent, &mate).await);
        },
    )
    .await;
    assert_eq!(census, (1, 0), "dentry stripe-mate ⇒ one collision");

    let d = dlm.clone();
    let (census, _) = contend(
        &DLM_DENTRY_CENSUS,
        async move {
            let g = d.lock_dentry_exclusive(parent, "f0").await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(dlm.lock_dentry_shared(parent, "f0").await);
        },
    )
    .await;
    assert_eq!(census, (0, 1), "same (parent, name) ⇒ one key wait");

    let d = dlm.clone();
    let (census, _) = contend(
        &DLM_DENTRY_CENSUS,
        async move {
            let g = d.lock_dentry_exclusive(parent, "f0").await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(dlm.lock_dentry_exclusive(parent, &foreign).await);
        },
    )
    .await;
    assert_eq!(census, (0, 0));
}

/// `lock_many` classifies per stripe like the single-object doors, and a
/// group whose members share a stripe dedupes BEFORE the census (one
/// acquire per distinct stripe — never a self-collision).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_many_dedupes_before_the_census_and_classifies_per_stripe() {
    use squeezefs::meta_backend::dlm::{DlmLockManager, LockMode};
    use std::sync::Arc;
    let _g = serial().await;
    let dlm = Arc::new(DlmLockManager::new());
    let base = 0xD3_1001u64;
    let (mate, foreign) = colliding_and_foreign(base, |k| dlm.inode_stripe(k));

    // Uncontended group with an internal stripe collision: nothing counted.
    let c0 = snap(&DLM_INODE_CENSUS);
    drop(
        dlm.lock_many(
            &[
                (base, LockMode::Exclusive),
                (mate, LockMode::Exclusive),
                (foreign, LockMode::Shared),
            ],
            &[],
        )
        .await,
    );
    assert_eq!(
        delta(c0, snap(&DLM_INODE_CENSUS)),
        (0, 0),
        "an uncontended lock_many touches no counter"
    );

    // Group contending with a holder on a stripe-mate: one collision.
    let d = dlm.clone();
    let (census, waits) = contend(
        &DLM_INODE_CENSUS,
        async move {
            let g = d.lock_inode_exclusive(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(
                dlm.lock_many(
                    &[(mate, LockMode::Exclusive), (foreign, LockMode::Exclusive)],
                    &[],
                )
                .await,
            );
        },
    )
    .await;
    assert_eq!(census, (1, 0));
    assert_eq!(waits, 1);
}

// ---------------------------------------------------------------------------
// 3.5 — INODE_META_LOCKS
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inode_meta_stripe_mate_is_a_collision_same_ino_is_a_key_wait() {
    use squeezefs::routing::{inode_meta_stripe, meta_lock_acquire};
    let _g = serial().await;
    let base = 0xD3_2001u64;
    let (mate, foreign) = colliding_and_foreign(base, inode_meta_stripe);

    let (census, _) = contend(
        &INODE_META_CENSUS,
        async move {
            let g = meta_lock_acquire(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(meta_lock_acquire(mate).await);
        },
    )
    .await;
    assert_eq!(census, (1, 0));

    let (census, _) = contend(
        &INODE_META_CENSUS,
        async move {
            let g = meta_lock_acquire(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(meta_lock_acquire(base).await);
        },
    )
    .await;
    assert_eq!(census, (0, 1));

    let (census, _) = contend(
        &INODE_META_CENSUS,
        async move {
            let g = meta_lock_acquire(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(meta_lock_acquire(foreign).await);
        },
    )
    .await;
    assert_eq!(census, (0, 0));
}

// ---------------------------------------------------------------------------
// 3 — BLOCK_FLUSH_LOCKS
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn block_flush_stripe_mate_is_a_collision_same_block_is_a_key_wait() {
    use squeezefs::fuse_client::{block_lock_acquire, BlockLockSite, BLOCK_FLUSH_LOCKS};
    let _g = serial().await;
    let base = 0xD3_3001u64;
    let b = 3u32;
    let want = BLOCK_FLUSH_LOCKS.block_shard_index(base, b);
    let mate = (base + 1..base + 1_000_000)
        .find(|&k| BLOCK_FLUSH_LOCKS.block_shard_index(k, b) == want)
        .unwrap();
    let foreign = (base + 1..base + 1_000_000)
        .find(|&k| BLOCK_FLUSH_LOCKS.block_shard_index(k, b) != want)
        .unwrap();
    let site = BlockLockSite::WriteCheckout;

    let (census, _) = contend(
        &BLOCK_FLUSH_CENSUS,
        async move {
            let g = block_lock_acquire(base, b, site).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(block_lock_acquire(mate, b, site).await);
        },
    )
    .await;
    assert_eq!(census, (1, 0));

    let (census, _) = contend(
        &BLOCK_FLUSH_CENSUS,
        async move {
            let g = block_lock_acquire(base, b, site).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(block_lock_acquire(base, b, site).await);
        },
    )
    .await;
    assert_eq!(census, (0, 1));

    let (census, _) = contend(
        &BLOCK_FLUSH_CENSUS,
        async move {
            let g = block_lock_acquire(base, b, site).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(block_lock_acquire(foreign, b, site).await);
        },
    )
    .await;
    assert_eq!(census, (0, 0));
}

// ---------------------------------------------------------------------------
// The owner-side serve stripe (S9 publish plane, rung 17)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_ino_stripe_mate_is_a_collision_same_ino_is_a_key_wait() {
    use squeezefs::meta_ship::publish::{serve_ino_guard, serve_ino_stripe};
    let _g = serial().await;
    let base = 0xD3_4001u64;
    let (mate, foreign) = colliding_and_foreign(base, serve_ino_stripe);

    let (census, _) = contend(
        &SERVE_INO_CENSUS,
        async move {
            let g = serve_ino_guard(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(serve_ino_guard(mate).await);
        },
    )
    .await;
    assert_eq!(census, (1, 0));

    let (census, _) = contend(
        &SERVE_INO_CENSUS,
        async move {
            let g = serve_ino_guard(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(serve_ino_guard(base).await);
        },
    )
    .await;
    assert_eq!(census, (0, 1));

    let (census, _) = contend(
        &SERVE_INO_CENSUS,
        async move {
            let g = serve_ino_guard(base).await;
            HELD.notify_one();
            std::thread::sleep(Duration::from_millis(2));
            drop(g);
        },
        async {
            drop(serve_ino_guard(foreign).await);
        },
    )
    .await;
    assert_eq!(census, (0, 0));
}

// ---------------------------------------------------------------------------
// The lease waiter stripes (src/dlm.rs LOCK_WAITERS) — a collision here
// is a SPURIOUS WAKE (the custody table is per key; the stripe only fans
// the release notification out), never a wait.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_waiter_stripe_mate_release_is_a_spurious_wake_own_release_is_a_key_wait() {
    use squeezefs::dlm::{lease_waiter_stripe, LocalLockManager};
    let _g = serial().await;
    let lm = LocalLockManager::new().unwrap();
    let base = 0xD3_5001u64;
    let (mate, foreign) = colliding_and_foreign(base, lease_waiter_stripe);
    let path = |ino: u64| format!("inode_{ino}");
    let ttl = Duration::from_secs(5);

    // A holds `base`, C holds `mate` (same stripe). B waits on `mate`.
    // A's release wakes the stripe: B re-checks, finds `mate` still held
    // — a spurious wake (1 collision). C's release then admits B (1 key
    // wait).
    let c0 = snap(&LEASE_WAITER_CENSUS);
    let a = lm.acquire_lock(&path(base), None, ttl).await.unwrap();
    let c = lm.acquire_lock(&path(mate), None, ttl).await.unwrap();
    let lm2 = lm.clone();
    let mate_path = path(mate);
    let b = tokio::spawn(async move { lm2.acquire_lock(&mate_path, None, ttl).await });
    // B must be parked before A releases: poll the waiter gauge.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while squeezefs::dlm::lease_waiters_parked() == 0 {
        assert!(std::time::Instant::now() < deadline, "B never parked");
        tokio::task::yield_now().await;
    }
    drop(a);
    // B's spurious wake lands before C releases: poll the census, then
    // wait for B to be PARKED AGAIN (its re-check found `mate` still
    // held) so C's release is a wake B observes — not a free lock B's
    // re-check takes without parking.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while delta(c0, snap(&LEASE_WAITER_CENSUS)).0 == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "B's spurious wake never counted"
        );
        tokio::task::yield_now().await;
    }
    while squeezefs::dlm::lease_waiters_parked() == 0 {
        assert!(std::time::Instant::now() < deadline, "B never re-parked");
        tokio::task::yield_now().await;
    }
    drop(c);
    let bl = b.await.unwrap().unwrap();
    drop(bl);
    assert_eq!(
        delta(c0, snap(&LEASE_WAITER_CENSUS)),
        (1, 1),
        "one stripe-mate wake (collision) + one own-key wake (key wait)"
    );

    // Unrelated stripe: B acquires `foreign` while A holds `base` — never
    // parks, nothing counted.
    let c0 = snap(&LEASE_WAITER_CENSUS);
    let a = lm.acquire_lock(&path(base), None, ttl).await.unwrap();
    drop(lm.acquire_lock(&path(foreign), None, ttl).await.unwrap());
    drop(a);
    assert_eq!(delta(c0, snap(&LEASE_WAITER_CENSUS)), (0, 0));
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Every table exports `{<table>_stripes, <table>_stripe_collisions,
/// <table>_key_waits}` flat in the stats metrics (beside `lock_phase_ns`),
/// the widths are powers of two at or above the shipped floors, and the
/// 4a `dlm_guard_wait` phase joined `lock_phase_ns`.
#[test]
fn stripe_census_exports_every_table_flat_with_its_width() {
    let entries = squeezefs::fuse_client::stripe_census_entries();
    let map: std::collections::BTreeMap<&str, u64> = entries.iter().copied().collect();
    for table in [
        "dlm_inode",
        "dlm_dentry",
        "serve_ino",
        "inode_meta",
        "block_flush",
        "lease_waiter",
    ] {
        let w = *map
            .get(format!("{table}_stripes").as_str())
            .unwrap_or_else(|| panic!("{table}_stripes missing: {map:?}"));
        assert!(
            w.is_power_of_two(),
            "{table} width {w} must be a power of two"
        );
        assert!(w >= 1024, "{table} width {w} below every shipped floor");
        for suffix in ["stripe_collisions", "key_waits"] {
            assert!(
                map.contains_key(format!("{table}_{suffix}").as_str()),
                "{table}_{suffix} missing"
            );
        }
    }
    assert_eq!(map.len(), 18, "exactly six tables × three words: {map:?}");
    let fam = squeezefs::fuse_client::lock_phase_json();
    assert!(
        fam.get("dlm_guard_wait").is_some(),
        "lock_phase_ns carries the 4a wait phase: {fam}"
    );
}
