//! Async block-reclaim contract — the overwrite-throughput fix
//! (`.benchmarks/2026-07-27-async-block-reclaim.md`).
//!
//! Field conviction (4-node NVMe-oF/TCP, elbencho 16t 1 MiB O_DIRECT,
//! A-B-B-A order-controlled): first writes sustain ~5.4–5.5 GB/s, but
//! OVERWRITES drop to ~3.46–3.49 GB/s on kernel and shim paths alike.
//! Every displaced block of a striped overwrite issued its `BLKDISCARD`
//! SYNCHRONOUSLY on the write path (`reclaim_freed_range_sync`) — on
//! fabric each discard is a ~235 µs round-trip, and 4 GiB/s of overwrite
//! displaces ~1000 blocks/s: a serialized discard stream stealing ~2 GB/s.
//!
//! The fix: terminal frees ENQUEUE their device reclaim to a per-router
//! background reclaimer. Correctness never belonged to the discard — the
//! `begin_free → finish_free` window owns crash-safe free accounting and
//! the reclaimer now holds that window open until the (batched,
//! off-write-path) reclaim completes, so a queued discard can never race
//! a new owner's DMA at the reused offset.
//!
//! Contracts pinned here:
//!
//! 1. **Off the write path**: a terminal free QUEUES its reclaim
//!    (`block_free_reclaim_queued` + the `block_free_reclaim_queue_bytes`
//!    gauge) and the background worker completes it — the discard/punch
//!    counters still account every displaced block exactly once (the
//!    field ledger `block_free_discards ≡ displaced blocks` is preserved,
//!    now via the background counters), and the freed offset becomes
//!    reusable after the reclaim.
//! 2. **ENOSPC pressure valve**: an allocation that would refuse for
//!    space FORCES a synchronous drain of queued reclaims before failing
//!    (`block_free_reclaim_sync_drains` — 0 except under real space
//!    pressure). A full volume can never be wedged by lazily-queued
//!    space.
//! 3. **Clean unmount drains the queue**: dismount teardown returns all
//!    queued device space before declaring the unmount clean.
//! 4. **Conservation / exactly-once**: under concurrent frees, the
//!    background worker, and concurrent explicit drains, every enqueued
//!    range is reclaimed (or consciously skipped-and-counted) EXACTLY
//!    once — no double frees, no lost entries, every freed offset
//!    reusable exactly once.
//! 5. **Kill-9 with queued discards is harmless**: the queue is RAM-only
//!    space-return work; the pending accounting is rebuilt by the mount
//!    recovery walk. Live blocks are never touched, and the volume is
//!    fully usable after the crash. (Posture: a discard lost to a crash
//!    is UN-RETURNED thin-device space, not lost space — freed offsets
//!    re-enter the free list at recovery, allocation prefers the free
//!    list, and the new owner's write-before-publish repurposes the
//!    range; no journal-adjacent replay is needed.)
//! 6. **Fenced writer guard halts all device reclaims**: the D0 fail-stop
//!    lattice fences a usurped holder at its journal barriers, but the
//!    reclaim worker runs on the blocking pool, decoupled from barriers —
//!    unchecked, a fenced zombie with a deep queue would keep issuing
//!    BLKDISCARD/PUNCH_HOLE ioctls that, on non-PR (detection-grade)
//!    substrates, can land on offsets the successor writer has replayed
//!    and reallocated (data corruption of the new writer's blocks). The
//!    worker must observe the SAME `failed` latch the journal-barrier
//!    escalation sets (`KvMetaBackend::is_failed`, the
//!    `disabled_volumes` mirror source): once fenced, ZERO device
//!    commands from any path — worker batches, explicit drains, AND the
//!    ENOSPC valve (a fenced daemon must not sync-drain; allocation just
//!    fails, the daemon is dead anyway). Halted entries drop WITHOUT
//!    `finish_free` (the successor's recovery owns the accounting — the
//!    contract-5 posture) and are counted in
//!    `block_free_reclaim_fence_halts`.
//!
//! RED against dev 2bd041e: no reclaim queue exists; `free_block` punches
//! synchronously inside the free window and none of the
//! `block_free_reclaim_{queued,queue_bytes,batches,sync_drains}` family
//! exists.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BackendRouter, DataRouter};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

