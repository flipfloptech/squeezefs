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
//!
//! ## Field ledger inversion (2026-07-27, three-session matrix) — contracts 7–9
//!
//! The user's 4-node NVMe-oF/TCP cluster (zram-lz4 data targets, dev tip
//! 78b9498) produced a three-session ledger the suite above never could:
//! at fill 1.0 a full rewrite showed `queued=0, batches=0, discards=0`
//! with `sync_drains=67,376` (≈ per allocation, climbing even idle-ish);
//! at fill 0.62 the engine engaged (queued +49,278, discards +56,615) but
//! `sync_drains` STILL ran ≈ 1 per allocation and the rewrite paid −33 %.
//! Root causes pinned by contracts 7–9 (red against dev `78b9498`):
//!
//! 7. **Free-list claim must survive the lost race** (`sync_drains == 0`
//!    at headroom): `try_allocate_block` read ONE free-list head and, on
//!    losing the `DashSet::remove` race, fell to `next_fresh_block()` —
//!    which refuses `StorageFull` on any cursor-at-cap store (any store
//!    that has EVER been full; `highest_block` never shrinks). Every lost
//!    race fired the ENOSPC valve: a counted `sync_drains` bump plus a
//!    SYNCHRONOUS whole-queue discard drain on the write path (the −33 %
//!    rewrite tax), at ANY fill. The claim loop must retry the next
//!    candidate until the list is observed empty.
//! 8. **The valve counts only genuine engagement**: a drain that found
//!    the queue empty (nothing to reclaim — the fill-1.0 idle climb) must
//!    NOT increment `block_free_reclaim_sync_drains`; the counter means
//!    "queued space was force-reclaimed for an allocation", nothing else.
//! 9. **Brim rewrites are space-neutral and must converge**: CoW
//!    allocate-before-free can never succeed at fill 1.0, and the
//!    never-lossy staging fallback silently turned a full store into an
//!    unbounded writeback spiral (zero enqueues, zero device commands,
//!    unbounded valve counts — the session-A ledger). A full-block
//!    overwrite of a sole-owned, undecorated, passthrough striped mapping
//!    must instead rewrite IN PLACE (the W1 incarnation fence, whole-block
//!    face): no allocation, no free, no staging detour — counted in
//!    `write_through_inplace_rewrites` — while genuine frees at the brim
//!    (rm/truncate) still ride the queue with device commands.
//!
//! Contracts 7–9 go through the REAL striped write path (fs.create /
//! fs.write / fs.setattr — the write-through + displacement machinery),
//! never a hand-armed queue: the fidelity gap that let the original suite
//! stay green while the field disengaged.
//!
//! ## Rewrite-wall drain rate (2026-07-31 write-wall campaign) — contracts 10–11
//!
//! Field conviction (4-node cluster, dev c9921f1, mid-rewrite at
//! 6.3–6.8 GB/s): `block_free_reclaim_queue_bytes` pinned at the queue
//! cap (17,418,944,512 B ≈ 4,153 × 4 MiB) with 196,747 queued vs 589
//! worker batches — the at-cap enqueue arm processed MOST displaced
//! blocks INLINE on the write path (the conservation-over-latency
//! backstop engaged as the steady state) because the single serial
//! worker drained ~500 blocks/s while the rewrite displaced ~1,650/s.
//! Backpressure is the DESIGNED last resort; a drain rate below any
//! sustainable displacement rate makes it the norm.
//!
//! 10. **Drain rate must exceed any sustainable displacement rate**: the
//!     worker fans device groups out across demand-derived parallel
//!     lanes (backends × queued demand, bounded per device); a
//!     displacement storm arriving at ~4× the SERIAL drain rate (the
//!     deterministic slow-device seam prices each lane pass) must never
//!     cap the queue — ZERO inline spills
//!     (`block_free_reclaim_inline_spills`, the engagement instrument) —
//!     while conservation (per-block ledger, exactly-once, gauge
//!     convergence) holds unchanged.
//! 11. **Adjacent-range coalescing survives the fan-out**: contiguous
//!     displaced ranges merge into FEW device commands
//!     (`block_free_reclaim_commands` — the command-economy face) while
//!     the per-block discard/punch ledger stays exact; lane chunking
//!     never splits what one batch could coalesce.
//!
//! RED against dev c9921f1: neither counter family exists, and the
//! worker drains serially (one batch of `SQUEEZEFS_RECLAIM_BATCH_BLOCKS`
//! per pass — contract 10's storm caps the queue and spills inline).
//!
//! ## Reclaim manners + park-don't-spill (write-wall iteration 1) — contracts 12–13
//!
//! Field verdict v2 (f629b46): the fresh row ran minutes after a
//! 128 GiB `rm` and paid −17 % to the backlog drain flooding the fabric
//! (settled re-measure: 12.3 GB/s vs the dirty 8.8), the rewrite row
//! still spilled 11,265 at-cap inline discards, and the width
//! experiments measured the TARGET deallocate service as the drain
//! ceiling (idle ~2,700 cmd/s at width 32 / ~5,900 at 128; under
//! foreground load ~1,700 at ANY width). Laws pinned here:
//!
//! 12. **The deferred-drain law**: foreground device I/O moving + queue
//!     below cap ⇒ ZERO device commands (the backlog waits); foreground
//!     idle ⇒ full-width catch-up to empty. Foreground detection is
//!     DEVICE-byte movement (stats pollers and RAM-served reads never
//!     hold the drain deferred); injectable seam for determinism.
//! 13. **Park-don't-spill**: an at-cap enqueue PARKS (async, bounded by
//!     `SQUEEZEFS_RECLAIM_CAP_PARK_MS`, counted
//!     `block_free_reclaim_cap_parks`) until the drain — which runs
//!     at-cap REGARDLESS of foreground — relieves it; bound expiry
//!     soft-overflows (`block_free_reclaim_cap_overflow`, ≈ 0 steady
//!     state). Device commands NEVER issue from the enqueue context
//!     (the retired inline arm charged a field-measured 12–22 ms
//!     synchronous fabric round-trip to the write path).
//!
//! RED against dev f629b46: the inline at-cap arm exists (spills), no
//! deferral exists, and neither cap_parks nor cap_overflow exists.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use fuse3::SetAttr;
use squeezefs::block_allocator::{BlockAllocator, CHUNK_SIZE};
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::meta_backend::Metadata;
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
fn cap_parks() -> u64 {
    METRICS.block_free_reclaim_cap_parks.load(Ordering::Relaxed)
}
fn cap_overflow() -> u64 {
    METRICS
        .block_free_reclaim_cap_overflow
        .load(Ordering::Relaxed)
}
fn reclaim_commands() -> u64 {
    METRICS.block_free_reclaim_commands.load(Ordering::Relaxed)
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

/// Contract 2b (probe-up-governor campaign, 2026-07-29 — the write-funnel
/// conviction): the ENOSPC valve's device-reclaim work must run OFF the
/// caller's executor thread. The old shape ran `drain_sync()` (1 ms
/// thread-sleeps + synchronous device discard/punch ioctls) INLINE in the
/// allocating task — on the fuse3 tpc lanes (current-thread runtimes)
/// each engagement froze a whole lane, stalling every handler and upload
/// future queued there: at steady-state rewrite near volume fill the
/// valve fires constantly (275 engagements / 30 s measured on the 4-wide
/// nvmet-tcp rig), so admitted pipeline blocks parked while ALL devices
/// starved in lockstep — the field's aqu-sz 2–4.5 with a full 512 MiB
/// pipe. Valve engagement must stall only the ALLOCATING TASK (honest
/// backpressure), never sibling tasks on the runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enospc_valve_never_blocks_executor_threads() {
    let _g = serial().await;
    // Park the background worker (valve-only completion) and stall the
    // reclaim work itself (the deterministic seam: reclaim = slow device).
    let _e = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "600000");
    let _s = EnvGuard::set("SQUEEZEFS_TEST_RECLAIM_STALL_MS", "300");
    let (router, ba, _backing, _staging) = make_router().await;

    ba.set_capacity_bytes(4 * CHUNK_SIZE);
    let mut offsets = Vec::new();
    for _ in 0..4 {
        let o = ba.allocate_block().await.expect("fill volume");
        ba.publish_block(o);
        offsets.push(o);
    }
    for o in &offsets[..2] {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
    }

    // Canary: a sibling task on the same runtime. If the valve blocks
    // executor threads, the timer-driven canary starves.
    let ticks = Arc::new(AtomicU64::new(0));
    let t2 = ticks.clone();
    let canary = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            t2.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Two concurrent brim allocations on SEPARATE tasks (one per worker
    // thread — the lane-freeze shape): both engage the (stalled) valve.
    let t0 = std::time::Instant::now();
    let ba1 = ba.clone();
    let ba2 = ba.clone();
    let h1 = tokio::spawn(async move { ba1.allocate_block().await });
    let h2 = tokio::spawn(async move { ba2.allocate_block().await });
    let (a, b) = (h1.await.unwrap(), h2.await.unwrap());
    let elapsed = t0.elapsed();
    let a = a.expect("valve must recover the queued space");
    let b = b.expect("valve must recover the queued space");
    assert!(offsets[..2].contains(&a) && offsets[..2].contains(&b) && a != b);
    assert!(
        elapsed >= std::time::Duration::from_millis(300),
        "fixture: the stalled valve must actually have engaged \
         (elapsed {elapsed:?})"
    );

    let observed = ticks.load(Ordering::Relaxed);
    canary.abort();
    // 300+ ms of valve engagement at 5 ms ticks ⇒ a live runtime observes
    // dozens; a runtime whose workers are frozen in the inline drain
    // observes ~0 (generous floor: 20).
    assert!(
        observed >= 20,
        "ENOSPC valve engagement must not freeze the runtime's executor \
         threads — device reclaim belongs on the blocking pool, with only \
         the ALLOCATING task awaiting (canary ticked {observed}× during \
         {elapsed:?})"
    );
}

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

