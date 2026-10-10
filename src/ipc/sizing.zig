//! The IL session's sizing derivations (charter §1.5, §7; spec
//! `docs/porting/il_session.md` §6): every default a pure function of
//! `derive.Facts` and the budget, each with a tie test that restates the
//! formula and the measured number beside it. The arena family (cap,
//! population, arena, per-uid cap) already lives in `mem/sizing.zig` and is
//! composed here; the batch-wake threshold `k` lives beside the doorbell it
//! governs (`doorbell.reapBatchWakeThreshold`).

const std = @import("std");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const clock = @import("../core/clock.zig");
const mem_sizing = @import("../mem/sizing.zig");
const layout = @import("layout.zig");

const KiB = derive.KiB;
const MiB = derive.MiB;

// ---------------------------------------------------------------------------
// 1. Sessions per (process, mount) — the shim's and the SDK's K.
// ---------------------------------------------------------------------------

/// `clamp(ceil(cpus/4), 2, 16)`. WHY cpus/4: the drain-thread saturation
/// slope of the 2026-07-19 service sweep; the 2026-07-28 field capture
/// convicted a DEFAULTS MISMATCH (shim flat 4 sessions vs daemon 8 threads:
/// svc0..3 at 94–99 %, svc4..7 idle; sessions=8 by hand recovered +44 %,
/// 7.5 → 10.8 GB/s against a 16.6 GB/s raw ceiling). Rounds UP (a
/// fractional share derives the next whole unit). Floor 2 = the single-
/// dequeue-thread plateau; ceiling 16 = the shim's static session table
/// (32) over two concurrently-bound mounts.
pub fn ilSessionsDefault(cpus: u32) u32 {
    return @intCast(derive.clamp(derive.ceilDiv(cpus, 4), mem_sizing.il_sessions_per_mount_floor, 16));
}

/// `SQUEEZEFS_IL_SESSIONS` verbatim (1..=16) else derived.
pub fn resolveIlSessions(explicit: ?u32, cpus: u32) u32 {
    return explicit orelse ilSessionsDefault(cpus);
}

pub fn ilSessionsFromEnv(env: *const std.process.Environ.Map, cpus: u32) u32 {
    return resolveIlSessions(knobs.optIntKnob("SQUEEZEFS_IL_SESSIONS", u32, env), cpus);
}

// ---------------------------------------------------------------------------
// 2–3. Arena and admission cap (composed from mem/sizing).
// ---------------------------------------------------------------------------

/// `budget / 8` (charter §7 "admission cap `budget/8`").
pub const ipcArenaCap = mem_sizing.ipcArenaCap;
pub const resolveIpcArenaCap = mem_sizing.resolveIpcArenaCap;
/// `max(64 MiB, align_down(cap / population, 4 MiB))` (charter §7 "arena
/// `max(64 MiB, cap/population)`") where the cap holds the population
/// floor (`2 × cpus` sessions) at it; the arena yields to the floor on a
/// bounded budget (decision 0003 §H).
pub const resolveIpcArenaBytes = mem_sizing.resolveIpcArenaBytes;
pub const ipcSessionPopulationTarget = mem_sizing.ipcSessionPopulationTarget;
pub const ipcSessionPopulationFloor = mem_sizing.ipcSessionPopulationFloor;
pub const ipcAdmittedPopulation = mem_sizing.ipcAdmittedPopulation;
pub const ipcPerUidSessionCap = mem_sizing.ipcPerUidSessionCap;

/// The host's face: `SQUEEZEFS_IPC_ARENA_MB` verbatim else the derived
/// arena, the default geometry's overhead charged per session.
pub fn ipcArenaBytesFromEnv(env: *const std.process.Environ.Map, arena_cap_bytes: u64, cpus: u32) u64 {
    return mem_sizing.ipcArenaBytesFromEnv(env, arena_cap_bytes, cpus, layout.DEFAULT_SESSION_OVERHEAD_BYTES);
}

