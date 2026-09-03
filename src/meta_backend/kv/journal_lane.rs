//! The per-volume **journal lane** (e2e perf audit C-2 — DLM board #3,
//! `.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`): one OS thread
//! per writable volume that runs the commit conveyor's two stages AND owns
//! the volume's journal io_uring, parking IN that ring.
//!
//! **Why a lane per volume, and why it owns the ring.** D-2 left the
//! journal ring write's round trip as THE term on the co-located fleet —
//! `journal_ring_write` 1.0–1.3 ms mean / 50–200 µs mode for a ~1 KiB
//! page-cache write — and `uring_fs_write_phase_ns` split it into thread
//! hops: the pass pushed the write onto the process pool's queue (a
//! parked worker woke), the worker submitted and waited in its ring (the
//! kernel's io-wq punt woke it back), the worker's oneshot woke the
//! durability task onto one of the two `sqz-meta` lanes it shared with
//! the owner's ~7 k/s shipped-verb serves, the checkpoint task and every
//! other metadata-plane loop — and there it waited behind whatever was
//! queued ahead (`wake_hop` 66 µs of a 116 µs quiet round trip; 923 of
//! 1,206 under lane + box load; behind a 2 ms serve burst, 2.9 ms).
//!
//! On this lane: stage A submits the window's entries on the lane's OWN
//! ring (`uring_fs::OwnedReactor::submit_write_at_batch` — admit +
//! `submit()`, no wait, no queue hop); the lane parks in the ring
//! (`park`: `io_uring_enter` with the wake-eventfd `READ` SQE armed, so a
//! task wake and a device completion are ONE wait); and the lane reaps its
//! own CQEs (`service`, once per loop iteration) — the completion's oneshot
//! fires on the very thread that polls the durability task next, so the
//! `wake_hop` is a queue push on the same thread, never a scheduler wake,
//! never a run-queue wait behind the serve plane. The kernel's own two
//! wakes (io-wq punt, ring-wait return) are what remains.
//!
//! **Lane count is derived**: one per volume a writable backend opens (a
//! reader / co-writer / peer-owned volume commits nothing and opens no
//! lane); the population is the volume set's, never a constant.
//! **A/B lever**: `SQUEEZEFS_JOURNAL_LANE=0` keeps the shipped shape (both
//! stages on the shared `sqz-meta` pool, writes through the process pool)
//! — the same-binary control the field rows compare against.
//! **Degrade loud**: a box that cannot give the lane a ring (no io_uring)
//! runs the stages on a plain condvar-parked lane with the pool path,
//! logged once — the isolation half of the fix survives, the ring half
//! does not.

use crate::uring_fs::{OwnedReactor, OwnedRing, RingWake, WriteCompletion};
use squeezefs_ipc::sqz_exec::{LaneExec, LanePark};
use std::cell::RefCell;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// SQ depth of a volume's journal ring: a window's entries (≤ the commit
/// batch cap, each one or two segment SQEs plus its owned page headers)
/// plus the wake `READ`; the `uring_fs` pool's depth, whose SQ-full arm
/// (`submit()` then push again) also covers a larger chained batch.
const JOURNAL_RING_ENTRIES: u32 = 512;

/// Lanes spawned (`journal_lanes_spawned`).
pub static JOURNAL_LANES_SPAWNED: AtomicU64 = AtomicU64::new(0);
/// Lanes that had to run without a ring (`journal_lanes_ringless` — the
/// degrade-loud arm; 0 on any io_uring box).
pub static JOURNAL_LANES_RINGLESS: AtomicU64 = AtomicU64::new(0);
/// Journal ring writes submitted on a lane's OWN ring
/// (`journal_ring_lane_writes` — the engagement instrument: on a
/// lane-armed mount this accounts for every conveyor window).
pub static JOURNAL_RING_LANE_WRITES: AtomicU64 = AtomicU64::new(0);
/// Journal ring writes that went through the process pool
/// (`journal_ring_pool_writes` — the shipped path: every write on a
/// `SQUEEZEFS_JOURNAL_LANE=0` mount, a ringless lane, or a submission from
/// off the lane thread).
pub static JOURNAL_RING_POOL_WRITES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The owning lane thread's reactor (None on every other thread).
    static LANE_REACTOR: RefCell<Option<OwnedReactor>> = const { RefCell::new(None) };
}