/// The journal-head physical offset (the crash_kill_tests helper, verbatim).
fn journal_physical_offset(
    be: &squeezefs::meta_backend::kv::backend::KvMetaBackend,
    pos: u64,
) -> u64 {
    let geo = *be.journal_ring().core().geometry();
    be.superblock().journal.start + geo.page_index(pos) * 4096 + 24 + geo.in_page_off(pos)
}

/// Drive the REAL D0 fail-stop through the sanctioned `uring_fs` fault
/// harness (the exact recipe of crash_kill_tests::
/// `test_repeated_journal_failures_escalate_to_disabled_volume`): a torn
/// write at the journal head poisons the path — every subsequent request
/// fails EIO ("device died") — and repeated journal failures latch the
/// SAME `failed` state a fenced holder's reservation-conflict barrier
/// latches immediately (the signal `disabled_volumes` mirrors and
/// `check_volume_enabled` consults). No test-only backdoor.
async fn fence_meta_volume(routed: &squeezefs::meta_backend::RoutedMetaBackend) {
    let be = &routed.volumes[0];
    let head = be.journal_ring().core().head();
    squeezefs::uring_fs::arm_torn_write(journal_physical_offset(be, head), 0);
    for i in 0..8 {
        let _ = routed
            .create(1, &format!("fence-{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await;
        if be.is_failed() {
            break;
        }
    }
    squeezefs::uring_fs::clear_faults();
    assert!(
        be.is_failed(),
        "harness premise: repeated journal write failures must latch the \
         volume failed (the §4.4 pt 4 rung the fence probe consults)"
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

    fence_meta_volume(&routed).await;

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

    fence_meta_volume(&routed).await;

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

// ---------------------------------------------------------------------------
// Contracts 7–9 — the field ledger inversion (2026-07-27 three-session
// matrix). REAL striped write path: fs.create / fs.write / fs.setattr.
// ---------------------------------------------------------------------------

/// Real-path harness (the write_through_tests fixture shape): file-backed
/// data volume + v3 meta backend, so fs.create/fs.write drive the actual
/// striped write-through / displacement machinery.
struct FieldH {
    fs: Arc<SqueezefsFilesystem>,
    req: Request,
    ba: Arc<BlockAllocator>,
    backing: NamedTempFile,
    _m: NamedTempFile,
    _s: tempfile::TempDir,
}

/// Block size of the real-path harness (`SQUEEZEFS_DEFAULT_BLOCK_SIZE`).
const FBS: u64 = 4096;

/// RAII lever: force the W1 patch OFF (the sanctioned §6 A/B knob) so
/// full-block overwrites exercise the CoW displacement path under test —
/// the field's 4 MiB-block shape, where every write is patch-oversize.
struct PatchOff(u64);
impl PatchOff {
    fn arm() -> Self {
        let prev = squeezefs::fuse_client::patch_max_bytes();
        squeezefs::fuse_client::set_patch_max_bytes(0);
        Self(prev)
    }
}
impl Drop for PatchOff {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_patch_max_bytes(self.0);
    }
}

/// Pin the CoW-always venue (write-wall iteration 1): contracts 7/9 pin
/// the COW displacement ledger and the StorageFull-triggered brim arm —
/// with the default in-place-overwrite arm ON, eligible rewrites never
/// reach either (they land in place BEFORE allocation;
/// `tests/inplace_overwrite_tests.rs` owns that venue).
struct InplaceOff;
impl InplaceOff {
    fn arm() -> Self {
        squeezefs::fuse_client::set_inplace_overwrite(false);
        Self
    }
}
impl Drop for InplaceOff {
    fn drop(&mut self) {
        squeezefs::fuse_client::set_inplace_overwrite(false);
    }
}

async fn make_field_harness(test_id: &str) -> FieldH {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 128 * 1024 * 1024).await,
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    FieldH {
        fs: Arc::new(fs),
        req,
        ba,
        backing,
        _m: m,
        _s: s,
    }
}

async fn field_create(h: &FieldH, name: &str) -> u64 {
    h.fs.create(
        h.req,
        1,
        std::ffi::OsStr::new(name),
        libc::S_IFREG | 0o644,
        0,
    )
    .await
    .unwrap()
    .attr
    .ino
}

async fn field_write(fs: &SqueezefsFilesystem, req: Request, ino: u64, off: u64, data: &[u8]) {
    let w = fs
        .write(req, ino, 0, off, bytes::Bytes::copy_from_slice(data), 0, 0)
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn field_read(h: &FieldH, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

fn field_pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
}

/// Fresh striped file of `blocks` full blocks (one big first write — the
/// fresh-file direct striped route; asserts the striped premise).
async fn field_make_striped(h: &FieldH, name: &str, blocks: u64, seed: u8) -> u64 {
    let ino = field_create(h, name).await;
    let p = field_pattern((blocks * FBS) as usize, seed);
    field_write(&h.fs, h.req, ino, 0, &p).await;
    // Write-pipeline drain (2026-07-27 depth campaign): coverage-complete
    // writes ACK with custody parked; the mapped-premise and every
    // fill/ledger observation downstream are deterministic only at the
    // pipeline's drain point.
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "fixture write pipeline must drain"
    );
    let path = squeezefs::keys::inode_path(ino);
    h.fs.router.metadata_cache.remove(&ino);
    let meta = h.fs.router.fetch_metadata(&path).await.unwrap();
    assert_eq!(
        meta.file_type, "striped",
        "premise: fresh {blocks}-block file must be striped"
    );
    assert_eq!(
        meta.block_map.as_ref().map(|m| m.len()).unwrap_or(0),
        blocks as usize,
        "premise: every block mapped"
    );
    ino
}

/// Truncate to zero through the real setattr path: every mapped block is
/// a terminal free riding the displacement/free machinery.
async fn field_truncate_zero(h: &FieldH, ino: u64) {
    h.fs.setattr(
        h.req,
        ino,
        None,
        SetAttr {
            size: Some(0),
            ..Default::default()
        },
    )
    .await
    .expect("truncate to zero");
}

fn batches() -> u64 {
    METRICS.block_free_reclaim_batches.load(Ordering::Relaxed)
}
fn wt_blocks() -> u64 {
    METRICS.write_through_blocks.load(Ordering::Relaxed)
}
fn wt_fallbacks() -> u64 {
    METRICS.write_through_fallbacks.load(Ordering::Relaxed)
}
fn inplace_rewrites() -> u64 {
    METRICS
        .write_through_inplace_rewrites
        .load(Ordering::Relaxed)
}
fn untracked_refusals() -> u64 {
    METRICS
        .block_untracked_free_refusals
        .load(Ordering::Relaxed)
}

/// Contract 7 (+ the conviction-3 tax mechanism): a striped rewrite in the
/// FREE-LIST REGIME — cursor at cap, real headroom on the free list, the
/// shape of ANY store that has ever been full (the field's post-rm fill
/// 0.62) — must run entirely off the background reclaimer: `sync_drains`
/// stays 0 (no valve on the write path), every displaced block enqueues,
/// the worker batches them, and device commands account for every one.
///
/// RED against dev 78b9498: concurrent allocators all read the SAME
/// free-list head; losers of the `DashSet::remove` race fall to
/// `next_fresh_block()`, which is `StorageFull` at the cap — so the valve
/// fires ≈ per allocation (counted + a synchronous whole-queue drain on
/// the write path) despite 50 % headroom, and double-losers fail writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn field_rewrite_free_list_regime_runs_off_the_write_path() {
    let _g = serial().await;
    let _p = PatchOff::arm();
    let _i = InplaceOff::arm();
    let h = make_field_harness("field_free_list_regime").await;

    // 32-chunk volume, filled to the brim by 8 × 4-block fresh files —
    // the cursor is now AT CAP forever (highest_block never shrinks).
    h.ba.set_capacity_bytes(32 * CHUNK_SIZE);
    let mut inos = Vec::new();
    for i in 0..8 {
        inos.push(field_make_striped(&h, &format!("g{i}"), 4, i as u8).await);
    }

    // Free half the store through the real truncate path (the field's
    // session-B rm): 16 terminal frees ride the queue; settle the worker.
    for &ino in &inos[4..] {
        field_truncate_zero(&h, ino).await;
    }
    h.fs.router.backend_router.reclaim_drain().await;
    assert_eq!(
        h.ba.free_blocks_count(),
        16,
        "premise: free-list regime (fill 0.5, cursor at cap)"
    );

    let (sd0, q0, p0, d0, s0, b0, wt0, uf0, df0) = (
        sync_drains(),
        queued(),
        punches(),
        discards(),
        skipped(),
        batches(),
        wt_blocks(),
        untracked_refusals(),
        double_frees(),
    );

    // 3 rounds × 8 CONCURRENT full-block rewrites (2 files × 4 blocks,
    // one spawned task per block — true OS-thread parallelism on the
    // allocator): 24 displaced blocks total, never more than 8 in flight
    // against 16 free — allocation must never genuinely exhaust.
    const ROUNDS: u64 = 3;
    for round in 0..ROUNDS {
        let mut tasks = Vec::new();
        for (fi, &ino) in inos[..2].iter().enumerate() {
            for b in 0..4u64 {
                let fs = h.fs.clone();
                let req = h.req;
                let seed = (round * 16 + fi as u64 * 4 + b) as u8;
                tasks.push(tokio::spawn(async move {
                    let p = field_pattern(FBS as usize, seed);
                    field_write(&fs, req, ino, b * FBS, &p).await;
                }));
            }
        }
        for t in tasks {
            t.await.expect("rewrite task");
        }
        // The WORKER must complete the round's displaced reclaims (no
        // explicit drain here — batches>0 is part of the contract).
        eventually(
            || queue_bytes() == 0,
            "worker drains the round's displaced reclaims",
        )
        .await;
    }

    let displaced = ROUNDS * 8;
    assert_eq!(
        sync_drains() - sd0,
        0,
        "free-list-regime rewrites must NEVER fire the ENOSPC valve — a \
         lost free-list claim race must retry the next candidate, not \
         fresh-mint into StorageFull (the field's per-allocation \
         sync_drains at 38 % headroom)"
    );
    assert_eq!(
        queued() - q0,
        displaced,
        "every displaced block must enqueue to the background reclaimer"
    );
    assert_eq!(
        (punches() - p0) + (discards() - d0) + (skipped() - s0),
        displaced,
        "device commands must account for every displaced block"
    );
    assert!(
        batches() - b0 > 0,
        "the background worker (not a write-path drain) must process the \
         displaced reclaims"
    );
    assert_eq!(
        wt_blocks() - wt0,
        displaced,
        "engagement: every rewrite must ride the real write-through path"
    );
    assert_eq!(untracked_refusals() - uf0, 0, "no untracked-free residue");
    assert_eq!(double_frees() - df0, 0, "no double-free residue");

    // Content survives the churn: final round's pattern reads back.
    for (fi, &ino) in inos[..2].iter().enumerate() {
        for b in 0..4u64 {
            let seed = ((ROUNDS - 1) * 16 + fi as u64 * 4 + b) as u8;
            assert_eq!(
                field_read(&h, ino, b * FBS, FBS as u32).await,
                field_pattern(FBS as usize, seed),
                "file {fi} block {b} content after concurrent rewrite rounds"
            );
        }
    }
}

/// Contract 8 — valve economy: `block_free_reclaim_sync_drains` counts
/// ONLY genuine allocation-failure-driven drains that reclaimed queued
/// space. (a) A genuinely-full store with an EMPTY queue refuses
/// StorageFull without counting a drain (the field's fill-1.0 idle climb
/// — one bump per failed allocation attempt, forever). (b) Racing
/// allocators consuming a pre-reclaimed free list at cap never fire the
/// valve at all and never fail.
///
/// RED against dev 78b9498: (a) counts +1 per attempt on an empty queue;
/// (b) lost head-claim races fresh-mint into StorageFull → valve bumps
/// and double-losers surface allocation errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn valve_counts_only_genuine_nonempty_drains() {
    let _g = serial().await;
    let (router, ba, _backing, _s) = make_router().await;
    let _keepalive = &router;

    // (a) Genuine full, empty queue: honest StorageFull, NO counted drain.
    ba.set_capacity_bytes(CHUNK_SIZE);
    let only = ba.allocate_block().await.expect("fill the 1-block volume");
    ba.publish_block(only);
    let sd0 = sync_drains();
    for attempt in 0..3 {
        match ba.allocate_block().await {
            Err(squeezefs::error::SqueezefsError::Io(ref e))
                if e.kind() == std::io::ErrorKind::StorageFull => {}
            other => panic!("attempt {attempt}: expected StorageFull, got {other:?}"),
        }
    }
    assert_eq!(
        sync_drains() - sd0,
        0,
        "an empty-queue valve pass is a no-op, not engagement — it must \
         not be counted (the field's unbounded idle sync_drains climb)"
    );

    // (b) Free-list regime at cap: 64 entries pre-reclaimed, 16 racing
    // tasks × 4 tight-loop allocations consume them exactly. No frees in
    // flight, no queue involvement — the valve must never fire and every
    // allocation must succeed with a distinct offset.
    ba.set_capacity_bytes(64 * CHUNK_SIZE);
    let mut offsets = Vec::new();
    while let Ok(o) = ba.allocate_block().await {
        ba.publish_block(o);
        offsets.push(o);
    }
    assert_eq!(offsets.len(), 63, "premise: cursor driven to cap");
    for o in &offsets {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
    }
    router.backend_router.reclaim_drain().await;
    assert_eq!(
        ba.free_blocks_count(),
        63,
        "premise: everything reclaimed onto the free list"
    );

    let sd1 = sync_drains();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let ba = ba.clone();
        tasks.push(tokio::spawn(async move {
            let mut got = Vec::new();
            for _ in 0..3 {
                got.push(
                    ba.allocate_block()
                        .await
                        .expect("free-list-regime allocation must not fail"),
                );
            }
            got
        }));
    }
    let mut claimed = std::collections::HashSet::new();
    for t in tasks {
        for o in t.await.expect("alloc task") {
            assert!(claimed.insert(o), "offset {o} handed out twice");
        }
    }
    assert_eq!(claimed.len(), 48, "48 racing allocations all served");
    assert_eq!(
        sync_drains() - sd1,
        0,
        "consuming a populated free list at cap must never fire the \
         ENOSPC valve — the lost-race fresh-mint fallback is the field's \
         per-allocation sync_drains + write-path drain tax"
    );
}

