//! First-party async channels (the rip-tokio-total program, 2026-08-13):
//! `oneshot`, bounded/unbounded `mpsc`, and `watch` — the exact API
//! subset the daemon uses, waker-based end to end, no runtime driver
//! anywhere. Delivery is OUR code: a woken future's poll rides whatever
//! executor polls it (sqz-exec lanes, sqz-meta, a test runtime).
//!
//! Design: one short-hold `Mutex<State>` per channel. The lost-wake
//! window does not exist by construction — a waiter re-checks state and
//! registers its waker under the SAME lock the sender mutates state
//! under, and wakes are always called AFTER the guard drops. On top of
//! that, every unbounded park self-heals on the [`crate::sqz_time`]
//! TICK (the sqz-sync law: a lost wake costs one tick, never a wedge)
//! via [`ticked`], and `TICKED_WAIT_RECOVERIES` counts engagements
//! loudly (0 on healthy schedules).
//!
//! Fairness posture: mpsc send-side capacity waiters are FIFO-woken and
//! re-contend on poll (bounded barging — the sqz-sync fairness law;
//! waiters keep their slot across ticks because [`ticked`] re-polls the
//! SAME future).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// The park tick (mirrors `sqz_sync::TICK` — one law, two planes).
pub(crate) const TICK: Duration = Duration::from_secs(2);

/// Ticked-park engagements across ALL sqz primitives (channels, notify,
/// semaphore): growth = a lost wake was absorbed, loudly. Exported on
/// the stats inode as `channel_ticked_reregisters`.
pub static TICKED_WAIT_RECOVERIES: AtomicU64 = AtomicU64::new(0);

