//! The sqz-exec task state word (docs/design-sqz-sync.md, Stage 1b —
//! first-party poll delivery for the metadata plane's handler venue).
//!
//! The Stage-1 field attribution proved the OQ-5 wedge class lives in
//! tokio's task DELIVERY: a woken task is never re-polled, its timer
//! wakes no-op like its I/O wakes, and no future-layer backstop (locks,
//! ticks) can heal it. The fix is that wake→queue→poll must be OUR code.
//! This module is the delivery protocol's heart: one atomic state word
//! per task, with the two laws that make a lost poll unrepresentable:
//!
//! 1. **A wake is never dropped while the task runs**: a `wake()` racing
//!    the poll CASes RUNNING → NOTIFIED, and the runner's post-poll
//!    transition observes it and RESCHEDULES — the classic protocol,
//!    here small enough to model exhaustively.
//! 2. **A task is never queued twice**: only the IDLE→SCHEDULED and the
//!    post-poll NOTIFIED→SCHEDULED transitions may enqueue, and both are
//!    single-winner CASes.
//!
//! Dependency-free on purpose: `loom-models/src/lib.rs` `#[path]`-includes
//! this file and model-checks the exact shipped transitions (the
//! `wake_core`/`gauge_core` convention). The main build never sets
//! `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU8, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU8, Ordering};
}

use atomic::{AtomicU8, Ordering};

/// Not scheduled, not running — the only state a wake may enqueue from.
pub const IDLE: u8 = 0;
/// In the ready queue (or being pushed to it by the wake that won).
pub const SCHEDULED: u8 = 1;
/// Being polled by the executor thread.
pub const RUNNING: u8 = 2;
/// Being polled AND woken since the poll began — the runner must
/// reschedule instead of parking the task.
pub const NOTIFIED: u8 = 3;
/// The future returned `Ready` — terminal; wakes no-op forever.
pub const COMPLETE: u8 = 4;

/// One task's delivery state word.
pub struct TaskState {
    state: AtomicU8,
}

impl TaskState {
    /// A freshly spawned task starts SCHEDULED: the spawner enqueues it
    /// (spawn IS its first wake).
    pub fn new_scheduled() -> Self {
        TaskState {
            state: AtomicU8::new(SCHEDULED),
        }
    }

    /// A wake. Returns `true` iff the CALLER must enqueue the task (it
    /// won the IDLE→SCHEDULED transition). Every other state either has
    /// the task already queued (SCHEDULED), guarantees a reschedule
    /// check (RUNNING→NOTIFIED — the runner observes it post-poll), is
    /// already recorded (NOTIFIED), or is terminal (COMPLETE).
    pub fn wake(&self) -> bool {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            let (next, enqueue) = match cur {
                IDLE => (SCHEDULED, true),
                RUNNING => (NOTIFIED, false),
                SCHEDULED | NOTIFIED | COMPLETE => return false,
                _ => unreachable!("invalid task state {cur}"),
            };
            // AcqRel: an enqueue-winning wake publishes the waker's
            // preceding writes to the runner's next poll; the failure
            // load re-reads the racing state.
            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return enqueue,
                Err(actual) => cur = actual,
            }
        }
    }

    /// The runner dequeued this task and is about to poll: SCHEDULED →
    /// RUNNING. Returns `false` if the task completed concurrently (a
    /// stale queue entry — skip it; COMPLETE is terminal and the state
    /// machine admits no other state at dequeue).
    pub fn begin_poll(&self) -> bool {
        // The task is in the queue, so its state is SCHEDULED (the only
        // transition that enqueues) or COMPLETE is impossible pre-poll —
        // but a panicked poll path may have completed it defensively.
        self.state
            .compare_exchange(SCHEDULED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The poll returned `Pending`. RUNNING → IDLE, unless a wake landed
    /// mid-poll (NOTIFIED) — then → SCHEDULED and the caller MUST
    /// re-enqueue (law 1: the mid-poll wake is never dropped). Returns
    /// `true` iff the caller must re-enqueue.
    pub fn end_poll_pending(&self) -> bool {
        match self
            .state
            .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => false,
            Err(actual) => {
                debug_assert_eq!(actual, NOTIFIED, "poll ended from invalid state");
                // The mid-poll wake already holds its record; the runner
                // owns the reschedule. Store (not CAS): only the runner
                // may leave NOTIFIED.
                self.state.store(SCHEDULED, Ordering::Release);
                true
            }
        }
    }

    /// The poll returned `Ready`: terminal. Any state (RUNNING or
    /// NOTIFIED — a wake racing completion) collapses to COMPLETE.
    pub fn end_poll_complete(&self) {
        let prev = self.state.swap(COMPLETE, Ordering::AcqRel);
        debug_assert!(
            prev == RUNNING || prev == NOTIFIED,
            "completed from invalid state {prev}"
        );
    }

    /// Diagnostic read (the executor's tick backstop scan).
    pub fn load(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}
