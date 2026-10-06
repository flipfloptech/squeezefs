//! R5-derived sizing laws (spec `docs/porting/mem_budget.md` §4 helpers;
//! charter §1.5 "Every constant is DERIVED"). Every cap here is a pure
//! function of the resolved budget (and the core count); the family
//! resolvers take knob values the ONE parser (`core/knobs.zig`) already
//! admitted — the gate refused a fraction or `> 100` (§1.6: never clamps),
//! so there is no second parser and nothing to warn about here — and apply
//! `explicit > percentage > derived`.

const std = @import("std");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");

const MiB = derive.MiB;
const GiB = derive.GiB;

/// Shipped per-session IPC arena, the derived default's floor
/// (never-regress-below-shipped: every box ran 64 MiB sessions).
pub const ipc_arena_floor_bytes: u64 = 64 * MiB;
/// Slot-slab DMA law: 1024 slots × the 4 KiB O_DIRECT LBA. An odd 2 MiB
/// multiple makes the slab ≡ 2048 mod 4096, so EXACTLY half of all
/// round-robin direct-drive reads bounce (measured −15.4 %, bounce rate
/// 50.006 % where 0 was expected).
pub const ipc_arena_dma_align_bytes: u64 = 4 * MiB;
/// External instrument dims (DDN exa-client kit: libaio, direct=1, qd=8) —
/// an instrument fact like a wire format, never tuning.
pub const exa_canon_qd: u64 = 8;
/// FUSE-over-io_uring per-queue depth floor: the shipped posture (charter
/// §4 Geometry, `clamp(cap / (nqueues × payload), 4, 32)`).
pub const transport_q_depth_floor: u64 = 4;
/// FUSE-over-io_uring payload base: the 1 MiB `max_write` floor (charter §4).
pub const transport_payload_base: u64 = 1 * MiB;

/// Transport payload-buffer cap = budget / 8. No absolute ceiling: the former
/// fixed 2 GiB degraded depth below the measured-best 32 on > 64-possible-CPU
/// big-RAM boxes for no physical reason; the pinned-arena bound is structural
/// (`nqueues × Q_DEPTH_DESIRED × payload_sz`).
pub fn transportBufferCap(budget: u64) u64 {
    return budget / 8;
}

/// ipc_session_arenas admission cap = budget / 8 (12.5 %), the same
/// scale-free fraction. No ceiling: on the 251 GB field box a fixed 2 GiB
/// clamped the pool to ~31 × 64 MiB sessions against a 48-HELLO fleet
/// (ipc_bind_refused_budget 13/48).
pub fn ipcArenaCap(budget: u64) u64 {
    return budget / 8;
}

/// `SQUEEZEFS_TRANSPORT_MEM_MAX` (MiB, verbatim) > `_PCT` (1..=100, percent
/// of budget) > budget / 8.
pub fn resolveTransportBufferCap(budget: u64, mem_max_mib: ?u64, mem_pct: ?u64) u64 {
    return resolveCapFamily(budget, mem_max_mib, mem_pct);
}

/// `SQUEEZEFS_IPC_MEM_MAX` > `SQUEEZEFS_IPC_MEM_PCT` > budget / 8; same law
/// as the transport family.
pub fn resolveIpcArenaCap(budget: u64, mem_max_mib: ?u64, mem_pct: ?u64) u64 {
    return resolveCapFamily(budget, mem_max_mib, mem_pct);
}

/// `v × pct / 100`, widened: the integer percentage of a budget (multiply
/// first so a 12.5 %-class fraction is impossible by construction and a 3 %
/// of 4096 rounds like the reference's float form, 122).
fn pctOf(v: u64, pct: u64) u64 {
    return @intCast(@as(u128, v) * pct / 100);
}

fn resolveCapFamily(budget: u64, mem_max_mib: ?u64, mem_pct: ?u64) u64 {
    if (mem_max_mib) |mib| return mib *| MiB;
    if (mem_pct) |pct| return pctOf(budget, pct);
    return budget / 8;
}

