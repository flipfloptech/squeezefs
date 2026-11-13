//! The D0 single-writer mount guard (charter §5.7; `docs/porting/
//! guard_claims.md`): the three layers every writer walks — (A) the flock
//! on a dedicated fd (`backend.zig`), (B1) NVMe Persistent Reservations
//! where `RESCAP ≠ 0` (`pr.zig` over the `/dev/ngXnY` uring_cmd transport
//! of `pr_linux.zig`, or the fake), (B2) the `writer_claim` record in tree 0
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
pub const member_fence = @import("guard/member_fence.zig");
pub const data_fence_model = @import("guard/data_fence_model.zig");
pub const meta_fence_model = @import("guard/meta_fence_model.zig");

pub const proc = if (is_linux) @import("guard/proc.zig") else struct {};
pub const pr_linux = if (is_linux) @import("guard/pr_linux.zig") else struct {};
pub const gate = if (is_linux) @import("guard/gate.zig") else struct {};
pub const host_fence = if (is_linux) @import("guard/host_fence.zig") else struct {};
pub const marker = if (is_linux) @import("guard/marker.zig") else struct {};
pub const convert_pr = if (is_linux) @import("guard/convert_pr.zig") else struct {};
pub const data_fence = if (is_linux) @import("guard/data_fence.zig") else struct {};
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
    /// `writer_guard_preempt_alls`: symmetric takeovers that landed a
    /// preempt-all (decision 0004 §4.4). One per command, including a first
    /// manager's preempt-all that removes nobody.
    preempt_alls: counters.ShardedCounter = .{},
    /// `writer_guard_preempt_all_victims`: other host ids in the report
    /// taken before the preempt-all.
    preempt_all_victims: counters.ShardedCounter = .{},
    /// `writer_guard_preempt_all_unseen_events`: generation gap beyond the
    /// preempt-all's own bump (decision 0004 §4.4 step 6).
    preempt_all_unseen_events: counters.ShardedCounter = .{},
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
    /// `pr_host_registrations`: registrations that admitted this host
    /// afresh on a namespace (after the stale-sibling rule).
    pr_host_registrations: counters.ShardedCounter = .{},
    /// `pr_host_adoptions`: mounts that found their host registered under
    /// the set's key — no device mutation, nobody re-admitted.
    pr_host_adoptions: counters.ShardedCounter = .{},
    /// `pr_stale_sibling_kills`: stale siblings killed before a
    /// registration admitted their host.
    pr_stale_sibling_kills: counters.ShardedCounter = .{},
    /// `pr_siblings_proven_dead`: registry entries the writer-id proof
    /// found dead.
    pr_siblings_proven_dead: counters.ShardedCounter = .{},
    /// `pr_stale_sibling_refusals`: registrations refused by the host's rule —
    /// a stale sibling that could be neither proven dead nor killed, or one it
    /// could not judge at all (a foreign registry root, another node token:
    /// decision 0004 §4.4.2, KD19). Every refusal below counts here too.
    pr_stale_sibling_refusals: counters.ShardedCounter = .{},
    /// `pr_stale_sibling_refusals_registry_root`: the daemon's host registry
    /// root is not the host's (its own `/run`), or pid 1's view has no such
    /// path: its siblings are invisible to it and it to them (KD19).
    pr_stale_sibling_refusals_registry_root: counters.ShardedCounter = .{},
    /// `pr_stale_sibling_refusals_registry_root_unreadable`: the comparison
    /// itself failed (`/proc/1/root` under `hidepid`, an LSM, any other
    /// errno). "Cannot compare" is never a pass (KD19).
    pr_stale_sibling_refusals_registry_root_unreadable: counters.ShardedCounter = .{},
    /// `pr_stale_sibling_refusals_node_token_hostname`: a live sibling under
    /// another node token in ANOTHER UTS namespace — two hostnames for one set
    /// on one host (KD19).
    pr_stale_sibling_refusals_node_token_hostname: counters.ShardedCounter = .{},
    /// `pr_stale_sibling_refusals_node_token_renamed`: a live sibling under
    /// another node token in THIS UTS namespace — the host's name changed while
    /// it runs, so it keeps the previous name's token (KD19).
    pr_stale_sibling_refusals_node_token_renamed: counters.ShardedCounter = .{},
    /// `pr_stale_sibling_refusals_node_token_missing`: a live sibling's entry
    /// carries no node token at all (an older binary). Never judged under this
    /// process's token (KD19).
    pr_stale_sibling_refusals_node_token_missing: counters.ShardedCounter = .{},
    /// `writer_guard_claim_probes`: private claim replays (`gate.probeClaim`:
    /// two per writer open on an NVMe namespace, up to three per lapse).
    claim_probes: counters.ShardedCounter = .{},
    /// `writer_guard_claim_probe_bytes`: what they read off the device (both
    /// superblock copies, the ledger, the whole fixed ring).
    claim_probe_bytes: counters.ShardedCounter = .{},
    /// `writer_guard_claim_probe_frames`: the ring entries they replayed.
    claim_probe_frames: counters.ShardedCounter = .{},
    /// `writer_guard_lapse_beats`: heartbeats that found the reservation
    /// lapsed with the device's claim ours (the lapse's length in beats,
    /// the re-acquiring beat included).
    lapse_beats: counters.ShardedCounter = .{},
    /// `writer_guard_lapse_early_ends`: lapse graces ended before their
    /// length — every live member's host key in the census registered
    /// again (decision 0003 §DL).
    lapse_early_ends: counters.ShardedCounter = .{},
    /// `writer_guard_succession_settles`: succession windows that settled
    /// (decision 0004 §4.6). One per managed volume.
    succession_settles: counters.ShardedCounter = .{},
    /// `writer_guard_succession_early_settles`: of those, settled before
    /// the grace deadline — every expected member back and registered.
    succession_early_settles: counters.ShardedCounter = .{},
    /// `writer_guard_succession_evictions`: listed writers W_end evicted
    /// because their host key was not registered on every managed namespace
    /// (decision 0004 §4.6 step 2). One per writer, not per volume.
    succession_evictions: counters.ShardedCounter = .{},
    /// `writer_guard_preempt_all_ns`: how long the preempt-and-abort ioctl
    /// ran (decision 0004 §9, A7). Commands admitted before the victim's
    /// namespace ref was killed are what stretch it.
    preempt_all_ns: counters.ShardedCounter = .{},
    /// `writer_guard_identity_failures`: beats whose appender-0 page named
    /// another writer or another term (decision 0004 §4.4.2). The volume
    /// fail-stops.
    identity_failures: counters.ShardedCounter = .{},
    /// `writer_guard_identity_unreadable_beats`: beats whose appender-0
    /// page did not decode. One claim TTL of them fail-stops.
    identity_unreadable_beats: counters.ShardedCounter = .{},
};

