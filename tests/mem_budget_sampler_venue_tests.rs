//! RES-11 (pre-RC engineering spec §7): the R5 sampler tick must not run
//! on a tokio worker.
//!
//! `MEM_BUDGET.tick()` reads procfs (`/proc/self/statm`, the cgroup
//! memory files), calls every registered component's gauge closure, and
//! — on a Red tick — runs `arena.4096.purge`, a **full jemalloc
//! all-arena purge**. That is tens of milliseconds of uninterruptible,
//! syscall-heavy work, once per second, executed on a runtime worker
//! *precisely when the daemon is under memory pressure* and every FUSE
//! handler on that worker is already the thing being measured.
//!
//! Contract: the tick's work happens off the async workers, so a runtime
//! whose workers are all otherwise idle keeps making progress across a
//! long tick.
//!
//! The test uses a SINGLE-worker runtime, which makes the property
//! binary rather than statistical: if the tick body runs on the worker,
//! the concurrently-spawned probe cannot possibly run until it finishes.
//!
//! RED against dev 7d1ec2e1: `tick_off_thread` does not exist and
//! `spawn_sampler` calls `MEM_BUDGET.tick()` inline.

use squeezefs::mem_budget::{Component, MEM_BUDGET};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// A gauge that blocks for `SLOW_MS` while armed — standing in for the
/// procfs reads plus the all-arena purge.
const SLOW_MS: u64 = 400;
static ARMED: AtomicBool = AtomicBool::new(false);

#[test]
fn sampler_tick_does_not_occupy_an_async_worker() {
    MEM_BUDGET.register(Component::new(
        "res11_slow_probe",
        0,
        0,
        Arc::new(|| {
            if ARMED.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(SLOW_MS));
            }
            0
        }),
        Arc::new(|_| {}),
    ));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("single-worker runtime");

    // Completion-order ticket: "who finished first" as a total order,
    // never a timeout.
    let seq = Arc::new(AtomicU64::new(0));
    rt.block_on(async move {
        ARMED.store(true, Ordering::SeqCst);
        // Spawned FIRST, so on a single worker a tick that runs INLINE
        // necessarily completes before the probe gets the worker at all.
        let s = seq.clone();
        let tick = tokio::spawn(async move {
            squeezefs::mem_budget::tick_off_thread().await;
            s.fetch_add(1, Ordering::SeqCst)
        });
        let s = seq.clone();
        let probe = tokio::spawn(async move {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            s.fetch_add(1, Ordering::SeqCst)
        });
        let probe_at = probe.await.expect("probe task");
        let tick_at = tick.await.expect("tick task");
        ARMED.store(false, Ordering::SeqCst);
        assert!(
            probe_at < tick_at,
            "RES-11: the R5 sampler tick occupied the async worker for its \
             whole ~{SLOW_MS} ms of procfs + jemalloc-purge work (probe \
             ticket {probe_at} vs tick ticket {tick_at}) — once per second, \
             precisely under memory pressure"
        );
    });
}
