//! **Custody by the slot holder — the two recall Dekker pairs' protocol
//! core** (symmetric metadata PR 9, review round 4 — Issue 29;
//! docs/design-symmetric-metadata.md §5.1.4 flush-then-transfer).
//!
//! Self-contained (std under the main build, `loom`'s under `--cfg loom`)
//! so `loom-models/` `#[path]`-includes it and model-checks the two racing
//! edges the recall design rests on. The process plane around it — the
//! FUSE layer's cached-lease map and its custody hooks
//! (`fuse_client::SqueezefsFilesystem::{custody_use_enter,
//! revoke_local_lease_for_recall, settle_recalled_leases}`), the handover
//! body and the grant paths (`data_grant::{defer_handover_for_custody,
//! handover_recall_defers}`) — is never included by loom.
//!
//! # Pair 1 — the FUSE layer's revoke / enter ([`CustodyUseCore`])
//!
//! One word per foreign-custody ino: `uses` (the in-flight custody uses —
//! every mutating handler and detached DMA continuation holds one from
//! entry to end) and `cached` (whether the FUSE layer's cached lease of
//! the ino may be served — the `active_leases` entry's presence, as the
//! recall sees it). A handler registers its use BEFORE it reads the word
//! (`enter` then `is_cached`), the recall clears the word BEFORE it reads
//! the uses (`revoke` then `settle_ready`), a `SeqCst` fence on each side
//! between the two steps — so either the handler reads the lease revoked
//! and re-acquires, or the settle counts it in flight and the release
//! waits. A handler that used the cached lease unseen by the settle is
//! the "write under a released grant" shape the weakened model reports.
//!
//! # Pair 2 — the handover's mark / the grant's re-check ([`HandoverMarkCore`])
//!
//! The handover arms its slot's mid-handover mark (under the mark table's
//! lock, the fast-path count stored) BEFORE its census of the live grants
//! (under the grant table's lock); a grant path inserts its grant (under
//! the grant table's lock) BEFORE it re-reads the mark (the `Relaxed`
//! count first, the table under its lock only when the count is non-zero).
//! Both sides take both locks, one per step, so the locks' acquire/release
//! chains order the steps: a grant the census missed was inserted after
//! the census's lock, so the grant's later count read happens-after the
//! mark's count store and sees it — the `Relaxed` fast path is INSIDE the
//! happens-before chain. A grant neither seen by the census nor deferred
//! is the "grant spanning the move" shape the weakened model reports.

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
    pub use loom::sync::Mutex;
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
    pub use std::sync::Mutex;
}

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};
use sync::{fence, AtomicU64, AtomicUsize, Mutex, Ordering};

/// Pair 1's per-ino words.
#[derive(Debug, Default)]
pub struct CustodyUseCore {
    /// In-flight custody uses of the ino.
    uses: AtomicU64,
    /// 1 ⇔ the FUSE layer's cached lease of the ino may be served; 0 ⇔
    /// revoked (parked by a recall) or never cached.
    cached: AtomicU64,
}