/// METRICS is process-global; counter-delta tests serialize (same pattern
/// as `block_free_reclaim_tests`).
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Env-knob guard: set a reclaim knob for one test, restore on drop
/// (tests are serialized by `serial()`; knobs are read at router
/// construction).
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}
impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

async fn make_router() -> (
    DataRouter,
    Arc<BlockAllocator>,
    NamedTempFile,
    tempfile::TempDir,
) {
    let dlm = DlmClient::new("local").unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), "async_block_reclaim_test")
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, ba.clone(), nvme);
    (router, ba, b, s)
}

fn punches() -> u64 {
    METRICS.block_free_file_punches.load(Ordering::Relaxed)
}
fn discards() -> u64 {
    METRICS.block_free_discards.load(Ordering::Relaxed)
}
fn skipped() -> u64 {
    METRICS.block_free_reclaim_skipped.load(Ordering::Relaxed)
}
fn queued() -> u64 {
    METRICS.block_free_reclaim_queued.load(Ordering::Relaxed)
}
fn queue_bytes() -> u64 {
    METRICS
        .block_free_reclaim_queue_bytes
        .load(Ordering::Relaxed)
}
fn sync_drains() -> u64 {
    METRICS
        .block_free_reclaim_sync_drains
        .load(Ordering::Relaxed)
}
fn fence_halts() -> u64 {
    METRICS
        .block_free_reclaim_fence_halts
        .load(Ordering::Relaxed)
}
fn double_frees() -> u64 {
    METRICS.block_double_frees.load(Ordering::Relaxed)
}

/// Poll until `cond` holds or ~5 s elapse (background-worker completion —
/// no sleep-for-synchronization: the condition IS the contract).
async fn eventually(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "background reclaim worker never completed: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Contract 1 — terminal free queues; the background worker completes it.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_free_queues_and_background_worker_reclaims() {
    let _g = serial().await;
    let (router, ba, _backing, _s) = make_router().await;

    let offset = ba.allocate_block().await.expect("alloc");
    let key = offset.to_string();
    ba.publish_block(offset);

    let (q0, qb0, p0, s0) = (queued(), queue_bytes(), punches(), skipped());
    router
        .backend_router
        .free_block(&key)
        .await
        .expect("terminal free");

    // The free ENQUEUED the reclaim (counter is monotonic — safe to assert
    // even if the worker already drained it).
    assert_eq!(
        queued() - q0,
        1,
        "a terminal free must queue its device reclaim, not issue it on \
         the write path"
    );

    // The background worker completes it: one counted punch (file
    // backing), gauge back to baseline, nothing skipped.
    eventually(
        || punches() - p0 == 1 && queue_bytes() == qb0,
        "one queued punch",
    )
    .await;
    assert_eq!(skipped() - s0, 0, "nothing skipped");

    // The freed offset is reusable AFTER the reclaim completed (the
    // begin_free → reclaim → finish_free window moved to the worker
    // wholesale — a queued discard can never race a new owner's DMA).
    let again = ba.allocate_block().await.expect("realloc");
    assert_eq!(
        again, offset,
        "the reclaimed offset must return to the free list (finish_free \
         belongs to the reclaimer now)"
    );
}

/// Non-terminal (clone-shared) frees queue nothing — extends contract 3 of
/// `block_free_reclaim_tests` to the queue counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nonterminal_free_queues_nothing() {
    let _g = serial().await;
    let (router, ba, _backing, _s) = make_router().await;

    let offset = ba.allocate_block().await.expect("alloc");
    let key = offset.to_string();
    ba.publish_block(offset);
    assert!(router.backend_router.increment_refcount(&key), "clone pin");

    let (q0, p0, d0) = (queued(), punches(), discards());
    router
        .backend_router
        .free_block(&key)
        .await
        .expect("non-terminal free");
    router.backend_router.reclaim_drain().await;
    assert_eq!(queued() - q0, 0, "non-terminal free must not queue");
    assert_eq!(punches() - p0, 0, "non-terminal free must not punch");
    assert_eq!(discards() - d0, 0, "non-terminal free must not discard");
}

