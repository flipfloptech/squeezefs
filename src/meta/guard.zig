//! The D0 single-writer mount guard (charter §5.7; `docs/porting/
//! guard_claims.md`): the three layers every writer walks — (A) the flock
//! on a dedicated fd (`backend.zig`), (B1) NVMe Persistent Reservations
//! where `RESCAP ≠ 0` (`pr.zig` over the `/dev/ngXnY` uring_cmd transport
//! of `pr_linux.zig`, or the fake), (B2) the `writer_claim` record on ino 1
//! committed and barriered before the checkpoint task spawns and before
//! FUSE arms, heartbeat 10 s, stale at 45 s (`claim.zig`, `gate.zig`) —
//! plus the fence latch's third rung (`journal.FailureLatch.noteFence`),
//! the recovery ladder (same-host dead-pid proof → PR preempt → TTL →
//! `squeezefs claim clear`) and the `clients` plane. No bypass knob.
//!
//! Portable here: the record codec and classification, the lease-clock
//! arithmetic and its models, the NVMe command codec, the fake namespace,
//! the PR client. Linux only: the procfs/sysfs readers, the char-device
//! transport, the backend binding and the smoke stage.

const std = @import("std");
const builtin = @import("builtin");
const is_linux = builtin.os.tag == .linux;

const counters = @import("../core/counters.zig");

pub const claim = @import("guard/claim.zig");
pub const lease_clock = @import("guard/lease_clock.zig");
pub const lease_clock_model = @import("guard/lease_clock_model.zig");
pub const nvme_cmd = @import("guard/nvme_cmd.zig");
pub const fake_nvme = @import("guard/fake_nvme.zig");
pub const pr = @import("guard/pr.zig");

pub const proc = if (is_linux) @import("guard/proc.zig") else struct {};
pub const pr_linux = if (is_linux) @import("guard/pr_linux.zig") else struct {};
pub const gate = if (is_linux) @import("guard/gate.zig") else struct {};
pub const smoke = if (is_linux) @import("guard/smoke.zig") else struct {};

/// Process-wide guard counters (charter §10; spec §4.2). Per-volume detail
/// (`writer_guard_mode`, the PR gauges) rides the backend's `Guard`.
pub const Stats = struct {
    /// `writer_guard_fenced`: reservation-conflict fail-stops at a write,
    /// a barrier or the heartbeat's usurpation re-check. Must stay 0 on a
    /// healthy fleet — "working as designed, always investigate".
    fenced: counters.ShardedCounter = .{},
    /// `volume_fenced`: volumes FAILED by the fence this process holds.
    volume_fenced: counters.Gauge = .{},
    /// `writer_guard_pr_reacquires`: PTPL-lapse re-acquires (expected to
    /// grow across nvmet restarts — a counter, not a tripwire).
    pr_reacquires: counters.ShardedCounter = .{},
    /// `writer_guard_claims_committed`: claim gates that committed.
    claims_committed: counters.ShardedCounter = .{},
    heartbeats: counters.ShardedCounter = .{},
    heartbeat_failures: counters.ShardedCounter = .{},
    /// `writer_guard_reclaims_dead_pid`: claims reclaimed by the proof.
    reclaims_dead_pid: counters.ShardedCounter = .{},
    /// `writer_guard_preempts`: PR preempts of a TTL-stale holder.
    preempts: counters.ShardedCounter = .{},
    /// `writer_guard_pr_unavailable`: NVMe namespaces without a char twin.
    pr_unavailable: counters.ShardedCounter = .{},
    /// `writer_guard_claim_refusals`: mounts refused by a FRESH or
    /// non-PR-stale claim (the flock refusals count on the backend).
    claim_refusals: counters.ShardedCounter = .{},
    clear_cleared: counters.ShardedCounter = .{},
    clear_refusals: counters.ShardedCounter = .{},
    /// `clients_registered`: live writer claims this process holds.
    clients_registered: counters.Gauge = .{},
    /// A changed hostnqn/hostid under a live mount cannot happen on a
    /// correctly provisioned host.
    identity_mismatch: counters.Tripwire = .init("writer_guard_identity_mismatch"),
};

pub var stats: Stats = .{};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"writer_guard_fenced\":{d},\"volume_fenced\":{d},\"writer_guard_pr_reacquires\":{d},\"writer_guard_claims_committed\":{d},\"writer_guard_heartbeats\":{d},\"writer_guard_heartbeat_failures\":{d},\"writer_guard_reclaims_dead_pid\":{d},\"writer_guard_preempts\":{d},\"writer_guard_pr_unavailable\":{d},\"writer_guard_claim_refusals\":{d},\"writer_guard_clear_cleared\":{d},\"writer_guard_clear_refusals\":{d},\"clients_registered\":{d},\"writer_guard_identity_mismatch\":{d},\"writer_guard_heartbeat_interval_s\":{d},\"writer_guard_stale_ttl_s\":{d}}}", .{
        stats.fenced.load(),
        stats.volume_fenced.load(),
        stats.pr_reacquires.load(),
        stats.claims_committed.load(),
        stats.heartbeats.load(),
        stats.heartbeat_failures.load(),
        stats.reclaims_dead_pid.load(),
        stats.preempts.load(),
        stats.pr_unavailable.load(),
        stats.claim_refusals.load(),
        stats.clear_cleared.load(),
        stats.clear_refusals.load(),
        stats.clients_registered.load(),
        stats.identity_mismatch.load(),
        claim.HEARTBEAT_INTERVAL_S,
        claim.STALE_TTL_S,
    });
}

test {
    _ = claim;
    _ = lease_clock;
    _ = lease_clock_model;
    _ = nvme_cmd;
    _ = fake_nvme;
    _ = pr;
    if (is_linux) {
        _ = proc;
        _ = pr_linux;
        _ = gate;
        _ = smoke;
    }
}

test "the stats JSON names every writer_guard_* family and the two constants" {
    var buf: [1024]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    inline for (.{ "writer_guard_fenced", "volume_fenced", "writer_guard_pr_reacquires", "writer_guard_claims_committed", "writer_guard_heartbeats", "writer_guard_reclaims_dead_pid", "writer_guard_preempts", "writer_guard_pr_unavailable", "clients_registered", "writer_guard_identity_mismatch" }) |k| {
        try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"" ++ k ++ "\":") != null);
    }
    try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"writer_guard_stale_ttl_s\":45") != null);
}
