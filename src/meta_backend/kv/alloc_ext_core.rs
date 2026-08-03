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
//!   ring twin — and since the Option-A coverage fix,
//!   `docs/design-smo-replay-currency.md` §2-A, the SAME clock): a freed
//!   extent enters a bounded FIFO of `(extent, gate_seq)` entries —
//!   `gate_seq` = the freeing SMO's **free-record journal seq**, the
//!   entry's highest seq — with its bitmap bit **still set** and its
//!   budget byte **not** freed. It becomes claimable only when
//!   [`ExtCore::advance_durable`] passes its `gate_seq` with the **durable
//!   journal tail**, i.e. only once a post-barrier root-ledger record's
//!   tail covers the freeing entry: per-SMO-entry floor pinning
//!   (`tail > free.seq ⇒ tail ≥ res.end`) then proves every flip of that
//!   entry is materialized in the durable structure — durability of a
//!   ledger *record* alone certified the wrong thing (the record's tail
//!   can sit below the flips it does not cover: the recycled-extent
//!   stale-route mechanism, child-seq mount refusals). Hence any route
//!   replay can reach — mounted image or window flip — references only
//!   never-overwritten extents (risk R3, §4.7 restored to the letter).
//!   The FIFO is a bounded Vyukov-stamped MPMC ring; the drain stops at
//!   the first entry the watermark does not cover — complete because gate
//!   seqs are **non-decreasing in push order** (journal positions are
//!   monotonic and frees are pushed by the serialized per-volume
//!   checkpoint/SMO task, §4.6; debug-asserted). A full FIFO refuses the
//!   free ([`PendingFreeFull`]) — §4.7: "capped; pressure forces a
//!   checkpoint rather than unsafe reuse" — and [`ExtCore::pending_has_room`]
//!   is the producer-side headroom probe that makes the caller's
//!   at-cap protocol (SMO admission refusal + forced checkpoint cycles)
//!   sound: the serialized SMO task is the only producer, so headroom
//!   observed at admission still holds at the post-swap push (consumers
//!   only ever vacate slots). The cap is a *pressure valve*, never a
//!   liveness gate: retirements whose refusal would close the §4.7
//!   pinned-floor dependency cycle — the checkpoint flush pass's own
//!   compactions, and mount-side re-parking of replayed in-window frees
//!   — go through [`ExtCore::free_pending_forced`], which parks in an
//!   unbounded mutex-guarded overflow at cap instead of refusing (P2
//!   2026-07-26 §9 fix direction a). Overflow entries ride the same
//!   non-decreasing-gate order and the same durable-tail release clock;
//!   only the container differs.
//! - **Reserve accounting** (§4.7 ENOSPC semantics): `reserve` extents
//!   (production: `max(8, 2 %)` — the wrapper computes it) are claimable
//!   only by [`AllocClass::Internal`] (compaction / checkpoint / SMO
//!   internals). An [`AllocClass::User`] claim fails once granting it
//!   would dip the free budget to-or-below the reserve — the metadata op
//!   surfaces ENOSPC while the tree can still fold appends and free
//!   space: no write-to-free-space deadlock.
//!
//! ## Append partitioning (pre-RC engineering spec §6.2 item 3)
//!
//! Everything above is **whole-volume single-appender**: one free budget,
//! one scan hint, one pending-free FIFO, and one `advance_durable` tail
//! for the entire bitmap. Two appenders on one volume would claim from one
//! budget, park into one FIFO whose gate seqs come from two incomparable
//! ring spaces, and — via the wrapper — write one page's A/B slots from
//! two sides.
//!
//! The partitioned form ([`PartitionMap`]) splits the bitmap by **page**:
//!
//! * the page is the A/B write unit, so one owner per page is exactly
//!   what keeps the alternate-slot discipline single-appender;
//! * ownership is `page % writers` — **interleaved, not contiguous** — so
//!   an existing volume's allocation (which is dense at the low end)
//!   spreads roughly evenly across appenders instead of handing writer 0 a
//!   full partition and its peers empty ones;
//! * each partition carries its own free budget, scan hint, compaction
//!   reserve floor, pending-free FIFO + overflow, and durable tail. Gate
//!   seqs are positions in the freeing appender's OWN journal ring, so a
//!   single tail would release a peer's parked extent while replay can
//!   still route into it — the §4.7 CoW reuse rule demands one clock per
//!   appender.
//! * the bitmap **words stay shared**: a bit is a bit, and every mutation
//!   is routed to the owning partition ([`ExtCore::release`],
//!   [`ExtCore::free_pending`], [`ExtCore::mark_allocated`]), so a writer
//!   that touches a foreign extent shows up in the *owner's* accounting
//!   rather than silently corrupting its own.
//!
//! Solo (`writers == 1`) is one partition owning every extent — the
//! shipped structure, the shipped scan loop (the partitioned scan's
//! page-skipping arm is branched around), and the shipped costs.
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

