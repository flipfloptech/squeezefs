//! Pure lock-free extent-allocator core (design §4.7): bitmap claim /
//! release, the pending-free seq gate, and compaction-reserve accounting.
//!
//! Self-contained (no crate dependencies) so the `loom-models` crate can
//! `#[path]`-include this file and exhaustively model-check the claim /
//! pending-free / durable-advance interleavings under `cfg(loom)`, exactly
//! like `alloc_core.rs` and `journal_core.rs`. The I/O wrapper
//! ([`super::alloc_ext`]) adds the A/B bitmap pages, the journaled
//! alloc/free delta records, and the typed ENOSPC surface; nothing in this
//! module touches a device.
//!
//! ## The three §4.7 protocols, in one core
//!
//! - **Claim** generalizes the proven `alloc_core` protocol to extents:
//!   one bit per extent in a `Box<[AtomicU64]>`, a single `fetch_or`
//!   decides the unique winner for a bit — no lock, no double-alloc. The
//!   claim is gated by a **free-budget CAS** first (see *why
//!   budget-then-bit is safe* below), so a won budget always finds a clear
//!   bit.
//! - **Pending-free seq gate** (§4.7's CoW reuse rule; §4.6 pt 3 is its
//!   ring twin): a freed extent enters a bounded FIFO of
//!   `(extent, retire_seq)` entries — `retire_seq` = the checkpoint seq
//!   that stops referencing it — with its bitmap bit **still set** and its
//!   budget byte **not** freed. It becomes claimable only when
//!   [`ExtCore::advance_durable`] passes its `retire_seq`, i.e. only after
//!   that root-ledger record is *known durable* (post-barrier). Hence any
//!   ledger record mount can select — newest valid or its predecessor —
//!   references only never-overwritten extents (risk R3). The FIFO is a
//!   bounded Vyukov-stamped MPMC ring; the drain stops at the first entry
//!   the watermark does not cover — complete because retire seqs are
//!   **non-decreasing in push order** (checkpoint seqs are monotonic and
//!   frees are pushed by the serialized per-volume checkpoint/SMO task,
//!   §4.6; debug-asserted). A full FIFO refuses the free
//!   ([`PendingFreeFull`]) — §4.7: "capped; pressure forces a checkpoint
//!   rather than unsafe reuse".
//! - **Reserve accounting** (§4.7 ENOSPC semantics): `reserve` extents
//!   (production: `max(8, 2 %)` — the wrapper computes it) are claimable
//!   only by [`AllocClass::Internal`] (compaction / checkpoint / SMO
//!   internals). An [`AllocClass::User`] claim fails once granting it
//!   would dip the free budget to-or-below the reserve — the metadata op
//!   surfaces ENOSPC while the tree can still fold appends and free
//!   space: no write-to-free-space deadlock.
//!
//! ## Why budget-then-bit is safe
//!
//! `free_budget` counts exactly the claimable clear bits. A claim
//! decrements the budget first (CAS with the class floor), then scans
//! `fetch_or` for a clear bit. The invariant is
//! `clear_bits ≥ free_budget + budget-winners-not-yet-holding-a-bit`:
//! claims decrement the budget before setting a bit (both sides balanced),
//! and every release path **clears the bit before incrementing the
//! budget**, so the invariant is never violated in either direction and a
//! budget winner's clear bit is already visible when its scan runs (the
//! budget CAS's acquire pairs with the release increment). Racing
//! claimers may steal the specific bit a scanner was eyeing — never the
//! last one it is entitled to — so the scan retries and terminates.
//! Mount-time seeding ([`ExtCore::mark_allocated`]) transiently violates
//! the invariant in the other direction (bit set, then budget decrement),
//! which is why it takes `&mut self`: construction-phase only, never
//! concurrent with claims.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// Who is asking for an extent (§4.7 ENOSPC semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocClass {
    /// A user-op allocation: refused once the free budget would dip
    /// to-or-below the compaction reserve — the op surfaces ENOSPC.
    User,
    /// Compaction / checkpoint / SMO internals: may consume the reserve,
    /// so the tree can always fold appends and free space even at
    /// user-visible ENOSPC (§4.7: no write-to-free-space deadlock).
    Internal,
}