// ---------------------------------------------------------------------------
// Contract 2 — the ENOSPC pressure valve.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enospc_pressure_valve_drains_queued_reclaims_before_refusing() {
    let _g = serial().await;
    // Park the background worker far beyond the test window: the queued
    // reclaims can ONLY be completed by the pressure valve.
    let _e = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "600000");
    let (router, ba, _backing, _s) = make_router().await;

    // A 4-block volume, filled to the brim.
    ba.set_capacity_bytes(4 * CHUNK_SIZE);
    let mut offsets = Vec::new();
    for _ in 0..4 {
        let o = ba.allocate_block().await.expect("fill volume");
        ba.publish_block(o);
        offsets.push(o);
    }

    // Free two blocks: space is QUEUED (worker parked), not yet reusable.
    let (sd0, p0) = (sync_drains(), punches());
    for o in &offsets[..2] {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
    }
    assert_eq!(
        punches() - p0,
        0,
        "worker is parked — the reclaims must still be queued"
    );

    // Allocate to the brim: both allocations MUST succeed — the valve
    // drains the queued reclaims synchronously instead of refusing.
    let a = ba
        .allocate_block()
        .await
        .expect("allocation must drain queued reclaims before ENOSPC");
    let b = ba
        .allocate_block()
        .await
        .expect("second allocation must also fit the drained space");
    assert!(offsets[..2].contains(&a) && offsets[..2].contains(&b) && a != b);
    assert!(
        sync_drains() - sd0 >= 1,
        "the pressure valve must be counted (block_free_reclaim_sync_drains)"
    );
    assert_eq!(punches() - p0, 2, "the valve completed both reclaims");

    // A 5th block genuinely does not fit: honest StorageFull, valve or not.
    let refused = ba.allocate_block().await;
    match refused {
        Err(squeezefs::error::SqueezefsError::Io(ref e))
            if e.kind() == std::io::ErrorKind::StorageFull => {}
        other => panic!("a genuinely full volume must refuse StorageFull, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Contract 3 — clean unmount drains the queue.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_unmount_drains_queued_reclaims() {
    let _g = serial().await;
    // Park the worker: only the dismount teardown can complete the queue.
    let _e = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "600000");
    let (router, ba, _backing, s) = make_router().await;
    let dlm = DlmClient::new("local").unwrap();
    let fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let mut freed = 0u64;
    let (p0, qb0) = (punches(), queue_bytes());
    for _ in 0..3 {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        fs.router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
        freed += 1;
    }
    assert_eq!(punches() - p0, 0, "worker parked — reclaims queued");

    let req = Request {
        unique: 0,
        uid: 0,
        gid: 0,
        pid: 0,
    };
    fs.destroy(req).await;

    assert_eq!(
        punches() - p0,
        freed,
        "dismount teardown must drain the reclaim queue (conservation: \
         nothing is lost on clean unmount)"
    );
    assert_eq!(
        queue_bytes(),
        qb0,
        "the queue-bytes gauge must return to baseline after the teardown \
         drain"
    );
    drop(s);
}

// ---------------------------------------------------------------------------
// Contract 4 — conservation / exactly-once under concurrency.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_frees_and_drains_reclaim_exactly_once() {
    let _g = serial().await;
    let (router, ba, _backing, _s) = make_router().await;

    const N: usize = 48;
    let mut offsets = Vec::new();
    for _ in 0..N {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        offsets.push(o);
    }

    let (p0, d0, s0, df0, qb0) = (
        punches(),
        discards(),
        skipped(),
        double_frees(),
        queue_bytes(),
    );

    // 8 freeing tasks racing the background worker AND 3 concurrent
    // explicit drains: every entry must be processed exactly once,
    // whoever pops it.
    let br = Arc::new(router.backend_router.clone());
    let mut tasks = Vec::new();
    for chunk in offsets.chunks(N / 8) {
        let br = br.clone();
        let chunk = chunk.to_vec();
        tasks.push(tokio::spawn(async move {
            for o in chunk {
                br.free_block(&o.to_string()).await.expect("terminal free");
            }
        }));
    }
    for _ in 0..3 {
        let br = br.clone();
        tasks.push(tokio::spawn(async move {
            br.reclaim_drain().await;
        }));
    }
    for t in tasks {
        t.await.expect("task");
    }
    // Whatever the drains missed, the worker (or a final drain) finishes.
    br.reclaim_drain().await;

    assert_eq!(
        (punches() - p0) + (discards() - d0) + (skipped() - s0),
        N as u64,
        "every enqueued range must be reclaimed-or-consciously-skipped \
         EXACTLY once"
    );
    assert_eq!(double_frees() - df0, 0, "no double finish_free");
    assert_eq!(queue_bytes(), qb0, "gauge conserved");

    // Every freed offset is reusable exactly once.
    let mut reused = std::collections::HashSet::new();
    for _ in 0..N {
        let o = ba.allocate_block().await.expect("realloc");
        assert!(reused.insert(o), "offset {o} handed out twice");
    }
    assert_eq!(
        reused,
        offsets
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        "the reallocated set must be exactly the freed set"
    );
}

