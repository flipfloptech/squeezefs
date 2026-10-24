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
/// (never-regress-below-shipped: every box ran 64 MiB sessions) — on a
/// budget whose cap holds the population floor at it; a bounded budget's
/// arena yields to the floor (`resolveIpcArenaBytes`).
pub const ipc_arena_floor_bytes: u64 = 64 * MiB;
/// The shim's/SDK's sessions per (process, mount) floor: the single-
/// dequeue-thread plateau of the 2026-07-19 service sweep (`ipc/sizing.
/// ilSessionsDefault`'s clamp floor). ONE constant: the population floor
/// below is "one client process per CPU at this many sessions".
pub const il_sessions_per_mount_floor: u64 = 2;
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

/// The session population the admission cap must hold WHATEVER the budget:
/// one client process per CPU at the per-mount session floor — the box
/// run's 32 processes × 2 sessions. A cap that holds fewer refuses the
/// matched-inflight fleet at its first over-budget client (measured on the
/// venue: a 2 GiB budget ⇒ cap 256 MiB ⇒ three 64 MiB sessions; fio's
/// second process was refused "session budget exhausted (class 5)" and
/// kernel-served, engagement 72 %, the row INVALID — decision 0003 §H).
pub fn ipcSessionPopulationFloor(cpus: u32) u64 {
    return il_sessions_per_mount_floor * @as(u64, @max(cpus, 1));
}

/// Per-session arena: `SQUEEZEFS_IPC_ARENA_MB` (verbatim MiB; the row's
/// range starts at 1) else the charter's `max(64 MiB, align_down(cap /
/// population_target(cpus), 4 MiB))` — a full population of default arenas
/// fills the admission cap exactly — which YIELDS to the population floor
/// when the cap cannot hold `population_floor(cpus)` sessions of it: then
/// the arena is the largest 4 MiB-aligned size at which the floor's
/// sessions (arena + `session_overhead_bytes` each: the memfd's header,
/// ring, slots and stats page before the arena) fit, never below one
/// DMA-align grain (4 MiB = 1024 slots × the 4 KiB O_DIRECT LBA, the
/// smallest arena whose every slot is a page).
pub fn resolveIpcArenaBytes(arena_mib: ?u64, arena_cap_bytes: u64, cpus: u32, session_overhead_bytes: u64) u64 {
    if (arena_mib) |mib| return mib *| MiB;
    const derived = derive.alignDown(arena_cap_bytes / ipcSessionPopulationTarget(cpus), ipc_arena_dma_align_bytes);
    const charter = @max(ipc_arena_floor_bytes, derived);
    const floor = ipcSessionPopulationFloor(cpus);
    if (ipcAdmittedPopulation(arena_cap_bytes, charter + session_overhead_bytes) >= floor) return charter;
    const per_session = arena_cap_bytes / floor;
    return @max(ipc_arena_dma_align_bytes, derive.alignDown(per_session -| session_overhead_bytes, ipc_arena_dma_align_bytes));
}

/// Sessions the admission cap holds at one session footprint
/// (`cap / footprint`): the census the arm line prints and the population
/// floor is judged on.
pub fn ipcAdmittedPopulation(arena_cap_bytes: u64, footprint_bytes: u64) u64 {
    return arena_cap_bytes / @max(footprint_bytes, 1);
}

/// The shipped slot slab: 64 MiB / 1024 slots. It is the ring's per-op
/// ceiling for an async op (libaio's one-iocb-one-slot law routes a larger
/// iocb to the kernel) and the hybrid lane gate's floor, so it holds when
/// the arena yields: an arena under 64 MiB carries FEWER slots of this
/// slab, not smaller slabs.
pub const ipc_slab_floor_bytes: u64 = 64 * 1024;
/// The default slot count (`layout.DEFAULT_SLOTS`; the ring is sized to it).
pub const ipc_default_slots: u64 = 1024;

