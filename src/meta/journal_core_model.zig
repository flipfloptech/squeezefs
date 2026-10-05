//! Exhaustive models of the journal reservation core (charter §9 "the journal
//! reservation (admission + reservation jointly, no over-commit)";
//! `docs/porting/kv_journal_ledger.md` §2.6), explored with
//! `model/explorer.zig`.
//!
//! Every model is a plain value type: the core's shared words are fields, each
//! thread is an explicit program counter, and every `step` performs ONE atomic
//! action (a load, an RMW, a fence boundary). The engine below restates the
//! protocol of `journal_core.zig` step for step — in BOTH forms:
//!
//! - `.reference`: the shipped two-word protocol (`head`, `admitted`; CAS on
//!   `admitted`; transfer = `head += len` then `admitted −= claim`);
//! - `.monotonic`: this port's protocol (`admitted_total` as the CAS word,
//!   `released_total`, `transferred_total`; transfer = `head += len` then
//!   `transferred_total += claim`).
//!
//! Models 1–4 are the reference's four loom models verbatim (they hold for
//! both protocols: each is sized so every admission is unconditional, which
//! the loom header says in as many words). Model 5 is the TIGHT two-thread
//! race the spec's weakening derivation needs: an admission racing a transfer
//! on a ring with no slack. Reversing EITHER access order — reading `head`
//! before the retire counter in the admission, or retiring the claim before
//! bumping `head` in the transfer — admits the under-count interleaving and
//! model 5 fails (`InvariantViolated`: the ghost commitment `head +
//! outstanding` passes `reusable_upto + L`). Model 6 is the three-party ABA
//! the module docs of `journal_core.zig` describe: the reference protocol's
//! counter CAS is blind to a 3→0→3 round trip and OVER-COMMITS; the monotonic
//! protocol holds. The last model is the in-flight registry's completed-prefix
//! law under the durability lane's out-of-order device shape, with the three
//! weakenings the reference's two-stage composition model records.
//!
//! The ghost state is the truth the words approximate: `outstanding` = Σ
//! claims admitted and neither released nor yet moved into the head; the
//! physical law is `head + outstanding ≤ reusable_upto + L`, and the
//! reserve-protection law is checked at every admission success with the
//! class's held-back bytes added.

const std = @import("std");
const explorer = @import("../model/explorer.zig");
const jc = @import("journal_core.zig");

const CoreGeometry = jc.CoreGeometry;
const Reservation = jc.Reservation;

pub const Protocol = enum { reference, monotonic };

/// The access-order weakenings of the two-counter protocol.
pub const Weakening = enum {
    none,
    /// The admission reads `head` BEFORE the retire counter (`admitted` /
    /// `transferred_total`).
    admit_reads_head_first,
    /// The transfer retires the claim from its counter BEFORE bumping `head`.
    reserve_retires_claim_first,
};

pub const Admit = struct { len: u64, class: jc.AdmissionClass = .user, keep: u64 = 0 };

/// One thread program step. `admit`/`reserve`/`release` are the core's three
/// operations; the rest let a model restate a loom test's sequential tail
/// (`join` = the loom `join()`; `advance` = `advance_reusable_upto`;
/// `release_thread` = the main thread releasing a joined thread's admission).
pub const Op = union(enum) {
    admit: Admit,
    reserve,
    release,
    release_thread: u8,
    advance: u64,
    join,
};

pub const Cfg = struct {
    geo: CoreGeometry,
    programs: []const []const Op,
    /// Entry bytes thread `t` already holds admitted at init (slack =
    /// `maxPad`); 0 = none. Lets a model start a transfer racing an admission
    /// without the admissions themselves racing.
    preadmitted: []const u64 = &.{},
    protocol: Protocol = .monotonic,
    weakening: Weakening = .none,
};