/// The IPC session population the admission cap is sized to hold:
/// `max(128, cpus × EXA_CANON_QD)`. The slope is the matched-inflight
/// client-fleet shape that refused twice (32 cpus ⇒ 256 fio processes
/// against the ~128-session budget the retired /128 fraction implied:
/// ipc_admission_refusals ~125/pass, engagement 0.697 ⇒ INVALID row); the
/// floor 128 keeps small boxes byte-identical to the shipped cap/128.
pub fn ipcSessionPopulationTarget(cpus: u32) u64 {
    return @max(128, @as(u64, cpus) * exa_canon_qd);
}

/// Per-session arena: `SQUEEZEFS_IPC_ARENA_MB` (verbatim MiB; the row's
/// range starts at 1) else `max(64 MiB, align_down(cap /
/// population_target(cpus), 4 MiB))` — so a full population of default
/// arenas fills the admission cap exactly.
pub fn resolveIpcArenaBytes(arena_mib: ?u64, arena_cap_bytes: u64, cpus: u32) u64 {
    if (arena_mib) |mib| return mib *| MiB;
    const derived = derive.alignDown(arena_cap_bytes / ipcSessionPopulationTarget(cpus), ipc_arena_dma_align_bytes);
    return @max(ipc_arena_floor_bytes, derived);
}

/// Per-uid concurrent session cap = `clamp(cap / max(arena, 1), 64, 4096)`:
/// the population the admission cap can actually hold. Floor 64 = the
/// shipped posture (it refused half of a 131-session budget: 22.5 GiB over
/// 176 MiB arenas); rail 4096 = the ctl-thread exhaustion rail (one OS
/// thread per admitted session).
pub fn ipcPerUidSessionCap(arena_cap_bytes: u64, arena_bytes: u64) u64 {
    return derive.clamp(arena_cap_bytes / @max(arena_bytes, 1), 64, 4096);
}

/// Dentry-class entry capacity: one entry per 200 KB of (shared) RAM,
/// floored at the shipped 50 000.
pub fn dirEntryCapacity(total_memory: u64) u64 {
    return @max(50_000, total_memory / 200_000);
}

const refusal_template = "fleet share {d} (squeezefs daemons in this cgroup): the divided derived memory budget {d} B cannot satisfy the never-divided transport payload floor {d} B (= {d} kernel possible CPUs × Q_DEPTH_FLOOR {d} × {d} B payload — kernel-mandated queue geometry, fleet-share EXEMPT). Floors hold per daemon regardless of share, so this many daemons oversubscribe the machine instead of dividing it: run fewer daemons in this cage, or set an explicit SQUEEZEFS_MEM_BUDGET_MB (absolute overrides win verbatim and own their own arithmetic)";
/// Six `{d}` of at most 20 digits each on top of the template.
pub const RefusalBuf = [refusal_template.len + 6 * 20]u8;

/// Floors are never divided, so a share whose divided derived budget cannot
/// hold the kernel-mandated transport floor (`possible_cpus × Q_DEPTH_FLOOR
/// × PAYLOAD_BASE` — the queue count cannot divide, depth floors at 4)
/// refuses the mount naming the arithmetic. Share 1 never refuses: the solo
/// posture is the shipped never-regress behaviour.
pub fn fleetShareFloorRefusal(share: u32, divided_budget: u64, possible_cpus: u32, buf: *RefusalBuf) ?[]const u8 {
    if (share <= 1) return null;
    const floor = @as(u64, possible_cpus) * transport_q_depth_floor * transport_payload_base;
    if (divided_budget >= floor) return null;
    // `RefusalBuf` is sized to the template plus every digit the six
    // arguments can produce, so the print cannot run out of space.
    return std.fmt.bufPrint(buf, refusal_template, .{ share, divided_budget, floor, possible_cpus, transport_q_depth_floor, transport_payload_base }) catch unreachable;
}

// ---- knob faces (the rows' ranges are enforced by the readers) ----

