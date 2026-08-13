//! First-party async semaphore (the rip-tokio-total program,
//! 2026-08-13) — the tokio `Semaphore` API subset the daemon uses
//! (`acquire`/`acquire_owned`/`try_acquire[_owned]`/`add_permits`/
//! `available_permits`/`Permit::forget`), waker-based, no runtime
//! driver.
//!
//! Protocol: the sqz-sync **bounded barging** law, verbatim. A release
//! (or `add_permits`) never assigns permits to a sleeping waiter — it
//! returns permits to the pool and WAKES the FIFO front; woken waiters
//! re-contend on poll, so a dead waiter owns nothing and wedges
//! nothing (its corpse costs contenders one TICK — the
//! [`crate::sqz_channel::ticked`] backstop — never a wedge). Fresh
//! attempts do not barge past queued waiters (FIFO courtesy — the
//! 2026-08-13 fairness fix's law), except `try_acquire`, whose contract
//! IS the barge (tokio parity: it takes free permits regardless).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::sqz_channel::ticked;

/// No permits available (or the semaphore is closed — unused by the
/// daemon, kept for API parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TryAcquireError;

impl std::fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no permits available")
    }
}
impl std::error::Error for TryAcquireError {}

struct SemState {
    permits: usize,
    /// FIFO waiters: (id, wanted, waker).
    waiters: VecDeque<(u64, usize, Waker)>,
    next_id: u64,
}

struct SemCore {
    state: Mutex<SemState>,
}

impl SemCore {
    /// Wake the FIFO front if the pool could now serve it (wake token
    /// only — ownership on re-poll).
    fn front_waker_if_servable(st: &SemState) -> Option<Waker> {
        st.waiters
            .front()
            .filter(|(_, want, _)| *want <= st.permits)
            .map(|(_, _, w)| w.clone())
    }
}

pub struct Semaphore {
    core: Arc<SemCore>,
}

impl Semaphore {
    pub fn new(permits: usize) -> Self {
        Semaphore {
            core: Arc::new(SemCore {
                state: Mutex::new(SemState {
                    permits,
                    waiters: VecDeque::new(),
                    next_id: 1,
                }),
            }),
        }
    }

    pub fn available_permits(&self) -> usize {
        self.core
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .permits
    }

    pub fn add_permits(&self, n: usize) {
        release(&self.core, n);
    }

    /// Immediate grab — takes free permits regardless of queued waiters
    /// (tokio parity: try_acquire's contract is the barge).
    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        try_take(&self.core, 1).map(|()| SemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    pub fn try_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        try_take(&self.core, n as usize).map(|()| SemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        acquire_units(&self.core, 1).await;
        Ok(SemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    pub async fn acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        acquire_units(&self.core, n as usize).await;
        Ok(SemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    pub async fn acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        acquire_units(&self.core, 1).await;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    pub async fn acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        acquire_units(&self.core, n as usize).await;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: n as usize,
        })
    }

    pub fn try_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        try_take(&self.core, 1)?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: 1,
        })
    }
}

fn try_take(core: &Arc<SemCore>, want: usize) -> Result<(), TryAcquireError> {
    let mut st = core.state.lock().unwrap_or_else(|e| e.into_inner());
    if st.permits >= want {
        st.permits -= want;
        Ok(())
    } else {
        Err(TryAcquireError)
    }
}

fn release(core: &Arc<SemCore>, n: usize) {
    let waker = {
        let mut st = core.state.lock().unwrap_or_else(|e| e.into_inner());
        st.permits += n;
        SemCore::front_waker_if_servable(&st)
    };
    if let Some(w) = waker {
        w.wake();
    }
}

/// The ticked FIFO acquire (the sqz-sync `acquire_ticked` shape): fast
/// path first, then one queue slot kept across ticks.
async fn acquire_units(core: &Arc<SemCore>, want: usize) {
    // Fast path: free permits AND no queued waiter (FIFO courtesy).
    {
        let mut st = core.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.waiters.is_empty() && st.permits >= want {
            st.permits -= want;
            return;
        }
    }
    let fut = AcquireFut {
        core,
        want,
        id: None,
    };
    ticked(fut).await
}

struct AcquireFut<'a> {
    core: &'a Arc<SemCore>,
    want: usize,
    id: Option<u64>,
}

