//! Background block-reclaim queue — the overwrite-throughput fix
//! (`.benchmarks/2026-07-27-async-block-reclaim.md`; contract pinned in
//! `tests/async_block_reclaim_tests.rs`).
//!
//! A striped overwrite's displaced blocks used to issue their device
//! reclaim (`BLKDISCARD` on namespaces / `PUNCH_HOLE` on file backings —
//! `free_reclaim_op`) SYNCHRONOUSLY inside `BackendRouter::free_block`.
//! On NVMe-oF every discard is a fabric round-trip (~235 µs measured), so
//! a 4 GiB/s overwrite stream (≈1000 displaced 4 MiB blocks/s) serialized
//! ~2 GB/s of throughput behind space-return work that owns NO
//! correctness: the `begin_free → finish_free` window owns crash-safe
//! free accounting; the reclaim is purely returning space to the device.
//!
//! Design:
//!
//! * A terminal free enqueues a [`ReclaimEntry`] — with `begin_free`
//!   already taken and the offset registered in the allocator's in-flight
//!   registry — and returns. The background worker (or a drain) pops the
//!   entry, issues the reclaim OFF the write path, then `finish_free`s.
//!   The offset is **not reallocatable until after its reclaim**, exactly
//!   the pre-existing law, so a queued discard can never race a new
//!   owner's DMA at the reused offset.
//! * **Exactly-once**: the lock-free `SegQueue` pop is the ownership
//!   transfer — whoever pops an entry (worker batch, ENOSPC valve drain,
//!   unmount drain) processes it; nothing is ever re-queued.
//! * **ENOSPC pressure valve**: `BlockAllocator::allocate_block` invokes
//!   the wired [`ReclaimQueue::drain_sync`] before refusing for space —
//!   a full volume can never be wedged by lazily-queued reclaims.
//!   `block_free_reclaim_sync_drains` counts ONLY passes that actually
//!   processed queued entries (contract 8, field ledger inversion
//!   2026-07-27): an empty-queue pass is a no-op, never engagement.
//! * **Crash posture** (kill-9 with queued entries): the queue is
//!   RAM-only space-return work. The mount recovery walk rebuilds
//!   refcounts/free-list from durable layout maps, so the freed offsets
//!   re-enter the free list; the only loss is the *discard itself* —
//!   un-returned thin-device space, re-covered when the offset is reused
//!   (allocation prefers the free list, and write-before-publish rewrites
//!   the range) or freed terminally again. No journal-adjacent replay is
//!   needed and none is kept.
//! * **io_uring posture**: `BLKDISCARD` and `fallocate(PUNCH_HOLE)` are
//!   synchronous ioctl/fallocate calls with no io_uring opcode on the
//!   kernels we target. Per the io_uring-first policy this is acceptable
//!   ONLY because the work is off the hot path: batches run on the
//!   blocking pool (`spawn_blocking`), never on a tokio worker and never
//!   on the FUSE write path.
//!
//! * **Demand-derived parallel drain** (2026-07-31 write-wall campaign —
//!   the rewrite-wall fix; contract 10, `tests/async_block_reclaim_tests.rs`):
//!   the field capture (4-node NVMe-oF/TCP, mid-rewrite at 6.3 GB/s)
//!   showed the queue pinned at its cap (17.4 GB ≈ 4,153 × 4 MiB;
//!   196,747 queued vs 589 worker batches) — the single serial worker
//!   drained ~500 blocks/s against ~1,650 blocks/s of displacement, so
//!   the at-cap inline-backpressure arm became the STEADY STATE and put
//!   the discard stream back on the write path (the exact stream the
//!   2026-07-27 campaign moved off it). The worker now fans each popped
//!   span out across parallel blocking-pool lanes: group by device, sort
//!   by offset, chunk CONSECUTIVE slices of the sorted order (adjacency
//!   runs survive — coalescing happens inside each lane), one lane per
//!   `batch_blocks` of that device's queued demand, capped at
//!   `lanes_per_dev` — width = Σ over backends of min(demand, cap), the
//!   "backends × demand" law: a shallow queue keeps today's single-lane
//!   shape, a displacement storm overlaps up to `lanes_per_dev` fabric
//!   round-trips per device. Every entry stays `processing`-reserved for
//!   its lane's whole lifetime (valve/pending/drain predicates exact),
//!   the fence check runs per lane, and per-block counting is unchanged.
//!   Engagement instrument: `block_free_reclaim_commands` (device
//!   commands; blocks ÷ commands = the live coalesce factor).
//!
//! * **Reclaim manners — the deferred-drain law** (write-wall iteration
//!   1, field-measured on the 4-node cluster; contracts 12–13): device
//!   reclaims MUST yield to foreground device I/O. The field verdict-v2
//!   fresh row ran minutes after a 128 GiB `rm` and paid −17 % to the
//!   backlog drain flooding the fabric; the width experiments proved
//!   the TARGET-side deallocate service is the drain ceiling (idle
//!   ~2,700 cmd/s at width 32, ~5,900 at width 128; under foreground
//!   write load ~1,700 cmd/s at ANY width), so drain aggressiveness
//!   under load buys nothing and steals target CPU from foreground
//!   writes. The law:
//!
//!   - **Foreground device I/O active + queue below cap ⇒ DEFER** (zero
//!     device commands — the backlog waits; queued space is bounded by
//!     the cap budget and the ENOSPC valve force-drains if allocation
//!     actually needs it).
//!   - **Queue at cap ⇒ drain regardless of foreground** (parked
//!     enqueues — see below — are foreground writers too; the cap is
//!     the deferred-space budget, and relieving it is what they wait
//!     on).
//!   - **Foreground idle ⇒ full-width drain to empty** (the fast idle
//!     catch-up: the field's 128 GiB backlog clears in ~12 s).
//!
//!   Foreground detection is DEVICE-byte movement, not op counts: the
//!   probe sums the device-plane counters (write-through/patch/staging
//!   bytes + ranged-read bytes + device-true reads + tier misses), so
//!   a 1 Hz stats poller or RAM-served reads never hold the drain
//!   deferred (they move no device bytes). Injectable for tests via
//!   [`ReclaimQueue::set_foreground_signal`].
//!
//! * **Park-don't-spill at the cap** (write-wall iteration 1; contract
//!   13): an enqueue that finds the queue at the cap **never issues
//!   device commands from the enqueue context** (the retired inline
//!   arm charged a measured ~12–22 ms synchronous fabric round-trip to
//!   the write path — and ran it ON the tpc handler lane). It PARKS
//!   (async, 5 ms ticks) until the drain relieves the cap, bounded by
//!   `SQUEEZEFS_RECLAIM_CAP_PARK_MS` (default 1000, clamp 0..=60000;
//!   `0` = never park); a bound expiry soft-overflows the entry into
//!   the queue (counted `block_free_reclaim_cap_overflow` — RAM-bounded
//!   growth, conservation preserved by the valve/unmount/idle drains).
//!   Engagement gauges: `block_free_reclaim_cap_parks` (parked
//!   enqueues) and `block_free_reclaim_cap_overflow` (bound expiries —
//!   ≈ 0 in steady state).
//!
//! Knobs (read at queue construction — i.e. per `BackendRouter`):
//! `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` (max entries per worker batch,
//! default 64, clamp 1..=1024), `SQUEEZEFS_RECLAIM_BATCH_MS`
//! (accumulation window after first wake, default 2, clamp 0..=600000 —
//! large values park the worker, used by tests),
//! `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` (the deferred-space budget,
//! default 4096; an enqueue past the cap PARKS — see park-don't-spill),
//! `SQUEEZEFS_RECLAIM_CAP_PARK_MS` (park liveness bound, default 1000),
//! and `SQUEEZEFS_RECLAIM_LANES_PER_DEV` (per-device parallel-drain
//! lane cap, default 32, clamp 1..=64 — engaged only on idle-fabric
//! catch-up drains since the manners law; the field width experiment
//! measured idle drain 2,700 → 5,900 cmd/s from 8 → 32 lanes/device,
//! and NO width sensitivity under foreground load).