// ---------------------------------------------------------------------------
// 4. The hybrid lane gate.
// ---------------------------------------------------------------------------

/// The D14 field bracket: ring 594 k vs kernel ≈ 253 k IOPS rand-4k ⇒
/// `1/253k − 1/594k ≈ 2.27 µs` — a MEASURED class constant, not tuning.
pub const KERNEL_LANE_RTT_DELTA_NS: u64 = 2300;
/// Daemon serve-into-arena + client arena→app. Writes pay one copy; their
/// crossover is 2× and collapses to the same floor.
pub const KERNEL_LANE_RING_COPIES: u64 = 2;

/// `size* = memBW × Δrtt / copies`, rounded DOWN to the 4 KiB grain, floored
/// at the slab (at or below it a ring op is the single-flight allocation-
/// free path the gate exists to protect), capped at `max_op` (past it an op
/// must chunk into multiple flights). Strictly-greater routes kernel.
/// Values: 15 GB/s → 64 KiB (the slab floor); 100 GB/s → 112 KiB; the
/// charter's "≈ 140–164 KiB" corresponds to probed 125–147 GB/s clients.
pub fn kernelLaneMinDefault(membw_bytes_per_s: u64, slab: u64, max_op: u64) u64 {
    const raw: u64 = @intCast(@as(u128, membw_bytes_per_s) * KERNEL_LANE_RTT_DELTA_NS / std.time.ns_per_s / KERNEL_LANE_RING_COPIES);
    const grained = derive.alignDown(raw, layout.PAGE_BYTES);
    return derive.clamp(grained, slab, max_op);
}

/// `SQUEEZEFS_IL_KERNEL_LANE_MIN` verbatim (0 = off: every op rings) else
/// derived from the probed bandwidth.
pub fn resolveKernelLaneMin(explicit: ?u64, membw_bytes_per_s: u64, slab: u64, max_op: u64) u64 {
    return explicit orelse kernelLaneMinDefault(membw_bytes_per_s, slab, max_op);
}

pub fn kernelLaneMinFromEnv(env: *const std.process.Environ.Map, membw_bytes_per_s: u64, slab: u64, max_op: u64) u64 {
    return resolveKernelLaneMin(knobs.optIntKnob("SQUEEZEFS_IL_KERNEL_LANE_MIN", u64, env), membw_bytes_per_s, slab, max_op);
}

/// The memBW probe's buffer: 64 MiB is above every target part's last-level
/// cache (the largest is ~32 MiB per die on the field parts), so the figure
/// is DRAM-class bandwidth, not a cache hit; one copy costs ≈ 5–10 ms.
pub const MEMBW_PROBE_BYTES: usize = 64 * MiB;
/// The floor a degenerate probe (a 0 ns clock delta) resolves to: the 15
/// GB/s row, which maps to the slab floor anyway.
pub const MEMBW_PROBE_FLOOR_BYTES_PER_S: u64 = 15_000_000_000;

var membw_cache: std.atomic.Value(u64) = .init(0);

/// Bytes/s of one `memcpy` of `MEMBW_PROBE_BYTES`, timed ONCE per process
/// (memoised; interference only slows the figure, which only lowers the
/// gate — the safe direction: more ops ring). Run at the first establish,
/// never in a constructor; the probe allocates and frees its own buffers.
pub fn probeMemBandwidth(gpa: std.mem.Allocator) std.mem.Allocator.Error!u64 {
    const cached = membw_cache.load(.monotonic);
    if (cached != 0) return cached;
    const src = try gpa.alloc(u8, MEMBW_PROBE_BYTES);
    defer gpa.free(src);
    const dst = try gpa.alloc(u8, MEMBW_PROBE_BYTES);
    defer gpa.free(dst);
    // Touch both so the copy measures bandwidth, not page faults.
    @memset(src, 0x5a);
    @memset(dst, 0);
    const t0 = clock.monotonicNs();
    @memcpy(dst, src);
    const dt = clock.monotonicNs() - t0;
    std.mem.doNotOptimizeAway(dst[MEMBW_PROBE_BYTES - 1]);
    // Both sides of the copy move through the memory system: 2 × bytes.
    const bw: u64 = if (dt == 0) MEMBW_PROBE_FLOOR_BYTES_PER_S else @intCast(@as(u128, 2 * MEMBW_PROBE_BYTES) * std.time.ns_per_s / dt);
    const resolved = @max(bw, MEMBW_PROBE_FLOOR_BYTES_PER_S);
    membw_cache.store(resolved, .monotonic);
    return resolved;
}