// ---------------------------------------------------------------------------
// Contract 6 — a fenced writer guard halts all device reclaims.
// ---------------------------------------------------------------------------

/// Format + open one v3 metadata volume (the dismount-tests fixture shape).
async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

/// Drive the REAL D0 fail-stop: poison the meta backing (chmod 000 — the
/// next `uring_fs::fdatasync` open fails) and run `JOURNAL_FAILURE_LATCH`
/// (= 3) consecutive durability barriers. This latches the SAME `failed`
/// state a fenced holder's reservation-conflict barrier latches
/// immediately (`note_barrier_failure` — the signal `disabled_volumes`
/// mirrors and `check_volume_enabled` consults). No test-only backdoor.
async fn fence_meta_volume(
    routed: &squeezefs::meta_backend::RoutedMetaBackend,
    meta_path: &std::path::Path,
) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(meta_path, std::fs::Permissions::from_mode(0o000))
        .expect("poison meta backing");
    for _ in 0..3 {
        let _ = routed.volumes[0].sync_device().await;
    }
    assert!(
        routed.volumes[0].is_failed(),
        "harness premise: three failed durability barriers must latch the \
         volume failed (the journal-barrier fail-stop rung)"
    );
}

/// Contract 6a — the WORKER batch path: with the guard fenced BEFORE the
/// frees, the live background worker must issue ZERO device commands —
/// entries are dropped without finish_free and counted in
/// `block_free_reclaim_fence_halts`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_guard_halts_worker_batch_reclaims() {
    let _g = serial().await;
    let (router, ba, _backing, _s) = make_router().await;
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 64 * 1024 * 1024).await,
    ]));
    router.set_meta_backend(routed.clone());

    let mut offsets = Vec::new();
    for _ in 0..3 {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        offsets.push(o);
    }

    fence_meta_volume(&routed, m.path()).await;

    let (p0, d0, s0, f0, qb0) = (
        punches(),
        discards(),
        skipped(),
        fence_halts(),
        queue_bytes(),
    );
    for o in &offsets {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
    }
    // The live worker consumes the entries; it must HALT, not reclaim.
    eventually(
        || fence_halts() - f0 == 3 && queue_bytes() == qb0,
        "worker halts all 3 queued reclaims",
    )
    .await;
    assert_eq!(
        (punches() - p0) + (discards() - d0) + (skipped() - s0),
        0,
        "a fenced daemon must issue ZERO device reclaim commands — a \
         zombie's discard can destroy the successor writer's reallocated \
         blocks on detection-grade substrates"
    );
}

/// Contract 6b — the SYNC-DRAIN + ENOSPC-valve paths: explicit drains on a
/// fenced daemon issue zero device commands, and an allocation that would
/// need the valve just fails StorageFull — no sync drain, no finish_free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_guard_halts_sync_drain_and_enospc_valve() {
    let _g = serial().await;
    // Park the worker: the entries can only be consumed by the drain
    // paths under test.
    let _e = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "600000");
    let (router, ba, _backing, _s) = make_router().await;
    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 64 * 1024 * 1024).await,
    ]));
    router.set_meta_backend(routed.clone());

    ba.set_capacity_bytes(3 * CHUNK_SIZE);
    let mut offsets = Vec::new();
    for _ in 0..3 {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        offsets.push(o);
    }

    fence_meta_volume(&routed, m.path()).await;

    let (p0, d0, s0, f0, sd0, qb0) = (
        punches(),
        discards(),
        skipped(),
        fence_halts(),
        sync_drains(),
        queue_bytes(),
    );
    for o in &offsets {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
    }

    // Explicit drain: consumes the entries, issues NOTHING.
    router.backend_router.reclaim_drain().await;
    assert_eq!(fence_halts() - f0, 3, "drain must halt-count all entries");
    assert_eq!(
        (punches() - p0) + (discards() - d0) + (skipped() - s0),
        0,
        "a fenced daemon's drain must issue zero device commands"
    );
    assert_eq!(queue_bytes(), qb0, "gauge reconciled on the halt path");

    // ENOSPC valve: the freed offsets were NEVER finish_freed (the
    // successor's recovery owns them now), so allocation on the full
    // volume must fail StorageFull WITHOUT a valve drain — a fenced
    // daemon must not sync-drain either.
    let refused = ba.allocate_block().await;
    match refused {
        Err(squeezefs::error::SqueezefsError::Io(ref e))
            if e.kind() == std::io::ErrorKind::StorageFull => {}
        other => panic!("fenced-daemon allocation must fail StorageFull, got {other:?}"),
    }
    assert_eq!(
        sync_drains() - sd0,
        0,
        "the ENOSPC valve must not sync-drain on a fenced daemon"
    );
    assert_eq!(
        (punches() - p0) + (discards() - d0) + (skipped() - s0),
        0,
        "still zero device commands after the valve path"
    );
}

