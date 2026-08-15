//! First-party blocking-work offload + `block_on` (the rip-tokio-total
//! program, 2026-08-13) — replaces `tokio::task::spawn_blocking` and
//! `Runtime::block_on` in the daemon.
//!
//! * [`block_on`] — the thread-park executor: the waker unparks the
//!   calling OS thread, the loop re-polls. No driver: timers ride the
//!   `sqz-timer` thread, wakes ride whoever holds the waker. Spurious
//!   unparks are absorbed by the re-poll loop.
//! * [`run_blocking`] — a cached OS-thread pool (`sqz-blk{N}`) for
//!   syscall-class work awaited from async context. Panic in the job
//!   propagates to the AWAITER (resume_unwind — the `.await.expect`
//!   call-site shape keeps its meaning). Pool sizing derives from the
//!   core count (no free constants): cap = `cpus × 16` (blocking work
//!   is I/O-parked, not CPU-bound — the oversubscription mirrors the
//!   class), floor 8 (physical minimum useful parallel blocking
//!   capacity on any host) — where `cpus` is the fleet-share-DIVIDED
//!   sizing root the daemon feeds ([`set_sizing_parallelism`], KD-MW-14
//!   rung 3c). Idle threads exit after [`IDLE_REAP`].

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Idle blocking threads park this long, then exit (cached-thread
/// hygiene; respawn on demand is one thread::spawn).
const IDLE_REAP: Duration = Duration::from_secs(10);

/// The CPU count the pool SIZES with, fed by the embedding daemon.
///
/// KD-MW-14 rung 3c: this crate cannot depend on the root crate, so the
/// daemon feeds the fleet-share-DIVIDED sizing root
/// (`crate::cpu::process_parallelism()`) here at startup — the sizing.rs
/// pattern (pure forms fed a `cpus` the caller resolves), adapted for a
/// process-global lazy pool. First set wins (the pool caches its cap at
/// first use anyway); unfed embeddings (unit tests, foreign hosts) fall
/// back to the raw mask below.
static SIZING_CPUS: OnceLock<usize> = OnceLock::new();

/// Feed the sizing CPU root (the daemon calls this with the DIVIDED
/// `process_parallelism()` before any blocking offload). Idempotent:
/// the first value wins.
pub fn set_sizing_parallelism(cpus: usize) {
    let _ = SIZING_CPUS.set(cpus.max(1));
}

/// Pure pool-cap form (tie-tested by the root suite's derivation sweep):
/// `cpus × 16` — blocking work is I/O-parked, not CPU-bound, so the
/// admitted width oversubscribes the (divided) core root ×16; floor 8 =
/// the minimum useful parallel blocking capacity on any host.
pub fn pool_cap_from(cpus: usize) -> usize {
    cpus.saturating_mul(16).max(8)
}

/// The sizing CPU count: the daemon-fed divided root, or — for unfed
/// embeddings only — the calling thread's mask (the daemon never relies
/// on this arm, which is what keeps the Hang-1 pinned-first-toucher
/// class and the fleet-share bypass out of the shipped binary).
fn sizing_parallelism() -> usize {
    SIZING_CPUS.get().copied().unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

/// Run a future to completion on the calling OS thread (the bootstrap /
/// lane-edge executor). The waker unparks this thread; the loop
/// re-polls.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    use std::task::Wake;
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut fut = std::pin::pin!(fut);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Cooperative yield: wake-then-Pending once (the `tokio::task::
/// yield_now` shape, executor-agnostic — the waker is the whole
/// contract).
pub async fn yield_now() {
    struct YieldNow(bool);
    impl Future for YieldNow {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                return Poll::Ready(());
            }
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
    YieldNow(false).await
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct PoolState {
    queue: VecDeque<Job>,
    idle: usize,
    total: usize,
    seq: u64,
}

struct Pool {
    state: Mutex<PoolState>,
    ready: Condvar,
    cap: usize,
}

fn pool() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| {
        Pool {
            state: Mutex::new(PoolState {
                queue: VecDeque::new(),
                idle: 0,
                total: 0,
                seq: 0,
            }),
            ready: Condvar::new(),
            // Derived from the daemon-fed DIVIDED sizing root (KD-MW-14
            // rung 3c) via the pure form above.
            cap: pool_cap_from(sizing_parallelism()),
        }
    })
}

fn submit(job: Job) {
    let p = pool();
    let spawn_name = {
        let mut st = p.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.push_back(job);
        // Spawn whenever queued demand exceeds idle coverage (tokio's
        // per-un-covered-job posture — the displacement-storm lesson:
        // notifying one idle worker for a BURST double-books it, the
        // worker drains serially, and demand-derived parallel lanes
        // collapse to one). At cap or covered: wake an idle worker;
        // in-flight workers absorb the rest.
        if st.queue.len() > st.idle && st.total < p.cap {
            st.total += 1;
            st.seq += 1;
            Some(format!("sqz-blk{}", st.seq))
        } else {
            drop(st);
            p.ready.notify_one();
            None
        }
    };
    if let Some(name) = spawn_name {
        std::thread::Builder::new()
            .name(name)
            .spawn(worker_loop)
            .expect("sqz-blk thread spawns");
    }
}

fn worker_loop() {
    let p = pool();
    let mut st = p.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(job) = st.queue.pop_front() {
            drop(st);
            job();
            st = p.state.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        st.idle += 1;
        let (guard, timeout) = p
            .ready
            .wait_timeout(st, IDLE_REAP)
            .unwrap_or_else(|e| e.into_inner());
        st = guard;
        st.idle -= 1;
        if st.queue.is_empty() && timeout.timed_out() {
            st.total -= 1;
            return;
        }
    }
}

/// Offload `f` to the blocking pool and await its result. A panicking
/// job re-panics in the AWAITER (join-and-unwrap semantics).
pub fn run_blocking<F, R>(f: F) -> RunBlocking<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = crate::sqz_channel::oneshot::channel();
    submit(Box::new(move || {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // A dead awaiter is fine — the result is dropped.
        let _ = tx.send(out);
    }));
    RunBlocking { rx }
}

type PanicPayload = Box<dyn std::any::Any + Send + 'static>;

pub struct RunBlocking<R> {
    rx: crate::sqz_channel::oneshot::Receiver<Result<R, PanicPayload>>,
}

impl<R> Future for RunBlocking<R> {
    type Output = R;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(Ok(v))) => Poll::Ready(v),
            Poll::Ready(Ok(Err(payload))) => std::panic::resume_unwind(payload),
            Poll::Ready(Err(_)) => unreachable!(
                "the job always sends (catch_unwind) and the sender \
                 outlives submission"
            ),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn block_on_drives_timers_and_channels() {
        let t0 = std::time::Instant::now();
        block_on(crate::sqz_time::sleep(Duration::from_millis(40)));
        assert!(t0.elapsed() >= Duration::from_millis(35));
    }

    #[test]
    fn run_blocking_returns_and_reuses_threads() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut futs = Vec::new();
        for i in 0..32u64 {
            let hits = hits.clone();
            futs.push(run_blocking(move || {
                hits.fetch_add(1, Ordering::Relaxed);
                i * 2
            }));
        }
        let mut sum = 0;
        for f in futs {
            sum += block_on(f);
        }
        assert_eq!(sum, (0..32u64).map(|i| i * 2).sum::<u64>());
        assert_eq!(hits.load(Ordering::Relaxed), 32);
    }

    /// Burst width (the displacement-storm cap graze, 2026-08-13): a
    /// burst of K blocking jobs must run in PARALLEL even when an idle
    /// worker exists — the submit that merely notifies the one idle
    /// worker double-books it (the worker drains the queue SERIALLY),
    /// and the reclaim drain's demand-derived lanes collapse to one.
    /// tokio's pool spawns per un-covered job; so must ours.
    #[test]
    fn burst_submits_run_in_parallel_past_one_idle_worker() {
        // Warm exactly one idle worker.
        block_on(run_blocking(|| {}));
        std::thread::sleep(Duration::from_millis(20));
        // Burst 4 × 100 ms blocking jobs; serial = 400 ms.
        let t0 = std::time::Instant::now();
        let futs: Vec<_> = (0..4)
            .map(|_| run_blocking(|| std::thread::sleep(Duration::from_millis(100))))
            .collect();
        for f in futs {
            block_on(f);
        }
        let wall = t0.elapsed();
        assert!(
            wall < Duration::from_millis(250),
            "a 4-job burst must parallelize (wall {wall:?}; ~400 ms = the \
             serialized single-worker shape)"
        );
    }

    #[test]
    fn run_blocking_panic_propagates_to_awaiter() {
        let res = std::panic::catch_unwind(|| {
            block_on(run_blocking(|| panic!("job panicked")));
        });
        assert!(res.is_err(), "panic must reach the awaiter");
    }
}
