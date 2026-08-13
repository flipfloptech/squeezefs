//! Scheduler-free metadata-plane lock primitives (`docs/design-sqz-sync.md`,
//! Stage 1 — user ruling 2026-08-12: the metadata plane must not depend on
//! `tokio::sync`'s wake-delivery protocol after the OQ-5 lost-wakeup wedge).
//!
//! Two properties make the wedge class unrepresentable here:
//!
//! 1. **Barging (no ownership assignment to sleeping waiters):** release
//!    flips the lock FREE and wakes the queue front; woken waiters
//!    re-contend on poll. A waiter whose wake is lost owns nothing and
//!    blocks nobody — the tokio batch-semaphore protocol assigns permits
//!    to a popped waiter first, so a never-polled assignee wedges the
//!    world; ours cannot.
//! 2. **The tick backstop lives IN the primitive:** every async acquire
//!    re-polls at [`TICK`]. Timer wakes ride the driver — a different
//!    delivery path from waker handoff — so a waiter whose own wake was
//!    lost self-heals one tick later. [`TICK_RECOVERIES`] counts
//!    engagements (exported as `lock_ticked_reregisters`): 0 on healthy
//!    schedules; growth = a lost wake absorbed, loudly.
//!
//! Fairness: FIFO wake order (all leading readers, or one writer) bounds
//! barging in practice; the tick bounds the pathological case. Cancel
//! safety: the acquire future's `Drop` unlinks its waiter — nothing is
//! ever reserved for it, so cancellation leaks nothing. The interior
//! short-hold `std::sync::Mutex` is never held across `.await` (it guards
//! a queue push/pop measured in nanoseconds).

use crate::sqz_sync_core::{LockCore, Want};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// The liveness tick — the backstop's re-poll period. Unreachable by
/// healthy waits (P1-9 holds are never device-I/O-length), cheap enough
/// that a lost wake costs one beat instead of a wedge. The same posture
/// `write_pipeline_core` documents for its admit park.
pub const TICK: Duration = Duration::from_secs(2);

/// Backstop engagements: async acquires that re-polled past a tick.
/// Each one is a wait that, on the tokio primitives, could have parked
/// forever under a lost wake. 0 on healthy schedules.
pub static TICK_RECOVERIES: AtomicU64 = AtomicU64::new(0);

/// The shipped face of the state machine: wakers are `std::task::Waker`.
/// The core returns wakers instead of calling them, so every `wake()`
/// here runs OUTSIDE the interior mutex's critical section.
type Core = LockCore<Waker>;

fn wake_all(wakers: Vec<Waker>) {
    for w in wakers {
        w.wake();
    }
}

// =========================================================================
// Acquire future (cancel-safe; Drop unlinks)
// =========================================================================

struct Acquire<'a> {
    core: &'a Core,
    want: Want,
    id: Option<u64>,
}

impl Future for Acquire<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let (granted, wake) = self.core.try_acquire(self.want, self.id);
        wake_all(wake);
        if granted {
            // Any queue entry was removed inside try_acquire.
            self.id = None;
            return Poll::Ready(());
        }
        let id = self.core.register(self.want, self.id, cx.waker());
        self.id = Some(id);
        Poll::Pending
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.core.unregister(id);
        }
    }
}