/// Slots per session for an arena: `min(1024, arena / 64 KiB)` — the
/// shipped 1024 at and above the shipped arena (a larger arena grows the
/// slab, as the charter's derivation does), fewer slots of the shipped slab
/// below it. The floor is the 4 MiB DMA grain's 64 slots = 2 × fio's QD32:
/// per-session depth is what a bounded budget trades, never the per-op shape.
pub fn ipcSlotsForArena(arena_bytes: u64) u32 {
    return @intCast(@max(1, @min(ipc_default_slots, arena_bytes / ipc_slab_floor_bytes)));
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

// ---- the idle floor (the idle-RSS law) ----

/// std's per-thread signal stack (`std.Options.signal_stack_size`'s
/// default, which the product's root `src/main.zig` keeps): a
/// `threadlocal` array, so every thread's spawn zero-fills it.
pub const signal_stack_bytes: u64 = (std.Options{}).signal_stack_size orelse 0;

/// The stack an idle thread has touched, measured on the venue: ≤ 50 KiB
/// of a 16 MiB reservation for the FUSE ring workers and the lanes (every
/// thread but one), 113 KiB for the budget's sampler (its census buffer);
/// 64 KiB bounds the average with room.
pub const idle_thread_stack_bytes: u64 = 64 * 1024;
/// The image and the mount's fixed tables, measured on the venue (32 CPUs,
/// x86_64, ReleaseSafe: 84 MiB): the text, rodata, loader and libc the
/// daemon ran (14 MiB, file-backed), the counter and histogram tables in
/// `.bss` (16 MiB of its 20 resident: THP maps a touched 2 MiB range
/// whole), the fsck verifier's two 4 MiB windows, the sym planes' holder
/// and token tables (11 MiB), the lanes' inboxes and the rings (10 MiB),
/// the metadata engine's conveyor, journal lane, allocator and lease table
/// (10 MiB) and ≈ 15 MiB of smaller tables; 96 leaves an eighth for the
/// tables that grow with the CPU count.
pub const idle_image_bytes: u64 = 96 * MiB;

/// THE IDLE FLOOR — the resident bytes of an idle mounted daemon that do
/// NOT follow its memory budget. The idle-RSS law (`docs/evidence/
/// rung-10-idle-rss.md`): an idle daemon's RSS ≤ its budget + this floor;
/// every other byte is a bound the budget derives, and a reservation sized
/// from the machine's RAM (the read cache's) is touched lazily, resident
/// only as far as its bound let it fill.
///   (1) the FUSE payload pool's floor, `possible_cpus × Q_DEPTH_FLOOR ×
///       1 MiB`: pinned by the rings' fixed-buffer registration at arm,
///       one queue per possible CPU (the kernel's) — the never-divided
///       floor the fleet-share refusal names;
///   (2) per thread: std's signal stack (`signal_stack_bytes`, in the
///       static TLS block the C runtime zero-fills at every spawn) and
///       `idle_thread_stack_bytes`;
///   (3) the image (`idle_image_bytes`).
pub fn idleFloorBytes(possible_cpus: u32, threads: u64) u64 {
    const transport = @as(u64, possible_cpus) * transport_q_depth_floor * transport_payload_base;
    const per_thread = signal_stack_bytes + idle_thread_stack_bytes;
    return transport + threads * per_thread + idle_image_bytes;
}

/// The idle-RSS bound: `budget + idleFloorBytes`.
pub fn idleRssBound(budget: u64, possible_cpus: u32, threads: u64) u64 {
    return budget +| idleFloorBytes(possible_cpus, threads);
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

pub fn ipcArenaBytesFromEnv(env: *const std.process.Environ.Map, arena_cap_bytes: u64, cpus: u32, session_overhead_bytes: u64) u64 {
    return resolveIpcArenaBytes(knobs.optIntKnob("SQUEEZEFS_IPC_ARENA_MB", u64, env), arena_cap_bytes, cpus, session_overhead_bytes);
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

/// The default session geometry's bytes before the arena (header + ring +
/// slots + stats page = 0x25000; `ipc/layout.zig` pins the figure).
const session_overhead: u64 = 0x25000;

test "ipc arena default = max(64 MiB, align_down(cap / population_target(cpus), 4 MiB)) where the cap holds the population floor at it; env verbatim" {
    const cap = ipcArenaCap(field_budget);
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, cap, 32, session_overhead));
    try testing.expectEqual(176 * MiB, resolveIpcArenaBytes(null, cap, 8, session_overhead));
    // 512 cpus on the field budget: the charter's 64 MiB floor holds 351
    // sessions, under the 1024-session population floor ⇒ 22 GiB / 1024 =
    // 22 MiB per session, 20 MiB of arena after the overhead.
    try testing.expectEqual(20 * MiB, resolveIpcArenaBytes(null, cap, 512, session_overhead));
    try testing.expectEqual(64 * MiB, resolveIpcArenaBytes(null, cap, 128, session_overhead));
    try testing.expectEqual(128 * MiB, resolveIpcArenaBytes(null, 129 * 128 * MiB, 8, session_overhead));
    try testing.expectEqual(128 * MiB, resolveIpcArenaBytes(null, 129 * 256 * MiB, 32, session_overhead));
    // The bounce shape: cap/population = 90 MiB (an odd 2 MiB multiple) rounds down to 88 MiB.
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, 90 * 128 * MiB, 8, session_overhead));
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, 90 * 256 * MiB, 32, session_overhead));
    for ([_]u64{ 1, 300, 8192, 11_520, 22_528, 90 * 128, 129 * 128 }) |cap_mib| {
        for ([_]u32{ 1, 2, 8, 16, 32, 64, 192, 512 }) |cpus| {
            try testing.expectEqual(@as(u64, 0), resolveIpcArenaBytes(null, cap_mib * MiB, cpus, session_overhead) % ipc_arena_dma_align_bytes);
        }
    }
    try testing.expectEqual(64 * MiB, resolveIpcArenaBytes(64, 22 * GiB, 32, session_overhead));
    try testing.expectEqual(80 * MiB, resolveIpcArenaBytes(80, 22 * GiB, 32, session_overhead));
    try testing.expectEqual(90 * MiB, resolveIpcArenaBytes(90, 22 * GiB, 32, session_overhead));
    // Explicit wins even where the derived arena would yield to the floor.
    try testing.expectEqual(64 * MiB, resolveIpcArenaBytes(64, 256 * MiB, 9, session_overhead));
    // 0 never reaches here: the row's range starts at 1 and the gate refuses it.
    try testing.expectEqual(@as(i128, 1), knobs.row("SQUEEZEFS_IPC_ARENA_MB").kind.int.lo);
}