pub fn Engine(comptime cfg: Cfg) type {
    const N = cfg.programs.len;
    const geo = cfg.geo;
    const L = geo.logicalLen();
    const padded = geo.grain > 1;
    return struct {
        const Self = @This();
        pub const thread_count: usize = N;

        // ---- the shared words ----
        head: u64 = 0,
        reusable: u64 = 0,
        /// `.reference`: bytes admitted and not yet transferred.
        admitted: u64 = 0,
        /// `.monotonic`: the three monotonic totals.
        admitted_total: u64 = 0,
        released_total: u64 = 0,
        transferred_total: u64 = 0,
        // ---- ghost ----
        outstanding: u64 = 0,
        overcommit: bool = false,
        threads: [N]Thread = @splat(.{}),

        pub const Thread = struct {
            op: u8 = 0,
            pc: u8 = 0,
            // Admission-loop registers (the loads, one per step).
            a: u64 = 0,
            t: u64 = 0,
            r: u64 = 0,
            h: u64 = 0,
            u: u64 = 0,
            // Padded-reserve CAS-loop register.
            start: u64 = 0,
            holding: bool = false,
            adm_len: u64 = 0,
            adm_slack: u64 = 0,
            admits: u8 = 0,
            refusals: u8 = 0,
            has_res: bool = false,
            res: Reservation = .{ .start = 0, .len = 0, .pad = 0 },
        };

        pub fn init() Self {
            var m: Self = .{};
            for (cfg.preadmitted, 0..) |len, t| {
                if (len == 0) continue;
                const claim = len + geo.maxPad();
                m.threads[t].holding = true;
                m.threads[t].adm_len = len;
                m.threads[t].adm_slack = geo.maxPad();
                m.admitted += claim;
                m.admitted_total += claim;
                m.outstanding += claim;
            }
            return m;
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.threads[t].op == cfg.programs[t].len;
        }

        pub fn key(self: *const Self) u64 {
            var h = std.hash.Wyhash.init(0);
            std.hash.autoHash(&h, self.*);
            return h.final();
        }

        /// What `JournalCore.admitted()` answers.
        pub fn admittedNow(self: *const Self) u64 {
            return switch (cfg.protocol) {
                .reference => self.admitted,
                .monotonic => self.admitted_total - self.released_total - self.transferred_total,
            };
        }

        pub fn invariant(self: *const Self) !void {
            if (self.overcommit) return error.OverCommit;
            if (self.head + self.outstanding > self.reusable + L) return error.CommitmentPastWatermark;
        }

        pub fn final(_: *const Self) !void {}

        pub fn step(self: *Self, t: usize) void {
            const th = &self.threads[t];
            switch (cfg.programs[t][th.op]) {
                .admit => |adm| self.stepAdmit(t, adm),
                .reserve => if (padded) self.stepReservePadded(t) else self.stepReserveUnpadded(t),
                .release => {
                    self.releaseOf(t);
                    self.advanceOp(t);
                },
                .release_thread => |other| {
                    self.releaseOf(other);
                    self.advanceOp(t);
                },
                .advance => |pos| {
                    self.reusable = @max(self.reusable, pos);
                    self.advanceOp(t);
                },
                .join => {
                    for (0..N) |o| if (o != t and !self.done(o)) return;
                    self.advanceOp(t);
                },
            }
        }

        fn advanceOp(self: *Self, t: usize) void {
            self.threads[t].op += 1;
            self.threads[t].pc = 0;
        }

        /// The retire counter the admission's view subtracts and the transfer
        /// moves: `admitted` (reference) or `transferred_total` (monotonic).
        fn loadRetire(self: *const Self) u64 {
            return switch (cfg.protocol) {
                .reference => self.admitted,
                .monotonic => self.transferred_total,
            };
        }

        fn retireClaim(self: *Self, claim: u64) void {
            switch (cfg.protocol) {
                .reference => self.admitted -= claim,
                .monotonic => self.transferred_total += claim,
            }
        }

        fn releaseOf(self: *Self, t: usize) void {
            const th = &self.threads[t];
            if (!th.holding) return;
            const claim = th.adm_len + th.adm_slack;
            switch (cfg.protocol) {
                .reference => self.admitted -= claim,
                .monotonic => self.released_total += claim,
            }
            self.outstanding -= claim;
            th.holding = false;
        }

        /// The head bump of a transfer: the moment the claim's bytes move from
        /// `outstanding` into the head (the ghost truth).
        fn bumpHead(self: *Self, t: usize, len: u64, pad: u64) void {
            const th = &self.threads[t];
            th.res = .{ .start = self.head, .len = len, .pad = pad };
            th.has_res = true;
            self.head += len + pad;
            self.outstanding -= th.adm_len + th.adm_slack;
        }

        fn stepAdmit(self: *Self, t: usize, adm: Admit) void {
            const th = &self.threads[t];
            const held_back: u64 = switch (adm.class) {
                .user => geo.reserve_bytes,
                .checkpoint => adm.keep,
            };
            const claim = adm.len + geo.maxPad();
            switch (cfg.protocol) {
                .reference => switch (th.pc) {
                    // Correct: admitted, head, reusable. Weakened: head, admitted, reusable.
                    0 => {
                        if (cfg.weakening == .admit_reads_head_first) th.h = self.head else th.a = self.admitted;
                        th.pc = 1;
                    },
                    1 => {
                        if (cfg.weakening == .admit_reads_head_first) th.a = self.admitted else th.h = self.head;
                        th.pc = 2;
                    },
                    2 => {
                        th.u = self.reusable;
                        th.pc = 3;
                    },
                    3 => {
                        if (th.h + th.a + claim + held_back > th.u + L) return self.refuse(t);
                        if (self.admitted != th.a) {
                            // CAS failure: the correct loop keeps the CAS's
                            // fresh `admitted` and re-reads head; the weakened
                            // loop re-reads head first, then admitted.
                            th.a = self.admitted;
                            th.pc = if (cfg.weakening == .admit_reads_head_first) 0 else 1;
                            return;
                        }
                        self.admitted = th.a + claim;
                        self.admitSuccess(t, adm.len, claim, held_back);
                    },
                    else => unreachable,
                },
                .monotonic => switch (th.pc) {
                    0 => {
                        th.a = self.admitted_total;
                        th.pc = 1;
                    },
                    // Correct: transferred, released, head. Weakened: head, transferred, released.
                    1 => {
                        if (cfg.weakening == .admit_reads_head_first) th.h = self.head else th.t = self.transferred_total;
                        th.pc = 2;
                    },
                    2 => {
                        if (cfg.weakening == .admit_reads_head_first) th.t = self.transferred_total else th.r = self.released_total;
                        th.pc = 3;
                    },
                    3 => {
                        if (cfg.weakening == .admit_reads_head_first) th.r = self.released_total else th.h = self.head;
                        th.pc = 4;
                    },
                    4 => {
                        th.u = self.reusable;
                        th.pc = 5;
                    },
                    5 => {
                        if (th.a < th.r + th.t) {
                            th.a = self.admitted_total;
                            th.pc = 1;
                            return;
                        }
                        const outstanding = th.a - th.r - th.t;
                        if (th.h + outstanding + claim + held_back > th.u + L) return self.refuse(t);
                        if (self.admitted_total != th.a) {
                            th.a = self.admitted_total;
                            th.pc = 1;
                            return;
                        }
                        self.admitted_total = th.a + claim;
                        self.admitSuccess(t, adm.len, claim, held_back);
                    },
                    else => unreachable,
                },
            }
        }

        fn refuse(self: *Self, t: usize) void {
            self.threads[t].refusals += 1;
            self.advanceOp(t);
        }

        fn admitSuccess(self: *Self, t: usize, len: u64, claim: u64, held_back: u64) void {
            const th = &self.threads[t];
            th.holding = true;
            th.adm_len = len;
            th.adm_slack = geo.maxPad();
            th.admits += 1;
            self.outstanding += claim;
            // The law the words approximate, checked against the ghost truth
            // at the instant the admission linearises.
            if (self.head + self.outstanding + held_back > self.reusable + L) self.overcommit = true;
            self.advanceOp(t);
        }

        fn stepReserveUnpadded(self: *Self, t: usize) void {
            const th = &self.threads[t];
            std.debug.assert(th.holding);
            const claim = th.adm_len + th.adm_slack;
            const retire_first = cfg.weakening == .reserve_retires_claim_first;
            switch (th.pc) {
                // Correct: head += len (the ONE fetch_add), then retire the claim.
                0 => {
                    if (retire_first) self.retireClaim(claim) else self.bumpHead(t, th.adm_len, 0);
                    th.pc = 1;
                },
                1 => {
                    if (retire_first) self.bumpHead(t, th.adm_len, 0) else self.retireClaim(claim);
                    th.holding = false;
                    self.advanceOp(t);
                },
                else => unreachable,
            }
        }

        fn stepReservePadded(self: *Self, t: usize) void {
            const th = &self.threads[t];
            std.debug.assert(th.holding);
            const claim = th.adm_len + th.adm_slack;
            const retire_first = cfg.weakening == .reserve_retires_claim_first;
            // Correct: load head; CAS loop; retire. Weakened: retire; load head; CAS loop.
            const load_pc: u8 = if (retire_first) 1 else 0;
            const cas_pc: u8 = load_pc + 1;
            if (retire_first and th.pc == 0) {
                self.retireClaim(claim);
                th.pc = 1;
                return;
            }
            if (th.pc == load_pc) {
                th.start = self.head;
                th.pc = cas_pc;
                return;
            }
            if (th.pc == cas_pc) {
                const need = geo.padOf(th.start + th.adm_len);
                std.debug.assert(need <= th.adm_slack);
                const pad = @min(need, th.adm_slack);
                if (self.head != th.start) {
                    th.start = self.head;
                    return;
                }
                self.bumpHead(t, th.adm_len, pad);
                if (retire_first) {
                    th.holding = false;
                    self.advanceOp(t);
                } else th.pc = cas_pc + 1;
                return;
            }
            self.retireClaim(claim);
            th.holding = false;
            self.advanceOp(t);
        }
    };
}