/// Contract 9 — the brim (fill 1.0): a space-neutral full-block rewrite
/// of a sole-owned striped file must CONVERGE — in place, on the device,
/// with no staging spiral — and genuine frees at the brim must still ride
/// the queue with device commands.
///
/// RED against dev 78b9498: every rewrite block fails allocation, counts
/// an empty-queue sync drain, and degrades into the never-lossy staging
/// fallback (`write_through_fallbacks` ≈ blocks) — acked bytes never
/// reach the device (the session-A ledger: queued=0, zero device
/// commands, sync_drains ≈ allocations), and
/// `write_through_inplace_rewrites` does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn brim_rewrite_converges_in_place_with_honest_ledger() {
    let _g = serial().await;
    let _p = PatchOff::arm();
    let _i = InplaceOff::arm();
    let h = make_field_harness("field_brim_rewrite").await;

    // 8-chunk volume filled exactly by two 4-block files: fill 1.0,
    // empty free list, empty queue.
    h.ba.set_capacity_bytes(8 * CHUNK_SIZE);
    let f1 = field_make_striped(&h, "f1", 4, 11).await;
    let f2 = field_make_striped(&h, "f2", 4, 22).await;
    assert_eq!(h.ba.free_blocks_count(), 0, "premise: brim");

    // Capture f1's mapping (offset per block) BEFORE the rewrite.
    let path = squeezefs::keys::inode_path(f1);
    h.fs.router.metadata_cache.remove(&f1);
    let before = h.fs.router.fetch_metadata(&path).await.unwrap();
    let map_before = before.block_map.clone().expect("mapped");

    let (sd0, q0, p0, d0, s0, fb0, ip0, wt0) = (
        sync_drains(),
        queued(),
        punches(),
        discards(),
        skipped(),
        wt_fallbacks(),
        inplace_rewrites(),
        wt_blocks(),
    );

    // Full rewrite of f1, block by block (the field's session-A shape).
    for b in 0..4u64 {
        let p = field_pattern(FBS as usize, 100 + b as u8);
        field_write(&h.fs, h.req, f1, b * FBS, &p).await;
    }
    // The rewrites' detached uploads settle at the drain point (the
    // standing pipeline-rendezvous adaptation — the counters below are
    // exact only once every admitted upload completed).
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "rewrite pipeline must drain"
    );

    assert_eq!(
        wt_fallbacks() - fb0,
        0,
        "a brim rewrite must CONVERGE, not spiral into the staging \
         fallback (the session-A unbounded writeback backlog)"
    );
    assert_eq!(
        sync_drains() - sd0,
        0,
        "empty-queue valve passes at the brim must not be counted"
    );
    assert_eq!(
        inplace_rewrites() - ip0,
        4,
        "each sole-owner full-block rewrite at the brim lands IN PLACE \
         (write_through_inplace_rewrites is the engagement counter)"
    );
    assert_eq!(
        wt_blocks() - wt0,
        4,
        "in-place brim rewrites are still write-throughs"
    );
    assert_eq!(
        queued() - q0,
        0,
        "an in-place rewrite displaces nothing — no free, no queue entry"
    );

    // The mapping is UNCHANGED (in place) and the bytes are ON THE DEVICE
    // (not parked in staging): pread the backing file at each mapped
    // offset — passthrough volume, plaintext on device.
    h.fs.router.metadata_cache.remove(&f1);
    let after = h.fs.router.fetch_metadata(&path).await.unwrap();
    let map_after = after.block_map.clone().expect("still mapped");
    assert_eq!(
        *map_before, *map_after,
        "in-place rewrite must not move the mapping"
    );
    use std::os::unix::fs::FileExt;
    let dev = std::fs::File::open(h.backing.path()).expect("open backing");
    let mut buf = vec![0u8; FBS as usize];
    for b in 0..4u32 {
        let key = map_after.get(&b).expect("mapped block");
        let (_, off) = h.fs.router.backend_router.parse_block_key(key).unwrap();
        dev.read_exact_at(&mut buf, off).expect("pread device");
        assert_eq!(
            buf,
            field_pattern(FBS as usize, 100 + b as u8),
            "block {b}: the rewrite must be ON THE DEVICE at the mapped \
             offset, not acked into a staging spiral"
        );
        assert!(
            h.fs.router
                .cache
                .nvme
                .read_staged(&staged_field_key(f1, b as u64))
                .is_none(),
            "block {b}: no staged residue after an in-place brim rewrite"
        );
    }
    for b in 0..4u64 {
        assert_eq!(
            field_read(&h, f1, b * FBS, FBS as u32).await,
            field_pattern(FBS as usize, 100 + b as u8),
            "read-back through the FUSE path"
        );
    }

    // Genuine frees at the brim still ride the queue WITH device
    // commands (accounting-only finish_free would leak thin space).
    field_truncate_zero(&h, f2).await;
    assert_eq!(
        queued() - q0,
        4,
        "brim truncate frees must enqueue to the reclaimer"
    );
    eventually(
        || (punches() - p0) + (discards() - d0) + (skipped() - s0) == 4 && queue_bytes() == 0,
        "brim frees reclaimed with device commands",
    )
    .await;

    // And the reclaimed space is allocatable: a fresh 4-block file lands.
    let f3 = field_make_striped(&h, "f3", 4, 33).await;
    assert_eq!(
        field_read(&h, f3, 0, (4 * FBS) as u32).await,
        field_pattern((4 * FBS) as usize, 33),
        "fresh file over reclaimed brim space"
    );
}

