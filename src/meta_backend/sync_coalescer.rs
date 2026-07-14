//! Group-commit `fdatasync` coalescer for a single meta volume.
//!
//! Many concurrent `fsync`s on the same device each need a durability barrier,
//! but one `fdatasync` flushes *all* pending device writes. So a single barrier
//! can satisfy every caller whose write completed before that barrier started —
//! classic journal group commit (jbd2-style).
//!
//! ## Correctness contract
//!
//! A caller invokes [`SyncCoalescer::barrier`] **after** its data write has
//! completed. A caller is released only by a `sync_fn` invocation that *started
//! after the caller registered* (i.e. strictly after the caller's write). This
//! preserves crash safety: the completed barrier guarantees the caller's write
//! is on stable storage.
//!
//! ## Algorithm
//!
//! * Every caller registers a `oneshot` waiter under a short mutex.
//! * If no barrier is in flight, the caller becomes the *leader*: it repeatedly
//!   takes the pending batch, runs one `sync_fn`, and fans the result out to the
//!   whole batch. Callers that arrive *during* a barrier land in the next batch,
//!   so they are served by a fresh `sync_fn` that starts after they registered.
//! * The leader stops once it drains a batch and finds no new waiters.
//!
//! The mutex is only ever held for O(1) list pushes/takes — never across the
//! `sync_fn` await — so it is a plain `std::sync::Mutex`.

use crate::error::{Result, SqueezefsError};
use std::future::Future;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// Outcome fanned out to every waiter in a batch. `SqueezefsError` is not
/// `Clone`, so failures are carried as a rendered string and re-wrapped per
/// waiter.
type BatchOutcome = std::result::Result<(), String>;

#[derive(Default)]
struct Inner {
    /// A barrier is currently in flight (a leader is running).
    flushing: bool,
    /// Waiters queued for the next `sync_fn` invocation.
    pending: Vec<oneshot::Sender<BatchOutcome>>,
}

/// Per-device group-commit barrier.
#[derive(Default)]
pub struct SyncCoalescer {
    inner: Mutex<Inner>,
}

