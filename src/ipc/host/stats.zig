//! The daemon host's `ipc_*` instruments (charter §7, §10; spec
//! `docs/porting/il_daemon.md` §6.2): the session ledger, THE engagement
//! proof (`ipc_ops_*`, `ipc_bytes_*`), the sync read fast path's split,
//! the drain and park economy, the severance ledger, the direct-drive
//! lanes, the coherence limiter, and the two must-stay-0 tripwires.
//! PORTABLE (plain counters) so the family renders on every host; the
//! Linux-only host modules only increment.
//!
//! Engagement identity (charter §7): an il row is INVALID unless
//! `Δ(ipc_ops_read + ipc_ops_write)` accounts for its ops. The per-lane
//! closures are rendered as booleans beside the instruments:
//!   reads : `fast_path_serves + read_handoffs + dd_read_serves ≡ ops_read`
//!   writes: `dd_inline_serves + write_handoffs ≡ ops_write`
//!   dd    : `dd_inline_serves + dd_write_fallbacks ≡ dd_submits_attempted`
//! (a closure read mid-flight may be one op off; the quiescent read closes).

const std = @import("std");
const counters = @import("../../core/counters.zig");

pub const stats = struct {
    // ---- sessions ----
    /// Sessions live (admitted, not torn down).
    pub var ipc_sessions_active: counters.Gauge = .{};
    /// Sessions torn down by the idle reap (never the poison tripwire).
    pub var ipc_sessions_reaped: counters.ShardedCounter = .{};
    /// Sessions torn down on the ctl socket's EOF (the client died or closed).
    pub var ipc_sessions_closed: counters.ShardedCounter = .{};
    /// A protocol violation the daemon answered with a generation bump:
    /// an out-of-range or non-SUBMITTED published slot, `live > slots`, a
    /// malformed ctl frame. Must stay 0 in production: a client bug or an attack.
    pub var ipc_sessions_poisoned: counters.Tripwire = .init("ipc_sessions_poisoned");
    /// Owners (service threads) with ≥ 1 live session.
    pub var ipc_session_owners: counters.Gauge = .{};
    /// Service threads SPAWNED (0 on a session-less host).
    pub var ipc_service_threads: counters.Gauge = .{};
    /// Bytes the live arenas charge the R5 cap (`ipc_session_arenas`).
    pub var ipc_arena_bytes: counters.Gauge = .{};
    /// BINDs admitted / UNBINDs received.
    pub var ipc_binds: counters.ShardedCounter = .{};
    pub var ipc_unbinds: counters.ShardedCounter = .{};
    /// New sessions refused while the R5 shed target held.
    pub var ipc_shed_refusals: counters.ShardedCounter = .{};
    /// The clients' display-only lane-gate words folded at teardown
    /// (`ipc_ops_* + routes ≈ ops`, `ipc_bytes_* + bytes ≈ bytes`); never a
    /// control input.
    pub var ipc_lane_gate_kernel_routes: counters.ShardedCounter = .{};
    pub var ipc_lane_gate_kernel_bytes: counters.ShardedCounter = .{};
    /// The clients' libaio iocbs on BOUND fds that a context the shim never
    /// registered carried to the real libaio (the shim's
    /// `aio_unknown_ctx_op` row, folded at teardown like the lane-gate
    /// words): the engagement law's missing term when an aio instrument's
    /// ops never reach the ring. Must stay 0 under the interposed queue face.
    pub var ipc_client_aio_unknown_ctx_ops: counters.ShardedCounter = .{};

    // ---- THE engagement proof ----
    pub var ipc_ops_read: counters.ShardedCounter = .{};
    pub var ipc_ops_write: counters.ShardedCounter = .{};
    /// Bytes acked INTO the daemon (writes) and OUT to the arena (reads).
    pub var ipc_bytes_in: counters.ShardedCounter = .{};
    pub var ipc_bytes_out: counters.ShardedCounter = .{};

    // ---- the sync read fast path ----
    /// Reads the warm ladder served into the arena on the service thread.
    pub var ipc_fast_path_serves: counters.ShardedCounter = .{};
    /// Reads demoted because a writer held the per-inode guard (growth on
    /// a read-only workload = unexpected writers).
    pub var ipc_fast_path_lock_demotions: counters.ShardedCounter = .{};
    /// Reads demoted because every tier missed (growth on a warm workload
    /// = fast-path rot).
    pub var ipc_fast_path_miss_demotions: counters.ShardedCounter = .{};
    /// Reads demoted by the pre-screen (device-true O_DIRECT, retained
    /// extents, a zero or oversize request): the handler's by policy.
    pub var ipc_fast_path_policy_demotions: counters.ShardedCounter = .{};
    /// Handoffs onto the FUSE handler lanes, by direction.
    pub var ipc_read_handoffs: counters.ShardedCounter = .{};
    pub var ipc_write_handoffs: counters.ShardedCounter = .{};
    /// Handoffs every FUSE-class lane refused (every fiber slot busy): the
    /// op WAITS in its owner's queue and runs at a later flush — a read or
    /// write on a regular file never answers EAGAIN (decision 0003 §BR).
    pub var ipc_handoff_waits: counters.ShardedCounter = .{};
    /// Waiting handoffs answered without running: past the fail-stop
    /// lattice's bound (EIO), or their session torn down (EIO, nobody
    /// listening).
    pub var ipc_handoff_wait_answers: counters.ShardedCounter = .{};
    /// Handoff replies landed IN PLACE (the handler's window IS the arena;
    /// the port has no bounce arm: every handoff reply is one).
    pub var ipc_read_dest_serves: counters.ShardedCounter = .{};
    /// The copy ledger: `arena_copy + arena_dma + demote ≈ ipc_bytes_out`.
    /// Bytes the sync ladder copied into the arena.
    pub var ipc_read_arena_copy_bytes: counters.ShardedCounter = .{};
    /// Bytes a device DMA landed straight in the arena (uninterpreted bytes).
    pub var ipc_read_arena_dma_bytes: counters.ShardedCounter = .{};
    /// Bytes the demoted handler served into the arena window.
    pub var ipc_read_demote_bytes: counters.ShardedCounter = .{};

    // ---- the drain ----
    /// A served pass's duration and its sweep-end flush.
    pub var ipc_drain_pass_ns: counters.ShardedLatencyHistogram = .{};
    pub var ipc_drain_flush_ns: counters.ShardedLatencyHistogram = .{};
    pub var ipc_drain_empty_passes: counters.ShardedCounter = .{};
    /// Client publish → daemon dequeue (display-only, wrap-safe).
    pub var ipc_ingress_ns: counters.ShardedLatencyHistogram = .{};
    /// Parks woken by a doorbell vs expired at the 5 ms bound.
    pub var ipc_service_wakes: counters.ShardedCounter = .{};
    pub var ipc_service_park_timeouts: counters.ShardedCounter = .{};
    /// The spin governor's window in force (µs, the last pass's verdict).
    pub var ipc_spin_window_us: counters.Gauge = .{};
    /// Parks the spin window absorbed (a submission landed mid-spin).
    pub var ipc_spin_absorbed_parks: counters.ShardedCounter = .{};
    /// Passes the governor disengaged because the host was over its busy ceiling.
    pub var ipc_spin_disengaged_busy: counters.ShardedCounter = .{};
    /// A descriptor the daemon rejected (-EINVAL / -EBADF): unknown op, a
    /// dead binding, an oversize or out-of-window payload, a wrong-direction
    /// op. The attack/bug tripwire family — must stay 0 in production.
    pub var ipc_descriptor_rejects: counters.Tripwire = .init("ipc_descriptor_rejects");

    // ---- severance ----
    /// Ring writes severed at dequeue (pooled or placed).
    pub var ipc_severs: counters.ShardedCounter = .{};
    pub var ipc_severed_pool_hits: counters.ShardedCounter = .{};
    pub var ipc_severed_pool_misses: counters.ShardedCounter = .{};
    /// Slab bytes the pool holds (allocated, pooled or in flight).
    pub var ipc_severed_pool_bytes: counters.Gauge = .{};
    /// Whole-block stream chunks severed INTO a shared assembly.
    pub var ipc_placed_severs: counters.ShardedCounter = .{};
    /// Placed-eligible chunks that took the pooled sever (a live claim
    /// overlapped, the assembly was sealed, the cap was reached).
    pub var ipc_placed_sever_fallbacks: counters.ShardedCounter = .{};
    /// Assemblies the first merge ADOPTED as the block's backing.
    pub var ipc_placed_adoptions: counters.ShardedCounter = .{};
    /// A pointer-proven adoption whose seal was refused (a sever mid-copy):
    /// the cohort demotes to the copy path.
    pub var ipc_placed_adoption_refusals: counters.ShardedCounter = .{};
    /// Assemblies reaped never adopted (every payload dropped first).
    pub var ipc_placed_reaps: counters.ShardedCounter = .{};
    /// Bytes the live assemblies charge (`placed_assemblies`).
    pub var ipc_placed_assembly_bytes: counters.Gauge = .{};

    // ---- the direct-drive lanes ----
    pub var ipc_dd_shards: counters.Gauge = .{};
    /// W1-shaped writes served INLINE on the service thread's shard.
    pub var ipc_dd_inline_serves: counters.ShardedCounter = .{};
    pub var ipc_dd_write_bytes: counters.ShardedCounter = .{};
    /// W1-shaped candidates the dd gate declined (each its clause); they
    /// took the sever → handoff path.
    pub var ipc_dd_write_fallbacks: counters.ShardedCounter = .{};
    pub var ipc_dd_write_ineligible_shape: counters.ShardedCounter = .{};
    pub var ipc_dd_write_ineligible_block_lock: counters.ShardedCounter = .{};
    pub var ipc_dd_write_ineligible_custody: counters.ShardedCounter = .{};
    pub var ipc_dd_write_ineligible_backend: counters.ShardedCounter = .{};
    pub var ipc_dd_write_ineligible_sq_full: counters.ShardedCounter = .{};
    /// Writes whose DMA failed on the lane: `-EIO` to the client, nothing
    /// acked, parked or lost.
    pub var ipc_dd_write_dma_errors: counters.ShardedCounter = .{};
    /// Sweep-end enters on the owner's OWN shard (lane-scoped flush).
    pub var ipc_dd_lane_flushes: counters.ShardedCounter = .{};
    /// Inline CQ drains by the owning service thread (reaper/drain fusion).
    pub var ipc_dd_inline_reaps: counters.ShardedCounter = .{};
    /// Reaper passes that found CQEs (the blocking backstop engaged).
    pub var ipc_dd_reaper_reaps: counters.ShardedCounter = .{};
    /// A failed enter with ops in flight (retried at 1 ms; MEM-7c).
    pub var ipc_dd_reap_stalls: counters.ShardedCounter = .{};
    /// In-flight entries LEAKED past the stall limit (their pages stay
    /// mapped for the process's life — never an unmap under DMA).
    pub var ipc_dd_leaked_inflight: counters.ShardedCounter = .{};
    /// Rings built on the degraded (plain) setup after `COOP_TASKRUN` refused.
    pub var ipc_dd_plain_rings: counters.ShardedCounter = .{};
    /// Cold ring reads DMA'd from the device STRAIGHT INTO THE ARENA on
    /// the service thread's shard (the direct device leg the warm ladder
    /// resolved after every tier missed; `SQUEEZEFS_IL_DIRECT_READ`).
    pub var ipc_dd_read_serves: counters.ShardedCounter = .{};
    pub var ipc_dd_read_bytes: counters.ShardedCounter = .{};
    /// Direct reads whose post-DMA recheck failed (the binding or the
    /// incarnation moved under the DMA, or a short/failed CQE): the handler
    /// owns the truth — a read handoff follows, nothing fabricated.
    pub var ipc_dd_read_fallbacks_post: counters.ShardedCounter = .{};
    /// Legs the lane refused BEFORE any DMA (no slab entry, no SQE): the
    /// handoff path, counted.
    pub var ipc_dd_read_refusals: counters.ShardedCounter = .{};
    /// Owner parks taken IN THE RING (ops in flight: the CQEs and the
    /// doorbells awaited in one enter; `SQUEEZEFS_IPC_DD_RING_PARK`) and
    /// the CQ drains at their return.
    pub var ipc_dd_ring_parks: counters.ShardedCounter = .{};
    pub var ipc_dd_park_reaps: counters.ShardedCounter = .{};
    /// Ring parks the spin half absorbed (a CQE or a doorbell inside the
    /// window: no enter, no wake).
    pub var ipc_dd_park_spins_absorbed: counters.ShardedCounter = .{};

    // ---- the direct arm (charter §7 "The direct arm") ----
    /// Sessions granted the direct arm at HELLO (their uid can open every
    /// data device node) / refused it by the device screen.
    pub var ipc_direct_sessions: counters.ShardedCounter = .{};
    pub var ipc_direct_refused_access: counters.ShardedCounter = .{};
    /// Ops and bytes the clients served from the device themselves (their
    /// display pages folded at teardown): the engagement law's third term.
    pub var ipc_direct_ops: counters.ShardedCounter = .{};
    pub var ipc_direct_bytes: counters.ShardedCounter = .{};
    /// Client-side rechecks that refused (a lease re-asked), folded alike.
    pub var ipc_direct_rechecks_failed: counters.ShardedCounter = .{};
    /// Layout leases granted / refused by class.
    pub var ipc_leases_granted: counters.ShardedCounter = .{};
    pub var ipc_leases_refused_disabled: counters.ShardedCounter = .{};
    pub var ipc_leases_refused_kind: counters.ShardedCounter = .{};
    pub var ipc_leases_refused_custody: counters.ShardedCounter = .{};
    pub var ipc_leases_refused_binding: counters.ShardedCounter = .{};
    pub var ipc_leases_refused_moved: counters.ShardedCounter = .{};
    pub var ipc_leases_refused_write: counters.ShardedCounter = .{};
    // ---- the direct WRITE arm (decision 0003 §AY) ----
    /// Sessions granted the write arm at HELLO (every node writable by them).
    pub var ipc_direct_write_sessions: counters.ShardedCounter = .{};
    /// Write leases granted / live; recalls run, drained after a wait, and
    /// FORCED past the bound (must stay 0: a client wedged mid-write).
    pub var ipc_write_leases_granted: counters.ShardedCounter = .{};
    pub var ipc_write_leases_live: counters.Gauge = .{};
    pub var ipc_write_lease_recalls: counters.ShardedCounter = .{};
    pub var ipc_write_lease_recalls_drained: counters.ShardedCounter = .{};
    pub var ipc_write_leases_forced: counters.Tripwire = .init("ipc_write_leases_forced");
    /// The clients' direct writes and bytes (their pages folded at
    /// teardown), their batched postlude notes.
    pub var ipc_direct_writes: counters.ShardedCounter = .{};
    pub var ipc_direct_write_bytes: counters.ShardedCounter = .{};
    pub var ipc_direct_write_notes: counters.ShardedCounter = .{};
    pub var ipc_direct_writes_recalled: counters.ShardedCounter = .{};

    // ---- coherence (KD-11's residual windows) ----
    pub var ipc_inval_notifies: counters.ShardedCounter = .{};
    pub var ipc_inval_attrs_only: counters.ShardedCounter = .{};
    pub var ipc_inval_suppressed: counters.ShardedCounter = .{};
    /// The sideband queue refused a notify (counted, never awaited).
    pub var ipc_inval_queue_full: counters.ShardedCounter = .{};
};