pub fn transportBufferCapFromEnv(env: *const std.process.Environ.Map, budget: u64) u64 {
    return resolveTransportBufferCap(budget, knobs.optIntKnob("SQUEEZEFS_TRANSPORT_MEM_MAX", u64, env), knobs.optIntKnob("SQUEEZEFS_TRANSPORT_MEM_PCT", u64, env));
}

pub fn ipcArenaCapFromEnv(env: *const std.process.Environ.Map, budget: u64) u64 {
    return resolveIpcArenaCap(budget, knobs.optIntKnob("SQUEEZEFS_IPC_MEM_MAX", u64, env), knobs.optIntKnob("SQUEEZEFS_IPC_MEM_PCT", u64, env));
}

pub fn ipcArenaBytesFromEnv(env: *const std.process.Environ.Map, arena_cap_bytes: u64, cpus: u32) u64 {
    return resolveIpcArenaBytes(knobs.optIntKnob("SQUEEZEFS_IPC_ARENA_MB", u64, env), arena_cap_bytes, cpus);
}

// ---------------------------------------------------------------------------
// Tie tests (spec §9.3 derivation_sweep): each restates its formula.
// ---------------------------------------------------------------------------

const testing = std.testing;

/// 251 GB RAM × 70 % ≈ 176 GiB: the field shape every tie is pinned on.
const field_budget: u64 = 176 * GiB;
/// 4 GiB RAM × 70 % = 2.8 GiB: the floor shape.
const floor_budget: u64 = 2 * GiB + 820 * MiB;

test "transport_buffer_cap = budget / 8, no fixed ceiling" {
    try testing.expectEqual((6 * GiB + 410 * MiB) / 8, transportBufferCap(6 * GiB + 410 * MiB));
    try testing.expectEqual(76 * GiB / 8, transportBufferCap(76 * GiB));
    try testing.expectEqual(22 * GiB, transportBufferCap(field_budget));
    try testing.expectEqual(@as(u64, 0), transportBufferCap(0));
}

test "transport cap resolution: absolute MiB > pct in 1..=100 > budget/8 — integer values the ONE parser admitted, no second parser" {
    try testing.expectEqual(2 * GiB, resolveTransportBufferCap(8 * GiB, 2048, 50));
    try testing.expectEqual(@as(u64, 0), resolveTransportBufferCap(8 * GiB, 0, null));
    try testing.expectEqual(4 * GiB, resolveTransportBufferCap(8 * GiB, null, 50));
    try testing.expectEqual(GiB, resolveTransportBufferCap(8 * GiB, null, null));
    try testing.expectEqual(8 * GiB, resolveTransportBufferCap(8 * GiB, null, 100));
    try testing.expectEqual(8 * GiB * 12 / 100, resolveTransportBufferCap(8 * GiB, null, 12));
    // Multiply-first: 3 % of 4096 is 122 (the reference's float form rounds the same way).
    try testing.expectEqual(@as(u64, 122), resolveTransportBufferCap(4096, null, 3));
    // A fraction or a value above 100 never reaches here: the gate refuses them (knobs.zig).
    try testing.expectEqual(@as(?u64, null), switch (knobs.parseIntIn(u64, "SQUEEZEFS_TRANSPORT_MEM_PCT", "250", 1, 100)) {
        .value => |v| @as(?u64, v),
        else => null,
    });
    try testing.expectEqual(@as(?u64, null), switch (knobs.parseIntIn(u64, "SQUEEZEFS_TRANSPORT_MEM_PCT", "12.5", 1, 100)) {
        .value => |v| @as(?u64, v),
        else => null,
    });
}

test "ipc arena cap resolution mirrors the transport family: absolute > pct > budget/8" {
    try testing.expectEqual(22 * GiB, ipcArenaCap(field_budget));
    try testing.expectEqual(2 * GiB, resolveIpcArenaCap(8 * GiB, 2048, 50));
    try testing.expectEqual(@as(u64, 0), resolveIpcArenaCap(8 * GiB, 0, null));
    try testing.expectEqual(4 * GiB, resolveIpcArenaCap(8 * GiB, null, 50));
    try testing.expectEqual(GiB, resolveIpcArenaCap(8 * GiB, null, null));
    try testing.expectEqual(8 * GiB, resolveIpcArenaCap(8 * GiB, null, 100));
}