pub var stats: Stats = .{};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    // Two prints: a format call takes at most 32 arguments.
    try w.print("{{\"writer_guard_fenced\":{d},\"volume_fenced\":{d},\"writer_guard_pr_reacquires\":{d},\"writer_guard_claims_committed\":{d},\"writer_guard_heartbeats\":{d},\"writer_guard_heartbeat_failures\":{d},\"writer_guard_reclaims_dead_pid\":{d},\"writer_guard_preempts\":{d},\"writer_guard_preempt_alls\":{d},\"writer_guard_preempt_all_victims\":{d},\"writer_guard_preempt_all_unseen_events\":{d},\"writer_guard_pr_unavailable\":{d},\"writer_guard_claim_refusals\":{d},\"writer_guard_clear_cleared\":{d},\"writer_guard_clear_refusals\":{d},\"clients_registered\":{d},\"writer_guard_identity_mismatch\":{d},\"pr_host_registrations\":{d},\"pr_host_adoptions\":{d},\"pr_stale_sibling_kills\":{d},\"pr_siblings_proven_dead\":{d},\"pr_stale_sibling_refusals\":{d},\"writer_guard_claim_probes\":{d},\"writer_guard_claim_probe_bytes\":{d},\"writer_guard_claim_probe_frames\":{d},\"writer_guard_lapse_beats\":{d},\"writer_guard_lapse_early_ends\":{d},\"writer_guard_succession_settles\":{d},\"writer_guard_succession_early_settles\":{d},\"writer_guard_succession_evictions\":{d},\"writer_guard_preempt_all_ns\":{d}", .{
        stats.fenced.load(),
        stats.volume_fenced.load(),
        stats.pr_reacquires.load(),
        stats.claims_committed.load(),
        stats.heartbeats.load(),
        stats.heartbeat_failures.load(),
        stats.reclaims_dead_pid.load(),
        stats.preempts.load(),
        stats.preempt_alls.load(),
        stats.preempt_all_victims.load(),
        stats.preempt_all_unseen_events.load(),
        stats.pr_unavailable.load(),
        stats.claim_refusals.load(),
        stats.clear_cleared.load(),
        stats.clear_refusals.load(),
        stats.clients_registered.load(),
        stats.identity_mismatch.load(),
        stats.pr_host_registrations.load(),
        stats.pr_host_adoptions.load(),
        stats.pr_stale_sibling_kills.load(),
        stats.pr_siblings_proven_dead.load(),
        stats.pr_stale_sibling_refusals.load(),
        stats.claim_probes.load(),
        stats.claim_probe_bytes.load(),
        stats.claim_probe_frames.load(),
        stats.lapse_beats.load(),
        stats.lapse_early_ends.load(),
        stats.succession_settles.load(),
        stats.succession_early_settles.load(),
        stats.succession_evictions.load(),
        stats.preempt_all_ns.load(),
    });
    try w.print(",\"writer_guard_identity_failures\":{d},\"writer_guard_identity_unreadable_beats\":{d},\"pr_stale_sibling_refusals_registry_root\":{d},\"pr_stale_sibling_refusals_registry_root_unreadable\":{d},\"pr_stale_sibling_refusals_node_token_hostname\":{d},\"pr_stale_sibling_refusals_node_token_renamed\":{d},\"pr_stale_sibling_refusals_node_token_missing\":{d},\"writer_guard_heartbeat_interval_s\":{d},\"writer_guard_stale_ttl_s\":{d}}}", .{
        stats.identity_failures.load(),
        stats.identity_unreadable_beats.load(),
        stats.pr_stale_sibling_refusals_registry_root.load(),
        stats.pr_stale_sibling_refusals_registry_root_unreadable.load(),
        stats.pr_stale_sibling_refusals_node_token_hostname.load(),
        stats.pr_stale_sibling_refusals_node_token_renamed.load(),
        stats.pr_stale_sibling_refusals_node_token_missing.load(),
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
    _ = member_fence;
    _ = data_fence_model;
    _ = meta_fence_model;
    if (is_linux) {
        _ = proc;
        _ = pr_linux;
        _ = gate;
        _ = host_fence;
        _ = marker;
        _ = convert_pr;
        _ = data_fence;
        _ = smoke;
    }
}

test "the stats JSON names every writer_guard_* family and the two constants" {
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    inline for (.{ "writer_guard_claim_probes", "writer_guard_claim_probe_bytes", "writer_guard_claim_probe_frames", "writer_guard_lapse_beats", "writer_guard_lapse_early_ends", "writer_guard_succession_settles", "writer_guard_succession_early_settles", "writer_guard_succession_evictions", "writer_guard_preempt_all_ns", "writer_guard_identity_failures", "writer_guard_identity_unreadable_beats", "writer_guard_fenced", "volume_fenced", "writer_guard_pr_reacquires", "writer_guard_claims_committed", "writer_guard_heartbeats", "writer_guard_reclaims_dead_pid", "writer_guard_preempts", "writer_guard_preempt_alls", "writer_guard_preempt_all_victims", "writer_guard_preempt_all_unseen_events", "writer_guard_pr_unavailable", "clients_registered", "writer_guard_identity_mismatch", "pr_host_registrations", "pr_host_adoptions", "pr_stale_sibling_kills", "pr_siblings_proven_dead", "pr_stale_sibling_refusals", "pr_stale_sibling_refusals_registry_root", "pr_stale_sibling_refusals_registry_root_unreadable", "pr_stale_sibling_refusals_node_token_hostname", "pr_stale_sibling_refusals_node_token_renamed", "pr_stale_sibling_refusals_node_token_missing" }) |k| {
        try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"" ++ k ++ "\":") != null);
    }
    try std.testing.expect(std.mem.indexOf(u8, w.buffered(), "\"writer_guard_stale_ttl_s\":45") != null);
}
