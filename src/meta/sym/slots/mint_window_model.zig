//! The mint's two lock-free halves (decision 0003 §BV) as exhaustive
//! models, each with the weakenings that break it.
//!
//! 1. `MintRecheck` — the recheck against a handover of the minted slot
//! (`slots/plane.zig` `mintStands`). Shared words: `holder`/`releasing` (the slot's lease
//! word), `g` (the lease generation), `cell` (the slot's cursor cell on the
//! minting mount), `floor` (the cursor the release carries to the
//! receiver).
//!
//!   MINTER   = check (the word names us, not releasing, g = g₀)
//!              → mint (ino = cell; cell += 1, one RMW)
//!              → recheck (the same words) ⇒ keep the ino, else burn it
//!   RELEASER = gate (releasing) → read (floor := cell)
//!              → grant (the word names B, g += 1) → B mints `floor`
//!              → B releases back (cell := max(cell, floor + 1), the word
//!                names us, g += 1)
//!
//! The law: a kept ino is never one the receiver mints (the release's
//! cursor covers every ino the minter keeps). Store buffering is explicit:
//! the gate's store may sit in the releaser's buffer until a drain step
//! (what the seq_cst store's fence forbids). Weakenings: (a) `no_recheck`
//! — the check before the RMW alone (the decision, then the mint: dev
//! before §BV); (b) `recheck_ignores_generation` — the recheck reads the
//! word only, so the slot released and granted back between the check and
//! the RMW reads as ours; (c) `gate_buffered` — the gate store drains
//! after the cursor read (no fence between the release's two halves).
//!
//! 2. `WindowClaim` — two threads claiming one slot's mint window through
//! its claim word (`mint_window.Claims.tryClaim`): read the word; free ⇒
//! CAS(free → me); a thread holds the window iff its CAS won. The law:
//! never two holders. Weakening `store_claim`: the free word read, then a
//! plain store (check-then-store) — both hold. (A claim's expiry only
//! frees a word a thread stopped using; the claim orders no mint — the
//! recheck above does.)

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

pub const RecheckWeakening = enum { none, no_recheck, recheck_ignores_generation, gate_buffered };

pub fn MintRecheck(comptime weak: RecheckWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const MINTER: usize = 0;
        const RELEASER: usize = 1;
        const US: u8 = 1;
        const B: u8 = 2;
        const NONE: u8 = 0xFF;

        holder: u8 = US,
        releasing: bool = false,
        g: u8 = 0,
        cell: u8 = 10,
        floor: u8 = NONE,
        /// The releaser's buffered gate store (`gate_buffered` only).
        gate_pending: bool = false,
        minter_pc: u8 = 0,
        releaser_pc: u8 = 0,
        ino: u8 = NONE,
        kept: u8 = NONE,
        b_minted: u8 = NONE,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return if (t == MINTER) self.minter_pc >= 3 else self.releaser_pc >= 6;
        }

        fn stands(self: *const Self, with_generation: bool) bool {
            return self.holder == US and !self.releasing and (!with_generation or self.g == 0);
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == MINTER) {
                switch (self.minter_pc) {
                    0 => if (!self.stands(true)) {
                        self.minter_pc = 3;
                        return;
                    },
                    1 => {
                        self.ino = self.cell;
                        self.cell += 1;
                    },
                    2 => {
                        const keep = switch (weak) {
                            .no_recheck => true,
                            .recheck_ignores_generation => self.stands(false),
                            .none, .gate_buffered => self.stands(true),
                        };
                        if (keep) self.kept = self.ino;
                    },
                    else => unreachable,
                }
                self.minter_pc += 1;
                return;
            }
            switch (self.releaser_pc) {
                0 => if (weak == .gate_buffered) {
                    self.gate_pending = true;
                } else {
                    self.releasing = true;
                },
                1 => self.floor = self.cell,
                // The buffered store drains here (after the read).
                2 => if (self.gate_pending) {
                    self.gate_pending = false;
                    self.releasing = true;
                },
                3 => {
                    self.holder = B;
                    self.releasing = false;
                    self.g += 1;
                },
                4 => self.b_minted = self.floor,
                5 => {
                    self.cell = @max(self.cell, self.floor + 1);
                    self.holder = US;
                    self.g += 1;
                },
                else => unreachable,
            }
            self.releaser_pc += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.kept != NONE and self.b_minted != NONE and self.kept == self.b_minted) return error.InoMintedTwice;
        }

        pub fn final(self: *const Self) !void {
            try self.invariant();
            // The minter's cell never fell below what the receiver used.
            if (self.b_minted != NONE and self.cell <= self.b_minted) return error.CursorBelowReceiver;
        }
    };
}

test "the mint against a handover of its slot: check → RMW → recheck keeps only inos the release's cursor covers, across a release and a grant back; no recheck, a recheck blind to the generation, or the gate store drained after the cursor read each keep an ino the receiver mints" {
    try explorer.expectHolds(MintRecheck(.none), .{});
    try explorer.expectFails(MintRecheck(.no_recheck), .{}, error.InvariantViolated);
    try explorer.expectFails(MintRecheck(.recheck_ignores_generation), .{}, error.InvariantViolated);
    try explorer.expectFails(MintRecheck(.gate_buffered), .{}, error.InvariantViolated);
}

pub const ClaimWeakening = enum { none, store_claim };

pub fn WindowClaim(comptime weak: ClaimWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const FREE: u8 = 0;

        word: u8 = FREE,
        pc: [2]u8 = .{ 0, 0 },
        seen: [2]u8 = .{ FREE, FREE },
        holds: [2]bool = .{ false, false },

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] >= 2;
        }

        pub fn step(self: *Self, t: usize) void {
            const me: u8 = @intCast(t + 1);
            switch (self.pc[t]) {
                0 => self.seen[t] = self.word,
                1 => if (self.seen[t] == FREE) switch (weak) {
                    .none => if (self.word == FREE) {
                        self.word = me;
                        self.holds[t] = true;
                    },
                    .store_claim => {
                        self.word = me;
                        self.holds[t] = true;
                    },
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        pub fn invariant(self: *const Self) !void {
            if (self.holds[0] and self.holds[1]) return error.TwoHolders;
        }

        pub fn final(self: *const Self) !void {
            try self.invariant();
        }
    };
}

test "the window claim: one CAS from free names one holder; a check-then-store names two" {
    try explorer.expectHolds(WindowClaim(.none), .{});
    try explorer.expectFails(WindowClaim(.store_claim), .{}, error.InvariantViolated);
}