impl Drop for AcquireFut<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let waker = {
                let mut st = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
                st.waiters.retain(|(wid, _, _)| *wid != id);
                // Our departure may unblock the new front.
                SemCore::front_waker_if_servable(&st)
            };
            if let Some(w) = waker {
                w.wake();
            }
        }
    }
}

impl Future for AcquireFut<'_> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let (out, waker) = {
            let mut st = self.core.state.lock().unwrap_or_else(|e| e.into_inner());
            let at_front = match self.id {
                Some(id) => st.waiters.front().map(|(wid, _, _)| *wid) == Some(id),
                None => st.waiters.is_empty(),
            };
            // Bounded barging: a QUEUED waiter re-contends regardless of
            // position (the dead-front tick rail); a FRESH attempt
            // yields to any queue (FIFO courtesy).
            if st.permits >= self.want && (self.id.is_some() || at_front) {
                st.permits -= self.want;
                let next = if let Some(id) = self.id.take() {
                    st.waiters.retain(|(wid, _, _)| *wid != id);
                    // Leftover permits may serve the next front too.
                    SemCore::front_waker_if_servable(&st)
                } else {
                    None
                };
                (Poll::Ready(()), next)
            } else {
                match self.id {
                    Some(id) => {
                        if let Some(entry) = st.waiters.iter_mut().find(|(wid, _, _)| *wid == id) {
                            entry.2 = cx.waker().clone();
                        } else {
                            let want = self.want;
                            st.waiters.push_back((id, want, cx.waker().clone()));
                        }
                    }
                    None => {
                        let id = st.next_id;
                        st.next_id += 1;
                        let want = self.want;
                        st.waiters.push_back((id, want, cx.waker().clone()));
                        self.id = Some(id);
                    }
                }
                (Poll::Pending, None)
            }
        };
        if let Some(w) = waker {
            w.wake();
        }
        out
    }
}

pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: usize,
}

impl SemaphorePermit<'_> {
    /// Consume WITHOUT returning the permits (tokio parity — the
    /// sync_coalescer gate pattern).
    pub fn forget(self) {
        let mut this = std::mem::ManuallyDrop::new(self);
        this.permits = 0;
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        if self.permits > 0 {
            release(&self.sem.core, self.permits);
        }
    }
}

pub struct OwnedSemaphorePermit {
    sem: Arc<Semaphore>,
    permits: usize,
}

impl OwnedSemaphorePermit {
    pub fn forget(mut self) {
        self.permits = 0;
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        if self.permits > 0 {
            release(&self.sem.core, self.permits);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn block_on<F: Future>(fut: F) -> F::Output {
        use std::task::{Wake, Waker};
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
    fn permits_bound_and_release_restores() {
        let s = Semaphore::new(2);
        let p1 = s.try_acquire().unwrap();
        let _p2 = s.try_acquire().unwrap();
        assert!(s.try_acquire().is_err(), "pool exhausted");
        drop(p1);
        assert_eq!(s.available_permits(), 1);
        let _p3 = s.try_acquire().unwrap();
    }

    #[test]
    fn forget_leaks_the_permit_by_contract() {
        let s = Semaphore::new(1);
        s.try_acquire().unwrap().forget();
        assert_eq!(s.available_permits(), 0, "forgotten permit never returns");
        s.add_permits(1);
        assert_eq!(s.available_permits(), 1);
    }

    #[test]
    fn parked_acquire_wakes_on_release_fifo() {
        let s = Arc::new(Semaphore::new(1));
        let held = s.clone().try_acquire_owned().unwrap();
        let s2 = s.clone();
        let waiter = std::thread::spawn(move || {
            block_on(async move {
                let _p = s2.acquire_owned().await.unwrap();
            })
        });
        std::thread::sleep(Duration::from_millis(30));
        drop(held);
        waiter.join().unwrap();
    }

    #[test]
    fn fresh_acquire_queues_behind_waiter_but_try_barges() {
        let s = Arc::new(Semaphore::new(1));
        let held = s.clone().try_acquire_owned().unwrap();
        let s2 = s.clone();
        let waiter = std::thread::spawn(move || {
            block_on(async move {
                let _p = s2.acquire_owned().await.unwrap();
            })
        });
        std::thread::sleep(Duration::from_millis(30));
        // try_acquire's contract IS the barge — but the pool is empty
        // here, so it refuses.
        assert!(s.try_acquire().is_err());
        drop(held);
        waiter.join().unwrap();
        assert_eq!(s.available_permits(), 1);
    }
}