fn tinyRing(pages: u64, reserve_bytes: u64) CoreGeometry {
    return .{ .page_data_len = 4, .pages = pages, .reserve_bytes = reserve_bytes, .grain = 1 };
}

/// Wrap an engine with a model-specific `final`.
fn Model(comptime E: type, comptime finalFn: fn (*const E) anyerror!void) type {
    return struct {
        const Self = @This();
        pub const thread_count = E.thread_count;
        eng: E,
        pub fn init() Self {
            return .{ .eng = E.init() };
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.eng.done(t);
        }
        pub fn step(self: *Self, t: usize) void {
            self.eng.step(t);
        }
        pub fn invariant(self: *const Self) !void {
            try self.eng.invariant();
        }
        pub fn final(self: *const Self) !void {
            try finalFn(&self.eng);
        }
        pub fn key(self: *const Self) u64 {
            return self.eng.key();
        }
    };
}

fn sortedPair(a: Reservation, b: Reservation) struct { Reservation, Reservation } {
    return if (a.start < b.start) .{ a, b } else .{ b, a };
}

fn expectEq(comptime T: type, want: T, got: T) !void {
    if (!std.meta.eql(want, got)) return error.FinalMismatch;
}

/// Model 1 — `journal_core_reservations_disjoint_monotonic_wrap_exact`: cap
/// 12; two threads admit+reserve 3 B each ⇒ exactly [0,3) and [3,6) in either
/// assignment, seq == start, strictly monotonic, admitted 0; then (the loom
/// test's sequential tail, as a joined third thread) `try_admit(7)` is refused
/// at head 6, `advance_reusable_upto(6)` opens it, and the wrapping
/// reservation is [6,13) (its lap/segment derivation is a geometry unit test).
pub fn DisjointMonotonicWrap(comptime protocol: Protocol, comptime weakening: Weakening) type {
    const E = Engine(.{
        .geo = tinyRing(3, 0),
        .programs = &.{
            &.{ .{ .admit = .{ .len = 3 } }, .reserve },
            &.{ .{ .admit = .{ .len = 3 } }, .reserve },
            &.{ .join, .{ .admit = .{ .len = 7 } }, .{ .advance = 6 }, .{ .admit = .{ .len = 7 } }, .reserve },
        },
        .protocol = protocol,
        .weakening = weakening,
    });
    return Model(E, struct {
        fn final(m: *const E) !void {
            if (!m.threads[0].has_res or !m.threads[1].has_res) return error.MissingReservation;
            const a, const b = sortedPair(m.threads[0].res, m.threads[1].res);
            try expectEq(Reservation, .{ .start = 0, .len = 3, .pad = 0 }, a);
            try expectEq(Reservation, .{ .start = 3, .len = 3, .pad = 0 }, b);
            if (a.end() > b.start or b.seq() <= a.seq()) return error.Overlap;
            const tail = m.threads[2];
            try expectEq(u8, 1, tail.refusals);
            try expectEq(u8, 1, tail.admits);
            try expectEq(Reservation, .{ .start = 6, .len = 7, .pad = 0 }, tail.res);
            try expectEq(u64, 13, m.head);
            try expectEq(u64, 0, m.admittedNow());
        }
    }.final);
}

