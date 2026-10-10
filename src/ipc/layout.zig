//! The sealed-memfd session layout, byte-exact (charter §7; spec
//! `docs/porting/il_session.md` §1). The memfd holds
//!
//!     [header page | submission ring | slot array | client stats page | arena]
//!
//! every region page-aligned, strictly ordered, the arena last. Every word a
//! client can write is an atomic (a hostile mid-serve mutation is bounded
//! behaviour, never a data race); every struct here is `extern`, little-
//! endian by construction (the two sides are one host), with `comptime`
//! size/offset asserts, and zero-initialised valid (a fresh memfd reads as a
//! valid but EMPTY layout — except the ring's Vyukov cells, which the daemon
//! seeds before the fd is shared, `ring_core.seedForSharing`).
//!
//! Trust boundary (spec §1.3): every header word is client-writable for the
//! session's life. The daemon treats its own fields (magic, abi, geometry)
//! as write-once OUTPUTS it never re-reads — geometry authority is the
//! daemon's private copy; `SessionHeader.validate` is the CLIENT's screen.
//!
//! No wire compatibility with the reference is owed (decision 0002 row 300):
//! `IPC_ABI = 1` is the PORT's own series and the magic is `SQZIPC02`, so a
//! reference shim can never map a port session by accident.

const std = @import("std");
const fuzzkit = @import("../core/fuzzkit.zig");
const derive = @import("../core/derive.zig");
const sizing = @import("../mem/sizing.zig");

const Atomic = std.atomic.Value;
const assert = std.debug.assert;

pub const MiB = derive.MiB;

/// Header line 0 identity.
pub const IPC_MAGIC: u64 = std.mem.readInt(u64, "SQZIPC02", .little);
/// Coarse structural guard, bumped on ANY layout change; fine skew is the
/// build-commit equality at HELLO (`wire.zig`).
pub const IPC_ABI: u32 = 1;
/// Every region starts page-aligned.
pub const PAGE_BYTES: u64 = 4096;
/// One slot = one cache-line PAIR: the protocol word owns the second line
/// outright (spec §1.5 "the pad is the second line the protocol word must
/// own"; co-located words that the hot writer reads every op stay on line
/// one — PERF-18's split measured 2.46× WORSE in the symmetric shape).
pub const SLOT_BYTES: u64 = 128;
/// The producer-contended ring tail CAS word never shares a line with the
/// cells.
pub const RING_TAIL_LINE_BYTES: u64 = 128;
/// `RingCell{seq u32, value u32}`.
pub const RING_CELL_BYTES: u64 = 8;
/// Ring AND slot count default. WHY 1024: `slots ≤ ring_entries` makes a
/// full-ring refusal unobservable for the honest protocol (one ring entry
/// per CLAIMED slot), and `1024 slots × 4 KiB LBA = 4 MiB` is the arena DMA
/// alignment grain (`mem/sizing.ipc_arena_dma_align_bytes`).
pub const DEFAULT_RING_ENTRIES: u32 = 1024;
pub const DEFAULT_SLOTS: u32 = 1024;
/// The shipped floor (`mem/sizing.ipc_arena_floor_bytes`).
pub const DEFAULT_ARENA_BYTES: u64 = sizing.ipc_arena_floor_bytes;
/// Per-op payload ceiling, daemon-validated; the transport's `max_write`
/// floor so both lanes share one per-op ceiling (decision 0002 ledger Q14).
pub const DEFAULT_MAX_OP_BYTES: u64 = 1 * MiB;
/// Arithmetic sanity bound only: admission is the budget's job.
pub const MAX_ARENA_BYTES: u64 = derive.TiB;
/// A 1-cell ring is structurally broken: position 1's claim check
/// (`seq == 1`) is indistinguishable from position 0's publication
/// (`seq == 0 + 1`) — found by the ring-vs-reference property suite.
pub const MIN_RING_ENTRIES: u32 = 2;
/// 2^16 keeps `seq − pos` inside `i32` under wrapping u32 arithmetic.
pub const MAX_RING_ENTRIES: u32 = 1 << 16;
/// Arena SIZE alignment: `slots × LBA` (4 MiB; every 4 MiB multiple is also
/// PMD-aligned). WHY not PMD alone: an odd 2 MiB-multiple arena makes the
/// slab `≡ 2048 (mod 4096)` — half the slots fail the O_DIRECT DMA screen
/// (the 2026-08-04 −15.4 % randread row).
pub const ARENA_ALIGN_BYTES: u64 = sizing.ipc_arena_dma_align_bytes;
/// x86_64 / arm64-4K huge page: the mapping's alignment for `MADV_COLLAPSE`.
pub const PMD_BYTES: u64 = 2 * MiB;

/// The op table is `{READ, WRITE}` (decision 0002 ledger Q8: the
/// reference's `ECHO` read-and-invert ping served no app; liveness in tests
/// is a 0-byte READ on a bound fd).
pub const Op = enum(u32) {
    read = 1,
    write = 2,
    _,
};

/// A u32-ns ingress stamp wraps every 4.295 s, so a same-host delta at or
/// past 1 s is wrap-ambiguous or hostile — a physical bound, not tuning.
pub const INGRESS_PLAUSIBLE_MAX_NS: u32 = std.time.ns_per_s;

// ---------------------------------------------------------------------------
// Geometry.
// ---------------------------------------------------------------------------