// ---------------------------------------------------------------------------
// Contract 5 — kill-9 with queued discards: zero residue, recoverable
// volume (re-exec pattern per tests/crash_kill_tests.rs).
// ---------------------------------------------------------------------------

fn ledger_append(path: &std::path::Path, line: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open ledger");
    f.write_all(format!("{line}\n").as_bytes()).expect("append");
    f.sync_data().expect("fsync ledger");
}

fn block_pattern(offset: u64) -> u8 {
    (((offset / CHUNK_SIZE) % 251) + 1) as u8
}

/// Child branch (`SQUEEZEFS_RECLAIM_CRASH_CHILD=1`): churn
/// write-live / free-queued against a file-backed volume with the reclaim
/// worker PARKED, so the SIGKILL always lands with discards queued.
#[test]
fn reclaim_crash_child_entry() {
    if std::env::var("SQUEEZEFS_RECLAIM_CRASH_CHILD").is_err() {
        return;
    }
    let backing = std::path::PathBuf::from(std::env::var("SQUEEZEFS_RECLAIM_CRASH_DEV").unwrap());
    let ledger = std::path::PathBuf::from(std::env::var("SQUEEZEFS_RECLAIM_CRASH_LEDGER").unwrap());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let dlm = DlmClient::new("local").unwrap();
        let nvme = Arc::new(NvmeBlockDev::new(backing.to_str().unwrap()));
        let ba = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), "reclaim_crash_child")
                .await
                .unwrap(),
        );
        let br = BackendRouter::new(
            ba.clone(),
            nvme.clone(),
            Arc::new(AtomicU64::new(CHUNK_SIZE)),
        );

        let mut i = 0u64;
        loop {
            // One LIVE block: written, published, acked, never freed.
            let live = ba.allocate_block().await.expect("alloc live");
            let pat = vec![block_pattern(live); 8192];
            nvme.write_block(live, bytes::Bytes::from(pat))
                .await
                .expect("write live");
            ba.publish_block(live);
            ledger_append(&ledger, &format!("live {live}"));

            // One FREED block: terminal free with the worker parked — the
            // discard stays QUEUED until the kill.
            let doomed = ba.allocate_block().await.expect("alloc doomed");
            let pat = vec![0xEEu8; 8192];
            nvme.write_block(doomed, bytes::Bytes::from(pat))
                .await
                .expect("write doomed");
            ba.publish_block(doomed);
            br.free_block(&doomed.to_string()).await.expect("free");
            ledger_append(&ledger, &format!("freed {doomed}"));

            i += 1;
            if i > 5000 {
                break; // parent kill is late — stop quietly
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill9_with_queued_discards_leaves_recoverable_volume() {
    let _g = serial().await;
    let rounds: u32 = std::env::var("SQUEEZEFS_RECLAIM_CRASH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let exe = std::env::current_exe().expect("test binary path");

    for round in 0..rounds {
        let dir = tempdir().unwrap();
        let backing = dir.path().join("reclaim.crash.dev");
        std::fs::File::create(&backing)
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        let ledger = dir.path().join("ledger.log");

        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "reclaim_crash_child_entry",
                "--test-threads=1",
                "--nocapture",
            ])
            .env("SQUEEZEFS_RECLAIM_CRASH_CHILD", "1")
            .env("SQUEEZEFS_RECLAIM_CRASH_DEV", &backing)
            .env("SQUEEZEFS_RECLAIM_CRASH_LEDGER", &ledger)
            // Park the worker: the kill must land with discards QUEUED.
            .env("SQUEEZEFS_RECLAIM_BATCH_MS", "600000")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn reclaim crash child");

        // Wait for the first QUEUED free, then kill inside a jitter window
        // (anchored on ledger progress, not wall clock — the
        // crash_kill_tests flake lesson).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut freed_seen = false;
        while std::time::Instant::now() < deadline {
            if ledger.exists()
                && std::fs::read_to_string(&ledger)
                    .map(|s| s.lines().any(|l| l.starts_with("freed ")))
                    .unwrap_or(false)
            {
                freed_seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(freed_seen, "round {round}: child never queued a free");
        let jitter = 5 + (round as u64 * 9) % 45;
        std::thread::sleep(std::time::Duration::from_millis(jitter));

        let pid = child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        let _ = child.wait();

        // Parse the ledger: live blocks (never freed) and freed blocks
        // (discards were queued; the queue died with the process).
        let text = std::fs::read_to_string(&ledger).unwrap_or_default();
        let mut live: Vec<u64> = Vec::new();
        let mut freed: Vec<u64> = Vec::new();
        for line in text.lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            match p.as_slice() {
                ["live", o] => live.push(o.parse().unwrap()),
                ["freed", o] => freed.push(o.parse().unwrap()),
                _ => {}
            }
        }

        // 1. LIVE blocks are untouched: no queued (or racing) discard ever
        //    reclaimed a referenced block.
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(&backing).expect("open backing");
        let mut buf = vec![0u8; 8192];
        for &o in &live {
            f.read_exact_at(&mut buf, o).expect("pread live");
            let want = block_pattern(o);
            assert!(
                buf.iter().all(|&x| x == want),
                "round {round}: live block at {o} lost its bytes after \
                 kill-9 with queued discards"
            );
        }

        // 2. Zero residue / recoverable: a fresh mount-shape recovery over
        //    the same backing (recover_block seeds each LIVE reference —
        //    the recovery-walk shape) leaves a volume that can allocate,
        //    write, free, and drain cleanly. Freed blocks' lost discards
        //    are un-returned thin space, nothing else.
        let (df0, uf0) = (
            double_frees(),
            METRICS
                .block_untracked_free_refusals
                .load(Ordering::Relaxed),
        );
        let dlm = DlmClient::new("local").unwrap();
        let nvme = Arc::new(NvmeBlockDev::new(backing.to_str().unwrap()));
        let ba = Arc::new(
            BlockAllocator::new(dlm.meta_client().clone(), "reclaim_crash_recovery")
                .await
                .unwrap(),
        );
        let br = BackendRouter::new(
            ba.clone(),
            nvme.clone(),
            Arc::new(AtomicU64::new(CHUNK_SIZE)),
        );
        for &o in &live {
            ba.recover_block(o / CHUNK_SIZE)
                .await
                .expect("recover live");
        }
        // The freed offsets re-enter circulation (gap-filled free list) —
        // allocate a few, write, read back, free, drain.
        for _ in 0..freed.len().clamp(1, 4) {
            let o = ba.allocate_block().await.expect("post-crash alloc");
            let pat = vec![0xA5u8; 4096];
            nvme.write_block(o, bytes::Bytes::from(pat.clone()))
                .await
                .expect("post-crash write");
            ba.publish_block(o);
            let mut rb = vec![0u8; 4096];
            f.read_exact_at(&mut rb, o).expect("pread post-crash");
            assert_eq!(rb, pat, "round {round}: post-crash write not readable");
            br.free_block(&o.to_string())
                .await
                .expect("post-crash free");
        }
        br.reclaim_drain().await;
        assert_eq!(
            double_frees() - df0,
            0,
            "round {round}: double free residue"
        );
        assert_eq!(
            METRICS
                .block_untracked_free_refusals
                .load(Ordering::Relaxed)
                - uf0,
            0,
            "round {round}: untracked-free residue"
        );
        // Live blocks still intact after the post-crash churn.
        for &o in &live {
            f.read_exact_at(&mut buf, o).expect("pread live 2");
            let want = block_pattern(o);
            assert!(
                buf.iter().all(|&x| x == want),
                "round {round}: post-crash churn touched a live block at {o}"
            );
        }
    }
}