/// Model 2 — `journal_core_admission_never_overcommits_reserve_protected`:
/// cap 8, reserve 3 ⇒ user budget 5; two racing user admits of 3: EXACTLY ONE
/// succeeds; the checkpoint class admits 3 more from the carve-out; a user
/// retry is still refused; releases settle `admitted` to 0.
pub fn NoOvercommitReserveProtected(comptime protocol: Protocol, comptime weakening: Weakening) type {
    const E = Engine(.{
        .geo = tinyRing(2, 3),
        .programs = &.{
            &.{.{ .admit = .{ .len = 3 } }},
            &.{.{ .admit = .{ .len = 3 } }},
            &.{ .join, .{ .admit = .{ .len = 3, .class = .checkpoint } }, .{ .admit = .{ .len = 3 } }, .release, .{ .release_thread = 0 }, .{ .release_thread = 1 } },
        },
        .protocol = protocol,
        .weakening = weakening,
    });
    return Model(E, struct {
        fn final(m: *const E) !void {
            if ((m.threads[0].admits == 1) == (m.threads[1].admits == 1)) return error.NotExactlyOne;
            try expectEq(u8, 1, m.threads[2].admits);
            try expectEq(u8, 1, m.threads[2].refusals);
            try expectEq(u64, 0, m.admittedNow());
            try expectEq(u64, 0, m.head);
        }
    }.final);
}

/// Model 3 — `journal_core_budget_conserved_across_transfer_release`: cap 8;
/// a racing reserve(2) and release(2) settle with admitted 0, head 2, range
/// [0,2); then an exact-fit 6 admits and 1 more is refused.
pub fn BudgetConservedTransferRelease(comptime protocol: Protocol, comptime weakening: Weakening) type {
    const E = Engine(.{
        .geo = tinyRing(2, 0),
        .programs = &.{
            &.{ .{ .admit = .{ .len = 2 } }, .reserve },
            &.{ .{ .admit = .{ .len = 2 } }, .release },
            &.{ .join, .{ .admit = .{ .len = 6 } }, .{ .admit = .{ .len = 1 } }, .release },
        },
        .protocol = protocol,
        .weakening = weakening,
    });
    return Model(E, struct {
        fn final(m: *const E) !void {
            try expectEq(u64, 0, m.admittedNow());
            try expectEq(u64, 2, m.head);
            try expectEq(Reservation, .{ .start = 0, .len = 2, .pad = 0 }, m.threads[0].res);
            try expectEq(u8, 1, m.threads[2].admits);
            try expectEq(u8, 1, m.threads[2].refusals);
        }
    }.final);
}