test "ipc_session_population_floor = 2 × cpus (one client process per CPU at the per-mount session floor)" {
    try testing.expectEqual(@as(u64, 2), il_sessions_per_mount_floor);
    try testing.expectEqual(@as(u64, 64), ipcSessionPopulationFloor(32));
    try testing.expectEqual(@as(u64, 18), ipcSessionPopulationFloor(9));
    try testing.expectEqual(@as(u64, 2), ipcSessionPopulationFloor(1));
    try testing.expectEqual(@as(u64, 2), ipcSessionPopulationFloor(0));
    var c: u32 = 1;
    while (c <= 4096) : (c += 1) try testing.expectEqual(il_sessions_per_mount_floor * c, ipcSessionPopulationFloor(c));
}

test "the arena yields to the population floor on a bounded budget: the largest 4 MiB-aligned arena at which 2 × cpus sessions (arena + overhead each) fit the cap, never below the 4 MiB DMA grain; the charter's arena where the cap holds the floor" {
    // The venue's measured shape: 9 cpus, SQUEEZEFS_MEM_BUDGET_MB=2048 ⇒
    // cap 256 MiB; 64 MiB sessions ⇒ 3 admitted (F4). Now 12 MiB ⇒ 21.
    const venue_cap = ipcArenaCap(2 * GiB);
    try testing.expectEqual(256 * MiB, venue_cap);
    const venue_arena = resolveIpcArenaBytes(null, venue_cap, 9, session_overhead);
    try testing.expectEqual(12 * MiB, venue_arena);
    try testing.expectEqual(@as(u64, 3), ipcAdmittedPopulation(venue_cap, 64 * MiB + session_overhead));
    try testing.expectEqual(@as(u64, 21), ipcAdmittedPopulation(venue_cap, venue_arena + session_overhead));
    try testing.expect(ipcAdmittedPopulation(venue_cap, venue_arena + session_overhead) >= ipcSessionPopulationFloor(9));
    // The harness's default budget (6144 MiB): cap 768 MiB ⇒ 40 MiB arenas, 19 sessions ≥ 18.
    const default_cap = ipcArenaCap(6144 * MiB);
    const default_arena = resolveIpcArenaBytes(null, default_cap, 9, session_overhead);
    try testing.expectEqual(40 * MiB, default_arena);
    try testing.expect(ipcAdmittedPopulation(default_cap, default_arena + session_overhead) >= ipcSessionPopulationFloor(9));
    // The floor shape (4 GiB RAM, 2 cpus): cap ≈ 358 MiB holds 5 × 64 MiB ≥ the floor's 4 ⇒ the charter's arena stands.
    const floor_cap = ipcArenaCap(floor_budget);
    try testing.expectEqual(64 * MiB, resolveIpcArenaBytes(null, floor_cap, 2, session_overhead));
    try testing.expect(ipcAdmittedPopulation(floor_cap, 64 * MiB + session_overhead) >= ipcSessionPopulationFloor(2));
    // The grain: a cap too small for the floor even at 4 MiB arenas stays at 4 MiB (loud at arm, never 0).
    try testing.expectEqual(4 * MiB, resolveIpcArenaBytes(null, 8 * MiB, 64, session_overhead));
    try testing.expectEqual(ipc_arena_dma_align_bytes, 4 * MiB);
    // The field shape is untouched: 22 GiB / 88 MiB sessions = 255 ≥ 64.
    const cap = ipcArenaCap(field_budget);
    try testing.expectEqual(88 * MiB, resolveIpcArenaBytes(null, cap, 32, session_overhead));
    try testing.expect(ipcAdmittedPopulation(cap, 88 * MiB + session_overhead) >= ipcSessionPopulationFloor(32));
    // The law restated over a sweep: wherever the cap holds the floor at the
    // charter's arena the answer IS the charter's; elsewhere the floor's
    // sessions fit (or the grain binds) and the arena is 4 MiB-aligned.
    for ([_]u64{ 8, 64, 256, 360, 768, 2048, 8192, 22_528 }) |cap_mib| {
        for ([_]u32{ 1, 2, 9, 16, 32, 64, 192 }) |cpus| {
            const c = cap_mib * MiB;
            const got = resolveIpcArenaBytes(null, c, cpus, session_overhead);
            const charter = @max(ipc_arena_floor_bytes, derive.alignDown(c / ipcSessionPopulationTarget(cpus), ipc_arena_dma_align_bytes));
            const floor = ipcSessionPopulationFloor(cpus);
            if (ipcAdmittedPopulation(c, charter + session_overhead) >= floor) {
                try testing.expectEqual(charter, got);
            } else {
                try testing.expect(got < charter);
                try testing.expect(got == ipc_arena_dma_align_bytes or ipcAdmittedPopulation(c, got + session_overhead) >= floor);
                try testing.expectEqual(@as(u64, 0), got % ipc_arena_dma_align_bytes);
            }
        }
    }
}

