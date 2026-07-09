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
//! `try_admit` reads `admitted` **before** `head` while `reserve` (the
//! transfer) bumps `head` **before** decrementing `admitted`: any racing
//! observation therefore sees `admitted + head` at-or-above the true claim
//! (the transferred bytes are transiently counted twice, never zero times),
//! so admission can be spuriously refused but never over-commits the ring.
//! The CAS on `admitted` re-validates after any concurrent change.
//!
//! ## `reusable_upto` (§4.6 pt 3)
//!
//! The ring-reclamation watermark: positions strictly below it may be
//! overwritten. It advances only after the root-ledger record that retired
//! them is *known durable* (post-barrier) — never merely because the in-RAM
//! tail moved — so the head can never overwrite the pages a
//! torn-newest-ledger-slot fallback would need to replay. The watermark is
//! monotonic and never exceeds the durable tail its caller derived it from
//! (`advance_reusable_upto` debug-asserts it never passes `head`).

#[cfg(loom)]
pub(crate) mod atomic {
    pub use loom::sync::atomic::AtomicU64;
}
#[cfg(not(loom))]
pub(crate) mod atomic {
    pub use std::sync::atomic::AtomicU64;
}

use atomic::AtomicU64;

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
        todo!()
    }

    /// Which lap `pos` belongs to.
    pub fn lap(&self, _pos: u64) -> u64 {
        todo!()
    }

    /// `pos`'s offset within its lap (`pos % logical_len`).
    pub fn ring_offset(&self, _pos: u64) -> u64 {
        todo!()
    }

    /// Physical page index holding `pos` (`(pos / page_data_len) % pages`).
    pub fn page_index(&self, _pos: u64) -> u64 {
        todo!()
    }

    /// The logical position of the first byte of `pos`'s page
    /// (`pos` aligned down to `page_data_len`).
    pub fn page_start_pos(&self, _pos: u64) -> u64 {
        todo!()
    }

    /// `pos`'s offset within its page's data area (`pos % page_data_len`).
    pub fn in_page_off(&self, _pos: u64) -> u64 {
        todo!()
    }

    /// Map the logical range `[start, start + len)` onto physical page-data
    /// segments, in logical order. Segments are contiguous: the first may
    /// begin mid-page, the last may end mid-page, and every interior
    /// segment covers its page's whole data area — the §4.4 pt 2 "no
    /// special casing" shape of a multi-page reservation.
    pub fn segments(&self, _start: u64, _len: u64) -> Vec<PageSegment> {
        todo!()
    }

    /// Logical positions of every page-first-byte inside
    /// `[start, start + len)` — the pages whose 24 B header the owner of
    /// this reservation writes (§4.4 pt 2: "exactly one committer's
    /// reservation contains each page's first logical byte").
    pub fn owned_page_starts(&self, _start: u64, _len: u64) -> Vec<u64> {
        todo!()
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
/// dropping it on the floor leaks budget forever (debug builds catch this
/// in the owning module's tests via the accounting assertions).
#[derive(Debug)]
#[must_use = "admitted budget must be reserved or released, or the ring leaks"]
pub struct Admission {
    _len: u64,
}

impl Admission {
    /// The admitted byte count.
    pub fn byte_len(&self) -> u64 {
        todo!()
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
        todo!()
    }

    /// One past the last logical byte (`start + len`).
    pub fn end(&self) -> u64 {
        todo!()
    }
}

/// The lock-free reservation/admission core. See the module docs for the
/// protocol; the loom models in `loom-models/` pin its invariants:
/// no reservation overlap, seq monotonic, lap/wrap correctness, multi-page
/// contiguity, no ring over-commit, admitted-byte conservation across
/// transfer/release, and reserve-not-consumable-by-users.
pub struct JournalCore {
    _geo: CoreGeometry,
    _head: AtomicU64,
    _admitted: AtomicU64,
    _reusable_upto: AtomicU64,
}

impl JournalCore {
    /// Build a core over `geo`, resuming at `head_pos` (0 for a fresh ring;
    /// the replay-recovered head on remount) with the reclamation watermark
    /// at `reusable_upto` (0 fresh; the mounted ledger record's durable
    /// tail on remount — §4.6 pt 3).
    pub fn new(_geo: CoreGeometry, _head_pos: u64, _reusable_upto: u64) -> Self {
        todo!()
    }

    /// The ring geometry.
    pub fn geometry(&self) -> &CoreGeometry {
        todo!()
    }

    /// Current head position (the next reservation's start).
    pub fn head(&self) -> u64 {
        todo!()
    }

    /// Bytes admitted but not yet reserved (diagnostics / accounting tests).
    pub fn admitted(&self) -> u64 {
        todo!()
    }

    /// Current ring-reclamation watermark (§4.6 pt 3).
    pub fn reusable_upto(&self) -> u64 {
        todo!()
    }

    /// Admit `len` bytes of ring budget, or `None` when the ring cannot
    /// hold them without overwriting un-reclaimable pages (the caller
    /// parks holding **no node locks** — §4.4 pt 5). Wait-free apart from
    /// CAS retries against concurrent admissions/transfers.
    pub fn try_admit(&self, _len: u64, _class: AdmissionClass) -> Option<Admission> {
        todo!()
    }

    /// Give admitted budget back (a transaction that failed before its
    /// reservation). Conservation: `admitted` decreases by exactly the
    /// admission's length; `head` is untouched.
    pub fn release(&self, _adm: Admission) {
        todo!()
    }

    /// Transfer admitted budget to the head: the single `fetch_add` of
    /// §4.4 pt 2. Never blocks and never fails — the budget was admitted
    /// up front, so the head advance is claim-by-construction. Returns the
    /// claimed range (its start doubles as the entry seq).
    pub fn reserve(&self, _adm: Admission) -> Reservation {
        todo!()
    }

    /// Advance the reclamation watermark (monotonic `fetch_max`; a stale
    /// or repeated advance is a no-op). Callers pass only durable-covered
    /// tails (§4.6 pt 3); debug builds assert the watermark never passes
    /// the head.
    pub fn advance_reusable_upto(&self, _pos: u64) {
        todo!()
    }
}