/// Each refusal a distinct member (the refusal ledger attributes exactly).
pub const GeometryError = error{
    RingEntriesTooSmall,
    RingEntriesNotPow2,
    RingEntriesTooLarge,
    SlotsZero,
    SlotsExceedRing,
    ArenaZero,
    ArenaNotPageMultiple,
    ArenaTooLarge,
    MaxOpZero,
    MaxOpExceedsArena,
    /// `SessionLayout.compute`'s checked arithmetic overflowed.
    Overflow,
};

pub const Geometry = extern struct {
    ring_entries: u32,
    slots: u32,
    arena_bytes: u64,
    max_op_bytes: u32,
    _pad: u32 = 0,

    pub const default: Geometry = .{
        .ring_entries = DEFAULT_RING_ENTRIES,
        .slots = DEFAULT_SLOTS,
        .arena_bytes = DEFAULT_ARENA_BYTES,
        .max_op_bytes = @intCast(DEFAULT_MAX_OP_BYTES),
    };

    pub fn validate(g: Geometry) GeometryError!void {
        if (g.ring_entries < MIN_RING_ENTRIES) return error.RingEntriesTooSmall;
        if (!std.math.isPowerOfTwo(g.ring_entries)) return error.RingEntriesNotPow2;
        if (g.ring_entries > MAX_RING_ENTRIES) return error.RingEntriesTooLarge;
        if (g.slots == 0) return error.SlotsZero;
        if (g.slots > g.ring_entries) return error.SlotsExceedRing;
        if (g.arena_bytes == 0) return error.ArenaZero;
        if (g.arena_bytes % PAGE_BYTES != 0) return error.ArenaNotPageMultiple;
        if (g.arena_bytes > MAX_ARENA_BYTES) return error.ArenaTooLarge;
        if (g.max_op_bytes == 0) return error.MaxOpZero;
        if (g.max_op_bytes > g.arena_bytes) return error.MaxOpExceedsArena;
    }

    /// THE ONE SLAB LAW: `slab = min(arena/slots, max_op_bytes)`; at or
    /// above a page it is floored to a page multiple so `slot × slab` is
    /// page-aligned for EVERY slot whatever the arena (an odd explicit
    /// `SQUEEZEFS_IPC_ARENA_MB` otherwise made half the slots fail the
    /// O_DIRECT DMA screen: measured 50.006 % bounce rate). Sub-page slabs
    /// (toy geometries) pass verbatim and ride the bounce path. Default:
    /// 64 MiB / 1024 = 64 KiB. Slot `i`'s arena window is
    /// `[i·slab, (i+1)·slab)`.
    pub fn slotSlab(g: Geometry) u64 {
        const raw = @min(g.arena_bytes / @max(g.slots, 1), @as(u64, g.max_op_bytes));
        return if (raw >= PAGE_BYTES) derive.alignDown(raw, PAGE_BYTES) else raw;
    }

    pub fn eql(a: Geometry, b: Geometry) bool {
        return a.ring_entries == b.ring_entries and a.slots == b.slots and a.arena_bytes == b.arena_bytes and a.max_op_bytes == b.max_op_bytes;
    }
};

comptime {
    assert(@sizeOf(Geometry) == 24);
    assert(@offsetOf(Geometry, "ring_entries") == 0);
    assert(@offsetOf(Geometry, "slots") == 4);
    assert(@offsetOf(Geometry, "arena_bytes") == 8);
    assert(@offsetOf(Geometry, "max_op_bytes") == 16);
    assert(@offsetOf(Geometry, "_pad") == 20);
}

// ---------------------------------------------------------------------------
// Region arithmetic.
// ---------------------------------------------------------------------------

pub const SessionLayout = struct {
    geometry: Geometry,
    header_off: u64,
    header_bytes: u64,
    ring_off: u64,
    ring_bytes: u64,
    ring_cells_off: u64,
    slots_off: u64,
    slots_bytes: u64,
    stats_off: u64,
    stats_bytes: u64,
    arena_off: u64,
    arena_bytes: u64,
    /// `== memfd size`.
    total_bytes: u64,
    slab: u64,

    /// Pure: the §1.2 arithmetic; every step checked (`Overflow`).
    pub fn compute(g: Geometry) GeometryError!SessionLayout {
        try g.validate();
        const header_bytes = PAGE_BYTES;
        const ring_off = header_bytes;
        const ring_bytes = try add(RING_TAIL_LINE_BYTES, try mul(g.ring_entries, RING_CELL_BYTES));
        const slots_off = pageRound(try add(ring_off, ring_bytes));
        const slots_bytes = try mul(g.slots, SLOT_BYTES);
        const stats_off = pageRound(try add(slots_off, slots_bytes));
        const stats_bytes = PAGE_BYTES;
        const arena_off = pageRound(try add(stats_off, stats_bytes));
        const total = try add(arena_off, g.arena_bytes);
        return .{
            .geometry = g,
            .header_off = 0,
            .header_bytes = header_bytes,
            .ring_off = ring_off,
            .ring_bytes = ring_bytes,
            .ring_cells_off = ring_off + RING_TAIL_LINE_BYTES,
            .slots_off = slots_off,
            .slots_bytes = slots_bytes,
            .stats_off = stats_off,
            .stats_bytes = stats_bytes,
            .arena_off = arena_off,
            .arena_bytes = g.arena_bytes,
            .total_bytes = total,
            .slab = g.slotSlab(),
        };
    }

    fn pageRound(v: u64) u64 {
        return derive.alignUp(v, PAGE_BYTES);
    }
    fn add(a: u64, b: u64) GeometryError!u64 {
        return std.math.add(u64, a, b) catch error.Overflow;
    }
    fn mul(a: u64, b: u64) GeometryError!u64 {
        return std.math.mul(u64, a, b) catch error.Overflow;
    }
};