/// Claim failure. The wrapper maps this onto the typed ENOSPC surface
/// (`KvError::NoSpace`) for user ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// No claimable extent within this class's budget: for `User` this is
    /// "free ≤ reserve" — the §4.7 ENOSPC condition with the reserve
    /// intact; for `Internal` it is a genuinely exhausted heap.
    NoSpace,
}

/// Pending-free FIFO full (§4.7 "capped"): the caller must force a
/// checkpoint + durable-advance to drain it — never reuse unsafely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingFreeFull;

/// One pending-free FIFO slot, Vyukov-stamped so producers and consumers
/// synchronize per slot without locks: a producer at position `pos` may
/// fill the slot only while `stamp == pos` and publishes with
/// `stamp = pos + 1`; the consumer that wins position `pos` vacates with
/// `stamp = pos + capacity`.
struct PendingSlot {
    stamp: AtomicU64,
    /// Extent index (stable while `stamp == pos + 1`).
    extent: AtomicU64,
    /// The checkpoint seq that stops referencing the extent (§4.7).
    retire_seq: AtomicU64,
}

/// The pure lock-free extent-allocator core. See the module docs for the
/// protocols; the loom models in `loom-models/` pin the invariants: no
/// double-alloc under concurrent claimers, a pending-freed extent never
/// claimable before its checkpoint-durable seq, and reserve isolation
/// (user claims can never consume the compaction reserve).
pub struct ExtCore {
    /// One bit per extent; bit set == allocated **or** pending-free.
    words: Box<[AtomicU64]>,
    /// Exclusive upper bound on extent indices.
    total: u64,
    /// Compaction reserve in extents (§4.7): claimable by `Internal` only.
    reserve: u64,
    /// Claimable clear bits (pending-free bits are set, so they are
    /// neither clear nor counted).
    free_budget: AtomicU64,
    /// Next-scan hint (monotonic-ish; reset downward on release).
    hint: AtomicU64,
    /// Newest checkpoint seq known durable (post-barrier).
    durable_seq: AtomicU64,
    /// Bounded pending-free FIFO (Vyukov-stamped ring).
    pending: Box<[PendingSlot]>,
    /// FIFO producer cursor (a position, not an index).
    pending_head: AtomicU64,
    /// FIFO consumer cursor (a position, not an index).
    pending_tail: AtomicU64,
    /// Debug guard: retire seqs must be non-decreasing in push order
    /// (§4.6's serialized checkpoint/SMO task guarantees it).
    last_retire_seq: AtomicU64,
}

impl ExtCore {
    /// Build an all-free core over `total` extents with `reserve` of them
    /// held back for `Internal` claims and a pending-free FIFO of
    /// `pending_cap` entries. `reserve` must leave claimable space;
    /// `pending_cap ≥ 2` — hard-asserted (construction-time, once per
    /// volume) because a 1-slot Vyukov ring aliases its stamps: "full at
    /// position p" and "vacant for position p+1" would both read `p + 1`,
    /// silently overwriting an un-drained entry — a §4.7 gate bypass.
    pub fn new(total: u64, reserve: u64, pending_cap: usize) -> Self {
        assert!(total > 0, "degenerate heap");
        assert!(reserve < total, "reserve must leave claimable extents");
        assert!(
            pending_cap >= 2,
            "pending-free FIFO needs ≥ 2 slots (Vyukov stamp aliasing at 1)"
        );
        let words: Vec<AtomicU64> = (0..total.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        let pending: Vec<PendingSlot> = (0..pending_cap)
            .map(|i| PendingSlot {
                stamp: AtomicU64::new(i as u64),
                extent: AtomicU64::new(0),
                retire_seq: AtomicU64::new(0),
            })
            .collect();
        Self {
            words: words.into_boxed_slice(),
            total,
            reserve,
            free_budget: AtomicU64::new(total),
            hint: AtomicU64::new(0),
            durable_seq: AtomicU64::new(0),
            pending: pending.into_boxed_slice(),
            pending_head: AtomicU64::new(0),
            pending_tail: AtomicU64::new(0),
            last_retire_seq: AtomicU64::new(0),
        }
    }

    /// Total extents.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The compaction reserve in extents.
    pub fn reserve(&self) -> u64 {
        self.reserve
    }

    /// Claimable extents right now (excludes allocated and pending-free).
    pub fn free_extents(&self) -> u64 {
        self.free_budget.load(Ordering::Acquire)
    }

    /// Entries currently parked in the pending-free FIFO (diagnostics —
    /// includes in-flight pushes).
    pub fn pending_count(&self) -> u64 {
        let head = self.pending_head.load(Ordering::Acquire);
        let tail = self.pending_tail.load(Ordering::Acquire);
        head.saturating_sub(tail)
    }

    /// Newest checkpoint seq known durable.
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq.load(Ordering::Acquire)
    }

