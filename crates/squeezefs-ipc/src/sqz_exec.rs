//! The sqz-exec single-thread task executor (docs/design-sqz-sync.md,
//! Stage 1b): first-party poll delivery for the handler-lane venue.
//!
//! The Stage-1 field attribution (2026-08-12) proved the OQ-5 wedge is a
//! task lost inside tokio's delivery — woken but never re-polled, timer
//! wakes no-oping like I/O wakes. This executor makes that class
//! unrepresentable by OWNERSHIP: the wake→queue→poll path is this file
//! plus [`crate::exec_core`]'s loom-verified state word — there is no
//! foreign scheduler to lose a task in.
//!
//! Design (KISS, the svc-thread pattern):
//! * One [`LaneExec`] per lane OS thread; one `Mutex<VecDeque>` ready
//!   queue + condvar. Spawn and wake from ANY thread push under the
//!   mutex and notify — nanosecond critical sections, no async inside.
//! * The waker is `Arc<LaneTask>` via `std::task::Wake`: `wake()` runs
//!   the exec_core state word and, on winning the enqueue, pushes +
//!   notifies. A wake landing mid-poll flips NOTIFIED and the run loop
//!   re-enqueues after the poll — the loom-modeled never-lost law.
//! * **The park is time-bounded** ([`TICK`]): even against a bug in our
//!   own notify path, the lane re-checks its queue every tick. A tick
//!   that finds runnable work counts `tick_rescues` — ≈0 healthy
//!   (a benign push-vs-timeout race is possible), growth = a notify
//!   delivery bug, loudly.
//! * **A panicking task never kills the lane** (`catch_unwind` per
//!   poll): the task completes-by-panic, `task_panics` grows (the
//!   detached-task-panics discipline), the lane serves on.
//! * **The park is pluggable** ([`LanePark`], e2e perf audit C-2): a lane
//!   that owns an I/O ring parks IN the ring (`io_uring_enter` with a
//!   wake-eventfd SQE armed) instead of on a condvar, so the ring's
//!   completions and the lane's task wakes arrive through ONE wait and
//!   a completion never crosses a thread to reach the task awaiting it.
//!   The default parker is the condvar. The lost-wake-freedom argument
//!   is the queue mutex: the lane marks itself parked UNDER the lock in
//!   the same section that found the queue empty, and every enqueue
//!   reads that mark UNDER the lock after its push — a push the lane's
//!   check missed sees the mark and unparks (parkers are sticky: an
//!   unpark before the park returns it immediately).

use crate::exec_core::TaskState;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// The park bound — the same 2 s liveness posture as `sqz_sync::TICK`.
pub const TICK: Duration = Duration::from_secs(2);

/// How a lane thread waits for work (see the module doc). `park` runs on
/// the lane thread only, with the ready-queue mutex NOT held; `unpark`
/// runs on any thread and must be STICKY (an unpark delivered before the
/// matching park makes that park return at once); `service` runs on the
/// lane thread once per loop iteration — the owner's non-blocking hook
/// (a ring parker reaps its completions here, so a completion posted
/// while the lane was busy is delivered without a park).
pub trait LanePark: Send + Sync {
    /// Block until unparked or `tick` elapses; `true` = the tick fired.
    fn park(&self, tick: Duration) -> bool;
    /// Wake a parked lane.
    fn unpark(&self);
    /// Per-iteration owner hook (never blocks).
    fn service(&self) {}
}

/// The default parker: a sticky flag under a mutex + condvar.
#[derive(Default)]
pub struct CondvarPark {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl LanePark for CondvarPark {
    fn park(&self, tick: Duration) -> bool {
        let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        let mut timed_out = false;
        while !*g && !timed_out {
            let (guard, t) = self
                .cv
                .wait_timeout(g, tick)
                .unwrap_or_else(|e| e.into_inner());
            g = guard;
            timed_out = t.timed_out();
        }
        let woke = std::mem::replace(&mut *g, false);
        timed_out && !woke
    }

