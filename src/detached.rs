//! RES-8 (pre-RC engineering spec §7): containment for DETACHED tasks.
//!
//! The data path runs a lot of fire-and-forget work whose `JoinHandle`
//! nobody holds — by design, because the whole point is that the FUSE
//! reply does not wait for it. The write ACK path is the named anchor:
//! the completing WRITE replies with the block's custody parked and the
//! durable upload rides a detached `tpc_spawn`. Without containment a
//! panic in that task loses the block's write-back with **no counter**,
//! and because the phase histogram is recorded at the END of the task
//! body the failure surfaces as an *under-report* (fewer `Total`
//! samples) rather than as a failure — the worst possible observability
//! shape.
//!
//! Several detached bodies are LOOPS (the R5 parked-drain worker, the
//! extent-fold worker, the two dehydration workers, the rewrite-epoch
//! sweeper, the writeback requeue worker). One panic in any of them ends
//! that machinery for the life of the mount, silently.
//!
//! [`contain`] is the whole mechanism: catch the unwind, count it in
//! `detached_task_panics` (stats inode — 0 on a healthy daemon), and log
//! it loudly with the site name. It is deliberately NOT a retry: the
//! panic is a bug, and swallowing-then-retrying would turn a bug into an
//! infinite loop. Everything else here is a spawn convenience so the
//! venue rules (`tpc_spawn` = the 2026-07-26 handoff-economy law; never
//! a `Handle::spawn` onto the global inject queue from a foreign thread)
//! stay expressed exactly as before.
//!
//! `tests/detached_task_guard_tests.rs` includes a grep guard: nothing
//! outside this module may call `fuse3::raw::tpc_spawn` directly.

use std::future::Future;
use std::panic::AssertUnwindSafe;

/// Run a detached body with its unwind contained, counted and named.
///
/// `site` is a static label that appears in the log line — it is what
/// turns "something panicked somewhere" into an actionable report.
pub async fn contain<F>(site: &'static str, fut: F)
where
    F: Future<Output = ()>,
{
    if futures::FutureExt::catch_unwind(AssertUnwindSafe(fut))
        .await
        .is_err()
    {
        crate::fuse_client::METRICS
            .detached_task_panics
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        log::error!(
            "detached task '{site}' PANICKED — its work is LOST (nothing joins a \
             detached task, so this is the only record of it); counted in \
             detached_task_panics. A detached panic is always a bug: the daemon \
             keeps serving, but whatever this task owed was never delivered"
        );
    }
}

/// [`contain`] + the fuse3 per-core handler lanes — the venue every
/// kernel-lane handler runs on (the 2026-07-26 handoff-economy law: a
/// `Handle::spawn` from a foreign thread lands on the global inject
/// queue, the measured ~130 µs/op term).
pub fn tpc_spawn_guarded<F>(site: &'static str, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    fuse3::raw::tpc_spawn(contain(site, fut));
}

/// [`tpc_spawn_guarded`] with the NUMA-affinity campaign's node
/// preference (`None` / inactive placement / a node without lanes all
/// take exactly the [`tpc_spawn_guarded`] path — locality is a
/// preference, never an availability constraint).
pub fn tpc_spawn_guarded_on_node<F>(node: Option<usize>, site: &'static str, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    match node {
        Some(n) if crate::numa_core::placement_active() => {
            fuse3::raw::tpc_spawn_on_node(n, contain(site, fut))
        }
        _ => tpc_spawn_guarded(site, fut),
    }
}