#[cfg(loom)]
pub(crate) mod sync {
    pub use loom::sync::Mutex;
}
#[cfg(not(loom))]
pub(crate) mod sync {
    pub use std::sync::Mutex;
}

use atomic::{AtomicU64, Ordering};
use sync::Mutex;

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
    /// RES-14 (pre-RC engineering spec §7): the free-budget CAS won an
    /// entitlement the bitmap cannot honour — a full rescan found no
    /// clear bit, repeatedly. The budget and the bitmap disagree, which
    /// the protocol says is impossible; the pre-fix code SPUN on it,
    /// forever, inside a sync fn called from async. It is now a loud
    /// typed refusal, kept distinct from [`Self::NoSpace`] so an operator
    /// never reads a broken invariant as an honest full heap. The won
    /// entitlement is returned to the budget before this is raised.
    InvariantDrift,
}

/// RES-14: full-bitmap rescans a [`ExtCore::claim`] will do before
/// declaring [`ClaimError::InvariantDrift`]. A rescan is only ever needed
/// because a concurrent release landed behind the cursor, so the
/// legitimate need is O(racing releases); this is a generous ceiling on
/// that, not a tuning knob.
const CLAIM_RESCAN_LIMIT: u32 = 64;

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
    /// The coverage gate (§4.7 + design-smo-replay-currency §2-A): the
    /// freeing entry's free-record journal seq — released only once the
    /// durable tail passes it.
    retire_seq: AtomicU64,
}

/// How the bitmap is split across appenders (spec §6.2 item 3; module
/// docs). The partition unit is the **bitmap page** — the A/B write unit —
/// and ownership is interleaved (`page % writers`).
///
/// `extents_per_page` is [`super::alloc_ext::ALLOC_PAGE_BITS`] in
/// production; tests and loom models pass small values to make partition
/// interleavings reachable. Solo carries `writers == 1`, which makes every
/// owner query 0 and every ownership branch a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionMap {
    writers: u64,
    extents_per_page: u64,
}

impl PartitionMap {
    /// The shipped single-appender map: one partition owning every page.
    pub fn solo(extents_per_page: u64) -> Self {
        Self::new(1, extents_per_page)
    }

    /// A `writers`-way map over pages of `extents_per_page` bits.
    pub fn new(writers: u64, extents_per_page: u64) -> Self {
        assert!(writers > 0, "a partition map needs at least one appender");
        assert!(extents_per_page > 0, "degenerate bitmap page");
        Self {
            writers,
            extents_per_page,
        }
    }

    /// Appenders this map splits the bitmap across.
    pub fn writers(&self) -> u64 {
        self.writers
    }

    /// Extents covered by one bitmap page.
    pub fn extents_per_page(&self) -> u64 {
        self.extents_per_page
    }

    /// Whether this is the shipped single-appender map.
    pub fn is_solo(&self) -> bool {
        self.writers == 1
    }

    /// The appender that owns bitmap page `page`.
    pub fn owner_of_page(&self, page: u64) -> u64 {
        page % self.writers
    }

    /// The appender that owns `extent` (its page's owner).
    pub fn owner_of_extent(&self, extent: u64) -> u64 {
        if self.writers == 1 {
            return 0;
        }
        self.owner_of_page(extent / self.extents_per_page)
    }
}

