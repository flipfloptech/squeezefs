//! REGISTER-THEN-CHECK between a holder's extending write and a foreign
//! grant, as an exhaustive model (charter §5.5 tokens; the holder's
//! `Holder.mayHold` / `PrepareSeam.pending`, `kv_fs.landIfWatched`).
//!
//! The writer W parks its size claim under the pending map's stripe lock
//! (no commit, so no pass recalls anyone), then reads the registration
//! filter: a foreign registration ⇒ it lands and journals the claim, whose
//! pass recalls the token. The grant G counts its registration in the
//! filter, then reads the claim under the same stripe lock: a parked claim
//! ⇒ it lands it before the page. The law: at least one of them sees the
//! other — never a page without the claim AND a writer that recalled no one
//! (the reader's next append would take that page's stale EOF).
//!
//! Store buffering is explicit: each side's store sits in its own pending
//! slot until a drain — any moment, or at the latest its lock release (the
//! release publishes what precedes it). Loads read memory or the thread's
//! own pending store. Weakenings, each proven to fail: G reads the claim
//! BEFORE it registers (the order before this law: `prepare` alone, ahead
//! of the registration), and G reads the claim off the lock (the map's
//! relaxed `count` fast path), which pairs with nothing.

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

const Order = enum { register_then_check, check_then_register, check_unlocked };

fn ExtensionWatch(comptime order: Order) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const W = 0;
        const G = 1;

        // Memory.
        claim: bool = false,
        watch: bool = false,
        /// The stripe lock's holder (0 = free, else thread + 1).
        lock: u8 = 0,
        // Pending (store-buffered) stores.
        w_claim_pending: bool = false,
        g_watch_pending: bool = false,
        // What each side saw.
        w_saw_watch: bool = false,
        g_saw_claim: bool = false,
        pc: [2]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        fn steps(t: usize) u8 {
            if (t == W) return 4;
            return switch (order) {
                .register_then_check => 4,
                .check_then_register => 5,
                .check_unlocked => 3,
            };
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] >= steps(t);
        }

        fn drainW(self: *Self) void {
            if (self.w_claim_pending) self.claim = true;
            self.w_claim_pending = false;
        }

        fn drainG(self: *Self) void {
            if (self.g_watch_pending) self.watch = true;
            self.g_watch_pending = false;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == W) self.stepW() else self.stepG();
        }

        fn stepW(self: *Self) void {
            switch (self.pc[W]) {
                // lock (spins: no progress while held)
                0 => if (self.lock != 0) return else {
                    self.lock = W + 1;
                },
                1 => self.w_claim_pending = true,
                // unlock: the release publishes the park
                2 => {
                    self.drainW();
                    self.lock = 0;
                },
                // the filter's load (G's pending store is G's alone)
                3 => self.w_saw_watch = self.watch,
                else => unreachable,
            }
            self.pc[W] += 1;
        }

        fn readClaim(self: *Self) void {
            self.g_saw_claim = self.claim;
        }

        fn stepG(self: *Self) void {
            switch (order) {
                .register_then_check => switch (self.pc[G]) {
                    0 => self.g_watch_pending = true,
                    1 => if (self.lock != 0) return else {
                        self.lock = G + 1;
                    },
                    2 => self.readClaim(),
                    3 => {
                        self.drainG();
                        self.lock = 0;
                    },
                    else => unreachable,
                },
                .check_then_register => switch (self.pc[G]) {
                    0 => if (self.lock != 0) return else {
                        self.lock = G + 1;
                    },
                    1 => self.readClaim(),
                    2 => self.lock = 0,
                    3 => self.g_watch_pending = true,
                    // the registration's lock release
                    4 => self.drainG(),
                    else => unreachable,
                },
                .check_unlocked => switch (self.pc[G]) {
                    0 => self.g_watch_pending = true,
                    1 => self.readClaim(),
                    2 => self.drainG(),
                    else => unreachable,
                },
            }
            self.pc[G] += 1;
        }

        pub fn invariant(_: *const Self) !void {}

        pub fn final(self: *const Self) !void {
            if (!self.w_saw_watch and !self.g_saw_claim) return error.StaleEofServedAndNoRecall;
        }
    };
}

/// A drain the scheduler may run at any moment: the store buffer's own
/// flush, independent of the threads' program order.
fn WithEagerDrains(comptime M: type) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 4;
        m: M = .{},
        drained: [2]bool = @splat(false),

        pub fn init() Self {
            return .{ .m = M.init() };
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t < 2) self.m.done(t) else self.drained[t - 2];
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0, 1 => self.m.step(t),
                2 => {
                    self.m.drainW();
                    self.drained[0] = true;
                },
                3 => {
                    self.m.drainG();
                    self.drained[1] = true;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            return self.m.invariant();
        }
        pub fn final(self: *const Self) !void {
            return self.m.final();
        }
    };
}

test "register-then-check: a holder's parked extension is seen by the grant's post-registration check or the grant's registration by the writer — never neither; checking before registering, or off the stripe lock, serves a stale EOF and recalls no one" {
    try explorer.expectHolds(WithEagerDrains(ExtensionWatch(.register_then_check)), .{});
    try explorer.expectFails(WithEagerDrains(ExtensionWatch(.check_then_register)), .{}, error.FinalViolated);
    try explorer.expectFails(WithEagerDrains(ExtensionWatch(.check_unlocked)), .{}, error.FinalViolated);
}
