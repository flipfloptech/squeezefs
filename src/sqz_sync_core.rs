//! The sqz-sync lock state machine (docs/design-sqz-sync.md) — the
//! acquire/release/wake protocol under `SqzMutex`/`SqzRwLock`, extracted
//! dependency-free so `loom-models/` can `#[path]`-include it and
//! model-check the exact shipped transitions (the `gauge_core` pattern;
//! the main build never sets `cfg(loom)`).
//!
//! THE design property (what makes the OQ-5 wedge class unrepresentable):
//! **bounded barging** — release never assigns ownership to a queued
//! waiter; it flips the lock free and returns the front wakers (all
//! leading shared, or one exclusive) for the caller to wake. Woken
//! waiters re-contend on poll, so a waiter whose wake is lost (or whose
//! task is never polled again) owns nothing and blocks nobody. The
//! bound (fairness fix, 2026-08-13): only QUEUED waiters barge — a
//! FRESH attempt takes a free lock only when no one is queued (FIFO
//! courtesy, the tokio fairness contract every call site was written
//! against; without it a 100 %-duty release→relock writer reacquires
//! inline nanoseconds after release and starves the woken FIFO front
//! forever — the rw5a patch-storm capture). A dead front waiter
//! therefore costs contenders at most one TICK re-contend, never a
//! wedge.
//!
//! Generic over the waker type `W` (the shipped face uses
//! `std::task::Waker`; the loom models use a recording token) — the core
//! only clones and returns wakers, it never calls them, so no wake runs
//! under the interior mutex's critical section by construction of the
//! shipped wrapper (which wakes after the guard drops).

#[cfg(loom)]
use loom::sync::Mutex as CoreMutex;
#[cfg(not(loom))]
use std::sync::Mutex as CoreMutex;

/// Acquisition mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    Shared,
    Exclusive,
}

struct WaitEntry<W> {
    id: u64,
    want: Want,
    waker: W,
}

struct State<W> {
    /// Readers currently inside (0 with `writer=false` = free).
    readers: usize,
    writer: bool,
    /// FIFO wait queue — wake order only; NEVER ownership (barging).
    queue: std::collections::VecDeque<WaitEntry<W>>,
    next_id: u64,
}

/// The lock word + wait queue. Every transition runs under one interior
/// short-hold mutex (nanosecond critical sections, never held across an
/// await by the shipped wrapper).
pub struct LockCore<W> {
    state: CoreMutex<State<W>>,
}

impl<W: Clone> LockCore<W> {
    #[allow(clippy::new_without_default)] // W-generic; Default adds nothing
    pub fn new() -> Self {
        LockCore {
            state: CoreMutex::new(State {
                readers: 0,
                writer: false,
                queue: std::collections::VecDeque::new(),
                next_id: 1,
            }),
        }
    }

    /// One acquisition attempt. Returns `(granted, wakers_to_wake)` — the
    /// caller wakes AFTER its own lock ceremony (a granted shared acquire
    /// lets contiguous leading shared waiters through with it).
    ///
    /// FIFO courtesy: a FRESH attempt (`my_id == None`) does not barge —
    /// shared yields to any queued exclusive waiter (write preference)
    /// and exclusive yields to ANY queued waiter (fairness, the tokio
    /// posture the call sites were written against). A QUEUED waiter
    /// re-contends unconditionally — barging among waiters keeps the
    /// lock takeable when a woken waiter is dead.
    pub fn try_acquire(&self, want: Want, my_id: Option<u64>) -> (bool, Vec<W>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let at_front = match my_id {
            Some(id) => st.queue.front().map(|w| w.id) == Some(id),
            None => st.queue.is_empty(),
        };
        match want {
            Want::Shared => {
                let blocked_by_writer_wait = !at_front
                    && st.queue.iter().any(|w| w.want == Want::Exclusive)
                    && my_id.is_none();
                if !st.writer && !blocked_by_writer_wait {
                    st.readers += 1;
                    if let Some(id) = my_id {
                        st.queue.retain(|w| w.id != id);
                    }
                    let wake = Self::front_wakers(&st);
                    return (true, wake);
                }
            }
            Want::Exclusive => {
                // FIFO courtesy (fairness fix, 2026-08-13 — the rw5a
                // writer-storm starvation): a FRESH exclusive attempt
                // queues behind existing waiters, mirroring
                // tokio::sync::Mutex fairness (release→relock loops
                // reacquired inline nanoseconds after the release and
                // starved the woken FIFO front forever). A QUEUED
                // waiter still re-contends unconditionally — barging
                // among waiters is the dead-waiter tick rail: a dead
                // front waiter costs contenders one TICK, never a
                // wedge.
                if !st.writer && st.readers == 0 && (my_id.is_some() || at_front) {
                    st.writer = true;
                    if let Some(id) = my_id {
                        st.queue.retain(|w| w.id != id);
                    }
                    return (true, Vec::new());
                }
            }
        }
        (false, Vec::new())
    }

    /// Register (or refresh) a waiter. Returns its id.
    pub fn register(&self, want: Want, my_id: Option<u64>, waker: &W) -> u64 {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = my_id {
            if let Some(w) = st.queue.iter_mut().find(|w| w.id == id) {
                w.waker = waker.clone();
                return id;
            }
        }
        let id = st.next_id;
        st.next_id += 1;
        st.queue.push_back(WaitEntry {
            id,
            want,
            waker: waker.clone(),
        });
        id
    }

    /// Unlink a waiter (acquire-future cancellation — nothing was ever
    /// reserved for it, so cancellation leaks nothing).
    pub fn unregister(&self, id: u64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.retain(|w| w.id != id);
    }

    /// Release one shared hold; returns the wakers to wake (nonempty only
    /// when the last reader leaves and waiters are queued).
    pub fn release_shared(&self) -> Vec<W> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(st.readers > 0);
        st.readers = st.readers.saturating_sub(1);
        if st.readers == 0 {
            Self::front_wakers(&st)
        } else {
            Vec::new()
        }
    }

    /// Release the exclusive hold; returns the wakers to wake. The lock
    /// is FREE on return — never assigned (barging).
    pub fn release_exclusive(&self) -> Vec<W> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(st.writer);
        st.writer = false;
        Self::front_wakers(&st)
    }

    /// The queue front's wakers: one exclusive waiter, or every leading
    /// shared waiter. Clones only — ownership is taken on re-poll.
    /// Allocation is LAZY (`Vec::new()` + push): the uncontended path
    /// (empty queue — every warm fast-path serve) allocates NOTHING,
    /// which the ipc op-economy contract pins (≤ 1 alloc / 100 warm ops).
    fn front_wakers(st: &State<W>) -> Vec<W> {
        let mut out = Vec::new();
        for w in st.queue.iter() {
            match w.want {
                Want::Exclusive => {
                    if out.is_empty() {
                        out.push(w.waker.clone());
                    }
                    break;
                }
                Want::Shared => out.push(w.waker.clone()),
            }
        }
        out
    }

    /// Queue length (test/diagnostic surface).
    pub fn queue_len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .len()
    }
}
