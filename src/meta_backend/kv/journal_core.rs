//! Pure lock-free journal-ring reservation & admission core (design §4.4
//! pts 2/5, §4.6 pt 3).
//!
//! Self-contained (no crate dependencies) so the `loom-models` crate can
//! `#[path]`-include this file and exhaustively model-check the admission /
//! reservation / watermark interleavings under `cfg(loom)`, exactly like
//! `alloc_core.rs`. The I/O wrapper ([`super::journal`]) adds page/entry
//! framing and `uring_fs` writes; nothing in this module touches a device.
//!
//! ## The logical byte space
//!
//! Ring positions are **monotonic u64 logical byte offsets** that never
//! wrap: the ring's 4 KiB pages contribute only their *data* bytes (the
//! 24 B page-header slots are excluded by a fixed logical→physical mapping,
//! design §4.4 pt 2 — entry bytes can never overwrite headers and
//! multi-page entries need no special casing). Lap and in-ring offset are
//! *derived*: `lap = pos / logical_len`, `offset = pos % logical_len`. A
//! monotonic position is the packed `(lap, logical-offset)` word of §4.4
//! pt 2 in its canonical form.
//!
//! ## seq ≡ reservation start position (K3 resolution)
//!
//! §4.4 pt 2 specifies "`seq`/ring-offset assignment is a **single** atomic
//! `fetch_add`". One `fetch_add` can hand out one value, so the entry seq
//! **is** the reservation's logical start position: unique, strictly
//! monotonic in reservation order, and — because reservations happen inside
//! the node locks — per-key seq order equals RAM apply order, which is all
//! the fold algebra (K1) requires. Replay exploits the identity: a valid
//! entry at logical position `p` must carry `seq == p`, which rejects
//! stale previous-lap bytes on an entry chain deterministically (the page
//! `lap` field covers discovery; the seq identity covers chains).
//!
//! ## Budget accounting (§4.4 pt 5)
//!
//! Two counters partition ring claims:
//!
//! - `head`: bytes reserved (position of the next reservation);
//! - `admitted`: bytes admitted but not yet transferred to `head`.
//!
//! The invariant is `head + admitted ≤ reusable_upto + logical_len − reserve`
//! for user admissions (`reserve = 0` for the checkpoint task's own records
//! — the carve-out is *theirs*). `reserve` never moves, so the checkpoint /
//! writeback / SMO task can always admit its interior-pointer and alloc/free
//! records even when user commits have parked (the R10 liveness argument).
//!
//! ### Why the two-counter read is safe
//!
//! [`JournalCore::try_admit`] reads `admitted` **before** `head`, while
//! [`JournalCore::reserve`] (the transfer) bumps `head` **before**
//! decrementing `admitted`. Any interleaved observation therefore sees
//! `admitted + head` at-or-above the true claim — the transferring bytes
//! are transiently counted twice, never zero times (the under-count would
//! need `admitted` read *after* its decrement and `head` read *before* its
//! increment, which the two orderings jointly forbid). Admission can be
//! spuriously refused under a race, but can never over-commit the ring.
//! The CAS on `admitted` re-validates after any concurrent admission,
//! release, or transfer-completion touching it. `reusable_upto` only
//! grows, so a stale read is conservative too.
//!
//! ## `reusable_upto` (§4.6 pt 3)
//!
//! The ring-reclamation watermark: positions strictly below it may be
//! overwritten. It advances only after the root-ledger record that retired
//! them is *known durable* (post-barrier) — never merely because the in-RAM
//! tail moved — so the head can never overwrite the pages a
//! torn-newest-ledger-slot fallback would need to replay. The watermark is
//! monotonic (`fetch_max`) and debug-asserted never to pass the head.

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::{AtomicU64, Ordering};
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::{AtomicU64, Ordering};
}

use atomic::{AtomicU64, Ordering};

