//! The metadata plane's own sqz-exec venue (design-sqz-sync Stage 1c).
//!
//! Stage 1 moved the plane's LOCKS off tokio's wake protocol and Stage
//! 1b moved the FUSE handler venue off tokio's task delivery — but the
//! plane-critical detached tasks (the M7 conveyor pass, the checkpoint/
//! SMO task, the times drain, the publish pass) still ran as
//! `tokio::spawn` tasks on the main multi-thread runtime: a lost one
//! wedges every committer behind it with no census presence (the OQ-5
//! class, one venue up). This module gives them a first-party home:
//!
//! * A small process-global [`sqz_exec::LaneExec`] pool (`sqz-meta{N}`
//!   OS threads — 2, or 1 on a uniprocessor: the population is a
//!   handful of mostly-parked loops, not a throughput venue).
//! * Each thread enters a dedicated parked current-thread tokio runtime
//!   handle (`sqz-meta-timerdrv`) so the tasks' `tokio::time`
//!   sleeps/intervals keep working — the driver only FIRES wakers; the
//!   woken task's delivery (queue → poll) is sqz-exec, first-party by
//!   construction (the fuse3 lane pattern, deliberately NOT shared with
//!   fuse3's driver: the meta plane must not depend on a mounted
//!   transport — `fsck`/`format`/offline verbs run this backend too).
//!
//! Venue rule: metadata-plane detached tasks spawn HERE (panic-contained
//! — the RES-8 discipline); FUSE-handler-adjacent detached work keeps
//! the fuse3 lanes (`crate::detached`).

use squeezefs_ipc::sqz_exec::LaneExec;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

struct MetaExec {
    lanes: Vec<LaneExec>,
    next: AtomicUsize,
}

/// Lane population, pure form (tie-tested in the derivation sweep): 2,
/// or 1 on an EFFECTIVE uniprocessor — the population is a handful of
/// mostly-parked loops, not a throughput venue. `cpus` is the
/// fleet-share-DIVIDED sizing root (KD-MW-14 rung 3c; the retired direct
/// `available_parallelism()` read was also the Hang-1
/// pinned-first-toucher class — this Lazy can be first touched from a
/// pinned worker).
pub fn meta_lanes_from(cpus: usize) -> usize {
    if cpus > 1 {
        2
    } else {
        1
    }
}

static META_EXEC: once_cell::sync::Lazy<MetaExec> = once_cell::sync::Lazy::new(|| {
    let n = meta_lanes_from(crate::cpu::process_parallelism());
    let mut lanes = Vec::with_capacity(n);
    for i in 0..n {
        let exec = LaneExec::new();
        let ex = exec.clone();
        std::thread::Builder::new()
            .name(squeezefs_ipc::comm_core::comm_name(&format!("sqz-meta{i}")))
            .spawn(move || {
                // rip-tokio-total: no ambient runtime — timers ride the
                // sqz-timer thread, delivery rides this lane.
                ex.run();
            })
            .expect("sqz-meta lane thread spawns");
        lanes.push(exec);
    }
    MetaExec {
        lanes,
        next: AtomicUsize::new(0),
    }
});

/// Spawn a metadata-plane detached task onto the sqz-meta pool,
/// panic-contained and counted (`detached_task_panics` — RES-8: a
/// panic here is a bug and the record of lost work; the lane survives).
/// [`spawn_meta`] under the `contain(site, fut)` call shape the swapped
/// `tokio::spawn(contain(..))` sites used — same behavior (spawn_meta
/// already contains).
pub fn spawn_meta_contained<F>(site: &'static str, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    spawn_meta(site, fut);
}

pub fn spawn_meta<F>(site: &'static str, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let idx = META_EXEC.next.fetch_add(1, Ordering::Relaxed) % META_EXEC.lanes.len();
    META_EXEC.lanes[idx].spawn(crate::detached::contain(site, fut));
}

/// A joinable spawn (the `tokio::spawn` + `JoinHandle::await` shape the
/// rip-tokio-total sweep replaces): the task runs panic-contained on
/// the sqz-meta pool, and the returned receiver yields `Ok(T)` on
/// completion or `Err(RecvError)` if the task unwound (the JoinError
/// face — the panic itself is already counted by `detached_task_panics`
/// via `contain`).
pub fn spawn_meta_join<F, T>(
    site: &'static str,
    fut: F,
) -> squeezefs_ipc::sqz_channel::oneshot::Receiver<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
    spawn_meta(site, async move {
        let out = fut.await;
        let _ = tx.send(out);
    });
    rx
}

/// Drop-guarded completion signal for JOINED plane tasks (checkpoint /
/// times drain): sends `true` on a clean exit (the task's final cycle
/// ran) and `false` on an unwind — the shutdown join keeps the old
/// JoinHandle semantics (a panicked task surfaces as Corrupt, never a
/// silent skipped final checkpoint).
pub struct DoneGuard {
    tx: Option<squeezefs_ipc::sqz_channel::oneshot::Sender<bool>>,
    completed: bool,
}

impl DoneGuard {
    pub fn new(tx: squeezefs_ipc::sqz_channel::oneshot::Sender<bool>) -> Self {
        DoneGuard {
            tx: Some(tx),
            completed: false,
        }
    }

    /// Mark the task's body as having run to completion.
    pub fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(self.completed);
        }
    }
}
