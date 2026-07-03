//! Admission control for opportunistic background tasks (P1-5).
//!
//! Critical path work (FUSE write_striped block I/O, writeback flush, DLM drivers)
//! uses dedicated semaphores or is fully awaited. This module gates *best-effort*
//! work that must not exhaust the runtime under load:
//! prefetch, DHT/peer publish, and similar fire-and-forget jobs.
//!
//! When the pool is full, tasks are **dropped** (not queued) so memory stays bounded.
//!
//! Striped block concurrency (P2-6) is cores-based via
//! [`striped_block_concurrency`], overridable at runtime.

use once_cell::sync::Lazy;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Shared permit pool sized from host parallelism.
pub static BG_TASK_SEM: Lazy<Arc<Semaphore>> = Lazy::new(|| {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // Generous enough for sequential read prefetch bursts; hard-capped vs. unbounded spawn.
    Arc::new(Semaphore::new(std::cmp::max(32, cores * 8)))
});

/// Override for [`striped_block_concurrency`]. `0` means auto (cores-based).
static STRIPED_BLOCK_CONCURRENCY_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Default striped **read** fan-out when auto policy is not used by call sites
/// that still want a fixed historical constant (tests / docs).
pub const STRIPED_READ_CONCURRENCY: usize = 16;

/// Max concurrent block tasks inside a single prefetch job.
pub const PREFETCH_BLOCK_CONCURRENCY: usize = 8;

/// Cores-based concurrency for striped block I/O (reads and writes).
///
/// Policy: `clamp(cores * 2, 4, 64)`. Override with
/// [`set_striped_block_concurrency`] (`0` restores auto).
#[inline]
pub fn striped_block_concurrency() -> usize {
    let over = STRIPED_BLOCK_CONCURRENCY_OVERRIDE.load(Ordering::Relaxed);
    if over > 0 {
        return over;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cores.saturating_mul(2).clamp(4, 64)
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
pub fn spawn_bg<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let sem = BG_TASK_SEM.clone();
    match sem.clone().try_acquire_owned() {
        Ok(permit) => {
            tokio::spawn(async move {
                let _permit = permit;
                fut.await;
            });
        }
        Err(_) => {
            log::debug!("bg_admit: rejected background task (admission full)");
        }
    }
}

/// Current available permits (for tests / metrics).
pub fn available_permits() -> usize {
    BG_TASK_SEM.available_permits()
}

/// Total configured capacity of the background pool.
pub fn capacity() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    std::cmp::max(32, cores * 8)
}