/// Model 4 — `journal_core_padded_reserve_lands_every_end_sector_aligned`:
/// 64 B pages, 2 pages, grain 8 (`(24+off) % 8 == 0 ⇔ off % 8 == 0`, max_pad
/// 29); two racing admit(3)+reserve ⇒ a.start == 0, b.start == a.padded_end,
/// every pad ≤ max_pad and == pad_of(end), every padded end sector-aligned,
/// head == Σ padded_len, admitted == 0.
pub fn PaddedReserveAligned(comptime protocol: Protocol, comptime weakening: Weakening) type {
    const geo: CoreGeometry = .{ .page_data_len = 64, .pages = 2, .reserve_bytes = 0, .grain = 8 };
    const E = Engine(.{
        .geo = geo,
        .programs = &.{
            &.{ .{ .admit = .{ .len = 3 } }, .reserve },
            &.{ .{ .admit = .{ .len = 3 } }, .reserve },
        },
        .protocol = protocol,
        .weakening = weakening,
    });
    return Model(E, struct {
        fn final(m: *const E) !void {
            if (!m.threads[0].has_res or !m.threads[1].has_res) return error.MissingReservation;
            if (m.threads[0].adm_slack != geo.maxPad() or m.threads[1].adm_slack != geo.maxPad()) return error.SlackMissing;
            const a, const b = sortedPair(m.threads[0].res, m.threads[1].res);
            try expectEq(u64, 0, a.start);
            try expectEq(u64, 3, a.len);
            try expectEq(u64, a.paddedEnd(), b.start);
            for ([_]Reservation{ a, b }) |r| {
                if (r.pad > geo.maxPad()) return error.PadPastMax;
                if (!geo.sectorAligned(r.paddedEnd())) return error.EndNotAligned;
                if (r.pad != geo.padOf(r.end())) return error.PadNotTheLaw;
                if (r.pad != 0 and r.pad < jc.PAD_MIN) return error.PadBelowMin;
            }
            try expectEq(u64, a.paddedLen() + b.paddedLen(), m.head);
            try expectEq(u64, 0, m.admittedNow());
        }
    }.final);
}

/// Model 5 — the tight race the weakening derivation needs: cap 8, no slack;
/// thread 0 already holds 5 admitted and transfers it; thread 1 admits 4.
/// 5 + 4 > 8, so the admission must ALWAYS be refused: the transfer's bytes
/// must be seen at least once (in the head or still outstanding) by every
/// interleaving. Either order reversal lets one interleaving see them zero
/// times and over-commit by 4.
pub fn AdmitRacesTransfer(comptime protocol: Protocol, comptime weakening: Weakening) type {
    const E = Engine(.{
        .geo = tinyRing(2, 0),
        .programs = &.{
            &.{.reserve},
            &.{.{ .admit = .{ .len = 4 } }},
        },
        .preadmitted = &.{ 5, 0 },
        .protocol = protocol,
        .weakening = weakening,
    });
    return Model(E, struct {
        fn final(m: *const E) !void {
            try expectEq(u8, 1, m.threads[1].refusals);
            try expectEq(Reservation, .{ .start = 0, .len = 5, .pad = 0 }, m.threads[0].res);
            try expectEq(u64, 5, m.head);
            try expectEq(u64, 0, m.admittedNow());
        }
    }.final);
}

/// Model 6 — the three-party ABA (`journal_core.zig` module docs): cap 8;
/// X holds 3 and transfers; Y admits 3; Z admits 5. Z reads the counter (3)
/// then head (0); X moves 3 to the head and retires it; Y admits 3 (counter
/// back to 3); Z's CAS on the byte count succeeds — Y's bytes counted zero
/// times, commitment 11 of 8. The reference protocol FAILS; the monotonic
/// CAS word cannot round-trip and the model holds.
pub fn AbaAdmitSlipsPastCounterCas(comptime protocol: Protocol) type {
    const E = Engine(.{
        .geo = tinyRing(2, 0),
        .programs = &.{
            &.{.reserve},
            &.{.{ .admit = .{ .len = 3 } }},
            &.{.{ .admit = .{ .len = 5 } }},
        },
        .preadmitted = &.{ 3, 0, 0 },
        .protocol = protocol,
    });
    return Model(E, struct {
        fn final(m: *const E) !void {
            try expectEq(u64, 3, m.head);
            if (m.head + m.admittedNow() > 8) return error.OverCommitted;
            // Y and Z cannot both admit (3 + 3 + 5 > 8).
            if (m.threads[1].admits == 1 and m.threads[2].admits == 1) return error.BothAdmitted;
        }
    }.final);
}

// ---------------------------------------------------------------------------
// The in-flight registry's completed-prefix law under the durability lane.
// ---------------------------------------------------------------------------

