//! A handover's reservation read latch-free (`lease_table.viewReserved`,
//! decision 0003 §CQ.3) as an exhaustive model, with the three weakenings
//! that read the slot as FREE between its release and its accepter's
//! grant.
//!
//! Shared words: `word` (the slot's lease word: A releasing → unleased →
//! B's lease, with its generation), `stands` (the table's count of kept
//! stands — the latch-free gate in front of the locked read), `cell` (the
//! slot's stand words, read and written under the table lock) and `lock`.
//! Each step is one atomic action; a step that needs the lock while the
//! other thread holds it is a no-op.
//!
//!   MANAGER = [lock → count (stands += 1) → word = unleased → unlock]
//!             (`release`), then [lock → word = leased{B} (g + 1) → drop
//!             (cell cleared, stands −= 1) → unlock] (`acquire`)
//!   READER  = v = word; leased/releasing ⇒ its holder; unleased ⇒
//!             stands == 0 ? re-read : under the lock, the cell names B ⇒
//!             reserved; re-read the word: unchanged ⇒ FREE, moved ⇒ judge
//!             the new word afresh
//!
//! The slot is never free in this history (A holds it, then it is
//! reserved for B, then B holds it), so a reader that answers FREE told a
//! caller — the manager's resolver, `ResolveSlot`, the commit's mint — a
//! state that never existed (gate 173: the manager's served step judged
//! the slot its own mid-grant and read the tree the grant forgot).
//! Weakenings: (a) `count_after_word` — the release's word before its
//! count (a reader reads `unleased`, then a count of 0); (b)
//! `stand_before_word` — the grant's stand dropped before its word (the
//! reader reads `unleased`, a count of 0, then still `unleased`): dev's
//! order, the failure the matrix met; (c) `no_reread` — the reader trusts
//! its first read of the word when it finds no stand.

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

pub const Weakening = enum { none, count_after_word, stand_before_word, no_reread };

const Word = enum(u8) { releasing_a, unleased, leased_b };

const Verdict = enum(u8) { pending, holder_a, reserved_b, holder_b, free };

pub fn StandView(comptime weak: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const MANAGER: usize = 0;
        const READER: usize = 1;
        const MANAGER_END: u8 = 8;

        word: Word = .releasing_a,
        g: u8 = 1,
        stands: u8 = 0,
        /// The slot's stand words (the offer's accepter and its end).
        cell: bool = true,
        /// 0 = free, 1 = the manager holds it.
        lock: u8 = 0,
        manager_pc: u8 = 0,
        reader_pc: u8 = 0,
        seen: Word = .releasing_a,
        seen_g: u8 = 0,
        verdict: Verdict = .pending,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return if (t == MANAGER) self.manager_pc == MANAGER_END else self.verdict != .pending;
        }

        fn count(self: *Self) void {
            self.stands += 1;
        }

        fn unleased(self: *Self) void {
            self.word = .unleased;
        }

        fn grant(self: *Self) void {
            self.word = .leased_b;
            self.g += 1;
        }

        fn drop(self: *Self) void {
            self.cell = false;
            self.stands -= 1;
        }

        fn managerStep(self: *Self) void {
            switch (self.manager_pc) {
                // `release`, under the lock.
                0 => {
                    if (self.lock != 0) return;
                    self.lock = 1;
                },
                1 => if (weak == .count_after_word) self.unleased() else self.count(),
                2 => if (weak == .count_after_word) self.count() else self.unleased(),
                3 => self.lock = 0,
                // `acquire` (the accepter's grant), under the lock.
                4 => {
                    if (self.lock != 0) return;
                    self.lock = 1;
                },
                5 => if (weak == .stand_before_word) self.drop() else self.grant(),
                6 => if (weak == .stand_before_word) self.grant() else self.drop(),
                7 => self.lock = 0,
                else => unreachable,
            }
            self.manager_pc += 1;
        }

        /// The verdict on a word read: a lease or a release names its
        /// holder; `unleased` goes on to the stand.
        fn judge(self: *Self) void {
            switch (self.seen) {
                .releasing_a => self.verdict = .holder_a,
                .leased_b => self.verdict = .holder_b,
                .unleased => self.reader_pc = 1,
            }
        }

        fn readerStep(self: *Self) void {
            switch (self.reader_pc) {
                // v = word.
                0 => {
                    self.seen = self.word;
                    self.seen_g = self.g;
                    self.judge();
                },
                // The latch-free gate.
                1 => self.reader_pc = if (self.stands == 0) 3 else 2,
                // The locked read of the stand.
                2 => {
                    if (self.lock != 0) return;
                    if (self.word == .unleased and self.cell) {
                        self.verdict = .reserved_b;
                        return;
                    }
                    self.reader_pc = 3;
                },
                // No stand: the word again.
                3 => {
                    if (weak == .no_reread) {
                        self.verdict = .free;
                        return;
                    }
                    if (self.word == self.seen and self.g == self.seen_g) {
                        self.verdict = .free;
                        return;
                    }
                    self.seen = self.word;
                    self.seen_g = self.g;
                    self.judge();
                },
                else => unreachable,
            }
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == MANAGER) self.managerStep() else self.readerStep();
        }

        pub fn invariant(self: *const Self) !void {
            if (self.verdict == .free) return error.ReservedSlotReadFree;
        }

        pub fn final(self: *const Self) !void {
            if (self.verdict == .pending) return error.ReaderNeverAnswered;
        }
    };
}

test "a handover's reservation read latch-free: the release counts its stand before the word reads unleased, the grant names its grantee before the stand ends, and a reader that finds no stand re-reads the word — never FREE between A's release and B's grant; the release's word first, the grant's stand dropped first (dev's order: gate 173), or the reader trusting one read each answer FREE" {
    try explorer.expectHolds(StandView(.none), .{});
    try explorer.expectFails(StandView(.count_after_word), .{}, error.InvariantViolated);
    try explorer.expectFails(StandView(.stand_before_word), .{}, error.InvariantViolated);
    try explorer.expectFails(StandView(.no_reread), .{}, error.InvariantViolated);
}