test "dir_entry_capacity = max(50_000, total / 200_000) with the shipped floor" {
    try testing.expectEqual(176 * GiB / 200_000, dirEntryCapacity(176 * GiB));
    try testing.expectEqual(@as(u64, 50_000), dirEntryCapacity(floor_budget));
    try testing.expectEqual(@as(u64, 50_000), dirEntryCapacity(50_000 * 200_000));
    try testing.expectEqual(@as(u64, 50_001), dirEntryCapacity(50_001 * 200_000));
    for ([_]u64{ 0, 1 << 30, 64 << 30, 1 << 40 }) |total| {
        try testing.expectEqual(@max(50_000, total / 200_000), dirEntryCapacity(total));
    }
}

test "ipc_session_population_target = max(128, cpus × EXA_CANON_QD)" {
    try testing.expectEqual(@as(u64, 8), exa_canon_qd);
    try testing.expectEqual(@as(u64, 256), ipcSessionPopulationTarget(32));
    try testing.expectEqual(32 * exa_canon_qd, ipcSessionPopulationTarget(32));
    try testing.expectEqual(@as(u64, 128), ipcSessionPopulationTarget(1));
    try testing.expectEqual(@as(u64, 128), ipcSessionPopulationTarget(8));
    try testing.expectEqual(@as(u64, 128), ipcSessionPopulationTarget(16));
    try testing.expectEqual(@as(u64, 512), ipcSessionPopulationTarget(64));
    try testing.expectEqual(@as(u64, 1536), ipcSessionPopulationTarget(192));
}

test "ipc arena default = max(64 MiB, align_down(cap / population_target(cpus), 4 MiB)); env verbatim" {
    const cap = ipcArenaCap(field_budget);
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, cap, 32));
    try testing.expectEqual(176 * MiB, resolveIpcArenaBytes(null, cap, 8));
    try testing.expectEqual(64 * MiB, resolveIpcArenaBytes(null, ipcArenaCap(floor_budget), 2));
    try testing.expectEqual(64 * MiB, resolveIpcArenaBytes(null, cap, 512));
    try testing.expectEqual(128 * MiB, resolveIpcArenaBytes(null, 129 * 128 * MiB, 8));
    try testing.expectEqual(128 * MiB, resolveIpcArenaBytes(null, 129 * 256 * MiB, 32));
    // The bounce shape: cap/population = 90 MiB (an odd 2 MiB multiple) rounds down to 88 MiB.
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, 90 * 128 * MiB, 8));
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, 90 * 256 * MiB, 32));
    for ([_]u64{ 1, 300, 8192, 11_520, 22_528, 90 * 128, 129 * 128 }) |cap_mib| {
        for ([_]u32{ 1, 2, 8, 16, 32, 64, 192, 512 }) |cpus| {
            try testing.expectEqual(@as(u64, 0), resolveIpcArenaBytes(null, cap_mib * MiB, cpus) % ipc_arena_dma_align_bytes);
        }
    }
    try testing.expectEqual(64 * MiB, resolveIpcArenaBytes(64, 22 * GiB, 32));
    try testing.expectEqual(80 * MiB, resolveIpcArenaBytes(80, 22 * GiB, 32));
    try testing.expectEqual(90 * MiB, resolveIpcArenaBytes(90, 22 * GiB, 32));
    // 0 never reaches here: the row's range starts at 1 and the gate refuses it.
    try testing.expectEqual(@as(i128, 1), knobs.row("SQUEEZEFS_IPC_ARENA_MB").kind.int.lo);
}