/// The ticked async acquire: race the acquire against [`TICK`]; on
/// expiry the SAME waiter re-contends (its queue position — and so its
/// FIFO fairness slot — survives the tick) — the backstop that absorbs
/// a lost wake, since a queued waiter's re-contend barges
/// unconditionally past a dead front waiter.
///
/// The bare `try_acquire` fast path keeps the uncontended acquire
/// timer-free and allocation-free (no `Sleep` registration, no
/// `Box::pin` — the ipc op-economy posture; the pre-fix shape minted a
/// timer entry per lock op and `Sleep::new` showed up in the rw5a
/// storm profile).
async fn acquire_ticked(core: &Core, want: Want) {
    let (granted, wake) = core.try_acquire(want, None);
    wake_all(wake);
    if granted {
        return;
    }
    let mut attempt = Acquire {
        core,
        want,
        id: None,
    };
    loop {
        match squeezefs_ipc::sqz_time::timeout(TICK, &mut attempt).await {
            Ok(()) => return,
            Err(_elapsed) => {
                TICK_RECOVERIES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

// =========================================================================
// SqzRwLock
// =========================================================================

/// The metadata plane's reader-writer lock (see module docs).
pub struct SqzRwLock<T: ?Sized> {
    core: Core,
    data: std::cell::UnsafeCell<T>,
}

// SAFETY: access to `data` is serialized by the Core state machine
// exactly as std/tokio RwLock serialize theirs.
unsafe impl<T: ?Sized + Send> Send for SqzRwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for SqzRwLock<T> {}

impl<T> SqzRwLock<T> {
    pub fn new(value: T) -> Self {
        SqzRwLock {
            core: Core::new(),
            data: std::cell::UnsafeCell::new(value),
        }
    }

    pub async fn read(&self) -> SqzRwLockReadGuard<'_, T> {
        acquire_ticked(&self.core, Want::Shared).await;
        SqzRwLockReadGuard { lock: self }
    }

    pub async fn write(&self) -> SqzRwLockWriteGuard<'_, T> {
        acquire_ticked(&self.core, Want::Exclusive).await;
        SqzRwLockWriteGuard { lock: self }
    }

    pub fn try_read(&self) -> Result<SqzRwLockReadGuard<'_, T>, ()> {
        let (granted, wake) = self.core.try_acquire(Want::Shared, None);
        wake_all(wake);
        if granted {
            Ok(SqzRwLockReadGuard { lock: self })
        } else {
            Err(())
        }
    }

    pub fn try_write(&self) -> Result<SqzRwLockWriteGuard<'_, T>, ()> {
        let (granted, wake) = self.core.try_acquire(Want::Exclusive, None);
        wake_all(wake);
        if granted {
            Ok(SqzRwLockWriteGuard { lock: self })
        } else {
            Err(())
        }
    }

    /// `Arc`-owned shared guard (the `DlmGuard` shape).
    pub async fn read_owned(self: Arc<Self>) -> OwnedSqzRwLockReadGuard<T> {
        acquire_ticked(&self.core, Want::Shared).await;
        OwnedSqzRwLockReadGuard { lock: self }
    }

    /// `Arc`-owned exclusive guard (the `DlmGuard` shape).
    pub async fn write_owned(self: Arc<Self>) -> OwnedSqzRwLockWriteGuard<T> {
        acquire_ticked(&self.core, Want::Exclusive).await;
        OwnedSqzRwLockWriteGuard { lock: self }
    }
}

impl<T: Default> Default for SqzRwLock<T> {
    fn default() -> Self {
        SqzRwLock::new(T::default())
    }
}

pub struct SqzRwLockReadGuard<'a, T: ?Sized> {
    lock: &'a SqzRwLock<T>,
}
pub struct SqzRwLockWriteGuard<'a, T: ?Sized> {
    lock: &'a SqzRwLock<T>,
}
pub struct OwnedSqzRwLockReadGuard<T: ?Sized> {
    lock: Arc<SqzRwLock<T>>,
}
pub struct OwnedSqzRwLockWriteGuard<T: ?Sized> {
    lock: Arc<SqzRwLock<T>>,
}

impl<T: ?Sized> Drop for SqzRwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        wake_all(self.lock.core.release_shared());
    }
}
impl<T: ?Sized> Drop for SqzRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        wake_all(self.lock.core.release_exclusive());
    }
}
impl<T: ?Sized> Drop for OwnedSqzRwLockReadGuard<T> {
    fn drop(&mut self) {
        wake_all(self.lock.core.release_shared());
    }
}
impl<T: ?Sized> Drop for OwnedSqzRwLockWriteGuard<T> {
    fn drop(&mut self) {
        wake_all(self.lock.core.release_exclusive());
    }
}

impl<T: ?Sized> std::ops::Deref for SqzRwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: shared hold — no exclusive holder exists.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> std::ops::Deref for SqzRwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: exclusive hold.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> std::ops::DerefMut for SqzRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: exclusive hold.
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T: ?Sized> std::ops::Deref for OwnedSqzRwLockReadGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: shared hold.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> std::ops::Deref for OwnedSqzRwLockWriteGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: exclusive hold.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> std::ops::DerefMut for OwnedSqzRwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: exclusive hold.
        unsafe { &mut *self.lock.data.get() }
    }
}

// =========================================================================
// SqzMutex (an exclusive-only face over the same core)
// =========================================================================

/// The metadata plane's mutex (see module docs).
pub struct SqzMutex<T: ?Sized> {
    core: Core,
    data: std::cell::UnsafeCell<T>,
}

