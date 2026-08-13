//! First-party owned task set (the rip-tokio-total program) — the
//! `tokio::task::JoinSet` + `abort_all`-on-drop shape the daemon's
//! MEMORY-SAFETY-BEARING owners need (MEM-2 / RES-9: "cancellation
//! aborts the set so no detached writer outlives the request"), with
//! GENUINE cancel-at-next-poll-boundary semantics and no executor
//! dependency. A plain stop latch is NOT equivalent: a latch is only
//! observed where the loop chooses to look, while an aborted tokio task
//! never resumes past its current await point AND its future (captures
//! included) is dropped. This module reproduces exactly that:
//!
//! * [`OwnedSet::spawn`] wraps every task in a [`CancelGate`] sharing
//!   the set's cancel state. The gate's poll checks the cancel flag
//!   FIRST: once cancelled, the inner future is never polled again —
//!   the task cannot resume past its current await point — and the
//!   inner future is taken and DROPPED inline (its captures die: the
//!   MEM-2 requirement that a cancelled writer's borrow of the
//!   destination ends), then the gate resolves.
//! * [`OwnedSet::cancel_all`] sets the flag and wakes nothing: a task
//!   mid-poll finishes that poll (the same non-instant window tokio's
//!   `abort` had); a parked task resolves at its next poll — bounded by
//!   the sqz TICK backstop every sqz primitive parks under.
//! * [`OwnedSet::quiesce`] / [`OwnedSet::quiesce_blocking`] wait until
//!   every spawned gate has RESOLVED (`live == 0`, `live = spawned −
//!   resolved`) — which covers queued-but-never-polled tasks too (a
//!   cancelled gate resolves on its first poll, and a gate dropped
//!   unpolled resolves in its own `Drop`).
//! * Dropping the set cancels and then runs a TICK-bounded blocking
//!   quiesce (a set dropped from sync context must not hang forever);
//!   a timeout is reported LOUDLY. Drops from async contexts should
//!   prefer `cancel_all()` + `quiesce().await` — the blocking wait
//!   parks the dropping OS thread, and if that thread is also a
//!   spawning lane the stragglers cannot resolve until it unblocks.
//!
//! The spawning venue is caller-provided ([`Spawner`], a plain fn
//! pointer): the root crate passes `crate::meta_exec::spawn_meta`,
//! fuse3 could pass its handler lanes. The spawner is expected to
//! CONTAIN panics (`spawn_meta` does — the RES-8 discipline): a panic
//! unwinding out of a gate's poll still closes the accounting (the
//! run-count guard and the gate's drop-resolve), so quiesce never
//! wedges on a panicked task.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::sqz_notify::Notify;