use crate::block_allocator::{BlockAllocator, InflightAllocGuard};
use crate::fuse_client::METRICS;
use crate::routing::{free_reclaim_op, FreeReclaimOp};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

fn env_u64(key: &str, default: u64, lo: u64, hi: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
        .clamp(lo, hi)
}

// ---------------------------------------------------------------------------
// Idea 4 — discard elision until pressure (rewrite program P0,
// docs/design-rewrite-program.md §3; contracts in
// tests/discard_elision_tests.rs).
//
// For BdevDiscard-class backings a terminal free skips the reclaim queue
// entirely: begin_free → tier purge → debt record → finish_free — the
// offset is immediately reallocatable and ZERO device commands issue
// during foreground rows (the charter's zero-mid-row-discards gate). The
// discard becomes RAM-tracked DEBT (per-allocator `elided_debt`),
// cancelled on reuse (claim-cancels-debt) and drained at the trim venues:
// idle, the pressure watermark (debt > virgin tail — KD-4.6, no
// constants), fstrim/defrag (`BackendRouter::trim_elided`). The trim
// protocol claims each offset OUT of the free list before issuing
// (KD-4.4): an offset is never simultaneously allocatable and
// being-discarded, so a discard can never race a new owner's DMA.
//
// FilePunch-class backings (regular-file volumes) keep the queued
// reclaimer verbatim — the punch is the host-FS space return whose
// absence is a REAL ENOSPC vector on overcommitted hosts (KD-4.1).
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_DISCARD_ELISION` cell (default ON; `0` = the queued-reclaim
/// path verbatim — the A/B measurement lever AND the operational escape
/// for substrates that need eager space return). Runtime-settable for
/// tests via [`set_discard_elision`].
fn elision_cell() -> &'static AtomicBool {
    static CELL: std::sync::OnceLock<AtomicBool> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let on = crate::env_knobs::bool_knob("SQUEEZEFS_DISCARD_ELISION", true);
        AtomicBool::new(on)
    })
}

/// Whether bdev-class terminal frees elide their device discard into the
/// debt tracker (default ON).
pub fn discard_elision_enabled() -> bool {
    elision_cell().load(Ordering::Relaxed)
}

/// Set the elision lever (tests / A-B acceptance runs).
pub fn set_discard_elision(on: bool) {
    elision_cell().store(on, Ordering::Relaxed);
}

/// TEST SEAM: treat every backing class as elidable (unit harnesses are
/// file-backed and could otherwise never exercise the elision arms —
/// contracts 1–3 of `tests/discard_elision_tests.rs`). Production keeps
/// the bdev-only predicate; contract 5 pins the file-backing fence.
/// (The cell itself is private; [`set_elision_class_all`] is the seam.)
fn elision_class_all_cell() -> &'static AtomicBool {
    static CELL: std::sync::OnceLock<AtomicBool> = std::sync::OnceLock::new();
    CELL.get_or_init(|| AtomicBool::new(false))
}

/// Set the class-all test seam: treat every backing class as elidable
/// (test harnesses are file-backed; production keeps the bdev-only
/// predicate — contract 5 pins the file-backing fence).
pub fn set_elision_class_all(on: bool) {
    elision_class_all_cell().store(on, Ordering::Relaxed);
}

/// Memoized backing classification (one `stat` per device path per
/// process — never a syscall per free on the hot path).
fn backing_class(device_path: &str) -> FreeReclaimOp {
    static MEMO: std::sync::OnceLock<scc::HashMap<String, FreeReclaimOp>> =
        std::sync::OnceLock::new();
    let memo = MEMO.get_or_init(scc::HashMap::new);
    if let Some(op) = memo.read_sync(device_path, |_, v| *v) {
        return op;
    }
    use std::os::unix::fs::MetadataExt;
    let op = std::fs::metadata(device_path)
        .map(|m| free_reclaim_op(m.mode()))
        .unwrap_or(FreeReclaimOp::Skip);
    let _ = memo.insert_sync(device_path.to_string(), op);
    op
}

/// The elision predicate for one terminal free (KD-4.1): lever on AND the
/// backing is BdevDiscard-class (or the test seam forces all classes).
pub(crate) fn elide_reclaim_for(device_path: &str) -> bool {
    if !discard_elision_enabled() {
        return false;
    }
    if elision_class_all_cell().load(Ordering::Relaxed) {
        return true;
    }
    matches!(backing_class(device_path), FreeReclaimOp::BdevDiscard)
}

/// The pressure watermark (KD-4.6 — derived, no constants): elide while
/// the device's unreturned debt does not exceed its never-minted (virgin)
/// tail. While virgin ≥ debt, the substrate's thin exposure from elision
/// is bounded by what fresh-minting the same workload would have consumed
/// anyway; past it, unreturned debt is the dominant exposure and the
/// paced drain engages even under foreground.
pub fn debt_within_watermark(debt_bytes: u64, virgin_bytes: u64) -> bool {
    debt_bytes <= virgin_bytes
}

/// Counter attribution for [`issue_device_ranges`]: the queued-reclaim
/// worker and the trim venues share the issue engine but own separate
/// engagement families (`block_free_trim_*` is the trim face; the
/// per-block `block_free_{discards,file_punches}` ledger counts in BOTH —
/// device-reclaimed blocks are one population regardless of venue).
#[derive(Clone, Copy, PartialEq, Eq)]
enum IssueVenue {
    ReclaimWorker,
    Trim,
}

/// The per-mount debt drainer: owns the idle/pressure venues (the
/// fstrim/defrag venue calls `drain_debt_sync` directly via
/// `BackendRouter::trim_elided`). One detached worker per router, armed
/// lazily on the first elided free (the `ReclaimQueue::ensure_worker`
/// pattern, Weak-held). Venue law (KD-4.5):
///
/// * foreground active + debt within watermark ⇒ DEFER entirely (zero
///   device commands during the row — the charter gate);
/// * debt past the watermark ⇒ paced drain regardless of foreground
///   (one batch per pass; counted `block_free_debt_pressure_drains`);
/// * idle ⇒ drain to zero (substrate hygiene — idle target CPU is free);
/// * fenced ⇒ cease permanently (the reclaimer's fence-halt law).
pub struct DebtDrainer {
    /// Targets that ever elided: device_path → allocator.
    targets: scc::HashMap<String, Arc<BlockAllocator>>,
    notify: Arc<tokio::sync::Notify>,
    worker_armed: AtomicBool,
    /// Fence + foreground probes ride the sibling reclaim queue (same
    /// wiring sites; the drainer keeps its OWN foreground last-value so
    /// the two workers' movement observations stay independent).
    reclaim: Arc<ReclaimQueue>,
    fg_last: AtomicU64,
    /// Consecutive quiet manners ticks observed (the idle-CONFIRM
    /// counter): a single flat 50 ms tick is NOT idle — rows have
    /// sub-second lulls (fsync barriers, per-file closes) and the
    /// charter's zero-mid-row-discards gate counts them as "during the
    /// row". The idle venue engages only after the signal has been flat
    /// for the whole confirm horizon (`IDLE_CONFIRM_TICKS` × the 50 ms
    /// manners tick = 1 s — the reclaim park-bound scale).
    quiet_ticks: AtomicU64,
    batch_blocks: u64,
}

/// Idle-confirm horizon in 50 ms manners ticks (see `quiet_ticks`).
const IDLE_CONFIRM_TICKS: u64 = 20;

impl DebtDrainer {
    pub fn new(reclaim: Arc<ReclaimQueue>) -> Arc<Self> {
        let batch_blocks = reclaim.batch_blocks;
        Arc::new(Self {
            targets: scc::HashMap::new(),
            notify: Arc::new(tokio::sync::Notify::new()),
            worker_armed: AtomicBool::new(false),
            reclaim,
            fg_last: AtomicU64::new(0),
            quiet_ticks: AtomicU64::new(0),
            batch_blocks,
        })
    }

    /// Register an elided free's target and wake the venue worker.
    pub fn record(self: &Arc<Self>, allocator: &Arc<BlockAllocator>, device_path: &str) {
        if self.targets.read_sync(device_path, |_, _| ()).is_none() {
            let _ = self
                .targets
                .insert_sync(device_path.to_string(), allocator.clone());
        }
        self.ensure_worker();
        self.notify.notify_one();
    }

    /// Snapshot of the registered debt targets (trim venue).
    pub(crate) fn targets_snapshot(&self) -> Vec<(String, Arc<BlockAllocator>)> {
        let mut out = Vec::new();
        self.targets.iter_sync(|k, v| {
            out.push((k.clone(), v.clone()));
            true
        });
        out
    }

    /// One manners observation: `true` while the venue must DEFER —
    /// either the device-plane signal moved since the last tick, or it
    /// has not yet been flat for the whole idle-confirm horizon. The
    /// charter's zero-mid-row-discards gate treats a row's sub-second
    /// lulls (fsync barriers, file-close gaps) as "during the row", so a
    /// single quiet tick never counts as idle.
    fn must_defer(&self) -> bool {
        let sig = self.reclaim.foreground_value();
        let prev = self.fg_last.swap(sig, Ordering::AcqRel);
        if sig != prev {
            self.quiet_ticks.store(0, Ordering::Relaxed);
            return true;
        }
        self.quiet_ticks.fetch_add(1, Ordering::Relaxed) + 1 < IDLE_CONFIRM_TICKS
    }

    fn ensure_worker(self: &Arc<Self>) {
        if self.worker_armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.worker_armed.store(false, Ordering::Release);
            return;
        };
        let weak = Arc::downgrade(self);
        let notify = self.notify.clone();
        handle.spawn(async move {
            loop {
                notify.notified().await;
                loop {
                    let Some(d) = weak.upgrade() else { return };
                    if d.reclaim.fence_halted() {
                        return; // fenced: destructive commands cease permanently
                    }
                    let targets = d.targets_snapshot();
                    let mut outstanding = 0u64;
                    let mut drained_any = false;
                    let mut deferred_any = false;
                    let fg = d.must_defer();
                    for (device_path, allocator) in targets {
                        let debt = allocator.elided_debt_bytes_local();
                        if debt == 0 {
                            continue;
                        }
                        outstanding += debt;
                        let within = debt_within_watermark(debt, allocator.virgin_bytes());
                        if fg && within {
                            deferred_any = true;
                            continue; // the row pays zero discards (KD-4.5)
                        }
                        if fg && !within {
                            METRICS
                                .block_free_debt_pressure_drains
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        // One paced batch per pass (idle passes loop back
                        // immediately and converge to zero).
                        let a = allocator.clone();
                        let dev = device_path.clone();
                        let max = d.batch_blocks as usize;
                        let _ = tokio::task::spawn_blocking(move || {
                            drain_debt_sync(&a, &dev, max, false)
                        })
                        .await;
                        drained_any = true;
                    }
                    let park = outstanding == 0;
                    let defer_tick = deferred_any && !drained_any;
                    // No Arc across the sleep (the health-worker sentinel
                    // discipline).
                    drop(d);
                    if park {
                        break; // park on notify
                    }
                    if defer_tick {
                        // Deferred under foreground: coarse re-evaluation
                        // tick (the reclaim worker's manners loop pattern).
                        squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        });
    }
}

impl Drop for DebtDrainer {
    fn drop(&mut self) {
        // Wake a parked worker so it observes the dead Weak and exits.
        self.notify.notify_waiters();
    }
}

/// Drain up to `max` debt blocks of one target synchronously (blocking
/// ioctls — callers run this on the blocking pool). `full` trims the
/// WHOLE free list (the fstrim face: the free list is the durable truth,
/// debt is only the incremental tracker — KD-4.9). Returns
/// `(blocks, bytes)` reclaimed.
///
/// Protocol per offset (KD-4.4): claim OUT of the free list (the
/// allocation claim — losers skip: the offset was reused and owes
/// nothing), register a live in-flight owner for the claim window (fsck
/// C6 must never "complete" a mid-trim offset back onto the free list),
/// issue, RETURN to the free list. Claim windows are bounded to one
/// batch so the ENOSPC valve is never starved by a long trim.
pub(crate) fn drain_debt_sync(
    allocator: &Arc<BlockAllocator>,
    device_path: &str,
    max: usize,
    full: bool,
) -> (u64, u64) {
    let candidates: Vec<(u64, u64)> = if full {
        let chunk = allocator.chunk_size();
        allocator
            .free_block_indices()
            .into_iter()
            .map(|idx| (idx * chunk, chunk))
            .collect()
    } else {
        allocator.take_debt_batch(max)
    };
    if candidates.is_empty() {
        return (0, 0);
    }

    let mut total_blocks = 0u64;
    let mut total_bytes = 0u64;
    let batch = max.max(1);
    for window in candidates.chunks(batch) {
        // Claim phase: offsets leave the free list (and any lingering
        // debt entry dies with the claim on the full-trim face).
        let mut claimed: Vec<(u64, u64, InflightAllocGuard)> = Vec::new();
        for &(offset, bytes) in window {
            if full {
                allocator.cancel_elided_debt(offset);
            }
            if let Some(guard) = allocator.claim_free_for_trim(offset) {
                claimed.push((offset, bytes, guard));
            }
            // Lost claim: reused (or racing trim) — owes nothing.
        }
        if claimed.is_empty() {
            continue;
        }
        claimed.sort_unstable_by_key(|(o, _, _)| *o);
        let ranges: Vec<(u64, u64, u64)> = coalesce_ranges(&claimed);
        let (blocks, bytes) = issue_device_ranges(device_path, &ranges, IssueVenue::Trim);
        total_blocks += blocks;
        total_bytes += bytes;
        // Return phase: back onto the free list; guards drop here.
        for (offset, _, _) in &claimed {
            allocator.return_from_trim(*offset);
        }
    }
    (total_blocks, total_bytes)
}

/// Coalesce sorted `(offset, bytes, guard)` claims into
/// `(start, len, blocks)` ranges (the reclaim worker's adjacency rule).
fn coalesce_ranges(sorted: &[(u64, u64, InflightAllocGuard)]) -> Vec<(u64, u64, u64)> {
    let mut ranges: Vec<(u64, u64, u64)> = Vec::new();
    for (offset, bytes, _) in sorted {
        match ranges.last_mut() {
            Some((start, len, k)) if *start + *len == *offset => {
                *len += *bytes;
                *k += 1;
            }
            _ => ranges.push((*offset, *bytes, 1)),
        }
    }
    ranges
}

/// The production foreground device-activity signal (the manners law's
/// input): a monotonic sum of DEVICE-plane movement — write-through /
/// patch / staging bytes and device-read work. Deliberately NOT op
/// counts: a 1 Hz stats poller or RAM-tier-served reads move no device
/// bytes and must never hold a deferred backlog's drain hostage.
fn device_activity_signal() -> u64 {
    let m = &*METRICS;
    m.write_through_bytes
        .load(Ordering::Relaxed)
        .wrapping_add(m.patch_write_bytes.load(Ordering::Relaxed))
        .wrapping_add(m.staging_put_bytes_drain.load(Ordering::Relaxed))
        .wrapping_add(m.staging_put_bytes_flush.load(Ordering::Relaxed))
        .wrapping_add(m.staging_put_bytes_wt_fallback.load(Ordering::Relaxed))
        .wrapping_add(m.ranged_read_bytes.load(Ordering::Relaxed))
        .wrapping_add(m.read_device_true_reads.load(Ordering::Relaxed))
        .wrapping_add(m.hot_block_misses.load(Ordering::Relaxed))
}

/// One terminally-freed block whose device reclaim + `finish_free` the
/// queue now owns. `begin_free` has already retired the incarnation and
/// the read tiers are already purged; the in-flight guard keeps fsck's
/// C2/C3/C6 machinery from adjudicating the begin_free-limbo offset while
/// the reclaim is queued (a live owner shields it; a crashed owner drops
/// the guard and shields nothing).
pub struct ReclaimEntry {
    pub allocator: Arc<BlockAllocator>,
    /// Dropped (deregistered) after `finish_free` — the entry's natural
    /// drop order, fields being consumed at the end of processing.
    pub inflight: InflightAllocGuard,
    pub device_path: String,
    pub offset: u64,
    pub size: u64,
}

/// The per-router background reclaim queue. Lock-free (`SegQueue` +
/// atomics) — enqueue on the write path is push + notify, no latches, no
/// device I/O.
pub struct ReclaimQueue {
    q: crossbeam::queue::SegQueue<ReclaimEntry>,
    /// Entries currently queued (upper bound during a push window).
    len: AtomicU64,
    /// Entries popped whose reclaim has not completed — drains must wait
    /// for these too, or the ENOSPC valve could observe an empty queue
    /// while the last free blocks are in a worker's hands.
    processing: AtomicU64,
    notify: Arc<tokio::sync::Notify>,
    worker_armed: AtomicBool,
    /// The writer-guard fence probe (`tests/async_block_reclaim_tests.rs`
    /// contract 6): returns `true` when any volume of this mount's meta
    /// set has latched the D0 fail-stop `failed` state — the SAME signal
    /// the journal-barrier escalation sets (`KvMetaBackend::is_failed`,
    /// what `disabled_volumes` mirrors). Wired by
    /// `DataRouter::set_meta_backend`; bare routers (no meta set) have no
    /// probe and never halt.
    fence_signal: std::sync::OnceLock<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Sticky fence halt: once the probe fires, device reclaims cease
    /// PERMANENTLY for this queue (a fenced holder is dead until remount
    /// — `failed` never clears in-process).
    halted: AtomicBool,

    batch_blocks: u64,
    batch_ms: u64,
    max_queued: u64,
    /// Per-device parallel-drain lane cap (see module docs, demand-derived
    /// parallel drain). Width in use = Σ over devices of
    /// min(ceil(device demand / batch_blocks), this) — and 1 under
    /// foreground device I/O (the manners law).
    lanes_per_dev: u64,
    /// Park liveness bound for at-cap enqueues (ms; see park-don't-spill).
    cap_park_ms: u64,
    /// Foreground device-activity signal (a monotonic device-byte/op
    /// sum; ANY advance between worker passes = foreground active).
    /// Defaults to the METRICS device-plane sum; injectable for tests.
    fg_signal: std::sync::OnceLock<Arc<dyn Fn() -> u64 + Send + Sync>>,
    /// The signal value at the worker's previous manners decision.
    fg_last: AtomicU64,
    /// Test seam (`SQUEEZEFS_TEST_RECLAIM_STALL_MS`): stall each
    /// processed batch — deterministic slow-device schedules for the
    /// valve-liveness contract (`enospc_valve_never_blocks_executor_
    /// threads`). 0 in production.
    test_stall_ms: u64,
}