// SAFETY: the Core state machine only ever hands out EXCLUSIVE access
// (Want::Exclusive), so `T: Send` suffices for both — the tokio
// `Mutex` bound (`Sync where T: Send`), which the rwlock cannot offer
// because its read guards alias `&T` across threads. Load-bearing for
// the rip-tokio-total sweep: cluster-wire payloads are `Send + !Sync`.
unsafe impl<T: ?Sized + Send> Send for SqzMutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for SqzMutex<T> {}

impl<T> SqzMutex<T> {
    pub fn new(value: T) -> Self {
        SqzMutex {
            core: Core::new(),
            data: std::cell::UnsafeCell::new(value),
        }
    }

    pub async fn lock(&self) -> SqzMutexGuard<'_, T> {
        acquire_ticked(&self.core, Want::Exclusive).await;
        SqzMutexGuard { lock: self }
    }

    pub fn try_lock(&self) -> Result<SqzMutexGuard<'_, T>, ()> {
        let (granted, wake) = self.core.try_acquire(Want::Exclusive, None);
        wake_all(wake);
        if granted {
            Ok(SqzMutexGuard { lock: self })
        } else {
            Err(())
        }
    }
}

impl<T: Default> Default for SqzMutex<T> {
    fn default() -> Self {
        SqzMutex::new(T::default())
    }
}

pub struct SqzMutexGuard<'a, T: ?Sized> {
    lock: &'a SqzMutex<T>,
}

// SAFETY: the guard proves exclusive ownership; sending it moves that
// exclusivity (tokio guard parity — T: Send is the only requirement).
unsafe impl<T: ?Sized + Send> Send for SqzMutexGuard<'_, T> {}
unsafe impl<T: ?Sized + Send> Sync for SqzMutexGuard<'_, T> {}

impl<T: ?Sized> Drop for SqzMutexGuard<'_, T> {
    fn drop(&mut self) {
        wake_all(self.lock.core.release_exclusive());
    }
}

