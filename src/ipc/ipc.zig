//! The client bypass's shared-memory session (charter §7): the sealed memfd
//! layout, the slot/ring/doorbell cores with their models, the `il0`
//! rendezvous and the daemon's fd screen, the heartbeat, the sizing
//! derivations, and the `ipc_*` observability family. `client.zig` (THE ONE
//! client library both the shim and the SDK consume) hangs off
//! `squeezefs.ipc.client`; the daemon host (`host.zig`: admission, the
//! service threads, the direct-drive lanes) off `squeezefs.ipc.host`.
//!
//! Portable here: the layout, the cores, the models, the wire, the sizing,
//! the pure rendezvous verdicts. Linux only (gated, never stubbed): the
//! memfd (`shm.zig`), the sockets and the fd facts (`rendezvous_linux.zig`).

const std = @import("std");
const builtin = @import("builtin");
const counters = @import("../core/counters.zig");

pub const layout = @import("layout.zig");
pub const slot_core = @import("slot_core.zig");
pub const slot_core_model = @import("slot_core_model.zig");
pub const ring_core = @import("ring_core.zig");
pub const ring_core_model = @import("ring_core_model.zig");
pub const doorbell = @import("doorbell.zig");
pub const doorbell_model = @import("doorbell_model.zig");
pub const wire = @import("wire.zig");
pub const sizing = @import("sizing.zig");
pub const rendezvous = @import("rendezvous.zig");
pub const shm = if (builtin.os.tag == .linux) @import("shm.zig") else struct {};
pub const rendezvous_linux = if (builtin.os.tag == .linux) @import("rendezvous_linux.zig") else struct {};

// ---------------------------------------------------------------------------
// The `ipc_*` family (charter §10; spec §7.2): the refusal ledger, the
// admission instrument, the wake economy, the service parks. Per-op counts
// are sharded; the admission histogram is exact-sum.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Sessions admitted (SESSION_OK sent).
    pub var ipc_sessions_total: counters.ShardedCounter = .{};
    /// HELLO receipt → SESSION_OK sent: the fleet-launch convoy instrument.
    pub var ipc_session_admission_ns: counters.ShardedLatencyHistogram = .{};
    /// Budget refusals (the shed target, the arena cap, the per-uid cap).
    pub var ipc_admission_refusals: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_version: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_nonce: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_flags: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_mount: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_budget: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_peercred: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_internal: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_disabled: counters.ShardedCounter = .{};
    pub var ipc_bind_refused_rights: counters.ShardedCounter = .{};
    /// Degenerate identities admitted by `SQUEEZEFS_TEST_IPC_ALLOW_DIRTY`.
    pub var ipc_binds_dev_override: counters.ShardedCounter = .{};
    /// The completion doorbell's wake economy: `writes / (writes + elided
    /// + collapsed) ≤ 0.25` at 32×8 is the campaign target; ≈ 1 under a
    /// saturated parked reaper means the latch stopped engaging.
    pub var ipc_cqe_wake_writes: counters.ShardedCounter = .{};
    pub var ipc_cqe_wake_collapsed: counters.ShardedCounter = .{};
    pub var ipc_cqe_wake_elided: counters.ShardedCounter = .{};
    /// Service-thread parks (bounded 5 ms each).
    pub var ipc_service_parks: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try counters.writeNamespaceJson(stats, w);
}

/// The refusal ledger: one counter per class; a budget refusal also counts
/// `ipc_admission_refusals` (the engagement gauge of the cap).
pub fn countRefusal(class: wire.RefuseClass) void {
    switch (class) {
        .version => stats.ipc_bind_refused_version.inc(),
        .nonce => stats.ipc_bind_refused_nonce.inc(),
        .flags => stats.ipc_bind_refused_flags.inc(),
        .mount => stats.ipc_bind_refused_mount.inc(),
        .budget => {
            stats.ipc_bind_refused_budget.inc();
            stats.ipc_admission_refusals.inc();
        },
        .peercred => stats.ipc_bind_refused_peercred.inc(),
        .internal => stats.ipc_bind_refused_internal.inc(),
        .disabled => stats.ipc_bind_refused_disabled.inc(),
        .rights => stats.ipc_bind_refused_rights.inc(),
    }
}

/// The refusal class a class counter answers to (the ledger's inverse, for
/// tests and the CLI's `clients` verb).
pub fn refusalCount(class: wire.RefuseClass) u64 {
    return switch (class) {
        .version => stats.ipc_bind_refused_version.load(),
        .nonce => stats.ipc_bind_refused_nonce.load(),
        .flags => stats.ipc_bind_refused_flags.load(),
        .mount => stats.ipc_bind_refused_mount.load(),
        .budget => stats.ipc_bind_refused_budget.load(),
        .peercred => stats.ipc_bind_refused_peercred.load(),
        .internal => stats.ipc_bind_refused_internal.load(),
        .disabled => stats.ipc_bind_refused_disabled.load(),
        .rights => stats.ipc_bind_refused_rights.load(),
    };
}

pub fn countWake(o: doorbell.Outcome) void {
    switch (o) {
        .wake => stats.ipc_cqe_wake_writes.inc(),
        .collapsed => stats.ipc_cqe_wake_collapsed.inc(),
        .elided => stats.ipc_cqe_wake_elided.inc(),
    }
}

test {
    _ = layout;
    _ = slot_core;
    _ = slot_core_model;
    _ = ring_core;
    _ = ring_core_model;
    _ = doorbell;
    _ = doorbell_model;
    _ = wire;
    _ = sizing;
    _ = rendezvous;
    if (builtin.os.tag == .linux) _ = shm;
    if (builtin.os.tag == .linux) _ = rendezvous_linux;
    // The daemon host's PURE cores run on every host; its root (`host.zig`:
    // listeners, service threads, rings) is Linux only via `squeezefs.ipc.host`.
    _ = @import("host/stats.zig");
    _ = @import("host/severed_pool.zig");
    _ = @import("host/placed.zig");
    _ = @import("host/placed_claims_model.zig");
    _ = @import("host/inval.zig");
}

const testing = std.testing;

test "the refusal ledger counts each class on its own counter; budget also counts the admission refusals; the family renders every instrument" {
    const before_budget = refusalCount(.budget);
    const before_adm = stats.ipc_admission_refusals.load();
    const before_mount = refusalCount(.mount);
    countRefusal(.budget);
    countRefusal(.mount);
    try testing.expectEqual(before_budget + 1, refusalCount(.budget));
    try testing.expectEqual(before_adm + 1, stats.ipc_admission_refusals.load());
    try testing.expectEqual(before_mount + 1, refusalCount(.mount));
    var buf: [4096]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    inline for (comptime counters.instrumentNames(stats)) |name| try testing.expect(std.mem.indexOf(u8, out, name) != null);
    try testing.expect(std.mem.indexOf(u8, out, "\"ipc_bind_refused_rights\":") != null);
}
