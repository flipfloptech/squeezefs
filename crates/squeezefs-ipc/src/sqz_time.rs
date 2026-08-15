//! First-party timer service (design-sqz-sync, the rip-tokio-out sweep,
//! 2026-08-13): `sleep` / `timeout` / `interval` whose delivery is OUR
//! code end to end — one dedicated OS thread (`sqz-timer`) parks on a
//! condvar until the next deadline and fires registered wakers. No
//! tokio driver anywhere in the path: a woken task's poll delivery is
//! whatever executor polls it (sqz-exec lanes, sqz-meta, a tokio
//! runtime in tests — the waker abstraction carries it).
//!
//! Scope: coarse liveness timers (ticks, retry backoffs, parks,
//! deadlines) — the daemon's uses. Not a high-resolution timer wheel:
//! the heap + condvar gives ~ms-class accuracy under load, which every
//! call site here tolerates by design (they tolerated tokio's coarse
//! timer the same way).
//!
//! Cancel safety: dropping a [`Sleep`] unregisters its entry (an O(1)
//! tombstone — the service skips fired/cancelled ids), so a `timeout`
//! loser leaks nothing.

use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

struct Entry {
    deadline: Instant,
    id: u64,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.id == other.id
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap by deadline (BinaryHeap is a max-heap).
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.id.cmp(&self.id))
    }
}

#[derive(Default)]
struct Registry {
    heap: BinaryHeap<Entry>,
    /// id → waker for LIVE sleeps; absent = fired or cancelled (the
    /// heap entry becomes a tombstone the service thread skips).
    live: std::collections::HashMap<u64, SleepState>,
}

enum SleepState {
    /// Registered, not yet polled to completion.
    Waiting(Option<Waker>),
    /// Deadline passed; the next poll returns Ready.
    Fired,
}

struct Service {
    reg: Mutex<Registry>,
    cv: Condvar,
    next_id: AtomicU64,
}

fn service() -> &'static Service {
    static S: OnceLock<&'static Service> = OnceLock::new();
    S.get_or_init(|| {
        let svc: &'static Service = Box::leak(Box::new(Service {
            reg: Mutex::new(Registry::default()),
            cv: Condvar::new(),
            next_id: AtomicU64::new(1),
        }));
        std::thread::Builder::new()
            .name(crate::comm_core::comm_name("sqz-timer"))
            .spawn(move || service_loop(svc))
            .expect("sqz-timer thread spawns");
        svc
    })
}

fn service_loop(svc: &'static Service) {
    let mut reg = svc.reg.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let now = Instant::now();
        // Fire everything due; collect wakers to call OUTSIDE the lock.
        let mut wakers: Vec<Waker> = Vec::new();
        while let Some(top) = reg.heap.peek() {
            if top.deadline > now {
                break;
            }
            let id = reg.heap.pop().expect("peeked").id;
            if let Some(state) = reg.live.get_mut(&id) {
                let prev = std::mem::replace(state, SleepState::Fired);
                if let SleepState::Waiting(Some(w)) = prev {
                    wakers.push(w);
                }
            }
            // Absent = cancelled tombstone: skip.
        }
        if !wakers.is_empty() {
            drop(reg);
            for w in wakers {
                w.wake();
            }
            reg = svc.reg.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        // Park until the next deadline (or a registration wake).
        reg = match reg.heap.peek().map(|t| t.deadline) {
            Some(next) => {
                let wait = next.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    continue;
                }
                svc.cv
                    .wait_timeout(reg, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            None => svc.cv.wait(reg).unwrap_or_else(|e| e.into_inner()),
        };
    }
}

/// A first-party sleep future. Fires at `deadline`; drop cancels.
pub struct Sleep {
    id: u64,
    /// Completed or cancelled — the drop unregister is a no-op then.
    done: bool,
}

impl Sleep {
    fn new(deadline: Instant) -> Sleep {
        let svc = service();
        let id = svc.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut reg = svc.reg.lock().unwrap_or_else(|e| e.into_inner());
            let is_next = reg
                .heap
                .peek()
                .map(|t| deadline < t.deadline)
                .unwrap_or(true);
            reg.live.insert(id, SleepState::Waiting(None));
            reg.heap.push(Entry { deadline, id });
            if is_next {
                svc.cv.notify_one();
            }
        }
        Sleep { id, done: false }
    }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.done {
            return Poll::Ready(());
        }
        let svc = service();
        let mut reg = svc.reg.lock().unwrap_or_else(|e| e.into_inner());
        match reg.live.get_mut(&self.id) {
            Some(SleepState::Fired) | None => {
                reg.live.remove(&self.id);
                drop(reg);
                self.done = true;
                Poll::Ready(())
            }
            Some(SleepState::Waiting(w)) => {
                *w = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let svc = service();
        let mut reg = svc.reg.lock().unwrap_or_else(|e| e.into_inner());
        reg.live.remove(&self.id); // heap entry becomes a tombstone
    }
}

/// Sleep for `d`.
pub fn sleep(d: Duration) -> Sleep {
    Sleep::new(Instant::now() + d)
}

/// Sleep until `deadline`.
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep::new(deadline)
}