test "ipc slots for an arena = min(1024, arena / 64 KiB): the shipped 1024 at and above 64 MiB, fewer slots of the 64 KiB slab below it, 64 at the 4 MiB grain" {
    try testing.expectEqual(@as(u64, 64 * 1024), ipc_slab_floor_bytes);
    try testing.expectEqual(ipc_arena_floor_bytes / ipc_default_slots, ipc_slab_floor_bytes);
    try testing.expectEqual(@as(u32, 1024), ipcSlotsForArena(64 * MiB));
    try testing.expectEqual(@as(u32, 1024), ipcSlotsForArena(88 * MiB));
    try testing.expectEqual(@as(u32, 1024), ipcSlotsForArena(176 * MiB));
    try testing.expectEqual(@as(u32, 640), ipcSlotsForArena(40 * MiB));
    try testing.expectEqual(@as(u32, 448), ipcSlotsForArena(28 * MiB));
    try testing.expectEqual(@as(u32, 192), ipcSlotsForArena(12 * MiB));
    try testing.expectEqual(@as(u32, 64), ipcSlotsForArena(4 * MiB));
    try testing.expectEqual(@as(u32, 1), ipcSlotsForArena(1));
    // The venue's shapes keep the shipped slab: arena / slots = 64 KiB.
    for ([_]u64{ 4, 12, 28, 40, 64 }) |arena_mib| {
        try testing.expectEqual(ipc_slab_floor_bytes, arena_mib * MiB / ipcSlotsForArena(arena_mib * MiB));
    }
}

