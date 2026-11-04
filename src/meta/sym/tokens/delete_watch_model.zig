//! THE CROSS-MEMBER DELETE'S HANDSHAKE (decision 0003 §DH) between a
//! holder's delete and a member's grant, as an exhaustive model (the
//! holder's `Holder.registerLocked` / `Holder.mayHoldOrdered`, `kv_fs.
//! DeleteRegistry.insert` / `has`, `PrepareSeam.deleted`).
//!
//! The delete D registers the ino under its registry shard's lock, THEN raises
//! the registry's filter cell (still under the lock: the cells share its
//! line; the unlock queues behind a buffered raise), THEN reads the holder's
//! registration filter
//! (a member may hold a grant made before the delete ⇒ D kicks the destroy,
//! whose pass recalls that grant). A grant G registers under the holder's
//! lock — the FIRST registrant of the object raises the filter's watch
//! cell, a later one finds it raised — THEN reads the registry's filter
//! cell: 0 ⇒ the object is live, the page is served with no lock; non-zero
//! ⇒ the shard's lock and its set decide (another deleted ino may share
//! the cell). The law: a grant that serves a page of the deleted file is
//! one the delete saw — never a served page AND a delete that recalled no
//! one (the member's cached token would serve the deleted file).
//!
//! Store buffering is explicit: the two raises are the Dekker pair's
//! stores and sit in their thread's pending slot until a drain — at once
//! when the access is SeqCst (an SC store IS a drained store: Zig 0.17 has
//! no standalone fence, so the four accesses are themselves SeqCst), else
//! at any moment the scheduler picks. Loads read memory. A lock's data is
//! consistent for its holders. Weakenings, each proven to fail: the
//! delete's raise not SeqCst (its watch read passes its own store) and the
//! grant's raise not SeqCst (its cell read passes its registration).

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

const Variant = enum { strict, no_delete_fence, no_grant_fence };