/// Ring geometry: everything the pure core needs to derive laps, offsets,
/// page indices, and physical segments from monotonic logical positions.
///
/// Parametric on purpose (the loom models and unit tests use tiny pages to
/// make wrap interleavings reachable); the production ring passes
/// [`super::journal::JOURNAL_PAGE_DATA_LEN`] and the §4.4 pt 5 reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreGeometry {
    /// Entry-byte capacity of one page (production: 4096 − 24 = 4072).
    pub page_data_len: u64,
    /// Number of pages in the ring.
    pub pages: u64,
    /// Checkpoint-task carve-out in logical bytes (§4.4 pt 5:
    /// `max(256 KiB, ring/64)` for production rings): admissible only by
    /// [`AdmissionClass::Checkpoint`], invisible to user admissions.
    pub reserve_bytes: u64,
}

impl CoreGeometry {
    /// Total logical bytes per lap (`pages × page_data_len`).
    pub fn logical_len(&self) -> u64 {
        self.pages * self.page_data_len
    }

    /// Which lap `pos` belongs to.
    pub fn lap(&self, pos: u64) -> u64 {
        pos / self.logical_len()
    }

    /// `pos`'s offset within its lap (`pos % logical_len`).
    pub fn ring_offset(&self, pos: u64) -> u64 {
        pos % self.logical_len()
    }

    /// Physical page index holding `pos` (`(pos / page_data_len) % pages`).
    pub fn page_index(&self, pos: u64) -> u64 {
        (pos / self.page_data_len) % self.pages
    }

    /// The logical position of the first byte of `pos`'s page
    /// (`pos` aligned down to `page_data_len`).
    pub fn page_start_pos(&self, pos: u64) -> u64 {
        pos - (pos % self.page_data_len)
    }

    /// `pos`'s offset within its page's data area (`pos % page_data_len`).
    pub fn in_page_off(&self, pos: u64) -> u64 {
        pos % self.page_data_len
    }

    /// Map the logical range `[start, start + len)` onto physical page-data
    /// segments, in logical order. Segments are contiguous: the first may
    /// begin mid-page, the last may end mid-page, and every interior
    /// segment covers its page's whole data area — the §4.4 pt 2 "no
    /// special casing" shape of a multi-page reservation.
    pub fn segments(&self, start: u64, len: u64) -> Vec<PageSegment> {
        let mut out = Vec::new();
        let mut pos = start;
        let end = start + len;
        while pos < end {
            let in_page = self.in_page_off(pos);
            let take = (self.page_data_len - in_page).min(end - pos);
            out.push(PageSegment {
                page: self.page_index(pos),
                data_off: in_page,
                len: take,
            });
            pos += take;
        }
        out
    }

    /// Logical positions of every page-first-byte inside
    /// `[start, start + len)` — the pages whose 24 B header the owner of
    /// this reservation writes (§4.4 pt 2: "exactly one committer's
    /// reservation contains each page's first logical byte").
    pub fn owned_page_starts(&self, start: u64, len: u64) -> Vec<u64> {
        let end = start + len;
        // First page-start position ≥ start.
        let mut pos = match start % self.page_data_len {
            0 => start,
            rem => start + (self.page_data_len - rem),
        };
        let mut out = Vec::new();
        while pos < end {
            out.push(pos);
            pos += self.page_data_len;
        }
        out
    }
}

/// One physical page-data segment of a logical range (see
/// [`CoreGeometry::segments`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSegment {
    /// Physical page index within the ring.
    pub page: u64,
    /// Byte offset within the page's *data* area (0-based; the I/O layer
    /// adds the 24 B header skip).
    pub data_off: u64,
    /// Segment length in bytes.
    pub len: u64,
}

/// Who is asking for ring space (§4.4 pt 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionClass {
    /// A user commit: bounded by `capacity − reserve_bytes`.
    User,
    /// The per-volume checkpoint/writeback/SMO task's own records: bounded
    /// by the full capacity (the carve-out is exactly the difference), so
    /// SMOs never wait on ring space behind user commits.
    Checkpoint,
}

/// Admitted-but-not-reserved ring budget. Must be either transferred to the
/// head ([`JournalCore::reserve`]) or given back ([`JournalCore::release`]);
/// dropping it on the floor leaks budget forever (the owning module's tests
/// pin the conservation accounting that would catch it).
#[derive(Debug)]
#[must_use = "admitted budget must be reserved or released, or the ring leaks"]
pub struct Admission {
    len: u64,
}

