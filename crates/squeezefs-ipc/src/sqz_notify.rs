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
//!   law). The waiter registers **at creation** (see [`Notify::notified`]
//!   — the todo-24 wedge law), keeps ONE registration across ticks and
//!   re-checks the permit + epoch on every poll under the same lock
//!   every notifier mutates under — no lost-wake window by construction.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use crate::sqz_channel::ticked;

enum NotifyWaker {
    /// Registered via `Notified::enable` but not yet polled — no waker
    /// to call; the entry's completion is carried by the permit/epoch
    /// the poll re-checks.
    Unpolled,
    Waker(Waker),
}

#[derive(Default)]
struct NotifyState {
    /// One stored `notify_one` permit (never accumulates past 1 —
    /// tokio parity).
    permit: bool,
    /// Bumped by `notify_waiters`: a waiter whose registration epoch is
    /// older completes on its next poll.
    epoch: u64,
    /// FIFO registered waiters: (id, registration epoch, waker slot).
    waiters: Vec<(u64, u64, NotifyWaker)>,
    next_id: u64,
}

#[derive(Default)]
pub struct Notify {
    state: Mutex<NotifyState>,
}

impl Notify {
    /// Const: static cells (`sqz_once::OnceCell::new`) build on it.
    pub const fn new() -> Self {
        Notify {
            state: Mutex::new(NotifyState {
                permit: false,
                epoch: 0,
                waiters: Vec::new(),
                next_id: 0,
            }),
        }
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
                // path below covers the next arrival). An Unpolled
                // (enabled, never awaited) front entry has no waker —
                // the stored permit completes it on its first poll.
                st.permit = true;
                match st.waiters.remove(0).2 {
                    NotifyWaker::Waker(w) => Some(w),
                    NotifyWaker::Unpolled => None,
                }
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
            st.waiters
                .drain(..)
                .filter_map(|(_, _, w)| match w {
                    NotifyWaker::Waker(w) => Some(w),
                    NotifyWaker::Unpolled => None,
                })
                .collect()
        };
        for w in wakers {
            w.wake();
        }
    }

    /// Park until notified. **Registers AT CREATION** (todo-24, the
    /// meta-slot-migration wedge): the future this returns is already
    /// enabled, so a `notify_one`/`notify_waiters` landing between the
    /// call and the first poll is never lost — which is what makes the
    /// create-recheck-await idiom (`wait_completed_upto`, the admission
    /// loop, the hold seams) sound. The retired `async fn` shape was
    /// inert until first poll: a `notify_waiters` in that window bumped
    /// the epoch BEFORE registration and the waiter parked forever —
    /// unhealable by the tick, which re-polls only the inner
    /// epoch-gated future, never the caller's outer condition.
    pub fn notified(&self) -> impl Future<Output = ()> + '_ {
        let mut fut = self.notified_raw();
        fut.enable();
        ticked(fut)
    }

    /// The tokio `Notified` + `enable()` shape: a HANDLE future you can
    /// register BEFORE re-checking shared state (the enable-then-check
    /// ordering that closes the store/notify race), then await later.
    /// `enable()` returns `true` when a permit/epoch already satisfies
    /// the waiter (tokio parity — the caller may skip the await).
    /// NOT ticked: hot-path pop loops own their own liveness (the
    /// awaiting site composes with `ticked` if it wants the backstop).
    pub fn notified_raw(&self) -> Notified<'_> {
        Notified {
            notify: self,
            id: None,
            registered_epoch: 0,
        }
    }
}

pub struct Notified<'a> {
    notify: &'a Notify,
    id: Option<u64>,
    registered_epoch: u64,
}