// ---------------------------------------------------------------------------
// The header page: three cache lines + the geometry copy.
// ---------------------------------------------------------------------------

/// The completion doorbell (`doorbell.zig` is the protocol): four u32 words
/// on ONE line — the completer reads `parked`/`wake_at` on every bump; a
/// split would pull three remotely-dirtied lines per op (PERF-18).
pub const CqeDoorbell = extern struct {
    seq: Atomic(u32) = .init(0),
    parked: Atomic(u32) = .init(0),
    wake_at: Atomic(u32) = .init(0),
    /// `NO_PAY_RECORDED` initially (`doorbell.zig`).
    wake_paid_mark: Atomic(u32) = .init(std.math.maxInt(u32)),
};

pub const SessionHeader = extern struct {
    // ---- line 0: identity + the two daemon-stamped words ----
    magic: u64 = IPC_MAGIC,
    abi: u32 = IPC_ABI,
    _pad0: u32 = 0,
    /// Daemon: init 1; `fetchAdd(1, .release)` = POISON (every client op
    /// falls through to the real call). Client: `.acquire` at map and on
    /// every `poisoned()` check.
    generation: Atomic(u64) = .init(1),
    /// The daemon's service pass stamps `clock.monotonicNs()` at every pass
    /// top and park exit (decision 0003 §F: the reference never wrote it);
    /// the client's watch escalates 1 s unchanged → socket probe → poison.
    heartbeat: Atomic(u64) = .init(0),
    _pad1: [32]u8 = @splat(0),
    // ---- line 1: the submit doorbell pair (co-located, PERF-18) ----
    /// Client `fetchAdd(1, .seq_cst)` per submit; daemon snapshot + the
    /// `futex_waitv` word.
    doorbell: Atomic(u32) = .init(0),
    /// Daemon `store(1/0, .seq_cst)` around its park; the client's wake gate.
    daemon_parked: Atomic(u32) = .init(0),
    _pad2: [56]u8 = @splat(0),
    // ---- line 2: the completion doorbell ----
    cqe: CqeDoorbell = .{},
    _pad3: [48]u8 = @splat(0),
    // ---- line 3+: the client's geometry copy ----
    geometry: Geometry,
    _pad4: [PAGE_BYTES - 216]u8 = @splat(0),

    pub const ValidateError = error{ BadMagic, BadAbi } || GeometryError;

    /// CLIENT-side: the three screens before any word is trusted. The
    /// daemon never re-reads its own header fields.
    pub fn validate(h: *const SessionHeader) ValidateError!void {
        if (h.magic != IPC_MAGIC) return error.BadMagic;
        if (h.abi != IPC_ABI) return error.BadAbi;
        try h.geometry.validate();
    }

    /// Total decode of a header page image (the fuzz surface): a short
    /// image is `Truncated`; the three screens follow.
    pub fn decode(bytes: []const u8) (error{Truncated} || ValidateError)!SessionHeader {
        if (bytes.len < @sizeOf(SessionHeader)) return error.Truncated;
        var h: SessionHeader = undefined;
        @memcpy(std.mem.asBytes(&h), bytes[0..@sizeOf(SessionHeader)]);
        try h.validate();
        return h;
    }
};

comptime {
    assert(@sizeOf(SessionHeader) == PAGE_BYTES);
    assert(@offsetOf(SessionHeader, "magic") == 0);
    assert(@offsetOf(SessionHeader, "abi") == 8);
    assert(@offsetOf(SessionHeader, "generation") == 16);
    assert(@offsetOf(SessionHeader, "heartbeat") == 24);
    assert(@offsetOf(SessionHeader, "doorbell") == 64);
    assert(@offsetOf(SessionHeader, "daemon_parked") == 68);
    assert(@offsetOf(SessionHeader, "cqe") == 128);
    assert(@sizeOf(CqeDoorbell) == 16);
    assert(@offsetOf(CqeDoorbell, "seq") == 0);
    assert(@offsetOf(CqeDoorbell, "parked") == 4);
    assert(@offsetOf(CqeDoorbell, "wake_at") == 8);
    assert(@offsetOf(CqeDoorbell, "wake_paid_mark") == 12);
    assert(@offsetOf(SessionHeader, "geometry") == 192);
}

// ---------------------------------------------------------------------------
// The slot array.
// ---------------------------------------------------------------------------

/// The protocol word + the ABA guard (`slot_core.zig` is the state machine).
pub const SlotCore = extern struct {
    /// Low byte = state, bit 8 = WAITER; the per-slot futex word.
    state: Atomic(u32) = .init(0),
    _pad: u32 = 0,
    /// Monotonic per slot (not dense), bumped at claim; ordered by the
    /// state edges, never by its own ordering.
    generation: Atomic(u64) = .init(0),
};

pub const IpcSlot = extern struct {
    core: SlotCore = .{},
    op: Atomic(u32) = .init(0),
    /// 0 in v1 (reserved descriptor bits).
    flags: Atomic(u32) = .init(0),
    /// Daemon-issued binding id (`BIND_OK`).
    binding: Atomic(u64) = .init(0),
    offset: Atomic(u64) = .init(0),
    /// `≤ max_op_bytes`, daemon-validated on the private copy.
    len: Atomic(u32) = .init(0),
    /// CLOCK_MONOTONIC low 32 bits at publish; the daemon reads it ONCE at
    /// dequeue, display-only (`ingressDeltaNs`).
    ingress_stamp_ns: Atomic(u32) = .init(0),
    /// Payload window base (`slot × slab` for the slab-native client).
    arena_off: Atomic(u64) = .init(0),
    /// Bytes or `-errno`; published by `complete`'s release half.
    result: Atomic(i64) = .init(0),
    /// The second line of the pair.
    _pad: [64]u8 = @splat(0),
};