impl Admission {
    /// The admitted length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether nothing was admitted (a zero-length split remainder).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A claimed journal range: `[start, start + len)` in logical byte space.
/// `seq() == start` — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    /// Logical start position (== the entry seq).
    pub start: u64,
    /// Reserved length in bytes (== the exact entry size, §4.4 pt 5).
    pub len: u64,
}

impl Reservation {
    /// The entry seq this reservation carries (its start position).
    pub fn seq(&self) -> u64 {
        self.start
    }

    /// One past the last logical byte (`start + len`).
    pub fn end(&self) -> u64 {
        self.start + self.len
    }

    /// The POSITION-domain seq of this entry's `i`-th record — what a
    /// §4.7 pending free parks on and the durable tail (a position)
    /// covers: `start + i`, inside `[start, end())` since a record is at
    /// least one byte. The record's STAMP is `seq_base + i` (the overlay's
    /// domain, [`SeqSpan`]); on a ring stamping above its positions a
    /// free gated on the stamp waited `seq_offset` bytes past its entry
    /// (PR 4 review round 3, Issue 20's third site).
    pub fn record_gate(&self, i: usize) -> u64 {
        self.start + i as u64
    }
}

/// **The RECORD-SEQ span of one journal entry or one contiguous batch —
/// the ONE domain every overlay address uses** (PR 4 review round 3,
/// Issue 20). Records are stamped `seq_base + i` from the base the
/// ring's `reserve_registered` answers (`position + seq_offset`, §5.8.2's
/// seq-space law), so a `len`-byte entry's stamps lie in `[seq_base,
/// seq_base + len)` (a record is at least one byte) and adjacent members
/// of a batch occupy disjoint spans. A [`Reservation`] is the POSITION
/// domain — holes, coverage, floors, the ring's own arithmetic — and the
/// two differ by the ring's offset on any ring that ever received a
/// handed-over slot; the §4.4 pt 4 rollback arms that addressed the
/// overlay by the position range removed nothing there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqSpan {
    /// The first record seq the span holds.
    pub lo: u64,
    /// One past the last.
    pub hi: u64,
}

impl SeqSpan {
    /// The span of an entry (or batch) of `len` bytes stamped from
    /// `seq_base`.
    pub fn stamped(seq_base: u64, len: u64) -> Self {
        Self {
            lo: seq_base,
            hi: seq_base.saturating_add(len),
        }
    }

    /// Whether a record seq lies inside the span.
    #[inline]
    pub fn contains(&self, seq: u64) -> bool {
        seq >= self.lo && seq < self.hi
    }
}

/// The lock-free reservation/admission core. See the module docs for the
/// protocol; the loom models in `loom-models/` pin its invariants:
/// no reservation overlap, seq monotonic, lap/wrap correctness, multi-page
/// contiguity, no ring over-commit, admitted-byte conservation across
/// transfer/release, and reserve-not-consumable-by-users.
pub struct JournalCore {
    geo: CoreGeometry,
    /// Next reservation's logical start position (monotonic).
    head: AtomicU64,
    /// Bytes admitted but not yet transferred to `head`.
    admitted: AtomicU64,
    /// Ring-reclamation watermark (§4.6 pt 3): positions strictly below it
    /// are overwritable.
    reusable_upto: AtomicU64,
}

impl JournalCore {
    /// Build a core over `geo`, resuming at `head_pos` (0 for a fresh ring;
    /// the replay-recovered head on remount) with the reclamation watermark
    /// at `reusable_upto` (0 fresh; the mounted ledger record's durable
    /// tail on remount — §4.6 pt 3).
    pub fn new(geo: CoreGeometry, head_pos: u64, reusable_upto: u64) -> Self {
        debug_assert!(geo.page_data_len > 0 && geo.pages > 0, "degenerate ring");
        debug_assert!(
            reusable_upto <= head_pos,
            "watermark {reusable_upto} past head {head_pos}"
        );
        debug_assert!(
            head_pos <= reusable_upto + geo.logical_len(),
            "head {head_pos} claims more than one lap past the watermark"
        );
        Self {
            geo,
            head: AtomicU64::new(head_pos),
            admitted: AtomicU64::new(0),
            reusable_upto: AtomicU64::new(reusable_upto),
        }
    }

    /// The ring geometry.
    pub fn geometry(&self) -> &CoreGeometry {
        &self.geo
    }