fn staged_field_key(ino: u64, b: u64) -> String {
    squeezefs::keys::active_block(ino, b).to_string()
}

// ---------------------------------------------------------------------------
// Contract 10 — drain rate: a displacement storm at ~4× the SERIAL drain
// rate must never cap the queue (zero inline spills) while conservation
// holds. The 2026-07-31 write-wall campaign's rewrite-wall conviction.
// ---------------------------------------------------------------------------

/// Poll like [`eventually`] but with a longer deadline (a 256-block
/// storm's full drain on a seam-priced slow device).
async fn eventually_within(
    mut cond: impl FnMut() -> bool,
    deadline: std::time::Duration,
    what: &str,
) {
    let deadline = std::time::Instant::now() + deadline;
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "background reclaim never converged: {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn displacement_storm_never_caps_queue_or_spills_inline() {
    let _g = serial().await;
    // Deterministic drain-rate pricing (the field's slow-target shape):
    // each lane pass costs 50 ms (the seam), batches are 8 blocks, so the
    // SERIAL drain rate is 8 / 50 ms = 160 blocks/s. The storm below
    // arrives at ~640 blocks/s — 4× serial — and the cap is 96: the
    // pre-campaign single-lane worker caps within ~0.2 s and spills
    // inline; the demand-derived fan-out must not.
    let _b = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", "8");
    let _m = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "0");
    let _s = EnvGuard::set("SQUEEZEFS_TEST_RECLAIM_STALL_MS", "50");
    let _c = EnvGuard::set("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", "96");
    let (router, ba, _backing, _staging) = make_router().await;

    // 256 published blocks to displace (offsets may run past the sparse
    // backing's EOF — a refused punch is a counted skip, and the
    // conservation ledger below accepts punched-or-skipped).
    let mut offsets = Vec::with_capacity(256);
    for _ in 0..256 {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        offsets.push(o);
    }

    let (sp0, ov0, p0, s0, d0, df0, qb0) = (
        cap_parks(),
        cap_overflow(),
        punches(),
        skipped(),
        discards(),
        double_frees(),
        queue_bytes(),
    );

    // The storm: 256 terminal frees paced at ~640 blocks/s (8 per
    // 12.5 ms tick — pacing is the WORKLOAD shape, not synchronization).
    for chunk in offsets.chunks(8) {
        for o in chunk {
            router
                .backend_router
                .free_block(&o.to_string())
                .await
                .expect("terminal free");
        }
        tokio::time::sleep(std::time::Duration::from_millis(12)).await;
    }

    // THE contract: the queue never capped — no displaced block's free
    // ever parked (nor overflowed) on the (simulated) write path.
    assert_eq!(
        cap_parks() - sp0,
        0,
        "a displacement storm at 4× the serial drain rate must never \
         reach the deferred-space cap — the reclaim worker's \
         demand-derived lanes must outrun any sustainable displacement \
         rate (the 2026-07-31 rewrite-wall conviction)"
    );
    assert_eq!(cap_overflow() - ov0, 0, "and never soft-overflow");

    // Conservation unchanged: every block reclaimed-or-consciously-
    // skipped exactly once, gauge converges to baseline.
    eventually_within(
        || (punches() - p0) + (skipped() - s0) + (discards() - d0) == 256 && queue_bytes() == qb0,
        std::time::Duration::from_secs(30),
        "storm blocks reclaimed exactly once, gauge back to baseline",
    )
    .await;
    assert_eq!(double_frees() - df0, 0, "exactly-once under the fan-out");
}

