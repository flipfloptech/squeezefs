//! The journal ring's lock-free reservation and admission core (charter §5.1
//! "journal ring", §5.2 "ONE ring admission … ONE contiguous reservation",
//! §9 "model-check every lock-free core"; `docs/porting/kv_journal_ledger.md`
//! §1.3, §1.7, §2).
//!
//! Standalone and dependency-free (std only) so `journal_core_model.zig` can
//! restate it step for step and explore every interleaving exhaustively.
//! Nothing here touches a device: `journal.zig` adds the page/entry framing,
//! the io task binds it to the ring.
//!
//! ## The logical byte space
//!
//! Ring positions are monotonic u64 logical byte offsets that NEVER wrap:
//! the ring's 4 KiB pages contribute only their DATA bytes (the 24 B
//! page-header slot is excluded by a fixed logical→physical mapping), so an
//! entry can never land on a header and a multi-page entry needs no special
//! casing. Lap and in-ring offset are derived (`lap = pos / L`,
//! `offset = pos % L`): a monotonic position IS the packed `(lap, logical
//! offset)` word of charter §5.2 in its canonical form — one u64, no packing
//! arithmetic.
//!
//! ## seq ≡ reservation start position
//!
//! One `fetch_add` hands out one value, so the entry seq is the reservation's
//! logical start: unique, strictly monotonic in reservation order, and —
//! because reservations happen inside the node locks — per-key seq order is
//! RAM apply order. Replay exploits the identity: a valid entry at position
//! `p` carries `seq == p`, which rejects stale previous-lap bytes on a chain.
//!
//! ## Budget accounting
//!
//! The shipped protocol kept TWO words: `head` (bytes reserved) and
//! `admitted` (bytes admitted, not yet transferred to the head), with a CAS
//! on `admitted` at admission and `head += len; admitted -= claim` at
//! transfer. Its safety argument ("admit reads `admitted` then `head`; the
//! transfer bumps `head` then decrements `admitted`, so an interleaving
//! double-counts, never under-counts") holds for ONE transfer against ONE
//! admission — but a CAS on a byte COUNT is ABA-blind. With three parties
//! the exhaustive model (`journal_core_model.AbaAdmitSlipsPastCounterCas`)
//! finds the over-commit in a ring of 8:
//!
//!     X holds 3 admitted.  Z: admitted→3, head→0 (claimed 3; 3+5 ≤ 8 ok)
//!     X: head += 3; admitted -= 3 (→0).  Y: admit 3 (0+3+3 ≤ 8; CAS 0→3)
//!     Z: CAS admitted 3→8 SUCCEEDS (3 == 3)   ⇒ head 3 + admitted 8 = 11 > 8
//!
//! Y's bytes were counted zero times: Z read `admitted` before X's decrement
//! and `head` before X's bump, and the counter's round trip 3→0→3 hid both.
//! Positions `[8, 11)` would overwrite X's unreclaimed `[0, 3)`.
//!
//! This core therefore keeps MONOTONIC totals: `admitted_total` (every claim
//! ever admitted — the CAS word, so it can never ABA), `released_total`,
//! `transferred_total`. Outstanding = `admitted_total − released_total −
//! transferred_total`; the ring's commitment = `head + outstanding`. The
//! read/write orders that keep an admission's view conservative:
//!
//! - admission reads `transferred_total` BEFORE `head` (a transfer's head
//!   growth is never seen without the claim it retired from `outstanding`);
//! - the transfer bumps `head` BEFORE adding to `transferred_total` (the
//!   claim is still outstanding while its bytes are already in the head —
//!   transiently double-counted, never missed).
//!
//! A stale `released_total` or `reusable_upto` only under-reports space.
//! Admission can be spuriously refused under a race (the caller re-tries from
//! its park loop), never over-commit — proven over every interleaving in
//! `journal_core_model.zig`, with BOTH order reversals proven to fail.
//!
//! ## `reusable_upto`
//!
//! The reclamation watermark: positions strictly below it may be
//! overwritten. It advances only after the root-ledger record that retired
//! them is known durable (post-barrier, charter §5.3) — never because the
//! in-RAM tail moved — so the head can never overwrite the pages a torn-
//! newest-ledger-slot fallback would need to replay. Monotonic (`fetch_max`).

const std = @import("std");
const Allocator = std.mem.Allocator;

/// The physical page-header length the pad law accounts for: a logical page
/// start maps to the byte AFTER it. `{magic u32, lap u32, first_entry_off
/// u16, reserved u16, appender_id u32, xxh3 u64}` (`journal.zig` asserts
/// equality).
pub const PAGE_HDR_LEN: u64 = 24;

/// The smallest pad the ring can WRITE: a PAD entry is its 24 B entry header
/// alone (entry kind PAD, a zero-length payload — format v4). A gap shorter
/// than this is padded to the boundary after the next.
pub const PAD_MIN: u64 = 24;

