//! First-party `Notify` (the rip-tokio-total program, 2026-08-13) —
//! the tokio `Notify` semantics the daemon uses, waker-based, no
//! runtime driver:
//!
//! * `notify_one` — stores a PERMIT if no waiter is registered, else
//!   wakes exactly one waiter (FIFO). A permit satisfies the next
//!   `notified().await` immediately (the send-then-wait race is safe).
//! * `notify_waiters` — wakes every CURRENTLY-registered waiter and
//!   never stores a permit (tokio parity: waiters that registered
//!   before the call are released; later `notified()` calls park).
//! * `notified().await` — parks under the [`crate::sqz_channel::ticked`]
//!   backstop: a lost wake costs one TICK, never a wedge (the sqz-sync
//!   law). The waiter keeps ONE registration across ticks and re-checks
//!   the permit + epoch on every poll under the same lock every
//!   notifier mutates under — no lost-wake window by construction.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use crate::sqz_channel::ticked;

#[derive(Default)]
struct NotifyState {
    /// One stored `notify_one` permit (never accumulates past 1 —
    /// tokio parity).
    permit: bool,
    /// Bumped by `notify_waiters`: a waiter whose registration epoch is
    /// older completes on its next poll.
    epoch: u64,
    /// FIFO registered waiters: (id, registration epoch, waker).
    waiters: Vec<(u64, u64, Waker)>,
    next_id: u64,
}

#[derive(Default)]
pub struct Notify {
    state: Mutex<NotifyState>,
}

impl Notify {
    pub fn new() -> Self {
        Notify::default()
    }

    pub fn notify_one(&self) {
        let waker = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.waiters.is_empty() {
                st.permit = true;
                None
            } else {
                // FIFO wake; the waiter consumes the grant on poll (its
                // registration is removed there, not here — a woken
                // waiter that died re-registers nothing and the permit
                // path below covers the next arrival).
                st.permit = true;
                Some(st.waiters.remove(0).2)
            }
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    pub fn notify_waiters(&self) {
        let wakers: Vec<Waker> = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.epoch += 1;
            st.waiters.drain(..).map(|(_, _, w)| w).collect()
        };
        for w in wakers {
            w.wake();
        }
    }

    pub async fn notified(&self) {
        let fut = Notified {
            notify: self,
            id: None,
            registered_epoch: 0,
        };
        ticked(fut).await
    }
}

struct Notified<'a> {
    notify: &'a Notify,
    id: Option<u64>,
    registered_epoch: u64,
}

impl Drop for Notified<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut st = self.notify.state.lock().unwrap_or_else(|e| e.into_inner());
            st.waiters.retain(|(wid, _, _)| *wid != id);
        }
    }
}

impl Future for Notified<'_> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut st = self.notify.state.lock().unwrap_or_else(|e| e.into_inner());
        // A notify_waiters since registration completes this waiter.
        if self.id.is_some() && st.epoch > self.registered_epoch {
            let id = self.id.take().expect("checked");
            st.waiters.retain(|(wid, _, _)| *wid != id);
            return Poll::Ready(());
        }
        // A stored permit serves the front-most claimant (or a fresh
        // arrival when none is registered — the send-then-wait shape).
        if st.permit {
            st.permit = false;
            if let Some(id) = self.id.take() {
                st.waiters.retain(|(wid, _, _)| *wid != id);
            }
            return Poll::Ready(());
        }
        match self.id {
            Some(id) => {
                // Refresh the waker in place (ticked re-poll).
                if let Some(entry) = st.waiters.iter_mut().find(|(wid, _, _)| *wid == id) {
                    entry.2 = cx.waker().clone();
                } else {
                    // Woken by notify_one (removed there) but the permit
                    // was consumed by a faster claimant: re-register at
                    // the BACK (re-contention, the bounded-barging law).
                    let epoch = st.epoch;
                    st.waiters.push((id, epoch, cx.waker().clone()));
                    self.registered_epoch = epoch;
                }
            }
            None => {
                let id = st.next_id;
                st.next_id += 1;
                let epoch = st.epoch;
                st.waiters.push((id, epoch, cx.waker().clone()));
                self.id = Some(id);
                self.registered_epoch = epoch;
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn block_on<F: Future>(fut: F) -> F::Output {
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
    fn permit_satisfies_late_waiter() {
        let n = Notify::new();
        n.notify_one();
        // Registered AFTER the notify — the permit must serve it.
        block_on(n.notified());
    }

    #[test]
    fn notify_one_wakes_a_parked_waiter() {
        let n = Arc::new(Notify::new());
        let n2 = n.clone();
        let waiter = std::thread::spawn(move || block_on(n2.notified()));
        std::thread::sleep(Duration::from_millis(30));
        n.notify_one();
        waiter.join().unwrap();
    }

    #[test]
    fn notify_waiters_releases_registered_only() {
        let n = Arc::new(Notify::new());
        let n2 = n.clone();
        let waiter = std::thread::spawn(move || block_on(n2.notified()));
        std::thread::sleep(Duration::from_millis(30));
        n.notify_waiters();
        waiter.join().unwrap();
        // No permit was stored: a later notified() must park until a
        // fresh notify (proved via the ticked timeout).
        let late = crate::sqz_time::timeout(Duration::from_millis(100), n.notified());
        assert!(
            block_on(late).is_err(),
            "notify_waiters must not store a permit"
        );
    }
}