    /// Current head position (the next reservation's start).
    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// Bytes admitted but not yet reserved (diagnostics / accounting tests).
    pub fn admitted(&self) -> u64 {
        self.admitted.load(Ordering::Acquire)
    }

    /// Current ring-reclamation watermark (§4.6 pt 3).
    pub fn reusable_upto(&self) -> u64 {
        self.reusable_upto.load(Ordering::Acquire)
    }

    /// Admit `len` bytes of ring budget, or `None` when the ring cannot
    /// hold them without overwriting un-reclaimable pages (the caller
    /// parks holding **no node locks** — §4.4 pt 5). Lock-free: a CAS loop
    /// on `admitted` that retries only when a concurrent
    /// admission/release/transfer moved it.
    pub fn try_admit(&self, len: u64, class: AdmissionClass) -> Option<Admission> {
        let reserve = match class {
            AdmissionClass::User => self.geo.reserve_bytes,
            AdmissionClass::Checkpoint => 0,
        };
        let capacity = self.geo.logical_len();
        debug_assert!(reserve < capacity, "reserve must leave admissible space");
        let mut adm = self.admitted.load(Ordering::Acquire);
        loop {
            // Read order matters: `admitted` BEFORE `head` (see module
            // docs) so a racing transfer is double-counted, never missed.
            let head = self.head.load(Ordering::Acquire);
            let reusable = self.reusable_upto.load(Ordering::Acquire);
            let claimed = head + adm;
            if claimed + len + reserve > reusable + capacity {
                return None;
            }
            match self.admitted.compare_exchange(
                adm,
                adm + len,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Admission { len }),
                Err(cur) => adm = cur,
            }
        }
    }

    /// Give admitted budget back (a transaction that failed before its
    /// reservation). Conservation: `admitted` decreases by exactly the
    /// admission's length; `head` is untouched.
    pub fn release(&self, adm: Admission) {
        let prev = self.admitted.fetch_sub(adm.len, Ordering::AcqRel);
        debug_assert!(prev >= adm.len, "released more budget than admitted");
    }

    /// Split an admission into `first` bytes and the remainder — pure
    /// bookkeeping over the same admitted budget (the counter moves at
    /// `reserve` / `release`, never here). What lets a caller admit a
    /// WORST-CASE length before it knows the exact one (PR 4 review round
    /// 3, Issue 24: the door's first-touch acquire admits its control
    /// entry before taking the manager's verb mutex, then reserves the
    /// exact length and releases the rest). `first` must not exceed the
    /// admission.
    pub fn split_admission(&self, adm: Admission, first: u64) -> (Admission, Admission) {
        debug_assert!(first <= adm.len, "split past the admission");
        let first = first.min(adm.len);
        (
            Admission { len: first },
            Admission {
                len: adm.len - first,
            },
        )
    }

    /// Transfer admitted budget to the head: the single `fetch_add` of
    /// §4.4 pt 2. Never blocks and never fails — the budget was admitted
    /// up front, so the head advance is claim-by-construction. Returns the
    /// claimed range (its start doubles as the entry seq).
    ///
    /// Ordering: `head` is bumped BEFORE `admitted` is decremented, pairing
    /// with [`Self::try_admit`]'s admitted-then-head read order (module
    /// docs) so racing admissions never under-count the claim.
    pub fn reserve(&self, adm: Admission) -> Reservation {
        let start = self.head.fetch_add(adm.len, Ordering::AcqRel);
        let prev = self.admitted.fetch_sub(adm.len, Ordering::AcqRel);
        debug_assert!(prev >= adm.len, "reserved more budget than admitted");
        Reservation {
            start,
            len: adm.len,
        }
    }

    /// Advance the reclamation watermark (monotonic `fetch_max`; a stale
    /// or repeated advance is a no-op). Callers pass only durable-covered
    /// tails (§4.6 pt 3); debug builds assert the watermark never passes
    /// the head.
    pub fn advance_reusable_upto(&self, pos: u64) {
        debug_assert!(
            pos <= self.head.load(Ordering::Acquire),
            "watermark {pos} would pass the head"
        );
        self.reusable_upto.fetch_max(pos, Ordering::AcqRel);
    }
}
