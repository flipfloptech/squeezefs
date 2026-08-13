//! First-party single-flight completion fan-out (the rip-tokio-total
//! program, 2026-08-13). Every `tokio::sync::broadcast` use in the
//! daemon was this exact shape: a leader parks a `Sender` in a
//! lock-free map, cohort members `subscribe()` and await ONE completion
//! value, the leader `send`s once and removes the map entry. This is a
//! SHARED ONESHOT, not a stream — so that is what this primitive is.
//!
//! Strictly better than the broadcast it replaces at the races that
//! matter: a subscriber arriving AFTER the send still gets the stored
//! value (tokio broadcast would strand it until sender drop), and every
//! sender gone without a send is a loud [`Gone`] (the leader-died
//! retry signal). Waiter parks ride the [`crate::sqz_channel::ticked`]
//! backstop (a lost wake costs one TICK, never a wedge).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::sqz_channel::ticked;

/// Every sender dropped without sending — the cohort's leader died;
/// callers re-run their own flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "single-flight leader dropped without completing")
    }
}
impl std::error::Error for Gone {}

struct FlightState<T> {
    value: Option<T>,
    senders: usize,
    wakers: Vec<Waker>,
}

struct Shared<T> {
    state: Mutex<FlightState<T>>,
}

pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

#[derive(Clone)]
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

pub fn channel<T: Clone>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(FlightState {
            value: None,
            senders: 1,
            wakers: Vec::new(),
        }),
    });
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared },
    )
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .senders += 1;
        Sender {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let wakers = {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            st.senders -= 1;
            if st.senders == 0 {
                std::mem::take(&mut st.wakers)
            } else {
                Vec::new()
            }
        };
        for w in wakers {
            w.wake();
        }
    }
}

impl<T: Clone> Sender<T> {
    /// Publish the completion value to every current AND future waiter
    /// (first send wins; repeats are no-ops — idempotent by design).
    pub fn send(&self, value: T) {
        let wakers = {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.value.is_none() {
                st.value = Some(value);
            }
            std::mem::take(&mut st.wakers)
        };
        for w in wakers {
            w.wake();
        }
    }

    pub fn subscribe(&self) -> Receiver<T> {
        Receiver {
            shared: self.shared.clone(),
        }
    }

    /// Live cohort interest (waiter count; diagnostic parity with
    /// broadcast's `receiver_count`).
    pub fn waiter_count(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .wakers
            .len()
    }
}

impl<T: Clone> Receiver<T> {
    /// Await the flight's completion value.
    pub async fn wait(&self) -> Result<T, Gone> {
        let fut = WaitFut { shared: &self.shared };
        ticked(fut).await
    }

    /// Non-blocking probe (a completed flight serves immediately).
    pub fn try_get(&self) -> Option<T> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value
            .clone()
    }
}

struct WaitFut<'a, T> {
    shared: &'a Arc<Shared<T>>,
}

impl<T> Unpin for WaitFut<'_, T> {}

impl<T: Clone> Future for WaitFut<'_, T> {
    type Output = Result<T, Gone>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = st.value.clone() {
            return Poll::Ready(Ok(v));
        }
        if st.senders == 0 {
            return Poll::Ready(Err(Gone));
        }
        st.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn cohort_receives_and_late_subscriber_still_served() {
        let (tx, rx) = channel::<u32>();
        let early = rx.clone();
        let waiter = std::thread::spawn(move || block_on(early.wait()));
        std::thread::sleep(Duration::from_millis(20));
        tx.send(42);
        assert_eq!(waiter.join().unwrap(), Ok(42));
        // Late subscriber: after the send, before sender drop.
        let late = tx.subscribe();
        assert_eq!(block_on(late.wait()), Ok(42), "late subscriber served");
        drop(tx);
        assert_eq!(block_on(rx.wait()), Ok(42), "value outlives the sender");
    }

    #[test]
    fn leader_death_is_loud() {
        let (tx, rx) = channel::<u32>();
        let waiter = {
            let rx = rx.clone();
            std::thread::spawn(move || block_on(rx.wait()))
        };
        std::thread::sleep(Duration::from_millis(20));
        drop(tx);
        assert_eq!(waiter.join().unwrap(), Err(Gone), "dead leader = Gone");
    }
}