/// One appender's half of the allocator state (module docs): its claimable
/// budget, its scan hint, its pending-free FIFO + forced-retirement
/// overflow, and its own durable-coverage clock. A solo volume has exactly
/// one of these, holding the whole bitmap.
struct Partition {
    /// Claimable clear bits **in this partition's pages** (pending-free
    /// bits are set, so they are neither clear nor counted).
    free_budget: AtomicU64,
    /// Next-scan hint (monotonic-ish; reset downward on release).
    hint: AtomicU64,
    /// Newest durable-coverage watermark for THIS appender's journal ring
    /// (its post-barrier tail). Per partition because gate seqs live in
    /// the freeing appender's own logical space — one shared clock would
    /// release a peer's parked extent that replay can still route into.
    durable_seq: AtomicU64,
    /// Bounded pending-free FIFO (Vyukov-stamped ring).
    pending: Box<[PendingSlot]>,
    /// FIFO producer cursor (a position, not an index).
    pending_head: AtomicU64,
    /// FIFO consumer cursor (a position, not an index).
    pending_tail: AtomicU64,
    /// Debug guard: retire seqs must be non-decreasing in push order
    /// (§4.6's serialized checkpoint/SMO task guarantees it — per
    /// appender, which is why the guard lives per partition).
    last_retire_seq: AtomicU64,
    /// FORCED-retirement overflow (the §4.7 cycle-break, P2 2026-07-26
    /// §9 fix direction a): retirements the checkpoint cycle's flush pass
    /// must park even when the bounded FIFO is at cap — refusing THE SMO
    /// whose completion discharges the tail-pinning floor is the closed
    /// dependency cycle (parked frees ↔ pinned tail ↔ refused compaction)
    /// that wedged live volumes AND their remounts. Same `(extent,
    /// gate_seq)` tuples, same non-decreasing-gate order (one serialized
    /// producer), same durable-tail release clock — only the container
    /// is unbounded. Boundedness in fact: the flush pass pushes at most
    /// one per dirty node per cycle and every barriered cycle's tail
    /// advance drains all previously-parked entries (the progress
    /// theorem on the tree's `smo_replace` forced arm), so occupancy
    /// is bounded by ~two cycles of flush-pass SMOs. A cold pressure
    /// path: mutex-guarded, `overflow_len` keeps the hot paths lock-free
    /// when it is empty (the steady state).
    overflow: Mutex<std::collections::VecDeque<(u64, u64)>>,
    /// Lock-free mirror of `overflow.len()` (hot-path emptiness checks).
    overflow_len: AtomicU64,
}

impl Partition {
    fn new(budget: u64, first_extent: u64, pending_cap: usize) -> Self {
        let pending: Vec<PendingSlot> = (0..pending_cap)
            .map(|i| PendingSlot {
                stamp: AtomicU64::new(i as u64),
                extent: AtomicU64::new(0),
                retire_seq: AtomicU64::new(0),
            })
            .collect();
        Self {
            free_budget: AtomicU64::new(budget),
            hint: AtomicU64::new(first_extent),
            durable_seq: AtomicU64::new(0),
            pending: pending.into_boxed_slice(),
            pending_head: AtomicU64::new(0),
            pending_tail: AtomicU64::new(0),
            last_retire_seq: AtomicU64::new(0),
            overflow: Mutex::new(std::collections::VecDeque::new()),
            overflow_len: AtomicU64::new(0),
        }
    }
}

/// The pure lock-free extent-allocator core. See the module docs for the
/// protocols; the loom models in `loom-models/` pin the invariants: no
/// double-alloc under concurrent claimers, a pending-freed extent never
/// claimable before its checkpoint-durable seq, reserve isolation (user
/// claims can never consume the compaction reserve), and — since the
/// append partitioning — that two appenders' claims and coverage gates
/// never cross.
pub struct ExtCore {
    /// One bit per extent; bit set == allocated **or** pending-free.
    /// SHARED across partitions (a bit is a bit); every mutation routes to
    /// the OWNING partition's accounting, so a writer that touches a
    /// foreign extent moves the owner's budget rather than silently
    /// desynchronizing its own.
    words: Box<[AtomicU64]>,
    /// Exclusive upper bound on extent indices.
    total: u64,
    /// Compaction reserve in extents (§4.7), **per partition**: claimable
    /// by `Internal` only. Solo carries the whole-volume reserve, so the
    /// shipped semantics are byte-for-byte unchanged.
    reserve: u64,
    /// How the bitmap is split across appenders.
    map: PartitionMap,
    /// Per-appender state, indexed by writer id.
    parts: Box<[Partition]>,
}

impl ExtCore {
    /// Build an all-free **solo** core over `total` extents with `reserve`
    /// of them held back for `Internal` claims and a pending-free FIFO of
    /// `pending_cap` entries — the shipped single-appender allocator.
    /// `reserve` must leave claimable space; `pending_cap ≥ 2` —
    /// hard-asserted (construction-time, once per volume) because a 1-slot
    /// Vyukov ring aliases its stamps: "full at position p" and "vacant
    /// for position p+1" would both read `p + 1`, silently overwriting an
    /// un-drained entry — a §4.7 gate bypass.
    pub fn new(total: u64, reserve: u64, pending_cap: usize) -> Self {
        // One partition owning every page: `extents_per_page = total`
        // makes the whole bitmap page 0 by construction.
        Self::new_partitioned(total, reserve, pending_cap, PartitionMap::solo(total))
    }

