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
use squeezefs_ipc::sqz_channel::oneshot;
use std::future::Future;
use std::sync::Mutex;

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

/// **Leadership RAII** (spec §11 TEST-5/TEST-9, 2026-08-02).
///
/// `flushing` is a latch, and a latch a future can drop while holding is
/// a wedge. The module already refuses an OUTER `timeout()` for exactly
/// this reason ("would drop an inline LEADER mid-`sync_fn`, stranding
/// `flushing = true`") — but the hazard is **cancellation in general**,
/// not timeouts specifically: any dropped leader future (an aborted
/// handler task, a `select!` losing branch, teardown racing an in-flight
/// fsync) left `flushing` set forever, after which EVERY barrier on that
/// meta volume queued behind a leader that no longer exists. Measured
/// before this guard: a subsequent `barrier()` never returns
/// (`dropped_leader_never_wedges_the_volume`, red at 3 s).
///
/// On an unwound drop the guard clears the latch — the next arrival
/// elects a fresh leader — and fails the queued waiters, because their
/// barrier's outcome is genuinely unknown and an fsync must never ack on
/// an unknown barrier. Waiters already moved into the leader's local
/// batch are covered by `oneshot` drop semantics (their `rx` reports the
/// existing "leader dropped without completing barrier" error).
struct LeaderGuard<'a> {
    inner: &'a Mutex<Inner>,
    /// Cleared on every path that already reset `flushing` itself.
    armed: bool,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let drained = {
            let mut inner = self.inner.lock().unwrap();
            inner.flushing = false;
            std::mem::take(&mut inner.pending)
        };
        for waiter in drained {
            let _ = waiter.send(Err(
                "sync coalescer leader dropped mid-barrier — outcome unknown, \
                 leadership released"
                    .to_string(),
            ));
        }
    }
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
            // Leadership is a LATCH: hold it under RAII so a dropped
            // leader future can never strand it (see `LeaderGuard`).
            let mut guard = LeaderGuard {
                inner: &self.inner,
                armed: true,
            };
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
                    Some(b) => squeezefs_ipc::sqz_time::timeout(b, sync_fn()).await,
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
                        guard.armed = false; // this arm reset `flushing` itself
                        break;
                    }
                };

                for waiter in batch {
                    let _ = waiter.send(outcome.clone());
                }

                let mut inner = self.inner.lock().unwrap();
                if inner.pending.is_empty() {
                    inner.flushing = false;
                    drop(inner);
                    guard.armed = false; // clean hand-off, latch already clear
                    break;
                }
                // New waiters arrived during the barrier — serve them next.
            }
        }

        let waited = match bound {
            None => rx.await.map_err(|_| ()),
            // Follower budget: in-flight barrier remainder + own batch.
            Some(b) => match squeezefs_ipc::sqz_time::timeout(b.saturating_mul(2), rx).await {
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

    // -----------------------------------------------------------------
    // Cancellation + the bounded-wait ladder (spec §11 TEST-5/TEST-9,
    // 2026-08-02). DUR-3's reclamation watermarks ride these barriers, so
    // "the coalescer wedged" and "a caller was released by a barrier that
    // started before its write" are both data-loss-adjacent, not
    // liveness-only.
    // -----------------------------------------------------------------

    /// **Regression pin for a find.** A leader future dropped mid-`sync_fn`
    /// used to strand `flushing = true`, after which EVERY later barrier on
    /// that meta volume queued behind a leader that no longer existed —
    /// a permanent fsync wedge for the volume, the load-dependent-hang
    /// class the project treats as a first-class product bug.
    ///
    /// Red before `LeaderGuard`: the second barrier never returns (the
    /// 5 s timeout below fires).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropped_leader_never_wedges_the_volume() {
        let c = Arc::new(SyncCoalescer::new());
        let gate = Arc::new(Semaphore::new(0));
        let started = Arc::new(tokio::sync::Notify::new());

        let leader = {
            let (c, gate, started) = (c.clone(), gate.clone(), started.clone());
            tokio::spawn(async move {
                c.barrier(|| {
                    let (gate, started) = (gate.clone(), started.clone());
                    async move {
                        started.notify_one();
                        gate.acquire().await.unwrap().forget();
                        Ok(())
                    }
                })
                .await
            })
        };
        started.notified().await;
        // The cancellation: an aborted handler task, a `select!` losing
        // branch, teardown racing an in-flight fsync — all the same shape.
        leader.abort();
        let _ = leader.await;

        let c2 = c.clone();
        let after = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            c2.barrier(|| async { Ok(()) }).await
        })
        .await;
        assert!(
            after.is_ok(),
            "a dropped leader stranded `flushing` — every subsequent fsync \
             on this meta volume is wedged forever"
        );
        after
            .unwrap()
            .expect("and the fresh leader's barrier succeeds");
    }

    /// The other half: waiters already QUEUED behind a leader that then
    /// dies must be released with an error, not left parked forever. Their
    /// barrier outcome is genuinely unknown, and an fsync must never ack
    /// on an unknown barrier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn queued_waiters_are_released_when_the_leader_is_dropped() {
        let c = Arc::new(SyncCoalescer::new());
        let gate = Arc::new(Semaphore::new(0));
        let started = Arc::new(tokio::sync::Notify::new());

        let leader = {
            let (c, gate, started) = (c.clone(), gate.clone(), started.clone());
            tokio::spawn(async move {
                c.barrier(|| {
                    let (gate, started) = (gate.clone(), started.clone());
                    async move {
                        started.notify_one();
                        gate.acquire().await.unwrap().forget();
                        Ok(())
                    }
                })
                .await
            })
        };
        started.notified().await;

        // Two followers queue for the NEXT batch.
        let followers: Vec<_> = (0..2)
            .map(|_| {
                let c = c.clone();
                tokio::spawn(async move { c.barrier(|| async { Ok(()) }).await })
            })
            .collect();
        // Cooperative drain so both are certainly registered (no sleep).
        for _ in 0..500 {
            tokio::task::yield_now().await;
        }

        leader.abort();
        let _ = leader.await;

        for f in followers {
            let res = tokio::time::timeout(std::time::Duration::from_secs(5), f)
                .await
                .expect("a queued waiter must never park forever")
                .expect("task joins");
            assert!(
                res.is_err(),
                "an unknown barrier outcome must be an ERROR, never a \
                 silent ack — the caller's write may not be on stable storage"
            );
        }
    }

    /// **The crash-safety law** (module contract): a caller is released
    /// only by a `sync_fn` invocation that STARTED AFTER it registered.
    /// A caller arriving mid-barrier must therefore wait for a FRESH
    /// `sync_fn`, never piggyback on the in-flight one — piggybacking
    /// would ack an fsync whose write the completed barrier never covered.
    #[tokio::test]
    async fn a_caller_is_never_released_by_a_barrier_that_started_first() {
        let c = Arc::new(SyncCoalescer::new());
        // Monotonic "sync generation": each sync_fn invocation bumps it on
        // entry, so a waiter can name which invocation released it.
        let gen = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let mk = {
            let (gen, gate, started) = (gen.clone(), gate.clone(), started.clone());
            move || {
                let (gen, gate, started) = (gen.clone(), gate.clone(), started.clone());
                async move {
                    gen.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    gate.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        };

        let leader = {
            let (c, mk) = (c.clone(), mk.clone());
            tokio::spawn(async move { c.barrier(mk).await })
        };
        started.notified().await;
        let gen_at_registration = gen.load(Ordering::SeqCst);
        assert_eq!(gen_at_registration, 1, "invocation #1 is in flight");

        // The late caller registers NOW — strictly after invocation #1
        // started, so #1 cannot cover its write.
        let late = {
            let (c, mk) = (c.clone(), mk.clone());
            tokio::spawn(async move { c.barrier(mk).await })
        };
        for _ in 0..500 {
            tokio::task::yield_now().await;
        }
        assert!(!late.is_finished(), "the late caller must still be waiting");

        // Release invocation #1. If the law held, the late caller is STILL
        // waiting — it needs invocation #2.
        gate.add_permits(1);
        started.notified().await;
        assert_eq!(
            gen.load(Ordering::SeqCst),
            2,
            "a fresh sync_fn must start for the late caller"
        );
        assert!(
            !late.is_finished(),
            "the late caller was released by a barrier that started BEFORE \
             it registered — its write is not proven durable"
        );
        gate.add_permits(1);
        leader.await.unwrap().unwrap();
        late.await.unwrap().unwrap();
    }

    /// `barrier_bounded`: a leader whose device op never completes must
    /// synthesize a timeout for its batch AND release leadership, so the
    /// volume recovers. (The pin the module docs name.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bounded_barrier_errs_on_hung_leader_and_coalescer_recovers() {
        let c = Arc::new(SyncCoalescer::new());
        let hung = Arc::new(Semaphore::new(0));
        let res = c
            .barrier_bounded(std::time::Duration::from_millis(50), || {
                let hung = hung.clone();
                async move {
                    // Never completes within the bound.
                    hung.acquire().await.unwrap().forget();
                    Ok(())
                }
            })
            .await;
        let err = res.expect_err("a hung device barrier must not ack an fsync");
        let msg = err.to_string();
        assert!(
            msg.contains("synthesized") || msg.contains("timed out") || msg.contains("bound"),
            "the error must be a truthful synthesized timeout, got: {msg}"
        );

        // Recovery: the volume is not wedged behind the abandoned barrier.
        let ok = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            c.barrier(|| async { Ok(()) }).await
        })
        .await
        .expect("the coalescer must never wedge behind one hung barrier");
        ok.expect("and the fresh barrier succeeds");
    }

    /// A bounded leader that hangs must also fail the waiters QUEUED
    /// behind it (module docs: "the CURRENT batch **and every queued
    /// waiter**"), so nobody is left parked on a barrier that was
    /// abandoned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bounded_barrier_timeout_fails_the_queue_too() {
        let c = Arc::new(SyncCoalescer::new());
        let hung = Arc::new(Semaphore::new(0));
        let started = Arc::new(tokio::sync::Notify::new());

        let leader = {
            let (c, hung, started) = (c.clone(), hung.clone(), started.clone());
            tokio::spawn(async move {
                c.barrier_bounded(std::time::Duration::from_millis(100), || {
                    let (hung, started) = (hung.clone(), started.clone());
                    async move {
                        started.notify_one();
                        hung.acquire().await.unwrap().forget();
                        Ok(())
                    }
                })
                .await
            })
        };
        started.notified().await;
        let follower = {
            let c = c.clone();
            tokio::spawn(async move {
                c.barrier_bounded(std::time::Duration::from_millis(100), || async { Ok(()) })
                    .await
            })
        };

        assert!(
            leader.await.unwrap().is_err(),
            "the hung leader's own batch errors"
        );
        let f = tokio::time::timeout(std::time::Duration::from_secs(5), follower)
            .await
            .expect("a queued waiter must never park forever")
            .expect("task joins");
        assert!(
            f.is_err(),
            "a waiter queued behind an abandoned barrier must error, never ack"
        );
    }

    /// A dropped FOLLOWER is harmless: its dead oneshot must not disturb
    /// the leader, the batch, or any sibling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dropped_follower_never_disturbs_the_batch() {
        let c = Arc::new(SyncCoalescer::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let mk = {
            let (calls, gate, started) = (calls.clone(), gate.clone(), started.clone());
            move || {
                let (calls, gate, started) = (calls.clone(), gate.clone(), started.clone());
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    gate.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        };
        let leader = {
            let (c, mk) = (c.clone(), mk.clone());
            tokio::spawn(async move { c.barrier(mk).await })
        };
        started.notified().await;

        let doomed = {
            let (c, mk) = (c.clone(), mk.clone());
            tokio::spawn(async move { c.barrier(mk).await })
        };
        let survivor = {
            let (c, mk) = (c.clone(), mk.clone());
            tokio::spawn(async move { c.barrier(mk).await })
        };
        for _ in 0..500 {
            tokio::task::yield_now().await;
        }
        doomed.abort();
        let _ = doomed.await;

        gate.add_permits(1); // release batch 1
        started.notified().await;
        gate.add_permits(1); // release batch 2
        leader.await.unwrap().unwrap();
        survivor
            .await
            .unwrap()
            .expect("a sibling's cancellation must not fail this waiter");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "two batches, two syncs — a dropped follower changes nothing"
        );
    }

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