/// The elapsed marker (mirrors `tokio::time::error::Elapsed`'s role).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline elapsed")
    }
}
impl std::error::Error for Elapsed {}

/// First-party timeout: polls `fut` first (a ready future beats an
/// already-expired deadline, the tokio posture), then the deadline.
pub struct Timeout<F> {
    fut: Pin<Box<F>>,
    sleep: Sleep,
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(v) = self.fut.as_mut().poll(cx) {
            return Poll::Ready(Ok(v));
        }
        match Pin::new(&mut self.sleep).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Run `fut` under a deadline of `d`.
pub fn timeout<F: Future>(d: Duration, fut: F) -> Timeout<F> {
    Timeout {
        fut: Box::pin(fut),
        sleep: sleep(d),
    }
}

/// Run `fut` until `deadline` (the `tokio::time::timeout_at` shape).
pub fn timeout_at<F: Future>(deadline: Instant, fut: F) -> Timeout<F> {
    Timeout {
        fut: Box::pin(fut),
        sleep: sleep_until(deadline),
    }
}

/// A coarse fixed-period ticker (the `tokio::time::interval` shape the
/// daemon uses: cadence loops). Missed ticks DELAY (the
/// `MissedTickBehavior::Delay` posture every caller here set): the next
/// tick is `period` after the previous tick RETURNED, never a burst.
pub struct Interval {
    period: Duration,
}

impl Interval {
    pub async fn tick(&mut self) {
        sleep(self.period).await;
    }
}

/// A ticker with period `d` (first tick after one full period — every
/// daemon call site parks its loop on the tick, none needs the
/// tokio-style immediate first fire).
pub fn interval(d: Duration) -> Interval {
    Interval { period: d }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal thread-parking block_on (no executor dependency): the
    /// waker unparks this thread; the loop re-polls.
    fn block_on<F: Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::Wake;
        struct ThreadWaker(std::thread::Thread);
        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let mut fut = Box::pin(fut);
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    #[test]
    fn sleep_fires_and_timeout_splits() {
        // Plain OS-thread executor via futures::executor (no tokio).
        let t0 = Instant::now();
        block_on(sleep(Duration::from_millis(50)));
        assert!(t0.elapsed() >= Duration::from_millis(45));

        // Timeout: winner side.
        let r = block_on(timeout(Duration::from_millis(200), async {
            sleep(Duration::from_millis(20)).await;
            7u32
        }));
        assert_eq!(r, Ok(7));

        // Timeout: elapsed side.
        let r = block_on(timeout(
            Duration::from_millis(30),
            std::future::pending::<()>(),
        ));
        assert_eq!(r, Err(Elapsed));
    }

    #[test]
    fn dropped_sleep_cancels_cleanly() {
        {
            let _s = sleep(Duration::from_secs(3600));
            // dropped here — tombstoned, never fires
        }
        let t0 = Instant::now();
        block_on(sleep(Duration::from_millis(30)));
        assert!(t0.elapsed() < Duration::from_secs(5), "service stays live");
    }

    #[test]
    fn many_sleeps_orderly() {
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..50u64 {
            let tx = tx.clone();
            std::thread::spawn(move || {
                block_on(sleep(Duration::from_millis(10 + (i % 7) * 5)));
                tx.send(i).unwrap();
            });
        }
        for _ in 0..50 {
            rx.recv_timeout(Duration::from_secs(10)).expect("all fire");
        }
    }
}
