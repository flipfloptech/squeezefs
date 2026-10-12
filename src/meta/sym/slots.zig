//! Symmetric metadata, part B1 — SLOT LEASES (charter §5.5 "Slot
//! acquisition and minting", "Handover", "Slot → holder resolution", the
//! commit door and the structural door of "Extent grants", the page-budget
//! overflow law; `docs/porting/sym_slot_leases.md`): the dense per-slot
//! lease table with its model (`lease_table`, `slot_door_model`), the ONE
//! commit door that rung 5's cutover gate folded into (`door`),
//! acquisition and minting with parent affinity and the manager's five
//! slot verbs (`acquire`), holder-decided handover — the dominance window,
//! the offer rule, flush-then-transfer (`handover`), the page-budget law
//! (`page_budget`) and slot → holder resolution (`resolve`). Portable
//! throughout; the plane bound to a backend's regions, trees and
//! checkpoint hooks is `sym/bind.zig`'s (Linux).

const std = @import("std");
const counters = @import("../../core/counters.zig");

pub const lease_table = @import("slots/lease_table.zig");
pub const slot_door_model = @import("slots/slot_door_model.zig");
pub const door = @import("slots/door.zig");
pub const acquire = @import("slots/acquire.zig");
pub const handover = @import("slots/handover.zig");
pub const page_budget = @import("slots/page_budget.zig");
pub const resolve = @import("slots/resolve.zig");
pub const tails_spill = @import("slots/tails_spill.zig");

pub const LeaseTable = lease_table.LeaseTable;
pub const Door = door.Door;
pub const Tokens = door.Tokens;
pub const MAX_SLOTS = door.MAX_SLOTS;

/// Process-wide instruments of the plane (the per-volume words —
/// `slot_leases_held`, the per-slot tree gauges — ride the backend's
/// `AppenderSet.writeStatsJson`).
pub const stats = struct {
    /// Leases this process holds across its volumes.
    pub var slot_leases_held: counters.Gauge = .{};
    /// Cursor-derived per held slot: the minted population (an upper bound
    /// on the live one).
    pub var slot_tree_inos_p50: counters.Gauge = .{};
    pub var slot_tree_inos_p99: counters.Gauge = .{};
    pub var slot_tree_inos_max: counters.Gauge = .{};
    /// `extents × node_size` per held slot.
    pub var slot_tree_bytes_p99: counters.Gauge = .{};
    pub var slot_tree_bytes_max: counters.Gauge = .{};
};

/// Fold the per-held-slot census into the gauges: inos (cursor − 2) and
/// bytes, p50/p99/max — a census per stats read is not a price a gauge
/// pays, so the plane folds it at the cadence tick.
pub fn publishTreeGauges(inos_sorted: []const u64, bytes_sorted: []const u64) void {
    stats.slot_tree_inos_p50.set(percentile(inos_sorted, 50));
    stats.slot_tree_inos_p99.set(percentile(inos_sorted, 99));
    stats.slot_tree_inos_max.set(if (inos_sorted.len == 0) 0 else inos_sorted[inos_sorted.len - 1]);
    stats.slot_tree_bytes_p99.set(percentile(bytes_sorted, 99));
    stats.slot_tree_bytes_max.set(if (bytes_sorted.len == 0) 0 else bytes_sorted[bytes_sorted.len - 1]);
}

fn percentile(sorted: []const u64, p: u64) u64 {
    if (sorted.len == 0) return 0;
    const idx = (sorted.len - 1) * p / 100;
    return sorted[idx];
}

/// The `.stats` `meta.sym.slots` family: every namespace of the plane.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{\"leases\":");
    try counters.writeNamespaceJson(stats, w);
    try w.writeAll(",\"door\":");
    try counters.writeNamespaceJson(door.stats, w);
    try w.writeAll(",\"acquire\":");
    try counters.writeNamespaceJson(acquire.stats, w);
    try w.writeAll(",\"handover\":");
    try counters.writeNamespaceJson(handover.stats, w);
    try w.writeAll(",\"page_budget\":");
    try counters.writeNamespaceJson(page_budget.stats, w);
    try w.writeAll(",\"resolve\":");
    try counters.writeNamespaceJson(resolve.stats, w);
    try w.writeAll(",\"tails_spill\":");
    try counters.writeNamespaceJson(tails_spill.stats, w);
    try w.writeByte('}');
}

test {
    _ = lease_table;
    _ = slot_door_model;
    _ = door;
    _ = acquire;
    _ = handover;
    _ = page_budget;
    _ = resolve;
    _ = tails_spill;
}

test "the tree gauges fold a sorted census: p50/p99/max of inos and bytes; empty reads 0" {
    publishTreeGauges(&.{}, &.{});
    try std.testing.expectEqual(@as(u64, 0), stats.slot_tree_inos_max.load());
    publishTreeGauges(&.{ 1, 2, 3, 4, 100 }, &.{ 10, 20 });
    try std.testing.expectEqual(@as(u64, 3), stats.slot_tree_inos_p50.load());
    try std.testing.expectEqual(@as(u64, 4), stats.slot_tree_inos_p99.load());
    try std.testing.expectEqual(@as(u64, 100), stats.slot_tree_inos_max.load());
    try std.testing.expectEqual(@as(u64, 10), stats.slot_tree_bytes_p99.load());
    try std.testing.expectEqual(@as(u64, 20), stats.slot_tree_bytes_max.load());
}