impl SyncCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// [`Self::barrier`] with a bounded wait (PR M4 D1.b audit row 1:
    /// FLUSH/FSYNC-class barrier waits keep a synthesized error for
    /// userspace liveness on a sick device). The bound MUST live inside
    /// the coalescer: an outer `timeout()` would drop an inline LEADER
    /// mid-`sync_fn`, stranding `flushing = true` and wedging every later
    /// barrier on the volume.
    ///
    /// Semantics:
    /// - **Leader**: each batch's `sync_fn` is raced against `bound`. On
    ///   expiry the in-flight device op is abandoned to its owner (the
    ///   `uring_fs` worker completes it harmlessly — dropping that future
    ///   holds no budget), the CURRENT batch **and every queued waiter**
    ///   receive a synthesized timeout error, `flushing` resets, and the
    ///   leader returns — the next arrival elects a fresh leader with its
    ///   own bound. The coalescer can therefore never wedge behind one
    ///   hung barrier (pinned by
    ///   `bounded_barrier_errs_on_hung_leader_and_coalescer_recovers`).
    /// - **Follower**: the result wait is bounded by `2 × bound` (worst
    ///   case: the remainder of the in-flight barrier + its own batch's
    ///   barrier, each ≤ `bound` by the leader rule). On expiry the
    ///   follower synthesizes the same error; the leader's later send
    ///   into the dead oneshot is harmless.
    /// - The synthesized error carries `ETIMEDOUT` so callers (fsync)
    ///   surface a truthful errno. Escalation truth (the barrier-failure
    ///   rungs) stays with REAL `sync_fn` outcomes — an abandoned
    ///   barrier's outcome is unknown and deliberately not counted.
    pub async fn barrier_bounded<F, Fut>(
        &self,
        bound: std::time::Duration,
        sync_fn: F,
    ) -> Result<()>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.barrier_inner(Some(bound), sync_fn).await
    }

    /// Request a durability barrier, coalescing with any concurrent requests.
    ///
    /// `sync_fn` performs the actual device barrier (e.g. `fdatasync`). It is
    /// invoked once per batch by the leader; callers piggyback on the shared
    /// result. The caller must already have persisted its write before calling.
    pub async fn barrier<F, Fut>(&self, sync_fn: F) -> Result<()>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.barrier_inner(None, sync_fn).await
    }

    async fn barrier_inner<F, Fut>(
        &self,
        bound: Option<std::time::Duration>,
        sync_fn: F,
    ) -> Result<()>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let (tx, rx) = oneshot::channel();

        let is_leader = {
            let mut inner = self.inner.lock().unwrap();
            inner.pending.push(tx);
            if inner.flushing {
                false
            } else {
                inner.flushing = true;
                true
            }
        };

        if is_leader {
            loop {
                // Take the current batch (always non-empty: the first iteration
                // contains at least this leader; later iterations only run when
                // new waiters exist).
                let batch = {
                    let mut inner = self.inner.lock().unwrap();
                    std::mem::take(&mut inner.pending)
                };

                let raced = match bound {
                    None => Ok(sync_fn().await),
                    Some(b) => tokio::time::timeout(b, sync_fn()).await,
                };
                let outcome: BatchOutcome = match raced {
                    Ok(res) => res.map_err(|e| e.to_string()),
                    Err(_elapsed) => {
                        // Bounded-out barrier: fail this batch AND the queue,
                        // reset flushing, stop leading (audit row 1).
                        let msg = format!(
                            "device barrier exceeded its bound ({} ms) — synthesized \
                             timeout (D1.b bounded barrier wait; the device op was \
                             abandoned to its uring worker)",
                            bound.map(|b| b.as_millis()).unwrap_or_default()
                        );
                        let drained = {
                            let mut inner = self.inner.lock().unwrap();
                            inner.flushing = false;
                            std::mem::take(&mut inner.pending)
                        };
                        for waiter in batch.into_iter().chain(drained) {
                            let _ = waiter.send(Err(msg.clone()));
                        }
                        break;
                    }
                };

                for waiter in batch {
                    let _ = waiter.send(outcome.clone());
                }

                let mut inner = self.inner.lock().unwrap();
                if inner.pending.is_empty() {
                    inner.flushing = false;
                    break;
                }
                // New waiters arrived during the barrier — serve them next.
            }
        }

        let waited = match bound {
            None => rx.await.map_err(|_| ()),
            // Follower budget: in-flight barrier remainder + own batch.
            Some(b) => match tokio::time::timeout(b.saturating_mul(2), rx).await {
                Ok(res) => res.map_err(|_| ()),
                Err(_elapsed) => {
                    return Err(SqueezefsError::Io(std::io::Error::from_raw_os_error(
                        libc::ETIMEDOUT,
                    )));
                }
            },
        };
        match waited {
            Ok(Ok(())) => Ok(()),
            Ok(Err(msg)) => {
                // Synthesized-timeout outcomes carry a truthful errno for
                // the fsync surface; real sync errors stay generic I/O.
                if msg.contains("synthesized") {
                    Err(SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        msg,
                    )))
                } else {
                    Err(SqueezefsError::Io(std::io::Error::other(msg)))
                }
            }
            Err(()) => Err(SqueezefsError::InvalidOperation(
                "sync coalescer leader dropped without completing barrier".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    /// A lone barrier runs exactly one `sync_fn`.
    #[tokio::test]
    async fn test_single_barrier_runs_one_sync() {
        let c = SyncCoalescer::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        c.barrier(|| {
            let calls = calls2.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Serialized barriers do not coalesce — one `sync_fn` each.
    #[tokio::test]
    async fn test_sequential_barriers_do_not_coalesce() {
        let c = SyncCoalescer::new();
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let calls2 = calls.clone();
            c.barrier(|| {
                let calls = calls2.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    /// Deterministic coalescing: one in-flight leader, 49 followers pile into a
    /// single next batch -> exactly two `sync_fn` invocations for 50 barriers.
    /// Runs on the single-threaded runtime so scheduling is cooperative and the
    /// assertion is exact (no timing).
    #[tokio::test]
    async fn test_concurrent_barriers_coalesce_deterministically() {
        let c = Arc::new(SyncCoalescer::new());
        let calls = Arc::new(AtomicUsize::new(0));
        // 0 permits: sync_fn blocks until the test releases it.
        let gate = Arc::new(Semaphore::new(0));
        let started = Arc::new(tokio::sync::Notify::new());

        let mk_sync = {
            let calls = calls.clone();
            let gate = gate.clone();
            let started = started.clone();
            move || {
                let calls = calls.clone();
                let gate = gate.clone();
                let started = started.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    gate.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        };

        // Leader A: registers, becomes leader, runs sync_fn (calls==1), blocks.
        let a = {
            let c = c.clone();
            let mk_sync = mk_sync.clone();
            tokio::spawn(async move { c.barrier(mk_sync).await })
        };
        started.notified().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "leader mid-barrier");

        // 49 followers register while A is blocked; they all queue for batch 2.
        let mut followers = Vec::new();
        for _ in 0..49 {
            let c = c.clone();
            let mk_sync = mk_sync.clone();
            followers.push(tokio::spawn(async move { c.barrier(mk_sync).await }));
        }
        // Drain the ready queue so every follower registers (cooperative, not timed).
        for _ in 0..500 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no second sync until leader is released"
        );

        // Release batch 1 (A). Leader then serves the 49 followers as batch 2.
        gate.add_permits(1);
        started.notified().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "followers share one sync");
        gate.add_permits(1);

        a.await.unwrap().unwrap();
        for f in followers {
            f.await.unwrap().unwrap();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "50 barriers coalesced into exactly 2 syncs"
        );
    }

    /// A failing `sync_fn` propagates the error to every waiter in the batch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_error_propagates_to_all_waiters() {
        let c = Arc::new(SyncCoalescer::new());
        let mut handles = Vec::new();
        for _ in 0..32 {
            let c = c.clone();
            handles.push(tokio::spawn(async move {
                c.barrier(|| async { Err(SqueezefsError::InvalidOperation("boom".to_string())) })
                    .await
            }));
        }
        for h in handles {
            let res = h.await.unwrap();
            assert!(res.is_err(), "every waiter must observe the sync failure");
        }
    }

    /// Stress: many concurrent barriers on a multi-thread runtime all succeed,
    /// and coalescing never runs more syncs than callers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_stress_all_succeed_and_never_over_sync() {
        let c = Arc::new(SyncCoalescer::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let n = 500usize;
        let mut handles = Vec::new();
        for _ in 0..n {
            let c = c.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                c.barrier(|| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok(())
                    }
                })
                .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        let total = calls.load(Ordering::SeqCst);
        assert!(total >= 1 && total <= n, "sync count {total} out of range");
    }
}
