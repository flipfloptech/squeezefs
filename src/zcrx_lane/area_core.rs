//! Lock-free grant ledger for the zcrx area (design §4.3/§5): per-slot
//! refcounts + the MPSC free-return stack that feeds the refill ring.
//!
//! Protocol (the loom-modeled core — `loom-models/src/lib.rs`
//! `zcrx_area_core`):
//! * **grant** (single consumer — the queue driver): pop a free slot,
//!   refcount 0 → 1. In the sim backend the slot IS the area chunk the
//!   next recv lands in; in the real backend it is the bookkeeping record
//!   for a kernel-granted CQE span.
//! * **add_ref / release** (any thread): payload consumers (fill scatter
//!   slices, gather passes) clone and drop refs; the releaser that
//!   observes the 0-crossing pushes the slot back onto the free stack —
//!   exactly once per grant epoch.
//! * **Refill discipline** (design §4.3): a slot returns to the refill
//!   ring only when its refcount drops to zero — recycling can never
//!   stall mid-stream; exhaustion applies backpressure at command
//!   admission instead (the queue's admission semaphore).
//!
//! **Model precondition, stated because the model cannot see its
//! violation** (the `ipc_ring_core` lesson): exactly ONE consumer pops
//! grants per ledger — guaranteed structurally by the one-driver-per-queue
//! ownership (sim reader task / real ring thread), not by anything inside
//! this core. Pushers are unbounded (any thread may drop the last ref).
//! The single consumer is also what makes the pop ABA-free structurally
//! (a slot at the stack head cannot be re-pushed before this pop
//! completes — re-pushing requires a grant, and only this consumer
//! grants); the packed pop tag is a belt on top of that argument, not the
//! argument itself.
//!
//! Memory ordering is load-bearing: `release` is the Arc pattern
//! (`fetch_sub(Release)` + 0-crossing `fence(Acquire)`) so every
//! consumer's payload reads happen-before the driver recycles the chunk
//! into NIC-DMA-writable state; the stack push (Release CAS) / pop
//! (Acquire load) chain extends that edge to the driver. Weakening either
//! fails the loom model (weakening-verified — see the model doc).
//!
//! Self-contained so `loom-models/` can `#[path]`-include it; the main
//! build never sets `cfg(loom)`.

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicUsize, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::atomic::{fence, AtomicU32, AtomicU64, AtomicUsize, Ordering};
}

use sync::Ordering::{AcqRel, Acquire, Relaxed, Release};
use sync::{fence, AtomicU32, AtomicU64, AtomicUsize};

/// Packed head: high 32 = pop tag, low 32 = top slot + 1 (0 = empty).
fn pack(tag: u32, slot_plus1: u32) -> u64 {
    ((tag as u64) << 32) | slot_plus1 as u64
}
fn unpack(head: u64) -> (u32, u32) {
    ((head >> 32) as u32, head as u32)
}

/// Grant ledger over fixed slots (see module docs).
pub struct SpanLedger {
    /// Per-slot refcount; 0 = free (in the stack or mid-pop).
    refs: Box<[AtomicU32]>,
    /// Free-stack links: `next[i]` = successor slot + 1 (0 = end).
    next: Box<[AtomicU32]>,
    /// Packed (tag, top slot + 1).
    head: AtomicU64,
    /// Free-slot count (diagnostics/gauges only — racy-tolerant, exact at
    /// quiescence).
    free: AtomicUsize,
}

impl SpanLedger {
    /// All `slots` start FREE (seeded into the stack, 0 on top).
    pub fn new(slots: usize) -> Self {
        assert!(slots > 0 && slots < u32::MAX as usize, "slot count sane");
        let refs: Box<[AtomicU32]> = (0..slots).map(|_| AtomicU32::new(0)).collect();
        // Seed the intrusive list 0 → 1 → … → n-1 → end.
        let next: Box<[AtomicU32]> = (0..slots)
            .map(|i| {
                if i + 1 < slots {
                    AtomicU32::new(i as u32 + 2)
                } else {
                    AtomicU32::new(0)
                }
            })
            .collect();
        SpanLedger {
            refs,
            next,
            head: AtomicU64::new(pack(0, 1)),
            free: AtomicUsize::new(slots),
        }
    }

    pub fn capacity(&self) -> usize {
        self.refs.len()
    }

    /// Free-slot count (diagnostic gauge; exact at quiescence).
    pub fn free_count(&self) -> usize {
        self.free.load(Relaxed)
    }

    /// Pop a free slot and take its first ref (SINGLE consumer — module
    /// docs precondition). `None` = exhausted (admission backpressure).
    pub fn try_grant(&self) -> Option<u32> {
        let mut head = self.head.load(Acquire);
        loop {
            let (tag, top_plus1) = unpack(head);
            if top_plus1 == 0 {
                return None;
            }
            let slot = top_plus1 - 1;
            // Safe to read: a slot at the head cannot be re-pushed before
            // this (sole) consumer completes the pop — its link is stable.
            let succ = self.next[slot as usize].load(Relaxed);
            match self.head.compare_exchange_weak(
                head,
                pack(tag.wrapping_add(1), succ),
                AcqRel,
                Acquire,
            ) {
                Ok(_) => {
                    self.free.fetch_sub(1, Relaxed);
                    debug_assert_eq!(
                        self.refs[slot as usize].load(Relaxed),
                        0,
                        "granted slot must be ref-free"
                    );
                    // First ref: the slot is invisible to every other
                    // thread until the driver publishes a slice of it.
                    self.refs[slot as usize].store(1, Relaxed);
                    return Some(slot);
                }
                Err(cur) => head = cur,
            }
        }
    }

    /// Add a ref to a granted slot. Precondition: the caller already
    /// holds a ref (a clone edge, never a resurrection edge).
    pub fn add_ref(&self, slot: u32) {
        let prev = self.refs[slot as usize].fetch_add(1, Relaxed);
        debug_assert!(prev > 0, "add_ref on a free slot (resurrection)");
    }

    /// Drop a ref; the 0-crossing releaser recycles the slot onto the
    /// free stack exactly once per grant epoch. Returns `true` when this
    /// call freed the slot. The Release/Acquire pair is the Arc pattern:
    /// every holder's payload reads happen-before the recycle.
    pub fn release(&self, slot: u32) -> bool {
        let prev = self.refs[slot as usize].fetch_sub(1, Release);
        debug_assert!(prev > 0, "release without a ref (double free)");
        if prev != 1 {
            return false;
        }
        fence(Acquire);
        self.push_free(slot);
        true
    }

    /// MPSC push onto the free stack (the 0-crossing releaser's arm).
    fn push_free(&self, slot: u32) {
        let mut head = self.head.load(Relaxed);
        loop {
            let (tag, top_plus1) = unpack(head);
            self.next[slot as usize].store(top_plus1, Relaxed);
            match self
                .head
                .compare_exchange_weak(head, pack(tag, slot + 1), Release, Relaxed)
            {
                Ok(_) => {
                    self.free.fetch_add(1, Relaxed);
                    return;
                }
                Err(cur) => head = cur,
            }
        }
    }
}