/// The lane's parker: idle waits are `io_uring_enter` on the thread-local
/// reactor; unparks bump its eventfd; the per-iteration hook reaps.
struct RingPark {
    wake: RingWake,
}

impl LanePark for RingPark {
    fn park(&self, tick: std::time::Duration) -> bool {
        LANE_REACTOR.with(|r| {
            r.borrow_mut()
                .as_mut()
                .expect("the ring parker runs on the lane thread that owns the reactor")
                .park(tick)
        })
    }

    fn unpark(&self) {
        self.wake.wake();
    }

    fn service(&self) {
        LANE_REACTOR.with(|r| {
            if let Some(reactor) = r.borrow_mut().as_mut() {
                reactor.service();
            }
        });
    }
}

/// One volume's journal lane. Dropping it asks the lane thread to exit
/// (signal only — never joined from a drop, which may run on the lane).
pub struct JournalLane {
    exec: LaneExec,
    /// The lane thread's id: a submission is ring-native only from it.
    thread: std::thread::ThreadId,
    ringed: bool,
}

impl JournalLane {
    /// Spawn the lane for `path` (the volume's device/file). `idx` names
    /// the thread (`sqz-jrnl{idx}`).
    pub fn spawn(path: &Path, idx: usize) -> Arc<JournalLane> {
        let ring = match OwnedRing::new(JOURNAL_RING_ENTRIES) {
            Ok(r) => Some(r),
            Err(e) => {
                JOURNAL_LANES_RINGLESS.fetch_add(1, Ordering::Relaxed);
                log::error!(
                    "meta volume {}: journal lane could not create its io_uring ({e}) — the \
                     conveyor's stages run on the lane but its writes ride the process pool \
                     (journal_lanes_ringless)",
                    path.display()
                );
                None
            }
        };
        let ringed = ring.is_some();
        let exec = match ring.as_ref() {
            Some(r) => LaneExec::with_park(Arc::new(RingPark {
                wake: r.wake_handle(),
            })),
            None => LaneExec::new(),
        };
        let ex = exec.clone();
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(squeezefs_ipc::comm_core::comm_name(&format!(
                "sqz-jrnl{idx}"
            )))
            .spawn(move || {
                // The reactor's slot table is thread-bound: built HERE,
                // from the `Send` ring the spawner created.
                LANE_REACTOR.with(|r| *r.borrow_mut() = ring.map(OwnedRing::into_reactor));
                let _ = tid_tx.send(std::thread::current().id());
                ex.run();
                LANE_REACTOR.with(|r| r.borrow_mut().take());
            })
            .expect("journal lane thread spawns");
        let thread = tid_rx.recv().expect("journal lane thread reports its id");
        JOURNAL_LANES_SPAWNED.fetch_add(1, Ordering::Relaxed);
        Arc::new(JournalLane {
            exec,
            thread,
            ringed,
        })
    }

    /// Spawn a conveyor-stage task onto this lane, panic-contained and
    /// counted (the RES-8 discipline, `spawn_meta`'s).
    pub fn spawn_task<F>(&self, site: &'static str, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.exec.spawn(crate::detached::contain(site, fut));
    }

    /// Submit a journal write on the lane's own ring — `Some` only when
    /// called ON the lane thread with a ring (the apply pass, by
    /// construction); `None` = the caller must take the pool path.
    pub fn submit_write_at_batch(
        &self,
        path: &Path,
        ops: &mut Option<Vec<(u64, bytes::Bytes)>>,
    ) -> Option<WriteCompletion> {
        if !self.ringed || std::thread::current().id() != self.thread {
            return None;
        }
        LANE_REACTOR.with(|r| {
            let mut r = r.borrow_mut();
            let reactor = r.as_mut()?;
            let ops = ops.take()?;
            JOURNAL_RING_LANE_WRITES.fetch_add(1, Ordering::Relaxed);
            Some(reactor.submit_write_at_batch(path, ops))
        })
    }
}

impl Drop for JournalLane {
    fn drop(&mut self) {
        self.exec.shutdown();
    }
}