/// A boxed `()`-output task — the shape every spawning venue accepts.
pub type BoxTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// The caller-provided spawning venue: `(site, task)`. Must run the
/// task to completion (poll-driven) and contain panics.
pub type Spawner = fn(&'static str, BoxTask);

/// Shared cancel/accounting state — one per set, co-owned by every
/// gate (a straggler resolving after the set dropped still closes its
/// accounting against live state).
struct CancelState {
    cancelled: AtomicBool,
    /// Gates handed to the spawner.
    spawned: AtomicU64,
    /// Gates that reached a terminal state (inner completed, cancelled
    /// at a poll boundary, or dropped unpolled). `live = spawned −
    /// resolved` is the quiesce condition.
    resolved: AtomicU64,
    /// Gates currently INSIDE an inner poll (the mid-poll window —
    /// tokio's abort had the same one). Diagnostic for the drop
    /// timeout report; the decrement wakes quiesce waiters so a
    /// finishing poll is observed promptly.
    running: AtomicU64,
    quiesce: Notify,
    /// Parked gates' wakers, by gate id. THE abort-delivery vehicle:
    /// "resolve at the next poll" is vacuous for a task parked on a
    /// wake that will never fire (the cancelled-sibling shape), so
    /// [`CancelState::cancel`] drains this map and WAKES every parked
    /// gate — cancel itself delivers the poll, exactly as tokio's
    /// abort does. A gate registers under this lock BEFORE its
    /// cancelled re-check (enable-then-recheck), so a cancel racing
    /// the park is never lost.
    parked: Mutex<HashMap<u64, Waker>>,
    next_gate_id: AtomicU64,
}

impl CancelState {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let wakers: Vec<Waker> = {
            let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
            parked.drain().map(|(_, w)| w).collect()
        };
        for w in wakers {
            w.wake();
        }
    }

    fn live(&self) -> u64 {
        // `resolved` can lag a concurrent spawn's `spawned` increment;
        // the saturation only matters to racy observers (quiesce is
        // called after spawning stopped).
        self.spawned
            .load(Ordering::SeqCst)
            .saturating_sub(self.resolved.load(Ordering::SeqCst))
    }

    fn resolve(&self) {
        self.resolved.fetch_add(1, Ordering::SeqCst);
        self.quiesce.notify_waiters();
    }

    /// Enable-then-recheck park until `live == 0`. Registration happens
    /// BEFORE the recheck (under the same Notify lock every `resolve`
    /// wakes through), so a resolve landing between the two is never
    /// lost; the ticked backstop absorbs anything residual (the
    /// sqz-sync law: a lost wake costs one TICK, never a wedge).
    async fn quiesce(&self) {
        loop {
            let mut notified = self.quiesce.notified_raw();
            let _ = notified.enable();
            if self.live() == 0 {
                return;
            }
            crate::sqz_channel::ticked(notified).await;
        }
    }

    /// Blocking quiesce with a deadline; `true` = quiesced, `false` =
    /// timed out with tasks still live.
    fn quiesce_blocking(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        crate::sqz_blocking::block_on(async {
            loop {
                let mut notified = self.quiesce.notified_raw();
                let _ = notified.enable();
                if self.live() == 0 {
                    return true;
                }
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                let _ = crate::sqz_time::timeout(deadline - now, notified).await;
            }
        })
    }
}

/// An OWNED set of spawned tasks with genuine cancel-on-drop.
pub struct OwnedSet {
    site: &'static str,
    spawner: Spawner,
    state: Arc<CancelState>,
}

impl OwnedSet {
    pub fn new(site: &'static str, spawner: Spawner) -> Self {
        Self {
            site,
            spawner,
            state: Arc::new(CancelState {
                cancelled: AtomicBool::new(false),
                spawned: AtomicU64::new(0),
                resolved: AtomicU64::new(0),
                running: AtomicU64::new(0),
                quiesce: Notify::new(),
                parked: Mutex::new(HashMap::new()),
                next_gate_id: AtomicU64::new(1),
            }),
        }
    }

    /// Spawn a task onto the set's venue, wrapped in the cancel gate.
    /// A task spawned after [`Self::cancel_all`] resolves at its first
    /// poll without ever running.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.state.spawned.fetch_add(1, Ordering::SeqCst);
        let gate = CancelGate {
            id: self.state.next_gate_id.fetch_add(1, Ordering::Relaxed),
            state: Arc::clone(&self.state),
            inner: Some(Box::pin(fut)),
            resolved: false,
        };
        (self.spawner)(self.site, Box::pin(gate));
    }

    /// Cancel every task: set the flag AND wake every parked gate —
    /// cancel itself delivers the poll where the gate resolves (a
    /// never-fires park would otherwise wedge quiesce forever; the
    /// tokio-abort parity the assembly salvage reaper depends on).
    /// One mid-poll task finishes that poll first.
    pub fn cancel_all(&self) {
        self.state.cancel();
    }

    /// Tasks not yet resolved (`spawned − resolved`).
    pub fn live(&self) -> u64 {
        self.state.live()
    }

    /// Wait until every spawned task has resolved (completed, or
    /// cancelled-and-dropped at a poll boundary).
    pub async fn quiesce(&self) {
        self.state.quiesce().await;
    }

    /// Blocking [`Self::quiesce`] with a deadline; `true` = quiesced.
    /// Parks the calling OS thread — never call from a thread the
    /// set's tasks need in order to make progress.
    pub fn quiesce_blocking(&self, timeout: Duration) -> bool {
        self.state.quiesce_blocking(timeout)
    }

    /// A detachable handle (cancel/quiesce/live against the same state)
    /// — for reapers that must outlive the set (`Drop` cannot await).
    pub fn handle(&self) -> SetHandle {
        SetHandle {
            state: Arc::clone(&self.state),
        }
    }
}