impl Notified<'_> {
    /// Register this waiter NOW without awaiting (tokio
    /// `Notified::enable` parity): after `enable()`, a `notify_one`/
    /// `notify_waiters` between the caller's state re-check and its
    /// `.await` is never lost. Returns `true` if already satisfied
    /// (stored permit / passed epoch) — the waiter is then complete and
    /// the await returns immediately.
    pub fn enable(&mut self) -> bool {
        let mut st = self.notify.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.id.is_some() {
            // Already registered (or already satisfied on a prior poll).
            return st.permit || st.epoch > self.registered_epoch;
        }
        if st.permit {
            // Satisfied immediately: consume nothing here — the poll
            // consumes it (keeps enable() idempotent and side-effect
            // ordering identical to tokio's: consumption happens at
            // completion).
            return true;
        }
        let id = st.next_id;
        st.next_id += 1;
        let epoch = st.epoch;
        st.waiters.push((id, epoch, NotifyWaker::Unpolled));
        self.id = Some(id);
        self.registered_epoch = epoch;
        false
    }
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
                    entry.2 = NotifyWaker::Waker(cx.waker().clone());
                } else {
                    // Woken by notify_one (removed there) but the permit
                    // was consumed by a faster claimant: re-register at
                    // the BACK (re-contention, the bounded-barging law).
                    let epoch = st.epoch;
                    st.waiters
                        .push((id, epoch, NotifyWaker::Waker(cx.waker().clone())));
                    self.registered_epoch = epoch;
                }
            }
            None => {
                let id = st.next_id;
                st.next_id += 1;
                let epoch = st.epoch;
                st.waiters
                    .push((id, epoch, NotifyWaker::Waker(cx.waker().clone())));
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

    /// The enable-then-check ordering (the InboundQueue pop shape): a
    /// notify_waiters landing BETWEEN the enable and the await is never
    /// lost — the enabled waiter completes.
    #[test]
    fn enabled_waiter_never_loses_a_pre_await_notify() {
        let n = Notify::new();
        let mut fut = n.notified_raw();
        assert!(!fut.enable(), "nothing pending yet");
        // The race window: notification fires before the await.
        n.notify_waiters();
        // Must complete instantly (epoch passed at registration).
        block_on(fut);

        // Same for notify_one's stored permit.
        let n = Notify::new();
        let mut fut = n.notified_raw();
        assert!(!fut.enable());
        n.notify_one();
        block_on(fut);
    }

    /// THE meta-slot-migration wedge pin (todo-24, 2026-08-15 — the
    /// `test_remove_meta_capacity_preflight_refuses_honestly` ~50 %
    /// futex-park): a `notify_waiters` landing between `notified()`
    /// CREATION and its first poll was LOST — the old `async fn` body
    /// was inert until polled, so the waiter registered AFTER the epoch
    /// bump (live capture: waiter `registered_epoch == st.epoch`,
    /// `completed_upto == pos`, waiter list len 1) and parked forever.
    /// The journal's `wait_completed_upto` create-recheck-await idiom
    /// depends on creation-time registration; the tick backstop only
    /// re-polls the INNER epoch-gated future, so it can never heal
    /// this. Law: `notified()` registers AT CREATION.
    #[test]
    fn notify_between_creation_and_first_poll_is_never_lost() {
        let n = Notify::new();
        let fut = n.notified(); // created, NOT yet polled
        n.notify_waiters(); // the wake that used to be lost
        let out = block_on(crate::sqz_time::timeout(Duration::from_millis(300), fut));
        assert!(
            out.is_ok(),
            "a notify_waiters between notified() creation and its first poll \
             must complete the waiter (creation-time registration)"
        );
    }

    /// The same law for `notify_one` (its stored permit already covered
    /// the pre-registration shape; pinned so the eager-registration
    /// change can never regress it).
    #[test]
    fn notify_one_between_creation_and_first_poll_is_never_lost() {
        let n = Notify::new();
        let fut = n.notified();
        n.notify_one();
        let out = block_on(crate::sqz_time::timeout(Duration::from_millis(300), fut));
        assert!(out.is_ok(), "notify_one before the first poll must serve");
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
