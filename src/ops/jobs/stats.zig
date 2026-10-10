//! The job fabric's instruments (charter §10; spec §4.2): the fabric
//! counters and gauges, the remote wire's, the fleet's, the tripwires, and
//! the closure identities (stated here, tested here, exported through
//! `stats/stats.zig`'s `ops` family). Every per-op count is a
//! `ShardedCounter`; levels are `Gauge`s; every "cannot happen" a
//! `Tripwire` (must-stay-0, never a panic in a handler). Portable.

const std = @import("std");
const counters = @import("../../core/counters.zig");

pub const stats = struct {
    // ---- the fabric ----
    pub var job_submitted: counters.ShardedCounter = .{};
    pub var job_completed: counters.ShardedCounter = .{};
    pub var job_cancelled: counters.ShardedCounter = .{};
    pub var job_failed: counters.ShardedCounter = .{};
    pub var job_tasks_done: counters.ShardedCounter = .{};
    pub var job_checkpoint_writes: counters.ShardedCounter = .{};
    /// A mover queued LOUD behind an intersecting claimed scope (once per episode).
    pub var job_serialized_waits: counters.ShardedCounter = .{};
    /// Memory Red's shed hook paused a running job (never self-resumed).
    pub var job_paused_mem_pressure: counters.ShardedCounter = .{};
    /// A drain's checkpoint re-verification parked it `PausedCapacity`.
    pub var job_paused_capacity: counters.ShardedCounter = .{};
    /// Entries into the park window (the lost-resume test's observable).
    pub var job_park_entries: counters.ShardedCounter = .{};
    /// Adopted records that would not decode (skipped, named in the log).
    pub var job_records_corrupt: counters.ShardedCounter = .{};
    pub var job_adopted: counters.ShardedCounter = .{};
    pub var job_active: counters.Gauge = .{};
    pub var job_queued: counters.Gauge = .{};
    pub var job_paused: counters.Gauge = .{};
    /// The `job_copy_buffers` budget component's gauge.
    pub var job_copy_buffer_bytes: counters.Gauge = .{};
    pub var job_workers: counters.Gauge = .{};

    // ---- the remote wire ----
    pub var job_remote_workers: counters.Gauge = .{};
    pub var job_remote_enrollments: counters.ShardedCounter = .{};
    pub var job_remote_enroll_refused: counters.ShardedCounter = .{};
    pub var job_remote_shards: counters.ShardedCounter = .{};
    pub var job_remote_submissions: counters.ShardedCounter = .{};
    pub var job_remote_refused_stale: counters.ShardedCounter = .{};
    pub var job_remote_lease_expiries: counters.ShardedCounter = .{};
    pub var job_remote_reassignments: counters.ShardedCounter = .{};
    pub var job_remote_bytes_moved: counters.ShardedCounter = .{};
    pub var job_remote_verify_read_bytes: counters.ShardedCounter = .{};
    pub var job_remote_verify_failures: counters.ShardedCounter = .{};
    pub var job_remote_quarantined_destinations: counters.ShardedCounter = .{};
    pub var job_remote_pr_preempts: counters.ShardedCounter = .{};
    /// 0 = deferred-reclaim, 1 = pr.
    pub var job_remote_fence_mode: counters.Gauge = .{};
    pub var job_remote_abandons: counters.ShardedCounter = .{};

    // ---- the fleet read shards ----
    pub var job_fleet_shards_dispatched: counters.ShardedCounter = .{};
    pub var job_fleet_shards_completed: counters.ShardedCounter = .{};
    pub var job_fleet_shards_relocal: counters.ShardedCounter = .{};
    pub var job_fleet_worker_shards: counters.ShardedCounter = .{};
    pub var job_fleet_worker_red_aborts: counters.ShardedCounter = .{};

    // ---- the admin lane (`admin.zig`) ----
    /// Admin verbs served (every verb of the `0x07xx` block).
    pub var admin_verbs: counters.ShardedCounter = .{};
    /// Admin verbs answered with a non-ok verdict.
    pub var admin_refusals: counters.ShardedCounter = .{};
    /// Jobs submitted through the lane.
    pub var admin_submissions: counters.ShardedCounter = .{};

    // ---- tripwires (must-stay-0) ----
    /// A terminal state asked to change.
    pub var job_terminal_regressed: counters.Tripwire = .init("job_terminal_regressed");
    /// The allocator handed a quarantined tuple to a fresh assignment.
    pub var job_quarantined_destination_allocated: counters.Tripwire = .init("job_quarantined_destination_allocated");
    /// A claim released by a worker that never held it.
    pub var job_claim_released_unheld: counters.Tripwire = .init("job_claim_released_unheld");
};

/// `job_submitted ≡ job_completed + job_cancelled + job_failed + live
/// non-terminal` (the live count is the caller's: a process-wide gauge
/// cannot know a fabric instance's population).
pub fn submittedClosure(live_non_terminal: u64) bool {
    return stats.job_submitted.load() == stats.job_completed.load() + stats.job_cancelled.load() + stats.job_failed.load() + live_non_terminal;
}

/// `job_remote_shards ≡ job_remote_submissions + job_remote_lease_expiries
/// + in_flight` (a submission after an expiry is refused stale, not
/// counted twice).
pub fn remoteShardClosure(in_flight: u64) bool {
    return stats.job_remote_shards.load() == stats.job_remote_submissions.load() + stats.job_remote_lease_expiries.load() + in_flight;
}

/// `dispatched ≡ completed + relocal + pending`.
pub fn fleetClosure(pending: u64) bool {
    return stats.job_fleet_shards_dispatched.load() == stats.job_fleet_shards_completed.load() + stats.job_fleet_shards_relocal.load() + pending;
}

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(stats, w);
}

test "the family renders as one JSON object with every instrument named; the closures hold on a quiet fabric" {
    var buf: [8192]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    try std.testing.expect(out[0] == '{' and out[out.len - 1] == '}');
    try std.testing.expect(std.mem.indexOf(u8, out, "\"job_submitted\"") != null);
    try std.testing.expect(std.mem.indexOf(u8, out, "\"job_remote_fence_mode\"") != null);
    try std.testing.expect(std.mem.indexOf(u8, out, "\"job_terminal_regressed\"") != null);
    const live = stats.job_submitted.load() - stats.job_completed.load() - stats.job_cancelled.load() - stats.job_failed.load();
    try std.testing.expect(submittedClosure(live));
}