impl Drop for OwnedSet {
    fn drop(&mut self) {
        self.cancel_all();
        if self.state.live() == 0 {
            return;
        }
        // Bounded by the sqz TICK: every sqz park re-polls within one
        // TICK, so a cancelled straggler resolves inside the bound
        // unless it is stuck INSIDE a single poll (a bug surfacing —
        // the old tokio abort had the same non-instant mid-poll
        // window) or its spawning lane IS the dropping thread (drops
        // from async contexts should prefer `cancel_all()` +
        // `quiesce().await` instead). A set dropped from sync context
        // must not hang forever, so a timeout reports loudly and the
        // stragglers resolve at their next poll holding only their own
        // captures.
        if !self.state.quiesce_blocking(crate::sqz_channel::TICK) {
            // The ipc crate is dependency-free by law (no `log`):
            // stderr is the loud channel.
            eprintln!(
                "squeezefs sqz_taskset[{}]: drop quiesce timed out after {:?} with {} live \
                 task(s) ({} mid-poll) — a cancelled task has not reached its next poll \
                 boundary; stragglers resolve at their next poll",
                self.site,
                crate::sqz_channel::TICK,
                self.state.live(),
                self.state.running.load(Ordering::SeqCst),
            );
        }
    }
}

/// A clonable view of a set's cancel/quiesce state that outlives the
/// set (the detached salvage-reaper shape).
#[derive(Clone)]
pub struct SetHandle {
    state: Arc<CancelState>,
}

impl SetHandle {
    pub fn cancel_all(&self) {
        self.state.cancel();
    }

    pub fn live(&self) -> u64 {
        self.state.live()
    }

    pub async fn quiesce(&self) {
        self.state.quiesce().await;
    }

    pub fn quiesce_blocking(&self, timeout: Duration) -> bool {
        self.state.quiesce_blocking(timeout)
    }
}

/// The per-task wrapper enforcing cancel-at-next-poll-boundary: the
/// cancel flag is checked BEFORE every inner poll, and on observing it
/// the inner future is taken and dropped INLINE (captures freed —
/// MEM-2), never polled again.
struct CancelGate {
    id: u64,
    state: Arc<CancelState>,
    inner: Option<BoxTask>,
    resolved: bool,
}

