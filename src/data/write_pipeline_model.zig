//! The write-pipeline models (charter §9; `docs/porting/data_write_path.md`
//! §9.1, §9.3): the admission core's target law with its single empty-pipe
//! bypass, the gauges settling to zero, and the probe epoch roll's single
//! winner. Each with the weakening that fails it.
//!
//! Weakenings (recorded):
//! * `blocks_counter_bypass` — the empty-pipe predicate reads the BLOCKS
//!   counter (`blocks == 0`) instead of the bytes word the CAS charges; the
//!   blocks counter lags the CAS, so two racing oversized bypassers are both
//!   admitted (`InvariantViolated`).
//! * `check_then_store_roll` — the epoch roll loads `epoch_start`, compares,
//!   then stores instead of CAS'ing; two rollers both roll and `ups == 2`
//!   (`FinalViolated`).

const std = @import("std");
const explorer = @import("../model/explorer.zig");

pub const AdmitWeakening = enum { none, blocks_counter_bypass };

/// Three admitters over a target of 10: two oversized (15 each — only the
/// empty-pipe bypass can admit one) and one small (6). Every admitter
/// releases after admission, so the final gauges must read zero.
pub fn AdmissionModel(comptime weakening: AdmitWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 3;
        const target: u32 = 10;
        const sizes = [thread_count]u32{ 15, 15, 6 };

        bytes: u32 = 0,
        blocks: u32 = 0,
        pc: [thread_count]u8 = @splat(0),
        cur: [thread_count]u32 = @splat(0),
        admitted: [thread_count]bool = @splat(false),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 4;
        }

        pub fn step(self: *Self, t: usize) void {
            const n = sizes[t];
            switch (self.pc[t]) {
                0 => {
                    // load cur (bytes).
                    self.cur[t] = self.bytes;
                    self.pc[t] = 1;
                },
                1 => {
                    // The bypass judgement, then the CAS as ONE atomic action.
                    const empty = switch (weakening) {
                        .none => self.cur[t] == 0,
                        .blocks_counter_bypass => self.blocks == 0,
                    };
                    if (!empty and self.cur[t] + n > target) {
                        // Full: retry later (re-load) — never admitted while full.
                        self.pc[t] = 0;
                        return;
                    }
                    if (self.bytes == self.cur[t]) {
                        self.bytes += n;
                        self.admitted[t] = true;
                        self.pc[t] = 2;
                    } else {
                        self.pc[t] = 0; // raced
                    }
                },
                2 => {
                    // blocks += 1 (a separate RMW — the lag the weakening exploits).
                    self.blocks += 1;
                    self.pc[t] = 3;
                },
                3 => {
                    // release: bytes −= n; blocks −= 1.
                    self.bytes -= n;
                    self.blocks -= 1;
                    self.admitted[t] = false;
                    self.pc[t] = 4;
                },
                else => unreachable,
            }
        }

        /// Over the target only when exactly ONE permit is in flight (the
        /// single bypass).
        pub fn invariant(self: *const Self) !void {
            var in_flight: u32 = 0;
            for (self.admitted) |a| in_flight += @intFromBool(a);
            if (self.bytes > target and in_flight != 1) return error.TargetExceededByTwoPermits;
        }

        pub fn final(self: *const Self) !void {
            if (self.bytes != 0 or self.blocks != 0) return error.GaugesDidNotSettle;
        }
    };
}

pub const RollWeakening = enum { none, check_then_store_roll };

/// Two rollers see an elapsed epoch; exactly one may roll (one `ups`).
pub fn ProbeRollModel(comptime weakening: RollWeakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = 2;
        const epoch_start_old: u32 = 1;
        const now: u32 = 2;

        epoch_start: u32 = epoch_start_old,
        ups: u8 = 0,
        mul: u32 = 64,
        pc: [2]u8 = @splat(0),
        seen: [2]u32 = @splat(0),

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.pc[t] == 3;
        }

        pub fn step(self: *Self, t: usize) void {
            switch (self.pc[t]) {
                0 => {
                    self.seen[t] = self.epoch_start;
                    self.pc[t] = 1;
                },
                1 => {
                    if (self.seen[t] != epoch_start_old) {
                        // Someone rolled: not an elapsed epoch for us.
                        self.pc[t] = 3;
                        return;
                    }
                    switch (weakening) {
                        .none => {
                            // CAS(old → now): single winner.
                            if (self.epoch_start == self.seen[t]) {
                                self.epoch_start = now;
                                self.pc[t] = 2;
                            } else {
                                self.pc[t] = 3;
                            }
                        },
                        .check_then_store_roll => {
                            // A plain store after the check: both win.
                            self.epoch_start = now;
                            self.pc[t] = 2;
                        },
                    }
                },
                2 => {
                    // The winner's roll: launch.
                    self.ups += 1;
                    self.mul += self.mul / 4;
                    self.pc[t] = 3;
                },
                else => unreachable,
            }
        }

        pub fn invariant(_: *const Self) !void {}

        pub fn final(self: *const Self) !void {
            if (self.ups != 1) return error.RollNotSingleWinner;
            if (self.mul != 64 + 64 / 4) return error.MulNotOneStep;
        }
    };
}

test "write_pipeline_admission_never_exceeds_target + empty_pipe_bypass_is_single + gauges_settle_to_zero — hold; the blocks-counter bypass admits two oversized bypassers" {
    try explorer.expectHolds(AdmissionModel(.none), .{});
    try explorer.expectFails(AdmissionModel(.blocks_counter_bypass), .{}, error.InvariantViolated);
}

test "write_pipeline_probe_epoch_roll_is_single_winner — holds; a check-then-store roll lets both roll (ups == 2)" {
    try explorer.expectHolds(ProbeRollModel(.none), .{});
    try explorer.expectFails(ProbeRollModel(.check_then_store_roll), .{}, error.FinalViolated);
}

test "state counts: small enough for every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(AdmissionModel(.none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states > 20 and report.states < 5000);
}