test "ipc per-uid session cap = clamp(cap / max(arena, 1), 64, 4096); monotone non-increasing in arena" {
    const cap = ipcArenaCap(field_budget);
    try testing.expectEqual(@as(u64, 256), ipcPerUidSessionCap(cap, resolveIpcArenaBytes(null, cap, 32)));
    try testing.expectEqual(@as(u64, 128), ipcPerUidSessionCap(cap, resolveIpcArenaBytes(null, cap, 8)));
    try testing.expectEqual(@as(u64, 130), ipcPerUidSessionCap(90 * 128 * MiB, resolveIpcArenaBytes(null, 90 * 128 * MiB, 8)));
    const floor_cap = ipcArenaCap(floor_budget);
    try testing.expectEqual(@as(u64, 64), ipcPerUidSessionCap(floor_cap, resolveIpcArenaBytes(null, floor_cap, 2)));
    try testing.expectEqual(@as(u64, 4096), ipcPerUidSessionCap(std.math.maxInt(u64), 64 * MiB));
    try testing.expectEqual(@as(u64, 4096), ipcPerUidSessionCap(1024 * GiB, MiB));
    try testing.expectEqual(@as(u64, 4096), ipcPerUidSessionCap(100 * MiB, 0));
    var last: u64 = std.math.maxInt(u64);
    for ([_]u64{ 4, 16, 64, 88, 176, 256, 1024, 4096, 22 * 1024 }) |arena_mib| {
        const got = ipcPerUidSessionCap(22 * GiB, arena_mib * MiB);
        try testing.expect(got <= last);
        last = got;
    }
}

test "fleet share unsatisfiable floor refuses naming the arithmetic; share 1 never refuses" {
    var buf: RefusalBuf = undefined;
    // 32 possible CPUs ⇒ floor = 32 × Q_DEPTH_FLOOR 4 × 1 MiB = 128 MiB.
    const floor: u64 = 32 * transport_q_depth_floor * transport_payload_base;
    try testing.expectEqual(@as(u64, 134_217_728), floor);
    try testing.expectEqual(@as(?[]const u8, null), fleetShareFloorRefusal(2, floor, 32, &buf));
    try testing.expectEqual(@as(?[]const u8, null), fleetShareFloorRefusal(2, floor + 1, 32, &buf));
    const msg = fleetShareFloorRefusal(1024, floor - 1, 32, &buf).?;
    try testing.expect(std.mem.indexOf(u8, msg, "fleet share 1024") != null);
    try testing.expect(std.mem.indexOf(u8, msg, "134217727") != null);
    try testing.expect(std.mem.indexOf(u8, msg, "134217728") != null);
    try testing.expect(std.mem.indexOf(u8, msg, "Q_DEPTH_FLOOR") != null);
    try testing.expect(std.mem.indexOf(u8, msg, "32") != null);
    try testing.expectEqual(@as(?[]const u8, null), fleetShareFloorRefusal(1, floor - 1, 32, &buf));
    try testing.expectEqual(@as(?[]const u8, null), fleetShareFloorRefusal(1, 0, 4096, &buf));
}

test "knob faces: transport/ipc caps and the ipc arena read their rows through the registry" {
    const gpa = testing.allocator;
    var env = std.process.Environ.Map.init(gpa);
    defer env.deinit();
    try testing.expectEqual(GiB, transportBufferCapFromEnv(&env, 8 * GiB));
    try testing.expectEqual(GiB, ipcArenaCapFromEnv(&env, 8 * GiB));
    try testing.expectEqual(88 * MiB, ipcArenaBytesFromEnv(&env, 22 * GiB, 32));
    try env.put("SQUEEZEFS_TRANSPORT_MEM_PCT", "50");
    try env.put("SQUEEZEFS_IPC_MEM_MAX", "2048");
    try env.put("SQUEEZEFS_IPC_MEM_PCT", "50");
    try env.put("SQUEEZEFS_IPC_ARENA_MB", "80");
    try testing.expectEqual(4 * GiB, transportBufferCapFromEnv(&env, 8 * GiB));
    try env.put("SQUEEZEFS_TRANSPORT_MEM_MAX", "2048");
    try testing.expectEqual(2 * GiB, transportBufferCapFromEnv(&env, 8 * GiB));
    try testing.expectEqual(2 * GiB, ipcArenaCapFromEnv(&env, 8 * GiB));
    try testing.expectEqual(80 * MiB, ipcArenaBytesFromEnv(&env, 22 * GiB, 32));
}
