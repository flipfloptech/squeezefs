//! The dynamic-routing instruments (charter §5.4, §10; `docs/porting/
//! dynamic_routing.md` §5.2): the gate, the migration engine, the stamp's
//! encoding-budget pressure pair, the cross-volume commit classes and the
//! exact-sum cutover phase family. ONE namespace every routing module
//! writes into (the sub-modules are `stats.covered` by this path), rendered
//! under the `.stats` `meta` family. PORTABLE.

const std = @import("std");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");

const assert = std.debug.assert;

/// The cutover window's phases (`migrate_cutover_phase_ns`, exact-sum:
/// `Σ legs ≡ total` to the nanosecond): the drain of admitted ops, the
/// final delta round, the three durable writes (0: the target's
/// checkpoint; 1: the target's claim; 2: the source's release) and the RAM
/// swap. The port's parked Δtime refinements drain as ordinary gated ops
/// routed at commit time, so the reference's "times drain" leg has no
/// counterpart (`routing/migrate.zig`).
pub const Phase = enum(u8) {
    drain,
    final_delta,
    write0,
    write1,
    write2,
    swap,
    total,

    pub const count = @typeInfo(Phase).@"enum".field_names.len;
    pub const legs = count - 1;
};

pub const stats = struct {
    /// `routing_gate_parks`: one per park EVENT (an op that found its slot's
    /// gate closed and waited). Planned and bounded — never an escalation.
    pub var routing_gate_parks: counters.ShardedCounter = .{};
    /// `routing_gate_armed`: 0/1 — the set's fast-path word (a gate is armed
    /// for exactly one slot while a migration runs).
    pub var routing_gate_armed: counters.Gauge = .{};
    /// `routing_stamp_runs_max` / `routing_stamp_cursors_max`: the
    /// encoding-budget pressure pair — approaching `STAMP_MAX_RUNS` /
    /// `STAMP_MAX_CURSORS` means consolidate before a preflight refuses.
    pub var routing_stamp_runs_max: counters.Gauge = .{};
    pub var routing_stamp_cursors_max: counters.Gauge = .{};
    /// `routing_dual_claims_converged`: slots a writable open found claimed
    /// by two stamps and converged (the loser's idempotent release written
    /// before serving — §7 Q1's remedy).
    pub var routing_dual_claims_converged: counters.ShardedCounter = .{};
    /// Two stamps claim one slot at the SAME epoch — impossible by
    /// construction (one coordinator, one flip per slot): corruption.
    pub var routing_same_epoch_dual_claim: counters.Tripwire = .init("routing_same_epoch_dual_claim");
    /// `routing_mint_volume_picks`: mints routed to a volume other than the
    /// parent's (the health-weighted spread's engagement).
    pub var routing_mint_volume_picks: counters.ShardedCounter = .{};
    /// `routing_guest_trees_minted`: slot trees created on first touch.
    pub var routing_guest_trees_minted: counters.ShardedCounter = .{};
    /// `routing_split_commits`: transactions whose records lived on two
    /// volumes and committed as two ordered entries; `_partial`: the second
    /// entry refused after the first landed (a bounded fsck class, never a
    /// silent one).
    pub var routing_split_commits: counters.ShardedCounter = .{};
    pub var routing_split_partial: counters.ShardedCounter = .{};
    /// `routing_cross_volume_refused`: a transaction spanning volumes with
    /// no legal split order answered EXDEV (rename across two parents'
    /// volumes — the symmetric plane's cross-owner intents are rung 6's).
    pub var routing_cross_volume_refused: counters.ShardedCounter = .{};
    /// The engine: completed flips (flip + teardown), aborts (3 consecutive
    /// overflows / 16 missed windows), records copied (bulk + delta puts),
    /// teed keys applied, side-log overflows (each ⇒ a fresh snapshot pass),
    /// cutover windows that missed the deadline or found late commits.
    pub var migrate_slots_completed: counters.ShardedCounter = .{};
    pub var migrate_slots_aborted: counters.ShardedCounter = .{};
    pub var migrate_records_copied: counters.ShardedCounter = .{};
    pub var migrate_delta_keys: counters.ShardedCounter = .{};
    pub var migrate_delta_overflows: counters.ShardedCounter = .{};
    pub var migrate_cutover_retries: counters.ShardedCounter = .{};
    pub var migrate_cutover_phase_ns: [Phase.count]counters.ShardedLatencyHistogram = @splat(.{});
    /// `staging_rebind_barriers`: KD-8 prepare + finalize pairs run by the
    /// offline set verbs.
    pub var staging_rebind_barriers: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "routing_gate_parks", "routing_dual_claims_converged", "routing_mint_volume_picks", "routing_guest_trees_minted", "routing_split_commits", "routing_split_partial", "routing_cross_volume_refused", "migrate_slots_completed", "migrate_slots_aborted", "migrate_records_copied", "migrate_delta_keys", "migrate_delta_overflows", "migrate_cutover_retries", "staging_rebind_barriers" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "routing_gate_armed", "routing_stamp_runs_max", "routing_stamp_cursors_max" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"routing_same_epoch_dual_claim\":{d},", .{counters.tripwireCount("routing_same_epoch_dual_claim")});
    try w.writeAll("\"migrate_cutover_phase_ns\":{");
    inline for (@typeInfo(Phase).@"enum".field_names, 0..) |name, i| {
        if (i != 0) try w.writeByte(',');
        try w.print("\"{s}\":", .{name});
        try stats.migrate_cutover_phase_ns[i].snapshot().writeJson(w);
    }
    try w.writeAll("}}");
}