// ---------------------------------------------------------------------------
// 5. The slab (the ONE law lives on the geometry).
// ---------------------------------------------------------------------------

pub const slotSlab = layout.Geometry.slotSlab;

/// The per-op ceiling: `SQUEEZEFS_IPC_MAX_OP_BYTES` verbatim else the
/// transport's `max_write` desire `max(block_size, 1 MiB)` — ONE per-op
/// ceiling for both lanes (decision 0002 ledger Q14).
pub fn resolveMaxOpBytes(explicit: ?u64, block_size: u64) u64 {
    return explicit orelse @max(block_size, layout.DEFAULT_MAX_OP_BYTES);
}

pub fn maxOpBytesFromEnv(env: *const std.process.Environ.Map, block_size: u64) u64 {
    return resolveMaxOpBytes(knobs.optIntKnob("SQUEEZEFS_IPC_MAX_OP_BYTES", u64, env), block_size);
}

// ---------------------------------------------------------------------------
// 6. Service threads = direct-drive shards.
// ---------------------------------------------------------------------------

/// `clamp(ceil(3·cpus/8), 2, 64)` — ONE function sizes both halves (charter
/// §7). WHY 3/8: the counted 2026-08-06 width sweep (32 CPUs / 2 nodes, il
/// rand-4k 32×qd32, 3×30 s per width, W8 brackets): W8 622–636 k → W12
/// 695–700 k (+10.4 %, best clat) → W16 675–690 k → W24 616–626 k (the one
/// regression). One lane = 2 OS threads (`sqz-ipc-svcN` + `sqz-ipc-ddN`), so
/// the grid is `2W/cpus ∈ {0.5, 0.75, 1.0, 1.5}`: the optimum is ¾ of the
/// core budget (the complement left to the co-located client fleet) and the
/// one point past 1.0× is the regression. Floor 2; 64 = explicit-lever
/// parity (engages ≥ 171 cpus). Dominance `ceil(3c/8) ≥ ceil(c/4)` pointwise,
/// so every default session owns its own drain thread.
pub fn ilDrainLanesDefault(cpus: u32) u32 {
    return @intCast(derive.clamp(derive.ceilDiv(3 * @as(u64, cpus), 8), 2, 64));
}

/// `SQUEEZEFS_IPC_SERVICE_THREADS` verbatim (1..=64) else derived.
pub fn resolveDrainLanes(explicit: ?u32, cpus: u32) u32 {
    return explicit orelse ilDrainLanesDefault(cpus);
}

pub fn drainLanesFromEnv(env: *const std.process.Environ.Map, cpus: u32) u32 {
    return resolveDrainLanes(knobs.optIntKnob("SQUEEZEFS_IPC_SERVICE_THREADS", u32, env), cpus);
}

// ---------------------------------------------------------------------------
// Ctl bounds (spec §6.7): connections, not threads (decision 0002 ledger Q10).
// ---------------------------------------------------------------------------

/// `clamp(2 × cap / footprint, 32, 4096)`: twice the session population
/// the cap admits (a HELLO in flight per admitted session), railed.
pub fn ctlConnCap(arena_cap_bytes: u64, footprint_bytes: u64) u32 {
    return @intCast(derive.clamp(2 * (arena_cap_bytes / @max(footprint_bytes, 1)), 32, 4096));
}