    /// Build an all-free core whose bitmap is partitioned per `map`
    /// (spec §6.2 item 3; module docs). `reserve` is the WHOLE-volume
    /// compaction reserve — it is divided across appenders, since each one
    /// must keep its own §4.7 fold-and-free headroom — and `pending_cap`
    /// is the whole-volume pending-free FIFO budget, likewise divided
    /// (a partition's SMO rate is ~1/N of the volume's, so the memory
    /// footprint stays constant as appenders are added).
    pub fn new_partitioned(
        total: u64,
        reserve: u64,
        pending_cap: usize,
        map: PartitionMap,
    ) -> Self {
        assert!(total > 0, "degenerate heap");
        assert!(reserve < total, "reserve must leave claimable extents");
        assert!(
            pending_cap >= 2,
            "pending-free FIFO needs ≥ 2 slots (Vyukov stamp aliasing at 1)"
        );
        let writers = map.writers();
        let per_reserve = if writers == 1 {
            reserve
        } else {
            reserve.div_ceil(writers)
        };
        let per_pending = if writers == 1 {
            pending_cap
        } else {
            (pending_cap / writers as usize).max(2)
        };
        let words: Vec<AtomicU64> = (0..total.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        let parts: Vec<Partition> = (0..writers)
            .map(|w| {
                let budget = partition_extent_count(total, map, w);
                assert!(
                    per_reserve < budget || budget == 0,
                    "per-appender reserve must leave claimable extents in every partition"
                );
                Partition::new(budget, first_extent_of(map, w), per_pending)
            })
            .collect();
        Self {
            words: words.into_boxed_slice(),
            total,
            reserve: per_reserve,
            map,
            parts: parts.into_boxed_slice(),
        }
    }

    /// Total extents.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// The compaction reserve in extents, **per appender** (identical to
    /// the whole-volume reserve on a solo volume).
    pub fn reserve(&self) -> u64 {
        self.reserve
    }

    /// How the bitmap is split across appenders.
    pub fn map(&self) -> PartitionMap {
        self.map
    }

    /// The appender's state. **Index, never modulo**: `writer %
    /// parts.len()` on a runtime length emits a 64-bit division, and this
    /// sits on the claim / release / free_pending path — the solo bracket
    /// measured it at ~7 ns/claim, i.e. ~11 % of a claim, paid by a volume
    /// with exactly one partition to choose from
    /// (`.benchmarks/2026-08-05-mw-partitioned-append.md` §3). An
    /// out-of-range writer id is a caller bug, not a case to be folded.
    fn part(&self, writer: u64) -> &Partition {
        match self.parts.get(writer as usize) {
            Some(p) => p,
            None => {
                debug_assert!(false, "writer {writer} outside the partition set");
                &self.parts[0]
            }
        }
    }

    /// Claimable extents right now, across every partition (excludes
    /// allocated and pending-free).
    pub fn free_extents(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| p.free_budget.load(Ordering::Acquire))
            .sum()
    }

    /// Claimable extents in `writer`'s partition alone.
    pub fn free_extents_in(&self, writer: u64) -> u64 {
        self.part(writer).free_budget.load(Ordering::Acquire)
    }

    /// FIFO occupancy alone (cursor distance; includes in-flight pushes).
    fn fifo_count(&self, p: &Partition) -> u64 {
        let head = p.pending_head.load(Ordering::Acquire);
        let tail = p.pending_tail.load(Ordering::Acquire);
        head.saturating_sub(tail)
    }

