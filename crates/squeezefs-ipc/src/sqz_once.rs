//! First-party async once-cell (the rip-tokio-total program) — the
//! `tokio::sync::OnceCell::get_or_init` shape the daemon uses (the
//! zcrx-lane arm slot, the meta-ship dedup window's await-the-winner
//! slot). One leader runs the init future; concurrent callers park on
//! an [`crate::sqz_flight`]-style completion; a leader whose future is
//! CANCELLED (dropped mid-init) releases leadership so a parked caller
//! re-elects (tokio parity). Parks ride the ticked backstop.

use std::future::Future;
use std::sync::{Mutex, OnceLock};

use crate::sqz_notify::Notify;

pub struct OnceCell<T> {
    /// The initialized value (write-once; lock-free reads after init).
    value: OnceLock<T>,
    /// Leadership latch for the init race.
    busy: Mutex<bool>,
    /// Parked contenders (losers of the leadership race).
    changed: Notify,
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> OnceCell<T> {
    pub const fn new() -> Self {
        OnceCell {
            value: OnceLock::new(),
            busy: Mutex::new(false),
            changed: Notify::new(),
        }
    }

    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    /// Get, or run `init` exactly once across all concurrent callers.
    pub async fn get_or_init<F, Fut>(&self, init: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let mut init = Some(init);
        loop {
            if let Some(v) = self.value.get() {
                return v;
            }
            // Leadership election.
            let leader = {
                let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
                if *busy {
                    false
                } else {
                    *busy = true;
                    true
                }
            };
            if leader {
                // A cancelled leader must release the latch (tokio
                // parity: a parked caller re-elects).
                struct Unbusy<'a, T>(&'a OnceCell<T>);
                impl<T> Drop for Unbusy<'_, T> {
                    fn drop(&mut self) {
                        *self.0.busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
                        self.0.changed.notify_waiters();
                    }
                }
                let _un = Unbusy(self);
                let f = init.take().expect("leader elected once with the closure");
                let v = f().await;
                let _ = self.value.set(v);
                // _un drops: latch released + waiters woken (they read
                // the now-set value).
                if let Some(v) = self.value.get() {
                    return v;
                }
                unreachable!("value set by the leader above");
            }
            // Loser: park for a change (init done or leader cancelled),
            // enable-then-recheck so the completion cannot be lost.
            let mut park = self.changed.notified_raw();
            park.enable();
            if self.value.get().is_some() || !*self.busy.lock().unwrap_or_else(|e| e.into_inner()) {
                continue;
            }
            crate::sqz_channel::ticked(park).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn init_runs_exactly_once_across_racers() {
        let cell = Arc::new(OnceCell::<u64>::new());
        let inits = Arc::new(AtomicUsize::new(0));
        let mut joins = Vec::new();
        for _ in 0..8 {
            let cell = cell.clone();
            let inits = inits.clone();
            joins.push(std::thread::spawn(move || {
                crate::sqz_blocking::block_on(async {
                    *cell
                        .get_or_init(|| async {
                            inits.fetch_add(1, Ordering::SeqCst);
                            crate::sqz_time::sleep(std::time::Duration::from_millis(20)).await;
                            42u64
                        })
                        .await
                })
            }));
        }
        for j in joins {
            assert_eq!(j.join().unwrap(), 42);
        }
        assert_eq!(inits.load(Ordering::SeqCst), 1, "exactly one init ran");
    }

    #[test]
    fn cancelled_leader_releases_leadership() {
        let cell = Arc::new(OnceCell::<u64>::new());
        // Leader whose init never completes — poll it once, then DROP it.
        {
            let fut = cell.get_or_init(|| std::future::pending::<u64>());
            let mut fut = Box::pin(fut);
            let waker = futures_noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(matches!(
                std::pin::Pin::new(&mut fut).poll(&mut cx),
                std::task::Poll::Pending
            ));
            // fut drops here — leadership released.
        }
        // A successor must be able to init.
        let v = crate::sqz_blocking::block_on(cell.get_or_init(|| async { 7u64 }));
        assert_eq!(*v, 7);
    }

    fn futures_noop_waker() -> std::task::Waker {
        use std::sync::Arc;
        use std::task::Wake;
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        std::task::Waker::from(Arc::new(Noop))
    }
}