pub const PrefixWeakening = enum {
    none,
    /// The reservation completes at submission (stage A) instead of in the
    /// lane after the write landed.
    complete_at_submit,
    /// The device's `landed` publish is Relaxed: the lane can read a stale
    /// write outcome for a landed window.
    landed_relaxed,
    /// The lane's `acked` publish is Relaxed: a committer can read a stale
    /// answer behind the ack.
    ack_relaxed,
};

/// The registry (`reserve_registered` / `complete` / `completed_upto`
/// restated verbatim: `completed_upto = min(open)`, else `head`) under the
/// two-stage shape the reference's composition model explores: three 1-byte
/// windows reserved in order by stage A, the device landing window 1 FIRST as
/// a FAILURE, then 0, then 2; the lane awaiting the head window, taking every
/// landed successor, completing the group in journal order, ONE prefix check,
/// then acking in order; a committer probing each ack. Store buffering is
/// explicit: a value store sits in the publisher's buffer until its Release
/// publish drains it (or a later step does, under the Relaxed weakenings).
///
/// Laws checked after every step: the watermark never covers an unlanded
/// write (`!landed[i] ⇒ completed_upto ≤ i` — a parked predecessor holds its
/// successors); acks never leave journal order; no window is answered before
/// every predecessor landed; the group's in-order completion carries the
/// watermark to its end; the answer a committer reads is the write's true
/// outcome and its prefix is complete.
pub fn CompletedPrefix(comptime weakening: PrefixWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 4;
        const W: u8 = 3;
        const FAILED: u8 = 1;
        const device_order = [W]u8{ 1, 0, 2 };
        const a_steps: u8 = if (weakening == .complete_at_submit) 3 else 2;
        const lane_done: u8 = 99;

        // Ring + registry: window i is the reservation [i, i+1).
        head: u8 = 0,
        open: [W]bool = @splat(false),
        completed_upto: u8 = 0,
        // Stage A → device.
        submitted: [W]bool = @splat(false),
        // Device → lane: the outcome, then `landed` (Release). `write_ok`
        // starts at the stale default (false) — the truth for the two OK
        // windows is true, so a stale read answers a landed OK write as a
        // failure (the reference model's law 3).
        write_ok: [W]bool = @splat(false),
        pending_ok: [W]bool = @splat(false),
        landed: [W]bool = @splat(false),
        // Lane → committer: the answer, then `acked` (Release).
        ack_ok: [W]bool = @splat(false),
        ack_val: [W]bool = @splat(false),
        pending_ack: [W]bool = @splat(false),
        acked: [W]bool = @splat(false),
        next_ack: u8 = 0,
        // Program counters.
        a_pc: u8 = 0,
        d_pc: u8 = 0,
        l_pc: u8 = 0,
        l_cursor: u8 = 0,
        l_group_end: u8 = 0,
        l_idx: u8 = 0,
        l_ok: bool = false,
        c_pc: u8 = 0,
        // Violations.
        ack_out_of_order: bool = false,
        ack_passed_unlanded_predecessor: bool = false,
        prefix_short: bool = false,
        stale_answer: bool = false,
        answered_before_prefix: bool = false,

        pub fn init() Self {
            return .{};
        }

        pub fn key(self: *const Self) u64 {
            var h = std.hash.Wyhash.init(0);
            std.hash.autoHash(&h, self.*);
            return h.final();
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                0 => self.a_pc == a_steps * W,
                1 => self.d_pc == 4 * W,
                2 => self.l_pc == lane_done,
                3 => self.c_pc == 3 * W,
                else => unreachable,
            };
        }

        fn truth(w: u8) bool {
            return w != FAILED;
        }

        /// `complete`: remove, then the watermark is the oldest open start or
        /// the head when none is open.
        fn complete(self: *Self, w: u8) void {
            self.open[w] = false;
            self.completed_upto = self.head;
            var i: u8 = 0;
            while (i < W) : (i += 1) if (self.open[i]) {
                self.completed_upto = i;
                break;
            };
        }

        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0 => self.stepStageA(),
                1 => self.stepDevice(),
                2 => self.stepLane(),
                3 => self.stepCommitter(),
                else => unreachable,
            }
        }

        fn stepStageA(self: *Self) void {
            const w = self.a_pc / a_steps;
            switch (self.a_pc % a_steps) {
                // reserve_registered under the registry lock: head bump + open.
                0 => {
                    self.head = w + 1;
                    self.open[w] = true;
                },
                1 => self.submitted[w] = true,
                2 => self.complete(w),
                else => unreachable,
            }
            self.a_pc += 1;
        }

        fn stepDevice(self: *Self) void {
            const w = device_order[self.d_pc / 4];
            switch (self.d_pc % 4) {
                0 => if (!self.submitted[w]) return,
                // The outcome store: buffered until the publish drains it.
                1 => self.pending_ok[w] = true,
                2 => {
                    if (weakening != .landed_relaxed) self.drainOk(w);
                    self.landed[w] = true;
                },
                3 => self.drainOk(w),
                else => unreachable,
            }
            self.d_pc += 1;
        }

        fn drainOk(self: *Self, w: u8) void {
            if (self.pending_ok[w]) {
                self.write_ok[w] = truth(w);
                self.pending_ok[w] = false;
            }
        }

        fn drainAck(self: *Self, w: u8) void {
            if (self.pending_ack[w]) {
                self.ack_ok[w] = self.ack_val[w];
                self.pending_ack[w] = false;
            }
        }

        fn stepLane(self: *Self) void {
            switch (self.l_pc) {
                // Await the head window's landing.
                0 => {
                    if (self.l_cursor == W) {
                        self.l_pc = lane_done;
                        return;
                    }
                    if (!self.landed[self.l_cursor]) return;
                    self.l_group_end = self.l_cursor + 1;
                    self.l_pc = 1;
                },
                // Take every landed successor (one probe per step).
                1 => {
                    if (self.l_group_end < W and self.landed[self.l_group_end]) {
                        self.l_group_end += 1;
                        return;
                    }
                    self.l_idx = self.l_cursor;
                    self.l_pc = 2;
                },
                // Complete the group in journal order.
                2 => {
                    self.complete(self.l_idx);
                    self.l_idx += 1;
                    if (self.l_idx == self.l_group_end) self.l_pc = 3;
                },
                // ONE completed-prefix check per group.
                3 => {
                    if (self.completed_upto < self.l_group_end) self.prefix_short = true;
                    self.l_idx = self.l_cursor;
                    self.l_pc = 4;
                },
                // Per window: read the outcome …
                4 => {
                    self.l_ok = self.write_ok[self.l_idx];
                    self.l_pc = 5;
                },
                // … take the ack ticket and buffer the answer …
                5 => {
                    const expected = self.next_ack;
                    self.next_ack += 1;
                    if (self.l_idx != expected) self.ack_out_of_order = true;
                    var j: u8 = 0;
                    while (j < self.l_idx) : (j += 1) if (!self.landed[j]) {
                        self.ack_passed_unlanded_predecessor = true;
                    };
                    self.ack_val[self.l_idx] = self.l_ok;
                    self.pending_ack[self.l_idx] = true;
                    self.l_pc = 6;
                },
                // … publish the ack (Release drains the answer first) …
                6 => {
                    if (weakening != .ack_relaxed) self.drainAck(self.l_idx);
                    self.acked[self.l_idx] = true;
                    self.l_pc = 7;
                },
                // … and the buffered answer lands at some later point.
                7 => {
                    self.drainAck(self.l_idx);
                    self.l_idx += 1;
                    if (self.l_idx == self.l_group_end) {
                        self.l_cursor = self.l_group_end;
                        self.l_pc = 0;
                    } else self.l_pc = 4;
                },
                else => unreachable,
            }
        }

        fn stepCommitter(self: *Self) void {
            const w: u8 = self.c_pc / 3;
            switch (self.c_pc % 3) {
                0 => if (!self.acked[w]) return,
                1 => if (self.ack_ok[w] != truth(w)) {
                    self.stale_answer = true;
                },
                2 => if (self.completed_upto < w + 1) {
                    self.answered_before_prefix = true;
                },
                else => unreachable,
            }
            self.c_pc += 1;
        }

        pub fn invariant(self: *const Self) !void {
            var i: u8 = 0;
            while (i < W) : (i += 1) {
                if (!self.landed[i] and self.completed_upto > i) return error.WatermarkCoversUnlandedWrite;
            }
            if (self.ack_out_of_order) return error.AckLeftJournalOrder;
            if (self.ack_passed_unlanded_predecessor) return error.AckPassedParkedPredecessor;
            if (self.prefix_short) return error.GroupCompletionShortOfItsEnd;
            if (self.stale_answer) return error.StaleAnswerBehindAck;
            if (self.answered_before_prefix) return error.AnsweredBeforePrefixComplete;
        }

        pub fn final(self: *const Self) !void {
            try expectEq(u8, W, self.next_ack);
            try expectEq(u8, W, self.completed_upto);
            try expectEq([W]bool, .{ true, true, true }, self.acked);
            try expectEq([W]bool, .{ true, false, true }, self.ack_ok);
            try expectEq([W]bool, .{ false, false, false }, self.open);
        }
    };
}

