//! OQ-1 repro-port (`.benchmarks/2026-07-27-oq1-overwrite-op-economy.md`):
//! `fuse_ops` stats-row exactness contract.
//!
//! The "overwrite issues 2× the FUSE ops" finding
//! (`.benchmarks/2026-07-27-async-block-reclaim.md` OQ-1) was an
//! instrument artifact: `fuse_ops` was a `ProbabilisticAtomic` whose
//! per-thread 128-increment batches only flushed at thread death — but
//! tokio handler lanes live for the daemon's lifetime, so with 32 lanes
//! up to ~4k counts sat invisible indefinitely, and every stats-row
//! delta carried a ±(lanes × 127) quantization band in 128-op quanta
//! (3968 = 31 × 128 vs 7936 = 62 × 128 on identical kernel-side request
//! streams; a FRESH-write row read 7936 once the residue phase shifted).
//! Kernel-side `fuse_request_send` hists showed fresh and overwrite
//! rows within 0.2 % of each other.
//!
//! Contract: increments to `METRICS.fuse_ops` are visible in `load()`
//! immediately — including from threads that are still alive — so
//! before/after stats snapshots around a workload row account for every
//! op exactly.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier};

use squeezefs::fuse_client::METRICS;

/// A single increment from the calling thread is visible at once (the
/// smallest stats-row: one op, one snapshot delta). The retired
/// probabilistic counter held it in a thread-local batch and reported 0.
#[test]
fn fuse_ops_single_increment_is_immediately_visible() {
    let before = METRICS.fuse_ops.load(Ordering::Relaxed);
    METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
    let after = METRICS.fuse_ops.load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        1,
        "one fuse_ops increment must move load() by exactly 1 \
         (probabilistic batching regressed the stats-row contract)"
    );
}

/// Handler-lane shape: N live threads each record fewer ops than any
/// batching threshold, then park WITHOUT exiting (daemon worker threads
/// never die between stats reads). The observer must still see every
/// count. This is the exact mechanism that manufactured the OQ-1
/// "overwrite 2×" ghost — long-lived lanes holding sub-threshold
/// residue across row snapshots.
#[test]
fn fuse_ops_exact_across_live_parked_threads() {
    const THREADS: usize = 16;
    const OPS_PER_THREAD: u64 = 100; // below the retired 128 batch threshold

    let before = METRICS.fuse_ops.load(Ordering::Relaxed);

    // done: all threads finished counting; release: allow them to exit.
    let done = Arc::new(Barrier::new(THREADS + 1));
    let release = Arc::new(Barrier::new(THREADS + 1));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let done = Arc::clone(&done);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                for _ in 0..OPS_PER_THREAD {
                    METRICS.fuse_ops.fetch_add(1, Ordering::Relaxed);
                }
                done.wait();
                // Park alive until the observer has read the counter —
                // a thread-death flush must not be what makes it right.
                release.wait();
            })
        })
        .collect();

    done.wait();
    let after = METRICS.fuse_ops.load(Ordering::Relaxed);
    release.wait();
    for h in handles {
        h.join().expect("counter thread panicked");
    }

    assert_eq!(
        after - before,
        THREADS as u64 * OPS_PER_THREAD,
        "fuse_ops must account for every op from live (unexited) handler \
         threads; residue held in thread-local batches is the OQ-1 \
         stats-row quantization artifact"
    );
}
