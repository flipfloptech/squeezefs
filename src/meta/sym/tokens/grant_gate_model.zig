//! The `GrantPassGate` register-then-check law as three exhaustive models
//! (`docs/porting/sym_tokens_intents_striping.md` §1.5; the reference's
//! loom `token_grant_models`), each with the weakening that breaks it.
//!
//! Shared words: `inflight[o]` (the gate's REFCOUNT, one lock), `holders`
//! (the lane's grant count for the object, the other lock). Each thread's
//! step takes ONE lock, so a step is one atomic action; the locks'
//! acquire/release chains order the steps and no fence is claimed.
//!
//!   PASS   = mark (inflight += 1) → read holders (recall them) → settle
//!   GRANT  = register (holders += 1) → check (inflight > 0 ⇒ Park)
//!
//! Weakenings: (a) the grant's two steps SWAPPED — the in-flight read
//! before the registration, the first build's order — lets the pass read
//! `holders == 0` while the grant reads `Proceed`: a token stands at the
//! reader that no recall reached; (b) the refcount replaced by a SET —
//! user 1's settle clears user 2's mark and a grant registered inside
//! user 2's window reads `Proceed` against a window in progress that
//! recalled nobody.

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

const Admission = enum(u8) { pending, proceed, park };

/// Model 1 — a first-touch grant racing the pass: after both the grant's
/// steps and the pass's read, `recalled ≥ 1 ∨ admission == Park`; after
/// the settle nothing is in flight.
fn FirstTouch(comptime swapped: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        inflight: u8 = 0,
        holders: u8 = 0,
        recalled: u8 = 0,
        read_done: bool = false,
        admission: Admission = .pending,
        /// The grant's check ran while the pass's window was open (before
        /// its settle): a Proceed there is only legal if the pass recalled it.
        checked_in_window: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == @as(u8, if (t == 0) 3 else 2);
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == 0) switch (self.pc[0]) {
                0 => self.inflight += 1,
                1 => {
                    self.recalled = self.holders;
                    self.read_done = true;
                },
                2 => self.inflight -= 1,
                else => unreachable,
            } else {
                const register = (self.pc[1] == 0) != swapped;
                if (register) self.holders += 1 else {
                    self.admission = if (self.inflight > 0) .park else .proceed;
                    self.checked_in_window = self.pc[0] < 3;
                }
            }
            self.pc[t] += 1;
        }
        pub fn invariant(self: *const Self) !void {
            if (self.read_done and self.pc[1] == 2 and self.checked_in_window and self.recalled == 0 and self.admission == .proceed) return error.TokenEscapedBothRecallAndPark;
        }
        pub fn final(self: *const Self) !void {
            if (self.inflight != 0) return error.StillInFlightAfterSettle;
        }
    };
}

/// Model 2 — two readers racing the pass: `recalled + parked ≥ 2` once
/// every grant decided and the pass read; `holders ≤ 2`.
fn TwoReaders(comptime swapped: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        inflight: u8 = 0,
        holders: u8 = 0,
        recalled: u8 = 0,
        read_done: bool = false,
        admission: [2]Admission = @splat(.pending),
        checked_in_window: [2]bool = @splat(false),
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == @as(u8, if (t == 0) 3 else 2);
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == 0) switch (self.pc[0]) {
                0 => self.inflight += 1,
                1 => {
                    self.recalled = self.holders;
                    self.read_done = true;
                },
                2 => self.inflight -= 1,
                else => unreachable,
            } else {
                const register = (self.pc[t] == 0) != swapped;
                if (register) self.holders += 1 else {
                    self.admission[t - 1] = if (self.inflight > 0) .park else .proceed;
                    self.checked_in_window[t - 1] = self.pc[0] < 3;
                }
            }
            self.pc[t] += 1;
        }
        fn parked(self: *const Self) u8 {
            var n: u8 = 0;
            for (self.admission) |a| n += @intFromBool(a == .park);
            return n;
        }
        fn inWindow(self: *const Self) u8 {
            var n: u8 = 0;
            for (self.checked_in_window) |c| n += @intFromBool(c);
            return n;
        }
        pub fn invariant(self: *const Self) !void {
            if (self.holders > 2) return error.HoldersOverTwo;
            // Every grant that decided inside the pass's window is either
            // among the recalled holders or parked.
            if (self.read_done and self.pc[1] == 2 and self.pc[2] == 2 and self.recalled + self.parked() < self.inWindow()) return error.AReaderEscaped;
        }
        pub fn final(self: *const Self) !void {
            if (self.inflight != 0) return error.StillInFlightAfterSettle;
        }
    };
}

/// Model 3 — two overlapping users (a pass and a failed window's rollback)
/// of one object: a grant registered INSIDE user 2's window (after its
/// holders read) PARKS whatever user 1 does. The weakening replaces the
/// refcount by a set (user 1's settle clears the mark).
fn TwoUsers(comptime as_set: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        inflight: u8 = 0,
        holders: u8 = 0,
        user_read: [2]bool = @splat(false),
        user_settled: [2]bool = @splat(false),
        admission: Admission = .pending,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == @as(u8, if (t < 2) 3 else 2);
        }
        pub fn step(self: *Self, t: usize) void {
            if (t < 2) {
                switch (self.pc[t]) {
                    0 => self.inflight = if (as_set) 1 else self.inflight + 1,
                    1 => self.user_read[t] = true,
                    2 => {
                        if (as_set) self.inflight = 0 else self.inflight -= 1;
                        self.user_settled[t] = true;
                    },
                    else => unreachable,
                }
                self.pc[t] += 1;
            } else {
                // The grant arrives inside user 2's window: its first step is
                // enabled only once user 2 read the holders (a no-op step
                // before that leaves the state unchanged — the explorer
                // collapses it).
                if (self.pc[2] == 0) {
                    if (!self.user_read[1]) return;
                    self.holders += 1;
                } else {
                    self.admission = if (self.inflight > 0) .park else .proceed;
                }
                self.pc[2] += 1;
            }
        }
        pub fn invariant(self: *const Self) !void {
            // The grant decided while user 2 is still in its window: it must
            // have parked.
            if (self.pc[2] == 2 and !self.user_settled[1] and self.admission == .proceed) return error.GrantProceededInsideAWindowThatRecalledNobody;
        }
        pub fn final(self: *const Self) !void {
            if (self.inflight != 0) return error.StillInFlightAfterSettle;
        }
    };
}

test "a first-touch grant racing the pass is recalled or parked; the grant's steps swapped lets a token escape both" {
    try explorer.expectHolds(FirstTouch(false), .{});
    try explorer.expectFails(FirstTouch(true), .{}, error.InvariantViolated);
}

test "two readers racing the pass are each recalled or parked; the same swap reads holders 1, parked 0" {
    try explorer.expectHolds(TwoReaders(false), .{});
    try explorer.expectFails(TwoReaders(true), .{}, error.InvariantViolated);
}

test "two overlapping users keep the object in flight until the last settles; the refcount as a SET lets a grant proceed inside a window in progress" {
    try explorer.expectHolds(TwoUsers(false), .{});
    try explorer.expectFails(TwoUsers(true), .{}, error.InvariantViolated);
}
