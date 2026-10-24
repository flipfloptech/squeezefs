//! Explorer model of a device lane's FREE-CELL DOOR (`io/nvme_dev.zig`
//! `Lane.acquireCell` / `Lane.freeCell`; decision 0003 §BO's law for the
//! data plane: a full lane parks its late caller, never refuses).
//!
//! Two holders each own one cell of a lane whose free list is empty; two
//! callers each need a cell. A caller pops (the fast path); on an empty
//! list it loads the door's word (the epoch), counts itself
//! (`cell_waiters`, a SeqCst RMW), re-pops, and parks on the word (a futex
//! wait: returns at once when the word moved, else sleeps until a wake);
//! woken, it un-counts and goes round again. A holder gives its cell back
//! (the free list's publishing store), then reads the count with a SeqCst
//! RMW and, while it is non-zero, bumps the word and wakes ONE sleeper.
//! Store buffering is modelled explicitly: the give-back's store sits in
//! the holder's buffer until a step drains it; the RMW drains it first (a
//! full fence on every host, and in the language model the RMW pair on the
//! count orders the push before the caller's re-pop).
//!
//! Holds: both callers get a cell — never a caller asleep with a cell free
//! and no wake owed. Weakenings, each stranding a caller:
//!   `.freer_plain_load` — the holder reads the count with a plain load
//!     while its give-back is still buffered: it reads 0, the caller counts
//!     and re-pops an empty list, sleeps, then the store lands, unannounced.
//!   `.count_after_repop` — the caller re-pops before it counts: the
//!     holder's give-back and read land between the two, see no waiter.
//!   `.epoch_after_repop` — the caller loads the word after its failed
//!     re-pop: the holder's give-back, bump and wake land between the two,
//!     and the caller sleeps on the bumped word.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Weakening = enum { none, freer_plain_load, count_after_repop, epoch_after_repop };

fn Door(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 4;
        const holders = 2;

        const Hpc = enum(u8) { push, fence_read, load, drain, wake, done };
        const Cpc = enum(u8) { fast_pop, load_epoch, count, repop, park, asleep, uncount, got };

        /// Cells on the free list (visible to every thread).
        free: u8 = 0,
        waiters: u8 = 0,
        epoch: u8 = 0,
        /// A holder's give-back still in its store buffer.
        pending: [holders]bool = @splat(false),
        /// The count a holder read.
        read: [holders]u8 = @splat(0),
        hpc: [holders]Hpc = @splat(.push),
        cpc: [thread_count - holders]Cpc = @splat(.fast_pop),
        c_epoch: [thread_count - holders]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        fn holdersDone(self: *const Self) bool {
            for (self.hpc) |h| if (h != .done) return false;
            return true;
        }

        pub fn done(self: *const Self, t: usize) bool {
            if (t < holders) return self.hpc[t] == .done;
            const c = self.cpc[t - holders];
            // Asleep with every holder gone: nobody is left to wake it.
            return c == .got or (c == .asleep and self.holdersDone());
        }

        fn pop(self: *Self) bool {
            if (self.free == 0) return false;
            self.free -= 1;
            return true;
        }

        fn stepHolder(self: *Self, h: usize) void {
            switch (self.hpc[h]) {
                .push => {
                    self.pending[h] = true;
                    self.hpc[h] = if (weakening == .freer_plain_load) .load else .fence_read;
                },
                .fence_read => {
                    // The SeqCst RMW: drains the buffer, then reads.
                    self.drainPush(h);
                    self.read[h] = self.waiters;
                    self.hpc[h] = .wake;
                },
                .load => {
                    self.read[h] = self.waiters;
                    self.hpc[h] = .drain;
                },
                .drain => {
                    self.drainPush(h);
                    self.hpc[h] = .wake;
                },
                .wake => {
                    if (self.read[h] != 0) {
                        self.epoch +%= 1;
                        // Wake ONE sleeper (the callers are symmetric).
                        for (&self.cpc) |*c| if (c.* == .asleep) {
                            c.* = .uncount;
                            break;
                        };
                    }
                    self.hpc[h] = .done;
                },
                .done => unreachable,
            }
        }

        fn drainPush(self: *Self, h: usize) void {
            if (!self.pending[h]) return;
            self.pending[h] = false;
            self.free += 1;
        }

        fn stepCaller(self: *Self, i: usize) void {
            const c = &self.cpc[i];
            switch (c.*) {
                .fast_pop => c.* = if (self.pop()) .got else self.firstSlowStep(),
                .load_epoch => {
                    self.c_epoch[i] = self.epoch;
                    c.* = switch (weakening) {
                        .count_after_repop => .repop,
                        .epoch_after_repop => .park,
                        else => .count,
                    };
                },
                .count => {
                    self.waiters += 1;
                    c.* = switch (weakening) {
                        .count_after_repop => .park,
                        else => .repop,
                    };
                },
                .repop => {
                    if (self.pop()) {
                        // Got one: un-count unless the count comes later.
                        if (weakening != .count_after_repop) self.waiters -= 1;
                        c.* = .got;
                        return;
                    }
                    c.* = switch (weakening) {
                        .count_after_repop => .count,
                        .epoch_after_repop => .load_epoch,
                        else => .park,
                    };
                },
                // The futex wait: the word moved ⇒ return at once.
                .park => c.* = if (self.epoch != self.c_epoch[i]) .uncount else .asleep,
                // Blocked until a holder's wake moves it to `.uncount`.
                .asleep => {},
                .uncount => {
                    self.waiters -= 1;
                    c.* = self.firstSlowStep();
                },
                .got => unreachable,
            }
        }

        fn firstSlowStep(_: *const Self) Cpc {
            return switch (weakening) {
                .epoch_after_repop => .count,
                else => .load_epoch,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            if (t < holders) self.stepHolder(t) else self.stepCaller(t - holders);
        }

        pub fn invariant(_: *const Self) !void {}

        pub fn final(self: *const Self) !void {
            for (self.cpc) |c| if (c != .got) return error.CallerStrandedWithACellFree;
            if (self.free != 0 or self.waiters != 0) return error.DoorLeftUnbalanced;
        }
    };
}

test "the free-cell door: two callers parked on a full lane both get the two cells their holders give back — never asleep with a cell free; each weakening strands one" {
    try explorer.expectHolds(Door(.none), .{});
    try explorer.expectFails(Door(.freer_plain_load), .{}, error.FinalViolated);
    try explorer.expectFails(Door(.count_after_repop), .{}, error.FinalViolated);
    try explorer.expectFails(Door(.epoch_after_repop), .{}, error.FinalViolated);
}