impl ReclaimQueue {
    pub fn from_env() -> Arc<Self> {
        Arc::new(Self {
            q: crossbeam::queue::SegQueue::new(),
            len: AtomicU64::new(0),
            processing: AtomicU64::new(0),
            notify: Arc::new(tokio::sync::Notify::new()),
            worker_armed: AtomicBool::new(false),
            fence_signal: std::sync::OnceLock::new(),
            halted: AtomicBool::new(false),
            // Defaults are MEASURED constants (derivation-sweep filing —
            // honest measured evidence, never derivation theater):
            // batch 64 blocks / 2 ms = the shipped coalesce shape of the
            // async-reclaim campaign (`.benchmarks/2026-07-27-async-block-
            // reclaim.md` — blocks ÷ `commands` is the live factor).
            batch_blocks: env_u64("SQUEEZEFS_RECLAIM_BATCH_BLOCKS", 64, 1, 1024),
            batch_ms: env_u64("SQUEEZEFS_RECLAIM_BATCH_MS", 2, 0, 600_000),
            // 4096 = the deferred thin-space budget the write-wall rows ran
            // (`.benchmarks/2026-07-31-write-wall.md`); the filed derivation
            // thesis is aggregate data capacity (thin-space debt, not RAM)
            // — pending its queue-cap sweep, the measured constant stands.
            max_queued: env_u64("SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS", 4096, 1, 1 << 20),
            // 32 lanes = the write-wall width experiment's verdict (idle
            // drain 2,700 → 5,900 cmd/s from 8 → 32 lanes/device; NO width
            // sensitivity under foreground load — module doc, ibid.).
            lanes_per_dev: env_u64("SQUEEZEFS_RECLAIM_LANES_PER_DEV", 32, 1, 64),
            // 1 s park bound = the park-don't-spill liveness horizon
            // (write-wall iteration 1 — time horizon, not a resource cap).
            cap_park_ms: env_u64("SQUEEZEFS_RECLAIM_CAP_PARK_MS", 1000, 0, 60_000),
            fg_signal: std::sync::OnceLock::new(),
            fg_last: AtomicU64::new(0),
            test_stall_ms: env_u64("SQUEEZEFS_TEST_RECLAIM_STALL_MS", 0, 0, 600_000),
        })
    }