comptime {
    assert(@sizeOf(SlotCore) == 16);
    assert(@offsetOf(SlotCore, "state") == 0);
    assert(@offsetOf(SlotCore, "generation") == 8);
    assert(@sizeOf(IpcSlot) == SLOT_BYTES);
    assert(@alignOf(IpcSlot) >= 8);
    assert(@offsetOf(IpcSlot, "core") == 0);
    assert(@offsetOf(IpcSlot, "op") == 16);
    assert(@offsetOf(IpcSlot, "flags") == 20);
    assert(@offsetOf(IpcSlot, "binding") == 24);
    assert(@offsetOf(IpcSlot, "offset") == 32);
    assert(@offsetOf(IpcSlot, "len") == 40);
    assert(@offsetOf(IpcSlot, "ingress_stamp_ns") == 44);
    assert(@offsetOf(IpcSlot, "arena_off") == 48);
    assert(@offsetOf(IpcSlot, "result") == 56);
    assert(@offsetOf(IpcSlot, "_pad") == 64);
}

/// The daemon's private copy of a descriptor: the ONE linearization read
/// after `tryBeginServe` returned true (spec §5.3.1 rule 1); everything the
/// serve derives — the arena window, the op class, the compare — comes from
/// THIS copy, never from the shared words again.
pub const SlotDescriptor = struct {
    op: u32,
    flags: u32,
    binding: u64,
    offset: u64,
    len: u32,
    arena_off: u64,
    generation: u64,
};

/// `null` if the stamp is 0 (unstamped) or the delta is at/past the
/// plausibility bound (wrap-ambiguous or hostile); else the wrap-safe delta.
pub fn ingressDeltaNs(stamp: u32, now_low32: u32) ?u32 {
    if (stamp == 0) return null;
    const delta = now_low32 -% stamp;
    if (delta >= INGRESS_PLAUSIBLE_MAX_NS) return null;
    return delta;
}

// ---------------------------------------------------------------------------
// The submission ring region and the client stats page.
// ---------------------------------------------------------------------------

/// Vyukov cell: `seq == pos` empty/claimable at `pos`; `seq == pos + 1`
/// published; `seq == pos + capacity` freed for the next lap.
pub const RingCell = extern struct {
    seq: Atomic(u32) = .init(0),
    value: Atomic(u32) = .init(0),
};

comptime {
    assert(@sizeOf(RingCell) == RING_CELL_BYTES);
}

/// CLIENT-written, display-only daemon-side: summed verbatim into the stats
/// export at teardown, never a control input.
/// The shim's bail-out attribution rows (spec §4; `shim/classify.zig`
/// `Reason` indexes the array) and the daemon BIND refusal classes (index =
/// `wire.RefuseClass` value, 0 = unknown): the client's side of the
/// engagement ledger (decision 0002 ledger S5), display-only like the page.
pub const PRELOAD_REASONS: usize = 24;
pub const PRELOAD_REFUSE_CLASSES: usize = 10;

pub const ClientStatsPage = extern struct {
    lane_gate_kernel_routes: Atomic(u64) = .init(0),
    lane_gate_kernel_bytes: Atomic(u64) = .init(0),
    lane_gate_threshold_bytes: Atomic(u64) = .init(0),
    il_submit_harvested: Atomic(u64) = .init(0),
    il_park_eras: Atomic(u64) = .init(0),
    il_slot_reroutes: Atomic(u64) = .init(0),
    preload_passthrough_ops: [PRELOAD_REASONS]Atomic(u64) = @splat(.init(0)),
    preload_bind_refused: [PRELOAD_REFUSE_CLASSES]Atomic(u64) = @splat(.init(0)),
    _reserved: [PAGE_BYTES - 48 - 8 * (PRELOAD_REASONS + PRELOAD_REFUSE_CLASSES)]u8 = @splat(0),
};

comptime {
    assert(@sizeOf(ClientStatsPage) == PAGE_BYTES);
    assert(@offsetOf(ClientStatsPage, "lane_gate_kernel_routes") == 0);
    assert(@offsetOf(ClientStatsPage, "lane_gate_kernel_bytes") == 8);
    assert(@offsetOf(ClientStatsPage, "lane_gate_threshold_bytes") == 16);
    assert(@offsetOf(ClientStatsPage, "il_submit_harvested") == 24);
    assert(@offsetOf(ClientStatsPage, "il_park_eras") == 32);
    assert(@offsetOf(ClientStatsPage, "il_slot_reroutes") == 40);
    assert(@offsetOf(ClientStatsPage, "preload_passthrough_ops") == 48);
    assert(@offsetOf(ClientStatsPage, "preload_bind_refused") == 48 + 8 * PRELOAD_REASONS);
}

// ---------------------------------------------------------------------------
// The typed view over a mapping: both sides cast the mapped bytes into the
// region types at the computed offsets; every index and window is bounds-
// checked against the geometry (the daemon's screen of client-named
// offsets; the client's own guard).
// ---------------------------------------------------------------------------