// ---------------------------------------------------------------------------
// Tests: every model holds; every recorded weakening fails.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "model 1: disjoint, back-to-back, monotonic reservations and the lap-1 wrap after the watermark opens" {
    try explorer.expectHolds(DisjointMonotonicWrap(.monotonic, .none), .{});
    try explorer.expectHolds(DisjointMonotonicWrap(.reference, .none), .{});
}

test "model 2: exactly one of two racing user admits fits the user budget; the checkpoint carve-out is never user-consumable" {
    try explorer.expectHolds(NoOvercommitReserveProtected(.monotonic, .none), .{});
    try explorer.expectHolds(NoOvercommitReserveProtected(.reference, .none), .{});
}

test "model 2 is NOT a weakening detector: with no transfer in the race, the read-order reversal cannot under-count (the spec's derivation names the wrong model)" {
    try explorer.expectHolds(NoOvercommitReserveProtected(.monotonic, .admit_reads_head_first), .{});
    try explorer.expectHolds(NoOvercommitReserveProtected(.reference, .admit_reads_head_first), .{});
}

test "model 3: admitted budget is conserved across a racing transfer and release; exact fit re-admits, one byte more refuses" {
    try explorer.expectHolds(BudgetConservedTransferRelease(.monotonic, .none), .{});
    try explorer.expectHolds(BudgetConservedTransferRelease(.reference, .none), .{});
}