/// The cutover window's profile on the engine's frame: `mark(p)` records
/// the span since the previous boundary; `finish` records 0 ns for every
/// unmarked leg and `total = last − t0`, so `Σ legs ≡ total` on every
/// exit (an aborted window included) with equal counts per phase.
pub const Prof = struct {
    t0: u64,
    last: u64,
    legs: [Phase.legs]u64 = @splat(0),

    pub fn start() Prof {
        return startAt(clock.monotonicNs());
    }

    pub fn startAt(now: u64) Prof {
        return .{ .t0 = now, .last = now };
    }

    pub fn mark(self: *Prof, p: Phase) void {
        self.markAt(p, clock.monotonicNs());
    }

    pub fn markAt(self: *Prof, p: Phase, now: u64) void {
        assert(p != .total);
        const t = @max(now, self.last);
        self.legs[@backingInt(p)] += t - self.last;
        self.last = t;
    }

    pub fn finish(self: *Prof) void {
        for (self.legs, 0..) |ns, i| stats.migrate_cutover_phase_ns[i].record(ns);
        stats.migrate_cutover_phase_ns[@backingInt(Phase.total)].record(self.last - self.t0);
    }

    pub fn total(self: *const Prof) u64 {
        return self.last - self.t0;
    }
};

const testing = std.testing;

test "the cutover phase family is exact-sum: Σ legs ≡ total on a full window and on an early abort; the JSON names every leg" {
    var p = Prof.startAt(1000);
    p.markAt(.drain, 1100);
    p.markAt(.final_delta, 1400);
    p.markAt(.write0, 1900);
    p.markAt(.write1, 2000);
    p.markAt(.write2, 2100);
    p.markAt(.swap, 2101);
    var sum: u64 = 0;
    for (p.legs) |l| sum += l;
    try testing.expectEqual(p.total(), sum);
    try testing.expectEqual(@as(u64, 1101), p.total());
    const before = stats.migrate_cutover_phase_ns[@backingInt(Phase.total)].snapshot().count;
    p.finish();
    // An abort after the drain: the unmarked legs record 0 and the counts
    // stay equal across phases.
    var a = Prof.startAt(5000);
    a.markAt(.drain, 5300);
    a.finish();
    const after = stats.migrate_cutover_phase_ns[@backingInt(Phase.total)].snapshot();
    try testing.expectEqual(before + 2, after.count);
    for (stats.migrate_cutover_phase_ns[0..Phase.legs]) |*h| try testing.expectEqual(after.count, h.snapshot().count);
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"migrate_cutover_phase_ns\":{\"drain\":") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"routing_gate_armed\":") != null);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "\"routing_same_epoch_dual_claim\":") != null);
}