/// Ring geometry: everything the pure core needs to derive laps, offsets, page
/// indices and physical segments from monotonic logical positions.
/// Parametric on purpose (the models use tiny pages so wrap interleavings are
/// reachable); the production ring passes `journal.PAGE_DATA_LEN` and
/// `journal.checkpointReserveBytes`.
pub const CoreGeometry = struct {
    /// Entry-byte capacity of one page (production: 4096 − 24 = 4072).
    page_data_len: u64,
    /// Pages in the ring.
    pages: u64,
    /// Checkpoint-task carve-out in logical bytes (`max(256 KiB, ring/64)` in
    /// production): admissible only by `.checkpoint`, invisible to users.
    reserve_bytes: u64,
    /// The device's sector grain the ring's writes align to: every
    /// reservation's end is padded so the next begins on a physical sector
    /// boundary. `1` = no padding (the buffered posture; byte-identical to the
    /// pre-pad-law ring). A power of two ≤ 4096, so page boundaries are
    /// always aligned.
    grain: u64,

    /// Total logical bytes per lap (`pages × page_data_len`).
    pub fn logicalLen(self: CoreGeometry) u64 {
        return self.pages * self.page_data_len;
    }

    /// Whether `pos` sits on a physical sector boundary: a page's first byte
    /// (its header starts the page — 4 KiB aligned by construction) or an
    /// in-page offset whose physical form `24 + off` is a grain multiple.
    /// Always true on an unpadded ring.
    pub fn sectorAligned(self: CoreGeometry, pos: u64) bool {
        if (self.grain <= 1) return true;
        const off = self.inPageOff(pos);
        return off == 0 or (PAGE_HDR_LEN + off) % self.grain == 0;
    }

    /// The pad a reservation ending at `end` carries so the next begins
    /// sector-aligned: 0 when `end` already is; else the distance to the
    /// first aligned position at or after `end + PAD_MIN` (a pad must hold a
    /// PAD entry). Always in `[PAD_MIN, maxPad()]` when non-zero.
    pub fn padOf(self: CoreGeometry, end: u64) u64 {
        if (self.sectorAligned(end)) return 0;
        const target = end + PAD_MIN;
        const off = self.inPageOff(target);
        const next = if (off == 0) target else blk: {
            const phys = PAGE_HDR_LEN + off;
            const up = (phys + self.grain - 1) / self.grain * self.grain;
            // Past the page's data: the page end (the next page's header is
            // aligned by construction).
            if (up >= PAGE_HDR_LEN + self.page_data_len) break :blk target + (self.page_data_len - off);
            break :blk target + (up - phys);
        };
        return next - end;
    }

    /// The largest pad any reservation carries (`PAD_MIN` short of a boundary,
    /// then a whole grain to the next): the slack every admission claims.
    /// 4120 at grain 4096, 536 at 512; 0 on an unpadded ring.
    pub fn maxPad(self: CoreGeometry) u64 {
        return if (self.grain <= 1) 0 else PAD_MIN + self.grain;
    }

    /// The class space a checkpoint-class admission KEEPS behind it
    /// (`JournalCore.tryAdmitKeeping`): a quarter of the class (64 KiB at the
    /// 256 KiB floor, 128 KiB on the 32 MiB ring) floored at one page, plus
    /// the pad slack — holds the widest SMO entry at every format geometry
    /// (nodes 64 KiB–1 MiB). Every checkpoint-class consumer but the flush
    /// pass's oldest node per ring leaves this much, so the one node that can
    /// move the tail always admits at the next cycle's start.
    pub fn smoKeep(self: CoreGeometry) u64 {
        return @max(self.reserve_bytes / 4, self.page_data_len) + self.maxPad();
    }

    pub fn lap(self: CoreGeometry, pos: u64) u64 {
        return pos / self.logicalLen();
    }

    pub fn ringOffset(self: CoreGeometry, pos: u64) u64 {
        return pos % self.logicalLen();
    }

    /// Physical page index holding `pos`.
    pub fn pageIndex(self: CoreGeometry, pos: u64) u64 {
        return (pos / self.page_data_len) % self.pages;
    }

    /// The logical position of the first byte of `pos`'s page.
    pub fn pageStartPos(self: CoreGeometry, pos: u64) u64 {
        return pos - (pos % self.page_data_len);
    }

    pub fn inPageOff(self: CoreGeometry, pos: u64) u64 {
        return pos % self.page_data_len;
    }

    /// The physical page-data segments of `[start, start + len)`, in logical
    /// order: contiguous, the first may begin mid-page, every interior one
    /// covers its page's whole data area, the last may end mid-page.
    pub fn segments(self: CoreGeometry, start: u64, len: u64) SegmentIter {
        return .{ .geo = self, .pos = start, .end = start + len };
    }

    /// Logical positions of every page-first-byte inside `[start, start +
    /// len)` — the pages whose 24 B header the owner of this reservation
    /// writes (exactly one reservation contains each page's first byte).
    pub fn ownedPageStarts(self: CoreGeometry, start: u64, len: u64) OwnedStartIter {
        const rem = start % self.page_data_len;
        return .{
            .step = self.page_data_len,
            .pos = if (rem == 0) start else start + (self.page_data_len - rem),
            .end = start + len,
        };
    }
};

/// One physical page-data segment of a logical range.
pub const PageSegment = struct {
    /// Physical page index within the ring.
    page: u64,
    /// Offset within the page's DATA area (the I/O layer adds the 24 B skip).
    data_off: u64,
    len: u64,
};

/// Allocation-free segment walk (`CoreGeometry.segments`).
pub const SegmentIter = struct {
    geo: CoreGeometry,
    pos: u64,
    end: u64,

    pub fn next(self: *SegmentIter) ?PageSegment {
        if (self.pos >= self.end) return null;
        const in_page = self.geo.inPageOff(self.pos);
        const take = @min(self.geo.page_data_len - in_page, self.end - self.pos);
        const seg: PageSegment = .{ .page = self.geo.pageIndex(self.pos), .data_off = in_page, .len = take };
        self.pos += take;
        return seg;
    }
};

/// Allocation-free owned-page-start walk (`CoreGeometry.ownedPageStarts`).
pub const OwnedStartIter = struct {
    step: u64,
    pos: u64,
    end: u64,

    pub fn next(self: *OwnedStartIter) ?u64 {
        if (self.pos >= self.end) return null;
        const p = self.pos;
        self.pos += self.step;
        return p;
    }
};

/// Who is asking for ring space.
pub const AdmissionClass = enum {
    /// A user commit: bounded by `L − reserve_bytes`.
    user,
    /// The per-volume checkpoint/writeback/SMO task's own records: bounded
    /// by the full capacity minus the caller's `keep`, so SMOs never wait on
    /// ring space behind user commits (the liveness argument).
    checkpoint,
};

/// Admitted-but-not-reserved ring budget. Must be transferred to the head
/// (`reserve`) or given back (`release`): an admission dropped on the floor
/// leaks budget forever (charter §9 "cancellation of a committer is designed
/// OUT").
pub const Admission = struct {
    /// The ENTRY bytes admitted (what the caller asked for).
    len: u64,
    /// The sector-pad slack admitted beside them (`maxPad` at admission; 0 on
    /// an unpadded ring and on a split remainder). `claim() = len + slack` is
    /// the conservation law's term.
    slack: u64,

    pub fn claim(self: Admission) u64 {
        return self.len + self.slack;
    }

    pub fn isEmpty(self: Admission) bool {
        return self.len == 0 and self.slack == 0;
    }
};

/// A claimed journal range `[start, start + len + pad)`: `[start, end())` is
/// the ENTRY's exact range; `[end(), paddedEnd())` is the sector PAD behind
/// it, written as one PAD entry so the next reservation begins aligned.
pub const Reservation = struct {
    /// Logical start position (== the entry seq).
    start: u64,
    /// The entry's exact length.
    len: u64,
    /// The sector pad behind the entry (0 = none).
    pad: u64,

    pub fn unpadded(start: u64, len: u64) Reservation {
        return .{ .start = start, .len = len, .pad = 0 };
    }

    pub fn seq(self: Reservation) u64 {
        return self.start;
    }

    /// One past the entry's last byte — where the PAD entry (if any) begins.
    pub fn end(self: Reservation) u64 {
        return self.start + self.len;
    }

    /// One past the RESERVATION's last byte — where the next begins; what
    /// completion, tails and the head read.
    pub fn paddedEnd(self: Reservation) u64 {
        return self.start + self.len + self.pad;
    }

    pub fn paddedLen(self: Reservation) u64 {
        return self.len + self.pad;
    }

    /// The POSITION-domain seq of this entry's `i`-th record: `start + i`,
    /// inside `[start, end())` since a record is at least one byte. A pending
    /// free parks on it and the durable tail (a position) covers it. The
    /// record's STAMP is `seq_base + i` (`SeqSpan`, the overlay's domain).
    pub fn recordGate(self: Reservation, i: usize) u64 {
        return self.start + @as(u64, @intCast(i));
    }
};

