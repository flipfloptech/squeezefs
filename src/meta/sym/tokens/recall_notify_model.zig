//! The recall / kernel-notify / in-flight-op dependency graph as an
//! exhaustive model (decision 0003 §AB.2, §AG.2; libfuse's documented
//! hazard: `fuse_lowlevel_notify_inval_entry` for the parent of a request
//! in flight deadlocks, because the kernel's `fuse_reverse_inval_entry`
//! takes the parent's `i_rwsem`, which `lookup_open` / mkdir / unlink /
//! rename hold EXCLUSIVELY across the request).
//!
//! Three actors. The MANAGER serves a cross-owner step: it recalls the
//! parent's token from every holder — the requester among them — waits
//! on the ack, then applies and replies. The requester's OP holds the
//! parent's directory lock in its kernel, ships the step and waits on the
//! reply. The requester's SIDEBAND takes the pushed recall: it queues the
//! changed name's `INVAL_ENTRY`, acks, and its device write parks until
//! the op's directory lock drops.
//!
//! The law (`token_cache.handleRecall` + `session.notifyEntryOrEpoch`): a
//! recall ack NEVER waits on a kernel-view invalidation — the names ride
//! the sideband fire-and-forget and the push's reply is the ack. The
//! weakening is the ack AFTER the notify's write completes (libfuse's
//! synchronous notify posture): op → recall → notify → the op's directory
//! lock → op closes into the cycle the explorer finds.

const std = @import("std");
const explorer = @import("../../../model/explorer.zig");

fn RecallNotifyCycle(comptime ack_after_notify: bool) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const manager = 0;
        const op = 1;
        const sideband = 2;

        /// The requester's kernel holds the parent's `i_rwsem` for the op.
        dir_locked: bool = false,
        step_shipped: bool = false,
        recall_pushed: bool = false,
        notify_queued: bool = false,
        /// The device write of the `INVAL_ENTRY` returned.
        notify_written: bool = false,
        acked: bool = false,
        step_replied: bool = false,
        pc: [3]u8 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 3;
        }

        /// Can thread `t` take its next step now (its wait condition holds)?
        fn ready(self: *const Self, t: usize) bool {
            if (self.done(t)) return false;
            return switch (t) {
                manager => switch (self.pc[t]) {
                    0 => self.step_shipped,
                    1 => self.acked,
                    else => true,
                },
                op => switch (self.pc[t]) {
                    1 => self.step_replied,
                    else => true,
                },
                sideband => switch (self.pc[t]) {
                    0 => self.recall_pushed,
                    1 => if (ack_after_notify) !self.dir_locked else true,
                    2 => if (ack_after_notify) true else !self.dir_locked,
                    else => true,
                },
                else => unreachable,
            };
        }

        pub fn step(self: *Self, t: usize) void {
            if (!self.ready(t)) return;
            switch (t) {
                manager => switch (self.pc[t]) {
                    0 => self.recall_pushed = true,
                    1 => {},
                    2 => self.step_replied = true,
                    else => unreachable,
                },
                op => switch (self.pc[t]) {
                    0 => {
                        self.dir_locked = true;
                        self.step_shipped = true;
                    },
                    1 => {},
                    2 => self.dir_locked = false,
                    else => unreachable,
                },
                sideband => switch (self.pc[t]) {
                    0 => self.notify_queued = true,
                    1 => if (ack_after_notify) {
                        self.notify_written = true;
                    } else {
                        self.acked = true;
                    },
                    2 => if (ack_after_notify) {
                        self.acked = true;
                    } else {
                        self.notify_written = true;
                    },
                    else => unreachable,
                },
                else => unreachable,
            }
            self.pc[t] += 1;
        }

        /// Deadlock: nobody is done-free and nobody can step.
        pub fn invariant(self: *const Self) !void {
            var any_live = false;
            for (0..thread_count) |t| {
                if (self.done(t)) continue;
                any_live = true;
                if (self.ready(t)) return;
            }
            if (any_live) return error.Deadlock;
        }

        pub fn final(self: *const Self) !void {
            if (!(self.step_replied and self.notify_written and self.acked and !self.dir_locked)) return error.Incomplete;
        }
    };
}

test "the recall/notify/op graph: with the ack at the push the cycle is unreachable; with the ack after the notify's write the three actors deadlock behind the op's directory lock" {
    try explorer.expectHolds(RecallNotifyCycle(false), .{});
    try explorer.expectFails(RecallNotifyCycle(true), .{}, error.InvariantViolated);
}