test "model 4: padded reserve lands every end sector-aligned behind the law's pad, within the slack, conserving the budget" {
    try explorer.expectHolds(PaddedReserveAligned(.monotonic, .none), .{});
    try explorer.expectHolds(PaddedReserveAligned(.reference, .none), .{});
}

test "model 5: an admission racing a transfer never under-counts — and FAILS under either access-order reversal (the recorded weakening)" {
    try explorer.expectHolds(AdmitRacesTransfer(.monotonic, .none), .{});
    try explorer.expectHolds(AdmitRacesTransfer(.reference, .none), .{});
    try explorer.expectFails(AdmitRacesTransfer(.monotonic, .admit_reads_head_first), .{}, error.InvariantViolated);
    try explorer.expectFails(AdmitRacesTransfer(.monotonic, .reserve_retires_claim_first), .{}, error.InvariantViolated);
    try explorer.expectFails(AdmitRacesTransfer(.reference, .admit_reads_head_first), .{}, error.InvariantViolated);
    try explorer.expectFails(AdmitRacesTransfer(.reference, .reserve_retires_claim_first), .{}, error.InvariantViolated);
}

test "model 5 on the padded CAS-loop reserve: the same two reversals fail" {
    const geo: CoreGeometry = .{ .page_data_len = 64, .pages = 1, .reserve_bytes = 0, .grain = 8 };
    // Capacity 64; X holds 20 (+29 slack = 49); Y admits 3 (+29 = 32): 49 + 32 > 64 ⇒ refused always.
    const Tight = struct {
        fn Make(comptime weakening: Weakening) type {
            const E = Engine(.{
                .geo = geo,
                .programs = &.{ &.{.reserve}, &.{.{ .admit = .{ .len = 3 } }} },
                .preadmitted = &.{ 20, 0 },
                .weakening = weakening,
            });
            return Model(E, struct {
                fn final(m: *const E) !void {
                    try expectEq(u8, 1, m.threads[1].refusals);
                    try expectEq(u64, 0, m.admittedNow());
                }
            }.final);
        }
    };
    try explorer.expectHolds(Tight.Make(.none), .{});
    try explorer.expectFails(Tight.Make(.admit_reads_head_first), .{}, error.InvariantViolated);
    try explorer.expectFails(Tight.Make(.reserve_retires_claim_first), .{}, error.InvariantViolated);
}

test "model 6: the reference's byte-count CAS over-commits under a three-party ABA; the monotonic CAS word holds" {
    try explorer.expectFails(AbaAdmitSlipsPastCounterCas(.reference), .{}, error.InvariantViolated);
    try explorer.expectHolds(AbaAdmitSlipsPastCounterCas(.monotonic), .{});
}

test "completed-prefix model holds; completing at submit, a Relaxed landed publish and a Relaxed ack publish each fail" {
    try explorer.expectHolds(CompletedPrefix(.none), .{});
    try explorer.expectFails(CompletedPrefix(.complete_at_submit), .{}, error.InvariantViolated);
    try explorer.expectFails(CompletedPrefix(.landed_relaxed), .{}, error.InvariantViolated);
    try explorer.expectFails(CompletedPrefix(.ack_relaxed), .{}, error.InvariantViolated);
}

fn census(comptime M: type, name: []const u8) !void {
    var report: explorer.Report = undefined;
    try explorer.explore(M, testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    std.debug.print("  {s}: {d} states, deepest schedule {d}\n", .{ name, report.states, report.max_depth_seen });
}

test "model state census (the evidence note's numbers)" {
    std.debug.print("\n", .{});
    try census(DisjointMonotonicWrap(.monotonic, .none), "disjoint_monotonic_wrap");
    try census(NoOvercommitReserveProtected(.monotonic, .none), "no_overcommit_reserve_protected");
    try census(BudgetConservedTransferRelease(.monotonic, .none), "budget_conserved_transfer_release");
    try census(PaddedReserveAligned(.monotonic, .none), "padded_reserve_aligned");
    try census(AdmitRacesTransfer(.monotonic, .none), "admit_races_transfer");
    try census(AbaAdmitSlipsPastCounterCas(.monotonic), "aba_admit_slips_past_counter_cas");
    try census(CompletedPrefix(.none), "completed_prefix");
}
