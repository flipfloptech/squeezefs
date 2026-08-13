//! Admission control for opportunistic background tasks (P1-5).
//!
//! Critical path work (FUSE write_striped block I/O, writeback flush, DLM drivers)
//! uses dedicated semaphores or is fully awaited. This module gates *best-effort*
//! work that must not exhaust the runtime under load:
//! prefetch and similar fire-and-forget jobs.
//!
//! When the pool is full, tasks are **dropped** (not queued) so memory stays bounded.
//!
//! Striped block concurrency (P2-6) is cores-based via
//! [`striped_block_concurrency`], overridable at runtime.

use once_cell::sync::Lazy;
use squeezefs_ipc::sqz_semaphore::Semaphore;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Shared permit pool sized from host parallelism.
///
/// PROCESS parallelism, not `available_parallelism()`: this Lazy is first
/// touched from a core-pinned runtime worker, whose 1-CPU affinity mask
/// would collapse every cores-based pool to its floor (the Hang-1 sizing
/// poison — see `crate::cpu`).
pub static BG_TASK_SEM: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let cores = crate::cpu::process_parallelism();
    // Generous enough for sequential read prefetch bursts; hard-capped vs. unbounded spawn.
    Arc::new(Semaphore::new(std::cmp::max(32, cores * 8)))
});

/// Override for [`striped_block_concurrency`]. `0` means auto (cores-based).
static STRIPED_BLOCK_CONCURRENCY_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Default striped **read** fan-out when auto policy is not used by call sites
/// that still want a fixed historical constant (tests / docs).
pub const STRIPED_READ_CONCURRENCY: usize = 16;

/// Cores-based concurrency for striped block I/O (reads and writes).
///
/// Policy: `clamp(cores * 2, 4, 64)` where `cores` is the PROCESS
/// parallelism (`crate::cpu`) — callers are routinely core-pinned runtime
/// workers whose own affinity mask is 1 CPU (the Hang-1 sizing poison
/// reported `striped_block_concurrency == 4` on a 16-core mount). Override
/// with [`set_striped_block_concurrency`] (`0` restores auto).
#[inline]
pub fn striped_block_concurrency() -> usize {
    let over = STRIPED_BLOCK_CONCURRENCY_OVERRIDE.load(Ordering::Relaxed);
    if over > 0 {
        return over;
    }
    crate::cpu::process_parallelism()
        .saturating_mul(2)
        .clamp(4, 64)
}

/// Set striped block concurrency. Pass `0` to restore the auto (cores-based) policy.
pub fn set_striped_block_concurrency(n: usize) {
    STRIPED_BLOCK_CONCURRENCY_OVERRIDE.store(n, Ordering::Relaxed);
}

/// Dedicated permit pool for on-path striped block I/O (reads/range fills).
/// Separate from [`BG_TASK_SEM`] so best-effort work cannot starve critical reads.
/// Sized from [`striped_block_concurrency`] at first use (override before first
/// access if tests need a fixed size).
pub static STRIPED_IO_SEM: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(striped_block_concurrency())));

/// Spawn a best-effort background task. If no permit is available immediately,
/// the work is skipped (logged at debug) rather than queued unboundedly.
/// Returns whether the task was ADMITTED (`false` = shed, the future was
/// dropped un-run). Callers that account issue-side state before spawning
/// (the §5.5 prefetch pipeline) must roll it back on `false` — a shed
/// task never reaches its own settle path. Best-effort callers may ignore
/// the verdict.
pub fn spawn_bg<F>(fut: F) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    let sem = BG_TASK_SEM.clone();
    match sem.clone().try_acquire_owned() {
        Ok(permit) => {
            crate::fuse_client::METRICS
                .bg_spawn_admitted
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let task = async move {
                let _permit = permit;
                fut.await;
            };
            // Venue: the caller's runtime when there is one; otherwise
            // the fuse3 per-core handler lanes. The read-saturation
            // campaign's ring-side pipeline feed spawns prefetch tasks
            // from the IPC service threads — foreign OS threads with no
            // tokio context, where `tokio::spawn` panics. The TPC lanes
            // are the established foreign-thread venue (the 2026-07-26
            // handoff-economy rule: they are where kernel-lane handlers
            // run, and their per-core current-thread runtimes take
            // foreign submissions without the global-inject-queue tax).
            // RES-8: contained + counted at both venues — an admitted
            // background task that panics silently drops its permit's
            // worth of work (prefetch fills, tier publishes) with no
            // record.
            // rip-tokio-total: ONE venue — the fuse3 TPC handler lanes
            // (throughput-class prefetch/tier-publish fan-out; the
            // 2-lane sqz-meta pool would funnel it).
            crate::detached::tpc_spawn_guarded("bg_admit", task);
            true
        }
        Err(_) => {
            crate::fuse_client::METRICS
                .bg_spawn_rejected
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            log::debug!("bg_admit: rejected background task (admission full)");
            false
        }
    }
}

/// Current available permits (for tests / metrics).
pub fn available_permits() -> usize {
    BG_TASK_SEM.available_permits()
}

/// Total configured capacity of the background pool.
pub fn capacity() -> usize {
    std::cmp::max(32, crate::cpu::process_parallelism() * 8)
}