impl CancelGate {
    fn deregister(&self) {
        self.state
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

impl Future for CancelGate {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // All fields are Unpin (the inner future is heap-pinned).
        let this = self.get_mut();
        if this.resolved {
            return Poll::Ready(());
        }
        // Register the park waker BEFORE the cancelled re-check
        // (enable-then-recheck): a cancel landing between the check and
        // an eventual Pending return finds this waker in the map and
        // delivers the next poll itself.
        this.state
            .parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(this.id, cx.waker().clone());
        if this.state.cancelled.load(Ordering::SeqCst) {
            // The abort-at-next-poll-boundary semantic: resolve WITHOUT
            // polling the inner future, dropping it first so its
            // captures die before a quiesce waiter can observe
            // `live == 0`.
            this.deregister();
            this.inner = None;
            this.resolved = true;
            this.state.resolve();
            return Poll::Ready(());
        }
        // Track the mid-poll window; the decrement wakes quiesce
        // waiters, and the guard closes the count even when the inner
        // poll UNWINDS (the spawner's containment then drops this gate,
        // whose own Drop closes the resolution accounting).
        struct RunGuard<'a>(&'a CancelState);
        impl Drop for RunGuard<'_> {
            fn drop(&mut self) {
                self.0.running.fetch_sub(1, Ordering::SeqCst);
                self.0.quiesce.notify_waiters();
            }
        }
        this.state.running.fetch_add(1, Ordering::SeqCst);
        let polled = {
            let _running = RunGuard(&this.state);
            this.inner
                .as_mut()
                .expect("CancelGate inner present until resolution")
                .as_mut()
                .poll(cx)
        };
        match polled {
            Poll::Ready(()) => {
                this.deregister();
                this.inner = None;
                this.resolved = true;
                this.state.resolve();
                Poll::Ready(())
            }
            // Stays registered: the waker in the map is the one cancel
            // (or the inner future's own wake) delivers through.
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for CancelGate {
    fn drop(&mut self) {
        // Dropped without resolving (executor teardown, a queued task's
        // queue dropped, panic containment after an unwinding poll):
        // free the captures FIRST, then close the accounting — a
        // quiesce waiter observing `live == 0` must never see the
        // inner future's captures still alive.
        self.deregister();
        self.inner = None;
        if !self.resolved {
            self.resolved = true;
            self.state.resolve();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Deferred spawner: queues tasks; the test polls them explicitly
    /// (the cancelled-before-first-poll venue).
    static DEFERRED: Mutex<Vec<BoxTask>> = Mutex::new(Vec::new());

    fn deferred_spawner(_site: &'static str, task: BoxTask) {
        DEFERRED.lock().unwrap().push(task);
    }

    /// Threaded spawner with the production containment posture (the
    /// spawner is expected to contain panics — `spawn_meta` does).
    fn thread_spawner(_site: &'static str, task: BoxTask) {
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::sqz_blocking::block_on(task)
            }));
        });
    }

    #[test]
    fn cancelled_before_first_poll_never_runs_and_frees_captures() {
        DEFERRED.lock().unwrap().clear();
        let set = OwnedSet::new("test_deferred", deferred_spawner);
        let ran = Arc::new(AtomicBool::new(false));
        let witness = Arc::new(());
        {
            let ran = Arc::clone(&ran);
            let witness = Arc::clone(&witness);
            set.spawn(async move {
                let _held = witness;
                ran.store(true, Ordering::SeqCst);
            });
        }
        assert_eq!(Arc::strong_count(&witness), 2, "capture held pre-poll");
        set.cancel_all();
        assert_eq!(set.live(), 1, "queued task is live until its first poll");
        // First poll AFTER cancel: the gate must resolve immediately,
        // drop the inner future (captures freed) and never run it.
        let queued: Vec<BoxTask> = std::mem::take(&mut *DEFERRED.lock().unwrap());
        assert_eq!(queued.len(), 1);
        for task in queued {
            crate::sqz_blocking::block_on(task);
        }
        assert!(!ran.load(Ordering::SeqCst), "cancelled task must not run");
        assert_eq!(Arc::strong_count(&witness), 1, "captures freed at the gate");
        assert_eq!(set.live(), 0);
        crate::sqz_blocking::block_on(set.quiesce());
    }

    #[test]
    fn drop_cancels_quiesces_and_frees_captures() {
        let witness = Arc::new(());
        let resumed = Arc::new(AtomicBool::new(false));
        {
            let set = OwnedSet::new("test_drop", thread_spawner);
            let w = Arc::clone(&witness);
            let r = Arc::clone(&resumed);
            set.spawn(async move {
                let _held = w;
                // Park at an await point; the cancel fires before the
                // wake, so the poll after this sleep observes the flag
                // and DROPS the future — the code below never runs.
                crate::sqz_time::sleep(Duration::from_millis(100)).await;
                r.store(true, Ordering::SeqCst);
            });
            // Drop: cancel_all + TICK-bounded blocking quiesce.
        }
        assert!(
            !resumed.load(Ordering::SeqCst),
            "a cancelled task never resumes past its await point"
        );
        assert_eq!(
            Arc::strong_count(&witness),
            1,
            "drop returned only after the cancelled future was dropped"
        );
    }

    /// THE abort semantic (the assembly salvage wedge, 2026-08-13): a
    /// task parked on a wake that will NEVER fire (the cancelled
    /// sibling shape — a oneshot whose sender the owner keeps) must
    /// still resolve on cancel_all. "Resolve at the next poll" is
    /// vacuous for a parked task unless CANCEL ITSELF DELIVERS the
    /// poll — tokio's abort wakes the task; so must ours.
    #[test]
    fn cancel_wakes_a_parked_task_that_would_never_wake() {
        let set = OwnedSet::new("test_parked_cancel", thread_spawner);
        let witness = Arc::new(());
        let (_park_tx, park_rx) = crate::sqz_channel::oneshot::channel::<()>();
        {
            let w = Arc::clone(&witness);
            set.spawn(async move {
                let _held = w;
                // Parks forever: the sender lives in the test frame and
                // never sends.
                let _ = park_rx.await;
            });
        }
        // Let the task reach its park.
        std::thread::sleep(Duration::from_millis(50));
        set.cancel_all();
        assert!(
            set.quiesce_blocking(Duration::from_secs(4)),
            "cancel_all must WAKE the parked gate — a never-fires park \
             otherwise wedges quiesce forever (the salvage-reaper wedge)"
        );
        assert_eq!(Arc::strong_count(&witness), 1, "captures freed");
    }

    #[test]
    fn quiesce_waits_for_a_mid_poll_task() {
        let set = OwnedSet::new("test_midpoll", thread_spawner);
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let finished = Arc::new(AtomicBool::new(false));
        {
            let finished = Arc::clone(&finished);
            set.spawn(async move {
                started_tx.send(()).unwrap();
                // A single long poll (no await points): quiesce must
                // not report done while this poll is in flight.
                std::thread::sleep(Duration::from_millis(150));
                finished.store(true, Ordering::SeqCst);
            });
        }
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("task started");
        assert!(
            set.quiesce_blocking(Duration::from_secs(5)),
            "quiesce completes once the poll finishes"
        );
        assert!(
            finished.load(Ordering::SeqCst),
            "quiesce returned only after the mid-poll task finished"
        );
    }

    #[test]
    fn panic_in_a_task_does_not_break_accounting() {
        let set = OwnedSet::new("test_panic", thread_spawner);
        set.spawn(async move {
            panic!("sqz_taskset accounting test: injected panic");
        });
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        set.spawn(async move {
            started_tx.send(()).unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("sibling ran");
        assert!(
            set.quiesce_blocking(Duration::from_secs(5)),
            "a panicked task still resolves (drop-resolve under containment)"
        );
        assert_eq!(set.live(), 0);
    }

    #[test]
    fn quiesce_on_an_empty_set_is_immediate() {
        let set = OwnedSet::new("test_empty", thread_spawner);
        assert!(set.quiesce_blocking(Duration::from_millis(10)));
        crate::sqz_blocking::block_on(set.quiesce());
    }

    #[test]
    fn handle_quiesce_outlives_the_set() {
        let handle = {
            let set = OwnedSet::new("test_handle", thread_spawner);
            set.spawn(async move {
                crate::sqz_time::sleep(Duration::from_millis(20)).await;
            });
            set.handle()
            // Set drops here: cancel + bounded quiesce.
        };
        assert!(handle.quiesce_blocking(Duration::from_secs(5)));
        assert_eq!(handle.live(), 0);
    }
}