/// The RECORD-SEQ span of one entry or one contiguous batch — the ONE domain
/// every overlay address uses. Records are stamped `seq_base + i` from the
/// base `reserveRegistered` answers (`position + seq_offset`), so a
/// `len`-byte entry's stamps lie in `[seq_base, seq_base + len)`. A
/// `Reservation` is the POSITION domain; the two differ by the ring's offset
/// on any ring that ever received a handed-over slot.
pub const SeqSpan = struct {
    lo: u64,
    /// One past the last.
    hi: u64,

    pub fn stamped(seq_base: u64, len: u64) SeqSpan {
        return .{ .lo = seq_base, .hi = seq_base +| len };
    }

    pub fn contains(self: SeqSpan, s: u64) bool {
        return s >= self.lo and s < self.hi;
    }
};

/// The lock-free admission/reservation core (module docs).
pub const JournalCore = struct {
    geo: CoreGeometry,
    /// Next reservation's logical start (monotonic).
    head: std.atomic.Value(u64),
    /// Σ claims ever admitted — the admission CAS word (monotonic: ABA-proof).
    admitted_total: std.atomic.Value(u64),
    /// Σ claims ever released before their reservation.
    released_total: std.atomic.Value(u64),
    /// Σ claims ever transferred to the head.
    transferred_total: std.atomic.Value(u64),
    /// Ring-reclamation watermark: positions strictly below are overwritable.
    reusable_upto: std.atomic.Value(u64),

    /// A core over `geo` resuming at `head_pos` (0 fresh; the replay-recovered
    /// head on remount) with the watermark at `reusable_upto` (0 fresh; the
    /// mounted ledger record's `journal_tail_seq` on remount).
    pub fn init(geo: CoreGeometry, head_pos: u64, reusable_upto: u64) JournalCore {
        std.debug.assert(geo.page_data_len > 0 and geo.pages > 0);
        std.debug.assert(reusable_upto <= head_pos);
        std.debug.assert(head_pos <= reusable_upto + geo.logicalLen());
        return .{
            .geo = geo,
            .head = .init(head_pos),
            .admitted_total = .init(0),
            .released_total = .init(0),
            .transferred_total = .init(0),
            .reusable_upto = .init(reusable_upto),
        };
    }

    pub fn geometry(self: *const JournalCore) CoreGeometry {
        return self.geo;
    }

    /// The next reservation's start. Acquire pairs with the Release half of
    /// every head RMW so a reader sees the reservation's bytes claimed.
    pub fn headPos(self: *const JournalCore) u64 {
        return self.head.load(.acquire);
    }

    /// Bytes admitted but not yet reserved or released (diagnostics and the
    /// conservation tests; exact once the ring is quiescent).
    pub fn admitted(self: *const JournalCore) u64 {
        const t = self.transferred_total.load(.acquire);
        const r = self.released_total.load(.acquire);
        const a = self.admitted_total.load(.acquire);
        return a - r - t;
    }

    pub fn reusableUpto(self: *const JournalCore) u64 {
        return self.reusable_upto.load(.acquire);
    }

    /// Admit `len` bytes of ring budget, or null when the ring cannot hold
    /// them without overwriting un-reclaimable pages (the caller parks holding
    /// NO node locks). `tryAdmitKeeping` with `keep = 0`.
    pub fn tryAdmit(self: *JournalCore, len: u64, class: AdmissionClass) ?Admission {
        return self.tryAdmitKeeping(len, class, 0);
    }

    /// `tryAdmit` that refuses unless `keep` bytes of the class's space stay
    /// free BEHIND the admission — the checkpoint class's liveness law: every
    /// SMO record but the flush pass's oldest-per-ring node keeps `smoKeep`,
    /// so the class can never be consumed below what the one node that can
    /// move the tail needs (without it the pass ate the class to one free page
    /// and the tail's own 123 B record — claiming one page + 45 B — could
    /// never admit: a full ring whose every byte above the tail is live).
    pub fn tryAdmitKeeping(self: *JournalCore, len: u64, class: AdmissionClass, keep: u64) ?Admission {
        const held_back: u64 = switch (class) {
            .user => self.geo.reserve_bytes,
            .checkpoint => keep,
        };
        const capacity = self.geo.logicalLen();
        std.debug.assert(held_back < capacity);
        // The sector pad's slack rides every admission on a padded ring: the
        // reservation takes the exact pad and gives the rest back, so a pad
        // can never over-commit the budget.
        const slack = self.geo.maxPad();
        const claim = len + slack;
        var a = self.admitted_total.load(.acquire);
        while (true) {
            // READ ORDER (module docs): `transferred_total` BEFORE `head`, so
            // a racing transfer — head bumped, claim not yet retired — is
            // counted twice, never zero times. Acquire on each pairs with the
            // AcqRel RMWs in `reserve`/`release`/`advanceReusableUpto`.
            const t = self.transferred_total.load(.acquire);
            const r = self.released_total.load(.acquire);
            const h = self.head.load(.acquire);
            const reusable = self.reusable_upto.load(.acquire);
            if (a < r + t) {
                // A claim admitted after our `a` read already left: the CAS
                // below would fail anyway — refresh and re-derive.
                a = self.admitted_total.load(.acquire);
                continue;
            }
            const outstanding = a - r - t;
            if (h + outstanding + claim + held_back > reusable + capacity) return null;
            // AcqRel success: publishes the claim to every later admission's
            // Acquire load; Acquire failure: `cur` is a fresh admitted_total.
            if (self.admitted_total.cmpxchgWeak(a, a + claim, .acq_rel, .acquire)) |cur| {
                a = cur;
                continue;
            }
            return .{ .len = len, .slack = slack };
        }
    }

    /// Give admitted budget back (a transaction that failed before its
    /// reservation). `head` is untouched; `admitted()` drops by exactly the
    /// claim. AcqRel pairs with the admission loop's Acquire loads.
    pub fn release(self: *JournalCore, adm: Admission) void {
        const prev_released = self.released_total.fetchAdd(adm.claim(), .acq_rel);
        std.debug.assert(prev_released + adm.claim() <= self.admitted_total.load(.acquire));
    }

    /// Split an admission into `first` bytes and the remainder — pure
    /// bookkeeping over the same admitted budget (the counters move at
    /// `reserve`/`release`, never here): a caller admits a WORST-CASE length
    /// before it knows the exact one, reserves the exact piece and releases
    /// the rest. The pad slack stays with the FIRST piece (the one reserved);
    /// the remainder carries none, so reserving a remainder on a padded ring
    /// is a caller bug (`reserve` asserts).
    pub fn splitAdmission(adm: Admission, first: u64) struct { Admission, Admission } {
        std.debug.assert(first <= adm.len);
        const take = @min(first, adm.len);
        return .{
            .{ .len = take, .slack = adm.slack },
            .{ .len = adm.len - take, .slack = 0 },
        };
    }

    /// Transfer admitted budget to the head — THE single `fetch_add` of
    /// charter §5.2 on an unpadded ring; a CAS loop on a padded ring, since
    /// the pad depends on where the head stands. Never blocks, never fails:
    /// the budget was admitted up front. Conservation: `head` grows by exactly
    /// the padded length, `admitted()` shrinks by exactly the claim (the
    /// unused slack is given back in the same step).
    pub fn reserve(self: *JournalCore, adm: Admission) Reservation {
        const claim = adm.claim();
        if (self.geo.grain <= 1) {
            // WRITE ORDER (module docs): `head` BEFORE `transferred_total`,
            // pairing with the admission's transferred-then-head read order.
            // AcqRel on both: Release publishes the claimed range; Acquire
            // orders this transfer after every earlier one.
            const start = self.head.fetchAdd(adm.len, .acq_rel);
            _ = self.transferred_total.fetchAdd(claim, .acq_rel);
            return Reservation.unpadded(start, adm.len);
        }
        var start = self.head.load(.acquire);
        const pad = while (true) {
            // A pad past the slack would claim budget the ring never admitted.
            // Only a split REMAINDER carries no slack and no caller reserves
            // one: a debug build fails HERE (the caller's bug, at its
            // reserve); a release build clamps — never an over-commit; the
            // misaligned window's write is then refused loud by `aligned_ops`.
            const need = self.geo.padOf(start + adm.len);
            std.debug.assert(adm.slack > 0 or need == 0);
            std.debug.assert(need <= adm.slack);
            const pad = @min(need, adm.slack);
            // AcqRel success publishes the padded range; Acquire failure
            // hands back the head another reservation moved.
            if (self.head.cmpxchgWeak(start, start + adm.len + pad, .acq_rel, .acquire)) |cur| {
                start = cur;
                continue;
            }
            break pad;
        };
        // Head bumped above; now retire the claim (the write order).
        _ = self.transferred_total.fetchAdd(claim, .acq_rel);
        return .{ .start = start, .len = adm.len, .pad = pad };
    }

    /// Advance the reclamation watermark (monotonic `fetch_max`; a stale or
    /// repeated advance is a no-op). Callers pass ONLY durable-covered tails
    /// (post-barrier). AcqRel pairs with the admission's Acquire load.
    pub fn advanceReusableUpto(self: *JournalCore, pos: u64) void {
        std.debug.assert(pos <= self.head.load(.acquire));
        _ = self.reusable_upto.fetchMax(pos, .acq_rel);
    }
};