/// Re-poll `f` at TICK until ready — the backstop that absorbs a lost
/// wake. The future keeps its identity (and so any registered waiter
/// slot) across ticks.
pub(crate) async fn ticked<F: Future + Unpin>(mut f: F) -> F::Output {
    loop {
        match crate::sqz_time::timeout(TICK, &mut f).await {
            Ok(v) => return v,
            Err(_elapsed) => {
                TICKED_WAIT_RECOVERIES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn wake_all(wakers: &mut Vec<Waker>) {
    for w in wakers.drain(..) {
        w.wake();
    }
}

// =========================================================================
// oneshot
// =========================================================================

pub mod oneshot {
    use super::*;

    struct Shared<T> {
        state: Mutex<OneState<T>>,
    }

    struct OneState<T> {
        value: Option<T>,
        /// Sender gone (sent or dropped).
        closed: bool,
        /// Receiver gone — send returns the value back.
        rx_gone: bool,
        waker: Option<Waker>,
    }

    pub struct Sender<T> {
        shared: Arc<Shared<T>>,
    }

    pub struct Receiver<T> {
        shared: Arc<Shared<T>>,
    }

    /// The receive-side error (sender dropped without sending).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RecvError;

    impl std::fmt::Display for RecvError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "oneshot sender dropped without sending")
        }
    }
    impl std::error::Error for RecvError {}

    pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(Shared {
            state: Mutex::new(OneState {
                value: None,
                closed: false,
                rx_gone: false,
                waker: None,
            }),
        });
        (
            Sender {
                shared: shared.clone(),
            },
            Receiver { shared },
        )
    }

    impl<T> Sender<T> {
        /// Send the value; `Err(v)` hands it back if the receiver is gone.
        pub fn send(self, value: T) -> Result<(), T> {
            let waker = {
                let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if st.rx_gone {
                    return Err(value);
                }
                st.value = Some(value);
                st.closed = true;
                st.waker.take()
            };
            if let Some(w) = waker {
                w.wake();
            }
            // Drop runs next and is harmless: `closed` is already set
            // and the waker slot is already empty (never mem::forget —
            // that would leak the Arc).
            Ok(())
        }

        pub fn is_closed(&self) -> bool {
            self.shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rx_gone
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let waker = {
                let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                st.closed = true;
                st.waker.take()
            };
            if let Some(w) = waker {
                w.wake();
            }
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            st.rx_gone = true;
        }
    }

    impl<T> Receiver<T> {
        pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            match st.value.take() {
                Some(v) => Ok(v),
                None if st.closed => Err(TryRecvError::Closed),
                None => Err(TryRecvError::Empty),
            }
        }

        /// Close the receive side without dropping (tokio parity).
        pub fn close(&mut self) {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            st.rx_gone = true;
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TryRecvError {
        Empty,
        Closed,
    }

    impl<T> Future for Receiver<T> {
        type Output = Result<T, RecvError>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = st.value.take() {
                return Poll::Ready(Ok(v));
            }
            if st.closed {
                return Poll::Ready(Err(RecvError));
            }
            st.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// =========================================================================
// mpsc (bounded + unbounded)
// =========================================================================

pub mod mpsc {
    use super::*;

    pub mod error {
        /// Bounded/unbounded send onto a closed channel.
        #[derive(Debug, PartialEq, Eq)]
        pub struct SendError<T>(pub T);

        impl<T> std::fmt::Display for SendError<T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "channel closed")
            }
        }
        impl<T: std::fmt::Debug> std::error::Error for SendError<T> {}

        #[derive(Debug, PartialEq, Eq)]
        pub enum TrySendError<T> {
            Full(T),
            Closed(T),
        }

        impl<T> std::fmt::Display for TrySendError<T> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    TrySendError::Full(_) => write!(f, "no available capacity"),
                    TrySendError::Closed(_) => write!(f, "channel closed"),
                }
            }
        }
        impl<T: std::fmt::Debug> std::error::Error for TrySendError<T> {}

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum TryRecvError {
            Empty,
            Disconnected,
        }
    }
    use error::{SendError, TryRecvError, TrySendError};

    struct ChanState<T> {
        queue: VecDeque<T>,
        /// `usize::MAX` = unbounded.
        capacity: usize,
        senders: usize,
        rx_alive: bool,
        /// Receiver-side `close()` — sends refuse, drained recvs still
        /// serve (tokio semantics).
        closed: bool,
        rx_waker: Option<Waker>,
        /// FIFO capacity waiters (bounded send side).
        tx_wakers: VecDeque<Waker>,
    }

    struct Chan<T> {
        state: Mutex<ChanState<T>>,
    }

    pub struct Sender<T> {
        chan: Arc<Chan<T>>,
    }

    pub struct Receiver<T> {
        chan: Arc<Chan<T>>,
    }

    pub struct UnboundedSender<T>(Sender<T>);
    pub struct UnboundedReceiver<T>(Receiver<T>);

    pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
        assert!(capacity > 0, "mpsc bounded capacity must be > 0");
        make(capacity)
    }

    pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
        let (tx, rx) = make(usize::MAX);
        (UnboundedSender(tx), UnboundedReceiver(rx))
    }

    fn make<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
        let chan = Arc::new(Chan {
            state: Mutex::new(ChanState {
                queue: VecDeque::new(),
                capacity,
                senders: 1,
                rx_alive: true,
                closed: false,
                rx_waker: None,
                tx_wakers: VecDeque::new(),
            }),
        });
        (Sender { chan: chan.clone() }, Receiver { chan })
    }

    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            self.chan
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .senders += 1;
            Sender {
                chan: self.chan.clone(),
            }
        }
    }

    impl<T> Clone for UnboundedSender<T> {
        fn clone(&self) -> Self {
            UnboundedSender(self.0.clone())
        }
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let waker = {
                let mut st = self.chan.state.lock().unwrap_or_else(|e| e.into_inner());
                st.senders -= 1;
                if st.senders == 0 {
                    st.rx_waker.take()
                } else {
                    None
                }
            };
            if let Some(w) = waker {
                w.wake();
            }
        }
    }

    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            let mut wakers = {
                let mut st = self.chan.state.lock().unwrap_or_else(|e| e.into_inner());
                st.rx_alive = false;
                st.closed = true;
                std::mem::take(&mut st.tx_wakers)
            };
            for w in wakers.drain(..) {
                w.wake();
            }
        }
    }

    impl<T> Sender<T> {
        pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
            let waker = {
                let mut st = self.chan.state.lock().unwrap_or_else(|e| e.into_inner());
                if st.closed || !st.rx_alive {
                    return Err(TrySendError::Closed(value));
                }
                if st.queue.len() >= st.capacity {
                    return Err(TrySendError::Full(value));
                }
                st.queue.push_back(value);
                st.rx_waker.take()
            };
            if let Some(w) = waker {
                w.wake();
            }
            Ok(())
        }

        /// Async send: parks FIFO on capacity, ticked (a lost wake costs
        /// one TICK, never a wedge).
        pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
            // Fast path outside the ticked ceremony.
            let mut slot = Some(value);
            match self.try_send(slot.take().expect("value present")) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(v)) => return Err(SendError(v)),
                Err(TrySendError::Full(v)) => slot = Some(v),
            }
            let fut = SendFut {
                sender: self,
                value: slot,
            };
            ticked(fut).await
        }

        pub fn is_closed(&self) -> bool {
            let st = self.chan.state.lock().unwrap_or_else(|e| e.into_inner());
            st.closed || !st.rx_alive
        }

        /// Remaining capacity (tokio's `capacity()`).
        pub fn capacity(&self) -> usize {
            let st = self.chan.state.lock().unwrap_or_else(|e| e.into_inner());
            st.capacity.saturating_sub(st.queue.len())
        }

        /// Configured bound (tokio's `max_capacity()`).
        pub fn max_capacity(&self) -> usize {
            self.chan
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .capacity
        }
    }

    struct SendFut<'a, T> {
        sender: &'a Sender<T>,
        value: Option<T>,
    }

    impl<T> Unpin for SendFut<'_, T> {}

    impl<T> Future for SendFut<'_, T> {
        type Output = Result<(), SendError<T>>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let value = match self.value.take() {
                Some(v) => v,
                None => return Poll::Ready(Ok(())), // spurious re-poll after success
            };
            let waker = {
                let mut st = self
                    .sender
                    .chan
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if st.closed || !st.rx_alive {
                    return Poll::Ready(Err(SendError(value)));
                }
                if st.queue.len() >= st.capacity {
                    self.value = Some(value);
                    st.tx_wakers.push_back(cx.waker().clone());
                    return Poll::Pending;
                }
                st.queue.push_back(value);
                st.rx_waker.take()
            };
            if let Some(w) = waker {
                w.wake();
            }
            Poll::Ready(Ok(()))
        }
    }

    impl<T> UnboundedSender<T> {
        pub fn send(&self, value: T) -> Result<(), SendError<T>> {
            match self.0.try_send(value) {
                Ok(()) => Ok(()),
                Err(TrySendError::Closed(v)) => Err(SendError(v)),
                Err(TrySendError::Full(_)) => unreachable!("unbounded channel is never full"),
            }
        }

        pub fn is_closed(&self) -> bool {
            self.0.is_closed()
        }
    }

    impl<T> Receiver<T> {
        pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
            let (out, waker) = {
                let mut st = self.chan.state.lock().unwrap_or_else(|e| e.into_inner());
                match st.queue.pop_front() {
                    Some(v) => (Ok(v), st.tx_wakers.pop_front()),
                    None if st.senders == 0 || st.closed => (Err(TryRecvError::Disconnected), None),
                    None => (Err(TryRecvError::Empty), None),
                }
            };
            if let Some(w) = waker {
                w.wake();
            }
            out
        }

        /// Receive; `None` = channel closed AND drained (tokio parity).
        /// Parks ticked (a lost wake costs one TICK, never a wedge).
        pub async fn recv(&mut self) -> Option<T> {
            match self.try_recv() {
                Ok(v) => return Some(v),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            let fut = RecvFut { rx: self };
            ticked(fut).await
        }

        /// Refuse further sends; drained recvs still serve.
        pub fn close(&mut self) {
            let mut wakers = {
                let mut st = self.chan.state.lock().unwrap_or_else(|e| e.into_inner());
                st.closed = true;
                std::mem::take(&mut st.tx_wakers)
            };
            for w in wakers.drain(..) {
                w.wake();
            }
        }

        pub fn is_empty(&self) -> bool {
            self.chan
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .queue
                .is_empty()
        }

        pub fn len(&self) -> usize {
            self.chan
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .queue
                .len()
        }
    }

    struct RecvFut<'a, T> {
        rx: &'a mut Receiver<T>,
    }

    impl<T> Unpin for RecvFut<'_, T> {}

    impl<T> Future for RecvFut<'_, T> {
        type Output = Option<T>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let (out, waker) = {
                let mut st = self.rx.chan.state.lock().unwrap_or_else(|e| e.into_inner());
                match st.queue.pop_front() {
                    Some(v) => (Poll::Ready(Some(v)), st.tx_wakers.pop_front()),
                    None if st.senders == 0 || st.closed => (Poll::Ready(None), None),
                    None => {
                        st.rx_waker = Some(cx.waker().clone());
                        (Poll::Pending, None)
                    }
                }
            };
            if let Some(w) = waker {
                w.wake();
            }
            out
        }
    }

    impl<T> UnboundedReceiver<T> {
        pub async fn recv(&mut self) -> Option<T> {
            self.0.recv().await
        }

        pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
            self.0.try_recv()
        }

        pub fn close(&mut self) {
            self.0.close()
        }

        pub fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        pub fn len(&self) -> usize {
            self.0.len()
        }
    }
}

