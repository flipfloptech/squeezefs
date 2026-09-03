//! **READ fast-dispatch from the reap thread** (e2e perf audit R-2, read
//! board #2 — `.benchmarks/2026-09-03-4k-random-attribution.md` §4/§7).
//!
//! # The term this kills
//!
//! A fetched FUSE_READ used to travel reap thread → shared inbound queue
//! (channel push + eventfd wake) → the per-queue session dispatch task
//! (parked on the channel, polled on a `fuse3-tpc` lane) → classical
//! framing reconstruction → `handle_read` → a same-lane spawn → the
//! handler's first poll. The attribution pass measured that ingress at
//! `queue_wait` 69 µs + `dispatch_lag` 89 µs = 158 µs of a 439 µs 4 KiB
//! random read (36 %), with the device inside the op at 40 µs.
//!
//! # The engagement pair (this module — landed FIRST, step 1)
//!
//! * `transport_fast_dispatch_serves` — READs served AND committed inline
//!   on the queue worker (no channel, no wake, no lane): `queue_wait ==
//!   dispatch_lag == 0` by construction.
//! * `transport_fast_dispatch_demotes` — READs the inline probe declined,
//!   minted on the reap thread and handed straight to a `fuse3-tpc` lane
//!   (the inbound queue + session dispatch task skipped).
//!
//! `serves + demotes` ≡ the READs delivered on an armed session with the
//! lever on; both 0 until the mechanism lands and on the A/B control.

use std::sync::atomic::{AtomicU64, Ordering};

static FAST_DISPATCH_SERVES: AtomicU64 = AtomicU64::new(0);
static FAST_DISPATCH_DEMOTES: AtomicU64 = AtomicU64::new(0);

/// Count one READ served + committed inline on the reap thread.
#[inline]
pub fn note_fast_dispatch_serve() {
    FAST_DISPATCH_SERVES.fetch_add(1, Ordering::Relaxed);
}

/// Count one READ the probe declined — minted on the reap thread and
/// handed straight to a handler lane.
#[inline]
pub fn note_fast_dispatch_demote() {
    FAST_DISPATCH_DEMOTES.fetch_add(1, Ordering::Relaxed);
}

/// `transport_fast_dispatch_serves` (stats inode): on a warm row this
/// must account ≈ every warm READ; 0 on `SQUEEZEFS_FUSE_READ_FAST_DISPATCH=0`
/// mounts and before the session registers the dispatcher.
pub fn fast_dispatch_serves() -> u64 {
    FAST_DISPATCH_SERVES.load(Ordering::Relaxed)
}

/// `transport_fast_dispatch_demotes` (stats inode): see the module doc.
pub fn fast_dispatch_demotes() -> u64 {
    FAST_DISPATCH_DEMOTES.load(Ordering::Relaxed)
}
