//! Explorer model of the fiber-aware futex (`futex_table.zig` §3.5): one
//! fiber waiter (parks in a bucket list under the bucket lock), one raw
//! thread waiter (counts itself in `raw_waiters`, then the kernel's
//! compare-and-sleep), one waker (stores the word, wakes the bucket's
//! fibers, then `fence; load(raw_waiters)` decides whether the kernel wake
//! is paid).
//!
//! The Dekker pair: the raw waiter's increment precedes its kernel compare;
//! the waker's word store precedes its count read; both RMW the count, so
//! either the waker sees the count or the raw waiter's kernel compare sees
//! the new word (EAGAIN).
//!
//! Weakening ledger:
//!   `.count_after_wait` — the raw waiter increments the count AFTER the
//!     kernel wait (the increment step follows the sleep): the waker reads
//!     0, pays no kernel wake, and the raw waiter sleeps forever.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Weakening = struct {
    count_after_wait: bool = false,
};

fn FutexModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// 0 = the fiber waiter, 1 = the raw waiter, 2 = the waker.
        pub const thread_count: usize = 3;
        const fiber = 0;
        const raw = 1;
        const waker = 2;

        word: u8 = 0,
        bucket_locked: bool = false,
        fiber_linked: bool = false,
        raw_count: u8 = 0,
        fiber_sleeping: bool = false,
        fiber_woken: bool = false,
        raw_sleeping: bool = false,
        raw_woken: bool = false,
        fpc: u8 = 0,
        rpc: u8 = 0,
        wpc: u8 = 0,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return switch (t) {
                fiber => self.fpc == 9,
                raw => self.rpc == 9,
                waker => self.wpc == 9,
                else => unreachable,
            };
        }
        pub fn step(self: *Self, t: usize) void {
            switch (t) {
                fiber => switch (self.fpc) {
                    0 => if (!self.bucket_locked) {
                        self.bucket_locked = true;
                        self.fpc = 1;
                    },
                    // Under the lock: re-read the word, else link.
                    1 => {
                        if (self.word != 0) {
                            self.fpc = 8;
                        } else {
                            self.fiber_linked = true;
                            self.fpc = 2;
                        }
                    },
                    2 => {
                        self.bucket_locked = false;
                        self.fpc = 3;
                    },
                    // park: the delivery word absorbs a wake that already landed
                    3 => if (self.fiber_woken) {
                        self.fpc = 9;
                    } else {
                        self.fiber_sleeping = true;
                    },
                    // the word changed under the lock: return at once
                    8 => {
                        self.bucket_locked = false;
                        self.fpc = 9;
                    },
                    else => unreachable,
                },
                raw => switch (self.rpc) {
                    0 => {
                        if (weakening.count_after_wait) {
                            self.rpc = 1;
                        } else {
                            self.raw_count += 1;
                            self.rpc = 1;
                        }
                    },
                    // the kernel's compare-and-sleep (one atomic step); a
                    // sleeper leaves only through a kernel wake
                    1 => {
                        if (self.raw_sleeping) {
                            if (self.raw_woken) {
                                self.raw_sleeping = false;
                                self.rpc = 2;
                            }
                        } else if (self.word != 0) {
                            // EAGAIN: the word already changed
                            self.rpc = 2;
                        } else {
                            self.raw_sleeping = true;
                        }
                    },
                    // after the wait: the decrement (WEAKENED: the increment
                    // only lands here, after the wait returned — so a sleeper
                    // was never counted)
                    2 => {
                        if (weakening.count_after_wait) self.raw_count += 1;
                        self.raw_count -= 1;
                        self.rpc = 9;
                    },
                    else => unreachable,
                },
                waker => switch (self.wpc) {
                    // the primitive's store (the token)
                    0 => {
                        self.word = 1;
                        self.wpc = 1;
                    },
                    1 => if (!self.bucket_locked) {
                        self.bucket_locked = true;
                        self.wpc = 2;
                    },
                    // move the matching fibers off the list (their wake)
                    2 => {
                        if (self.fiber_linked) {
                            self.fiber_linked = false;
                            self.fiber_woken = true;
                        }
                        self.bucket_locked = false;
                        self.wpc = 3;
                    },
                    // fence + load(raw_waiters): pay the kernel wake iff counted
                    3 => {
                        if (self.raw_count != 0) self.raw_woken = true;
                        self.wpc = 9;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // The waker is gone: a sleeper nobody woke sleeps forever.
            if (self.wpc == 9) {
                if (self.fiber_sleeping and !self.fiber_woken) return error.FiberSleepsForever;
                if (self.raw_sleeping and !self.raw_woken) return error.RawWaiterSleepsForever;
            }
        }
        pub fn final(self: *const Self) !void {
            if (self.fpc != 9 or self.rpc != 9) return error.WaiterNeverReturned;
        }
    };
}

const testing = std.testing;

test "the fiber/raw futex Dekker holds; counting the raw waiter after its kernel wait sleeps it forever" {
    try explorer.expectHolds(FutexModel(.{}), .{});
    try explorer.expectFails(FutexModel(.{ .count_after_wait = true }), .{}, error.InvariantViolated);
}

test "futex table model: exhaustive within the explorer's default limits" {
    var report: explorer.Report = undefined;
    try explorer.explore(FutexModel(.{}), testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    try testing.expect(report.states < 500);
}