// ---------------------------------------------------------------------------
// The in-flight registry around the core.
// ---------------------------------------------------------------------------

/// A short critical-section lock for the registry: two O(1) list operations,
/// never held across I/O or a wait. Test-and-test-and-set with a yield after a
/// spin burst so a descheduled holder is not spun against. Portable (the
/// futex form is Linux-only and this registry runs in the portable tests);
/// the metadata-transaction class is the one place a mutex is sanctioned
/// (charter §12).
pub const SpinLock = struct {
    locked: std.atomic.Value(bool) = .init(false),

    /// Spins before yielding: ~a few hundred ns of a contended O(1) section.
    const spin_burst = 64;

    pub fn lock(self: *SpinLock) void {
        var spins: u32 = 0;
        while (true) {
            // Acquire pairs with the Release in `unlock`: the holder's writes
            // to the guarded state are visible to the next holder.
            if (!self.locked.swap(true, .acquire)) return;
            while (self.locked.load(.monotonic)) {
                spins += 1;
                if (spins % spin_burst == 0) std.Thread.yield() catch {} else std.atomic.spinLoopHint();
            }
        }
    }

    pub fn unlock(self: *SpinLock) void {
        self.locked.store(false, .release);
    }
};

/// A notify word: an epoch bumped on every notify. A waiter REGISTERS by
/// reading the epoch (`register`), re-checks its condition, then awaits a
/// change — the register-recheck-await shape (the wedge law: with a lazily
/// registering notify a completion landing between the re-check and the
/// first poll was LOST and the conveyor pass parked forever). The executor's
/// park (charter §9) will wait on this word; until then `firedSince` is the
/// poll.
pub const Notify = struct {
    epoch: std.atomic.Value(u32) = .init(0),

    pub const Token = struct { epoch: u32 };

    /// Registers NOW: every notify after this call is observable through
    /// `firedSince(token)`. Acquire pairs with `notifyWaiters`' Release.
    pub fn register(self: *const Notify) Token {
        return .{ .epoch = self.epoch.load(.acquire) };
    }

    /// Release: the state change that prompted the notify is visible to any
    /// waiter whose `firedSince` observes the new epoch.
    pub fn notifyWaiters(self: *Notify) void {
        _ = self.epoch.fetchAdd(1, .release);
    }

    pub fn firedSince(self: *const Notify, token: Token) bool {
        return self.epoch.load(.acquire) != token.epoch;
    }
};

/// An open reservation: `[start, padded_end)`.
const Open = struct { start: u64, padded_end: u64 };