    fn unpark(&self) {
        *self.flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_one();
    }
}

/// Ticks that found runnable work waiting (a notify that never landed).
/// ≈0 healthy; growth is a delivery bug surfacing loudly instead of a
/// wedge.
pub static TICK_RESCUES: AtomicU64 = AtomicU64::new(0);

/// Tasks that panicked mid-poll (completed-by-panic; the lane survives).
pub static TASK_PANICS: AtomicU64 = AtomicU64::new(0);

type LaneFut = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct LaneTask {
    state: TaskState,
    /// The future, present until completion. Only the run loop touches
    /// it (single-consumer by construction: begin_poll's single-winner
    /// CAS admits one poller), but the waker's `Arc` keeps the TASK
    /// alive from foreign threads, so the slot needs the mutex.
    future: Mutex<Option<LaneFut>>,
    exec: Arc<ExecShared>,
}

// SAFETY-free: every field is Send + Sync by composition (TaskState is
// atomics, the future slot is a Mutex over a Send future, ExecShared is
// Sync).

impl std::task::Wake for LaneTask {
    fn wake(self: Arc<Self>) {
        if self.state.wake() {
            let exec = self.exec.clone();
            exec.enqueue(self);
        }
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if self.state.wake() {
            self.exec.enqueue(self.clone());
        }
    }
}

/// The ready queue plus the lane's parked mark — ONE mutex, so the
/// mark's visibility to an enqueuer needs no fence argument.
struct Ready {
    queue: VecDeque<Arc<LaneTask>>,
    /// Set by the lane thread in the lock section that found the queue
    /// empty; cleared by it after the park returns.
    parked: bool,
}

struct ExecShared {
    ready: Mutex<Ready>,
    park: Arc<dyn LanePark>,
    shutdown: AtomicBool,
}

impl ExecShared {
    fn enqueue(&self, task: Arc<LaneTask>) {
        let mut r = self.ready.lock().unwrap_or_else(|e| e.into_inner());
        r.queue.push_back(task);
        let parked = r.parked;
        drop(r);
        if parked {
            self.park.unpark();
        }
    }
}

/// A handle to one lane executor: `spawn` from any thread; the lane's
/// own OS thread runs [`LaneExec::run`].
#[derive(Clone)]
pub struct LaneExec {
    shared: Arc<ExecShared>,
}

impl Default for LaneExec {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneExec {
    pub fn new() -> Self {
        Self::with_park(Arc::new(CondvarPark::default()))
    }

    /// A lane whose idle wait is `park` (see [`LanePark`]).
    pub fn with_park(park: Arc<dyn LanePark>) -> Self {
        LaneExec {
            shared: Arc::new(ExecShared {
                ready: Mutex::new(Ready {
                    queue: VecDeque::new(),
                    parked: false,
                }),
                park,
                shutdown: AtomicBool::new(false),
            }),
        }
    }

    /// Spawn a task onto this lane (any thread). Spawn IS the first
    /// wake: the task starts SCHEDULED and is enqueued immediately.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_boxed(Box::pin(fut));
    }

    /// [`Self::spawn`] for an already-boxed future (a caller that minted
    /// the box itself — the fuse3 READ fast-dispatch's demote arm — pays
    /// no second box here).
    pub fn spawn_boxed(&self, fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        let task = Arc::new(LaneTask {
            state: TaskState::new_scheduled(),
            future: Mutex::new(Some(fut)),
            exec: self.shared.clone(),
        });
        self.shared.enqueue(task);
    }

    /// Ask the run loop to exit once observed (tests / teardown; the
    /// process-wide lanes are process-lifetime in the daemon, the
    /// per-volume journal lanes live with their volume).
    pub fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.park.unpark();
    }

