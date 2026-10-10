//! The `nvmet_*` / `fabric_*` instruments (charter §10; `docs/porting/
//! ops_nvmet_keys.md` §8.2): always exported, zero when inert. The
//! `pr_*` family rides the PR client and `fencing.zig`; `writer_guard_*`
//! the guard. Portable: the `.stats` aggregator composes this namespace
//! on every host.

const std = @import("std");
const counters = @import("../../core/counters.zig");

/// Shares the configfs recipe completed (the namespace enabled and
/// every link made).
pub var nvmet_shares_applied: counters.ShardedCounter = .{};
pub var nvmet_shares_torn_down: counters.ShardedCounter = .{};
/// A kernel whose nvmet has no `resv_enable`: the share serves WITHOUT
/// Persistent Reservations (loud).
pub var nvmet_shares_without_pr: counters.ShardedCounter = .{};
/// A configfs attribute write the recipe expected to succeed refused:
/// must stay 0 in the fidelity tier (per-site label = the attribute).
pub var nvmet_configfs_write_failures: counters.Tripwire = .init("nvmet_configfs_write_failures");
/// The share ledger's generation after the last write this process made.
pub var nvmet_ledger_generation: counters.Gauge = .{};
pub var nvmet_ledger_writes: counters.ShardedCounter = .{};
/// Restores by outcome.
pub var nvmet_restore_restored: counters.ShardedCounter = .{};
pub var nvmet_restore_noops: counters.ShardedCounter = .{};
pub var nvmet_restore_failures: counters.ShardedCounter = .{};
pub var nvmet_adopts: counters.ShardedCounter = .{};

/// Controllers with `transport != pcie` (sampled off `/sys/class/nvme`).
pub var fabric_controllers: counters.Gauge = .{};
/// Fabric controllers whose `state != live`.
pub var fabric_ctrl_not_live: counters.Gauge = .{};
/// Observed not-live → live transitions per identity: a SAMPLED
/// counter that UNDERCOUNTS flaps faster than the cadence (F:294-303).
pub var fabric_ctrl_reconnects: counters.ShardedCounter = .{};
/// `nvme_core.fabrics_host_scoped_subsystems`: 0 N, 1 Y, 2 absent (stock).
pub var fabric_host_scoped: counters.Gauge = .{};
/// How the connect identity resolved: 0 explicit, 1 /etc/nvme, 2 machine id.
pub var fabric_host_identity_source: counters.Gauge = .{};
pub var fabric_connects: counters.ShardedCounter = .{};
pub var fabric_connect_failures: counters.ShardedCounter = .{};
pub var fabric_disconnects: counters.ShardedCounter = .{};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(@This(), w);
}

test "the family names every nvmet_* and fabric_* instrument" {
    var buf: [2048]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    inline for (.{ "nvmet_shares_applied", "nvmet_configfs_write_failures", "nvmet_ledger_generation", "fabric_controllers", "fabric_ctrl_not_live", "fabric_ctrl_reconnects", "fabric_host_scoped", "fabric_host_identity_source", "fabric_connects" }) |k| {
        try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"" ++ k ++ "\":") != null);
    }
}