// ---------------------------------------------------------------------------
// Contract 11 — adjacent-range coalescing survives the fan-out: few
// device commands, exact per-block ledger.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adjacent_displaced_ranges_coalesce_into_fewer_device_commands() {
    let _g = serial().await;
    // One accumulation window collects the whole burst into one take.
    let _b = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", "64");
    let _m = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "100");
    let (router, ba, _backing, _staging) = make_router().await;

    // 64 CONTIGUOUS blocks (fresh allocator: sequential chunk cursor —
    // asserted, the premise must be loud).
    let mut offsets = Vec::with_capacity(64);
    for _ in 0..64 {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        offsets.push(o);
    }
    for (i, o) in offsets.iter().enumerate() {
        assert_eq!(
            *o,
            offsets[0] + i as u64 * CHUNK_SIZE,
            "premise: fresh allocator must hand out contiguous chunks"
        );
    }

    let (c0, p0, s0, qb0) = (reclaim_commands(), punches(), skipped(), queue_bytes());

    // Burst-free all 64 inside the accumulation window.
    for o in &offsets {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
    }

    eventually(
        || (punches() - p0) + (skipped() - s0) == 64 && queue_bytes() == qb0,
        "burst reclaimed with per-block ledger intact",
    )
    .await;
    assert_eq!(
        skipped() - s0,
        0,
        "premise: contiguous in-file ranges must actually punch"
    );

    // The command-economy face: 64 adjacent blocks must merge into FEW
    // device commands (one per lane chunk at most — coalescing must
    // survive the parallel fan-out).
    let cmds = reclaim_commands() - c0;
    assert!(
        (1..=8).contains(&cmds),
        "64 contiguous displaced blocks must coalesce into ≤ 8 device \
         commands (got {cmds}); per-block counting stays exact by the \
         punches assertion above"
    );
}

