//! Pure leader-election / queue / drain core of the per-volume commit
//! conveyor (design-metadata-throughput §5.5 D5, PR M7).
//!
//! Self-contained (no crate dependencies) so the `loom-models` crate can
//! `#[path]`-include this file and exhaustively model-check the
//! enqueue / leader-elect / drain / unlead interleavings under
//! `cfg(loom)`, exactly like `journal_core.rs`. The backend wrapper
//! ([`super::backend`]) adds the pass pipeline (admission, union locks,
//! journal write, fan-out); nothing in this module touches a device or
//! an async runtime.
//!
//! ## The protocol
//!
//! Every committer runs the same two uninterruptible steps — there is no
//! await point between them, so a cancelled committer future can never
//! strand an enqueued entry without an elected leader:
//!
//! 1. [`ConveyorCore::enqueue`] its entry (FIFO);
//! 2. [`ConveyorCore::try_lead`] — on winning the CAS it spawns the
//!    detached pass task, then parks on its oneshot like every follower.
//!
//! The pass task loops: [`ConveyorCore::drain`] a batch (bounded by the
//! tx/byte caps, **no timers** — the batch is whatever is queued at
//! drain time, jbd2's no-wait shape) and process it; on an empty drain
//! it calls [`ConveyorCore::unlead_and_recheck`], which releases
//! leadership and **then** re-checks the queue — re-electing itself if
//! an enqueue raced the release. That release-then-recheck order is the
//! no-lost-wakeup theorem the loom model pins:
//!
//! - a committer that fails `try_lead` observed `leader == true`
//!   (SeqCst), i.e. it lost to a leader whose release had not yet
//!   happened in the SeqCst order;
//! - its `enqueue` precedes its failed CAS in program order;
//! - the leader's queue re-check follows its release in program order;
//! - so release-after-CAS ⇒ recheck-after-enqueue ⇒ the entry is seen
//!   (by the recheck) or was already drained. Either way exactly one
//!   pass owns it.
//!
//! ## Invariants (the loom models in `loom-models/src/lib.rs`)
//!
//! 1. **Leader uniqueness** — `try_lead` admits at most one leader until
//!    the matching release; two racing electors never both win.
//! 2. **No lost wakeups** — after any interleaving of enqueue+elect
//!    against a draining/unleading pass, no entry is left queued with no
//!    leader responsible for it.
//! 3. **FIFO** — `drain` returns entries in enqueue order (per-key
//!    journal-seq order equals RAM apply order downstream only if the
//!    batch preserves arrival order — §4.4 pt 2 transfers through this).
//! 4. **Budget conservation across committer drops** — entry byte
//!    budgets survive the enqueueing committer's disappearance: drained
//!    exactly once, leaked never (the drop only kills the committer's
//!    result channel, not the queue entry).
//! 5. **Guard-lifetime ≥ staged-record-lifetime** — an entry's co-owned
//!    guard token (modeled as an `Arc`) stays alive from enqueue to the
//!    pass's terminal outcome for that entry even when the committer's
//!    own clone drops first (Issue 13 — the exclusion is structural,
//!    from ownership, not pleaded from committer behavior).

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::atomic::{AtomicBool, Ordering};
    pub use loom::sync::Mutex;
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::atomic::{AtomicBool, Ordering};
    pub use std::sync::Mutex;
}

use std::collections::VecDeque;
use sync::{AtomicBool, Mutex, Ordering};

/// The queue + leadership word. `T` is the backend's queue entry
/// ({staged records, `Arc<[DlmGuard]>`, oneshot} in production; small
/// tokens in the models). The mutex guards O(1)/O(batch) queue ops only
/// and is never held across an await (it is a plain blocking mutex —
/// the `JournalRing::inflight` discipline; commit paths are in the
/// sanctioned metadata-transaction lock class).
pub struct ConveyorCore<T> {
    queue: Mutex<VecDeque<(T, u64)>>,
    /// Leadership word: `true` while exactly one pass task owns draining.
    leader: AtomicBool,
}