fn DeleteWatch(comptime variant: Variant, comptime other_deleted: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const D = 0;
        const G0 = 1;

        // Memory.
        /// The registry's shard holds the ino (under `rlock`, its lock).
        entry: bool = false,
        /// The registry's filter cell (another deleted ino may share it).
        cell: u8 = @intFromBool(other_deleted),
        /// The holder's watch cell: the object has a registration.
        watch: u8 = 0,
        /// The holder's registrations of the object (under `hlock`).
        reg: u2 = 0,
        /// Lock holders (0 = free, else thread + 1).
        rlock: u8 = 0,
        hlock: u8 = 0,
        // Pending (store-buffered) stores: the raises, and a grant's
        // unlock queued behind its raise (FIFO — TSO's order).
        d_cell_pending: bool = false,
        d_unlock_pending: bool = false,
        g_watch_pending: [2]bool = @splat(false),
        g_unlock_pending: [2]bool = @splat(false),
        // What each side decided.
        d_saw_watch: bool = false,
        served: [2]bool = @splat(false),
        gone: [2]bool = @splat(false),
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                D => self.pc[D] >= 5,
                else => self.pc[t] >= 8,
            };
        }

        pub fn drainD(self: *Self) void {
            if (self.d_cell_pending) self.cell += 1;
            self.d_cell_pending = false;
            if (self.d_unlock_pending) self.rlock = 0;
            self.d_unlock_pending = false;
        }

        pub fn drainG(self: *Self, g: usize) void {
            if (self.g_watch_pending[g]) self.watch += 1;
            self.g_watch_pending[g] = false;
            if (self.g_unlock_pending[g]) self.hlock = 0;
            self.g_unlock_pending[g] = false;
        }

        pub fn step(self: *Self, t: usize) void {
            if (t == D) self.stepD() else self.stepG(t - G0);
        }

        fn stepD(self: *Self) void {
            switch (self.pc[D]) {
                0 => if (self.rlock != 0) return else {
                    self.rlock = D + 1;
                },
                1 => self.entry = true,
                // The filter cell's raise, under the shard's lock (the
                // cells share its line).
                2 => {
                    self.d_cell_pending = true;
                    if (variant != .no_delete_fence) self.drainD();
                },
                // The unlock's release store queues behind a pending raise.
                3 => if (self.d_cell_pending) {
                    self.d_unlock_pending = true;
                } else {
                    self.rlock = 0;
                },
                // The holder's registration filter.
                4 => self.d_saw_watch = self.watch != 0,
                else => unreachable,
            }
            self.pc[D] += 1;
        }

        fn stepG(self: *Self, g: usize) void {
            const t = G0 + g;
            const bit: u2 = @as(u2, 1) << @intCast(g);
            switch (self.pc[t]) {
                0 => if (self.hlock != 0) return else {
                    self.hlock = @intCast(t + 1);
                },
                // The registration: the object's first registrant raises
                // the watch cell.
                1 => {
                    if (self.reg == 0) {
                        self.g_watch_pending[g] = true;
                        if (variant != .no_grant_fence) self.drainG(g);
                    }
                    self.reg |= bit;
                },
                // The unlock's release store queues behind a pending raise:
                // no other registrant enters before the raise is visible.
                2 => if (self.g_watch_pending[g]) {
                    self.g_unlock_pending[g] = true;
                } else {
                    self.hlock = 0;
                },
                // The registry's filter cell: 0 ⇒ served, no lock.
                3 => if (self.cell == 0) {
                    self.served[g] = true;
                    self.pc[t] = 8;
                    return;
                },
                // A lock's acquiring RMW drains the thread's own stores.
                4 => {
                    self.drainG(g);
                    if (self.rlock != 0) return;
                    self.rlock = @intCast(t + 1);
                },
                5 => {
                    if (self.entry) self.gone[g] = true else self.served[g] = true;
                },
                6 => self.rlock = 0,
                // A gone grant retracts its registration under the holder's
                // lock (the last one out lowers the watch cell).
                7 => if (self.gone[g]) {
                    self.drainG(g);
                    if (self.hlock != 0) return;
                    self.reg &= ~bit;
                    if (self.reg == 0) self.watch -= 1;
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        pub fn invariant(_: *const Self) !void {}

        pub fn final(self: *const Self) !void {
            for (self.served) |s| if (s and !self.d_saw_watch) return error.ServedAndNoRecall;
        }
    };
}

/// A drain the scheduler may run at any moment: each store buffer's own
/// flush, independent of the threads' program order.
fn WithEagerDrains(comptime M: type) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 6;
        m: M = .{},
        drained: [3]bool = @splat(false),

        pub fn init() Self {
            return .{ .m = M.init() };
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t < 3) self.m.done(t) else self.drained[t - 3];
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                0, 1, 2 => self.m.step(t),
                3 => {
                    self.m.drainD();
                    self.drained[0] = true;
                },
                4, 5 => {
                    self.m.drainG(t - 4);
                    self.drained[t - 3] = true;
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

test "the cross-member delete's handshake (decision 0003 §DH): a grant that serves a page of a file its holder deletes is one the delete saw — the delete raises the registry's filter cell THEN reads the holder's registration filter, the grant registers THEN reads the cell, each pair SeqCst; a second member's grant finding the object registered rides the first registration; another deleted ino in the cell sends the grant to the registry's lock; either raise left store-buffered serves a page no recall reaches" {
    try explorer.expectHolds(WithEagerDrains(DeleteWatch(.strict, false)), .{});
    try explorer.expectHolds(WithEagerDrains(DeleteWatch(.strict, true)), .{});
    try explorer.expectFails(WithEagerDrains(DeleteWatch(.no_delete_fence, false)), .{}, error.FinalViolated);
    try explorer.expectFails(WithEagerDrains(DeleteWatch(.no_grant_fence, false)), .{}, error.FinalViolated);
}