impl<T: ?Sized> std::ops::Deref for SqzMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: exclusive hold.
        unsafe { &*self.lock.data.get() }
    }
}
impl<T: ?Sized> std::ops::DerefMut for SqzMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: exclusive hold.
        unsafe { &mut *self.lock.data.get() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exclusion: N tasks × M increments through the mutex — the counter
    /// is exact and never torn (multi-thread flavor exposes races).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mutex_excludes_under_contention() {
        let m = Arc::new(SqzMutex::new(0u64));
        let mut js = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let m = m.clone();
            js.spawn(async move {
                for _ in 0..1000 {
                    let mut g = m.lock().await;
                    *g += 1;
                }
            });
        }
        while js.join_next().await.is_some() {}
        assert_eq!(*m.lock().await, 8000);
    }

    /// RwLock: readers coexist, writers exclude, and a queued writer is
    /// not starved by a shared stream (write preference).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rwlock_readers_share_writers_exclude() {
        let l = Arc::new(SqzRwLock::new(0u64));
        let r1 = l.read().await;
        let r2 = l.read().await;
        assert!(l.try_write().is_err(), "readers exclude a writer");
        drop(r1);
        drop(r2);
        let w = l.write().await;
        assert!(l.try_read().is_err(), "a writer excludes readers");
        drop(w);
        let _ = l.read().await;
    }

    /// The wedge-immunity rail (THE design property): a waiter whose
    /// wake is delivered but never acted on (its task is never polled
    /// again — simulated by a raw registered waiter that we simply
    /// forget) must NOT prevent other contenders from taking the lock,
    /// and must not leak the lock when the holder releases.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dead_waiter_never_wedges_the_lock() {
        let l = Arc::new(SqzRwLock::new(()));
        // Holder takes the lock exclusively.
        let g = l.write().await;
        // A "dead" waiter registers and is never polled again (we poll
        // its acquire once to enqueue it, then LEAK the future —
        // mem::forget keeps the queue entry alive like a never-polled
        // task would).
        let dead = Box::pin(Acquire {
            core: &l.core,
            want: Want::Exclusive,
            id: None,
        });
        let mut dead = dead;
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(dead.as_mut().poll(&mut cx).is_pending());
        std::mem::forget(dead); // the never-polled task
                                // Holder releases: under the tokio protocol the lock would be
                                // ASSIGNED to the dead waiter — wedged forever. Under bounded
                                // barging a fresh contender queues behind the dead waiter
                                // (FIFO courtesy) and its TICK re-contend barges past it —
                                // one tick of latency, never a wedge.
        drop(g);
        let fresh =
            squeezefs_ipc::sqz_time::timeout(TICK * 2 + Duration::from_secs(1), l.write()).await;
        assert!(
            fresh.is_ok(),
            "a dead queued waiter must never wedge the lock (tick-bounded barging)"
        );
    }

    /// Fairness under a 100 %-duty writer (the rw5a patch-storm shape,
    /// 2026-08-13): a release→relock loop with no suspension between
    /// `drop(guard)` and the next `lock()` (whose first poll runs
    /// inline) must NOT starve queued waiters. `tokio::sync::Mutex` is
    /// fair — release hands the permit to the FIFO front and the
    /// releaser's next `lock()` queues behind it — and every call site
    /// was written against that contract; the unbounded-barging Stage-1
    /// core starved the rw5a reader cohort for 40 minutes
    /// (TICK_RECOVERIES = 1351, storm task at 80 % CPU). The law: a
    /// FRESH exclusive attempt queues behind existing waiters; only
    /// QUEUED waiters barge (the dead-waiter tick rail).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn writer_storm_never_starves_a_waiter() {
        let m = Arc::new(SqzMutex::new(0u64));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let storm = {
            let m = m.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Acquire) {
                    let mut g = m.lock().await;
                    *g += 1;
                    // Yield INSIDE the guard (the patch storm's DMA
                    // window) — the lock is held for the dominant share
                    // of every iteration and freed only nanoseconds
                    // before the inline reacquire.
                    tokio::task::yield_now().await;
                    drop(g);
                }
            })
        };
        // Four SPAWNED waiters (the reader cohort). Spawned matters:
        // the release-wake lands in the storm worker's LIFO slot, so
        // the waiter re-polls exactly while the storm sits in its
        // yield — guard HELD — and the 2 s tick re-contend races a
        // nanosecond free window. Pre-fix this loses essentially
        // forever (the live rw5a capture: 40 min, 1351 ticks).
        let mut waiters = Vec::new();
        for _ in 0..4 {
            let m = m.clone();
            waiters.push(tokio::spawn(async move {
                let _g = m.lock().await;
            }));
        }
        for (i, w) in waiters.into_iter().enumerate() {
            let won = squeezefs_ipc::sqz_time::timeout(TICK * 5, w).await;
            assert!(
                won.is_ok(),
                "waiter {i} starved by the writer storm (unbounded barging)"
            );
        }
        stop.store(true, Ordering::Release);
        storm.await.expect("storm task");
    }

    /// Tick backstop: a waiter that outlives a tick re-registers and
    /// counts the recovery; it still acquires when the lock frees.
    /// Real-time since the rip-tokio-out sweep: the tick rides
    /// `sqz_time` (our own timer thread — virtual `tokio::time::advance`
    /// cannot drive it), so this test WAITS one real tick. ~2.2 s is the
    /// price of testing the shipped backstop verbatim.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tick_backstop_reregisters_and_recovers() {
        let l = Arc::new(SqzRwLock::new(()));
        let g = l.write().await;
        let before = TICK_RECOVERIES.load(Ordering::Relaxed);
        let l2 = l.clone();
        let waiter = tokio::spawn(async move {
            let _g = l2.write().await;
        });
        squeezefs_ipc::sqz_time::sleep(TICK + Duration::from_millis(200)).await;
        assert!(
            TICK_RECOVERIES.load(Ordering::Relaxed) > before,
            "a wait past the tick must count a recovery"
        );
        drop(g);
        squeezefs_ipc::sqz_time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter completes after release")
            .expect("waiter task");
    }

    /// Cancel safety: dropping a pending acquire unlinks its waiter —
    /// the queue does not grow and later acquires see no ghost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_acquire_unlinks() {
        let l = Arc::new(SqzRwLock::new(()));
        let g = l.write().await;
        {
            let fut = l.read();
            futures::pin_mut!(fut);
            let waker = futures::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(fut.as_mut().poll(&mut cx).is_pending());
            // fut drops here — waiter unlinked.
        }
        assert_eq!(
            l.core.queue_len(),
            0,
            "a cancelled acquire must leave no queue entry"
        );
        drop(g);
        let _ = l.read().await;
    }

    /// Owned guards: the Arc-owned shapes acquire, exclude, release.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owned_guards_work() {
        let l = Arc::new(SqzRwLock::new(7u64));
        let w = l.clone().write_owned().await;
        assert!(l.try_read().is_err());
        drop(w);
        let r = l.clone().read_owned().await;
        assert_eq!(*r, 7);
    }
}