    /// The lane thread body: pop → poll → route the outcome through the
    /// loom-verified state word. Runs until [`Self::shutdown`].
    pub fn run(&self) {
        let shared = &self.shared;
        loop {
            if shared.shutdown.load(Ordering::Acquire) {
                return;
            }
            let task = {
                let mut r = shared.ready.lock().unwrap_or_else(|e| e.into_inner());
                match r.queue.pop_front() {
                    Some(t) => t,
                    None => {
                        // Mark parked in the SAME lock section that found
                        // the queue empty (module doc: the lost-wake
                        // argument is this mutex).
                        r.parked = true;
                        drop(r);
                        let timed_out = shared.park.park(TICK);
                        let mut r = shared.ready.lock().unwrap_or_else(|e| e.into_inner());
                        r.parked = false;
                        if timed_out && !r.queue.is_empty() {
                            // The tick backstop engaged: work was waiting
                            // and no unpark delivered it. ≈0 healthy.
                            TICK_RESCUES.fetch_add(1, Ordering::Relaxed);
                        }
                        drop(r);
                        shared.park.service();
                        continue;
                    }
                }
            };

            if !task.state.begin_poll() {
                // Stale entry (completed-by-panic path) — skip.
                continue;
            }
            let waker = Waker::from(task.clone());
            let mut cx = Context::from_waker(&waker);
            let poll_result = {
                let mut slot = task.future.lock().unwrap_or_else(|e| e.into_inner());
                let Some(fut) = slot.as_mut() else {
                    // Future already dropped (defensive; begin_poll
                    // should have refused a completed task).
                    task.state.end_poll_complete();
                    continue;
                };
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fut.as_mut().poll(&mut cx)
                })) {
                    Ok(p) => p,
                    Err(_) => {
                        // Completed-by-panic: drop the future, count
                        // loudly, keep the lane serving (RES-8).
                        TASK_PANICS.fetch_add(1, Ordering::Relaxed);
                        *slot = None;
                        task.state.end_poll_complete();
                        continue;
                    }
                }
            };
            match poll_result {
                Poll::Ready(()) => {
                    let mut slot = task.future.lock().unwrap_or_else(|e| e.into_inner());
                    *slot = None;
                    task.state.end_poll_complete();
                }
                Poll::Pending => {
                    if task.state.end_poll_pending() {
                        // A wake landed mid-poll: the runner owns the
                        // reschedule (exec_core law 1).
                        shared.enqueue(task);
                    }
                }
            }
            shared.park.service();
        }
    }

    /// Queue depth (diagnostics).
    pub fn queue_len(&self) -> usize {
        self.shared
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn lane() -> (LaneExec, std::thread::JoinHandle<()>) {
        let ex = LaneExec::new();
        let ex2 = ex.clone();
        let jh = std::thread::spawn(move || ex2.run());
        (ex, jh)
    }

    /// Happy path: a spawned task runs to completion.
    #[test]
    fn spawn_runs_to_completion() {
        let (ex, jh) = lane();
        let (tx, rx) = mpsc::channel();
        ex.spawn(async move {
            tx.send(42u32).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 42);
        ex.shutdown();
        jh.join().unwrap();
    }

    /// A future parked Pending and woken from a FOREIGN OS thread (the
    /// NVMe-worker completion shape) is re-polled and completes.
    #[test]
    fn foreign_thread_wake_repolls() {
        struct Gate {
            open: Mutex<(bool, Option<Waker>)>,
        }
        impl Future for &Gate {
            type Output = ();
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                let mut g = self.open.lock().unwrap();
                if g.0 {
                    Poll::Ready(())
                } else {
                    g.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
        static GATE: std::sync::OnceLock<Gate> = std::sync::OnceLock::new();
        let gate = GATE.get_or_init(|| Gate {
            open: Mutex::new((false, None)),
        });

        let (ex, jh) = lane();
        let (tx, rx) = mpsc::channel();
        ex.spawn(async move {
            gate.await;
            tx.send(()).unwrap();
        });
        // Let the first poll park.
        std::thread::sleep(Duration::from_millis(50));
        // Foreign OS thread completes the "I/O" and wakes.
        std::thread::spawn(move || {
            let w = {
                let mut g = gate.open.lock().unwrap();
                g.0 = true;
                g.1.take()
            };
            if let Some(w) = w {
                w.wake();
            }
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("foreign wake must re-poll the task");
        ex.shutdown();
        jh.join().unwrap();
    }

    /// A wake DURING the poll (self-wake before Pending) re-polls — the
    /// exec_core NOTIFIED law end to end.
    #[test]
    fn mid_poll_wake_repolls() {
        struct SelfWake {
            polled: std::sync::atomic::AtomicU32,
        }
        impl Future for &SelfWake {
            type Output = ();
            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.polled.fetch_add(1, Ordering::SeqCst) == 0 {
                    cx.waker().wake_by_ref(); // mid-poll wake
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }
        }
        static SW: std::sync::OnceLock<SelfWake> = std::sync::OnceLock::new();
        let sw = SW.get_or_init(|| SelfWake {
            polled: std::sync::atomic::AtomicU32::new(0),
        });
        let (ex, jh) = lane();
        let (tx, rx) = mpsc::channel();
        ex.spawn(async move {
            sw.await;
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("mid-poll wake must reschedule");
        assert_eq!(sw.polled.load(Ordering::SeqCst), 2);
        ex.shutdown();
        jh.join().unwrap();
    }

    /// RES-8 for lanes: a panicking task completes-by-panic loudly and
    /// the lane keeps serving.
    #[test]
    fn panicking_task_never_kills_the_lane() {
        let (ex, jh) = lane();
        let before = TASK_PANICS.load(Ordering::Relaxed);
        ex.spawn(async {
            panic!("task panic (deliberate — this test)");
        });
        let (tx, rx) = mpsc::channel();
        ex.spawn(async move {
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("lane must survive a task panic");
        assert!(TASK_PANICS.load(Ordering::Relaxed) > before);
        ex.shutdown();
        jh.join().unwrap();
    }

    /// A recording parker: sticky like the contract demands, and it
    /// counts parks/unparks so the tests can read the protocol.
    #[derive(Default)]
    struct CountingPark {
        inner: CondvarPark,
        parks: AtomicU64,
        unparks: AtomicU64,
        services: AtomicU64,
    }

    impl LanePark for CountingPark {
        fn park(&self, tick: Duration) -> bool {
            self.parks.fetch_add(1, Ordering::SeqCst);
            self.inner.park(tick)
        }
        fn unpark(&self) {
            self.unparks.fetch_add(1, Ordering::SeqCst);
            self.inner.unpark();
        }
        fn service(&self) {
            self.services.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A pluggable parker sees the lane's idle waits and the wakes that
    /// end them: a spawn onto a PARKED lane unparks it exactly once, a
    /// spawn from the lane's own poll (or a mid-poll self-wake) never
    /// does (the lane is running, `parked` is clear), and the per-
    /// iteration hook runs at least once per poll and once per park.
    #[test]
    fn pluggable_park_is_unparked_only_while_parked() {
        let park = Arc::new(CountingPark::default());
        let ex = LaneExec::with_park(park.clone());
        let ex2 = ex.clone();
        let jh = std::thread::spawn(move || ex2.run());
        // Let the lane reach its first park.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while park.parks.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            park.parks.load(Ordering::SeqCst),
            1,
            "lane parked once idle"
        );
        assert_eq!(park.unparks.load(Ordering::SeqCst), 0);

        // A foreign spawn onto the parked lane: exactly one unpark.
        let (tx, rx) = mpsc::channel();
        let ex3 = ex.clone();
        ex.spawn(async move {
            // Spawned FROM the lane while it runs: no unpark.
            ex3.spawn(async move {
                tx.send(()).unwrap();
            });
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            park.unparks.load(Ordering::SeqCst),
            1,
            "one foreign spawn onto a parked lane = one unpark; the lane-local spawn adds none"
        );
        assert!(
            park.services.load(Ordering::SeqCst) >= 3,
            "the hook ran after the park and after each of the two polls"
        );
        ex.shutdown();
        jh.join().unwrap();
        assert!(
            park.unparks.load(Ordering::SeqCst) >= 2,
            "shutdown unparks the lane"
        );
    }

    /// The sticky law: an unpark delivered BEFORE the park makes that
    /// park return at once (the eventfd-counter shape the ring parker
    /// relies on), and a park with no unpark honours the tick.
    #[test]
    fn condvar_park_is_sticky_and_ticks() {
        let p = CondvarPark::default();
        p.unpark();
        let t0 = std::time::Instant::now();
        assert!(
            !p.park(Duration::from_secs(5)),
            "a pre-delivered unpark returns the park"
        );
        assert!(t0.elapsed() < Duration::from_secs(1));
        let t0 = std::time::Instant::now();
        assert!(
            p.park(Duration::from_millis(20)),
            "no unpark: the tick fires"
        );
        assert!(t0.elapsed() >= Duration::from_millis(20));
    }

    /// Lost-wake freedom under a spawn storm from foreign threads: every
    /// spawned task completes in well under one TICK (a push the lane's
    /// empty-check missed that did NOT unpark it would cost a whole tick
    /// before the backstop rescued it).
    #[test]
    fn pluggable_park_never_loses_a_wake() {
        let park = Arc::new(CountingPark::default());
        let ex = LaneExec::with_park(park.clone());
        let ex2 = ex.clone();
        let jh = std::thread::spawn(move || ex2.run());
        let t0 = std::time::Instant::now();
        const N: usize = 2000;
        let (tx, rx) = mpsc::channel();
        let spawners: Vec<_> = (0..4)
            .map(|s| {
                let ex = ex.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for i in 0..N / 4 {
                        let tx = tx.clone();
                        ex.spawn(async move {
                            tx.send(s * 1000 + i).unwrap();
                        });
                        // Give the lane a chance to park between spawns.
                        if i % 7 == 0 {
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        drop(tx);
        let mut got = 0;
        while got < N {
            rx.recv_timeout(Duration::from_secs(10))
                .expect("every spawned task completes (no lost wake)");
            got += 1;
        }
        for s in spawners {
            s.join().unwrap();
        }
        assert!(
            t0.elapsed() < TICK / 2,
            "{N} spawns took {:?}: a lost wake waited for the tick backstop",
            t0.elapsed()
        );
        ex.shutdown();
        jh.join().unwrap();
    }

    /// Contention: many tasks ping-ponging foreign wakes all complete
    /// (multi-thread stress on the queue + state word).
    #[test]
    fn stress_many_tasks_many_foreign_wakes() {
        let (ex, jh) = lane();
        let (tx, rx) = mpsc::channel();
        const N: usize = 200;
        for i in 0..N {
            let tx = tx.clone();
            // Yield-once future: first poll wakes itself from a foreign
            // thread after a beat, second completes.
            struct Once {
                done: bool,
            }
            impl Future for Once {
                type Output = ();
                fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                    if self.done {
                        Poll::Ready(())
                    } else {
                        self.done = true;
                        let w = cx.waker().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(1));
                            w.wake();
                        });
                        Poll::Pending
                    }
                }
            }
            ex.spawn(async move {
                Once { done: false }.await;
                tx.send(i).unwrap();
            });
        }
        let mut got = std::collections::HashSet::new();
        for _ in 0..N {
            got.insert(
                rx.recv_timeout(Duration::from_secs(10))
                    .expect("all complete"),
            );
        }
        assert_eq!(got.len(), N);
        ex.shutdown();
        jh.join().unwrap();
    }
}