test "ipc per-uid session cap = clamp(cap / max(arena, 1), 64, 4096); monotone non-increasing in arena" {
    const cap = ipcArenaCap(field_budget);
    try testing.expectEqual(@as(u64, 256), ipcPerUidSessionCap(cap, resolveIpcArenaBytes(null, cap, 32, session_overhead)));
    try testing.expectEqual(@as(u64, 128), ipcPerUidSessionCap(cap, resolveIpcArenaBytes(null, cap, 8, session_overhead)));
    try testing.expectEqual(@as(u64, 130), ipcPerUidSessionCap(90 * 128 * MiB, resolveIpcArenaBytes(null, 90 * 128 * MiB, 8, session_overhead)));
    const floor_cap = ipcArenaCap(floor_budget);
    try testing.expectEqual(@as(u64, 64), ipcPerUidSessionCap(floor_cap, resolveIpcArenaBytes(null, floor_cap, 2, session_overhead)));
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

test "tie: the idle floor = possible_cpus × Q_DEPTH_FLOOR × 1 MiB (the pinned payload pool) + threads × (std's signal stack + 64 KiB of idle stack) + 96 MiB of image — the venue's 32 CPUs and 146 threads: 128 MiB + 146 × 320 KiB + 96 MiB; the bound is the budget plus it" {
    const per_thread: u64 = signal_stack_bytes + 64 * 1024;
    try testing.expectEqual(@as(u64, 256 * 1024), signal_stack_bytes);
    try testing.expectEqual(@as(u64, 32 * 4 * MiB + 146 * per_thread + 96 * MiB), idleFloorBytes(32, 146));
    try testing.expectEqual(@as(u64, 128 * MiB + 146 * 320 * 1024 + 96 * MiB), idleFloorBytes(32, 146));
    try testing.expectEqual(64 * MiB + idleFloorBytes(32, 146), idleRssBound(64 * MiB, 32, 146));
    try testing.expectEqual(@as(u64, 4 * MiB + 96 * MiB), idleFloorBytes(1, 0));
    for ([_]u32{ 1, 9, 32, 192 }) |c| for ([_]u64{ 0, 20, 146, 600 }) |t| {
        try testing.expectEqual(@as(u64, c) * transport_q_depth_floor * transport_payload_base + t * per_thread + idle_image_bytes, idleFloorBytes(c, t));
    };
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
    try testing.expectEqual(88 * MiB, ipcArenaBytesFromEnv(&env, 22 * GiB, 32, session_overhead));
    try env.put("SQUEEZEFS_TRANSPORT_MEM_PCT", "50");
    try env.put("SQUEEZEFS_IPC_MEM_MAX", "2048");
    try env.put("SQUEEZEFS_IPC_MEM_PCT", "50");
    try env.put("SQUEEZEFS_IPC_ARENA_MB", "80");
    try testing.expectEqual(4 * GiB, transportBufferCapFromEnv(&env, 8 * GiB));
    try env.put("SQUEEZEFS_TRANSPORT_MEM_MAX", "2048");
    try testing.expectEqual(2 * GiB, transportBufferCapFromEnv(&env, 8 * GiB));
    try testing.expectEqual(2 * GiB, ipcArenaCapFromEnv(&env, 8 * GiB));
    try testing.expectEqual(80 * MiB, ipcArenaBytesFromEnv(&env, 22 * GiB, 32, session_overhead));
}