impl CustodyUseCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// **The handler's side of the pair, step 1**: register the use, then
    /// fence. The word is read as a separate later step
    /// ([`Self::is_cached`]) — the FUSE layer reads it at its ONE
    /// cached-lease accessor, not at the handler's door.
    pub fn enter(&self) {
        self.uses.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// **The handler's side, step 2**: may the cached lease be served?
    pub fn is_cached(&self) -> bool {
        self.cached.load(Ordering::SeqCst) == 1
    }

    /// The handler's use ended (its guard dropped).
    pub fn leave(&self) {
        self.uses.fetch_sub(1, Ordering::SeqCst);
    }

    /// In-flight uses.
    pub fn uses(&self) -> u64 {
        self.uses.load(Ordering::SeqCst)
    }

    /// The slow path cached a lease for the ino: the word reads served
    /// from here on. Stored BEFORE the lease enters the map, so no reader
    /// ever finds a cached lease under a cleared word.
    pub fn cache(&self) {
        self.cached.store(1, Ordering::SeqCst);
    }

    /// **The recall's side of the pair, step 1**: clear the word (the
    /// cached lease is parked), then fence.
    pub fn revoke(&self) {
        self.cached.store(0, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// **The recall's side, step 2**: may the parked lease's release
    /// depart? `true` ⇔ no use is in flight.
    pub fn settle_ready(&self) -> bool {
        self.uses.load(Ordering::SeqCst) == 0
    }
}

/// One mid-handover mark.
#[derive(Clone, Copy, Debug)]
pub struct HandoverMark {
    /// When it was armed (the expiry's clock).
    pub since: Instant,
    /// Held by a completing handover or the leave: never expires.
    pub held: bool,
    /// The arm's monotone stamp — a guard clears only the mark it armed
    /// (review round 4, Issue 30: an `Instant` stamp collided inside the
    /// clock's resolution).
    pub seq: u64,
}

/// Pair 2's mark table: the marks under one lock, the fast-path count
/// beside it.
#[derive(Debug)]
pub struct HandoverMarkCore<K> {
    marks: Mutex<HashMap<K, HandoverMark>>,
    /// `marks.len()` — the fast path's one `Relaxed` load: every shipped
    /// acquire reads 0 here and touches nothing else.
    count: AtomicUsize,
    seq: AtomicU64,
}

impl<K: Hash + Eq + Copy> Default for HandoverMarkCore<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq + Copy> HandoverMarkCore<K> {
    pub fn new() -> Self {
        Self {
            marks: Mutex::new(HashMap::new()),
            count: AtomicUsize::new(0),
            seq: AtomicU64::new(1),
        }
    }

    /// **The handover's side of the pair, step 1**: arm (or re-arm)
    /// `key`'s mark; returns the stamp written. The census of the live
    /// grants (under the grant table's lock) is the caller's step 2.
    pub fn arm(&self, key: K, held: bool) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
        marks.insert(
            key,
            HandoverMark {
                since: Instant::now(),
                held,
                seq,
            },
        );
        self.count.store(marks.len(), Ordering::Release);
        seq
    }

    /// A deferred tick's mark stays armed but UNHELD (it expires at the
    /// bound if the requester abandons the handover).
    pub fn unhold(&self, key: K) {
        let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = marks.get_mut(&key) {
            m.held = false;
        }
    }

    /// The guard's drop: clear `key`'s mark iff it is still the HELD mark
    /// stamped `seq` (a later re-arm — the next tick's, the leave's — is
    /// preserved). `true` ⇔ cleared.
    pub fn clear_if(&self, key: K, seq: u64) -> bool {
        let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
        let ours = marks.get(&key).is_some_and(|m| m.held && m.seq == seq);
        if ours {
            marks.remove(&key);
            self.count.store(marks.len(), Ordering::Release);
        }
        ours
    }

    /// **The grant path's side of the pair, step 2** (its step 1 is the
    /// grant's insertion under the grant table's lock): is `key`'s slot
    /// mid-handover? The `Relaxed` count first — 0 answers `false` and
    /// touches nothing else — then the table: a held mark stands, an
    /// unheld one stands while younger than `bound` and is removed once
    /// it expired.
    pub fn defers(&self, key: K, bound: Duration) -> bool {
        if self.count.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
        match marks.get(&key) {
            Some(m) if m.held || m.since.elapsed() < bound => true,
            Some(_) => {
                marks.remove(&key);
                self.count.store(marks.len(), Ordering::Release);
                false
            }
            None => false,
        }
    }

    /// Marks standing (the test seam's word; the fast path's count).
    pub fn pending(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Drop every mark (the disarm).
    pub fn clear_all(&self) {
        let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
        marks.clear();
        self.count.store(0, Ordering::Release);
    }
}