/// `clamp(ctl_cap, 64, 4096)`: the hardcoded 64 sat at ¼ of a 256-process
/// fleet, and a full SEQPACKET backlog makes `connect` BLOCK — part of the
/// measured 150–190 ms per-process launch convoy.
pub fn listenBacklog(ctl_cap: u32) u32 {
    return @intCast(derive.clamp(ctl_cap, 64, 4096));
}

// ---------------------------------------------------------------------------
// Tie tests (spec §8 "Sizing ties").
// ---------------------------------------------------------------------------

const testing = std.testing;

test "il_sessions_default = clamp(ceil(cpus/4), 2, 16): the spec's values; the explicit knob wins verbatim" {
    const rows = [_][2]u32{ .{ 1, 2 }, .{ 8, 2 }, .{ 9, 3 }, .{ 16, 4 }, .{ 22, 6 }, .{ 32, 8 }, .{ 64, 16 }, .{ 256, 16 } };
    for (rows) |r| try testing.expectEqual(r[1], ilSessionsDefault(r[0]));
    var c: u32 = 1;
    while (c <= 4096) : (c += 1) try testing.expectEqual(@as(u32, @intCast(derive.clamp((c + 3) / 4, 2, 16))), ilSessionsDefault(c));
    try testing.expectEqual(@as(u32, 3), resolveIlSessions(3, 256));
    try testing.expectEqual(@as(u32, 8), resolveIlSessions(null, 32));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u32, 8), ilSessionsFromEnv(&env, 32));
    try env.put("SQUEEZEFS_IL_SESSIONS", "16");
    try testing.expectEqual(@as(u32, 16), ilSessionsFromEnv(&env, 4));
}

test "drain_lane_derivation_values = clamp(ceil(3·cpus/8), 2, 64); the sweep's W12 at 32 cpus; 64 engages at 171 cpus; the explicit knob wins" {
    const rows = [_][2]u32{ .{ 1, 2 }, .{ 4, 2 }, .{ 8, 3 }, .{ 16, 6 }, .{ 32, 12 }, .{ 64, 24 }, .{ 170, 64 }, .{ 171, 64 }, .{ 256, 64 } };
    for (rows) |r| try testing.expectEqual(r[1], ilDrainLanesDefault(r[0]));
    try testing.expectEqual(@as(u32, 64), ilDrainLanesDefault(169));
    try testing.expectEqual(@as(u32, 63), ilDrainLanesDefault(168));
    try testing.expectEqual(@as(u32, 5), resolveDrainLanes(5, 256));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(@as(u32, 12), drainLanesFromEnv(&env, 32));
    try env.put("SQUEEZEFS_IPC_SERVICE_THREADS", "1");
    try testing.expectEqual(@as(u32, 1), drainLanesFromEnv(&env, 32));
}

test "drain lanes dominate sessions pointwise (1..=4096): every default session owns its own drain thread" {
    var c: u32 = 1;
    while (c <= 4096) : (c += 1) try testing.expect(ilDrainLanesDefault(c) >= ilSessionsDefault(c));
}

