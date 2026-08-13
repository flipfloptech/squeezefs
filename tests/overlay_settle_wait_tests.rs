//! The overlay settle wait's **event-park contract** (ACK-early wedge
//! fix, 2026-08-13 — docs/design-sqz-sync.md §A/B conviction; the
//! generic/464-at-3.0GHz repro-port).
//!
//! Field root cause: `await_overlay_inflight` was a `yield_now()` spin.
//! The write handler runs as a FUSED task polled by the transport queue
//! worker's own pass interleave, so a spinning settle SELF-WOKE forever
//! and starved the very pass that pumps the ZcStore `WorkerMsg` and
//! reaps the store CQE whose ticket the settle waits on — the live-gdb
//! capture caught the worker thread inside `yield_now::poll` under
//! `settle_overlay_block_locked` while same-block writers convoyed
//! behind the held stripe for 400+ s. Counted verdict of the fix:
//! generic/464 ×10 green at 3.0 GHz boost-off with ACK-early ON (the
//! posture that wedged run 1 of nearly every pre-fix count).
//!
//! Contracts:
//! 1. **Parks and wakes**: a waiter on a live ticket completes promptly
//!    once `complete_store` fires and `inflight_change` is notified
//!    (the `complete_store_and_wake` wrapper's law).
//! 2. **Never spins**: while the ticket is in flight the waiter's poll
//!    count stays bounded by the 100 ms belt cadence — a `yield_now`
//!    regression polls thousands of times per second and fails the
//!    bound loudly.
//! 3. **The belt**: a completion whose wake is LOST (complete_store
//!    without the notify — the missed-site simulation) still exits
//!    within a few belt ticks, never a wedge.

use squeezefs::fuse_client::await_overlay_inflight_on;
use squeezefs::overlay_core::OverlayRecordCore;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BLOCK: u32 = 4 * 1024 * 1024;

/// Poll-counting wrapper: counts how many times the inner future is
/// polled (the spin detector).
struct PollCounted<F> {
    inner: std::pin::Pin<Box<F>>,
    polls: Arc<AtomicU64>,
}

impl<F: std::future::Future> std::future::Future for PollCounted<F> {
    type Output = F::Output;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<F::Output> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        self.inner.as_mut().poll(cx)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settle_wait_parks_and_wakes_on_complete_store() {
    let core = Arc::new(OverlayRecordCore::new(BLOCK, 1));
    let change = Arc::new(squeezefs_ipc::sqz_notify::Notify::new());
    let ticket = core.begin_store(0, 8).expect("fresh overlay accepts");

    let polls = Arc::new(AtomicU64::new(0));
    let waiter = {
        let core = core.clone();
        let change = change.clone();
        let polls = polls.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            PollCounted {
                inner: Box::pin(async move {
                    await_overlay_inflight_on(&core, &change).await;
                }),
                polls,
            }
            .await;
            t0.elapsed()
        })
    };

    // Hold the ticket in flight for 300 ms: the waiter must PARK (poll
    // count bounded by the belt cadence), not spin.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let polls_inflight = polls.load(Ordering::Relaxed);
    assert!(
        polls_inflight <= 20,
        "the settle wait must event-park, not spin: {polls_inflight} polls in \
         300 ms of held ticket (a yield_now regression polls thousands of \
         times — the 464 wedge's starvation engine)"
    );

    // The wrapper's law: complete + notify → prompt completion.
    let _ = core.complete_store(ticket, true);
    change.notify_waiters();
    let waited = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("settle wait must complete promptly after the wake")
        .expect("waiter task");
    assert!(
        waited >= Duration::from_millis(250),
        "the waiter must actually have waited out the held window"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settle_wait_belt_survives_a_lost_wake() {
    let core = Arc::new(OverlayRecordCore::new(BLOCK, 1));
    let change = Arc::new(squeezefs_ipc::sqz_notify::Notify::new());
    let ticket = core.begin_store(0, 8).expect("fresh overlay accepts");

    let waiter = {
        let core = core.clone();
        let change = change.clone();
        tokio::spawn(async move {
            await_overlay_inflight_on(&core, &change).await;
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The MISSED-SITE simulation: complete WITHOUT the notify. The
    // 100 ms belt must still exit — a lost wake costs ticks, never a
    // wedge (the bounded-outcome posture the whole campaign enforces).
    let _ = core.complete_store(ticket, true);
    tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("the belt must exit a lost-wake settle within ticks, never wedge")
        .expect("waiter task");
}