    /// Entries currently parked awaiting durable-tail coverage — the
    /// bounded FIFO **plus** the forced-retirement overflow, summed over
    /// every partition (the wedge-detector gauge `meta_kv_pending_free`
    /// counts every parked retirement, wherever it is parked).
    pub fn pending_count(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| self.fifo_count(p) + p.overflow_len.load(Ordering::Acquire))
            .sum()
    }

    /// Parked retirements in `writer`'s partition alone.
    pub fn pending_count_in(&self, writer: u64) -> u64 {
        let p = self.part(writer);
        self.fifo_count(p) + p.overflow_len.load(Ordering::Acquire)
    }

    /// Producer-side headroom probe (the §4.7 at-cap protocol's admission
    /// check): whether one more [`Self::free_pending`] would fit in
    /// `writer`'s FIFO. Sound for the serialized SMO task — the FIFO's
    /// only producer — because a racing [`Self::advance_durable_in`] only
    /// ever *vacates* slots, so headroom observed here still holds at the
    /// later push. Speaks only for the bounded FIFO: it is the *pressure
    /// valve* for threshold SMOs; forced retirements
    /// ([`Self::free_pending_forced`]) never consult it.
    pub fn pending_has_room_in(&self, writer: u64) -> bool {
        let p = self.part(writer);
        self.fifo_count(p) < p.pending.len() as u64
    }

    /// [`Self::pending_has_room_in`] for the solo/authority partition.
    pub fn pending_has_room(&self) -> bool {
        self.pending_has_room_in(0)
    }

    /// Newest durable-coverage watermark for `writer` (its own ring's
    /// durable journal tail since the Option-A coverage fix).
    pub fn durable_seq_in(&self, writer: u64) -> u64 {
        self.part(writer).durable_seq.load(Ordering::Acquire)
    }

    /// [`Self::durable_seq_in`] for the solo/authority partition.
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq_in(0)
    }

    /// Claim a free extent for `class` from the solo/authority partition
    /// (on a solo volume that is the whole bitmap — the shipped call).
    pub fn claim(&self, class: AllocClass) -> Result<u64, ClaimError> {
        self.claim_in(0, class)
    }

    /// Claim a free extent for `class` **inside `writer`'s partition**.
    /// Lock-free: a free-budget CAS gated by the class floor (`User` must
    /// leave that partition's reserve behind), then a `fetch_or` bit scan
    /// whose 0→1 winner is unique (module docs). Errors with
    /// [`ClaimError::NoSpace`] — for `User`, the §4.7 ENOSPC signal with
    /// the appender's own reserve intact.
    pub fn claim_in(&self, writer: u64, class: AllocClass) -> Result<u64, ClaimError> {
        let part = self.part(writer);
        let floor = match class {
            AllocClass::User => self.reserve,
            AllocClass::Internal => 0,
        };
        // Budget first: win the entitlement to one claimable clear bit,
        // or refuse without touching the bitmap.
        let mut free = part.free_budget.load(Ordering::Acquire);
        loop {
            if free <= floor {
                return Err(ClaimError::NoSpace);
            }
            match part.free_budget.compare_exchange(
                free,
                free - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(cur) => free = cur,
            }
        }
        // The budget win entitles this caller to exactly one clear bit in
        // this partition; the scan must find one (module docs).
        let start = part.hint.load(Ordering::Relaxed).min(self.total);
        for _ in 0..CLAIM_RESCAN_LIMIT {
            // Solo keeps the shipped scan verbatim — one linear sweep from
            // the hint, wrapping once — so the single-appender claim path
            // pays nothing for the partitioning (the S4 "solo mode is
            // free" gate at the format layer).
            if self.map.is_solo() {
                for idx in (start..self.total).chain(0..start) {
                    if self.try_take_bit(idx) {
                        part.hint.store(idx + 1, Ordering::Relaxed);
                        return Ok(idx);
                    }
                }
            } else if let Some(idx) = self.scan_partition(writer, part, start) {
                return Ok(idx);
            }
            // A release between the budget win and this pass can land
            // behind the cursor; rescan. The budget win guarantees a clear
            // bit exists and stays clear until some claimant (possibly
            // this one) takes it — so a handful of rescans is the entire
            // legitimate need, and RES-14 caps them: the pre-fix `loop`
            // had no exit at all if budget and bitmap ever disagreed, and
            // this runs SYNCHRONOUSLY on an async worker.
            #[cfg(loom)]
            loom::thread::yield_now();
            #[cfg(not(loom))]
            core::hint::spin_loop();
        }
        // RES-14: hand the entitlement back (the caller never got a bit)
        // and refuse LOUD in its own class. Callers with crate access
        // (`super::alloc_ext`) log the drift; this core stays
        // dependency-free for the loom model.
        part.free_budget.fetch_add(1, Ordering::AcqRel);
        Err(ClaimError::InvariantDrift)
    }

    /// The unique-winner bit take (`fetch_or`, module docs).
    fn try_take_bit(&self, idx: u64) -> bool {
        let w = (idx / 64) as usize;
        let mask = 1u64 << (idx % 64);
        self.words[w].fetch_or(mask, Ordering::AcqRel) & mask == 0
    }

    /// One partitioned scan pass: visit **only `writer`'s own pages**,
    /// ascending from `start`'s page and wrapping once to the partition's
    /// first page. Page granularity is what keeps a foreign page skipped
    /// in one step instead of one modulo per bit — and it is why the solo
    /// arm above can stay the shipped flat sweep.
    ///
    /// `writer`'s pages are exactly `{writer, writer + n, writer + 2n, …}`
    /// (`n = writers`, `writer < n`), so the sequence needs no modular
    /// arithmetic across the wrap.
    fn scan_partition(&self, writer: u64, part: &Partition, start: u64) -> Option<u64> {
        let epp = self.map.extents_per_page();
        let n = self.map.writers();
        let pages = self.total.div_ceil(epp);
        let start_page = (start / epp).min(pages);
        // The first owned page at-or-after `start`'s page.
        let first = if start_page <= writer {
            writer
        } else {
            writer + (start_page - writer).div_ceil(n) * n
        };

        let take = |from: u64, to: u64| -> Option<u64> {
            for idx in from..to {
                if self.try_take_bit(idx) {
                    part.hint.store(idx + 1, Ordering::Relaxed);
                    return Some(idx);
                }
            }
            None
        };

        // Pass 1: owned pages from `first` to the end of the bitmap; the
        // hint's own page starts at the hint.
        let mut p = first;
        while p < pages {
            let lo = p * epp;
            let hi = (lo + epp).min(self.total);
            let from = if p == start_page && start > lo && start < hi {
                start
            } else {
                lo
            };
            if let Some(idx) = take(from, hi) {
                return Some(idx);
            }
            p += n;
        }
        // Pass 2: wrap — owned pages below `first`.
        let mut p = writer;
        while p < first.min(pages) {
            let lo = p * epp;
            if let Some(idx) = take(lo, (lo + epp).min(self.total)) {
                return Some(idx);
            }
            p += n;
        }
        // Pass 3: the head of the hint's own page, skipped by pass 1.
        if start_page < pages && self.map.owner_of_page(start_page) == writer {
            let lo = start_page * epp;
            if start > lo {
                if let Some(idx) = take(lo, start.min(self.total)) {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// TEST SEAM (RES-14): inflate the free budget without clearing a
    /// bit — the invariant drift the protocol says cannot happen, so that
    /// the bounded-and-loud behaviour can be pinned deterministically
    /// (`tests/unbounded_loop_bounds_tests.rs`). Production never calls
    /// it; no crate dependency, so the loom model still compiles.
    pub fn inflate_free_budget_for_test(&self, n: u64) {
        self.part(0).free_budget.fetch_add(n, Ordering::AcqRel);
    }

    /// Mark `extent` allocated — mount seeding only (newest-valid bitmap
    /// page bits, then replayed journal alloc records). `&mut self` on
    /// purpose: it sets the bit *before* decrementing the budget, which is
    /// only safe while the core is not yet shared with claimers (module
    /// docs). Idempotent; out-of-range indices are ignored. The budget
    /// decrement lands on the extent's OWNING partition.
    pub fn mark_allocated(&mut self, extent: u64) {
        if extent >= self.total {
            return;
        }
        let w = (extent / 64) as usize;
        let mask = 1u64 << (extent % 64);
        if self.words[w].fetch_or(mask, Ordering::AcqRel) & mask == 0 {
            let owner = self.map.owner_of_extent(extent);
            let prev = self.part(owner).free_budget.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(prev >= 1, "budget underflow marking {extent}");
        }
    }

    /// Release `extent` directly to the claimable pool: bit cleared, then
    /// the OWNING partition's budget incremented (that order — module
    /// docs). Only for extents **no root-ledger record references**: a
    /// build that failed before its extent was ever published, or the
    /// durable-advance drain below. A checkpoint-referenced extent must go
    /// through [`Self::free_pending`] + [`Self::advance_durable_in`]
    /// (§4.7). Idempotent; out-of-range indices are ignored.
    pub fn release(&self, extent: u64) {
        if extent >= self.total {
            return;
        }
        let w = (extent / 64) as usize;
        let mask = 1u64 << (extent % 64);
        if self.words[w].fetch_and(!mask, Ordering::AcqRel) & mask != 0 {
            let part = self.part(self.map.owner_of_extent(extent));
            part.free_budget.fetch_add(1, Ordering::AcqRel);
            // Prefer reusing low freed extents (locality under churn).
            let _ = part.hint.fetch_min(extent, Ordering::Relaxed);
        }
    }

    /// Enter `extent` into its OWNER's pending-free FIFO gated on
    /// `retire_seq` — the freeing entry's free-record journal seq in that
    /// appender's own ring (§4.7 + design-smo-replay-currency §2-A:
    /// release requires the durable tail to pass it, which per-SMO-entry
    /// floor pinning makes equivalent to "the whole freeing entry is
    /// checkpoint-covered"). The bit stays set and the budget untouched
    /// until [`Self::advance_durable_in`] covers the tag. Errors with
    /// [`PendingFreeFull`] at capacity (the §4.7 cap — the caller forces a
    /// checkpoint, never reuses unsafely).
    ///
    /// Routing by owner rather than by caller is deliberate: an appender
    /// only ever frees extents it owns, so the routing is a no-op in
    /// correct operation and a *containment* mechanism when it is not (the
    /// gate seq lands on the clock that can actually cover it).
    ///
    /// Contract: `retire_seq`s are non-decreasing in push order **per
    /// partition** — guaranteed by the serialized per-volume
    /// checkpoint/SMO task (§4.6) — and the extent's bit is set (it was
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
        let part = self.part(self.map.owner_of_extent(extent));
        debug_assert!(
            part.last_retire_seq.fetch_max(retire_seq, Ordering::AcqRel) <= retire_seq,
            "retire seqs must be non-decreasing in push order (§4.6 serialized SMO task)"
        );
        let cap = part.pending.len() as u64;
        let mut pos = part.pending_head.load(Ordering::Acquire);
        loop {
            let slot = &part.pending[(pos % cap) as usize];
            let stamp = slot.stamp.load(Ordering::Acquire);
            if stamp == pos {
                // Slot vacant for this position: claim it.
                match part.pending_head.compare_exchange(
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
                // A lap-old stamp means either (a) the FIFO is genuinely
                // full — every slot occupied, cursor distance == cap — or
                // (b) a draining consumer won this slot's previous-lap
                // entry (tail already bumped) but has not yet stored the
                // vacate stamp. (b) must NOT surface as full: the §4.7
                // at-cap protocol's admission headroom check
                // ([`Self::pending_has_room_in`]) reads the cursors, so a
                // cursor-level vacancy the producer then fails to push
                // into would break the clause-a soundness argument (the
                // post-swap push must succeed under observed headroom —
                // the loom model pins exactly this interleaving).
                // Distinguish via the cursor distance and spin out the
                // consumer's store — it is instructions away.
                let tail = part.pending_tail.load(Ordering::Acquire);
                if pos.saturating_sub(tail) >= cap {
                    // Genuinely full (§4.7 cap).
                    return Err(PendingFreeFull);
                }
                #[cfg(loom)]
                loom::thread::yield_now();
                #[cfg(not(loom))]
                core::hint::spin_loop();
                pos = part.pending_head.load(Ordering::Acquire);
            } else {
                // A racing producer advanced the head past our read.
                pos = part.pending_head.load(Ordering::Acquire);
            }
        }
    }

    /// [`Self::free_pending`] that can NEVER be refused (the §4.7
    /// cycle-break, design-cow-kv-metadata §4.7 / P2 2026-07-26 §9 fix
    /// direction a): try the owner's bounded FIFO first; at cap, park in
    /// its unbounded overflow instead — same gate seq, same release clock
    /// ([`Self::advance_durable_in`]). Returns `true` when the entry
    /// overflowed (the caller's engagement counter). Reserved for
    /// retirements whose refusal would close the pinned-floor dependency
    /// cycle: the checkpoint cycle's flush-pass compactions (which are
    /// what discharge tail-pinning floors) and mount-side re-parking of
    /// replayed in-window frees (a recovered window may legitimately
    /// carry more frees than the cap). Same producer contract as
    /// `free_pending`: serialized SMO task / mount bootstrap,
    /// non-decreasing gate seqs per partition, bit already set.
    pub fn free_pending_forced(&self, extent: u64, retire_seq: u64) -> bool {
        if self.free_pending(extent, retire_seq).is_ok() {
            return false;
        }
        let part = self.part(self.map.owner_of_extent(extent));
        let mut g = part.overflow.lock().unwrap();
        debug_assert!(
            g.back().is_none_or(|&(_, s)| s <= retire_seq),
            "overflow gate seqs must be non-decreasing in push order"
        );
        g.push_back((extent, retire_seq));
        part.overflow_len.store(g.len() as u64, Ordering::Release);
        true
    }

    /// Advance the solo/authority partition's durable-coverage watermark —
    /// the shipped call (on a solo volume there is exactly one clock).
    pub fn advance_durable(&self, seq: u64) -> Vec<u64> {
        self.advance_durable_in(0, seq)
    }

    /// Advance **`writer`'s** durable-coverage watermark to `seq`
    /// (monotonic `fetch_max`; stale advances are no-ops) and drain every
    /// pending entry in that partition the watermark now covers: bit
    /// cleared, budget incremented — the extent is claimable again, and
    /// only now. `seq` is the **durable journal tail** of a post-barrier
    /// root-ledger record written by THAT appender
    /// (design-smo-replay-currency §2-A): a tail past the free record's
    /// seq proves the whole freeing entry — flips included — is
    /// materialized in the durable structure, which is what §4.7's "any
    /// state replay can select references only never-overwritten extents"
    /// actually requires (record durability alone certified less).
    ///
    /// One clock per appender is load-bearing: gate seqs are positions in
    /// the freeing appender's own ring, so a peer's tail says nothing
    /// about them — a shared clock would release an extent another
    /// appender's replay window still routes into (spec §6.2 item 3).
    ///
    /// Returns the released extents (the wrapper marks their bitmap pages
    /// dirty).
    pub fn advance_durable_in(&self, writer: u64, seq: u64) -> Vec<u64> {
        let part = self.part(writer);
        part.durable_seq.fetch_max(seq, Ordering::AcqRel);
        let durable = part.durable_seq.load(Ordering::Acquire);
        let cap = part.pending.len() as u64;
        let mut released = Vec::new();
        loop {
            let pos = part.pending_tail.load(Ordering::Acquire);
            let head = part.pending_head.load(Ordering::Acquire);
            if pos == head {
                break; // FIFO empty.
            }
            let slot = &part.pending[(pos % cap) as usize];
            let stamp = slot.stamp.load(Ordering::Acquire);
            if stamp != pos + 1 {
                // The tail entry's producer claimed the position but has
                // not published yet (or a racing consumer just vacated
                // it); a later advance drains it.
                break;
            }
            // Peek is stable while stamp == pos + 1: a producer can only
            // reuse the slot after a consumer stamps pos + cap, and only
            // the tail-CAS winner below does that.
            let retire_seq = slot.retire_seq.load(Ordering::Relaxed);
            if retire_seq > durable {
                // FIFO order + non-decreasing tags ⇒ nothing behind this
                // entry is releasable either: the §4.7 gate holds.
                break;
            }
            let extent = slot.extent.load(Ordering::Relaxed);
            if part
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
        // The forced-retirement overflow drains on the same clock — the
        // gate check is identical per entry (release iff the durable tail
        // covers its seq; front-stop is exact because gates are
        // non-decreasing in push order). Lock-free when empty (the
        // steady state); the mutex serializes racing consumers so each
        // entry releases exactly once.
        if part.overflow_len.load(Ordering::Acquire) > 0 {
            let mut g = part.overflow.lock().unwrap();
            while let Some(&(extent, gate)) = g.front() {
                if gate > durable {
                    break;
                }
                g.pop_front();
                self.release(extent);
                released.push(extent);
            }
            part.overflow_len.store(g.len() as u64, Ordering::Release);
        }
        released
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

/// Extents in `writer`'s partition of a `total`-extent bitmap under `map`
/// — the partition's initial free budget.
fn partition_extent_count(total: u64, map: PartitionMap, writer: u64) -> u64 {
    if map.is_solo() {
        return total;
    }
    let epp = map.extents_per_page();
    let n = map.writers();
    let pages = total.div_ceil(epp);
    let mut count = 0;
    let mut page = writer;
    while page < pages {
        let lo = page * epp;
        count += (lo + epp).min(total) - lo;
        page += n;
    }
    count
}

/// The first extent `writer` owns (its scan-hint seed).
fn first_extent_of(map: PartitionMap, writer: u64) -> u64 {
    if map.is_solo() {
        0
    } else {
        writer * map.extents_per_page()
    }
}