test "kernel_lane_min = clamp(round_down(memBW × 2.3 µs / 2, 4 KiB), slab, max_op): the spec's values; 0 = off verbatim" {
    const slab: u64 = 64 * KiB;
    const max_op: u64 = 1 * MiB;
    try testing.expectEqual(@as(u64, 2300), KERNEL_LANE_RTT_DELTA_NS);
    try testing.expectEqual(@as(u64, 2), KERNEL_LANE_RING_COPIES);
    try testing.expectEqual(64 * KiB, kernelLaneMinDefault(15_000_000_000, slab, max_op));
    try testing.expectEqual(112 * KiB, kernelLaneMinDefault(100_000_000_000, slab, max_op));
    // The charter's ≈ 140–164 KiB band at probed 125–147 GB/s.
    try testing.expectEqual(140 * KiB, kernelLaneMinDefault(125_000_000_000, slab, max_op));
    try testing.expectEqual(164 * KiB, kernelLaneMinDefault(147_000_000_000, slab, max_op));
    // The ceiling: an absurd bandwidth caps at max_op.
    try testing.expectEqual(max_op, kernelLaneMinDefault(10_000_000_000_000, slab, max_op));
    // 4 KiB grain, rounded DOWN.
    try testing.expectEqual(@as(u64, 0), kernelLaneMinDefault(123_456_789_012, slab, max_op) % 4096);
    try testing.expectEqual(@as(u64, 0), resolveKernelLaneMin(0, 100_000_000_000, slab, max_op));
    try testing.expectEqual(@as(u64, 8192), resolveKernelLaneMin(8192, 100_000_000_000, slab, max_op));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(112 * KiB, kernelLaneMinFromEnv(&env, 100_000_000_000, slab, max_op));
    try env.put("SQUEEZEFS_IL_KERNEL_LANE_MIN", "0");
    try testing.expectEqual(@as(u64, 0), kernelLaneMinFromEnv(&env, 100_000_000_000, slab, max_op));
}

test "the memBW probe: one 64 MiB copy, memoised, floored at 15 GB/s, plausible on any host" {
    try testing.expectEqual(@as(usize, 64 * MiB), MEMBW_PROBE_BYTES);
    const a = try probeMemBandwidth(testing.allocator);
    const b = try probeMemBandwidth(testing.allocator);
    try testing.expectEqual(a, b);
    try testing.expect(a >= MEMBW_PROBE_FLOOR_BYTES_PER_S);
    // No host moves more than 10 TB/s through one core's memcpy.
    try testing.expect(a < 10_000_000_000_000);
}

test "slot_slab_is_dma_aligned: the ONE law reached through sizing is the geometry's" {
    try testing.expectEqual(@as(u64, 64 * KiB), slotSlab(layout.Geometry.default));
    const odd: layout.Geometry = .{ .ring_entries = 1024, .slots = 1024, .arena_bytes = 90 * MiB, .max_op_bytes = 1 << 20 };
    try testing.expectEqual(@as(u64, 0), slotSlab(odd) % layout.PAGE_BYTES);
}

test "max_op_bytes = explicit else max(block_size, 1 MiB) — the transport's ceiling is the ring's" {
    try testing.expectEqual(1 * MiB, resolveMaxOpBytes(null, 4096));
    try testing.expectEqual(4 * MiB, resolveMaxOpBytes(null, 4 * MiB));
    try testing.expectEqual(@as(u64, 65536), resolveMaxOpBytes(65536, 4 * MiB));
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(1 * MiB, maxOpBytesFromEnv(&env, 4096));
    try env.put("SQUEEZEFS_IPC_MAX_OP_BYTES", "8192");
    try testing.expectEqual(@as(u64, 8192), maxOpBytesFromEnv(&env, 4096));
}

test "ctl bounds: conn cap = clamp(2 × cap/footprint, 32, 4096); backlog = clamp(cap, 64, 4096)" {
    const footprint: u64 = 64 * MiB + 0x25000;
    try testing.expectEqual(@as(u32, 2 * (22 * derive.GiB / footprint)), ctlConnCap(22 * derive.GiB, footprint));
    try testing.expectEqual(@as(u32, 32), ctlConnCap(64 * MiB, footprint));
    try testing.expectEqual(@as(u32, 4096), ctlConnCap(1 << 50, footprint));
    try testing.expectEqual(@as(u32, 64), listenBacklog(32));
    try testing.expectEqual(@as(u32, 702), listenBacklog(702));
    try testing.expectEqual(@as(u32, 4096), listenBacklog(4096));
}

