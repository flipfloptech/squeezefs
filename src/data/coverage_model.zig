//! The coverage-union model (charter §9 "model-check every lock-free core";
//! `docs/porting/data_write_path.md` §5.2 RW3b / FIND-L1-A): N segment
//! writers record their fixed ranges in EVERY arrival order (the explorer
//! enumerates the orders), and after every step the union must agree with a
//! page-granular oracle of what has landed, fire `completed` exactly once and
//! only when the whole block is covered. The union itself is mutated under
//! the block lock, so the model is the law of its VERDICTS over arrival
//! order — the property the −34 % misfire broke.
//!
//! Weakening (recorded): `end_offset_proxy` — the retired trigger "the
//! segment's end is the block end" instead of "the union is full". A
//! tail-first arrival fires on partial coverage (`InvariantViolated`), the
//! reference's RW3b finding.

const std = @import("std");
const explorer = @import("../model/explorer.zig");
const cov = @import("write_coverage.zig");

pub const Weakening = enum { none, end_offset_proxy };

const PAGE: u32 = 4096;

pub fn TriggerModel(comptime segs: []const cov.Run, comptime len: u32, comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        pub const thread_count: usize = segs.len;
        const pages = len / PAGE;

        u: cov.CoverageUnion = .empty,
        done_flags: [segs.len]bool = @splat(false),
        fired: u8 = 0,

        pub fn init() Self {
            return .{};
        }

        pub fn done(self: *const Self, t: usize) bool {
            return self.done_flags[t];
        }

        pub fn step(self: *Self, t: usize) void {
            const s = segs[t];
            const v = self.u.record(s.start, s.end, len) catch unreachable;
            const trigger = switch (weakening) {
                .none => v.completed,
                .end_offset_proxy => s.end == len,
            };
            if (trigger) self.fired += 1;
            self.done_flags[t] = true;
        }

        /// Exactness against the oracle; the trigger law.
        pub fn invariant(self: *const Self) !void {
            var p: u32 = 0;
            while (p < pages) : (p += 1) {
                var landed = false;
                for (segs, 0..) |s, i| {
                    if (self.done_flags[i] and s.start <= p * PAGE and (p + 1) * PAGE <= s.end) landed = true;
                }
                if (self.u.contains(p * PAGE, (p + 1) * PAGE) != landed) return error.UnionDisagreesWithOracle;
            }
            if (self.fired > 1) return error.CompletedFiredTwice;
            if (self.fired == 1 and !self.u.isFull(len)) return error.FiredOnPartialCoverage;
        }

        pub fn final(self: *const Self) !void {
            if (!self.u.isFull(len)) return error.NeverCompleted;
            if (self.fired != 1) return error.CompletedNotExactlyOnce;
            if (self.u.n_extras != 0) return error.ExtrasLeftAfterCompletion;
        }

        /// The union's bytes carry an undefined tail past `n_extras`; hash
        /// only the defined state.
        pub fn key(self: *const Self) u64 {
            var h = std.hash.XxHash3.init(0);
            h.update(std.mem.asBytes(&self.u.primary));
            h.update(std.mem.sliceAsBytes(self.u.extras[0..self.u.n_extras]));
            h.update(std.mem.asBytes(&self.done_flags));
            h.update(std.mem.asBytes(&self.fired));
            return h.final();
        }
    };
}

const BS: u32 = 8 * PAGE;

/// Three disjoint thirds (the kernel-split cohort).
const thirds = [_]cov.Run{
    .{ .start = 0, .end = 2 * PAGE },
    .{ .start = 2 * PAGE, .end = 5 * PAGE },
    .{ .start = 5 * PAGE, .end = BS },
};

/// Four overlapping segments (rewrites inside the window).
const overlapping = [_]cov.Run{
    .{ .start = 0, .end = 3 * PAGE },
    .{ .start = 2 * PAGE, .end = 4 * PAGE },
    .{ .start = 6 * PAGE, .end = BS },
    .{ .start = 3 * PAGE, .end = 7 * PAGE },
};

test "coverage trigger model: three disjoint segments in every order complete exactly once at full union; the end-offset proxy fires on partial coverage" {
    try explorer.expectHolds(TriggerModel(&thirds, BS, .none), .{});
    try explorer.expectFails(TriggerModel(&thirds, BS, .end_offset_proxy), .{}, error.InvariantViolated);
}

test "coverage trigger model: overlapping segments in every order stay exact against the page oracle" {
    try explorer.expectHolds(TriggerModel(&overlapping, BS, .none), .{});
    try explorer.expectFails(TriggerModel(&overlapping, BS, .end_offset_proxy), .{}, error.InvariantViolated);
}

test "state counts: small enough for every gate" {
    var report: explorer.Report = undefined;
    try explorer.explore(TriggerModel(&overlapping, BS, .none), std.testing.allocator, .{}, &report);
    try std.testing.expect(report.states >= 16 and report.states < 200);
}