/// The in-flight reservation registry (`docs/porting/kv_journal_ledger.md`
/// §2.7): `minInflightStart` feeds the checkpoint tail rule (a tx between its
/// in-lock reservation and its RAM apply/entry write must hold the tail back —
/// the registry is the only witness of that window); `completedUpto` is the
/// contiguous completed-prefix watermark behind the barrier observation (an
/// acked entry starting mid-page is chain-reachable only through its
/// predecessors, so a tx returns — and a barrier acks — only once every
/// reservation below it has completed its write or explicitly abandoned it: a
/// rollback's reserved-but-unwritten hole still COMPLETES for watermark
/// purposes, it just never writes bytes).
///
/// Around the core, not inside it: the commit hot path's reads stay
/// latch-free; the lock is held for two list operations, entered at most
/// twice per transaction (reserve + complete).
pub const InflightRegistry = struct {
    gpa: Allocator,
    lock: SpinLock = .{},
    /// Open reservations, ascending by start. Reservations are issued under
    /// the lock in head order, so insertion is always an append; completion
    /// is out of order (a batch member may land before its predecessor).
    open: std.ArrayList(Open),
    /// Every position below this has a completed (or abandoned) reservation
    /// covering it: `min(open)`, or the head when none is open.
    completed_upto: u64,
    /// The ring's record-seq offset: a record's seq is its reservation's
    /// POSITION plus this. 0 for the life of a ring nothing was handed to
    /// (the shipped ring: seq ≡ position); raised by a slot grant so the
    /// ring's next stamp exceeds the departing ring's frontier. Read and
    /// raised under the lock so seq order equals reservation order.
    seq_offset: std.atomic.Value(u64) = .init(0),
    /// Wakes `pollCompletedUpto` waiters when the watermark advances.
    completion_notify: Notify = .{},

    pub const Registered = struct { res: Reservation, seq_base: u64 };

    /// `head_pos` is the core's head at open (the recovered head); `capacity`
    /// pre-sizes the open list so the steady state allocates nothing per op.
    pub fn init(gpa: Allocator, head_pos: u64, capacity: usize) Allocator.Error!InflightRegistry {
        return .{
            .gpa = gpa,
            .open = try std.ArrayList(Open).initCapacity(gpa, capacity),
            .completed_upto = head_pos,
        };
    }

    pub fn deinit(self: *InflightRegistry) void {
        self.open.deinit(self.gpa);
    }

    /// Transfer admitted budget to the head AND register the reservation in
    /// flight — ONE lock section, so the completed-prefix watermark can never
    /// observe a head past an unregistered reservation. Called inside the
    /// node-lock window. The entry's first record seq is stamped here, under
    /// the same lock a `raiseSeqFloor` takes: position + offset.
    pub fn reserveRegistered(self: *InflightRegistry, core: *JournalCore, adm: Admission) Allocator.Error!Registered {
        self.lock.lock();
        defer self.lock.unlock();
        const res = core.reserve(adm);
        std.debug.assert(self.open.items.len == 0 or self.open.items[self.open.items.len - 1].start < res.start);
        // Allocates only past the pre-sized capacity (a growing conveyor),
        // amortised; the steady state is `appendAssumeCapacity`.
        try self.open.append(self.gpa, .{ .start = res.start, .padded_end = res.paddedEnd() });
        // Monotonic suffices: the registry lock orders this read against
        // `raiseSeqFloor`'s RMW, which runs under the same lock.
        const seq_base = res.start +| self.seq_offset.load(.monotonic);
        return .{ .res = res, .seq_base = seq_base };
    }

    /// Mark a reservation's write complete — or abandoned (a failed writer's
    /// range stays unwritten but still completes for watermark purposes:
    /// replay's checksum walk drops it). Then wake the prefix waiters.
    pub fn complete(self: *InflightRegistry, core: *const JournalCore, res: Reservation) void {
        self.lock.lock();
        // ONE complete per reservation, on either outcome, is the commit
        // discipline; a debug build fails a double-complete at the caller's
        // bug, a release build recomputes the watermark and carries on
        // (never a panic on a handler path).
        const idx = self.indexOf(res.start);
        std.debug.assert(idx != null);
        if (idx) |i| _ = self.open.orderedRemove(i);
        self.completed_upto = if (self.open.items.len == 0) core.headPos() else self.open.items[0].start;
        self.lock.unlock();
        self.completion_notify.notifyWaiters();
    }

    fn indexOf(self: *const InflightRegistry, start: u64) ?usize {
        for (self.open.items, 0..) |o, i| if (o.start == start) return i;
        return null;
    }

    /// The contiguous completed-prefix watermark.
    pub fn completedUpto(self: *InflightRegistry) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return self.completed_upto;
    }

    /// The oldest in-flight reservation's start, or `maxInt(u64)` when none —
    /// the checkpoint tail rule's input.
    pub fn minInflightStart(self: *InflightRegistry) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return if (self.open.items.len == 0) std.math.maxInt(u64) else self.open.items[0].start;
    }

    pub fn openCount(self: *InflightRegistry) usize {
        self.lock.lock();
        defer self.lock.unlock();
        return self.open.items.len;
    }

    pub const Poll = union(enum) {
        ready,
        /// Await `completion_notify.firedSince(token)`, then poll again.
        pending: Notify.Token,
    };

    /// The register-recheck-await shape of `wait_completed_upto` as a poll:
    /// ready when every reservation below `pos` has completed; otherwise the
    /// token registered BETWEEN the first check and the re-check, so a
    /// completion landing after the re-check fires it and one landing before
    /// is caught by the re-check — no window is lost.
    pub fn pollCompletedUpto(self: *InflightRegistry, pos: u64) Poll {
        if (self.completedUpto() >= pos) return .ready;
        const token = self.completion_notify.register();
        if (self.completedUpto() >= pos) return .ready;
        return .{ .pending = token };
    }

    /// The blocking form for threads without an executor (tests, bring-up):
    /// poll, then yield until the registered token fires. No sleeps.
    pub fn waitCompletedUpto(self: *InflightRegistry, pos: u64) void {
        while (true) {
            switch (self.pollCompletedUpto(pos)) {
                .ready => return,
                .pending => |token| while (!self.completion_notify.firedSince(token)) std.Thread.yield() catch std.atomic.spinLoopHint(),
            }
        }
    }

    /// The offset in force (the appender page's `seq_offset`).
    pub fn seqOffset(self: *const InflightRegistry) u64 {
        return self.seq_offset.load(.acquire);
    }

    /// The seq the ring's NEXT reservation would stamp — the `seq_floor` a
    /// slot release records: every record this ring stamped so far is
    /// strictly below it.
    pub fn seqFrontier(self: *InflightRegistry, core: *const JournalCore) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return core.headPos() +| self.seq_offset.load(.monotonic);
    }

    /// Raise the offset so the ring's next stamp EXCEEDS `floor`. Monotone;
    /// a no-op when the frontier already exceeds the floor. Answers the
    /// offset in force afterwards.
    pub fn raiseSeqFloor(self: *InflightRegistry, core: *const JournalCore, floor: u64) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const need = (floor +| 1) -| core.headPos();
        // AcqRel pairs with the Acquire in `seqOffset`.
        const prev = self.seq_offset.fetchMax(need, .acq_rel);
        return @max(prev, need);
    }

    /// Install the offset recovered at open (the page's value, or the window's
    /// own stamps when they say more) — `max`, never lower.
    pub fn recoverSeqOffset(self: *InflightRegistry, offset: u64) void {
        // AcqRel pairs with the Acquire in `seqOffset`.
        _ = self.seq_offset.fetchMax(offset, .acq_rel);
    }
};

// ---------------------------------------------------------------------------
// Tests (`docs/porting/kv_journal_ledger.md` §7, the pure rows).
// ---------------------------------------------------------------------------

const testing = std.testing;

/// Production page geometry for the tests below.
const PAGE_DATA: u64 = 4072;

fn collectSegments(geo: CoreGeometry, start: u64, len: u64, out: []PageSegment) []PageSegment {
    var it = geo.segments(start, len);
    var n: usize = 0;
    while (it.next()) |s| : (n += 1) out[n] = s;
    return out[0..n];
}

fn collectOwned(geo: CoreGeometry, start: u64, len: u64, out: []u64) []u64 {
    var it = geo.ownedPageStarts(start, len);
    var n: usize = 0;
    while (it.next()) |p| : (n += 1) out[n] = p;
    return out[0..n];
}

test "pad law: PAD_MIN is the 24 B entry header alone (a PAD entry carries no payload); max_pad and smo_keep at the production grains" {
    try testing.expectEqual(@as(u64, 24), PAD_MIN);
    try testing.expectEqual(PAGE_HDR_LEN, PAD_MIN);
    const g4096: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 2048, .reserve_bytes = 256 * 1024, .grain = 4096 };
    const g512: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 2048, .reserve_bytes = 256 * 1024, .grain = 512 };
    const g1: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 2048, .reserve_bytes = 256 * 1024, .grain = 1 };
    try testing.expectEqual(@as(u64, 4120), g4096.maxPad());
    try testing.expectEqual(@as(u64, 536), g512.maxPad());
    try testing.expectEqual(@as(u64, 0), g1.maxPad());
    // smo_keep = max(reserve/4, page) + max_pad: 64 KiB + 4120 at the floor.
    try testing.expectEqual(@as(u64, 65536 + 4120), g4096.smoKeep());
    try testing.expectEqual(@as(u64, 65536), g1.smoKeep());
}