    /// Wire the writer-guard fence probe (see the field doc). Set once at
    /// meta-backend wiring; later calls are no-ops.
    pub fn set_fence_signal(&self, sig: Arc<dyn Fn() -> bool + Send + Sync>) {
        let _ = self.fence_signal.set(sig);
    }

    /// `true` ⇔ device reclaims are (now) fence-halted. Evaluated once
    /// per batch and by the ENOSPC valve — one atomic load when already
    /// halted, one cheap probe (a few atomic loads over the meta set)
    /// otherwise. Latches sticky and loud on the first observation.
    pub(crate) fn fence_halted(&self) -> bool {
        if self.halted.load(Ordering::Acquire) {
            return true;
        }
        let Some(sig) = self.fence_signal.get() else {
            return false;
        };
        if sig() {
            if !self.halted.swap(true, Ordering::AcqRel) {
                log::error!(
                    "block-reclaim: writer guard fenced / volume fail-stopped — ceasing \
                     ALL device reclaims permanently (a fenced zombie's discard can land \
                     on offsets the successor writer has reallocated); queued entries are \
                     dropped WITHOUT finish_free — the successor's recovery owns the \
                     accounting (un-returned thin space, re-covered on reuse; counted in \
                     block_free_reclaim_fence_halts)"
                );
            }
            return true;
        }
        false
    }

