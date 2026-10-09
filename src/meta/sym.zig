//! Symmetric metadata, part A (charter §5.5 "The forest", "Appenders",
//! "The record-seq space is monotone across rings", "Node seqs are
//! incarnation-scoped", "The manager lease", "Extent grants"; §5.7 the D0
//! winner IS the manager; `docs/porting/sym_appenders_manager.md`): tree
//! 0's control records (`control_records`), the appender page and its four
//! slots (`appender_page`), the appender directory chain (`appender_dir`),
//! the region — ring segments, seq words, the page plan, the tail law
//! (`region`), extent grants and the RAM pool (`extent_grant`), the manager
//! with its verbs, screens and the structural door (`manager`), the join
//! ladder of a later RW mount (`join`); part B1's slot leases — the dense
//! lease table, the ONE commit door, acquisition and minting, handover,
//! the page-budget law, slot → holder resolution (`slots`). Portable
//! throughout; the device binding — the backend's regions, the manager
//! over the volume, the checkpoint's page writes, the in-process join
//! door — is `bind` (Linux), the slot plane's `slots/plane.zig` (Linux).

const std = @import("std");
const builtin = @import("builtin");
const counters = @import("../core/counters.zig");

pub const control_records = @import("sym/control_records.zig");
pub const appender_page = @import("sym/appender_page.zig");
pub const appender_dir = @import("sym/appender_dir.zig");
pub const region = @import("sym/region.zig");
pub const extent_grant = @import("sym/extent_grant.zig");
pub const manager = @import("sym/manager.zig");
pub const join = @import("sym/join.zig");
pub const slots = @import("sym/slots.zig");
pub const bind = if (builtin.os.tag == .linux) @import("sym/bind.zig") else struct {};
pub const joined = if (builtin.os.tag == .linux) @import("sym/joined.zig") else struct {};
pub const wire_plane = if (builtin.os.tag == .linux) @import("sym/wire_plane.zig") else struct {};
pub const reader_plane = if (builtin.os.tag == .linux) @import("sym/reader_plane.zig") else struct {};
pub const tokens = @import("sym/tokens.zig");
pub const membership = @import("sym/membership.zig");
pub const xv = @import("sym/xv.zig");

/// Process-wide appender instruments (the per-volume words — `appenders_
/// live`, `appenders_capacity`, `manager_lease`, the ring bytes — ride the
/// backend's `AppenderSet.writeStatsJson`).
pub const stats = struct {
    /// Volumes this process mounted with the symmetric plane stamped.
    pub var symmetric_meta: counters.Gauge = .{};
    /// Live pages of OTHER appenders found at a writer open: their rings
    /// are unrecovered and their slots are refused until the recovery wave
    /// recovers them (loud at the mount).
    pub var appender_live_pages_at_mount: counters.ShardedCounter = .{};
    /// Regions this process holds across its volumes.
    pub var appenders_live: counters.Gauge = .{};
    /// Appender pages written by the checkpoint cycle.
    pub var appender_page_writes: counters.ShardedCounter = .{};
    /// The pool's recycles and the surplus returns at the cadence.
    pub var extent_grant_recycled: counters.ShardedCounter = .{};
    /// Appender rings reclaimed at a cycle fixpoint (the region tail law).
    pub var appender_ring_tail_advances: counters.ShardedCounter = .{};
    /// A region's record floored a node of the shared cache (ring 0's
    /// domain): the tail law's engagement.
    pub var appender_region_floors: counters.ShardedCounter = .{};
    /// Records appended into a region ≥ 1's ring.
    pub var appender_region_entries: counters.ShardedCounter = .{};
};

/// The `.stats` `meta.sym` family.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{\"appenders\":");
    try counters.writeNamespaceJson(stats, w);
    try w.writeAll(",\"manager\":");
    try counters.writeNamespaceJson(manager.stats, w);
    try w.writeAll(",\"join\":");
    try counters.writeNamespaceJson(join.stats, w);
    try w.writeAll(",\"slots\":");
    try slots.writeStatsJson(w);
    try w.writeAll(",\"tokens\":");
    try tokens.writeStatsJson(w);
    try w.writeAll(",\"membership\":");
    try membership.writeStatsJson(w);
    try w.writeAll(",\"xv\":");
    try xv.writeStatsJson(w);
    try w.writeAll(",\"wire\":");
    if (builtin.os.tag == .linux) try counters.writeNamespaceJson(wire_plane.stats, w) else try w.writeAll("{}");
    try w.writeAll(",\"joined\":");
    if (builtin.os.tag == .linux) try counters.writeNamespaceJson(joined.stats, w) else try w.writeAll("{}");
    try w.writeAll(",\"reader\":");
    if (builtin.os.tag == .linux) try counters.writeNamespaceJson(reader_plane.stats, w) else try w.writeAll("{}");
    try w.writeByte('}');
}

test {
    _ = control_records;
    _ = appender_page;
    _ = appender_dir;
    _ = region;
    _ = extent_grant;
    _ = manager;
    _ = join;
    _ = slots;
    if (builtin.os.tag == .linux) _ = bind;
    if (builtin.os.tag == .linux) _ = wire_plane;
    if (builtin.os.tag == .linux) _ = reader_plane;
    _ = tokens;
    _ = membership;
    _ = xv;
}