test "pad law at grain 4096: the only aligned in-page position is a page start, so every end pads to its page end" {
    const geo: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 8, .reserve_bytes = 0, .grain = 4096 };
    try testing.expect(geo.sectorAligned(0));
    try testing.expect(geo.sectorAligned(PAGE_DATA));
    try testing.expect(!geo.sectorAligned(1));
    try testing.expect(!geo.sectorAligned(PAGE_DATA - 1));
    try testing.expectEqual(@as(u64, 0), geo.padOf(PAGE_DATA));
    // 123 B entry at a page start: pad to the page end (4072 − 123 = 3949).
    try testing.expectEqual(PAGE_DATA - 123, geo.padOf(123));
    // Fewer than PAD_MIN bytes short of the page end: the pad runs into the
    // NEXT page's end — PAD_MIN short of a boundary, then the whole page.
    try testing.expectEqual(PAGE_DATA + 10, geo.padOf(PAGE_DATA - 10));
    try testing.expectEqual(PAGE_DATA + 23, geo.padOf(PAGE_DATA - 23));
    try testing.expectEqual(@as(u64, 24), geo.padOf(PAGE_DATA - 24));
    // Every pad is 0 or in [PAD_MIN, max_pad] and lands aligned.
    var end: u64 = 0;
    while (end < 3 * PAGE_DATA) : (end += 7) {
        const pad = geo.padOf(end);
        try testing.expect(pad == 0 or (pad >= PAD_MIN and pad <= geo.maxPad()));
        try testing.expect(geo.sectorAligned(end + pad));
    }
}

test "pad law at grain 512: aligned at off ∈ {488, 1000, …, 3560} and the page end" {
    const geo: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 8, .reserve_bytes = 0, .grain = 512 };
    try testing.expect(geo.sectorAligned(488));
    try testing.expect(geo.sectorAligned(1000));
    try testing.expect(geo.sectorAligned(3560));
    try testing.expect(!geo.sectorAligned(512));
    try testing.expectEqual(@as(u64, 488 - 100), geo.padOf(100));
    // 480: 480 + 21 = 501 → up to 1000.
    try testing.expectEqual(@as(u64, 1000 - 480), geo.padOf(480));
    // 3560 + 300 = 3860: +21 = 3881 → phys 3905 → up 4096 ≥ 4096 ⇒ page end.
    try testing.expectEqual(PAGE_DATA - 3860, geo.padOf(3860));
    var end: u64 = 0;
    while (end < 2 * PAGE_DATA) : (end += 3) {
        const pad = geo.padOf(end);
        try testing.expect(pad <= geo.maxPad());
        try testing.expect(geo.sectorAligned(end + pad));
    }
}

test "geometry: laps, offsets, segments and owned page starts at production page size" {
    const geo: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 8, .reserve_bytes = 0, .grain = 1 };
    try testing.expectEqual(@as(u64, 8 * 4072), geo.logicalLen());
    var segbuf: [8]PageSegment = undefined;
    var ownbuf: [8]u64 = undefined;
    // Interior single-page range: one segment, no owned page starts.
    const one = collectSegments(geo, 100, 50, &segbuf);
    try testing.expectEqual(@as(usize, 1), one.len);
    try testing.expectEqual(PageSegment{ .page = 0, .data_off = 100, .len = 50 }, one[0]);
    try testing.expectEqual(@as(usize, 0), collectOwned(geo, 100, 50, &ownbuf).len);
    // [4000, +10000): partial head, two full pages, partial tail.
    const four = collectSegments(geo, 4000, 10_000, &segbuf);
    try testing.expectEqual(@as(usize, 4), four.len);
    try testing.expectEqual(PageSegment{ .page = 0, .data_off = 4000, .len = 72 }, four[0]);
    try testing.expectEqual(PageSegment{ .page = 1, .data_off = 0, .len = 4072 }, four[1]);
    try testing.expectEqual(PageSegment{ .page = 2, .data_off = 0, .len = 4072 }, four[2]);
    try testing.expectEqual(PageSegment{ .page = 3, .data_off = 0, .len = 10_000 - 72 - 2 * 4072 }, four[3]);
    const owned = collectOwned(geo, 4000, 10_000, &ownbuf);
    try testing.expectEqualSlices(u64, &.{ 4072, 8144, 12216 }, owned);
    // Wrap: a range crossing the lap boundary re-enters page 0 at lap 1.
    const l = geo.logicalLen();
    const wrap = collectSegments(geo, l - 100, 300, &segbuf);
    try testing.expectEqual(@as(usize, 2), wrap.len);
    try testing.expectEqual(PageSegment{ .page = 7, .data_off = 4072 - 100, .len = 100 }, wrap[0]);
    try testing.expectEqual(PageSegment{ .page = 0, .data_off = 0, .len = 200 }, wrap[1]);
    try testing.expectEqual(@as(u64, 1), geo.lap(l));
    try testing.expectEqual(@as(u64, 0), geo.lap(l - 1));
    try testing.expectEqual(@as(u64, 5), geo.ringOffset(l + 5));
    try testing.expectEqual(@as(u64, 0), geo.pageIndex(l + 5));
    try testing.expectEqual(l, geo.pageStartPos(l + 5));
}

test "geometry: the loom model's tiny ring (4-byte pages): [3,6) and the lap-1 wrap of [6,13)" {
    const geo: CoreGeometry = .{ .page_data_len = 4, .pages = 3, .reserve_bytes = 0, .grain = 1 };
    var segbuf: [8]PageSegment = undefined;
    var ownbuf: [8]u64 = undefined;
    try testing.expectEqual(@as(u64, 0), geo.lap(3));
    try testing.expectEqual(@as(u64, 3), geo.ringOffset(3));
    try testing.expectEqual(@as(u64, 0), geo.pageIndex(3));
    try testing.expectEqual(@as(u64, 3), geo.inPageOff(3));
    const s36 = collectSegments(geo, 3, 3, &segbuf);
    try testing.expectEqual(@as(usize, 2), s36.len);
    try testing.expectEqual(PageSegment{ .page = 0, .data_off = 3, .len = 1 }, s36[0]);
    try testing.expectEqual(PageSegment{ .page = 1, .data_off = 0, .len = 2 }, s36[1]);
    try testing.expectEqualSlices(u64, &.{4}, collectOwned(geo, 3, 3, &ownbuf));
    try testing.expectEqual(@as(u64, 1), geo.lap(12));
    const s613 = collectSegments(geo, 6, 7, &segbuf);
    try testing.expectEqual(@as(usize, 3), s613.len);
    try testing.expectEqual(PageSegment{ .page = 1, .data_off = 2, .len = 2 }, s613[0]);
    try testing.expectEqual(PageSegment{ .page = 2, .data_off = 0, .len = 4 }, s613[1]);
    try testing.expectEqual(PageSegment{ .page = 0, .data_off = 0, .len = 1 }, s613[2]);
    try testing.expectEqualSlices(u64, &.{ 8, 12 }, collectOwned(geo, 6, 7, &ownbuf));
}