    /// Latch the sticky halt the writer-guard fence probe sets, directly:
    /// device reclaims cease PERMANENTLY for this queue — queued and
    /// future entries drop without `finish_free` (counted
    /// `block_free_reclaim_fence_halts`; successor recovery owns the
    /// accounting, the kill-9 crash posture). The in-process process-death
    /// analog for custody holders that end without a clean drain: a
    /// holder's straggler punch outliving its claims can land on offsets
    /// the SUCCESSOR writer has reallocated (the same hazard class the
    /// fence probe closes for fenced zombies). Production fencing rides
    /// the wired fence probe; this verb is the teardown/crash-analog arm
    /// (`BackendRouter::reclaim_cease`).
    pub fn halt_device_reclaims(&self) {
        self.halted.store(true, Ordering::Release);
    }

    /// Queue one terminal free's reclaim. Past the deferred-space cap the
    /// caller PARKS (bounded) until the drain relieves the cap — it
    /// **never issues device commands from the enqueue context** (the
    /// retired inline arm charged a measured 12–22 ms synchronous fabric
    /// round-trip to the write path, on the tpc handler lane; write-wall
    /// iteration 1). A bound expiry soft-overflows into the queue
    /// (RAM-bounded growth; conservation preserved by the valve /
    /// unmount / idle drains). `block_free_reclaim_queued` counts every
    /// entry — "entered the reclaim engine" — so the field ledger's
    /// `queued ≡ displaced blocks` identity holds in all arms.
    pub async fn enqueue(self: &Arc<Self>, entry: ReclaimEntry) {
        // DLM S5 (spec §6.8 item 1 / §6.3's "Reclaim and discard"
        // obligation): "offsets stay non-reallocatable until reclaimed" is
        // a PER-PROCESS invariant about SHARED hardware. A reader's
        // reclaimer would issue `BLKDISCARD` / `PUNCH_HOLE` against ranges
        // the live writer owns — the one hazard in the whole reader
        // surface that destroys data rather than merely reading it stale.
        // Nothing is ever queued on a read-only mount, and the queue is
        // latched halted at arm (`ro_coherence::arm_reader_data_plane`) so
        // a straggler entry from a racing teardown cannot issue either.
        //
        // DLM S9: a CO-WRITER is refused here for the same physical
        // reason and a different authority reason — the offsets it would
        // deallocate belong to the AUTHORITY's ledger (terminal frees ride
        // the shipped publish, and the authority's own reclaimer discards
        // them), so a co-writer issuing `BLKDISCARD` would destroy blocks
        // whose liveness only the authority can answer.
        //
        // Deliberately BEFORE the `queued` counter: the ledger identity is
        // "queued ≡ displaced blocks", and neither posture displaces any.
        if crate::fuse_client::read_only_mount() {
            log::error!(
                "{}",
                crate::fuse_client::read_only_refusal("device reclaim (discard/punch)")
            );
            self.halt_device_reclaims();
            drop(entry);
            return;
        }
        // DLM S9 free path: the scope probe marks the shipped-free
        // EXECUTOR — the authority's own ladder enqueuing here for a
        // validated peer request (`crate::cowriter::
        // with_authority_accounting`). A co-writer's OWN paths still land
        // in this refusal, and still latch the halt.
        if crate::fuse_client::co_writer_mount()
            && !crate::cowriter::authority_accounting_scope_active()
        {
            log::error!(
                "{}",
                crate::fuse_client::co_writer_refusal("device reclaim (discard/punch)")
            );
            self.halt_device_reclaims();
            drop(entry);
            return;
        }
        METRICS
            .block_free_reclaim_queued
            .fetch_add(1, Ordering::Relaxed);
        if self.len.load(Ordering::Acquire) >= self.max_queued && !self.fence_halted() {
            METRICS
                .block_free_reclaim_cap_parks
                .fetch_add(1, Ordering::Relaxed);
            // Make sure a drain is actually running to relieve us (the
            // manners law drains at-cap queues regardless of foreground).
            self.ensure_worker();
            self.notify.notify_one();
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(self.cap_park_ms);
            loop {
                if self.len.load(Ordering::Acquire) < self.max_queued || self.fence_halted() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    METRICS
                        .block_free_reclaim_cap_overflow
                        .fetch_add(1, Ordering::Relaxed);
                    break;
                }
                // 5 ms tick — the admit-park liveness pattern.
                squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        METRICS
            .block_free_reclaim_queue_bytes
            .fetch_add(entry.size, Ordering::Relaxed);
        // len before push: `len` is an upper bound, so a concurrent pop
        // can never underflow it.
        self.len.fetch_add(1, Ordering::AcqRel);
        self.q.push(entry);
        self.ensure_worker();
        self.notify.notify_one();
    }

    /// Inject the foreground device-activity signal (tests). Set once;
    /// later calls are no-ops. Production uses the METRICS device-plane
    /// sum (see `device_activity_signal`).
    pub fn set_foreground_signal(&self, sig: Arc<dyn Fn() -> u64 + Send + Sync>) {
        let _ = self.fg_signal.set(sig);
    }

    /// The raw foreground-signal value (injected or the METRICS
    /// device-plane sum) — shared with the [`DebtDrainer`], which keeps
    /// its OWN last-value so the two workers' movement observations stay
    /// independent.
    pub(crate) fn foreground_value(&self) -> u64 {
        match self.fg_signal.get() {
            Some(f) => f(),
            None => device_activity_signal(),
        }
    }

    /// `true` ⇔ foreground device I/O moved since the worker's previous
    /// manners decision (the deferred-drain law's input). One counter
    /// sum + one swap per worker pass — never on the enqueue path.
    fn foreground_active(&self) -> bool {
        let sig = self.foreground_value();
        let prev = self.fg_last.swap(sig, Ordering::AcqRel);
        sig != prev
    }

    /// Spawn the background worker once (lazily — the first enqueue runs
    /// inside a tokio context; if it somehow does not, the arm is retried
    /// and drains/valve still guarantee progress).
    fn ensure_worker(self: &Arc<Self>) {
        if self.worker_armed.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.worker_armed.store(false, Ordering::Release);
            return;
        };
        // Weak, deliberately (the health-worker sentinel discipline): a
        // strong Arc would keep the queue — and this loop — alive forever
        // after the router dropped. `Drop` notifies so a parked worker
        // wakes, fails its upgrade, and exits.
        let weak = Arc::downgrade(self);
        let notify = self.notify.clone();
        handle.spawn(async move {
            loop {
                notify.notified().await;
                let batch_ms = match weak.upgrade() {
                    Some(q) => q.batch_ms,
                    None => return,
                };
                // Accumulation window: displaced blocks of one overwrite
                // stream arrive back-to-back — waiting a beat lets the
                // batch coalesce adjacent ranges into fewer device
                // commands. (Arc NOT held across the sleep.)
                if batch_ms > 0 {
                    squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(batch_ms))
                        .await;
                }
                loop {
                    let Some(q) = weak.upgrade() else { return };
                    // The manners law (write-wall iteration 1): defer
                    // while foreground device I/O is moving and the
                    // queue sits below the deferred-space cap — the
                    // width experiments proved drain aggressiveness
                    // under load buys no drain rate and steals target
                    // CPU from foreground writes. At cap, drain
                    // regardless (parked enqueues are foreground
                    // writers too); idle, full-width catch-up.
                    let at_cap = q.len.load(Ordering::Acquire) >= q.max_queued;
                    if q.foreground_active() && !at_cap {
                        if q.len.load(Ordering::Acquire) == 0 {
                            break; // nothing deferred — park on notify
                        }
                        // Deferred: re-evaluate on a coarse tick (drop
                        // the Arc across the sleep — the health-worker
                        // sentinel discipline).
                        drop(q);
                        squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                    // Demand-derived take: up to one full fan-out's
                    // worth per pass (batch_blocks × lanes_per_dev) — a
                    // deep queue fills every lane, a shallow one stays a
                    // single batch, and the bigger sorted span coalesces
                    // MORE adjacency than per-batch windows could.
                    let take = (q.batch_blocks * q.lanes_per_dev) as usize;
                    let batch = q.take_batch(take);
                    if batch.is_empty() {
                        break;
                    }
                    q.dispatch_lanes(batch).await;
                }
            }
        });
    }

