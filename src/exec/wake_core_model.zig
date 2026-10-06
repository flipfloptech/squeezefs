//! Explorer model of the wake coalescer pair the lane parks on
//! (`nvme_derive.WakeCoalescer` + `ring_park.ParkedRing.service`, used
//! unchanged by `lane.zig`): producers publish → `arm` → eventfd write iff
//! arm returned true; the lane reaps the wake CQE → `service` DISARMS → scans
//! the queue. The charter's "drain → disarm → scan" order (§4 worker pass).
//!
//! Two producers race one lane. The lane's eventfd is a counter the kernel
//! keeps; a standing READ completes when it is non-zero.
//!
//! Weakening ledger:
//!   `.disarm_after_scan` — the lane scans the queue and disarms AFTERWARDS:
//!     a push that lands between the scan and the disarm finds the coalescer
//!     still armed, writes nothing, and the lane parks on a non-empty queue
//!     until its tick — the lost push.

const std = @import("std");
const explorer = @import("../model/explorer.zig");

const Weakening = struct {
    disarm_after_scan: bool = false,
};

fn WakeModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// Threads 0 and 1 produce; 2 is the lane.
        pub const thread_count: usize = 3;
        const lane = 2;

        /// Pushed, not yet scanned.
        queue: u8 = 0,
        armed: bool = false,
        /// The eventfd counter (pending wake writes).
        efd: u8 = 0,
        /// A wake CQE reaped and not yet serviced.
        consumed: bool = false,
        processed: u8 = 0,
        ppc: [2]u8 = @splat(0),
        /// 3 = parked at the loop bottom (the initial state: an idle lane).
        lpc: u8 = 3,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == lane) self.lpc == 9 else self.ppc[t] == 9;
        }
        fn producersDone(self: *const Self) bool {
            return self.ppc[0] == 9 and self.ppc[1] == 9;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == lane) return self.laneStep();
            switch (self.ppc[t]) {
                // publish
                0 => {
                    self.queue += 1;
                    self.ppc[t] = 1;
                },
                // arm: swap(true) — write iff it was unarmed
                1 => {
                    const was = self.armed;
                    self.armed = true;
                    self.ppc[t] = if (was) 9 else 2;
                },
                // the eventfd write
                2 => {
                    self.efd += 1;
                    self.ppc[t] = 9;
                },
                else => unreachable,
            }
        }
        fn laneStep(self: *Self) void {
            switch (self.lpc) {
                // reap: the standing READ completed (the counter is reset by the read)
                0 => {
                    if (self.efd > 0) {
                        self.efd = 0;
                        self.consumed = true;
                    }
                    self.lpc = 1;
                },
                // service: DISARM (the shipped order)
                1 => {
                    if (!weakening.disarm_after_scan and self.consumed) {
                        self.armed = false;
                        self.consumed = false;
                    }
                    self.lpc = 2;
                },
                // scan the queue
                2 => {
                    self.processed += self.queue;
                    self.queue = 0;
                    self.lpc = if (weakening.disarm_after_scan) 4 else 3;
                },
                // WEAKENED: disarm after the scan
                4 => {
                    if (self.consumed) {
                        self.armed = false;
                        self.consumed = false;
                    }
                    self.lpc = 3;
                },
                // park: wake on the eventfd, exit when everything is drained,
                // otherwise stay parked (a no-op step the explorer prunes)
                3 => {
                    if (self.efd > 0) {
                        self.lpc = 0;
                    } else if (self.producersDone() and self.queue == 0) {
                        self.lpc = 9;
                    }
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // Parked with work queued, no wake pending, and nobody left to write one.
            if (self.lpc == 3 and self.queue > 0 and self.efd == 0 and self.producersDone()) return error.LostPush;
        }
        pub fn final(self: *const Self) !void {
            if (self.processed != 2) return error.PushesNotProcessed;
            if (self.lpc != 9) return error.LaneNeverIdled;
        }
    };
}

const testing = std.testing;

test "the coalescer pair holds in drain → disarm → scan order; disarming after the scan loses a push" {
    try explorer.expectHolds(WakeModel(.{}), .{});
    try explorer.expectFails(WakeModel(.{ .disarm_after_scan = true }), .{}, error.InvariantViolated);
}

test "wake core model: exhaustive within the explorer's default limits" {
    var report: explorer.Report = undefined;
    try explorer.explore(WakeModel(.{}), testing.allocator, .{}, &report);
    try testing.expect(report.states > 1);
    try testing.expect(report.states < 500);
}