test "core admission/reservation budget round trip: user stops at L − reserve, checkpoint enters the reserve, nothing past L" {
    const geo: CoreGeometry = .{ .page_data_len = 10, .pages = 4, .reserve_bytes = 8, .grain = 1 }; // capacity 40, user budget 32
    var core = JournalCore.init(geo, 0, 0);
    try testing.expectEqual(@as(u64, 0), core.headPos());
    try testing.expectEqual(@as(u64, 0), core.admitted());
    try testing.expectEqual(@as(u64, 0), core.reusableUpto());
    try testing.expectEqual(@as(u64, 40), core.geometry().logicalLen());

    const a1 = core.tryAdmit(30, .user).?;
    try testing.expectEqual(@as(u64, 30), core.admitted());
    try testing.expectEqual(@as(?Admission, null), core.tryAdmit(3, .user)); // 30 + 3 > 32
    const a2 = core.tryAdmit(3, .checkpoint).?;
    try testing.expectEqual(@as(?Admission, null), core.tryAdmit(8, .checkpoint)); // 33 + 8 > 40

    // Transfer: head += len, admitted −= len, seq == start.
    const r1 = core.reserve(a1);
    try testing.expectEqual(Reservation{ .start = 0, .len = 30, .pad = 0 }, r1);
    try testing.expectEqual(@as(u64, 0), r1.seq());
    try testing.expectEqual(@as(u64, 30), r1.end());
    try testing.expectEqual(@as(u64, 30), core.headPos());
    try testing.expectEqual(@as(u64, 3), core.admitted());
    core.release(a2);
    try testing.expectEqual(@as(u64, 0), core.admitted());

    // Head bounds against the watermark: 30 + 8 ≤ 0 + 40 − 8 fails by 6;
    // advancing by 6 opens exactly that admission.
    try testing.expectEqual(@as(?Admission, null), core.tryAdmit(8, .user));
    core.advanceReusableUpto(6);
    try testing.expectEqual(@as(u64, 6), core.reusableUpto());
    const a3 = core.tryAdmit(8, .user).?;
    core.advanceReusableUpto(2); // stale: a no-op
    try testing.expectEqual(@as(u64, 6), core.reusableUpto());
    const r3 = core.reserve(a3);
    try testing.expectEqual(Reservation{ .start = 30, .len = 8, .pad = 0 }, r3);
    try testing.expect(r3.seq() > r1.seq());
}

test "a checkpoint-class admission keeps one SMO claim of class space (keep 0 ≡ try_admit)" {
    const geo: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 8, .reserve_bytes = 2 * PAGE_DATA, .grain = 4096 };
    const keep = geo.smoKeep();
    // At a two-page class the keep takes its FLOOR arm: a page + the slack.
    try testing.expectEqual(PAGE_DATA + geo.maxPad(), keep);
    var core = JournalCore.init(geo, 0, 0);
    const cap = geo.logicalLen();
    // Fill to exactly ONE free page, one 123 B record per page (every
    // reservation on a 4 KiB-grain ring starts and ends on a page).
    while (cap - core.headPos() > PAGE_DATA) {
        const adm = core.tryAdmit(123, .checkpoint).?;
        const res = core.reserve(adm);
        try testing.expectEqual(PAGE_DATA, res.len + res.pad);
    }
    try testing.expectEqual(PAGE_DATA, cap - core.headPos());
    // The claim (`len + max_pad` = one page + 45 B) does not fit the last page.
    try testing.expectEqual(@as(?Admission, null), core.tryAdmit(123, .checkpoint));
    // Two free pages: keep-honouring refuses, keep-free admits.
    core.advanceReusableUpto(PAGE_DATA);
    try testing.expectEqual(2 * PAGE_DATA, cap - (core.headPos() - core.reusableUpto()));
    try testing.expectEqual(@as(?Admission, null), core.tryAdmitKeeping(123, .checkpoint, keep));
    core.release(core.tryAdmitKeeping(123, .checkpoint, 0).?);
    // Three free pages: still short (claim + keep = 12,429 > 12,216).
    core.advanceReusableUpto(2 * PAGE_DATA);
    try testing.expectEqual(@as(?Admission, null), core.tryAdmitKeeping(123, .checkpoint, keep));
    // Four: lands and leaves the kept claim behind it.
    core.advanceReusableUpto(3 * PAGE_DATA);
    const adm = core.tryAdmitKeeping(123, .checkpoint, keep).?;
    _ = core.reserve(adm);
    try testing.expect(cap - (core.headPos() - core.reusableUpto()) >= keep);
    const a = core.tryAdmit(123, .checkpoint);
    const b = core.tryAdmitKeeping(123, .checkpoint, 0);
    try testing.expectEqual(a != null, b != null);
    if (a) |x| core.release(x);
    if (b) |x| core.release(x);
}

test "padded reserve: every reservation ends sector-aligned behind a pad within its slack; unused slack returns" {
    const geo: CoreGeometry = .{ .page_data_len = 64, .pages = 2, .reserve_bytes = 0, .grain = 8 };
    try testing.expectEqual(PAD_MIN + 8, geo.maxPad());
    var core = JournalCore.init(geo, 0, 0);
    var prev_end: u64 = 0;
    var i: u64 = 0;
    while (i < 3) : (i += 1) {
        const adm = core.tryAdmit(3, .user).?;
        try testing.expectEqual(geo.maxPad(), adm.slack);
        const r = core.reserve(adm);
        try testing.expectEqual(prev_end, r.start);
        try testing.expect(r.pad <= geo.maxPad());
        try testing.expect(geo.sectorAligned(r.paddedEnd()));
        try testing.expectEqual(geo.padOf(r.end()), r.pad);
        try testing.expect(r.pad == 0 or r.pad >= PAD_MIN);
        prev_end = r.paddedEnd();
    }
    try testing.expectEqual(prev_end, core.headPos());
    try testing.expectEqual(@as(u64, 0), core.admitted());
}

test "split admission: the slack stays with the first piece; the remainder is release-only" {
    const adm: Admission = .{ .len = 100, .slack = 29 };
    const first, const rest = JournalCore.splitAdmission(adm, 40);
    try testing.expectEqual(Admission{ .len = 40, .slack = 29 }, first);
    try testing.expectEqual(Admission{ .len = 60, .slack = 0 }, rest);
    try testing.expect(!rest.isEmpty());
    const all, const none = JournalCore.splitAdmission(adm, 100);
    try testing.expectEqual(adm, all);
    try testing.expect(none.isEmpty());
    const geo: CoreGeometry = .{ .page_data_len = 64, .pages = 2, .reserve_bytes = 0, .grain = 8 };
    var core = JournalCore.init(geo, 0, 0);
    const a = core.tryAdmit(40, .user).?;
    const f, const r = JournalCore.splitAdmission(a, 10);
    const res = core.reserve(f);
    core.release(r);
    try testing.expectEqual(@as(u64, 10), res.len);
    try testing.expectEqual(@as(u64, 0), core.admitted());
}

test "seq span and record gate: the position domain and the stamp domain" {
    const r: Reservation = .{ .start = 100, .len = 30, .pad = 5 };
    try testing.expectEqual(@as(u64, 135), r.paddedEnd());
    try testing.expectEqual(@as(u64, 35), r.paddedLen());
    try testing.expectEqual(@as(u64, 103), r.recordGate(3));
    const span = SeqSpan.stamped(1000, 30);
    try testing.expect(span.contains(1000));
    try testing.expect(span.contains(1029));
    try testing.expect(!span.contains(1030));
    try testing.expectEqual(std.math.maxInt(u64), SeqSpan.stamped(std.math.maxInt(u64) - 1, 10).hi);
}