    /// Pop up to `max` entries, reserving them in `processing` BEFORE the
    /// pop so a concurrent drain never observes empty+idle while entries
    /// sit in a processor's hands.
    fn take_batch(&self, max: usize) -> Vec<ReclaimEntry> {
        let mut out = Vec::new();
        while out.len() < max {
            self.processing.fetch_add(1, Ordering::AcqRel);
            match self.q.pop() {
                Some(e) => {
                    self.len.fetch_sub(1, Ordering::AcqRel);
                    out.push(e);
                }
                None => {
                    self.processing.fetch_sub(1, Ordering::AcqRel);
                    break;
                }
            }
        }
        out
    }

    /// Fan one popped span out across demand-derived parallel lanes (the
    /// 2026-07-31 write-wall campaign's rewrite-wall fix; contract 10):
    /// group by device, sort by offset, chunk CONSECUTIVE slices of the
    /// sorted order (adjacency runs survive into each lane's coalescer),
    /// one blocking-pool lane per `batch_blocks` of that device's queued
    /// demand, capped at `lanes_per_dev` — width = Σ over backends of
    /// min(demand, cap), never a fixed funnel. Awaits every lane before
    /// returning (the worker never outruns its own submissions); entries
    /// stay `processing`-reserved throughout, so the valve/pending/drain
    /// predicates observe them exactly as before. Synchronous ioctls stay
    /// on the blocking pool, never a tokio worker (module doc, io_uring
    /// posture); the fence check runs per lane inside `process_entries`.
    async fn dispatch_lanes(self: &Arc<Self>, entries: Vec<ReclaimEntry>) {
        let mut by_dev: BTreeMap<String, Vec<ReclaimEntry>> = BTreeMap::new();
        for e in entries {
            by_dev.entry(e.device_path.clone()).or_default().push(e);
        }
        let mut lanes = Vec::new();
        for (_dev, mut group) in by_dev {
            group.sort_by_key(|e| e.offset);
            let n_lanes = group
                .len()
                .div_ceil(self.batch_blocks.max(1) as usize)
                .clamp(1, self.lanes_per_dev as usize);
            let chunk = group.len().div_ceil(n_lanes);
            let mut it = group.into_iter();
            loop {
                let c: Vec<ReclaimEntry> = it.by_ref().take(chunk).collect();
                if c.is_empty() {
                    break;
                }
                METRICS
                    .block_free_reclaim_batches
                    .fetch_add(1, Ordering::Relaxed);
                let q = self.clone();
                lanes.push(tokio::task::spawn_blocking(move || {
                    q.process_entries(c);
                }));
            }
        }
        for lane in lanes {
            if lane.await.is_err() {
                // Panic in a lane: the batch guard already reconciled
                // the counters; entries were dropped (guards
                // deregistered) — fsck C6 heals any begin_free limbo.
                // Loud, never silent.
                log::error!("block-reclaim lane panicked; see fsck C6");
            }
        }
    }

