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
//!   that finds runnable work counts [`tick_rescues`] — ≈0 healthy
//!   (a benign push-vs-timeout race is possible), growth = a notify
//!   delivery bug, loudly.
//! * **A panicking task never kills the lane** (`catch_unwind` per
//!   poll): the task completes-by-panic, [`task_panics`] grows (the
//!   detached-task-panics discipline), the lane serves on.

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

struct ExecShared {
    queue: Mutex<VecDeque<Arc<LaneTask>>>,
    ready: Condvar,
    shutdown: AtomicBool,
}

impl ExecShared {
    fn enqueue(&self, task: Arc<LaneTask>) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.push_back(task);
        drop(q);
        self.ready.notify_one();
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
        LaneExec {
            shared: Arc::new(ExecShared {
                queue: Mutex::new(VecDeque::new()),
                ready: Condvar::new(),
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
        let task = Arc::new(LaneTask {
            state: TaskState::new_scheduled(),
            future: Mutex::new(Some(Box::pin(fut))),
            exec: self.shared.clone(),
        });
        self.shared.enqueue(task);
    }

    /// Ask the run loop to exit once observed (tests / teardown; lane
    /// threads are process-lifetime in the daemon).
    pub fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.ready.notify_one();
    }

    /// The lane thread body: pop → poll → route the outcome through the
    /// loom-verified state word. Runs until [`Self::shutdown`].
    pub fn run(&self) {
        let shared = &self.shared;
        loop {
            let task = {
                let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if shared.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(t) = q.pop_front() {
                        break t;
                    }
                    let (guard, timeout) = shared
                        .ready
                        .wait_timeout(q, TICK)
                        .unwrap_or_else(|e| e.into_inner());
                    q = guard;
                    if timeout.timed_out() && !q.is_empty() {
                        // The tick backstop engaged: work was waiting
                        // and no notify delivered it. ≈0 healthy.
                        TICK_RESCUES.fetch_add(1, Ordering::Relaxed);
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
        }
    }

    /// Queue depth (diagnostics).
    pub fn queue_len(&self) -> usize {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