// ---------------------------------------------------------------------------
// Contract 12 — reclaim manners (write-wall iteration 1): the drain
// DEFERS while foreground device I/O moves and the queue sits below the
// cap; a foreground-idle fabric gets the full-width catch-up. Field
// motivation: verdict-v2's fresh row paid −17 % to a 128 GiB rm-backlog
// drain flooding the fabric, and the width experiments measured ZERO
// drain-rate gain from client width under foreground load — deferring is
// the only move that returns fabric/target capacity to the foreground.
// ---------------------------------------------------------------------------

/// An injectable foreground signal: `advance=true` ⇒ every probe reads a
/// fresh value (foreground device I/O moving); `false` ⇒ frozen (idle).
struct FgSeam {
    val: Arc<AtomicU64>,
    advance: Arc<std::sync::atomic::AtomicBool>,
}
impl FgSeam {
    fn install(br: &squeezefs::routing::BackendRouter) -> Self {
        let val = Arc::new(AtomicU64::new(1));
        let advance = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (v, a) = (val.clone(), advance.clone());
        br.set_reclaim_foreground_signal(Arc::new(move || {
            if a.load(Ordering::Relaxed) {
                v.fetch_add(1, Ordering::Relaxed) + 1
            } else {
                v.load(Ordering::Relaxed)
            }
        }));
        Self { val, advance }
    }
    fn idle(&self) {
        self.advance.store(false, Ordering::Relaxed);
        // One extra bump so the worker's NEXT probe observes one final
        // advance, then quiescence (the realistic end-of-row shape).
        self.val.fetch_add(1, Ordering::Relaxed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_defers_under_foreground_and_catches_up_idle() {
    let _g = serial().await;
    let _m = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "0");
    let (router, ba, _backing, _staging) = make_router().await;
    let seam = FgSeam::install(&router.backend_router);

    let mut offsets = Vec::with_capacity(16);
    for _ in 0..16 {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        offsets.push(o);
    }
    let (p0, s0, c0, qb0) = (punches(), skipped(), reclaim_commands(), queue_bytes());
    for o in &offsets {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free");
    }

    // Foreground moving + queue below cap ⇒ ZERO device commands. (A
    // negative assertion needs an observation window — 500 ms spans ten
    // 50 ms manners re-evaluations; generous by construction.)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        (punches() - p0) + (skipped() - s0),
        0,
        "the drain must DEFER while foreground device I/O moves (the \
         manners law — a deferred backlog waits for idle fabric)"
    );
    assert_eq!(reclaim_commands() - c0, 0, "zero device commands too");
    assert!(
        queue_bytes() > qb0,
        "the deferred backlog is visibly queued (gauge)"
    );

    // Foreground idle ⇒ the full-width catch-up drains to empty.
    seam.idle();
    eventually(
        || (punches() - p0) + (skipped() - s0) == 16 && queue_bytes() == qb0,
        "idle catch-up drains the deferred backlog to empty",
    )
    .await;
    assert_eq!(double_frees(), 0, "exactly-once under deferral");
}

// ---------------------------------------------------------------------------
// Contract 13 — park-don't-spill at the cap: an at-cap enqueue PARKS and
// the drain relieves it REGARDLESS of foreground (parked enqueues are
// foreground writers too); device commands never issue from the enqueue
// context (the retired inline arm charged a field-measured 12–22 ms
// synchronous fabric round-trip to the write path).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_cap_enqueue_parks_until_drain_relieves_and_never_overflows() {
    let _g = serial().await;
    let _b = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", "8");
    let _m = EnvGuard::set("SQUEEZEFS_RECLAIM_BATCH_MS", "0");
    let _c = EnvGuard::set("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", "8");
    let _s = EnvGuard::set("SQUEEZEFS_TEST_RECLAIM_STALL_MS", "50");
    let _p = EnvGuard::set("SQUEEZEFS_RECLAIM_CAP_PARK_MS", "60000");
    let (router, ba, _backing, _staging) = make_router().await;
    // Foreground stays MOVING the whole time: the cap-relief drain must
    // override the manners deferral.
    let _seam = FgSeam::install(&router.backend_router);

    let mut offsets = Vec::with_capacity(32);
    for _ in 0..32 {
        let o = ba.allocate_block().await.expect("alloc");
        ba.publish_block(o);
        offsets.push(o);
    }
    let (pk0, ov0, p0, s0, qb0, df0) = (
        cap_parks(),
        cap_overflow(),
        punches(),
        skipped(),
        queue_bytes(),
        double_frees(),
    );
    for o in &offsets {
        router
            .backend_router
            .free_block(&o.to_string())
            .await
            .expect("terminal free (parks at cap, never spills inline)");
    }
    assert!(
        cap_parks() - pk0 > 0,
        "a 32-block burst against an 8-block cap must park enqueues \
         (the engagement gauge of park-don't-spill)"
    );
    assert_eq!(
        cap_overflow() - ov0,
        0,
        "the drain relieves parked enqueues well inside the liveness \
         bound — no soft overflow"
    );
    eventually_within(
        || (punches() - p0) + (skipped() - s0) == 32 && queue_bytes() == qb0,
        std::time::Duration::from_secs(30),
        "every burst block reclaimed exactly once through the park path",
    )
    .await;
    assert_eq!(double_frees() - df0, 0, "exactly-once under parking");
}
