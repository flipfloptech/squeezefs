//! The lifecycle's and the mover's instruments (spec §4.2): THE drain
//! engagement instrument is `evacuate_bytes_moved` — a drain row is
//! INVALID unless its delta accounts for the victim's used bytes; the
//! gauges `evacuate_{needed,avail,transient}_bytes` are published only for
//! the ACTIVE drain and zeroed at exit; `evacuate_inflight_bytes` returns
//! to 0 at every job exit (the RAII charge). Exported through the `ops`
//! family. Portable.

const std = @import("std");
const counters = @import("../../core/counters.zig");

pub const stats = struct {
    // ---- the mover ----
    pub var evacuate_blocks_moved: counters.ShardedCounter = .{};
    pub var evacuate_bytes_moved: counters.ShardedCounter = .{};
    pub var evacuate_shared_blocks_moved: counters.ShardedCounter = .{};
    pub var evacuate_inflight_bytes: counters.Gauge = .{};
    /// The expected-mismatch no-op (a foreground write won the race).
    pub var evacuate_stale_token_noops: counters.ShardedCounter = .{};
    pub var evacuate_deferred_staged_blocks: counters.ShardedCounter = .{};
    pub var evacuate_replans: counters.ShardedCounter = .{};
    pub var evacuate_needed_bytes: counters.Gauge = .{};
    pub var evacuate_avail_bytes: counters.Gauge = .{};
    pub var evacuate_transient_bytes: counters.Gauge = .{};
    pub var job_mover_custody_defers: counters.ShardedCounter = .{};
    pub var overlay_mover_skips: counters.ShardedCounter = .{};
    pub var pack_mover_open_defers: counters.ShardedCounter = .{};
    pub var pack_mover_resident_defers: counters.ShardedCounter = .{};

    // ---- the lifecycle ----
    pub var volume_preflight_refusals: counters.ShardedCounter = .{};
    pub var volume_lifecycle_commits: counters.ShardedCounter = .{};
    pub var volume_retired: counters.ShardedCounter = .{};
    pub var volume_undrained: counters.ShardedCounter = .{};
    /// Draining volumes NOW.
    pub var volume_draining: counters.Gauge = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(stats, w);
}

/// The active drain's gauges: published at entry, zeroed at exit.
pub fn publishDrainGauges(needed: u64, avail: u64, transient: u64) void {
    stats.evacuate_needed_bytes.set(needed);
    stats.evacuate_avail_bytes.set(avail);
    stats.evacuate_transient_bytes.set(transient);
}

pub fn clearDrainGauges() void {
    publishDrainGauges(0, 0, 0);
}

test "the family renders every instrument; the drain gauges publish and clear" {
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"evacuate_bytes_moved\"") != null);
    try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"volume_preflight_refusals\"") != null);
    publishDrainGauges(1, 2, 3);
    try std.testing.expectEqual(@as(u64, 2), stats.evacuate_avail_bytes.load());
    clearDrainGauges();
    try std.testing.expectEqual(@as(u64, 0), stats.evacuate_needed_bytes.load());
}