    /// Claim a free extent for `class`. Lock-free: a free-budget CAS gated
    /// by the class floor (`User` must leave the reserve behind), then a
    /// `fetch_or` bit scan whose 0→1 winner is unique (module docs).
    /// Errors with [`ClaimError::NoSpace`] — for `User`, the §4.7 ENOSPC
    /// signal with the reserve intact.
    pub fn claim(&self, class: AllocClass) -> Result<u64, ClaimError> {
        let floor = match class {
            AllocClass::User => self.reserve,
            AllocClass::Internal => 0,
        };
        // Budget first: win the entitlement to one claimable clear bit,
        // or refuse without touching the bitmap.
        let mut free = self.free_budget.load(Ordering::Acquire);
        loop {
            if free <= floor {
                return Err(ClaimError::NoSpace);
            }
            match self.free_budget.compare_exchange(
                free,
                free - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(cur) => free = cur,
            }
        }
        // The budget win entitles this caller to exactly one clear bit;
        // the scan must find one (module docs).
        let start = self.hint.load(Ordering::Relaxed).min(self.total);
        loop {
            for idx in (start..self.total).chain(0..start) {
                let w = (idx / 64) as usize;
                let mask = 1u64 << (idx % 64);
                if self.words[w].fetch_or(mask, Ordering::AcqRel) & mask == 0 {
                    self.hint.store(idx + 1, Ordering::Relaxed);
                    return Ok(idx);
                }
            }
            // A release between the budget win and this pass can land
            // behind the cursor; rescan. Bounded: the budget win
            // guarantees a clear bit exists and stays clear until some
            // claimant (possibly this one) takes it.
            #[cfg(loom)]
            loom::thread::yield_now();
            #[cfg(not(loom))]
            core::hint::spin_loop();
        }
    }

    /// Mark `extent` allocated — mount seeding only (newest-valid bitmap
    /// page bits, then replayed journal alloc records). `&mut self` on
    /// purpose: it sets the bit *before* decrementing the budget, which is
    /// only safe while the core is not yet shared with claimers (module
    /// docs). Idempotent; out-of-range indices are ignored.
    pub fn mark_allocated(&mut self, extent: u64) {
        if extent >= self.total {
            return;
        }
        let w = (extent / 64) as usize;
        let mask = 1u64 << (extent % 64);
        if self.words[w].fetch_or(mask, Ordering::AcqRel) & mask == 0 {
            let prev = self.free_budget.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(prev >= 1, "budget underflow marking {extent}");
        }
    }

    /// Release `extent` directly to the claimable pool: bit cleared, then
    /// budget incremented (that order — module docs). Only for extents
    /// **no root-ledger record references**: a build that failed before
    /// its extent was ever published, or the durable-advance drain below.
    /// A checkpoint-referenced extent must go through
    /// [`Self::free_pending`] + [`Self::advance_durable`] (§4.7).
    /// Idempotent; out-of-range indices are ignored.
    pub fn release(&self, extent: u64) {
        if extent >= self.total {
            return;
        }
        let w = (extent / 64) as usize;
        let mask = 1u64 << (extent % 64);
        if self.words[w].fetch_and(!mask, Ordering::AcqRel) & mask != 0 {
            self.free_budget.fetch_add(1, Ordering::AcqRel);
            // Prefer reusing low freed extents (locality under churn).
            let _ = self.hint.fetch_min(extent, Ordering::Relaxed);
        }
    }