test "registry: reserve_registered registers in head order; complete moves the watermark to min(open) or the head" {
    const geo: CoreGeometry = .{ .page_data_len = 100, .pages = 4, .reserve_bytes = 0, .grain = 1 };
    var core = JournalCore.init(geo, 0, 0);
    var reg = try InflightRegistry.init(testing.allocator, 0, 8);
    defer reg.deinit();
    const a = try reg.reserveRegistered(&core, core.tryAdmit(10, .user).?);
    const b = try reg.reserveRegistered(&core, core.tryAdmit(20, .user).?);
    const c = try reg.reserveRegistered(&core, core.tryAdmit(5, .user).?);
    try testing.expectEqual(@as(u64, 0), a.res.start);
    try testing.expectEqual(@as(u64, 10), b.res.start);
    try testing.expectEqual(@as(u64, 30), c.res.start);
    try testing.expectEqual(a.res.start, a.seq_base); // the shipped ring: seq ≡ position
    try testing.expectEqual(@as(u64, 0), reg.completedUpto());
    try testing.expectEqual(@as(u64, 0), reg.minInflightStart());
    try testing.expectEqual(@as(usize, 3), reg.openCount());
    // Out-of-order completion: b first — the prefix is still held by a.
    reg.complete(&core, b.res);
    try testing.expectEqual(@as(u64, 0), reg.completedUpto());
    try testing.expect(reg.pollCompletedUpto(10) == .pending);
    reg.complete(&core, a.res);
    try testing.expectEqual(@as(u64, 30), reg.completedUpto());
    try testing.expect(reg.pollCompletedUpto(30) == .ready);
    try testing.expect(reg.pollCompletedUpto(31) == .pending);
    reg.complete(&core, c.res);
    try testing.expectEqual(@as(u64, 35), reg.completedUpto()); // none open ⇒ the head
    try testing.expectEqual(std.math.maxInt(u64), reg.minInflightStart());
    try testing.expect(reg.pollCompletedUpto(35) == .ready);
}

test "registry: the register-recheck-await token fires on a completion registered after the re-check" {
    const geo: CoreGeometry = .{ .page_data_len = 100, .pages = 4, .reserve_bytes = 0, .grain = 1 };
    var core = JournalCore.init(geo, 0, 0);
    var reg = try InflightRegistry.init(testing.allocator, 0, 8);
    defer reg.deinit();
    const a = try reg.reserveRegistered(&core, core.tryAdmit(10, .user).?);
    const poll = reg.pollCompletedUpto(10);
    const token = poll.pending;
    try testing.expect(!reg.completion_notify.firedSince(token));
    reg.complete(&core, a.res);
    try testing.expect(reg.completion_notify.firedSince(token));
    try testing.expect(reg.pollCompletedUpto(10) == .ready);
}

test "registry: seq offset raises the frontier monotonically; recover installs max" {
    const geo: CoreGeometry = .{ .page_data_len = 100, .pages = 4, .reserve_bytes = 0, .grain = 1 };
    var core = JournalCore.init(geo, 0, 0);
    var reg = try InflightRegistry.init(testing.allocator, 0, 8);
    defer reg.deinit();
    const a = try reg.reserveRegistered(&core, core.tryAdmit(10, .user).?);
    reg.complete(&core, a.res);
    try testing.expectEqual(@as(u64, 10), reg.seqFrontier(&core));
    // A grant whose departing ring stamped up to 99: the next stamp must be ≥ 100 ⇒ offset 90.
    try testing.expectEqual(@as(u64, 90), reg.raiseSeqFloor(&core, 99));
    try testing.expectEqual(@as(u64, 100), reg.seqFrontier(&core));
    // A lower floor never lowers the offset.
    try testing.expectEqual(@as(u64, 90), reg.raiseSeqFloor(&core, 50));
    const b = try reg.reserveRegistered(&core, core.tryAdmit(5, .user).?);
    try testing.expectEqual(@as(u64, 10), b.res.start);
    try testing.expectEqual(@as(u64, 100), b.seq_base);
    reg.recoverSeqOffset(20);
    try testing.expectEqual(@as(u64, 90), reg.seqOffset());
    reg.recoverSeqOffset(200);
    try testing.expectEqual(@as(u64, 200), reg.seqOffset());
    reg.complete(&core, b.res);
}

test "concurrent appenders: disjoint back-to-back reservations, admitted settles to 0, head == Σ bytes" {
    const geo: CoreGeometry = .{ .page_data_len = PAGE_DATA, .pages = 64, .reserve_bytes = 0, .grain = 1 };
    const Shared = struct {
        core: JournalCore,
        reg: InflightRegistry,
        starts: [8][4]u64 = undefined,
    };
    var sh: Shared = .{ .core = JournalCore.init(geo, 0, 0), .reg = try InflightRegistry.init(testing.allocator, 0, 32) };
    defer sh.reg.deinit();
    const Worker = struct {
        fn run(s: *Shared, t: usize) void {
            for (0..4) |i| {
                const adm = s.core.tryAdmit(230, .user).?;
                const reg = s.reg.reserveRegistered(&s.core, adm) catch unreachable;
                s.starts[t][i] = reg.res.start;
                s.reg.complete(&s.core, reg.res);
            }
        }
    };
    var threads: [8]std.Thread = undefined;
    for (&threads, 0..) |*th, t| th.* = try std.Thread.spawn(.{}, Worker.run, .{ &sh, t });
    for (threads) |th| th.join();
    var all: [32]u64 = undefined;
    for (0..8) |t| for (0..4) |i| {
        all[t * 4 + i] = sh.starts[t][i];
    };
    std.mem.sort(u64, &all, {}, std.sort.asc(u64));
    for (all, 0..) |s, i| try testing.expectEqual(@as(u64, i) * 230, s);
    try testing.expectEqual(@as(u64, 32 * 230), sh.core.headPos());
    try testing.expectEqual(@as(u64, 0), sh.core.admitted());
    try testing.expectEqual(@as(u64, 32 * 230), sh.reg.completedUpto());
}

test "registry: waitCompletedUpto unblocks when a predecessor completes on another thread" {
    const geo: CoreGeometry = .{ .page_data_len = 100, .pages = 4, .reserve_bytes = 0, .grain = 1 };
    const Shared = struct { core: JournalCore, reg: InflightRegistry, res: Reservation };
    var sh: Shared = .{ .core = JournalCore.init(geo, 0, 0), .reg = try InflightRegistry.init(testing.allocator, 0, 8), .res = undefined };
    defer sh.reg.deinit();
    sh.res = (try sh.reg.reserveRegistered(&sh.core, sh.core.tryAdmit(10, .user).?)).res;
    const Completer = struct {
        fn run(s: *Shared) void {
            s.reg.complete(&s.core, s.res);
        }
    };
    const th = try std.Thread.spawn(.{}, Completer.run, .{&sh});
    sh.reg.waitCompletedUpto(10);
    th.join();
    try testing.expectEqual(@as(u64, 10), sh.reg.completedUpto());
}