test "the arena family reached through sizing is mem/sizing's: cap budget/8, arena max(64 MiB, cap/population); the overhead charged is the default layout's bytes before the arena" {
    const cap = ipcArenaCap(176 * derive.GiB);
    try testing.expectEqual(22 * derive.GiB, cap);
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, cap, 32, layout.DEFAULT_SESSION_OVERHEAD_BYTES));
    try testing.expectEqual(@as(u64, 256), ipcSessionPopulationTarget(32));
    try testing.expectEqual(@as(u64, 256), ipcPerUidSessionCap(cap, 88 * MiB));
    try testing.expectEqual(@as(u64, 0x25000), layout.DEFAULT_SESSION_OVERHEAD_BYTES);
    const l = try layout.SessionLayout.compute(layout.Geometry.default);
    try testing.expectEqual(l.arena_off, layout.DEFAULT_SESSION_OVERHEAD_BYTES);
    try testing.expectEqual(l.total_bytes - l.arena_bytes, layout.DEFAULT_SESSION_OVERHEAD_BYTES);
    var env = std.process.Environ.Map.init(testing.allocator);
    defer env.deinit();
    try testing.expectEqual(88 * MiB, ipcArenaBytesFromEnv(&env, cap, 32));
    try testing.expectEqual(12 * MiB, ipcArenaBytesFromEnv(&env, ipcArenaCap(2 * derive.GiB), 9));
}

test "the HELLO ladder on a bounded budget admits the population floor: 9 cpus at a 2 GiB budget (the venue's measured shape — 3 sessions before) admit ≥ 2 × cpus sessions with the derived geometry; the field shape still admits ≥ 2 × 32" {
    const rendezvous = @import("rendezvous.zig");
    const wire = @import("wire.zig");
    const Shape = struct { cpus: u32, budget: u64 };
    for ([_]Shape{ .{ .cpus = 9, .budget = 2 * derive.GiB }, .{ .cpus = 9, .budget = 6144 * MiB }, .{ .cpus = 32, .budget = 176 * derive.GiB }, .{ .cpus = 2, .budget = 2 * derive.GiB + 820 * MiB } }) |shape| {
        const cap = ipcArenaCap(shape.budget);
        const arena = resolveIpcArenaBytes(null, cap, shape.cpus, layout.DEFAULT_SESSION_OVERHEAD_BYTES);
        var geometry = layout.Geometry.default;
        geometry.arena_bytes = arena;
        geometry.max_op_bytes = @intCast(@min(layout.DEFAULT_MAX_OP_BYTES, arena));
        var p: rendezvous.Policy = .{ .armed = true, .our_commit = "0123456789ab", .default_geometry = geometry, .arena_cap_bytes = cap, .per_uid_session_cap = ipcPerUidSessionCap(cap, arena) };
        const nonces = rendezvous.NonceState.init(&testNonceFill, 0);
        var h: wire.Hello = .{};
        try wire.setField(&h.build_commit, "0123456789ab");
        h.nonce = nonces.current;
        const facts: rendezvous.FdFacts = .{ .flags = .{ .accmode = .rdwr }, .stat = .{ .kind = .regular, .dev = 77, .ino = 1234, .nlink = 1 } };
        const peer: rendezvous.PeerCred = .{ .pid = 1, .uid = 1000, .gid = 1000 };
        var admitted: u64 = 0;
        while (admitted < 4096) {
            switch (rendezvous.admitHello(p, &h, facts, 77, peer, &nonces)) {
                .admit => |a| {
                    try testing.expectEqual(arena, a.geometry.arena_bytes);
                    p.arena_in_use_bytes += a.footprint_bytes;
                    p.uid_sessions += 1;
                    admitted += 1;
                },
                .refuse => |class| {
                    try testing.expectEqual(wire.RefuseClass.budget, class);
                    break;
                },
            }
        }
        try testing.expect(admitted >= ipcSessionPopulationFloor(shape.cpus));
        try testing.expectEqual(ipcAdmittedPopulation(cap, arena + layout.DEFAULT_SESSION_OVERHEAD_BYTES), admitted);
    }
}

fn testNonceFill(n: *rendezvous_mod.Nonce) void {
    for (n, 0..) |*b, i| b.* = @truncate(i * 7 + 1);
}

const rendezvous_mod = @import("rendezvous.zig");