pub const Mapping = struct {
    base: [*]align(@alignOf(SessionHeader)) u8,
    layout: SessionLayout,

    pub const WindowError = error{ SlotOutOfRange, ArenaWindowOutOfRange };

    pub fn init(base: [*]align(@alignOf(SessionHeader)) u8, layout: SessionLayout) Mapping {
        return .{ .base = base, .layout = layout };
    }

    pub fn header(m: Mapping) *SessionHeader {
        return @ptrCast(m.base);
    }

    pub fn ringTail(m: Mapping) *Atomic(u32) {
        return @ptrCast(@alignCast(m.base + m.layout.ring_off));
    }

    pub fn ringCells(m: Mapping) []RingCell {
        const p: [*]RingCell = @ptrCast(@alignCast(m.base + m.layout.ring_cells_off));
        return p[0..m.layout.geometry.ring_entries];
    }

    pub fn slots(m: Mapping) []IpcSlot {
        const p: [*]IpcSlot = @ptrCast(@alignCast(m.base + m.layout.slots_off));
        return p[0..m.layout.geometry.slots];
    }

    /// Refuses `index ≥ slots` (a hostile ring entry).
    pub fn slot(m: Mapping, index: u32) WindowError!*IpcSlot {
        if (index >= m.layout.geometry.slots) return error.SlotOutOfRange;
        return &m.slots()[index];
    }

    pub fn statsPage(m: Mapping) *ClientStatsPage {
        return @ptrCast(@alignCast(m.base + m.layout.stats_off));
    }

    pub fn arena(m: Mapping) []u8 {
        return (m.base + m.layout.arena_off)[0..m.layout.arena_bytes];
    }

    /// Refuses `off + len > arena_bytes` (checked add — a lying length is
    /// never an address authority).
    pub fn arenaWindow(m: Mapping, off: u64, len: u64) WindowError![]u8 {
        const end = std.math.add(u64, off, len) catch return error.ArenaWindowOutOfRange;
        if (end > m.layout.arena_bytes) return error.ArenaWindowOutOfRange;
        return m.arena()[@intCast(off)..@intCast(end)];
    }

    /// Slot `i`'s slab-native window `[i·slab, (i+1)·slab)`.
    pub fn slotWindow(m: Mapping, index: u32) WindowError![]u8 {
        if (index >= m.layout.geometry.slots) return error.SlotOutOfRange;
        return m.arenaWindow(@as(u64, index) * m.layout.slab, m.layout.slab);
    }
};

// ---------------------------------------------------------------------------
// Placement hooks (pure; `shm.zig` makes the syscalls).
// ---------------------------------------------------------------------------

/// The PMD-aligned over-reserve plan: reserve `len + PMD` PROT_NONE (len
/// in whole kernel pages — `mmap` rounds it so), map the fd `MAP_FIXED` at
/// the first PMD-aligned address inside, and trim the head/tail slack in
/// the KERNEL's page size — a raw `aligned + len` munmap of a partial page
/// would be refused and leak the tail reservation.
pub const PmdPlan = struct {
    reserve_bytes: u64,
    aligned: u64,
    head_slack: u64,
    tail_slack: u64,

    pub fn reserveBytes(len: u64, page: u64) u64 {
        return derive.alignUp(len, page) + PMD_BYTES;
    }

    /// `reserve_addr` is what `mmap` returned for the reservation (page-
    /// aligned by the kernel).
    pub fn compute(reserve_addr: u64, len: u64, page: u64) PmdPlan {
        assert(reserve_addr % page == 0);
        const reserve_bytes = reserveBytes(len, page);
        const aligned = derive.alignUp(reserve_addr, PMD_BYTES);
        const mapped_end = aligned + derive.alignUp(len, page);
        return .{
            .reserve_bytes = reserve_bytes,
            .aligned = aligned,
            .head_slack = aligned - reserve_addr,
            .tail_slack = reserve_addr + reserve_bytes - mapped_end,
        };
    }
};

/// A candidate owner thread for a new session (NUMA, spec §1.7).
pub const OwnerCandidate = struct {
    /// Sessions currently pinned to it.
    load: u32,
    /// NUMA distance from the client's inferred node to the owner's node
    /// (0 = same node; a single-node map is all zeros — a structural no-op).
    distance: u32,
};

/// Balance-first owner pick `(load, distance, index)`: the least-loaded
/// owner, the nearest among equals, the lowest index among those. The
/// retired locality-first order confined a fork-clustered fleet to one
/// node's owner subset — the ~530 k ceiling.
pub fn pickOwner(candidates: []const OwnerCandidate) ?usize {
    var best: ?usize = null;
    for (candidates, 0..) |c, i| {
        const b = best orelse {
            best = i;
            continue;
        };
        const bc = candidates[b];
        if (c.load < bc.load or (c.load == bc.load and c.distance < bc.distance)) best = i;
    }
    return best;
}