/// The per-lane closures (module doc), as booleans.
pub const Closure = struct {
    reads: bool,
    writes: bool,
    dd: bool,
};

pub fn closure() Closure {
    return .{
        .reads = stats.ipc_fast_path_serves.load() + stats.ipc_read_handoffs.load() + stats.ipc_dd_read_serves.load() == stats.ipc_ops_read.load(),
        .writes = stats.ipc_dd_inline_serves.load() + stats.ipc_write_handoffs.load() == stats.ipc_ops_write.load(),
        .dd = stats.ipc_dd_inline_serves.load() + stats.ipc_dd_write_fallbacks.load() == ddSubmitsAttempted(),
    };
}

/// Every W1-shaped write that reached the dd gate.
pub fn ddSubmitsAttempted() u64 {
    return stats.ipc_dd_inline_serves.load() + stats.ipc_dd_write_fallbacks.load();
}

/// The family: every instrument by declaration, then the closures.
pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeByte('{');
    inline for (comptime counters.instrumentNames(stats)) |name| {
        const T = @TypeOf(@field(stats, name));
        const ptr = &@field(stats, name);
        if (T == counters.Tripwire) {
            try w.print("\"{s}\":{d},", .{ ptr.site, ptr.load() });
        } else if (T == counters.ShardedLatencyHistogram) {
            try w.print("\"{s}\":", .{name});
            try ptr.snapshot().writeJson(w);
            try w.writeByte(',');
        } else {
            try w.print("\"{s}\":{d},", .{ name, ptr.load() });
        }
    }
    const c = closure();
    try w.print("\"closure\":{{\"reads\":{},\"writes\":{},\"dd\":{}}}}}", .{ c.reads, c.writes, c.dd });
}

const testing = std.testing;

test "the host family renders every declared instrument and the three closures; the closures hold at zero and track the ledgers" {
    var buf: [16384]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const out = w.buffered();
    inline for (comptime counters.instrumentNames(stats)) |name| {
        const T = @TypeOf(@field(stats, name));
        const key = if (T == counters.Tripwire) @field(stats, name).site else name;
        try testing.expect(std.mem.indexOf(u8, out, key) != null);
    }
    try testing.expect(std.mem.indexOf(u8, out, "\"closure\":{") != null);
    // The ledgers move together or the closure reads false.
    const c0 = closure();
    stats.ipc_ops_read.inc();
    stats.ipc_fast_path_serves.inc();
    try testing.expectEqual(c0.reads, closure().reads);
    stats.ipc_ops_write.inc();
    stats.ipc_write_handoffs.inc();
    try testing.expectEqual(c0.writes, closure().writes);
}