    /// Enter `extent` into the pending-free FIFO tagged with `retire_seq`
    /// — the checkpoint seq that stops referencing it (§4.7). The bit
    /// stays set and the budget untouched until [`Self::advance_durable`]
    /// covers the tag. Errors with [`PendingFreeFull`] at capacity (the
    /// §4.7 cap — the caller forces a checkpoint, never reuses unsafely).
    ///
    /// Contract: `retire_seq`s are non-decreasing in push order —
    /// guaranteed by the serialized per-volume checkpoint/SMO task (§4.6)
    /// and debug-asserted here — and the extent's bit is set (it was
    /// claimed, and stays claimed while pending).
    pub fn free_pending(&self, extent: u64, retire_seq: u64) -> Result<(), PendingFreeFull> {
        debug_assert!(
            extent < self.total,
            "pending-free of extent {extent} out of range"
        );
        debug_assert!(
            self.words[(extent / 64) as usize].load(Ordering::Acquire) & (1u64 << (extent % 64))
                != 0,
            "pending-free of an unclaimed extent {extent}"
        );
        debug_assert!(
            self.last_retire_seq.fetch_max(retire_seq, Ordering::AcqRel) <= retire_seq,
            "retire seqs must be non-decreasing in push order (§4.6 serialized SMO task)"
        );
        let cap = self.pending.len() as u64;
        let mut pos = self.pending_head.load(Ordering::Acquire);
        loop {
            let slot = &self.pending[(pos % cap) as usize];
            let stamp = slot.stamp.load(Ordering::Acquire);
            if stamp == pos {
                // Slot vacant for this position: claim it.
                match self.pending_head.compare_exchange(
                    pos,
                    pos + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        slot.extent.store(extent, Ordering::Relaxed);
                        slot.retire_seq.store(retire_seq, Ordering::Relaxed);
                        slot.stamp.store(pos + 1, Ordering::Release);
                        return Ok(());
                    }
                    Err(cur) => pos = cur,
                }
            } else if stamp < pos {
                // The consumer has not vacated this slot from a lap ago:
                // the FIFO is full (§4.7 cap).
                return Err(PendingFreeFull);
            } else {
                // A racing producer advanced the head past our read.
                pos = self.pending_head.load(Ordering::Acquire);
            }
        }
    }

    /// Advance the durable-checkpoint watermark to `seq` (monotonic
    /// `fetch_max`; stale advances are no-ops) and drain every pending
    /// entry the watermark now covers: bit cleared, budget incremented —
    /// the extent is claimable again, and only now (§4.7: "it becomes
    /// allocatable only after that root record is *known durable*").
    /// Callers pass only post-barrier checkpoint seqs. Returns the
    /// released extents (the wrapper marks their bitmap pages dirty).
    pub fn advance_durable(&self, seq: u64) -> Vec<u64> {
        self.durable_seq.fetch_max(seq, Ordering::AcqRel);
        let durable = self.durable_seq.load(Ordering::Acquire);
        let cap = self.pending.len() as u64;
        let mut released = Vec::new();
        loop {
            let pos = self.pending_tail.load(Ordering::Acquire);
            let head = self.pending_head.load(Ordering::Acquire);
            if pos == head {
                return released; // FIFO empty.
            }
            let slot = &self.pending[(pos % cap) as usize];
            let stamp = slot.stamp.load(Ordering::Acquire);
            if stamp != pos + 1 {
                // The tail entry's producer claimed the position but has
                // not published yet (or a racing consumer just vacated
                // it); a later advance drains it.
                return released;
            }
            // Peek is stable while stamp == pos + 1: a producer can only
            // reuse the slot after a consumer stamps pos + cap, and only
            // the tail-CAS winner below does that.
            let retire_seq = slot.retire_seq.load(Ordering::Relaxed);
            if retire_seq > durable {
                // FIFO order + non-decreasing tags ⇒ nothing behind this
                // entry is releasable either: the §4.7 gate holds.
                return released;
            }
            let extent = slot.extent.load(Ordering::Relaxed);
            if self
                .pending_tail
                .compare_exchange(pos, pos + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Exclusive owner of this entry: vacate the slot for the
                // producer a lap ahead, then release the extent.
                slot.stamp.store(pos + cap, Ordering::Release);
                self.release(extent);
                released.push(extent);
            }
            // CAS failure: a racing consumer took it; re-read the tail.
        }
    }

    /// Whether `extent` is allocated or pending-free (bit set).
    /// Out-of-range reads as free.
    pub fn is_allocated(&self, extent: u64) -> bool {
        if extent >= self.total {
            return false;
        }
        self.words[(extent / 64) as usize].load(Ordering::Acquire) & (1u64 << (extent % 64)) != 0
    }

    /// Snapshot the bitmap words (bit set == allocated or pending-free) —
    /// the wrapper serializes these into A/B bitmap pages at checkpoints.
    pub fn snapshot_words(&self) -> Vec<u64> {
        self.words
            .iter()
            .map(|w| w.load(Ordering::Acquire))
            .collect()
    }
}