/// Owner-index → node partition: a largest-remainder weighted round-robin
/// by each node's CPU share. `shares[n]` = the node's CPU count;
/// `out[i]` = the node of owner `i`. Nodes with a zero share get no owners
/// unless every share is zero (then node 0 takes all).
pub fn nodePartition(owner_count: usize, shares: []const u32, out: []u16) void {
    assert(out.len >= owner_count);
    var total: u64 = 0;
    for (shares) |s| total += s;
    if (total == 0 or shares.len == 0) {
        @memset(out[0..owner_count], 0);
        return;
    }
    // Floor quotas, then the largest remainders fill the rest.
    var quota: [256]u32 = undefined;
    var rem: [256]u64 = undefined;
    assert(shares.len <= quota.len);
    var assigned: usize = 0;
    for (shares, 0..) |s, n| {
        const exact = @as(u64, owner_count) * s;
        quota[n] = @intCast(exact / total);
        rem[n] = exact % total;
        assigned += quota[n];
    }
    while (assigned < owner_count) : (assigned += 1) {
        var best: usize = 0;
        for (shares, 0..) |_, n| if (rem[n] > rem[best]) {
            best = n;
        };
        quota[best] += 1;
        rem[best] = 0;
    }
    // Round-robin the owners over the nodes in quota order.
    var i: usize = 0;
    while (i < owner_count) {
        var progressed = false;
        for (shares, 0..) |_, n| {
            if (quota[n] == 0 or i >= owner_count) continue;
            out[i] = @intCast(n);
            quota[n] -= 1;
            i += 1;
            progressed = true;
        }
        if (!progressed) break;
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "default geometry validates and computes: regions in order, page-aligned, non-overlapping, arena last; the spec's offsets as ties" {
    const l = try SessionLayout.compute(Geometry.default);
    try testing.expectEqual(@as(u64, 0), l.header_off);
    try testing.expectEqual(@as(u64, 0x1000), l.ring_off);
    try testing.expectEqual(@as(u64, 128 + 1024 * 8), l.ring_bytes);
    try testing.expectEqual(@as(u64, 0x1080), l.ring_cells_off);
    try testing.expectEqual(@as(u64, 0x4000), l.slots_off);
    try testing.expectEqual(@as(u64, 1024 * 128), l.slots_bytes);
    try testing.expectEqual(@as(u64, 0x24000), l.stats_off);
    try testing.expectEqual(@as(u64, 0x25000), l.arena_off);
    try testing.expectEqual(@as(u64, 0x25000) + 64 * MiB, l.total_bytes);
    try testing.expectEqual(@as(u64, 64 * 1024), l.slab);
    for ([_]u64{ l.header_off, l.ring_off, l.slots_off, l.stats_off, l.arena_off }) |off| try testing.expectEqual(@as(u64, 0), off % PAGE_BYTES);
    try testing.expect(l.header_off + l.header_bytes <= l.ring_off);
    try testing.expect(l.ring_off + l.ring_bytes <= l.slots_off);
    try testing.expect(l.slots_off + l.slots_bytes <= l.stats_off);
    try testing.expect(l.stats_off + l.stats_bytes <= l.arena_off);
    try testing.expect(l.arena_off + l.arena_bytes == l.total_bytes);
    // The arena is 4 MiB-aligned in SIZE and page-aligned in offset, so every
    // slab window is a DMA-eligible 4 KiB multiple.
    try testing.expectEqual(@as(u64, 0), l.arena_bytes % ARENA_ALIGN_BYTES);
    try testing.expectEqual(@as(u64, 0), l.slab % PAGE_BYTES);
}

test "geometry refusals attributed exactly: each of the 11 hits its own member via validate AND compute" {
    const d = Geometry.default;
    const rows = [_]struct { g: Geometry, e: GeometryError }{
        .{ .g = .{ .ring_entries = 1, .slots = 1, .arena_bytes = d.arena_bytes, .max_op_bytes = d.max_op_bytes }, .e = error.RingEntriesTooSmall },
        .{ .g = .{ .ring_entries = 1000, .slots = 1000, .arena_bytes = d.arena_bytes, .max_op_bytes = d.max_op_bytes }, .e = error.RingEntriesNotPow2 },
        .{ .g = .{ .ring_entries = 1 << 17, .slots = 1024, .arena_bytes = d.arena_bytes, .max_op_bytes = d.max_op_bytes }, .e = error.RingEntriesTooLarge },
        .{ .g = .{ .ring_entries = 1024, .slots = 0, .arena_bytes = d.arena_bytes, .max_op_bytes = d.max_op_bytes }, .e = error.SlotsZero },
        .{ .g = .{ .ring_entries = 1024, .slots = 1025, .arena_bytes = d.arena_bytes, .max_op_bytes = d.max_op_bytes }, .e = error.SlotsExceedRing },
        .{ .g = .{ .ring_entries = 1024, .slots = 1024, .arena_bytes = 0, .max_op_bytes = d.max_op_bytes }, .e = error.ArenaZero },
        .{ .g = .{ .ring_entries = 1024, .slots = 1024, .arena_bytes = 4097, .max_op_bytes = 4096 }, .e = error.ArenaNotPageMultiple },
        .{ .g = .{ .ring_entries = 1024, .slots = 1024, .arena_bytes = MAX_ARENA_BYTES + PAGE_BYTES, .max_op_bytes = d.max_op_bytes }, .e = error.ArenaTooLarge },
        .{ .g = .{ .ring_entries = 1024, .slots = 1024, .arena_bytes = d.arena_bytes, .max_op_bytes = 0 }, .e = error.MaxOpZero },
        .{ .g = .{ .ring_entries = 1024, .slots = 1024, .arena_bytes = PAGE_BYTES, .max_op_bytes = 2 * PAGE_BYTES }, .e = error.MaxOpExceedsArena },
    };
    for (rows) |r| {
        try testing.expectError(r.e, r.g.validate());
        try testing.expectError(r.e, SessionLayout.compute(r.g));
    }
    // The eleventh: the checked arithmetic. The validator's arena ceiling
    // keeps a valid geometry far from u64, so the overflow arm is reached
    // only by the helpers themselves.
    try testing.expectError(error.Overflow, SessionLayout.add(std.math.maxInt(u64), 1));
    try testing.expectError(error.Overflow, SessionLayout.mul(1 << 40, 1 << 40));
    try testing.expectEqual(@as(usize, 11), @typeInfo(GeometryError).error_set.error_names.?.len);
}

test "the ONE slab law: min(arena/slots, max_op) floored to a page at or above it; sub-page slabs verbatim; slot × slab page-aligned for every slot" {
    try testing.expectEqual(@as(u64, 64 * 1024), Geometry.default.slotSlab());
    // The bounce shape: 90 MiB over 1024 slots = 92,160 B ⇒ floored to 90,112 (22 pages).
    const odd: Geometry = .{ .ring_entries = 1024, .slots = 1024, .arena_bytes = 90 * MiB, .max_op_bytes = 1 << 20 };
    try testing.expectEqual(@as(u64, 22 * 4096), odd.slotSlab());
    var i: u32 = 0;
    while (i < odd.slots) : (i += 1) try testing.expectEqual(@as(u64, 0), (@as(u64, i) * odd.slotSlab()) % PAGE_BYTES);
    // max_op caps the slab.
    const capped: Geometry = .{ .ring_entries = 4, .slots = 2, .arena_bytes = 8 * MiB, .max_op_bytes = 4096 };
    try testing.expectEqual(@as(u64, 4096), capped.slotSlab());
    // A toy geometry: sub-page slabs pass verbatim.
    const toy: Geometry = .{ .ring_entries = 4, .slots = 4, .arena_bytes = 4096, .max_op_bytes = 512 };
    try testing.expectEqual(@as(u64, 512), toy.slotSlab());
    try testing.expectEqual(@as(u64, 4096), (try SessionLayout.compute(toy)).arena_bytes);
}

test "header round-trip and refusals: a fresh header validates; bad magic / abi+1 / a bad geometry each refuse at map time; decode is total" {
    var h: SessionHeader = .{ .geometry = Geometry.default };
    try h.validate();
    const back = try SessionHeader.decode(std.mem.asBytes(&h));
    try testing.expect(back.geometry.eql(Geometry.default));
    try testing.expectEqual(@as(u64, 1), back.generation.load(.monotonic));
    try testing.expectEqual(std.math.maxInt(u32), back.cqe.wake_paid_mark.load(.monotonic));
    h.magic +%= 1;
    try testing.expectError(error.BadMagic, h.validate());
    h.magic = IPC_MAGIC;
    h.abi = IPC_ABI + 1;
    try testing.expectError(error.BadAbi, h.validate());
    h.abi = IPC_ABI;
    h.geometry.slots = 0;
    try testing.expectError(error.SlotsZero, h.validate());
    try testing.expectError(error.Truncated, SessionHeader.decode(std.mem.asBytes(&h)[0..4095]));
}

fn sessionHeaderFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [PAGE_BYTES]u8 = undefined;
    const len = smith.slice(&buf);
    if (SessionHeader.decode(buf[0..len])) |d| try d.geometry.validate() else |_| {}
}

test "fuzz: SessionHeader.decode never panics on arbitrary bytes and a header that decodes carries a valid geometry" {
    try fuzzkit.fuzz({}, sessionHeaderFuzz, .{});
}

test "property: SessionHeader.decode over PRNG bytes and a mutated valid image refuses or round-trips its geometry" {
    var prng = std.Random.DefaultPrng.init(0x5151);
    const r = prng.random();
    var buf: [PAGE_BYTES]u8 = undefined;
    var round: usize = 0;
    while (round < 2000) : (round += 1) {
        r.bytes(&buf);
        _ = SessionHeader.decode(&buf) catch {};
        // A valid image with one random byte flipped.
        var h: SessionHeader = .{ .geometry = Geometry.default };
        const img = std.mem.asBytes(&h);
        img[r.uintLessThan(usize, 216)] ^= @as(u8, 1) << r.int(u3);
        if (SessionHeader.decode(img)) |d| {
            try d.geometry.validate();
        } else |_| {}
    }
}

test "ingress delta law: an unstamped slot, a stamp from the future, and ≥ 1 s all discard; a sub-second delta is wrap-safe" {
    try testing.expectEqual(@as(?u32, null), ingressDeltaNs(0, 1000));
    try testing.expectEqual(@as(?u32, 500), ingressDeltaNs(1000, 1500));
    // Across the u32 wrap.
    try testing.expectEqual(@as(?u32, 1000), ingressDeltaNs(std.math.maxInt(u32) - 499, 500));
    // A stamp ahead of now reads as a huge delta: discarded.
    try testing.expectEqual(@as(?u32, null), ingressDeltaNs(2000, 1000));
    try testing.expectEqual(@as(?u32, null), ingressDeltaNs(1, 1 + INGRESS_PLAUSIBLE_MAX_NS));
    try testing.expectEqual(@as(?u32, INGRESS_PLAUSIBLE_MAX_NS - 1), ingressDeltaNs(1, INGRESS_PLAUSIBLE_MAX_NS));
}

test "Mapping: typed views land at the computed offsets; slot and window refusals; the slab window of slot i is [i·slab, (i+1)·slab)" {
    const g: Geometry = .{ .ring_entries = 8, .slots = 4, .arena_bytes = 8 * PAGE_BYTES, .max_op_bytes = PAGE_BYTES };
    const l = try SessionLayout.compute(g);
    const mem = try testing.allocator.alignedAlloc(u8, .fromByteUnits(PAGE_BYTES), @intCast(l.total_bytes));
    defer testing.allocator.free(mem);
    @memset(mem, 0);
    const m = Mapping.init(mem.ptr, l);
    try testing.expectEqual(@intFromPtr(mem.ptr), @intFromPtr(m.header()));
    try testing.expectEqual(@intFromPtr(mem.ptr) + l.ring_off, @intFromPtr(m.ringTail()));
    try testing.expectEqual(@intFromPtr(mem.ptr) + l.ring_cells_off, @intFromPtr(m.ringCells().ptr));
    try testing.expectEqual(@as(usize, 8), m.ringCells().len);
    try testing.expectEqual(@intFromPtr(mem.ptr) + l.slots_off, @intFromPtr(m.slots().ptr));
    try testing.expectEqual(@intFromPtr(mem.ptr) + l.slots_off + 3 * SLOT_BYTES, @intFromPtr(try m.slot(3)));
    try testing.expectError(error.SlotOutOfRange, m.slot(4));
    try testing.expectEqual(@intFromPtr(mem.ptr) + l.stats_off, @intFromPtr(m.statsPage()));
    try testing.expectEqual(@intFromPtr(mem.ptr) + l.arena_off, @intFromPtr(m.arena().ptr));
    try testing.expectEqual(@as(usize, 4096), (try m.arenaWindow(4 * 4096, 4096)).len);
    try testing.expectError(error.ArenaWindowOutOfRange, m.arenaWindow(7 * 4096, 4097));
    try testing.expectError(error.ArenaWindowOutOfRange, m.arenaWindow(std.math.maxInt(u64), 1));
    try testing.expectEqual(@as(u64, 4096), l.slab);
    try testing.expectEqual(@intFromPtr(mem.ptr) + l.arena_off + 2 * 4096, @intFromPtr((try m.slotWindow(2)).ptr));
    try testing.expectError(error.SlotOutOfRange, m.slotWindow(4));
}

test "PmdPlan: the over-reserve is len (in pages) + 2 MiB; the aligned base is PMD-aligned inside it; head + mapped + tail = the reserve, all in whole pages" {
    for ([_]u64{ 0x7f00_0000_1000, 0x7f00_0020_0000, 0x7f00_001f_f000 }) |addr| {
        for ([_]u64{ 64 * MiB + 0x25000, 4 * MiB, 4096, 100_000 }) |len| {
            const p = PmdPlan.compute(addr, len, 4096);
            try testing.expectEqual(derive.alignUp(len, 4096) + PMD_BYTES, p.reserve_bytes);
            try testing.expectEqual(@as(u64, 0), p.aligned % PMD_BYTES);
            try testing.expect(p.aligned >= addr and p.aligned + len <= addr + p.reserve_bytes);
            try testing.expectEqual(p.aligned - addr, p.head_slack);
            try testing.expectEqual(@as(u64, 0), p.tail_slack % 4096);
            try testing.expectEqual(@as(u64, 0), p.head_slack % 4096);
            try testing.expectEqual(p.reserve_bytes, p.head_slack + derive.alignUp(len, 4096) + p.tail_slack);
        }
    }
    // A 64 KiB kernel page: the trim math follows the page, not 4 KiB.
    const big = PmdPlan.compute(0x7f00_0001_0000, 100_000, 65536);
    try testing.expectEqual(@as(u64, 0), big.tail_slack % 65536);
    try testing.expectEqual(big.reserve_bytes, big.head_slack + derive.alignUp(100_000, 65536) + big.tail_slack);
}

test "balance-first owner pick (load, distance, index): least load wins over nearest; nearest among equal loads; lowest index among ties; empty is null" {
    try testing.expectEqual(@as(?usize, null), pickOwner(&.{}));
    try testing.expectEqual(@as(?usize, 2), pickOwner(&.{ .{ .load = 2, .distance = 0 }, .{ .load = 2, .distance = 0 }, .{ .load = 1, .distance = 2 } }));
    try testing.expectEqual(@as(?usize, 1), pickOwner(&.{ .{ .load = 1, .distance = 2 }, .{ .load = 1, .distance = 0 } }));
    try testing.expectEqual(@as(?usize, 0), pickOwner(&.{ .{ .load = 1, .distance = 1 }, .{ .load = 1, .distance = 1 } }));
}

test "node partition: largest-remainder weighted round-robin by CPU share; a single node or zero shares is all node 0" {
    var out: [16]u16 = undefined;
    nodePartition(6, &.{ 16, 16 }, &out);
    var n0: u32 = 0;
    for (out[0..6]) |n| n0 += @intFromBool(n == 0);
    try testing.expectEqual(@as(u32, 3), n0);
    // Interleaved so the lowest owners already span both nodes.
    try testing.expectEqual(@as(u16, 0), out[0]);
    try testing.expectEqual(@as(u16, 1), out[1]);
    // 3:1 shares over 5 owners: 3.75 → 4 and 1.25 → 1.
    nodePartition(5, &.{ 24, 8 }, &out);
    n0 = 0;
    for (out[0..5]) |n| n0 += @intFromBool(n == 0);
    try testing.expectEqual(@as(u32, 4), n0);
    nodePartition(3, &.{32}, &out);
    for (out[0..3]) |n| try testing.expectEqual(@as(u16, 0), n);
    nodePartition(3, &.{ 0, 0 }, &out);
    for (out[0..3]) |n| try testing.expectEqual(@as(u16, 0), n);
    nodePartition(2, &.{}, &out);
    for (out[0..2]) |n| try testing.expectEqual(@as(u16, 0), n);
}

test "constants: the magic is the port's own, the ABI series starts at 1, the op table is {READ, WRITE}" {
    try testing.expectEqualStrings("SQZIPC02", std.mem.asBytes(&IPC_MAGIC));
    try testing.expectEqual(@as(u32, 1), IPC_ABI);
    try testing.expectEqual(@as(u32, 1), @backingInt(Op.read));
    try testing.expectEqual(@as(u32, 2), @backingInt(Op.write));
    try testing.expectEqual(@as(u64, 64 * MiB), DEFAULT_ARENA_BYTES);
    try testing.expectEqual(DEFAULT_SLOTS * PAGE_BYTES, ARENA_ALIGN_BYTES);
}