    /// Synchronously drain the queue to empty AND wait out in-flight
    /// batches. Callers: the ENOSPC pressure valve (which counts
    /// `block_free_reclaim_sync_drains` ONLY when the returned processed
    /// count is nonzero — contract 8: an empty-queue pass is a no-op, not
    /// engagement), unmount teardown, and tests via
    /// `BackendRouter::reclaim_drain`. Blocking by design — run it on the
    /// blocking pool from async contexts unless you ARE the emergency
    /// path (the valve blocks its worker briefly; ENOSPC is rarer and
    /// worse). Returns the number of entries THIS call processed.
    pub fn drain_sync(&self) -> u64 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut processed = 0u64;
        loop {
            let batch = self.take_batch(self.batch_blocks as usize);
            if !batch.is_empty() {
                processed += batch.len() as u64;
                self.process_entries(batch);
                continue;
            }
            if self.processing.load(Ordering::Acquire) == 0 && self.len.load(Ordering::Acquire) == 0
            {
                return processed;
            }
            if std::time::Instant::now() > deadline {
                log::error!(
                    "block-reclaim drain timed out waiting for in-flight batches \
                     (processing={}, queued={}) — proceeding; allocation may \
                     refuse StorageFull honestly",
                    self.processing.load(Ordering::Relaxed),
                    self.len.load(Ordering::Relaxed)
                );
                return processed;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Off-thread drain — the ENOSPC valve's async body (probe-up
    /// campaign, 2026-07-29: the valve's write-funnel fix). The
    /// allocating task AWAITS here (honest backpressure on exactly the
    /// task that needs the space); the device reclaim work — batch
    /// ioctls plus [`Self::drain_sync`]'s 1 ms in-flight waits — runs on
    /// the blocking pool, so an engagement can never freeze an executor
    /// thread (the fuse3 tpc lanes are current-thread runtimes: the old
    /// inline drain froze a whole lane and every handler/upload future
    /// on it — the measured cross-device lockstep starvation). Racing
    /// engagements each run their own pass CONCURRENTLY — width derives
    /// from allocation demand, never a fixed funnel (the field law): the
    /// first single-flight shape of this fix measured −15 % on the
    /// 4-wide storm bracket because the old inline valve, for all its
    /// lane-freezing, accidentally parallelized reclaim across every
    /// engaged thread; `take_batch`'s processing reservation already
    /// makes concurrent passes sound (each entry is popped exactly
    /// once). Returns entries processed by THIS engagement.
    pub async fn drain_off_thread(self: &Arc<Self>) -> u64 {
        let q = self.clone();
        match tokio::task::spawn_blocking(move || q.drain_sync()).await {
            Ok(n) => n,
            Err(e) => {
                log::error!("valve drain blocking task panicked: {e:?}");
                0
            }
        }
    }

    /// Reclaimable supply exists (queued or in a processor's hands) —
    /// the allocator's supply-wait predicate. A fence-halted queue
    /// reports `false`: nothing will ever be reclaimed by THIS daemon
    /// (contract 6), so allocation escalates straight to the honest
    /// refusal path instead of parking forever.
    pub fn pending(&self) -> bool {
        !self.fence_halted()
            && (self.len.load(Ordering::Acquire) > 0 || self.processing.load(Ordering::Acquire) > 0)
    }

    /// Gauge accessor (tests / stats).
    pub fn queued_len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// Process owned entries: group by device, coalesce adjacent ranges,
    /// issue the reclaim, then `finish_free` each offset. Counters stay
    /// PER-BLOCK (`block_free_discards ≡ displaced blocks` — the field
    /// ledger) even when ranges merge into one device command.
    ///
    /// Panic-safe accounting: the guard reconciles `processing` and the
    /// byte gauge for every entry, processed or unwound.
    fn process_entries(&self, entries: Vec<ReclaimEntry>) {
        if self.test_stall_ms > 0 {
            // Deterministic slow-device seam (test-only; see field doc).
            std::thread::sleep(std::time::Duration::from_millis(self.test_stall_ms));
        }
        struct BatchGuard<'a> {
            q: &'a ReclaimQueue,
            remaining: usize,
            bytes_remaining: u64,
        }
        impl Drop for BatchGuard<'_> {
            fn drop(&mut self) {
                self.q
                    .processing
                    .fetch_sub(self.remaining as u64, Ordering::AcqRel);
                METRICS
                    .block_free_reclaim_queue_bytes
                    .fetch_sub(self.bytes_remaining, Ordering::Relaxed);
            }
        }
        let mut guard = BatchGuard {
            q: self,
            remaining: entries.len(),
            bytes_remaining: entries.iter().map(|e| e.size).sum(),
        };

        // Fence check ONCE per batch, before any device command (contract
        // 6): a fenced holder must never issue destructive device I/O —
        // on detection-grade (non-PR) substrates a zombie's discard can
        // destroy the successor writer's reallocated blocks. Halted
        // entries drop here WITHOUT finish_free (the successor's journal
        // replay/recovery walk owns the accounting — the same posture as
        // the kill-9 crash story: un-returned thin space, re-covered on
        // reuse); the batch guard reconciles `processing` + the byte
        // gauge, and the entries' in-flight registrations deregister on
        // drop.
        if self.fence_halted() {
            METRICS
                .block_free_reclaim_fence_halts
                .fetch_add(entries.len() as u64, Ordering::Relaxed);
            return;
        }

        let mut by_dev: BTreeMap<String, Vec<ReclaimEntry>> = BTreeMap::new();
        for e in entries {
            by_dev.entry(e.device_path.clone()).or_default().push(e);
        }
        for (device_path, mut group) in by_dev {
            group.sort_by_key(|e| e.offset);
            reclaim_device_group(&device_path, &group);
            for e in group {
                // finish_free AFTER the reclaim — the offset becomes
                // reallocatable only now (the pre-existing free-window
                // law, unchanged; a new owner's DMA can never race the
                // discard). The in-flight guard drops with the entry.
                e.allocator.finish_free(e.offset);
                guard.remaining -= 1;
                guard.bytes_remaining -= e.size;
                self.processing.fetch_sub(1, Ordering::AcqRel);
                METRICS
                    .block_free_reclaim_queue_bytes
                    .fetch_sub(e.size, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for ReclaimQueue {
    fn drop(&mut self) {
        // Wake a parked worker so it can observe the dead Weak and exit
        // (no leaked tasks). Entries still queued here drop with the
        // queue: process-exit shape — see the module crash posture.
        self.notify.notify_waiters();
    }
}

/// Issue the device reclaim for one device's sorted entry group,
/// coalescing adjacent `[offset, offset+size)` ranges into single device
/// commands. Per-block counting (see `process_entries`).
///
/// The engine is the shim-write-amplification fix verbatim
/// (`.benchmarks/2026-07-27-shim-write-amplification.md`): `PUNCH_HOLE`
/// on regular-file backings (host-FS sparse reclaim), `BLKDISCARD` on
/// block devices (NVMe DSM Deallocate — no data payload, never
/// write-bandwidth-accounted; the former unconditional PUNCH_HOLE was
/// `blkdev_issue_zeroout` there). Refused/unsupported reclaims are
/// SKIPPED and counted — never degraded into a zeroing write; freed
/// ranges are never read (hole semantics + write-before-publish + the
/// incarnation seqlock).
#[cfg(target_os = "linux")]
fn reclaim_device_group(device_path: &str, group: &[ReclaimEntry]) {
    // Coalesce adjacent ranges: (start, len, blocks_covered).
    let mut ranges: Vec<(u64, u64, u64)> = Vec::new();
    for e in group {
        match ranges.last_mut() {
            Some((start, len, k)) if *start + *len == e.offset => {
                *len += e.size;
                *k += 1;
            }
            _ => ranges.push((e.offset, e.size, 1)),
        }
    }
    issue_device_ranges(device_path, &ranges, IssueVenue::ReclaimWorker);
}

#[cfg(not(target_os = "linux"))]
fn reclaim_device_group(_device_path: &str, group: &[ReclaimEntry]) {
    METRICS
        .block_free_reclaim_skipped
        .fetch_add(group.len() as u64, Ordering::Relaxed);
}

/// The shared device-reclaim issue engine (the shim-write-amplification
/// classification, verbatim): `PUNCH_HOLE` on regular-file backings,
/// `BLKDISCARD` on block devices, refused/unsupported SKIPPED-and-counted
/// — never a zeroing write. Counts the per-block device-reclaim ledger
/// (`block_free_{discards,file_punches}` + bytes + the per-COMMAND
/// economy counter) for every venue; the Trim venue additionally counts
/// its own engagement family (`block_free_trim_{discards,bytes}`).
/// Returns `(ok_blocks, ok_bytes)`.
#[cfg(target_os = "linux")]
fn issue_device_ranges(
    device_path: &str,
    ranges: &[(u64, u64, u64)],
    venue: IssueVenue,
) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;

    let total_blocks: u64 = ranges.iter().map(|(_, _, k)| *k).sum();
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(device_path) else {
        METRICS
            .block_free_reclaim_skipped
            .fetch_add(total_blocks, Ordering::Relaxed);
        return (0, 0);
    };
    let mode = file.metadata().map(|m| m.mode()).unwrap_or(0);
    let fd = file.as_raw_fd();
    let op = free_reclaim_op(mode);
    if matches!(op, FreeReclaimOp::Skip) {
        METRICS
            .block_free_reclaim_skipped
            .fetch_add(total_blocks, Ordering::Relaxed);
        return (0, 0);
    }

    let mut ok_blocks = 0u64;
    let mut ok_bytes = 0u64;
    for &(start, len, k) in ranges {
        // Per-COMMAND economy counter (contract 11): blocks ÷ commands is
        // the live coalesce factor; the per-block ledger stays on the
        // punch/discard/skip counters below.
        METRICS
            .block_free_reclaim_commands
            .fetch_add(1, Ordering::Relaxed);
        let issued = match op {
            FreeReclaimOp::FilePunch => {
                let r = unsafe {
                    libc::fallocate(
                        fd,
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        start as libc::off_t,
                        len as libc::off_t,
                    )
                };
                if r == 0 {
                    METRICS
                        .block_free_file_punches
                        .fetch_add(k, Ordering::Relaxed);
                    METRICS
                        .block_free_punch_bytes
                        .fetch_add(len, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }
            FreeReclaimOp::BdevDiscard => {
                // BLKDISCARD = _IO(0x12, 119): REQ_OP_DISCARD (NVMe DSM
                // Deallocate). Not in the libc crate's const table.
                const BLKDISCARD: libc::c_ulong = 0x1277;
                let range: [u64; 2] = [start, len];
                let r = unsafe { libc::ioctl(fd, BLKDISCARD as _, range.as_ptr()) };
                if r == 0 {
                    METRICS.block_free_discards.fetch_add(k, Ordering::Relaxed);
                    METRICS
                        .block_free_discard_bytes
                        .fetch_add(len, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }
            FreeReclaimOp::Skip => unreachable!("filtered above"),
        };
        if issued {
            ok_blocks += k;
            ok_bytes += len;
            if venue == IssueVenue::Trim {
                METRICS
                    .block_free_trim_discards
                    .fetch_add(k, Ordering::Relaxed);
                METRICS
                    .block_free_trim_bytes
                    .fetch_add(len, Ordering::Relaxed);
            }
        } else {
            // Unsupported/refused reclaim: skip loud-once in the counter,
            // NEVER a zeroing-write fallback.
            METRICS
                .block_free_reclaim_skipped
                .fetch_add(k, Ordering::Relaxed);
        }
    }
    (ok_blocks, ok_bytes)
}

#[cfg(not(target_os = "linux"))]
fn issue_device_ranges(
    _device_path: &str,
    ranges: &[(u64, u64, u64)],
    _venue: IssueVenue,
) -> (u64, u64) {
    let total_blocks: u64 = ranges.iter().map(|(_, _, k)| *k).sum();
    METRICS
        .block_free_reclaim_skipped
        .fetch_add(total_blocks, Ordering::Relaxed);
    (0, 0)
}