// =========================================================================
// watch
// =========================================================================

pub mod watch {
    use super::*;

    pub mod error {
        /// The sender is gone — no further change can ever arrive.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct RecvError;

        impl std::fmt::Display for RecvError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "watch sender dropped")
            }
        }
        impl std::error::Error for RecvError {}
    }
    use error::RecvError;

    struct WatchShared<T> {
        state: Mutex<WatchState<T>>,
    }

    struct WatchState<T> {
        value: T,
        version: u64,
        tx_alive: bool,
        wakers: Vec<Waker>,
    }

    pub struct Sender<T> {
        shared: Arc<WatchShared<T>>,
    }

    pub struct Receiver<T> {
        shared: Arc<WatchShared<T>>,
        seen: u64,
    }

    pub fn channel<T>(init: T) -> (Sender<T>, Receiver<T>) {
        let shared = Arc::new(WatchShared {
            state: Mutex::new(WatchState {
                value: init,
                version: 0,
                tx_alive: true,
                wakers: Vec::new(),
            }),
        });
        (
            Sender {
                shared: shared.clone(),
            },
            Receiver { shared, seen: 0 },
        )
    }

    impl<T> Drop for Sender<T> {
        fn drop(&mut self) {
            let mut wakers = {
                let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                st.tx_alive = false;
                std::mem::take(&mut st.wakers)
            };
            wake_all(&mut wakers);
        }
    }

    impl<T> Sender<T> {
        /// Publish a new value (always succeeds — receivers may come and
        /// go; the daemon's uses are config/posture publication).
        pub fn send(&self, value: T) {
            let mut wakers = {
                let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                st.value = value;
                st.version += 1;
                std::mem::take(&mut st.wakers)
            };
            wake_all(&mut wakers);
        }

        pub fn subscribe(&self) -> Receiver<T> {
            let st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            Receiver {
                shared: self.shared.clone(),
                seen: st.version,
            }
        }
    }

    impl<T> Clone for Receiver<T> {
        fn clone(&self) -> Self {
            Receiver {
                shared: self.shared.clone(),
                seen: self.seen,
            }
        }
    }

    /// Snapshot guard over the current value.
    pub struct Ref<'a, T> {
        guard: std::sync::MutexGuard<'a, WatchState<T>>,
    }

    impl<T> std::ops::Deref for Ref<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.guard.value
        }
    }

    impl<T> Receiver<T> {
        pub fn borrow(&self) -> Ref<'_, T> {
            Ref {
                guard: self.shared.state.lock().unwrap_or_else(|e| e.into_inner()),
            }
        }

        pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
            let guard = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            self.seen = guard.version;
            Ref { guard }
        }

        /// Wait for a version newer than the last seen. Ticked park.
        pub async fn changed(&mut self) -> Result<(), RecvError> {
            let fut = ChangedFut { rx: self };
            ticked(fut).await
        }
    }

    struct ChangedFut<'a, T> {
        rx: &'a mut Receiver<T>,
    }

    impl<T> Unpin for ChangedFut<'_, T> {}

    impl<T> Future for ChangedFut<'_, T> {
        type Output = Result<(), RecvError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let seen = self.rx.seen;
            let mut st = self
                .rx
                .shared
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if st.version > seen {
                let v = st.version;
                drop(st);
                self.rx.seen = v;
                return Poll::Ready(Ok(()));
            }
            if !st.tx_alive {
                return Poll::Ready(Err(RecvError));
            }
            st.wakers.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal thread-parking block_on (the sqz_time test pattern).
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
    fn oneshot_delivers_and_close_errors() {
        let (tx, rx) = oneshot::channel::<u32>();
        std::thread::spawn(move || {
            tx.send(7).expect("receiver alive");
        });
        assert_eq!(block_on(rx), Ok(7));

        let (tx, rx) = oneshot::channel::<u32>();
        drop(tx);
        assert!(block_on(rx).is_err(), "dropped sender = RecvError");

        let (tx, rx) = oneshot::channel::<u32>();
        drop(rx);
        assert_eq!(tx.send(9), Err(9), "dead receiver hands the value back");
    }

    #[test]
    fn mpsc_bounded_backpressures_and_drains() {
        let (tx, mut rx) = mpsc::channel::<u32>(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert!(matches!(
            tx.try_send(3),
            Err(mpsc::error::TrySendError::Full(3))
        ));
        // A parked async send completes once the receiver drains.
        let tx2 = tx.clone();
        let sender = std::thread::spawn(move || block_on(tx2.send(3)));
        assert_eq!(block_on(rx.recv()), Some(1));
        sender.join().unwrap().expect("send lands after drain");
        assert_eq!(block_on(rx.recv()), Some(2));
        assert_eq!(block_on(rx.recv()), Some(3));
        drop(tx);
        assert_eq!(block_on(rx.recv()), None, "closed + drained = None");
    }

    #[test]
    fn mpsc_unbounded_and_close_semantics() {
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        rx.close();
        assert!(tx.send(3).is_err(), "closed refuses new sends");
        assert_eq!(block_on(rx.recv()), Some(1), "drain continues past close");
        assert_eq!(block_on(rx.recv()), Some(2));
        assert_eq!(block_on(rx.recv()), None);
    }

    #[test]
    fn mpsc_receiver_drop_fails_parked_senders() {
        let (tx, rx) = mpsc::channel::<u32>(1);
        tx.try_send(1).unwrap();
        let tx2 = tx.clone();
        let sender = std::thread::spawn(move || block_on(tx2.send(2)));
        std::thread::sleep(Duration::from_millis(50));
        drop(rx);
        assert!(
            sender.join().unwrap().is_err(),
            "dropped receiver must fail the parked send"
        );
    }

    #[test]
    fn watch_publishes_and_changed_fires() {
        let (tx, mut rx) = watch::channel(0u32);
        assert_eq!(*rx.borrow(), 0);
        tx.send(5);
        block_on(rx.changed()).expect("sender alive");
        assert_eq!(*rx.borrow(), 5);
        // No re-fire for an already-seen version; a concurrent publish
        // unparks the waiter.
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            tx.send(6);
        });
        block_on(rx.changed()).expect("sender alive");
        assert_eq!(*rx.borrow(), 6);
        publisher.join().unwrap();
    }
}