impl<T> Default for ConveyorCore<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ConveyorCore<T> {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            leader: AtomicBool::new(false),
        }
    }

    /// FIFO-enqueue one entry carrying `len` budget bytes (the exact
    /// journal entry size — the drain's byte cap counts these).
    pub fn enqueue(&self, item: T, len: u64) {
        self.queue.lock().unwrap().push_back((item, len));
    }

    /// Attempt to become the leader. `true` ⇒ the caller MUST arrange a
    /// pass (spawn the detached task); `false` ⇒ a live leader is
    /// responsible for the queue (the no-lost-wakeup argument in the
    /// module docs). SeqCst: the failed-CAS ↔ release ↔ recheck ordering
    /// is exactly what the theorem consumes, and election is once per
    /// pass, not per commit — never hot.
    pub fn try_lead(&self) -> bool {
        self.leader
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Drain the next batch: FIFO order, at most `max_txs` entries, at
    /// most `max_bytes` summed budget — except the FIRST entry, which is
    /// always taken (progress: an entry larger than the byte cap must
    /// still commit; per-entry sizes are independently bounded by the
    /// 128 KiB whole-entry cap upstream). Only the leader calls this.
    pub fn drain(&self, max_txs: usize, max_bytes: u64) -> Vec<T> {
        let mut q = self.queue.lock().unwrap();
        let mut out = Vec::new();
        let mut bytes = 0u64;
        while out.len() < max_txs.max(1) {
            let Some((_, len)) = q.front() else { break };
            if !out.is_empty() && bytes + *len > max_bytes {
                break;
            }
            let (item, len) = q.pop_front().expect("front observed");
            bytes += len;
            out.push(item);
        }
        out
    }

    /// Release leadership, then re-check the queue (release-then-recheck
    /// — the order is the no-lost-wakeup theorem). Returns `true` if the
    /// caller re-elected itself and must run another pass; `false` if it
    /// may exit (either the queue is empty, or a racing enqueuer's
    /// `try_lead` won and owns the drain now).
    pub fn unlead_and_recheck(&self) -> bool {
        self.leader.store(false, Ordering::SeqCst);
        if self.queue.lock().unwrap().is_empty() {
            return false;
        }
        self.try_lead()
    }

    /// Entries currently queued (stats gauge + the conformance suite's
    /// enqueue-sequencing probe).
    pub fn pending(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// FIFO + caps: drains return enqueue order, bounded by both caps,
    /// with the first-entry progress rule.
    #[test]
    fn drain_is_fifo_and_cap_bounded() {
        let c: ConveyorCore<u32> = ConveyorCore::new();
        for i in 0..10u32 {
            c.enqueue(i, 10);
        }
        assert_eq!(c.pending(), 10);

        // tx cap.
        assert_eq!(c.drain(4, u64::MAX), vec![0, 1, 2, 3]);
        // byte cap: 25 bytes admits two 10-byte entries.
        assert_eq!(c.drain(usize::MAX, 25), vec![4, 5]);
        // first-entry progress: a cap below one entry still drains one.
        assert_eq!(c.drain(8, 1), vec![6]);
        // remainder in order.
        assert_eq!(c.drain(64, 1024), vec![7, 8, 9]);
        assert_eq!(c.pending(), 0);
        assert!(c.drain(64, 1024).is_empty());
    }

    /// Leadership: one winner at a time; release-then-recheck re-elects
    /// exactly when work is queued.
    #[test]
    fn leadership_is_exclusive_and_recheck_reelects() {
        let c: ConveyorCore<u32> = ConveyorCore::new();
        assert!(c.try_lead(), "first elector wins");
        assert!(!c.try_lead(), "second elector must lose to a live leader");

        // Empty queue: release stands, no re-election.
        assert!(!c.unlead_and_recheck());
        assert!(c.try_lead(), "leadership was released");

        // Non-empty queue: the leader must re-elect itself on release.
        c.enqueue(7, 1);
        assert!(
            c.unlead_and_recheck(),
            "a queued entry must re-elect the releasing leader"
        );
        assert_eq!(c.drain(64, 1024), vec![7]);
        assert!(!c.unlead_and_recheck());
    }
}
